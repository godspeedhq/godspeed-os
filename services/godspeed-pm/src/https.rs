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
//! `TCP_SECS`, 20 s); after them, a request ends when the connection has moved NOTHING, either way, for
//! `timeout-secs` from the configuration (10.8). A bound on the whole request would be a bound on the size
//! of what can be downloaded: at 30 s a package of a few megabytes failed on a working link. A stalled
//! connection is what the setting exists to catch, and one that is still delivering is not stalled.
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
pub(crate) const HEAD_MAX: usize = 4096;
/// A pause between reads that found nothing. `net-stack` keeps taking a download's frames in while this
/// waits (it polls every millisecond while frames arrive, into a 256 KiB window), so the pause costs the
/// download nothing but a little latency; it is short so that latency stays small. Measured, it is longer than
/// asked: the kernel ends a short sleep on the next scheduler tick, so each costs about 9 ms on the T630 (the
/// `time` row's waits, 2026-10-20).
pub(crate) const IDLE_MS: u64 = 2;

/// Where a session's time went, in counter ticks: what a download reports so the stage that holds it back is
/// measured rather than guessed (`backlog/93`). The handshake is in it, in both the reads and the decrypting
/// (its records are opened by `read_record` too, the certificate checks with them). Single-threaded, so the
/// parts do not overlap: what is left of a download's time beside them is the disk, the screen, the HTTP
/// framing, and what comes before a session or is sent in one - the name lookup, the TCP connect and the sends.
#[derive(Clone, Copy, Default)]
pub struct Timing {
    /// In `net-stack` reads that brought bytes, and how many there were.
    pub recv: u64,
    pub recvs: u32,
    /// Reads that found nothing yet, and the pauses after them (`IDLE_MS` each).
    pub empty: u32,
    pub idle: u64,
    /// Opening records (TLS's `read_record`).
    pub decrypt: u64,
}

/// Where it ended, when it did not succeed.
#[derive(Clone, Copy, Debug)]
pub enum Fail {
    NoRandom,
    /// The name did not resolve, and how net-stack answered: no server, no such name, or no answer in time.
    Resolve(gs::Error),
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
    /// When the connection last moved a byte, either way: what `timeout-secs` is measured from.
    moved_at: i64,
    tls: Client<'b, WebPkiVerifier<'b>>,
    stream: &'s mut gs::net::Stream<'n, 'c>,
    out: &'b mut [u8; OUT_MAX],
    rec: &'b mut [u8; RECORD_MAX],
    /// How many answers this connection has given.
    pub answered: u32,
    /// `rec[..read_len]` was read from the connection, and `rec[..rec_len]` is the record last handed to TLS:
    /// what follows it was read ahead, and is the start of the next one.
    rec_len: usize,
    read_len: usize,
    broken: bool,
    pub timing: Timing,
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
    let ip = net.resolve(host).map_err(Fail::Resolve)?;
    let mut stream = net.connect(ip, port).map_err(|_| Fail::Connect)?;
    let result = {
        let hello = Hello { server_name: host, random, session_id, x25519_private: private };
        let verifier = WebPkiVerifier::new(roots, host, now_secs);
        let (tls, n) = Client::start(hello, verifier, &mut handshake, &mut out).map_err(Fail::Tls)?;
        let mut s = Session {
            ctx, host, ip: ip.0, timeout_secs, moved_at: gs::task::uptime_secs(ctx), tls, stream: &mut stream, out: &mut out, rec: &mut rec,
            answered: 0, broken: false, rec_len: 0, read_len: 0, timing: Timing::default(),
        };
        s.send_out(n)?;
        // The handshake, to the point where application data may be sent.
        loop {
            match s.read_event()? {
                Some(Event::Connected) => break,
                Some(Event::Closed) | None => return Err(Fail::Closed),
                Some(_) => {}
            }
        }
        let result = f(&mut s);
        if !s.broken {
            if let Ok(n) = s.tls.close(s.out) {
                let _ = s.send_out(n);
            }
        }
        result
    };
    let _ = stream.close();
    Ok(result)
}

impl Session<'_, '_, '_, '_> {
    /// Stalled: nothing has moved, either way, for `timeout-secs`.
    fn stalled(&self) -> bool {
        gs::task::uptime_secs(self.ctx) - self.moved_at >= self.timeout_secs as i64
    }

    fn moved(&mut self) {
        self.moved_at = gs::task::uptime_secs(self.ctx);
    }

    /// Send the first `n` bytes of `out`, as much as `net-stack` takes at a time, until it stalls.
    fn send_out(&mut self, n: usize) -> Result<(), Fail> {
        let mut off = 0usize;
        while off < n {
            let took = self.stream.send(&self.out[off..n]).map_err(|_| Fail::Transport)?;
            if took > 0 {
                self.moved();
            } else {
                if self.stalled() {
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
    fn read_event(&mut self) -> Result<Option<Event>, Fail> {
        // What was read past the last record is the start of this one: to the front with it. Not before now,
        // because the last record's bytes were the caller's until it asked for the next.
        self.rec.copy_within(self.rec_len..self.read_len, 0);
        let mut have = self.read_len - self.rec_len;
        self.rec_len = 0;
        self.read_len = have;
        loop {
            // One whole record: its header says how long. READ AHEAD past it, as much as there is room for: a
            // read is a round trip to `net-stack`, and asking for a record's 5-byte header on its own was one
            // round trip for five bytes, for every 16 KiB of a download.
            let need = if have < 5 { 5 } else { record_len(&self.rec[..5]).map_err(Fail::Framing)? };
            if have < need {
                let t = gs::driver::wait::ticks(self.ctx);
                let got = match self.stream.recv(&mut self.rec[have..]) {
                    Ok(n) => n,
                    Err(_) if have == 0 => return Ok(None),
                    Err(_) => return Err(Fail::Transport),
                };
                let t2 = gs::driver::wait::ticks(self.ctx);
                if got > 0 {
                    self.timing.recv += t2.wrapping_sub(t);
                    self.timing.recvs += 1;
                    self.moved();
                } else {
                    if self.stalled() {
                        return Err(Fail::Timeout);
                    }
                    gs::task::sleep_ms(self.ctx, IDLE_MS);
                    self.timing.empty += 1;
                    self.timing.idle += gs::driver::wait::ticks(self.ctx).wrapping_sub(t);
                }
                have += got;
                self.read_len = have;
                continue;
            }
            self.rec_len = need;
            let t = gs::driver::wait::ticks(self.ctx);
            let opened = self.tls.read_record(&mut self.rec[..need], self.out);
            self.timing.decrypt += gs::driver::wait::ticks(self.ctx).wrapping_sub(t);
            let (event, n) = match opened {
                Ok(r) => r,
                Err(e) => {
                    // Tell the server why before going, as RFC 8446 6.2 asks; a courtesy, so a failure to
                    // send it changes nothing about what is reported.
                    if let Ok(n) = self.tls.failure_alert(self.out) {
                        let _ = self.send_out(n);
                    }
                    return Err(Fail::Tls(e));
                }
            };
            if n > 0 {
                self.send_out(n)?;
            }
            return Ok(Some(event));
        }
    }

    /// GET `path`. The status and headers come back; the body goes to `sink` only when the status is 200,
    /// and is read to its end otherwise, so the connection stays in step. `sink` returning `false` stops the
    /// request and the session ([`Fail::Refused`]).
    pub fn get(&mut self, path: &str, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
        self.get_sized(path, &mut |_| true, sink)
    }

    /// [`get`](Session::get), with `on_head` told the status and headers of a 200 before its body: what lets a
    /// caller size a file for a body it will stream to disk. `on_head` returning `false` stops it as `sink`
    /// would ([`Fail::Refused`]).
    pub fn get_sized(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail> {
        if self.broken {
            return Err(Fail::Closed);
        }
        let r = self.get_inner(path, on_head, sink);
        match r {
            Ok(h) if !h.close => {}
            _ => self.broken = true,
        }
        r
    }

    fn get_inner(&mut self, path: &str, on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool)
        -> Result<Head, Fail> {
        // A new request starts its own clock: the wait between two requests is the caller's, not a stall.
        self.moved();
        let mut req = [0u8; 512];
        let len = request(&mut req, self.host, path);
        let n = self.tls.seal(&req[..len], self.out).map_err(Fail::Tls)?;
        self.send_out(n)?;

        let mut head_buf = [0u8; HEAD_MAX];
        let mut head_len = 0usize;
        let mut head: Option<Head> = None;
        let mut body = Body::Length(0);
        loop {
            let range = match self.read_event()? {
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
                if h.status == 200 && !on_head(&h) {
                    return Err(Fail::Refused);
                }
                head = Some(h);
                body = Body::for_head(&h);
                // What came after the headers in this record is the start of the body.
                from += end - before;
            }
            let h = head.unwrap_or(Head::new(0, None, false, true));
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
pub(crate) fn request(out: &mut [u8; 512], host: &str, path: &str) -> usize {
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
