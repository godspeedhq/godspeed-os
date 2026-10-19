// SPDX-License-Identifier: GPL-2.0-only
//! The TLS 1.3 client handshake (RFC 8446 4) and the connection after it.
//!
//! # How a caller drives it
//!
//! This is a state machine with no I/O of its own. The caller owns the TCP connection and moves bytes:
//!
//! ```text
//!   let (client, n) = Client::start(hello, verifier, &mut handshake_buf, &mut out)?;   send out[..n]
//!   loop:
//!       read 5 bytes; record_len(header) says how many more; read exactly those
//!       let (event, n) = client.read_record(&mut record, &mut out)?;                 send out[..n]
//!       event: Continue | Connected | Data(range of record) | Closed
//!   client.seal(request, &mut out)  ->  send;     client.close(&mut out)  ->  send
//! ```
//!
//! Any error ends the connection: every later call returns the same error. [`Client::failure_alert`]
//! writes the alert that tells the server why, which the caller sends before closing the socket.
//!
//! # What the handshake checks
//!
//! The ServerHello must choose what was offered and nothing else: TLS 1.3, TLS_AES_128_GCM_SHA256, an
//! X25519 key, no pre-shared key, no retry (a HelloRetryRequest can only ask for a group this client does
//! not have). The encrypted extensions may answer only what the ClientHello asked. The server's
//! certificate chain and its CertificateVerify signature go to the caller's [`ServerVerifier`], which
//! alone decides whether they are acceptable; this module only proves the handshake was not altered,
//! through the Finished message.
//!
//! # What is deliberately absent
//!
//! No session resumption (tickets are read and dropped), no early data, no client certificate (a
//! CertificateRequest ends the handshake loudly), no renegotiation, which TLS 1.3 does not have.

use core::ops::Range;

use crate::codec::{Reader, Writer};
use crate::ecdhe::{self, X25519_LEN};
use crate::record::{self, content, Protector, HEADER_LEN, LEGACY_VERSION, MAX_PLAINTEXT};
use crate::schedule::{self, Transcript, HASH_LEN};
use crate::{CertProblem, TlsError};

/// Handshake message types (RFC 8446 4).
mod msg {
    pub const CLIENT_HELLO: u8 = 1;
    pub const SERVER_HELLO: u8 = 2;
    pub const NEW_SESSION_TICKET: u8 = 4;
    pub const ENCRYPTED_EXTENSIONS: u8 = 8;
    pub const CERTIFICATE: u8 = 11;
    pub const CERTIFICATE_REQUEST: u8 = 13;
    pub const CERTIFICATE_VERIFY: u8 = 15;
    pub const FINISHED: u8 = 20;
    pub const KEY_UPDATE: u8 = 24;
}

/// Extension types (RFC 8446 4.2, RFC 8449).
mod ext {
    pub const SERVER_NAME: u16 = 0;
    pub const SUPPORTED_GROUPS: u16 = 10;
    pub const SIGNATURE_ALGORITHMS: u16 = 13;
    pub const RECORD_SIZE_LIMIT: u16 = 28;
    pub const SUPPORTED_VERSIONS: u16 = 43;
    pub const KEY_SHARE: u16 = 51;
}

const TLS13: u16 = 0x0304;
const TLS_AES_128_GCM_SHA256: u16 = 0x1301;
const X25519: u16 = 0x001d;

/// The signature schemes this client offers (RFC 8446 4.2.3): ECDSA on P-256 and P-384, nothing else. The
/// registry's chain is ECDSA end to end (`docs/developer-services-design.md` 10.3). Offering them is not
/// accepting them: the [`ServerVerifier`] decides.
pub const ECDSA_SECP256R1_SHA256: u16 = 0x0403;
pub const ECDSA_SECP384R1_SHA384: u16 = 0x0503;

/// The ServerHello random that marks a HelloRetryRequest: SHA-256 of "HelloRetryRequest" (RFC 8446 4.1.3).
const HELLO_RETRY_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2, 0xa2, 0x11,
    0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

/// The most extension types a ClientHello may offer and still be read back here. This client sends five;
/// the bound exists for the RFC 8448 test, whose ClientHello sends ten.
const MAX_OFFERED: usize = 16;

/// Everything about the server's identity that this crate does not decide.
///
/// The verifier is the caller's, built with the server name, the trusted roots and the current time, so
/// nothing about trust lives in this module (`docs/developer-services-design.md` 10.8). It is called
/// twice per handshake, in this order, and either call may end it.
pub trait ServerVerifier {
    /// The server's certificate chain, leaf first. Return an error to refuse it: an untrusted root, an
    /// expired certificate, a name that does not match.
    fn certificate_chain(&mut self, chain: CertificateChain<'_>) -> Result<(), TlsError>;

    /// The server's CertificateVerify: `signature`, by the leaf certificate's key under `scheme`, over
    /// `message`. Return an error unless it verifies and `scheme` is one this client offered.
    fn handshake_signature(&mut self, scheme: u16, message: &[u8], signature: &[u8]) -> Result<(), TlsError>;
}

/// A server's certificate list, already checked to be well formed: at least one entry, each a non-empty
/// DER certificate followed by its extensions (RFC 8446 4.4.2).
#[derive(Clone, Copy)]
pub struct CertificateChain<'a> {
    list: &'a [u8],
}

impl<'a> CertificateChain<'a> {
    pub(crate) fn parse(list: &'a [u8]) -> Result<Self, TlsError> {
        let mut r = Reader::new(list);
        let mut count = 0;
        while !r.is_empty() {
            if r.vec24()?.is_empty() {
                return Err(TlsError::DecodeError);
            }
            r.vec16()?;
            count += 1;
        }
        if count == 0 {
            return Err(TlsError::NoCertificate);
        }
        Ok(Self { list })
    }

    /// The DER certificates, leaf first.
    pub fn certificates(&self) -> impl Iterator<Item = &'a [u8]> {
        let mut r = Reader::new(self.list);
        core::iter::from_fn(move || {
            if r.is_empty() {
                return None;
            }
            // Both reads were proven to succeed by `parse`.
            let der = r.vec24().ok()?;
            r.vec16().ok()?;
            Some(der)
        })
    }

    /// The server's own certificate.
    pub fn leaf(&self) -> &'a [u8] {
        self.certificates().next().unwrap_or(&[])
    }
}

/// What the caller supplies to begin a connection. Every random value is the caller's, from InspectKernel
/// query 19 on Godspeed, and each must be fresh for every connection.
pub struct Hello<'a> {
    /// The DNS name to connect to, sent as server_name (RFC 6066 3) and checked by the verifier.
    pub server_name: &'a str,
    /// The ClientHello random.
    pub random: [u8; 32],
    /// A random legacy session id. TLS 1.3 does not use it, but a non-empty one makes the exchange look like
    /// a TLS 1.2 resumption to a middlebox that would otherwise drop it (RFC 8446 D.4).
    pub session_id: [u8; 32],
    /// The X25519 private key for this handshake. It is used once and forgotten.
    pub x25519_private: [u8; X25519_LEN],
}

/// What a record meant, for the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// More of the handshake, or a message with nothing for the caller (a ticket, a key update).
    Continue,
    /// The handshake is complete and verified. Application data can be sent.
    Connected,
    /// Application data, at this range of the record just given.
    Data(Range<usize>),
    /// The server sent close_notify: it will send nothing more.
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    AwaitServerHello,
    AwaitEncryptedExtensions,
    AwaitCertificate,
    AwaitCertificateVerify,
    AwaitFinished,
    Connected,
    Closed,
    Failed(TlsError),
}

/// One TLS 1.3 client connection.
pub struct Client<'b, V: ServerVerifier> {
    state: State,
    verifier: V,
    /// Handshake messages received but not yet complete: a message may span records, and a record may hold
    /// several. The caller's buffer, so its size bounds the largest message accepted, in practice the
    /// server's certificate chain (about 5 KiB for the registry's).
    buf: &'b mut [u8],
    held: usize,
    transcript: Transcript,
    private: [u8; X25519_LEN],
    session_id: [u8; 32],
    session_id_len: usize,
    offered: [u16; MAX_OFFERED],
    offered_len: usize,
    handshake_secret: [u8; HASH_LEN],
    client_hs: [u8; HASH_LEN],
    server_hs: [u8; HASH_LEN],
    client_ap: [u8; HASH_LEN],
    server_ap: [u8; HASH_LEN],
    rx: Option<Protector>,
    tx: Option<Protector>,
    max_send: usize,
    write_closed: bool,
}

/// The result of one handshake message.
struct Step {
    event: Event,
    written: usize,
    /// The message changed the receiving keys, so no handshake bytes may follow it in the same record
    /// (RFC 8446 5.1).
    key_change: bool,
}

impl Step {
    fn quiet() -> Self {
        Self { event: Event::Continue, written: 0, key_change: false }
    }
}

impl<'b, V: ServerVerifier> Client<'b, V> {
    /// Begin a connection: write the ClientHello record into `out` and return the client and its length.
    ///
    /// `handshake_buf` holds handshake messages while they arrive; 16 KiB is ample for any certificate chain
    /// a public server sends, and a message that does not fit ends the handshake with
    /// [`TlsError::HandshakeTooLarge`] rather than being cut.
    pub fn start(hello: Hello<'_>, verifier: V, handshake_buf: &'b mut [u8], out: &mut [u8]) -> Result<(Self, usize), TlsError> {
        check_server_name(hello.server_name)?;
        let body = out.get_mut(HEADER_LEN..).ok_or(TlsError::BufferTooSmall)?;
        let n = write_client_hello(&hello, &ecdhe::public_key(&hello.x25519_private), body)?;
        // The ClientHello's record says 03 01, for servers that refuse anything newer there (RFC 8446 5.1).
        let mut header = [0u8; HEADER_LEN];
        header[0] = content::HANDSHAKE;
        header[1..3].copy_from_slice(&[0x03, 0x01]);
        header[3..5].copy_from_slice(&(n as u16).to_be_bytes());
        out[..HEADER_LEN].copy_from_slice(&header);
        let client = Self::after_hello(&out[HEADER_LEN..HEADER_LEN + n], hello.x25519_private, verifier, handshake_buf)?;
        Ok((client, HEADER_LEN + n))
    }

    /// The client as it stands once `client_hello` (a whole message, header included) has been sent. What
    /// was offered is read back from the message itself, so the checks on the server's answers cannot
    /// disagree with what was really sent. Also how the tests replay RFC 8448's own ClientHello.
    pub(crate) fn after_hello(client_hello: &[u8], private: [u8; X25519_LEN], verifier: V, buf: &'b mut [u8]) -> Result<Self, TlsError> {
        let mut client = Self {
            state: State::AwaitServerHello,
            verifier,
            buf,
            held: 0,
            transcript: Transcript::new(),
            private,
            session_id: [0; 32],
            session_id_len: 0,
            offered: [0; MAX_OFFERED],
            offered_len: 0,
            handshake_secret: [0; HASH_LEN],
            client_hs: [0; HASH_LEN],
            server_hs: [0; HASH_LEN],
            client_ap: [0; HASH_LEN],
            server_ap: [0; HASH_LEN],
            rx: None,
            tx: None,
            max_send: MAX_PLAINTEXT,
            write_closed: false,
        };
        let mut r = Reader::new(client_hello);
        if r.u8()? != msg::CLIENT_HELLO {
            return Err(TlsError::DecodeError);
        }
        let mut body = Reader::new(r.vec24()?);
        r.finish()?;
        body.u16()?;
        body.take(32)?;
        let sid = body.vec8()?;
        if sid.len() > 32 {
            return Err(TlsError::DecodeError);
        }
        client.session_id[..sid.len()].copy_from_slice(sid);
        client.session_id_len = sid.len();
        body.vec16()?;
        body.vec8()?;
        let mut exts = Reader::new(body.vec16()?);
        body.finish()?;
        while !exts.is_empty() {
            let t = exts.u16()?;
            exts.vec16()?;
            if client.offered_len == MAX_OFFERED {
                return Err(TlsError::BufferTooSmall);
            }
            client.offered[client.offered_len] = t;
            client.offered_len += 1;
        }
        client.transcript.update(client_hello);
        Ok(client)
    }

    fn offered(&self, t: u16) -> bool {
        self.offered[..self.offered_len].contains(&t)
    }

    /// The largest application data one [`Client::seal`] call takes: 16 KiB, or less if the server asked
    /// for a smaller record (RFC 8449).
    pub fn max_plaintext(&self) -> usize {
        self.max_send
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// The verifier, so a caller can ask it what it saw.
    pub fn verifier(&self) -> &V {
        &self.verifier
    }

    /// Read one complete record (`record_len` bytes, header included). Anything the protocol requires in
    /// answer, the client's Finished or a KeyUpdate, is written to `out`, and its length returned beside
    /// the event; the caller sends it before reading again.
    ///
    /// The record is decrypted in place, so application data is handed back as a range of it, not copied.
    pub fn read_record(&mut self, record: &mut [u8], out: &mut [u8]) -> Result<(Event, usize), TlsError> {
        match self.state {
            State::Failed(e) => return Err(e),
            State::Closed => return Err(TlsError::UnexpectedMessage),
            _ => {}
        }
        let result = self.read_inner(record, out);
        if let Err(e) = result {
            self.state = State::Failed(e);
        }
        result
    }

    fn handshaking(&self) -> bool {
        !matches!(self.state, State::Connected | State::Closed | State::Failed(_))
    }

    fn read_inner(&mut self, record: &mut [u8], out: &mut [u8]) -> Result<(Event, usize), TlsError> {
        if record::record_len(record)? != record.len() {
            return Err(TlsError::BadRecordHeader);
        }
        let ctype = record[0];
        match ctype {
            // Sent for middleboxes and otherwise meaningless (RFC 8446 5): one byte of 1, during the
            // handshake only.
            content::CHANGE_CIPHER_SPEC => {
                if self.handshaking() && record[HEADER_LEN..] == [1] {
                    Ok((Event::Continue, 0))
                } else {
                    Err(TlsError::UnexpectedMessage)
                }
            }
            content::ALERT if self.handshaking() => self.alert(&record[HEADER_LEN..]),
            content::HANDSHAKE if self.state == State::AwaitServerHello => {
                self.append(&record[HEADER_LEN..])?;
                self.process(out)
            }
            content::APPLICATION_DATA => {
                let rx = self.rx.as_mut().ok_or(TlsError::UnexpectedMessage)?;
                let (inner, range) = rx.open(record)?;
                match inner {
                    content::HANDSHAKE => {
                        self.append(&record[range])?;
                        self.process(out)
                    }
                    content::ALERT => self.alert(&record[range]),
                    // Application data may not arrive in the middle of a handshake message (5.1).
                    content::APPLICATION_DATA if self.state == State::Connected && self.held == 0 => {
                        Ok((Event::Data(range), 0))
                    }
                    _ => Err(TlsError::UnexpectedMessage),
                }
            }
            _ => Err(TlsError::UnexpectedMessage),
        }
    }

    fn alert(&mut self, body: &[u8]) -> Result<(Event, usize), TlsError> {
        if body.len() != 2 {
            return Err(TlsError::DecodeError);
        }
        // close_notify ends a connection cleanly; before the handshake finishes it ends it anyway.
        if body[1] == 0 && self.state == State::Connected {
            self.state = State::Closed;
            return Ok((Event::Closed, 0));
        }
        Err(TlsError::AlertReceived(body[1]))
    }

    fn append(&mut self, fragment: &[u8]) -> Result<(), TlsError> {
        // A zero-length handshake fragment is forbidden (RFC 8446 5.1).
        if fragment.is_empty() {
            return Err(TlsError::UnexpectedMessage);
        }
        let end = self.held + fragment.len();
        self.buf.get_mut(self.held..end).ok_or(TlsError::HandshakeTooLarge)?.copy_from_slice(fragment);
        self.held = end;
        Ok(())
    }

    /// Handle every complete handshake message held, in order.
    fn process(&mut self, out: &mut [u8]) -> Result<(Event, usize), TlsError> {
        let mut event = Event::Continue;
        let mut written = 0;
        while self.held >= 4 {
            let len = (self.buf[1] as usize) << 16 | (self.buf[2] as usize) << 8 | self.buf[3] as usize;
            let total = 4 + len;
            if total > self.buf.len() {
                return Err(TlsError::HandshakeTooLarge);
            }
            if self.held < total {
                break;
            }
            let buf = core::mem::take(&mut self.buf);
            let step = self.message(&buf[..total], &mut out[written..]);
            buf.copy_within(total..self.held, 0);
            self.held -= total;
            self.buf = buf;
            let step = step?;
            written += step.written;
            if step.event != Event::Continue {
                event = step.event;
            }
            if step.key_change && self.held != 0 {
                return Err(TlsError::UnexpectedMessage);
            }
        }
        Ok((event, written))
    }

    fn message(&mut self, m: &[u8], out: &mut [u8]) -> Result<Step, TlsError> {
        let body = &m[4..];
        match (self.state, m[0]) {
            (State::AwaitServerHello, msg::SERVER_HELLO) => self.server_hello(m, body),
            (State::AwaitEncryptedExtensions, msg::ENCRYPTED_EXTENSIONS) => self.encrypted_extensions(m, body),
            (State::AwaitCertificate, msg::CERTIFICATE_REQUEST) => Err(TlsError::CertificateRequested),
            (State::AwaitCertificate, msg::CERTIFICATE) => self.certificate(m, body),
            (State::AwaitCertificateVerify, msg::CERTIFICATE_VERIFY) => self.certificate_verify(m, body),
            (State::AwaitFinished, msg::FINISHED) => self.finished(m, body, out),
            // Tickets are for resumption, which this client does not do.
            (State::Connected, msg::NEW_SESSION_TICKET) => Ok(Step::quiet()),
            (State::Connected, msg::KEY_UPDATE) => self.key_update(body, out),
            _ => Err(TlsError::UnexpectedMessage),
        }
    }

    fn server_hello(&mut self, m: &[u8], body: &[u8]) -> Result<Step, TlsError> {
        let mut r = Reader::new(body);
        if r.u16()? != 0x0303 {
            return Err(TlsError::IllegalParameter);
        }
        if r.take(32)? == HELLO_RETRY_RANDOM {
            return Err(TlsError::HelloRetryRequest);
        }
        if r.vec8()? != &self.session_id[..self.session_id_len] {
            return Err(TlsError::IllegalParameter);
        }
        if r.u16()? != TLS_AES_128_GCM_SHA256 || r.u8()? != 0 {
            return Err(TlsError::IllegalParameter);
        }
        let mut exts = Reader::new(r.vec16()?);
        r.finish()?;
        let mut version = false;
        let mut peer_key: Option<[u8; X25519_LEN]> = None;
        while !exts.is_empty() {
            let t = exts.u16()?;
            let mut data = Reader::new(exts.vec16()?);
            match t {
                ext::SUPPORTED_VERSIONS if !version => {
                    if data.u16()? != TLS13 {
                        return Err(TlsError::IllegalParameter);
                    }
                    data.finish()?;
                    version = true;
                }
                ext::KEY_SHARE if peer_key.is_none() => {
                    if data.u16()? != X25519 {
                        return Err(TlsError::IllegalParameter);
                    }
                    let key = data.vec16()?;
                    data.finish()?;
                    peer_key = Some(key.try_into().map_err(|_| TlsError::IllegalParameter)?);
                }
                ext::SUPPORTED_VERSIONS | ext::KEY_SHARE => return Err(TlsError::IllegalParameter),
                _ => return Err(TlsError::UnsupportedExtension),
            }
        }
        // A server without supported_versions is offering TLS 1.2 or older (RFC 8446 4.2.1).
        if !version {
            return Err(TlsError::ProtocolVersion);
        }
        let peer_key = peer_key.ok_or(TlsError::IllegalParameter)?;
        let shared = ecdhe::shared_secret(&self.private, &peer_key)?;
        self.private = [0; X25519_LEN];
        self.transcript.update(m);
        let th = self.transcript.current();
        self.handshake_secret = schedule::handshake_secret(&schedule::early_secret(), &shared);
        self.client_hs = schedule::derive_secret(&self.handshake_secret, b"c hs traffic", &th);
        self.server_hs = schedule::derive_secret(&self.handshake_secret, b"s hs traffic", &th);
        self.rx = Some(Protector::new(&schedule::traffic_keys(&self.server_hs)));
        self.tx = Some(Protector::new(&schedule::traffic_keys(&self.client_hs)));
        self.state = State::AwaitEncryptedExtensions;
        Ok(Step { key_change: true, ..Step::quiet() })
    }

    fn encrypted_extensions(&mut self, m: &[u8], body: &[u8]) -> Result<Step, TlsError> {
        let mut r = Reader::new(body);
        let mut exts = Reader::new(r.vec16()?);
        r.finish()?;
        let mut seen = [0u16; MAX_OFFERED];
        let mut n_seen = 0;
        while !exts.is_empty() {
            let t = exts.u16()?;
            let data = exts.vec16()?;
            if seen[..n_seen].contains(&t) {
                return Err(TlsError::IllegalParameter);
            }
            if n_seen == MAX_OFFERED {
                return Err(TlsError::UnsupportedExtension);
            }
            seen[n_seen] = t;
            n_seen += 1;
            match t {
                // These belong in the ServerHello, never here (RFC 8446 4.2).
                ext::SUPPORTED_VERSIONS | ext::KEY_SHARE | ext::SIGNATURE_ALGORITHMS => {
                    return Err(TlsError::IllegalParameter)
                }
                // The server may list the groups it prefers, for next time; this client has one.
                ext::SUPPORTED_GROUPS => {}
                _ if !self.offered(t) => return Err(TlsError::UnsupportedExtension),
                // An acknowledgment of server_name is empty (RFC 6066 3).
                ext::SERVER_NAME if !data.is_empty() => return Err(TlsError::DecodeError),
                ext::RECORD_SIZE_LIMIT => {
                    let mut d = Reader::new(data);
                    let limit = d.u16()? as usize;
                    d.finish()?;
                    // The limit counts the inner content type byte (RFC 8449 4); below 64 is illegal.
                    if limit < 64 {
                        return Err(TlsError::IllegalParameter);
                    }
                    self.max_send = MAX_PLAINTEXT.min(limit - 1);
                }
                _ => {}
            }
        }
        self.transcript.update(m);
        self.state = State::AwaitCertificate;
        Ok(Step::quiet())
    }

    fn certificate(&mut self, m: &[u8], body: &[u8]) -> Result<Step, TlsError> {
        let mut r = Reader::new(body);
        // The request context is empty for a server's certificate (RFC 8446 4.4.2).
        if !r.vec8()?.is_empty() {
            return Err(TlsError::IllegalParameter);
        }
        let list = r.vec24()?;
        r.finish()?;
        self.verifier.certificate_chain(CertificateChain::parse(list)?)?;
        self.transcript.update(m);
        self.state = State::AwaitCertificateVerify;
        Ok(Step::quiet())
    }

    fn certificate_verify(&mut self, m: &[u8], body: &[u8]) -> Result<Step, TlsError> {
        let mut r = Reader::new(body);
        let scheme = r.u16()?;
        let signature = r.vec16()?;
        r.finish()?;
        // What the server signed (RFC 8446 4.4.3): 64 spaces, a context string, a zero, and the transcript.
        const CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify";
        let mut signed = [0x20u8; 64 + CONTEXT.len() + 1 + HASH_LEN];
        signed[64..64 + CONTEXT.len()].copy_from_slice(CONTEXT);
        signed[64 + CONTEXT.len()] = 0;
        signed[64 + CONTEXT.len() + 1..].copy_from_slice(&self.transcript.current());
        self.verifier.handshake_signature(scheme, &signed, signature)?;
        self.transcript.update(m);
        self.state = State::AwaitFinished;
        Ok(Step::quiet())
    }

    fn finished(&mut self, m: &[u8], body: &[u8], out: &mut [u8]) -> Result<Step, TlsError> {
        let expected = schedule::finished_verify_data(&self.server_hs, &self.transcript.current());
        if !schedule::verify_data_matches(&expected, body) {
            return Err(TlsError::BadFinished);
        }
        self.transcript.update(m);
        let th = self.transcript.current();
        let master = schedule::master_secret(&self.handshake_secret);
        self.client_ap = schedule::derive_secret(&master, b"c ap traffic", &th);
        self.server_ap = schedule::derive_secret(&master, b"s ap traffic", &th);

        let mut finished = [0u8; 4 + HASH_LEN];
        finished[0] = msg::FINISHED;
        finished[3] = HASH_LEN as u8;
        finished[4..].copy_from_slice(&schedule::finished_verify_data(&self.client_hs, &th));
        self.transcript.update(&finished);

        let mut written = 0;
        // With a non-empty session id the client keeps up the TLS 1.2 disguise with one change_cipher_spec
        // before its first encrypted record (RFC 8446 D.4).
        if self.session_id_len != 0 {
            written += record::write_plaintext(content::CHANGE_CIPHER_SPEC, LEGACY_VERSION, &[1], out)?;
        }
        let tx = self.tx.as_mut().ok_or(TlsError::UnexpectedMessage)?;
        written += tx.seal(content::HANDSHAKE, &finished, &mut out[written..])?;

        self.rx = Some(Protector::new(&schedule::traffic_keys(&self.server_ap)));
        self.tx = Some(Protector::new(&schedule::traffic_keys(&self.client_ap)));
        // The handshake secrets have done their work.
        self.handshake_secret = [0; HASH_LEN];
        self.client_hs = [0; HASH_LEN];
        self.server_hs = [0; HASH_LEN];
        self.state = State::Connected;
        Ok(Step { event: Event::Connected, written, key_change: true })
    }

    /// The server moved to its next key (RFC 8446 4.6.3), and may ask this client to move too.
    fn key_update(&mut self, body: &[u8], out: &mut [u8]) -> Result<Step, TlsError> {
        let requested = match body {
            [0] => false,
            [1] => true,
            _ => return Err(TlsError::IllegalParameter),
        };
        self.server_ap = next_secret(&self.server_ap);
        self.rx = Some(Protector::new(&schedule::traffic_keys(&self.server_ap)));
        let mut written = 0;
        if requested && !self.write_closed {
            let update = [msg::KEY_UPDATE, 0, 0, 1, 0];
            let tx = self.tx.as_mut().ok_or(TlsError::UnexpectedMessage)?;
            written = tx.seal(content::HANDSHAKE, &update, out)?;
            self.client_ap = next_secret(&self.client_ap);
            self.tx = Some(Protector::new(&schedule::traffic_keys(&self.client_ap)));
        }
        Ok(Step { event: Event::Continue, written, key_change: true })
    }

    /// Protect `data` as one application-data record into `out`. At most [`Client::max_plaintext`] bytes; a
    /// caller with more splits it.
    pub fn seal(&mut self, data: &[u8], out: &mut [u8]) -> Result<usize, TlsError> {
        if let State::Failed(e) = self.state {
            return Err(e);
        }
        if self.state != State::Connected || self.write_closed {
            return Err(TlsError::NotConnected);
        }
        if data.len() > self.max_send {
            return Err(TlsError::RecordTooLarge);
        }
        self.tx.as_mut().ok_or(TlsError::NotConnected)?.seal(content::APPLICATION_DATA, data, out)
    }

    /// Write close_notify, after which nothing more is sent. The server's remaining records can still be
    /// read until its own close_notify arrives.
    pub fn close(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        if self.state != State::Connected || self.write_closed {
            return Err(TlsError::NotConnected);
        }
        let n = self.tx.as_mut().ok_or(TlsError::NotConnected)?.seal(content::ALERT, &[1, 0], out)?;
        self.write_closed = true;
        Ok(n)
    }

    /// After an error: the alert that tells the server why, for the caller to send before closing the
    /// connection, and 0 when there is nothing to say (the server sent the alert, or nothing failed). It is
    /// encrypted once keys exist, as RFC 8446 6 requires.
    pub fn failure_alert(&mut self, out: &mut [u8]) -> Result<usize, TlsError> {
        let State::Failed(e) = self.state else { return Ok(0) };
        let Some(description) = alert_description(e) else { return Ok(0) };
        let alert = [2, description];
        match self.tx.as_mut() {
            Some(tx) if !self.write_closed => tx.seal(content::ALERT, &alert, out),
            Some(_) => Ok(0),
            None => record::write_plaintext(content::ALERT, LEGACY_VERSION, &alert, out),
        }
    }
}

/// application_traffic_secret_N+1 (RFC 8446 7.2).
fn next_secret(secret: &[u8; HASH_LEN]) -> [u8; HASH_LEN] {
    let mut next = [0u8; HASH_LEN];
    schedule::expand_label(secret, b"traffic upd", &[], &mut next);
    next
}

/// The alert for each failure (RFC 8446 6.2), or none when the server already ended the connection.
fn alert_description(e: TlsError) -> Option<u8> {
    Some(match e {
        TlsError::AlertReceived(_) => return None,
        TlsError::UnexpectedMessage => 10,
        TlsError::DecryptFailed => 20,
        TlsError::RecordTooLarge => 22,
        TlsError::HelloRetryRequest => 40,
        TlsError::CertificateRejected(CertProblem::Expired | CertProblem::NotYetValid) => 45,
        TlsError::CertificateRejected(CertProblem::UnknownIssuer) => 48,
        TlsError::CertificateRejected(CertProblem::UnsupportedAlgorithm) => 43,
        // A verifier that was set up wrongly is this end's fault, not the server's.
        TlsError::CertificateRejected(CertProblem::NoRoots | CertProblem::BadRoot) => 80,
        TlsError::CertificateRejected(_) | TlsError::NoCertificate => 42,
        TlsError::IllegalParameter | TlsError::ZeroSharedSecret => 47,
        TlsError::DecodeError | TlsError::BadRecordHeader | TlsError::NoContentType => 50,
        TlsError::BadFinished | TlsError::BadSignature => 51,
        TlsError::ProtocolVersion => 70,
        TlsError::UnsupportedExtension => 110,
        TlsError::CertificateRequested => 116,
        TlsError::HandshakeTooLarge
        | TlsError::BufferTooSmall
        | TlsError::SequenceExhausted
        | TlsError::BadServerName
        | TlsError::NotConnected => 80,
    })
}

/// A DNS name fit for server_name: 1 to 253 bytes of letters, digits, hyphens and dots, no empty label and
/// no trailing dot (RFC 6066 3). Checked before anything is sent, so a malformed name is the caller's error
/// and not the server's.
fn check_server_name(name: &str) -> Result<(), TlsError> {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > 253 {
        return Err(TlsError::BadServerName);
    }
    for label in b.split(|&c| c == b'.') {
        if label.is_empty() || label.len() > 63 || label[0] == b'-' || label[label.len() - 1] == b'-' {
            return Err(TlsError::BadServerName);
        }
        if !label.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-') {
            return Err(TlsError::BadServerName);
        }
    }
    // An IP address is not a server name (RFC 6066 3): a name whose last label is all digits is one.
    let last = b.rsplit(|&c| c == b'.').next().unwrap_or(b);
    if last.iter().all(u8::is_ascii_digit) {
        return Err(TlsError::BadServerName);
    }
    Ok(())
}

/// The ClientHello message, header included (RFC 8446 4.1.2).
fn write_client_hello(hello: &Hello<'_>, public: &[u8; X25519_LEN], out: &mut [u8]) -> Result<usize, TlsError> {
    let mut w = Writer::new(out);
    w.u8(msg::CLIENT_HELLO)?;
    let message = w.begin(3)?;
    w.u16(0x0303)?;
    w.put(&hello.random)?;
    w.u8(32)?;
    w.put(&hello.session_id)?;
    w.u16(2)?;
    w.u16(TLS_AES_128_GCM_SHA256)?;
    w.put(&[1, 0])?; // compression: null only
    let exts = w.begin(2)?;

    w.u16(ext::SERVER_NAME)?;
    let e = w.begin(2)?;
    let list = w.begin(2)?;
    w.u8(0)?; // host_name
    let name = w.begin(2)?;
    w.put(hello.server_name.as_bytes())?;
    w.end(name);
    w.end(list);
    w.end(e);

    w.u16(ext::SUPPORTED_GROUPS)?;
    let e = w.begin(2)?;
    let list = w.begin(2)?;
    w.u16(X25519)?;
    w.end(list);
    w.end(e);

    w.u16(ext::SIGNATURE_ALGORITHMS)?;
    let e = w.begin(2)?;
    let list = w.begin(2)?;
    w.u16(ECDSA_SECP256R1_SHA256)?;
    w.u16(ECDSA_SECP384R1_SHA384)?;
    w.end(list);
    w.end(e);

    w.u16(ext::SUPPORTED_VERSIONS)?;
    let e = w.begin(2)?;
    let list = w.begin(1)?;
    w.u16(TLS13)?;
    w.end(list);
    w.end(e);

    w.u16(ext::KEY_SHARE)?;
    let e = w.begin(2)?;
    let list = w.begin(2)?;
    w.u16(X25519)?;
    let key = w.begin(2)?;
    w.put(public)?;
    w.end(key);
    w.end(list);
    w.end(e);

    w.end(exts);
    w.end(message);
    Ok(w.len())
}
