// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI. `deny` rather
// than `forbid` for exactly one reason: the exported `service_main` symbol needs `#[allow(unsafe_code)]`.
#![deny(unsafe_code)]
//! `godspeed` - the native package manager (`docs/developer-services-design.md` 10, `utilities/61_godspeed.md`).
//!
//! A FOREGROUND PROGRAM FOR ONE COMMAND, shaped like `ask`: the shell starts it wired to send to the
//! shell, hands it the command line and an end-of-stream byte, reads the answer back as one record table,
//! and ends it. Nothing of it stays running and nothing restarts it.
//!
//! What it can do in this first version, and no more:
//! - `godspeed registry` - the registries this machine knows, and the one in use. `registry add`,
//!   `registry use` and `registry remove` change them. Exactly one is in use, so a package is never
//!   found in two places and there is no priority to decide (`utilities/61_godspeed.md` 2).
//! - `godspeed config` - every plain setting, its value, and whether it came from the file or the
//!   default; `godspeed config <key>` one of them; `godspeed config set <key> <value>` checked, then
//!   written, the rest of the file kept. The file (`/system/services/godspeed/config`) is written with
//!   the defaults on first start, so it always shows what is in effect (10.8).
//! - `godspeed check` - reach the registry in use over TLS, verified to the roots in `tls-roots`, and
//!   say what it answered. Each step is a row, and a failure says which step and why.
//!
//! Searching and fetching packages need the registry's metadata format and its signatures, which are
//! not built yet; this program says so rather than pretending (10.1).

#![no_std]
#![no_main]

mod config;
mod default_root;
mod https;

use core::fmt::Write as _;

use godspeed::{self as gs, ipc::Message, record::{RecordSink, Table, Value}, task::ClockSource, ServiceContext};
use godspeed_tls::{CertProblem, TlsError};

use config::Bad;

/// End of a stream, as every pipe stage uses it.
const EOT: u8 = 0x04;
/// The longest command line taken.
const LINE_MAX: usize = 512;
/// The most roots read from `tls-roots`, and the largest one. A root certificate is under 2 KiB.
const MAX_ROOTS: usize = 8;
const ROOT_MAX: usize = 2048;

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    gs::trace::as_name(&ctx, "godspeed-pm");
    let mut line = [0u8; LINE_MAX];
    let mut len = 0usize;
    loop {
        let m = gs::ipc::recv(&ctx);
        let p = m.payload_bytes();
        if p == [EOT] { break; }
        let take = p.len().min(line.len() - len);
        line[len..len + take].copy_from_slice(&p[..take]);
        len += take;
    }
    let text = core::str::from_utf8(&line[..len]).unwrap_or("");
    let mut words = text.split_whitespace();
    let mut r = Rows::new(&["item", "value", "note"]);
    match (words.next(), words.next()) {
        (Some("config"), None) => config_show(&ctx, None, &mut r),
        (Some("config"), Some("set")) => match (words.next(), words.next(), words.next()) {
            (Some(k), Some(v), None) => config_set(&ctx, k, v, &mut r),
            _ => r.fail("usage", "godspeed config set <key> <value>", "one key, one value"),
        },
        (Some("config"), Some(key)) => match words.next() {
            None => config_show(&ctx, Some(key), &mut r),
            Some(_) => r.fail("usage", "godspeed config [<key>] | godspeed config set <key> <value>", "one key"),
        },
        (Some("registry"), None) => registry_list(&ctx, &mut r),
        (Some("registry"), Some(verb)) => {
            let (a, b, c, d) = (words.next(), words.next(), words.next(), words.next());
            match (verb, a, b, c, d) {
                ("add", Some(name), Some(url), Some(pin), None) => registry_add(&ctx, name, url, pin, &mut r),
                ("use", Some(name), None, None, None) => registry_use(&ctx, name, &mut r),
                ("remove", Some(name), None, None, None) => registry_remove(&ctx, name, &mut r),
                _ => r.fail("usage", "godspeed registry [add <name> <url> <root> | use <name> | remove <name>]",
                            "`godspeed registry help` says what each does"),
            }
        }
        (Some("check"), None) => check(&ctx, &mut r),
        _ => r.fail("usage", "godspeed registry | godspeed config | godspeed check",
                    "`godspeed help` says what each does"),
    }
    r.send(&ctx);
    // DONE. The shell ends this program once it has the end-of-stream byte.
    gs::ipc::park(&ctx);
}

/// The answer being built: a record table whose last row is `status`, `ok` or `failed`.
struct Rows {
    t: Table,
    failed: bool,
}

impl Rows {
    fn new(cols: &[&str]) -> Rows {
        Rows { t: Table::new(cols), failed: false }
    }

    fn row(&mut self, item: &str, value: &str, note: &str) {
        let cells: [Value; 3] = [self.t.intern(item.as_bytes()), self.t.intern(value.as_bytes()), self.t.intern(note.as_bytes())];
        self.t.add_row(&cells);
    }

    fn fail(&mut self, item: &str, value: &str, note: &str) {
        self.row(item, value, note);
        self.failed = true;
    }

    fn send(mut self, ctx: &ServiceContext) {
        let st = if self.failed { "failed" } else { "ok" };
        self.row("status", st, "-");
        let mut sink = BufSink { buf: [0u8; 8 * 1024], len: 0 };
        self.t.encode(&mut sink);
        let Some(dst) = gs::ipc::peer_at(ctx, 0) else { return };
        let mut off = 0usize;
        while off < sink.len {
            let mut n = (sink.len - off).min(4000);
            // A chunk that is a lone 0x04 would read as the end of the stream (as `ask` guards).
            if sink.len - off - n == 1 && sink.buf[sink.len - 1] == EOT { n -= 1; }
            if n == 1 && sink.buf[off] == EOT && off + 1 < sink.len { n = 2; }
            let _ = gs::ipc::send_to(ctx, dst, &Message::from_bytes(&sink.buf[off..off + n]));
            off += n;
        }
        let _ = gs::ipc::send_to(ctx, dst, &Message::from_bytes(&[EOT]));
    }
}

struct BufSink { buf: [u8; 8 * 1024], len: usize }
impl RecordSink for BufSink {
    fn put(&mut self, b: &[u8]) {
        let end = (self.len + b.len()).min(self.buf.len());
        let n = end - self.len;
        self.buf[self.len..end].copy_from_slice(&b[..n]);
        self.len = end;
    }
}

/// A short formatted string, for a table cell.
struct Cell { b: [u8; 160], n: usize }
impl Cell {
    fn new() -> Cell { Cell { b: [0; 160], n: 0 } }
    fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
}
impl core::fmt::Write for Cell {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}
fn cell(args: core::fmt::Arguments<'_>) -> Cell {
    let mut c = Cell::new();
    let _ = c.write_fmt(args);
    c
}

/// Make sure the folder, the file and the default roots exist, writing what is missing, and read the file
/// into `buf`. `Ok(None)` cannot happen after a successful first start; `Err` is a sentence for the row.
fn load<'b>(ctx: &ServiceContext, r: &mut Rows, buf: &'b mut [u8; config::FILE_MAX]) -> Result<usize, Cell> {
    let mut fs = gs::fs::Fs::new(ctx);
    if let Err(e) = fs.create_dir_all(config::DIR) {
        return Err(cell(format_args!("could not create {}: {:?}", config::DIR, e)));
    }
    match fs.read_into(config::PATH, buf) {
        Ok(n) => return Ok(n),
        Err(gs::Error::NotFound) => {}
        Err(gs::Error::BufferTooSmall) => {
            return Err(cell(format_args!("{} is larger than {} bytes", config::PATH, config::FILE_MAX)))
        }
        Err(e) => return Err(cell(format_args!("could not read {}: {:?}", config::PATH, e))),
    }
    // FIRST START: the file shows every default, so nothing in effect is hidden (10.8).
    let n = config::defaults_file(buf);
    if let Err(e) = fs.write(config::PATH, &buf[..n]) {
        return Err(cell(format_args!("could not write the defaults to {}: {:?}", config::PATH, e)));
    }
    r.row("written", config::PATH, "first start: the defaults, so the file shows what is in effect");
    Ok(n)
}

/// The default roots folder is given ISRG Root X2 when it does not exist yet. A folder that exists is
/// left as it is, even empty: removing a root is a decision, and putting it back would undo it.
fn default_roots(ctx: &ServiceContext, r: &mut Rows, folder: &str) {
    if folder != config::DEFAULT_ROOTS { return; }
    let mut fs = gs::fs::Fs::new(ctx);
    if fs.exists(config::DEFAULT_ROOTS).unwrap_or(true) { return; }
    let ok = fs.create_dir_all(config::DEFAULT_ROOTS).is_ok()
        && fs.write(config::DEFAULT_ROOT_FILE, &default_root::ISRG_ROOT_X2).is_ok();
    if ok {
        r.row("written", config::DEFAULT_ROOT_FILE, "first start: the root the registry's chain reaches");
    }
}

fn problem_text(p: &config::Problem<'_>) -> Cell {
    match p.what {
        Bad::UnknownKey(k) => cell(format_args!("line {}: no setting is called '{}'", p.line, k)),
        Bad::NoValue(k) => cell(format_args!("line {}: '{}' has no value", p.line, k)),
        Bad::Twice(k) => cell(format_args!("line {}: '{}' is set twice", p.line, k)),
        Bad::Invalid(k, why) => cell(format_args!("line {}: {} ({})", p.line, why, k)),
        Bad::BadRegistry => cell(format_args!("line {}: a registry is `registry <name> <https url> <sha256:root>`", p.line)),
        Bad::TooManyRegistries => cell(format_args!("line {}: more than {} registries", p.line, config::MAX_REGISTRIES)),
        Bad::NoSuchRegistry(n) => cell(format_args!("line {}: `use {}` names no registry in the file", p.line, n)),
        Bad::NoneInUse => cell(format_args!("several registries and no `use` line - which one is in use is not guessed")),
        Bad::StrayTrustRoot => cell(format_args!("line {}: a signing root belongs on its registry's line now", p.line)),
        Bad::NotText => cell(format_args!("the file is not text")),
    }
}

fn config_show(ctx: &ServiceContext, key: Option<&str>, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(k) = key {
        if matches!(k, "registry" | "trust-root") {
            return r.fail(k, "not a plain setting", "registries are `godspeed registry`: the list, and the one in use");
        }
        let Some(i) = config::KEYS.iter().position(|x| x.name == k) else {
            return r.fail(k, "not a setting", "the settings are tls-roots, timeout-secs, max-package-mib");
        };
        let (v, src) = c.values[i];
        r.row(k, v, src.word());
    } else {
        for (i, k) in config::KEYS.iter().enumerate() {
            let (v, src) = c.values[i];
            r.row(k.name, v, src.word());
        }
        default_roots(ctx, r, c.get("tls-roots"));
    }
    if let Some(p) = c.problem {
        r.fail("problem", config::PATH, problem_text(&p).as_str());
    }
}

fn config_set(ctx: &ServiceContext, key: &str, value: &str, r: &mut Rows) {
    if !config::KEYS.iter().any(|k| k.name == key) {
        return r.fail(key, "not a setting", "the settings are tls-roots, timeout-secs, max-package-mib");
    }
    // A registry is a name, an address and the root it is signed by, so it is never one setting.
    if matches!(key, "registry" | "trust-root") {
        return r.fail(key, "not set here", "registries are `godspeed registry add`, `use` and `remove`");
    }
    if let Err(why) = config::check(key, value) {
        return r.fail(key, value, why);
    }
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let Ok(text) = core::str::from_utf8(&buf[..n]) else {
        return r.fail("config", config::PATH, "the file is not text; fix or delete it first");
    };
    let mut out = [0u8; config::FILE_MAX];
    let Some(m) = config::with_value(text, key, value, &mut out) else {
        return r.fail(key, value, "the file would be larger than 2 KiB");
    };
    // The new file must read back cleanly, or it is not written: a set must not create a problem.
    let after = config::parse(Some(&out[..m]));
    if let Some(p) = after.problem {
        return r.fail("config", config::PATH, problem_text(&p).as_str());
    }
    if let Err(e) = gs::fs::Fs::new(ctx).write(config::PATH, &out[..m]) {
        return r.fail(key, value, cell(format_args!("could not write {}: {:?}", config::PATH, e)).as_str());
    }
    r.row(key, value, "file");
}

/// The file read and parsed, ready for a registry command to change. Refuses a file with a problem: a
/// change must not be written over a file that does not say what it means.
fn registries_for_change<'b>(ctx: &ServiceContext, r: &mut Rows, buf: &'b mut [u8; config::FILE_MAX])
    -> Option<(config::Config<'b>, &'b str)> {
    let n = match load(ctx, r, buf) {
        Ok(n) => n,
        Err(why) => { r.fail("config", "unreadable", why.as_str()); return None; }
    };
    let buf: &'b [u8; config::FILE_MAX] = buf;
    let Ok(text) = core::str::from_utf8(&buf[..n]) else {
        r.fail("config", config::PATH, "the file is not text; fix or delete it first");
        return None;
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        r.fail("problem", config::PATH, problem_text(&p).as_str());
        return None;
    }
    Some((c, text))
}

/// Write the registries back, checking first that the new file reads as what was meant.
fn write_registries(ctx: &ServiceContext, r: &mut Rows, text: &str, regs: &[config::Registry<'_>], using: &str) -> bool {
    let mut out = [0u8; config::FILE_MAX];
    let Some(m) = config::with_registries(text, regs, using, &mut out) else {
        r.fail("config", config::PATH, "the file would be larger than 2 KiB");
        return false;
    };
    let after = config::parse(Some(&out[..m]));
    if let Some(p) = after.problem {
        r.fail("config", config::PATH, problem_text(&p).as_str());
        return false;
    }
    if let Err(e) = gs::fs::Fs::new(ctx).write(config::PATH, &out[..m]) {
        r.fail("config", config::PATH, cell(format_args!("could not be written: {:?}", e)).as_str());
        return false;
    }
    true
}

fn registry_row(r: &mut Rows, reg: &config::Registry<'_>, in_use: bool) {
    let note = cell(format_args!("{}signed by {}", if in_use { "in use; " } else { "" }, reg.pin));
    r.row(reg.name, reg.url, note.as_str());
}

/// `godspeed registry`: every registry, one row each, the one in use said so.
fn registry_list(ctx: &ServiceContext, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("problem", config::PATH, problem_text(&p).as_str());
    }
    let using = c.in_use().name;
    for reg in c.registries() {
        registry_row(r, reg, reg.name == using);
    }
}

/// `godspeed registry add <name> <url> <root>`. The shell has already asked whether to trust the root;
/// this checks every part and refuses a name that is taken. The new registry is known, not in use.
fn registry_add(ctx: &ServiceContext, name: &str, url: &str, pin: &str, r: &mut Rows) {
    let new = config::Registry { name, url, pin };
    if let Err(why) = config::check_registry(&new) {
        return r.fail(name, url, why);
    }
    let mut buf = [0u8; config::FILE_MAX];
    let Some((c, text)) = registries_for_change(ctx, r, &mut buf) else { return };
    if c.find(name).is_some() {
        return r.fail(name, "already known", "remove it first, or add the new one under another name");
    }
    if c.registries().len() == config::MAX_REGISTRIES {
        return r.fail(name, "not added", cell(format_args!("a machine knows at most {} registries", config::MAX_REGISTRIES)).as_str());
    }
    let mut regs = [config::DEFAULT_REGISTRY; config::MAX_REGISTRIES];
    let k = c.registries().len();
    regs[..k].copy_from_slice(c.registries());
    regs[k] = new;
    if write_registries(ctx, r, text, &regs[..k + 1], c.in_use().name) {
        registry_row(r, &new, false);
        r.row("next", cell(format_args!("godspeed registry use {}", name)).as_str(), "to search and install from it");
    }
}

/// `godspeed registry use <name>`: the one searched and installed from from now on.
fn registry_use(ctx: &ServiceContext, name: &str, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let Some((c, text)) = registries_for_change(ctx, r, &mut buf) else { return };
    let Some(reg) = c.find(name) else {
        return r.fail(name, "not known", "`godspeed registry` lists the registries this machine knows");
    };
    if write_registries(ctx, r, text, c.registries(), name) {
        registry_row(r, &reg, true);
    }
}

/// `godspeed registry remove <name>`. The one in use is refused: something is always in use.
fn registry_remove(ctx: &ServiceContext, name: &str, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let Some((c, text)) = registries_for_change(ctx, r, &mut buf) else { return };
    if c.find(name).is_none() {
        return r.fail(name, "not known", "`godspeed registry` lists the registries this machine knows");
    }
    if c.in_use().name == name {
        return r.fail(name, "in use", "use another one first: `godspeed registry use <name>`");
    }
    let mut regs = [config::DEFAULT_REGISTRY; config::MAX_REGISTRIES];
    let mut k = 0usize;
    for reg in c.registries().iter().filter(|x| x.name != name) {
        regs[k] = *reg;
        k += 1;
    }
    if write_registries(ctx, r, text, &regs[..k], c.in_use().name) {
        r.row(name, "removed", "-");
    }
}

/// Reach the registry over verified TLS, one row per step.
#[inline(never)]
fn check(ctx: &ServiceContext, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("config", config::PATH, problem_text(&p).as_str());
    }
    let using = c.in_use();
    let url = match config::url(using.url) {
        Ok(u) => u,
        Err(why) => return r.fail("registry", using.url, why),
    };
    r.row("registry", using.url, cell(format_args!("{} (in use)", using.name)).as_str());

    // THE CLOCK. A certificate's dates are checked against it, so without one nothing is checked.
    let (now, source) = match gs::task::wall_clock(ctx) {
        Some((now, source)) if now > 0 => (now as u64, source),
        _ => return r.fail("clock", "not set", "a certificate's dates cannot be checked - `date sync` first"),
    };
    let source_word = match source {
        ClockSource::Rtc => "hardware clock",
        ClockSource::Ntp => "network time",
        ClockSource::Floor => "floor from the last boot (a lower bound)",
        ClockSource::Unset => "unset",
    };
    let (y, mo, d, h, mi) = config::civil(now);
    r.row("clock", cell(format_args!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, mo, d, h, mi)).as_str(), source_word);

    // THE ROOTS, read from their folder: adding a CA is adding a file (10.8).
    let folder = c.get("tls-roots");
    default_roots(ctx, r, folder);
    let mut store = [[0u8; ROOT_MAX]; MAX_ROOTS];
    let mut lens = [0usize; MAX_ROOTS];
    let mut count = 0usize;
    let mut too_many = false;
    let mut names = [[0u8; 64]; MAX_ROOTS];
    let mut name_lens = [0usize; MAX_ROOTS];
    let mut fs = gs::fs::Fs::new(ctx);
    let listed = fs.list_dir(folder, |e| {
        if e.is_dir { return true; }
        let name = e.name;
        if count == MAX_ROOTS { too_many = true; return false; }
        let take = name.len().min(64);
        names[count][..take].copy_from_slice(&name[..take]);
        name_lens[count] = take;
        count += 1;
        true
    });
    if listed.is_err() {
        return r.fail("roots", folder, "the folder could not be listed - it holds the certificate authorities TLS accepts");
    }
    if too_many {
        return r.fail("roots", folder, cell(format_args!("more than {} files; TLS takes at most {}", MAX_ROOTS, MAX_ROOTS)).as_str());
    }
    if count == 0 {
        return r.fail("roots", folder, "no certificate authority is trusted, so no server can be");
    }
    for i in 0..count {
        let mut path = Cell::new();
        let _ = write!(path, "{}{}", folder, core::str::from_utf8(&names[i][..name_lens[i]]).unwrap_or("?"));
        match fs.read_into(path.as_str(), &mut store[i]) {
            Ok(n) => lens[i] = n,
            Err(gs::Error::BufferTooSmall) => {
                return r.fail("roots", path.as_str(), "larger than 2 KiB, which no root certificate is")
            }
            Err(e) => return r.fail("roots", path.as_str(), cell(format_args!("could not be read: {:?}", e)).as_str()),
        }
    }
    let mut roots: [&[u8]; MAX_ROOTS] = [&[]; MAX_ROOTS];
    for i in 0..count { roots[i] = &store[i][..lens[i]]; }
    r.row("roots", cell(format_args!("{}", count)).as_str(), folder);

    // THE REQUEST.
    let timeout = config::number_of(c.get("timeout-secs"));
    match https::get(ctx, url.host, url.port, url.base, &roots[..count], now, timeout) {
        Ok(a) => {
            r.row("address", cell(format_args!("{}.{}.{}.{}", a.ip[0], a.ip[1], a.ip[2], a.ip[3])).as_str(), url.host);
            r.row("tls", "verified", "TLS 1.3; the server's certificate chain reaches a trusted root and names it");
            let kept = &a.kept[..a.kept_len];
            let status = first_line(kept);
            let ok = status.starts_with("HTTP/1.1 200") || status.starts_with("HTTP/1.0 200");
            let size = cell(format_args!("{} bytes", a.total));
            if ok { r.row("http", status, size.as_str()); } else { r.fail("http", status, size.as_str()); }
            if let Some(body) = find(kept, b"\r\n\r\n").map(|i| &kept[i + 4..]) {
                r.row("says", first_line(body), "the first line of the registry's answer");
            }
        }
        Err(f) => {
            let (step, why) = failure(&f, folder, source_word);
            r.fail(step, url.host, why.as_str());
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn first_line(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == b'\r' || c == b'\n').unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap_or("(not text)")
}

/// Which step failed, and why, in words a person can act on.
fn failure(f: &https::Fail, folder: &str, clock: &str) -> (&'static str, Cell) {
    use https::Fail::*;
    match *f {
        NoRandom => ("tls", cell(format_args!("this machine has no hardware random source, and a key share must not be guessable"))),
        Resolve => ("address", cell(format_args!("the name did not resolve - is the network up? (`net`)"))),
        Connect => ("connect", cell(format_args!("no TCP connection - net-stack's log says why"))),
        Transport => ("connect", cell(format_args!("the connection broke before the answer was complete"))),
        Timeout => ("connect", cell(format_args!("no answer within timeout-secs"))),
        Framing(e) | Tls(e) => ("tls", match e {
            TlsError::CertificateRejected(p) => match p {
                CertProblem::Expired => cell(format_args!("the server's certificate has expired - or the clock is ahead ({})", clock)),
                CertProblem::NotYetValid => cell(format_args!("the server's certificate is not valid yet - or the clock is behind ({})", clock)),
                CertProblem::UnknownIssuer => cell(format_args!("the certificate does not reach any root in {}", folder)),
                CertProblem::NameMismatch => cell(format_args!("the certificate is for another name")),
                CertProblem::UnsupportedAlgorithm => cell(format_args!("the certificate is not ECDSA P-256 or P-384, which is all this client checks")),
                CertProblem::BadRoot => cell(format_args!("a file in {} is not a certificate", folder)),
                other => cell(format_args!("the certificate was refused: {:?}", other)),
            },
            TlsError::AlertReceived(a) => cell(format_args!("the server ended the handshake (alert {})", a)),
            TlsError::HelloRetryRequest => cell(format_args!("the server does not take X25519, the one key exchange offered")),
            TlsError::ProtocolVersion => cell(format_args!("the server does not speak TLS 1.3")),
            other => cell(format_args!("{:?}", other)),
        }),
    }
}
