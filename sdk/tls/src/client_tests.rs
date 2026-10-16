// SPDX-License-Identifier: GPL-2.0-only
//! The client state machine, driven through RFC 8448 section 3's handshake byte for byte, then through
//! every way a server can get it wrong.

use crate::client::{CertificateChain, Client, Event, Hello, ServerVerifier};
use crate::ecdhe;
use crate::record::{self, content, Protector, LEGACY_VERSION};
use crate::rfc8448 as v;
use crate::schedule::{self, Transcript, TrafficKeys, HASH_LEN};
use crate::TlsError;

/// A verifier that accepts everything and remembers what it was shown. Test-only: the crate has no
/// accepting verifier of its own, so a caller cannot reach for one by mistake.
struct Recorder {
    certs: usize,
    leaf: [u8; 512],
    leaf_len: usize,
    scheme: u16,
    signed: [u8; 130],
    signature_len: usize,
    refuse_chain: bool,
    refuse_signature: bool,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            certs: 0,
            leaf: [0; 512],
            leaf_len: 0,
            scheme: 0,
            signed: [0; 130],
            signature_len: 0,
            refuse_chain: false,
            refuse_signature: false,
        }
    }
}

impl ServerVerifier for Recorder {
    fn certificate_chain(&mut self, chain: CertificateChain<'_>) -> Result<(), TlsError> {
        self.certs = chain.certificates().count();
        let leaf = chain.leaf();
        self.leaf[..leaf.len()].copy_from_slice(leaf);
        self.leaf_len = leaf.len();
        if self.refuse_chain {
            return Err(TlsError::CertificateRejected);
        }
        Ok(())
    }

    fn handshake_signature(&mut self, scheme: u16, message: &[u8], signature: &[u8]) -> Result<(), TlsError> {
        self.scheme = scheme;
        self.signed.copy_from_slice(message);
        self.signature_len = signature.len();
        if self.refuse_signature {
            return Err(TlsError::BadSignature);
        }
        Ok(())
    }
}

const SERVER_HS: TrafficKeys = TrafficKeys { key: v::SERVER_HS_KEY, iv: v::SERVER_HS_IV };
const SERVER_AP: TrafficKeys = TrafficKeys { key: v::SERVER_AP_KEY, iv: v::SERVER_AP_IV };
const CLIENT_HS: TrafficKeys = TrafficKeys { key: v::CLIENT_HS_KEY, iv: v::CLIENT_HS_IV };
const CLIENT_AP: TrafficKeys = TrafficKeys { key: v::CLIENT_AP_KEY, iv: v::CLIENT_AP_IV };

/// The client exactly as RFC 8448's was after sending its ClientHello.
fn rfc_client<'b>(verifier: Recorder, buf: &'b mut [u8]) -> Client<'b, Recorder> {
    Client::after_hello(&v::CLIENT_HELLO, v::CLIENT_X25519_PRIVATE, verifier, buf).unwrap()
}

fn plaintext(ctype: u8, body: &[u8]) -> ([u8; 1024], usize) {
    let mut out = [0u8; 1024];
    let n = record::write_plaintext(ctype, LEGACY_VERSION, body, &mut out).unwrap();
    (out, n)
}

/// Seal as the server would, at sequence `seq` under `keys`.
fn server_record(keys: &TrafficKeys, seq: u64, ctype: u8, body: &[u8], out: &mut [u8]) -> usize {
    Protector::at_sequence(keys, seq).seal(ctype, body, out).unwrap()
}

fn read(c: &mut Client<'_, Recorder>, rec: &[u8], out: &mut [u8]) -> Result<(Event, usize), TlsError> {
    let mut buf = [0u8; 2048];
    buf[..rec.len()].copy_from_slice(rec);
    c.read_record(&mut buf[..rec.len()], out)
}

/// RFC 8448's ServerHello record, then its single encrypted record holding the rest of the server flight.
fn rfc_connect(c: &mut Client<'_, Recorder>) -> ([u8; 256], usize) {
    let (sh, n) = plaintext(content::HANDSHAKE, &v::SERVER_HELLO);
    let mut out = [0u8; 256];
    assert_eq!(read(c, &sh[..n], &mut out), Ok((Event::Continue, 0)));
    let r = read(c, &v::SERVER_HS_RECORD, &mut out).unwrap();
    assert_eq!(r.0, Event::Connected);
    (out, r.1)
}

#[test]
fn the_rfc8448_handshake_end_to_end() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (out, n) = rfc_connect(&mut c);
    // The client's whole answer is its Finished, byte for byte the RFC's record (empty session id, so no
    // compatibility change_cipher_spec).
    assert_eq!(&out[..n], &v::CLIENT_FINISHED_RECORD);
    assert!(c.is_connected());

    // The verifier was shown the one certificate and what the server signed.
    let r = c.verifier();
    assert_eq!(r.certs, 1);
    assert_eq!(&r.leaf[..r.leaf_len], &v::CERTIFICATE[11..11 + 0x1b0]);
    assert_eq!(r.scheme, 0x0804);
    assert_eq!(r.signature_len, 0x80);
    let mut t = Transcript::new();
    for m in [&v::CLIENT_HELLO[..], &v::SERVER_HELLO, &v::ENCRYPTED_EXTENSIONS, &v::CERTIFICATE] {
        t.update(m);
    }
    assert_eq!(&r.signed[..64], &[0x20u8; 64][..]);
    assert_eq!(&r.signed[64..98], b"TLS 1.3, server CertificateVerify\0");
    assert_eq!(&r.signed[98..], &t.current());

    // After it: a ticket (dropped), then data each way, then close_notify each way.
    let mut out = [0u8; 256];
    assert_eq!(read(&mut c, &v::SERVER_TICKET_RECORD, &mut out), Ok((Event::Continue, 0)));
    let n = c.seal(&v::CLIENT_APP_PAYLOAD, &mut out).unwrap();
    assert_eq!(&out[..n], &v::CLIENT_APP_RECORD);
    let mut rec = v::SERVER_APP_RECORD;
    let (event, _) = c.read_record(&mut rec, &mut out).unwrap();
    let Event::Data(range) = event else { panic!("expected data, got {event:?}") };
    assert_eq!(&rec[range], &v::SERVER_APP_PAYLOAD);
    let n = c.close(&mut out).unwrap();
    assert_eq!(&out[..n], &v::CLIENT_ALERT_RECORD);
    assert_eq!(c.seal(b"late", &mut out), Err(TlsError::NotConnected));
    assert_eq!(read(&mut c, &v::SERVER_ALERT_RECORD, &mut out), Ok((Event::Closed, 0)));
    assert_eq!(read(&mut c, &v::SERVER_APP_RECORD, &mut out), Err(TlsError::UnexpectedMessage));
}

#[test]
fn a_flight_split_into_small_records_gives_the_same_finished() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (sh, n) = plaintext(content::HANDSHAKE, &v::SERVER_HELLO);
    let mut out = [0u8; 256];
    read(&mut c, &sh[..n], &mut out).unwrap();
    // Seven bytes per record, so every message, and every message header, spans several records.
    let mut rec = [0u8; 64];
    let mut result = None;
    for (seq, chunk) in v::SERVER_HS_RECORD_PAYLOAD.chunks(7).enumerate() {
        let len = server_record(&SERVER_HS, seq as u64, content::HANDSHAKE, chunk, &mut rec);
        result = Some(read(&mut c, &rec[..len], &mut out).unwrap());
        if result.as_ref().unwrap().0 == Event::Connected {
            assert_eq!(seq, v::SERVER_HS_RECORD_PAYLOAD.len().div_ceil(7) - 1, "connected before the last fragment");
        }
    }
    let (event, n) = result.unwrap();
    assert_eq!(event, Event::Connected);
    assert_eq!(&out[..n], &v::CLIENT_FINISHED_RECORD);
}

#[test]
fn change_cipher_spec_is_ignored_during_the_handshake_and_refused_after() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (ccs, n) = plaintext(content::CHANGE_CIPHER_SPEC, &[1]);
    let mut out = [0u8; 256];
    let (sh, m) = plaintext(content::HANDSHAKE, &v::SERVER_HELLO);
    read(&mut c, &sh[..m], &mut out).unwrap();
    assert_eq!(read(&mut c, &ccs[..n], &mut out), Ok((Event::Continue, 0)));
    assert_eq!(read(&mut c, &v::SERVER_HS_RECORD, &mut out).unwrap().0, Event::Connected);
    assert_eq!(read(&mut c, &ccs[..n], &mut out), Err(TlsError::UnexpectedMessage));

    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (bad, n) = plaintext(content::CHANGE_CIPHER_SPEC, &[2]);
    assert_eq!(read(&mut c, &bad[..n], &mut out), Err(TlsError::UnexpectedMessage));
}

/// Replace the server flight's payload, reseal it as the server would and read it.
fn read_altered_flight(alter: impl Fn(&mut [u8]), verifier: Recorder) -> (Result<(Event, usize), TlsError>, [u8; 64], usize) {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(verifier, &mut buf);
    let (sh, n) = plaintext(content::HANDSHAKE, &v::SERVER_HELLO);
    let mut out = [0u8; 256];
    read(&mut c, &sh[..n], &mut out).unwrap();
    let mut payload = v::SERVER_HS_RECORD_PAYLOAD;
    alter(&mut payload);
    let mut rec = [0u8; 1024];
    let len = server_record(&SERVER_HS, 0, content::HANDSHAKE, &payload, &mut rec);
    let r = read(&mut c, &rec[..len], &mut out);
    // Every later call repeats the error, and the alert explaining it is encrypted under the client's
    // handshake key.
    if let Err(e) = r {
        assert_eq!(read(&mut c, &v::SERVER_TICKET_RECORD, &mut out), Err(e));
    }
    let mut alert = [0u8; 64];
    let alert_len = c.failure_alert(&mut alert).unwrap();
    (r, alert, alert_len)
}

fn alert_of(record: &[u8], keys: &TrafficKeys) -> [u8; 2] {
    let mut rec = [0u8; 64];
    rec[..record.len()].copy_from_slice(record);
    let (t, range) = Protector::new(keys).open(&mut rec[..record.len()]).unwrap();
    assert_eq!(t, content::ALERT);
    rec[range].try_into().unwrap()
}

#[test]
fn an_altered_finished_is_refused_and_the_server_is_told() {
    let last = v::SERVER_HS_RECORD_PAYLOAD.len() - 1;
    let (r, alert, n) = read_altered_flight(|p| p[last] ^= 1, Recorder::default());
    assert_eq!(r, Err(TlsError::BadFinished));
    assert_eq!(alert_of(&alert[..n], &CLIENT_HS), [2, 51]);
}

#[test]
fn an_altered_certificate_breaks_the_finished_even_if_the_verifier_accepts_it() {
    // A byte inside the certificate: the verifier here accepts anything, and the Finished still catches it,
    // because the server's Finished covers the transcript the client actually saw.
    let (r, _, _) = read_altered_flight(|p| p[100] ^= 1, Recorder::default());
    assert_eq!(r, Err(TlsError::BadFinished));
}

#[test]
fn the_verifier_decides_the_certificate_and_the_signature() {
    let refuse_chain = Recorder { refuse_chain: true, ..Recorder::default() };
    let (r, alert, n) = read_altered_flight(|_| {}, refuse_chain);
    assert_eq!(r, Err(TlsError::CertificateRejected));
    assert_eq!(alert_of(&alert[..n], &CLIENT_HS), [2, 42]);
    let refuse_signature = Recorder { refuse_signature: true, ..Recorder::default() };
    let (r, alert, n) = read_altered_flight(|_| {}, refuse_signature);
    assert_eq!(r, Err(TlsError::BadSignature));
    assert_eq!(alert_of(&alert[..n], &CLIENT_HS), [2, 51]);
}

#[test]
fn a_certificate_request_ends_the_handshake() {
    // Replace the Certificate message's type with CertificateRequest: this client has nothing to answer.
    let (r, _, _) = read_altered_flight(|p| p[40] = 13, Recorder::default());
    assert_eq!(r, Err(TlsError::CertificateRequested));
}

#[test]
fn an_extension_the_client_never_offered_is_refused() {
    // EncryptedExtensions carries supported_groups (00 0a) at offset 6: make it ALPN (00 10), never offered.
    let (r, _, _) = read_altered_flight(|p| p[7] = 0x10, Recorder::default());
    assert_eq!(r, Err(TlsError::UnsupportedExtension));
}

/// Read a ServerHello built from RFC 8448's with one change.
fn read_server_hello(alter: impl Fn(&mut [u8; 90])) -> Result<(Event, usize), TlsError> {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let mut sh = v::SERVER_HELLO;
    alter(&mut sh);
    let (rec, n) = plaintext(content::HANDSHAKE, &sh);
    let mut out = [0u8; 256];
    read(&mut c, &rec[..n], &mut out)
}

#[test]
fn the_server_hello_must_choose_what_was_offered() {
    // Offsets into RFC 8448's ServerHello: 4 version, 6 random, 38 session id length, 39 suite,
    // 41 compression, 42 extensions length, 44 key_share, 84 supported_versions.
    assert_eq!(read_server_hello(|_| {}), Ok((Event::Continue, 0)));
    assert_eq!(read_server_hello(|s| s[40] = 0x02), Err(TlsError::IllegalParameter), "AES-256 was not offered");
    assert_eq!(read_server_hello(|s| s[41] = 1), Err(TlsError::IllegalParameter), "compression");
    assert_eq!(read_server_hello(|s| s[49] = 0x17), Err(TlsError::IllegalParameter), "P-256 key share");
    assert_eq!(read_server_hello(|s| s[89] = 0x03), Err(TlsError::IllegalParameter), "TLS 1.2");
    assert_eq!(read_server_hello(|s| s[85] = 0x2c), Err(TlsError::UnsupportedExtension), "unknown extension");
    // A one-byte session id the client did not send.
    assert_eq!(read_server_hello(|s| s[38] = 1), Err(TlsError::IllegalParameter), "session id");
}

#[test]
fn a_hello_retry_request_is_refused() {
    const HRR: [u8; 32] = [
        0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2, 0xa2,
        0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
    ];
    assert_eq!(read_server_hello(|s| s[6..38].copy_from_slice(&HRR)), Err(TlsError::HelloRetryRequest));
}

#[test]
fn a_server_hello_without_supported_versions_is_tls12() {
    // Rebuild the ServerHello with only its key_share extension.
    let mut sh = [0u8; 90];
    sh.copy_from_slice(&v::SERVER_HELLO);
    let without = &mut sh[..84];
    without[3] = 80; // message length 0x56 - 6
    without[43] = 0x28; // extensions length 0x2e - 6
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (rec, n) = plaintext(content::HANDSHAKE, without);
    let mut out = [0u8; 64];
    assert_eq!(read(&mut c, &rec[..n], &mut out), Err(TlsError::ProtocolVersion));
    // Nothing is encrypted yet, so the alert goes out in the clear.
    let n = c.failure_alert(&mut out).unwrap();
    assert_eq!(&out[..n], &[content::ALERT, 3, 3, 0, 2, 2, 70]);
}

#[test]
fn application_data_before_the_handshake_is_refused() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let mut out = [0u8; 64];
    assert_eq!(read(&mut c, &v::SERVER_APP_RECORD, &mut out), Err(TlsError::UnexpectedMessage));
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    assert_eq!(c.seal(b"too soon", &mut out), Err(TlsError::NotConnected));
}

#[test]
fn a_message_larger_than_the_buffer_is_refused_not_cut() {
    let mut buf = [0u8; 300];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (sh, n) = plaintext(content::HANDSHAKE, &v::SERVER_HELLO);
    let mut out = [0u8; 64];
    read(&mut c, &sh[..n], &mut out).unwrap();
    // The 445-byte Certificate cannot fit a 300-byte buffer.
    assert_eq!(read(&mut c, &v::SERVER_HS_RECORD, &mut out), Err(TlsError::HandshakeTooLarge));
}

#[test]
fn a_server_alert_ends_the_handshake_and_is_not_answered() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (alert, n) = plaintext(content::ALERT, &[2, 40]);
    let mut out = [0u8; 64];
    assert_eq!(read(&mut c, &alert[..n], &mut out), Err(TlsError::AlertReceived(40)));
    assert_eq!(c.failure_alert(&mut out), Ok(0));
}

#[test]
fn a_key_update_moves_both_directions_when_asked() {
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    rfc_connect(&mut c);
    // The server's first application-keyed record: KeyUpdate, update_requested.
    let mut rec = [0u8; 128];
    let n = server_record(&SERVER_AP, 0, content::HANDSHAKE, &[24, 0, 0, 1, 1], &mut rec);
    let mut out = [0u8; 128];
    let (event, written) = read(&mut c, &rec[..n], &mut out).unwrap();
    assert_eq!(event, Event::Continue);
    // The client answered with KeyUpdate(update_not_requested) under its OLD key...
    let mut answer = [0u8; 128];
    answer[..written].copy_from_slice(&out[..written]);
    let (t, range) = Protector::new(&CLIENT_AP).open(&mut answer[..written]).unwrap();
    assert_eq!((t, &answer[range]), (content::HANDSHAKE, &[24u8, 0, 0, 1, 0][..]));
    // ...and both sides now use the next secrets.
    let mut next = [0u8; HASH_LEN];
    schedule::expand_label(&v::SERVER_AP_TRAFFIC, b"traffic upd", &[], &mut next);
    let server_next = schedule::traffic_keys(&next);
    schedule::expand_label(&v::CLIENT_AP_TRAFFIC, b"traffic upd", &[], &mut next);
    let client_next = schedule::traffic_keys(&next);
    let n = server_record(&server_next, 0, content::APPLICATION_DATA, b"after", &mut rec);
    let mut got = [0u8; 128];
    got[..n].copy_from_slice(&rec[..n]);
    let (event, _) = c.read_record(&mut got[..n], &mut out).unwrap();
    let Event::Data(range) = event else { panic!("{event:?}") };
    assert_eq!(&got[range], b"after");
    let n = c.seal(b"me too", &mut out).unwrap();
    let (_, range) = Protector::new(&client_next).open(&mut out[..n]).unwrap();
    assert_eq!(&out[range], b"me too");
}

#[test]
fn our_client_hello_offers_exactly_what_the_client_accepts() {
    let mut buf = [0u8; 4096];
    let mut out = [0u8; 512];
    let hello = Hello {
        server_name: "godspeed-registry.bankole-422.workers.dev",
        random: [7; 32],
        session_id: [9; 32],
        x25519_private: v::CLIENT_X25519_PRIVATE,
    };
    let (_c, n) = Client::start(hello, Recorder::default(), &mut buf, &mut out).unwrap();
    let rec = &out[..n];
    assert_eq!(&rec[..3], &[content::HANDSHAKE, 3, 1]);
    assert_eq!(u16::from_be_bytes([rec[3], rec[4]]) as usize, n - 5);
    let m = &rec[5..];
    assert_eq!(m[0], 1);
    assert_eq!((m[1] as usize) << 16 | (m[2] as usize) << 8 | m[3] as usize, m.len() - 4);
    assert_eq!(&m[4..6], &[3, 3]);
    assert_eq!(&m[6..38], &[7; 32]);
    assert_eq!(m[38], 32);
    assert_eq!(&m[39..71], &[9; 32]);
    assert_eq!(&m[71..77], &[0, 2, 0x13, 0x01, 1, 0]);
    // The extensions, by type.
    let mut types = [0u16; 8];
    let mut count = 0;
    let exts = &m[79..];
    assert_eq!(u16::from_be_bytes([m[77], m[78]]) as usize, exts.len());
    let mut at = 0;
    let mut key_share = &[][..];
    let mut name = &[][..];
    while at < exts.len() {
        let t = u16::from_be_bytes([exts[at], exts[at + 1]]);
        let len = u16::from_be_bytes([exts[at + 2], exts[at + 3]]) as usize;
        let data = &exts[at + 4..at + 4 + len];
        match t {
            0 => name = &data[5..],
            51 => key_share = data,
            _ => {}
        }
        types[count] = t;
        count += 1;
        at += 4 + len;
    }
    assert_eq!(&types[..count], &[0, 10, 13, 43, 51]);
    assert_eq!(name, b"godspeed-registry.bankole-422.workers.dev");
    assert_eq!(&key_share[..6], &[0, 36, 0, 0x1d, 0, 32]);
    assert_eq!(&key_share[6..], &ecdhe::public_key(&v::CLIENT_X25519_PRIVATE));
}

#[test]
fn our_own_hello_completes_a_handshake_in_compatibility_mode() {
    // Play the server against this client's own ClientHello, with RFC 8448's server key. Not an independent
    // check like the RFC trace above, but the only way to drive `start` and the session-id path end to end.
    let mut buf = [0u8; 4096];
    let mut out = [0u8; 512];
    let hello = Hello { server_name: "registry.example", random: [7; 32], session_id: [9; 32], x25519_private: v::CLIENT_X25519_PRIVATE };
    let (mut c, n) = Client::start(hello, Recorder::default(), &mut buf, &mut out).unwrap();
    let mut t = Transcript::new();
    t.update(&out[5..n]);

    let mut sh = [0u8; 122];
    sh[..4].copy_from_slice(&[2, 0, 0, 118]);
    sh[4..6].copy_from_slice(&[3, 3]);
    sh[6..38].copy_from_slice(&[5; 32]);
    sh[38] = 32;
    sh[39..71].copy_from_slice(&[9; 32]);
    sh[71..76].copy_from_slice(&[0x13, 0x01, 0, 0, 46]);
    sh[76..84].copy_from_slice(&[0, 51, 0, 36, 0, 0x1d, 0, 32]);
    sh[84..116].copy_from_slice(&v::SERVER_X25519_PUBLIC);
    sh[116..122].copy_from_slice(&[0, 43, 0, 2, 3, 4]);
    t.update(&sh);
    let shared = ecdhe::shared_secret(&v::SERVER_X25519_PRIVATE, &v::CLIENT_X25519_PUBLIC).unwrap();
    let hs = schedule::handshake_secret(&schedule::early_secret(), &shared);
    let th = t.current();
    let s_hs = schedule::derive_secret(&hs, b"s hs traffic", &th);
    let c_hs = schedule::derive_secret(&hs, b"c hs traffic", &th);

    let mut flight = [0u8; 6 + 445 + 136 + 36];
    let ee = [8u8, 0, 0, 2, 0, 0];
    let mut at = 0;
    for m in [&ee[..], &v::CERTIFICATE, &v::CERTIFICATE_VERIFY] {
        flight[at..at + m.len()].copy_from_slice(m);
        t.update(m);
        at += m.len();
    }
    flight[at..at + 4].copy_from_slice(&[20, 0, 0, 32]);
    flight[at + 4..].copy_from_slice(&schedule::finished_verify_data(&s_hs, &t.current()));
    t.update(&flight[at..]);
    let expected_finished = schedule::finished_verify_data(&c_hs, &t.current());

    let (rec, n) = plaintext(content::HANDSHAKE, &sh);
    assert_eq!(read(&mut c, &rec[..n], &mut out), Ok((Event::Continue, 0)));
    let (rec, n) = plaintext(content::CHANGE_CIPHER_SPEC, &[1]);
    assert_eq!(read(&mut c, &rec[..n], &mut out), Ok((Event::Continue, 0)));
    let mut rec = [0u8; 1024];
    let n = server_record(&schedule::traffic_keys(&s_hs), 0, content::HANDSHAKE, &flight, &mut rec);
    let (event, written) = read(&mut c, &rec[..n], &mut out).unwrap();
    assert_eq!(event, Event::Connected);
    // A client that sent a session id sends change_cipher_spec before its first encrypted record.
    assert_eq!(&out[..6], &[20, 3, 3, 0, 1, 1]);
    let (t, range) = Protector::new(&schedule::traffic_keys(&c_hs)).open(&mut out[6..written]).unwrap();
    assert_eq!(t, content::HANDSHAKE);
    let fin = &out[6..written][range];
    assert_eq!(&fin[..4], &[20, 0, 0, 32]);
    assert_eq!(&fin[4..], &expected_finished);
}

#[test]
fn server_names_are_checked_before_anything_is_sent() {
    for bad in ["", "a..b", ".a", "a.", "-a.b", "a-.b", "a b", "10.0.0.1", "\u{e9}.com", &"a".repeat(64)] {
        let mut buf = [0u8; 64];
        let mut out = [0u8; 512];
        let hello = Hello { server_name: bad, random: [0; 32], session_id: [0; 32], x25519_private: [1; 32] };
        assert_eq!(Client::start(hello, Recorder::default(), &mut buf, &mut out).err(), Some(TlsError::BadServerName), "{bad:?}");
    }
}

#[test]
fn record_framing_refuses_before_reading_the_body() {
    assert_eq!(record::record_len(&[22, 3, 3, 0, 90]), Ok(95));
    assert_eq!(record::record_len(&[23, 3, 3, 0x41, 0x00]), Ok(5 + 0x4100), "2^14 + 256, the largest");
    assert_eq!(record::record_len(&[23, 3, 3, 0x41, 0x01]), Err(TlsError::RecordTooLarge));
    assert_eq!(record::record_len(&[22, 3, 3, 0x40, 0x01]), Err(TlsError::RecordTooLarge));
    assert_eq!(record::record_len(&[22, 3, 1, 0, 1]), Err(TlsError::BadRecordHeader));
    assert_eq!(record::record_len(&[24, 3, 3, 0, 1]), Err(TlsError::BadRecordHeader));
    assert_eq!(record::record_len(&[21, 3, 3, 0, 0]), Err(TlsError::BadRecordHeader));
}

#[test]
fn handshake_bytes_after_a_key_change_in_the_same_record_are_refused() {
    // The ServerHello changes the keys, so the record carrying it must end with it (RFC 8446 5.1). Put the
    // first bytes of the next message after it, in the clear, where an attacker could have written them.
    let mut body = [0u8; 94];
    body[..90].copy_from_slice(&v::SERVER_HELLO);
    body[90..].copy_from_slice(&v::ENCRYPTED_EXTENSIONS[..4]);
    let mut buf = [0u8; 4096];
    let mut c = rfc_client(Recorder::default(), &mut buf);
    let (rec, n) = plaintext(content::HANDSHAKE, &body);
    let mut out = [0u8; 64];
    assert_eq!(read(&mut c, &rec[..n], &mut out), Err(TlsError::UnexpectedMessage));
}
