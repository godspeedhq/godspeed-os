// SPDX-License-Identifier: GPL-2.0-only
//! HTTP/1.1 answer framing: the status line, the headers that frame the body, and the body itself
//! (`Content-Length`, chunked, or to the end of the connection), fed in pieces of any size.
//!
//! Pure `core`, no allocation, so it is tested on the host (`scripts/host_test_check.py`): a framing mistake
//! here would desynchronise a kept-alive connection, and every later answer on it would be read wrong.

/// One answer's status and the headers this client acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    pub status: u16,
    pub length: Option<u64>,
    pub chunked: bool,
    /// The server will close the connection after this answer.
    pub close: bool,
}

/// The body's framing, as the headers said it.
pub enum Body {
    Length(u64),
    Chunked(Chunk),
    UntilClose,
}

/// Where a chunked body's reader is.
#[derive(Clone, Copy)]
pub enum Chunk {
    Size { value: u64, digits: u8, ext: bool, cr: bool },
    Data(u64),
    DataEnd { cr: bool },
    Trailer { line_empty: bool, cr: bool },
}

impl Body {
    pub fn for_head(h: &Head) -> Body {
        if h.chunked {
            Body::Chunked(Chunk::Size { value: 0, digits: 0, ext: false, cr: false })
        } else if let Some(n) = h.length {
            Body::Length(n)
        } else {
            Body::UntilClose
        }
    }

    /// Feed decrypted bytes; the body's own bytes go to `out`. `Ok(true)` once the body is complete.
    pub fn feed(&mut self, mut data: &[u8], out: &mut dyn FnMut(&[u8])) -> Result<bool, &'static str> {
        loop {
            match self {
                Body::Length(left) => {
                    let take = (*left).min(data.len() as u64) as usize;
                    if take > 0 { out(&data[..take]); }
                    *left -= take as u64;
                    return Ok(*left == 0);
                }
                Body::UntilClose => {
                    if !data.is_empty() { out(data); }
                    return Ok(false);
                }
                Body::Chunked(state) => {
                    if data.is_empty() {
                        return Ok(false);
                    }
                    match *state {
                        Chunk::Size { value, digits, ext, cr } => {
                            let c = data[0];
                            data = &data[1..];
                            if cr {
                                if c != b'\n' { return Err("a chunk size line is not ended by CRLF"); }
                                if digits == 0 { return Err("a chunk has no size"); }
                                *state = if value == 0 { Chunk::Trailer { line_empty: true, cr: false } } else { Chunk::Data(value) };
                            } else if c == b'\r' {
                                *state = Chunk::Size { value, digits, ext, cr: true };
                            } else if ext {
                                // A chunk extension: ignored, to the end of the line.
                            } else if c == b';' {
                                *state = Chunk::Size { value, digits, ext: true, cr };
                            } else {
                                let d = match c {
                                    b'0'..=b'9' => c - b'0',
                                    b'a'..=b'f' => c - b'a' + 10,
                                    b'A'..=b'F' => c - b'A' + 10,
                                    _ => return Err("a chunk size is not hexadecimal"),
                                };
                                if digits >= 15 { return Err("a chunk size is too large"); }
                                *state = Chunk::Size { value: value * 16 + d as u64, digits: digits + 1, ext, cr };
                            }
                        }
                        Chunk::Data(left) => {
                            let take = left.min(data.len() as u64) as usize;
                            out(&data[..take]);
                            data = &data[take..];
                            let rest = left - take as u64;
                            *state = if rest == 0 { Chunk::DataEnd { cr: false } } else { Chunk::Data(rest) };
                        }
                        Chunk::DataEnd { cr } => {
                            let c = data[0];
                            data = &data[1..];
                            match (cr, c) {
                                (false, b'\r') => *state = Chunk::DataEnd { cr: true },
                                (true, b'\n') => *state = Chunk::Size { value: 0, digits: 0, ext: false, cr: false },
                                _ => return Err("a chunk is not followed by CRLF"),
                            }
                        }
                        Chunk::Trailer { line_empty, cr } => {
                            let c = data[0];
                            data = &data[1..];
                            if cr {
                                if c != b'\n' { return Err("a trailer line is not ended by CRLF"); }
                                if line_empty { return Ok(true); }
                                *state = Chunk::Trailer { line_empty: true, cr: false };
                            } else if c == b'\r' {
                                *state = Chunk::Trailer { line_empty, cr: true };
                            } else {
                                *state = Chunk::Trailer { line_empty: false, cr: false };
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Where the headers end in `buf`, the blank line included.
pub fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// The status line and the headers that frame the body.
pub fn parse_head(head: &[u8]) -> Result<Head, &'static str> {
    let text = core::str::from_utf8(head).map_err(|_| "the headers are not text")?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or("").splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err("the answer does not begin with an HTTP/1.x status line");
    }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or("the status is not a number")?;
    let mut h = Head { status, length: None, chunked: false, close: version == "HTTP/1.0" };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            h.length = Some(value.parse().map_err(|_| "Content-Length is not a number")?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.eq_ignore_ascii_case("chunked") {
                return Err("a transfer encoding other than chunked");
            }
            h.chunked = true;
        } else if name.eq_ignore_ascii_case("connection") {
            if value.eq_ignore_ascii_case("close") { h.close = true; }
            if value.eq_ignore_ascii_case("keep-alive") { h.close = false; }
        }
    }
    // An answer that has no body by definition (1xx, 204, 304) is length zero.
    if matches!(status, 100..=199 | 204 | 304) {
        h.length = Some(0);
        h.chunked = false;
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(body: &mut Body, parts: &[&[u8]]) -> (bool, [u8; 64], usize) {
        let mut got = [0u8; 64];
        let mut n = 0usize;
        let mut done = false;
        for p in parts {
            done = body.feed(p, &mut |b: &[u8]| { got[n..n + b.len()].copy_from_slice(b); n += b.len(); }).unwrap();
        }
        (done, got, n)
    }

    #[test]
    fn length_framed_body_ends_exactly() {
        let mut b = Body::Length(5);
        let (done, got, n) = run(&mut b, &[b"he", b"llo"]);
        assert!(done);
        assert_eq!(&got[..n], b"hello");
        let mut b = Body::Length(0);
        assert!(b.feed(&[], &mut |_| {}).unwrap());
    }

    #[test]
    fn chunked_body_across_every_split() {
        let wire: &[u8] = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-T: 1\r\n\r\n";
        for split in 0..wire.len() {
            let mut b = Body::for_head(&Head { status: 200, length: None, chunked: true, close: false });
            let (done, got, n) = run(&mut b, &[&wire[..split], &wire[split..]]);
            assert!(done, "split {}", split);
            assert_eq!(&got[..n], b"Wikipedia");
        }
    }

    #[test]
    fn chunked_body_refuses_what_is_not_chunked() {
        let mut b = Body::for_head(&Head { status: 200, length: None, chunked: true, close: false });
        assert!(b.feed(b"zz\r\n", &mut |_| {}).is_err());
        let mut b = Body::for_head(&Head { status: 200, length: None, chunked: true, close: false });
        assert!(b.feed(b"2\r\nabX", &mut |_| {}).is_err());
    }

    #[test]
    fn head_reads_status_and_framing() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 821\r\nConnection: keep-alive\r\n\r\nformat = 1";
        let end = head_end(raw).unwrap();
        assert_eq!(&raw[end..], b"format = 1");
        let h = parse_head(&raw[..end]).unwrap();
        assert_eq!(h, Head { status: 200, length: Some(821), chunked: false, close: false });
        let h = parse_head(b"HTTP/1.1 404 Not Found\r\ntransfer-encoding: chunked\r\n\r\n").unwrap();
        assert_eq!((h.status, h.chunked), (404, true));
        let h = parse_head(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n").unwrap();
        assert_eq!((h.length, h.close), (Some(0), true));
        assert!(parse_head(b"SSH-2.0 hello\r\n\r\n").is_err());
        assert!(parse_head(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n").is_err());
    }
}
