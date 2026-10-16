// SPDX-License-Identifier: GPL-2.0-only
//! One HTTPS request: TLS 1.3 (`sdk/tls`) over one TCP connection held as a capability (`gs::net::Stream`).
//!
//! Every wait has a deadline. The name and the connection keep `gs::net`'s own (`NET_SECS`, 10 s, and
//! `TCP_SECS`, 20 s); everything after them is bounded by `timeout-secs` from the configuration (10.8),
//! counted from the start of the request, so a value smaller than those two does not cut them short.
//! Every way it can end is a [`Fail`] that says where: the clock, the randomness, the name, the connection, the handshake (with
//! TLS's own reason, a certificate's included), or the server's answer.

use godspeed::{self as gs, ServiceContext};
use godspeed_tls::{record_len, Client, Event, Hello, TlsError, WebPkiVerifier};

/// The largest record a server sends, header included (RFC 8446 5.2).
const RECORD_MAX: usize = 5 + (1 << 14) + 256;
/// How much of the server's answer is kept: enough for its status line, headers and a first look at the
/// body. The rest is read and counted, not kept.
pub const KEEP: usize = 2048;
/// A pause between reads that found nothing. The connection's data arrives on `net-stack`'s 100 ms poll,
/// so reading faster finds nothing more.
const IDLE_MS: u64 = 20;

/// Where it ended, when it did not succeed.
#[derive(Clone, Copy, Debug)]
pub enum Fail {
    NoRandom,
    Resolve,
    Connect,
    /// Sending or receiving on the connection failed, or the server closed it early.
    Transport,
    Timeout,
    Tls(TlsError),
    /// A record header the TLS layer refused before reading the rest.
    Framing(TlsError),
}

/// What came back.
pub struct Answer {
    pub ip: [u8; 4],
    pub kept: [u8; KEEP],
    pub kept_len: usize,
    pub total: usize,
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

/// GET `path` from `host:port`, the server checked against `roots` at `now_secs`.
#[inline(never)]
pub fn get(ctx: &ServiceContext, host: &str, port: u16, path: &str, roots: &[&[u8]], now_secs: u64,
           timeout_secs: u32) -> Result<Answer, Fail> {
    let (random, session_id, private) = match (random32(ctx), random32(ctx), random32(ctx)) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Err(Fail::NoRandom),
    };
    let start = gs::task::uptime_secs(ctx);
    let late = || gs::task::uptime_secs(ctx) - start >= timeout_secs as i64;

    let mut net = gs::net::Net::new(ctx);
    let ip = net.resolve(host).map_err(|_| Fail::Resolve)?;
    let mut stream = net.connect(ip, port).map_err(|_| Fail::Connect)?;

    let mut handshake = [0u8; 16 * 1024];
    let mut out = [0u8; RECORD_MAX];
    let hello = Hello { server_name: host, random, session_id, x25519_private: private };
    let verifier = WebPkiVerifier::new(roots, host, now_secs);
    let (mut tls, n) = Client::start(hello, verifier, &mut handshake, &mut out).map_err(Fail::Tls)?;
    send_all(ctx, &mut stream, &out[..n], &late)?;

    let mut answer = Answer { ip: ip.0, kept: [0; KEEP], kept_len: 0, total: 0 };
    let mut rec = [0u8; RECORD_MAX];
    let mut have = 0usize;
    let mut asked = false;
    loop {
        // One whole record: its header says how long, so nothing is read past it.
        let need = if have < 5 { 5 } else { record_len(&rec[..5]).map_err(Fail::Framing)? };
        if have < need {
            let got = match stream.recv(&mut rec[have..need]) {
                Ok(n) => n,
                // `net-stack` released the connection: the server closed it and everything was read.
                Err(_) if asked && answer.total > 0 => break,
                Err(_) => return Err(Fail::Transport),
            };
            if got == 0 {
                if late() {
                    return Err(Fail::Timeout);
                }
                gs::task::sleep_ms(ctx, IDLE_MS);
            }
            have += got;
            continue;
        }
        let result = tls.read_record(&mut rec[..need], &mut out);
        let (event, n) = match result {
            Ok(r) => r,
            Err(e) => {
                // Tell the server why before going, as RFC 8446 6.2 asks; it is a courtesy, so a failure
                // to send it changes nothing about what is reported.
                if let Ok(n) = tls.failure_alert(&mut out) {
                    let _ = send_all(ctx, &mut stream, &out[..n], &late);
                }
                return Err(Fail::Tls(e));
            }
        };
        if n > 0 {
            send_all(ctx, &mut stream, &out[..n], &late)?;
        }
        match event {
            Event::Connected => {
                let mut req = [0u8; 512];
                let len = request(&mut req, host, path);
                let n = tls.seal(&req[..len], &mut out).map_err(Fail::Tls)?;
                send_all(ctx, &mut stream, &out[..n], &late)?;
                asked = true;
            }
            Event::Data(range) => {
                let data = &rec[range];
                let room = KEEP - answer.kept_len;
                let take = data.len().min(room);
                answer.kept[answer.kept_len..answer.kept_len + take].copy_from_slice(&data[..take]);
                answer.kept_len += take;
                answer.total += data.len();
            }
            Event::Closed => break,
            Event::Continue => {}
        }
        have = 0;
    }
    if let Ok(n) = tls.close(&mut out) {
        let _ = send_all(ctx, &mut stream, &out[..n], &late);
    }
    let _ = stream.close();
    Ok(answer)
}

/// Send all of `data`, as much as `net-stack` takes at a time, until the deadline.
fn send_all(ctx: &ServiceContext, stream: &mut gs::net::Stream<'_, '_>, data: &[u8], late: &dyn Fn() -> bool)
    -> Result<(), Fail> {
    let mut off = 0usize;
    while off < data.len() {
        let took = stream.send(&data[off..]).map_err(|_| Fail::Transport)?;
        if took == 0 {
            if late() {
                return Err(Fail::Timeout);
            }
            gs::task::sleep_ms(ctx, IDLE_MS);
        }
        off += took;
    }
    Ok(())
}

/// The request: HTTP/1.1, one connection, nothing a server could mistake for anything else.
fn request(out: &mut [u8; 512], host: &str, path: &str) -> usize {
    let mut n = 0usize;
    for part in ["GET ", path, " HTTP/1.1\r\nHost: ", host, "\r\nUser-Agent: godspeed/0.1\r\nAccept: */*\r\n",
                 "Connection: close\r\n\r\n"] {
        let b = part.as_bytes();
        let take = b.len().min(out.len() - n);
        out[n..n + take].copy_from_slice(&b[..take]);
        n += take;
    }
    n
}
