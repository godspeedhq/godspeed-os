// SPDX-License-Identifier: GPL-2.0-only
//! `download <link> <path>`: one file from a web server, written to this machine's disk (`utilities/65_download.md`).
//!
//! **In this program, and not one of its own, on purpose.** It needs exactly what the package manager already has
//! and nothing more: `net-stack` for the connection, `fs` for the file, `time` for the clock a certificate is
//! checked against, TLS (`https.rs`), and the trusted roots in `tls-roots` - one trust store for the machine,
//! not two. It touches nothing of the package store, and it adds no authority.
//!
//! - `https://` is checked as everything here is: TLS 1.3, the chain to a root in `tls-roots`, the name. A link
//!   that fails a check is not downloaded.
//! - `http://` is taken, and the answer says that nothing checked it: anyone between here and the server could
//!   have changed the bytes. A link that BEGAN `https://` and is redirected to `http://` is refused - a link
//!   that looked checked never quietly becomes one that is not. Typing the `http://` link is the way to take it.
//! - Redirects are followed, up to [`MAX_REDIRECTS`].
//! - The file is written to `<path>.partial` and renamed only when its last byte is in: a file at `<path>` is
//!   always a whole one. A download that ends early deletes its partial; one that is ended (`q`) leaves it, and
//!   the next download to that path deletes it first.
//! - The server must say how large the file is (`Content-Length`): `fs` sizes a file before it is written, and
//!   has no append (`gs::fs::Fs::write_at`). Static files nearly always say; a page built as it is sent may not,
//!   and is refused with that reason.
//!
//! Every wait is the stall timer `https.rs` keeps: `timeout-secs` with nothing moving, never a total, so a
//! download of any size that keeps arriving is never cut off.

use core::cell::RefCell;

use godspeed::{self as gs, ServiceContext};

use crate::config::{self, Url};
use crate::http::{head_end, parse_head, Body, Head};
use crate::https::{self, Fail, HEAD_MAX, IDLE_MS};
use crate::{cell, Rows};

/// Redirects followed before giving up: what `wget` and browsers allow, about.
pub const MAX_REDIRECTS: usize = 5;
/// The longest link held while redirects replace it.
const LINK_MAX: usize = 512;

/// What the TLS half needs, read by the caller once: the roots, the clock, the stall bound, and the words a
/// failure is told in.
pub struct Setup<'a> {
    pub roots: &'a [&'a [u8]],
    /// The wall clock, when it is set. Only `https://` needs it.
    pub now: Option<u64>,
    pub clock_word: &'a str,
    pub folder: &'a str,
    pub timeout: u32,
}

/// A link, kept while a redirect replaces it.
struct Link { b: [u8; LINK_MAX], n: usize }

impl Link {
    fn new() -> Link { Link { b: [0; LINK_MAX], n: 0 } }
    fn set(&mut self, parts: &[&str]) -> bool {
        let total: usize = parts.iter().map(|p| p.len()).sum();
        if total > LINK_MAX { return false; }
        let mut n = 0;
        for p in parts {
            self.b[n..n + p.len()].copy_from_slice(p.as_bytes());
            n += p.len();
        }
        self.n = n;
        true
    }
    fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
}

/// The file being written, shared by the head's callback (which sizes it) and the body's (which fills it).
struct Writer<'c> {
    ctx: &'c ServiceContext,
    fs: gs::fs::Fs<'c>,
    partial: &'c str,
    name: &'c str,
    total: u64,
    got: u64,
    written: u64,
    held: usize,
    chunk: [u8; gs::fs::IO_CHUNK],
    created: bool,
    /// The server sent no `Content-Length`.
    no_length: bool,
    err: Option<gs::Error>,
    t0: i64,
    t0_ms: u64,
    shown_step: u64,
    shown_secs: i64,
    /// Ticks spent writing to `fs` and drawing the progress line, and the session's own (`https::Timing`),
    /// for the `time` row.
    disk: u64,
    screen: u64,
    net: https::Timing,
}

impl<'c> Writer<'c> {
    fn on_head(&mut self, h: &Head) -> bool {
        let Some(len) = h.length else { self.no_length = true; return false };
        self.total = len;
        if let Err(e) = self.fs.create_sized(self.partial, len) { self.err = Some(e); return false; }
        self.created = true;
        self.t0 = gs::task::uptime_secs(self.ctx);
        self.t0_ms = gs::driver::wait::counter_ms(self.ctx);
        true
    }

    fn put(&mut self, bytes: &[u8]) -> bool {
        self.got += bytes.len() as u64;
        // The bar, redrawn as `fetch` redraws it: a twenty-eighth, a second, or the end - never per record.
        let step = self.got * 28 / self.total.max(1);
        let secs = gs::task::uptime_secs(self.ctx) - self.t0;
        if step != self.shown_step || secs != self.shown_secs || self.got == self.total {
            let t = gs::driver::wait::ticks(self.ctx);
            self.shown_step = step;
            self.shown_secs = secs;
            let ms = gs::driver::wait::counter_ms(self.ctx).saturating_sub(self.t0_ms);
            crate::progress(self.ctx, format_args!("  downloading {}  [{}]  {}%  {}  {}  {}{}", self.name,
                crate::packages::bar(step as usize, 28).as_str(), self.got * 100 / self.total.max(1),
                crate::packages::of_text(self.got, self.total).as_str(),
                crate::packages::rate_text(self.got, ms).as_str(),
                crate::packages::time_text(secs, self.got, self.total).as_str(),
                if self.got == self.total { "" } else { "   [q] quit" }));
            self.screen += gs::driver::wait::ticks(self.ctx).wrapping_sub(t);
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = rest.len().min(self.chunk.len() - self.held);
            self.chunk[self.held..self.held + take].copy_from_slice(&rest[..take]);
            self.held += take;
            rest = &rest[take..];
            if self.held == self.chunk.len() {
                let t = gs::driver::wait::ticks(self.ctx);
                let wrote = self.fs.write_at(self.partial, self.written, &self.chunk);
                self.disk += gs::driver::wait::ticks(self.ctx).wrapping_sub(t);
                if let Err(e) = wrote { self.err = Some(e); return false; }
                self.written += self.held as u64;
                self.held = 0;
            }
        }
        true
    }

    /// The last, short chunk.
    fn finish(&mut self) -> Result<(), gs::Error> {
        if self.held > 0 {
            self.fs.write_at(self.partial, self.written, &self.chunk[..self.held])?;
            self.written += self.held as u64;
            self.held = 0;
        }
        Ok(())
    }
}

/// Download `typed` to `dest`, an absolute path the shell resolved. Rows say what happened; a failure is a row
/// saying where and why, and leaves no partial behind.
#[inline(never)]
pub fn run(ctx: &ServiceContext, r: &mut Rows, typed: &str, dest: &str, setup: &Setup<'_>) {
    let mut fs = gs::fs::Fs::new(ctx);
    let (dir, name) = match dest.rfind('/') {
        Some(i) if i + 1 < dest.len() => (&dest[..i.max(1)], &dest[i + 1..]),
        _ => return r.fail("file", dest, "not a path to a file"),
    };
    match fs.exists(dest) {
        Ok(true) => return r.fail("file", dest, "already exists - delete it, or name another with `to <file>`"),
        Ok(false) => {}
        Err(e) => return r.fail("file", dest, cell(format_args!("could not be checked: {:?}", e)).as_str()),
    }
    match fs.exists(dir) {
        Ok(true) => {}
        _ => return r.fail("folder", dir, "does not exist - `mkdir` it first"),
    }
    // Not a `Cell`, which is 160 bytes: a path can be longer, and a cut one would name another file.
    let mut partial_buf = [0u8; 512];
    let suffix = b".partial";
    if dest.len() + suffix.len() > partial_buf.len() { return r.fail("file", dest, "a path of at most 500 characters"); }
    partial_buf[..dest.len()].copy_from_slice(dest.as_bytes());
    partial_buf[dest.len()..dest.len() + suffix.len()].copy_from_slice(suffix);
    let partial = core::str::from_utf8(&partial_buf[..dest.len() + suffix.len()]).unwrap_or("");
    // What an ended download left (`q`): ours to replace, since it is named for this file.
    let _ = fs.delete(partial);

    let mut link = Link::new();
    if !link.set(&[typed]) { return r.fail("link", "too long", "a link of at most 512 characters"); }
    let typed_http = typed.starts_with("http://");
    let mut was_tls = false;
    let mut hops = 0usize;
    let began = gs::driver::wait::counter_ms(ctx);
    let w = RefCell::new(Writer {
        ctx, fs: gs::fs::Fs::new(ctx), partial, name, total: 0, got: 0, written: 0, held: 0,
        chunk: [0u8; gs::fs::IO_CHUNK], created: false, no_length: false, err: None, t0: 0, t0_ms: 0, shown_step: u64::MAX, shown_secs: -1,
        disk: 0, screen: 0, net: https::Timing::default(),
    });
    let (head, tls) = loop {
        let (u, tls) = match config::link(link.as_str()) {
            Ok(x) => x,
            Err(why) => return r.fail("link", link.as_str(), why),
        };
        if !tls && was_tls && !typed_http {
            return r.fail("redirect", link.as_str(), "an https:// link was sent to plain http://, where nothing checks what arrives - \
                refused; type the http:// link yourself to take it");
        }
        was_tls |= tls;
        let res = if tls {
            let Some(now) = setup.now else {
                return r.fail("clock", "not set", "a certificate's dates cannot be checked - `date sync` first");
            };
            https::session(ctx, u.host, u.port, setup.roots, now, setup.timeout, |s| {
                let r = s.get_sized(u.base, &mut |h| w.borrow_mut().on_head(h), &mut |b| w.borrow_mut().put(b));
                w.borrow_mut().net = s.timing;
                r
            }).and_then(|x| x)
        } else {
            plain_get(ctx, &u, setup.timeout, &mut |h| w.borrow_mut().on_head(h), &mut |b| w.borrow_mut().put(b))
        };
        let head = match res {
            Ok(h) => h,
            Err(f) => {
                let wr = w.borrow();
                let created = wr.created;
                let (item, why) = if wr.no_length {
                    ("http", cell(format_args!("the server did not say how large the file is, and the disk needs to know first")))
                } else if let Some(e) = wr.err {
                    ("file", cell(format_args!("could not be written: {:?}", e)))
                } else {
                    crate::failure(&f, setup.folder, setup.clock_word)
                };
                drop(wr);
                if created { let _ = fs.delete(partial); }
                return r.fail(item, u.host, why.as_str());
            }
        };
        match head.status {
            200 => break (head, tls),
            301 | 302 | 303 | 307 | 308 => {
                hops += 1;
                if hops > MAX_REDIRECTS {
                    return r.fail("redirect", link.as_str(), cell(format_args!("more than {} redirects", MAX_REDIRECTS)).as_str());
                }
                let Some(to) = head.location.get() else {
                    return r.fail("redirect", link.as_str(), if head.location.too_long() {
                        "a redirect to a link longer than 512 characters"
                    } else { "a redirect that says nowhere to go" });
                };
                let mut next = Link::new();
                let ok = if to.starts_with("http://") || to.starts_with("https://") {
                    next.set(&[to])
                } else if to.starts_with('/') {
                    // On the same server: this link's scheme and authority, the new path.
                    let cur = link.as_str();
                    let scheme_end = cur.find("://").map(|i| i + 3).unwrap_or(0);
                    let auth_end = cur[scheme_end..].find(['/', '?']).map(|i| scheme_end + i).unwrap_or(cur.len());
                    next.set(&[&cur[..auth_end], to])
                } else {
                    // Beside this link's path.
                    let cur = link.as_str();
                    let base_end = cur.rfind('/').map(|i| i + 1).unwrap_or(cur.len());
                    next.set(&[&cur[..base_end], to])
                };
                if !ok { return r.fail("redirect", link.as_str(), "a redirect to a link longer than 512 characters"); }
                link = next;
            }
            other => return r.fail("http", link.as_str(), cell(format_args!("the server answered HTTP {} - nothing was saved", other)).as_str()),
        }
    };
    let _ = head;
    let mut wr = w.borrow_mut();
    if let Err(e) = wr.finish() {
        drop(wr);
        let _ = fs.delete(partial);
        return r.fail("file", dest, cell(format_args!("could not be written: {:?}", e)).as_str());
    }
    if wr.got != wr.total || wr.written != wr.total {
        let (got, total) = (wr.got, wr.total);
        drop(wr);
        let _ = fs.delete(partial);
        return r.fail("http", dest, cell(format_args!("{} of {} bytes arrived - nothing was saved", got, total)).as_str());
    }
    let (total, got) = (wr.total, wr.got);
    let (disk, screen, net) = (wr.disk, wr.screen, wr.net);
    drop(wr);
    if let Err(e) = fs.rename(partial, name) {
        let _ = fs.delete(partial);
        return r.fail("file", dest, cell(format_args!("could not be put in place: {:?}", e)).as_str());
    }
    let ms = gs::driver::wait::counter_ms(ctx).saturating_sub(began).max(1);

    r.columns(&["key", "value", "note"]);
    r.row("saved", dest, crate::packages::size_text(total).as_str());
    r.row("from", link.as_str(), if hops == 0 { cell(format_args!("as typed")) } else {
        cell(format_args!("after {} redirect{} from {}", hops, if hops == 1 { "" } else { "s" }, typed)) }.as_str());
    if tls {
        r.row("checked", "https", "TLS 1.3; the server's certificate chain reaches a trusted root and names it");
    } else {
        r.row("checked", "NOT checked", "plain http: nothing proves these are the bytes the server sent");
    }
    if began != 0 {
        r.row("speed", crate::packages::rate_text(got, ms).as_str(),
              cell(format_args!("{} bytes in {}, the connection included", got, crate::packages::duration_text(ms / 1000).as_str())).as_str());
    }
    if tls {
        // Where the time went (`backlog/93`): the session's reads and records, the disk and the screen. Over
        // plain http the session parts are not measured.
        let per = gs::driver::wait::ticks_per_10ms(ctx);
        let to_ms = |t: u64| if per == 0 { 0 } else { t.saturating_mul(10) / per };
        r.row("time", cell(format_args!("network {} ms, decrypt {} ms, disk {} ms, screen {} ms",
                to_ms(net.recv + net.idle), to_ms(net.decrypt), to_ms(disk), to_ms(screen))).as_str(),
              cell(format_args!("{} reads of net-stack in {} ms; {} found nothing and waited, {} ms", net.recvs,
                to_ms(net.recv), net.empty, to_ms(net.idle))).as_str());
    }
    r.note(format_args!("{}", if tls { "downloaded and checked" } else { "downloaded over plain http - not checked" }));
}

/// GET over plain HTTP: the same framing (`http.rs`) and stall bound as `https.rs`, with no TLS between. The body
/// goes to `sink` only for a 200, which `on_head` is told about first; any other answer comes back with its head
/// and nothing delivered.
fn plain_get(ctx: &ServiceContext, u: &Url<'_>, timeout: u32, on_head: &mut dyn FnMut(&Head) -> bool,
             sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
    let mut net = gs::net::Net::new(ctx);
    let ip = net.resolve(u.host).map_err(Fail::Resolve)?;
    let mut stream = net.connect(ip, u.port).map_err(|_| Fail::Connect)?;
    let r = plain_exchange(ctx, &mut stream, u, timeout, on_head, sink);
    let _ = stream.close();
    r
}

fn plain_exchange(ctx: &ServiceContext, stream: &mut gs::net::Stream<'_, '_>, u: &Url<'_>, timeout: u32,
                  on_head: &mut dyn FnMut(&Head) -> bool, sink: &mut dyn FnMut(&[u8]) -> bool) -> Result<Head, Fail> {
    let mut moved = gs::task::uptime_secs(ctx);
    let stalled = |moved: i64| gs::task::uptime_secs(ctx) - moved >= timeout as i64;
    let mut req = [0u8; 512];
    let n = https::request(&mut req, u.host, u.base);
    let mut off = 0usize;
    while off < n {
        let took = stream.send(&req[off..n]).map_err(|_| Fail::Transport)?;
        if took > 0 { moved = gs::task::uptime_secs(ctx); } else {
            if stalled(moved) { return Err(Fail::Timeout); }
            gs::task::sleep_ms(ctx, IDLE_MS);
        }
        off += took;
    }
    let mut buf = [0u8; 4096];
    let mut head_buf = [0u8; HEAD_MAX];
    let mut head_len = 0usize;
    let mut head: Option<Head> = None;
    let mut body = Body::Length(0);
    loop {
        let got = match stream.recv(&mut buf) {
            Ok(n) => n,
            // The connection ended: an answer with no stated length ends with it.
            Err(_) => return match head {
                Some(h) if h.length.is_none() && !h.chunked => Ok(h),
                _ => Err(Fail::Closed),
            },
        };
        if got == 0 {
            if stalled(moved) { return Err(Fail::Timeout); }
            gs::task::sleep_ms(ctx, IDLE_MS);
            continue;
        }
        moved = gs::task::uptime_secs(ctx);
        let mut from = 0usize;
        if head.is_none() {
            let before = head_len;
            let take = got.min(HEAD_MAX - head_len);
            head_buf[head_len..head_len + take].copy_from_slice(&buf[..take]);
            head_len += take;
            let Some(end) = head_end(&head_buf[..head_len]) else {
                if head_len == HEAD_MAX { return Err(Fail::Http("the answer's headers are larger than 4 KiB")); }
                continue;
            };
            let h = parse_head(&head_buf[..end]).map_err(Fail::Http)?;
            if h.status != 200 { return Ok(h); }
            if !on_head(&h) { return Err(Fail::Refused); }
            head = Some(h);
            body = Body::for_head(&h);
            from = end - before;
        }
        let mut refused = false;
        let done = body.feed(&buf[from..got], &mut |b: &[u8]| {
            if !refused && !sink(b) { refused = true; }
        }).map_err(Fail::Http)?;
        if refused { return Err(Fail::Refused); }
        if done { return head.ok_or(Fail::Closed); }
    }
}
