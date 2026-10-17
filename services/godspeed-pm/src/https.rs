// SPDX-License-Identifier: GPL-2.0-only
//! HTTPS: TLS 1.3 (`sdk/tls`) over one TCP connection held as a capability (`gs::net::Stream`).
//!
//! **One session, many requests.** A package fetch is a dozen small requests (the trust metadata, the index,
//! the manifest, the files), and a TLS handshake costs seconds on a Pi 2, so [`session`] opens one connection
//! and [`Session::get`] sends each request on it with `Connection: keep-alive`, reading each answer to its
//! exact end (`http.rs`) so the next one starts where it should. A server that closes the connection instead
//! ends the session ([`Fail::Closed`]), and the caller opens another.
//!
//! **The body is streamed, never held.** It is handed to the caller's sink as it is decrypted, so a file of
//! any size goes straight to `fs` and nothing here grows (26.6.1). Only the status line and the headers are
//! kept, bounded.
//!
//! Every wait has a deadline. The name and the connection keep `gs::net`'s own (`NET_SECS`, 10 s, and
//! `TCP_SECS`, 20 s); each request after them is bounded by `timeout-secs` from the configuration (10.8).
//! Every way it can end is a [`Fail`] that says where: the clock, the randomness, the name, the connection,
//! the handshake (with TLS's own reason, a certificate's included), or the server's answer.

use godspeed::{self as gs, ServiceContext};
use godspeed_tls::{record_len, Client, Event, Hello, TlsError, WebPkiVerifier};

use crate::http::{head_end, parse_head, Body, Head};

/// The largest record a server sends, header included (RFC 8446 5.2).
const RECORD_MAX: usize = 5 + (1 << 14) + 256;
/// What this client SENDS in one go: a ClientHello, a request line and its headers, an alert or the
/// handshake's Finished - none near a full record, so the buffer for them is smaller and the 256 KiB stack
/// keeps room for the trust chain (`chain.rs`).
const OUT_MAX: usize = 4096;
/// The status line and headers of one answer. The registry's are about 900 bytes, most of it a reporting
/// header; an answer whose head is larger is refused rather than cut.
const HEAD_MAX: usize = 4096;
/// A pause between reads that found nothing. The connection's data arrives on `net-stack`'s 100 ms poll,
/// so reading faster finds nothing more.
const IDLE_MS: u64 = 20;

/// Where it ended, when it did not succeed.
#[derive(Clone, Copy, Debug)]
pub enum Fail {
    NoRandom,
    Resolve,
    Connect,
    /// Sending or receiving on the connection failed.
    Transport,
    Timeout,
    Tls(TlsError),
    /// A record header the TLS layer refused before reading the rest.
    Framing(TlsError),
    /// The server closed the connection before this answer was complete. A fresh session may succeed.
    Closed,
    /// The answer is not HTTP this client reads: the text says what.
    Http(&'static str),
    /// The caller's sink refused the body (too large, or not what it should be). The session is over.
    Refused,
}

/// 32 bytes from the hardware random source, or `None` when the machine has none. Never a weaker
/// substitute: a predictable key share is a broken connection that looks like a working one.
fn random32(ctx: &ServiceContext) -> Option<[u8; 32]> {
    let mut r = [0u8; 32];
    for chunk in r.chunks_mut(4) {
        chunk.copy_from_slice(&gs::task::hw_random(ctx)?.to_le_bytes());
    }
    Some(r)
}

/// An open TLS connection to one host, ready for [`Session::get`].
pub struct Session<'c, 'b, 'n, 's> {
    ctx: &'c ServiceContext,
    host: &'c str,
    pub ip: [u8; 4],
    timeout_secs: u32,
    tls: Client<'b, WebPkiVerifier<'b>>,
    stream: &'s mut gs::net::Stream<'n, 'c>,
    out: &'b mut [u8; OUT_MAX],
    rec: &'b mut [u8; RECORD_MAX],
    /// How many answers this connection has given.
    pub answered: u32,
    broken: bool,
}

/// Open a session to `host:port`, the server checked against `roots` at `now_secs`, and run `f` on it.
#[inline(never)]
pub fn session<T>(ctx: &ServiceContext, host: &str, port: u16, roots: &[&[u8]], now_secs: u64, timeout_secs: u32,
                  f: impl FnOnce(&mut Session<'_, '_, '_, '_>) -> T) -> Result<T, Fail> {
    let (random, session_id, private) = match (random32(ctx), random32(ctx), random32(ctx)) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Err(Fail::NoRandom),
    };
    let mut handshake = [0u8; 16 * 1024];
    let mut out = [0u8; OUT_MAX];
    let mut rec = [0u8; RECORD_MAX];
    let mut net = gs::net::Net::new(ctx);
    let ip = net.resolve(host).map_err(|_| Fail::Resolve)?;
    let mut stream = net.connect(ip, port).map_err(|_| Fail::Connect)?;
    let result = {
        let hello = Hello { server_name: host, random, session_id, x25519_private: private };
        let verifier = WebPkiVerifier::new(roots, host, now_secs);
        let (tls, n) = Client::start(hello, verifier, &mut handshake, &mut out).map_err(Fail::Tls)?;
        let mut s = Session {
            ctx, host, ip: ip.0, timeout_secs, tls, stream: &mut stream, out: &mut out, rec: &mut rec,
            answered: 0, broken: false,
        };
        let start = gs::task::uptime_secs(ctx);
        s.send_out(n, start)?;
        // The handshake, to the point where application data may be sent.
        loop {
            match s.read_event(start)? {
                Some(Event::Connected) => break,
                Some(Event::Closed) | None => return Err(Fail::Closed),
                Some(_) => {}
            }
        }
        let result = f(&mut s);
        if !s.broken {
            if let Ok(n) = s.tls.close(s.out) {
                let _ = s.send_out(n, gs::task::uptime_secs(ctx));
            }
        }
        result
    };
    let _ = stream.close();
    Ok(result)
}

impl Session<'_, '_, '_, '_> {
    fn late(&self, start: i64) -> bool {
        gs::task::uptime_secs(self.ctx) - start >= self.timeout_secs as i64
    }

    /// Send the first `n` bytes of `out`, as much as `net-stack` takes at a time, until the deadline.
    fn send_out(&mut self, n: usize, start: i64) -> Result<(), Fail> {
        let mut off = 0usize;
        while off < n {
            let took = self.stream.send(&self.out[off..n]).map_err(|_| Fail::Transport)?;
            if took == 0 {
                if self.late(start) {
                    return Err(Fail::Timeout);
                }
                gs::task::sleep_ms(self.ctx, IDLE_MS);
            }
            off += took;
        }
        Ok(())
    }

    /// The next TLS event, with any reply TLS asks for sent. `None` means the connection ended (the peer
    /// closed it, or `net-stack` released it). A data event's bytes are `self.rec[range]`.
    fn read_event(&mut self, start: i64) -> Result<Option<Event>, Fail> {
        let mut have = 0usize;
        loop {
            // One whole record: its header says how long, so nothing is read past it.
            let need = if have < 5 { 5 } else { record_len(&self.rec[..5]).map_err(Fail::Framing)? };
            if have < need {
                let got = match self.stream.recv(&mut self.rec[have..need]) {
                    Ok(n) => n,
                    Err(_) if have == 0 => return Ok(None),
                    Err(_) => return Err(Fail::Transport),
                };
                if got == 0 {
                    if self.late(start) {
                        return Err(Fail::Timeout);
                    }
                    gs::task::sleep_ms(self.ctx, IDLE_MS);
                }
                have += got;
                continue;
            }
            let (event, n) = match self.tls.read_record(&mut self.rec[..need], self.out) {
                Ok(r) => r,
                Err(e) => {
                    // Tell the server why before going, as RFC 8446 6.2 asks; a courtesy, so a failure to
                    // send it changes nothing about what is reported.
                    if let Ok(n) = self.tls.failure_alert(self.out) {
                        let _ = self.send_out(n, start);
                    }
                    return Err(Fail::Tls(e));
                }
            };
            if n > 0 {
                self.send_out(n, start)?;
            }
            return Ok(Some(event));
        }
    }

    /// GET `path`. The status and headers come back; the body goes to `sink` only when the status is 200,
    /// and is read to its end otherwise, so the connection stays in step. `sink` returning `false` stops the
    /// request and the session ([`Fail::Refused`]).
    pub fn get(&mut self, path: &str, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
        if self.broken {
            return Err(Fail::Closed);
        }
        let r = self.get_inner(path, sink);
        match r {
            Ok(h) if !h.close => {}
            _ => self.broken = true,
        }
        r
    }

    fn get_inner(&mut self, path: &str, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
        let start = gs::task::uptime_secs(self.ctx);
        let mut req = [0u8; 512];
        let len = request(&mut req, self.host, path);
        let n = self.tls.seal(&req[..len], self.out).map_err(Fail::Tls)?;
        self.send_out(n, start)?;

        let mut head_buf = [0u8; HEAD_MAX];
        let mut head_len = 0usize;
        let mut head: Option<Head> = None;
        let mut body = Body::Length(0);
        loop {
            let range = match self.read_event(start)? {
                Some(Event::Data(r)) => r,
                Some(Event::Closed) | None => {
                    // An answer with no stated length ends when the connection does.
                    return match head {
                        Some(h) if h.length.is_none() && !h.chunked => {
                            self.answered += 1;
                            Ok(Head { close: true, ..h })
                        }
                        _ => Err(Fail::Closed),
                    };
                }
                Some(_) => continue,
            };
            let mut from = range.start;
            if head.is_none() {
                let before = head_len;
                let take = (range.end - from).min(HEAD_MAX - head_len);
                head_buf[head_len..head_len + take].copy_from_slice(&self.rec[from..from + take]);
                head_len += take;
                let Some(end) = head_end(&head_buf[..head_len]) else {
                    if head_len == HEAD_MAX {
                        return Err(Fail::Http("the answer's headers are larger than 4 KiB"));
                    }
                    continue;
                };
                let h = parse_head(&head_buf[..end]).map_err(Fail::Http)?;
                head = Some(h);
                body = Body::for_head(&h);
                // What came after the headers in this record is the start of the body.
                from += end - before;
            }
            let h = head.unwrap_or(Head { status: 0, length: None, chunked: false, close: true });
            let deliver = h.status == 200;
            let mut refused = false;
            let done = body.feed(&self.rec[from..range.end], &mut |b: &[u8]| {
                if deliver && !refused && !sink(b) {
                    refused = true;
                }
            }).map_err(Fail::Http)?;
            if refused {
                return Err(Fail::Refused);
            }
            if done {
                self.answered += 1;
                return Ok(h);
            }
        }
    }
}

/// The request: HTTP/1.1, kept alive for the next one, nothing a server could mistake for anything else.
fn request(out: &mut [u8; 512], host: &str, path: &str) -> usize {
    let mut n = 0usize;
    for part in ["GET ", path, " HTTP/1.1\r\nHost: ", host, "\r\nUser-Agent: godspeed/0.2\r\nAccept: */*\r\n",
                 "Connection: keep-alive\r\n\r\n"] {
        let b = part.as_bytes();
        let take = b.len().min(out.len() - n);
        out[n..n + take].copy_from_slice(&b[..take]);
        n += take;
    }
    n
}

/// How much of a body [`get`] keeps: enough for a first look.
pub const KEEP: usize = 2048;

/// What one GET came back with (`godspeed check`).
pub struct Answer {
    pub ip: [u8; 4],
    pub status: u16,
    pub kept: [u8; KEEP],
    pub kept_len: usize,
    pub total: usize,
}

/// GET `path` from `host:port` on a session of its own: the first `KEEP` bytes of the body kept, the rest
/// counted.
pub fn get(ctx: &ServiceContext, host: &str, port: u16, path: &str, roots: &[&[u8]], now_secs: u64,
           timeout_secs: u32) -> Result<Answer, Fail> {
    session(ctx, host, port, roots, now_secs, timeout_secs, |s| {
        let mut a = Answer { ip: s.ip, status: 0, kept: [0; KEEP], kept_len: 0, total: 0 };
        let h = {
            let (kept, kept_len, total) = (&mut a.kept, &mut a.kept_len, &mut a.total);
            s.get(path, &mut |b: &[u8]| {
                let take = b.len().min(KEEP - *kept_len);
                kept[*kept_len..*kept_len + take].copy_from_slice(&b[..take]);
                *kept_len += take;
                *total += b.len();
                true
            })?
        };
        a.status = h.status;
        Ok(a)
    })?
}
