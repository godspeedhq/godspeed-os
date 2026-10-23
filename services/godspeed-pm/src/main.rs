// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK's audited ABI. `deny` rather
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
//! - `godspeed search <word>`, `godspeed info <publisher/name>`, `godspeed fetch <publisher/name>[@version]`,
//!   `godspeed list available | fetched | installed` and `godspeed remove <publisher/name>[@version]` - the
//!   packages the registry's signed metadata vouches for, verified on this machine by the shared crate's own
//!   checks, and a verified copy FETCHED to `/system/packages/` (`packages.rs`, `chain.rs`). A list answers
//!   with its packages only; an empty one is no rows, not a failure.
//! - `godspeed install <publisher/name>[@version]` and `uninstall` - a fetched package started by the supervisor,
//!   at once and at every boot, with the send peers the operator approved of those its contract requests and no
//!   privilege (`install.rs`; `docs/developer-services-design.md` 12). The supervisor checks the copy from the
//!   disk again, by the same code, before it starts it.

#![no_std]
#![no_main]

mod chain;
mod check;
mod install;
mod keys;
mod config;
mod default_root;
mod download;
mod http;
mod https;
mod meta;
mod packages;
mod source;
mod toml_lines;

use core::fmt::Write as _;

use godspeed::{self as gs, ipc::Message, record::{RecordSink, Table, Value}, task::ClockSource, ServiceContext};
use godspeed_tls::{CertProblem, TlsError};

use config::Bad;

/// End of a stream, as every pipe stage uses it.
const EOT: u8 = 0x04;
/// The longest command line taken.
const LINE_MAX: usize = 512;
/// The most roots read from `tls-roots`, and the largest one. A root certificate is under 2 KiB.
pub(crate) const MAX_ROOTS: usize = 8;
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
        (Some("check"), None) => check(&ctx, &mut r, None),
        // `download`, the shell's own command (`utilities/65_download.md`): the shell resolved the path.
        (Some("download"), Some(link)) => match (words.next(), words.next()) {
            (Some(dest), None) => download(&ctx, &mut r, link, dest),
            _ => r.fail("usage", "download <link> [to <file>]", "one link, and where to put it"),
        },
        (Some("check"), Some(page)) if words.clone().next().is_none() => check(&ctx, &mut r, Some(page)),
        (Some("search"), Some(word)) => match page_from(&mut words) {
            Ok(from) => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::search(&ctx, r, ready, roots, Some(word), from)),
            Err(()) => r.fail("usage", "godspeed search <word> [from <n>]", "`from` and a number: where the page starts"),
        },
        (Some("search"), _) => r.fail("usage", "godspeed search <word>", "a word is needed; `godspeed list available` lists everything"),
        (Some("info"), Some(name)) if words.clone().next().is_none() => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::info(&ctx, r, ready, roots, name)),
        (Some("fetch"), Some(name)) if words.clone().next().is_none() => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::fetch(&ctx, r, ready, roots, name, None)),
        (Some("fetch"), Some(name)) => match (words.next(), words.next(), words.next()) {
            (Some("save"), Some(file), None) => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::fetch(&ctx, r, ready, roots, name, Some(file))),
            _ => r.fail("usage", "godspeed fetch <publisher/name>[@version] [offline]", "`offline` writes it as one file too, for a machine with no network"),
        },
        (Some("list"), Some("available")) => match page_from(&mut words) {
            Ok(from) => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::search(&ctx, r, ready, roots, None, from)),
            Err(()) => r.fail("usage", "godspeed list available [from <n>]", "`from` and a number: where the page starts"),
        },
        (Some("list"), Some("fetched")) if words.clone().next().is_none() => packages::list_fetched(&ctx, &mut r),
        (Some("list"), Some("installed")) if words.clone().next().is_none() => install::list(&ctx, &mut r),
        (Some("list"), Some("updates")) if words.clone().next().is_none() => list_updates(&ctx, &mut r),
        (Some("list"), Some("versions")) => match (words.next(), words.next()) {
            (Some(name), None) => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::versions(&ctx, r, ready, roots, name)),
            _ => r.fail("usage", "godspeed list versions <publisher/name>", "one package"),
        },
        (Some("list"), _) => r.fail("usage", "godspeed list available | fetched | installed | updates | versions <publisher/name>", "say which"),
        (Some("verify"), which) if words.clone().next().is_none() => packages::verify(&ctx, &mut r, which),
        (Some("trust"), None) => trust_show(&ctx, &mut r),
        // The operator's own keys, for unpublished package files (`keys.rs`). The shell asks before `add`.
        (Some("trust"), Some(verb @ ("add" | "remove"))) => match (words.next(), words.next(), words.next()) {
            (Some(p), Some(k), None) if verb == "add" => keys::add(&ctx, &mut r, p, k),
            (Some(p), Some(k), None) => keys::remove(&ctx, &mut r, p, k),
            _ => r.fail("usage", "godspeed trust add | remove <publisher> <key>", "a publisher and the key, as `godspeed key show` prints it"),
        },
        (Some("trust"), Some("reset")) => match (words.next(), words.next()) {
            (Some(name), None) => trust_reset(&ctx, &mut r, name),
            _ => r.fail("usage", "godspeed trust reset <registry>", "one registry, by the name `godspeed registry` lists"),
        },
        (Some("remove"), Some(name)) if words.clone().next().is_none() => packages::remove(&ctx, &mut r, name),
        // The shell asks the operator about each requested peer between the two forms (`install.rs`).
        (Some("install"), Some(name)) => match (words.next(), words.next(), words.next()) {
            (None, _, _) => install::requests(&ctx, &mut r, name),
            (Some("grant"), Some(peers), None) => install::install(&ctx, &mut r, name, peers),
            _ => r.fail("usage", "godspeed install <publisher/name>[@version] [grant <peer>,... | grant none]",
                        "the shell asks about each peer the contract requests"),
        },
        // A PACKAGE FILE: `unpack` is the shell's first step of `godspeed install <file>.gsp`, which then installs
        // what it unpacked as any fetched package (`docs/developer-services-design.md` 15).
        // A FILE, or a release on the registry: checked as installing would check it, and nothing kept.
        (Some("inspect"), Some(file)) if words.clone().next().is_none() && file.ends_with(".gsp") => unpack(&ctx, &mut r, file, false, true),
        (Some("inspect"), Some(name)) if words.clone().next().is_none() =>
            with_ready(&ctx, &mut r, None, |ready, roots, r| packages::inspect(&ctx, r, ready, roots, name)),
        (Some("inspect"), _) => r.fail("usage", "godspeed inspect <publisher/name>[@version] | <file>.gsp",
                                       "a release on the registry, or a package file"),
        (Some("unpack"), Some(file)) => match (words.next(), words.next()) {
            (None, _) => unpack(&ctx, &mut r, file, false, false),
            (Some("accept-old"), None) => unpack(&ctx, &mut r, file, true, false),
            _ => r.fail("usage", "godspeed install <file>.gsp [yes] [accept-old]", "a package file, as the CLI writes one"),
        },
        (Some("install"), None) => r.fail("usage", "godspeed install <publisher/name>[@version]", "a fetched package to install"),
        // What installing a package takes: its requirements, resolved, in the order to install them (`install.rs`).
        (Some("plan"), Some(name)) if words.clone().next().is_none() => install::plan(&ctx, &mut r, name),
        (Some("resolve"), Some(name)) => match (words.next(), words.next()) {
            (Some(range), None) => with_ready(&ctx, &mut r, None, |ready, roots, r| packages::resolve(&ctx, r, ready, roots, name, range)),
            _ => r.fail("usage", "godspeed resolve <publisher/name> <range>", "a package and the versions that will do"),
        },
        // What the package manager keeps that nothing uses: listed, and deleted only once the shell has asked.
        (Some("cleanup"), None) => packages::cleanup(&ctx, &mut r, false),
        (Some("cleanup"), Some("delete")) if words.clone().next().is_none() => packages::cleanup(&ctx, &mut r, true),
        (Some("cleanup"), _) => r.fail("usage", "godspeed cleanup [yes]", "lists what nothing uses and asks before deleting it"),
        (Some("uninstall"), Some(name)) if words.clone().next().is_none() => install::uninstall(&ctx, &mut r, name, false),
        // PURGE: uninstalled, and every fetched version of it deleted - what `uninstall` and `remove` did as two
        // commands. The shell asked first, naming both. A package file the operator keeps is theirs, not touched.
        (Some("uninstall"), Some(name)) => match (words.next(), words.next()) {
            (Some("purge"), None) => {
                install::uninstall(&ctx, &mut r, name, true);
                if !r.failed { packages::purge_fetched(&ctx, &mut r, name); }
            }
            _ => r.fail("usage", "godspeed uninstall <publisher/name> [purge]", "`purge` deletes its fetched copies too"),
        },
        (Some("uninstall"), _) => r.fail("usage", "godspeed uninstall <publisher/name> [purge]", "one installed package"),
        _ => r.fail("usage", "godspeed search | info | fetch | list | verify | remove | trust | registry | config | check",
                    "`godspeed help` says what each does"),
    }
    r.send(&ctx);
    // DONE. The shell ends this program once it has the end-of-stream byte.
    gs::ipc::park(&ctx);
}

/// `unpack <file>`: the settings it needs, and the clock if it is set. Unlike every registry command, a package file
/// is checked with the clock unset - its chain is checked as of its own date - and then only installs if accepted.
#[inline(never)]
fn unpack(ctx: &ServiceContext, r: &mut Rows, file: &str, accept_old: bool, inspect: bool) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("config", config::PATH, problem_text(&p).as_str());
    }
    let clock = match gs::task::wall_clock(ctx) {
        Some((now, _)) if now > 0 => Some(now as u64),
        _ => None,
    };
    let max_bytes = config::number_of(c.get("max-package-mib")) as u64 * 1024 * 1024;
    let max_age = config::number_of(c.get("offline-max-age-days")) as u64;
    packages::unpack(ctx, r, &c.in_use(), max_bytes, clock, max_age, file, accept_old, inspect);
}

/// `[from <n>]` at the end of a listing: where its page starts. Nothing is 0; anything else is a usage error.
fn page_from<'a>(words: &mut impl Iterator<Item = &'a str>) -> Result<usize, ()> {
    match (words.next(), words.next(), words.next()) {
        (None, _, _) => Ok(0),
        (Some("from"), Some(n), None) => n.parse::<usize>().map_err(|_| ()),
        _ => Err(()),
    }
}

/// The answer being built: a record table whose last row is `status`, `ok` or `failed`.
pub(crate) struct Rows {
    t: Table,
    failed: bool,
    /// The `status` row's note: what a list's rows do not say about themselves (that the registry's chain
    /// verified, or why a list is empty). The shell prints it under the table.
    note: Cell,
    /// The columns are `item value note`, the shape every answer had and every failure still has.
    triple: bool,
}

/// One cell of a [`Rows::entry`]: text, or a number a pipe can compare (`where size>1000000`).
#[derive(Clone, Copy)]
pub(crate) enum C<'a> { S(&'a str), N(u64) }

impl Rows {
    fn new(cols: &[&str]) -> Rows {
        Rows { t: Table::new(cols), failed: false, note: Cell::new(), triple: true }
    }

    /// The answer's columns, for a command whose rows are not `item value note`: a LIST names what it lists
    /// (`package version from size state`), and a CARD about one thing is `key value note`. Called before
    /// any row. A failure afterwards still answers in `item value note` (see [`fail`](Rows::fail)).
    pub(crate) fn columns(&mut self, cols: &[&str]) {
        self.t = Table::new(cols);
        self.triple = cols == ["item", "value", "note"];
    }

    pub(crate) fn row(&mut self, item: &str, value: &str, note: &str) {
        self.entry(&[C::S(item), C::S(value), C::S(note)]);
    }

    /// A row of up to eight cells, in the columns' order; the rest are empty.
    pub(crate) fn entry(&mut self, cells: &[C<'_>]) {
        let mut vals = [Value::Empty; 8];
        for (i, c) in cells.iter().enumerate().take(8) {
            vals[i] = match *c { C::S(s) => self.t.intern(s.as_bytes()), C::N(n) => Value::Int(n) };
        }
        let n = self.t.ncols();
        self.t.add_row(&vals[..n]);
    }

    /// Set the status row's note.
    pub(crate) fn note(&mut self, args: core::fmt::Arguments<'_>) {
        self.note = cell(args);
    }

    /// The command failed, and its rows, in the shape they already have, say why (a damaged copy in `verify`).
    pub(crate) fn failed_in_place(&mut self) {
        self.failed = true;
    }

    /// A failure, always as `item value note`: a list or a card that fails part way is not shown half made,
    /// and the failure reads the same whatever the command was.
    pub(crate) fn fail(&mut self, item: &str, value: &str, note: &str) {
        if !self.triple {
            self.t = Table::new(&["item", "value", "note"]);
            self.triple = true;
        }
        self.row(item, value, note);
        self.failed = true;
    }

    fn send(mut self, ctx: &ServiceContext) {
        let st = if self.failed { "failed" } else { "ok" };
        let note = if self.note.n == 0 { cell(format_args!("-")) } else { core::mem::replace(&mut self.note, Cell::new()) };
        self.row("status", st, note.as_str());
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

/// A progress frame for the shell, sent BEFORE the answer: `PROGRESS` and one line of text, which the shell
/// draws in place until the answer arrives - the words on one line and, when the frame has a bar, the bar on
/// the line under them. The shell reads frames only until the first byte of the answer, and sets aside anything
/// else that comes before it, so nothing in an answer can be taken for one.
pub(crate) const PROGRESS: &[u8; 8] = b"\x01GSPROG1";

/// Send one progress line to the shell. A frame that cannot be sent changes nothing: progress is a courtesy,
/// the answer is what counts.
pub(crate) fn progress(ctx: &ServiceContext, args: core::fmt::Arguments<'_>) {
    let Some(dst) = gs::ipc::peer_at(ctx, 0) else { return };
    let mut b = [0u8; 8 + 160];
    b[..8].copy_from_slice(PROGRESS);
    let line = cell(args);
    let n = line.n.min(160);
    b[8..8 + n].copy_from_slice(&line.as_str().as_bytes()[..n]);
    let _ = gs::ipc::send_to(ctx, dst, &Message::from_bytes(&b[..8 + n]));
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
#[derive(Clone, Copy)]
pub(crate) struct Cell { b: [u8; 160], pub(crate) n: usize }
impl Cell {
    pub(crate) fn new() -> Cell { Cell { b: [0; 160], n: 0 } }
    pub(crate) fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
}
impl core::fmt::Write for Cell {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}
pub(crate) fn cell(args: core::fmt::Arguments<'_>) -> Cell {
    let mut c = Cell::new();
    let _ = c.write_fmt(args);
    c
}

/// Make sure the folder and the file exist, writing the defaults where the file is missing, and read the
/// file into `buf`: `Ok` is its length, `Err` a sentence for the row. The default roots are
/// `default_roots`'s, written by the commands that read them.
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
            return r.fail(k, "not a setting", "the settings are tls-roots, timeout-secs, max-package-mib, offline-max-age-days");
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
    // A registry is a name, an address and the root it is signed by, so it is never one setting.
    if matches!(key, "registry" | "trust-root") {
        return r.fail(key, "not set here", "registries are `godspeed registry add`, `use` and `remove`");
    }
    if !config::KEYS.iter().any(|k| k.name == key) {
        return r.fail(key, "not a setting", "the settings are tls-roots, timeout-secs, max-package-mib, offline-max-age-days");
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

/// `download <link> <path>`: the settings, the clock if it is set, and the roots, then `download::run`.
#[inline(never)]
fn download(ctx: &ServiceContext, r: &mut Rows, link: &str, dest: &str) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("config", config::PATH, problem_text(&p).as_str());
    }
    // The clock only matters to https; a plain http download needs none, so its absence is not a failure here.
    let (now, clock_word) = match gs::task::wall_clock(ctx) {
        Some((t, src)) if t > 0 => (Some(t as u64), match src {
            ClockSource::Rtc => "hardware clock",
            ClockSource::Ntp => "network time",
            ClockSource::Floor => "floor from the last boot (a lower bound)",
            ClockSource::Unset => "unset",
        }),
        _ => (None, "unset"),
    };
    let folder = c.get("tls-roots");
    let mut store = Roots::new();
    if !read_roots(ctx, r, folder, &mut store) { return; }
    let mut roots: [&[u8]; MAX_ROOTS] = [&[]; MAX_ROOTS];
    for i in 0..store.count { roots[i] = &store.store[i][..store.lens[i]]; }
    let setup = download::Setup {
        roots: &roots[..store.count], now, clock_word, folder, timeout: config::number_of(c.get("timeout-secs")),
    };
    download::run(ctx, r, link, dest, &setup);
}

/// Reach the registry over verified TLS, one row per step.
#[inline(never)]
/// `godspeed check`: the registry in use, one row per step. With an address, that one page instead, read to its
/// end and thrown away: how fast this machine downloads from there, through every step a fetch takes.
fn check(ctx: &ServiceContext, r: &mut Rows, page: Option<&str>) {
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
    let url = match page {
        Some(p) => match config::page(p) {
            Ok(u) => { r.row("page", p, "read to its end and thrown away"); u }
            Err(why) => return r.fail("page", p, why),
        },
        None => match config::url(using.url) {
            Ok(u) => { r.row("registry", using.url, cell(format_args!("{} (in use)", using.name)).as_str()); u }
            Err(why) => return r.fail("registry", using.url, why),
        },
    };

    let Some((now, source_word)) = clock(ctx, r) else { return };
    let (y, mo, d, h, mi) = config::civil(now);
    r.row("clock", cell(format_args!("{:04}-{:02}-{:02} {:02}:{:02} UTC", y, mo, d, h, mi)).as_str(), source_word);

    let folder = c.get("tls-roots");
    let mut store = Roots::new();
    if !read_roots(ctx, r, folder, &mut store) { return; }
    let mut roots: [&[u8]; MAX_ROOTS] = [&[]; MAX_ROOTS];
    for i in 0..store.count { roots[i] = &store.store[i][..store.lens[i]]; }
    let count = store.count;
    r.row("roots", cell(format_args!("{}", count)).as_str(), folder);

    // THE REQUEST.
    let timeout = config::number_of(c.get("timeout-secs"));
    let began = gs::driver::wait::counter_ms(ctx);
    match https::get(ctx, url.host, url.port, url.base, &roots[..count], now, timeout) {
        Ok(a) => {
            // HOW FAST, from the name lookup to the last byte: what a download of this size costs on this link.
            let ms = gs::driver::wait::counter_ms(ctx).saturating_sub(began).max(1);
            r.row("address", cell(format_args!("{}.{}.{}.{}", a.ip[0], a.ip[1], a.ip[2], a.ip[3])).as_str(), url.host);
            r.row("tls", "verified", "TLS 1.3; the server's certificate chain reaches a trusted root and names it");
            let status = cell(format_args!("HTTP/1.1 {}{}", a.status, if a.status == 200 { " OK" } else { "" }));
            let size = cell(format_args!("{} bytes", a.total));
            if a.status == 200 { r.row("http", status.as_str(), size.as_str()); } else { r.fail("http", status.as_str(), size.as_str()); }
            if a.status == 200 && began != 0 {
                let kib_s = a.total as u64 * 1000 / 1024 / ms;
                r.row("speed", cell(format_args!("{} KiB/s", kib_s)).as_str(),
                      cell(format_args!("{} bytes in {}.{:03} s, the handshake included", a.total, ms / 1000, ms % 1000)).as_str());
            }
            if a.status == 200 && a.kept_len > 0 && page.is_none() {
                r.row("says", first_line(&a.kept[..a.kept_len]), "the first line of the registry's answer");
            }
        }
        Err(f) => {
            let (step, why) = failure(&f, folder, source_word);
            r.fail(step, url.host, why.as_str());
        }
    }
}

/// THE CLOCK. A certificate's dates, and the trust metadata's expiries, are checked against it, so without
/// one nothing is checked.
fn clock(ctx: &ServiceContext, r: &mut Rows) -> Option<(u64, &'static str)> {
    let (now, source) = match gs::task::wall_clock(ctx) {
        Some((now, source)) if now > 0 => (now as u64, source),
        _ => {
            r.fail("clock", "not set", "a certificate's dates cannot be checked - `date sync` first");
            return None;
        }
    };
    Some((now, match source {
        ClockSource::Rtc => "hardware clock",
        ClockSource::Ntp => "network time",
        ClockSource::Floor => "floor from the last boot (a lower bound)",
        ClockSource::Unset => "unset",
    }))
}

/// The certificate authorities TLS accepts, read from their folder: adding one is adding a file (10.8).
pub(crate) struct Roots {
    pub(crate) store: [[u8; ROOT_MAX]; MAX_ROOTS],
    pub(crate) lens: [usize; MAX_ROOTS],
    pub(crate) count: usize,
}

impl Roots {
    fn new() -> Roots { Roots { store: [[0; ROOT_MAX]; MAX_ROOTS], lens: [0; MAX_ROOTS], count: 0 } }
}

/// Read every root in `folder` into `out`, writing ISRG Root X2 there on first start. `false` after a row
/// saying why there are none to use.
#[inline(never)]
fn read_roots(ctx: &ServiceContext, r: &mut Rows, folder: &str, out: &mut Roots) -> bool {
    default_roots(ctx, r, folder);
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
        r.fail("roots", folder, "the folder could not be listed - it holds the certificate authorities TLS accepts");
        return false;
    }
    if too_many {
        r.fail("roots", folder, cell(format_args!("more than {} files; TLS takes at most {}", MAX_ROOTS, MAX_ROOTS)).as_str());
        return false;
    }
    if count == 0 {
        r.fail("roots", folder, "no certificate authority is trusted, so no server can be");
        return false;
    }
    for i in 0..count {
        let mut path = Cell::new();
        let _ = write!(path, "{}{}", folder, core::str::from_utf8(&names[i][..name_lens[i]]).unwrap_or("?"));
        match fs.read_into(path.as_str(), &mut out.store[i]) {
            Ok(n) => out.lens[i] = n,
            Err(gs::Error::BufferTooSmall) => {
                r.fail("roots", path.as_str(), "larger than 2 KiB, which no root certificate is");
                return false;
            }
            Err(e) => {
                r.fail("roots", path.as_str(), cell(format_args!("could not be read: {:?}", e)).as_str());
                return false;
            }
        }
    }
    out.count = count;
    true
}

/// The configuration, a registry (the one in use, or `which` by name), the clock and the roots, then `f`: what
/// every registry command needs before it connects. Each that is missing is a row saying so, and `f` does not
/// run.
#[inline(never)]
fn with_ready(ctx: &ServiceContext, r: &mut Rows, which: Option<&str>, f: impl FnOnce(&packages::Ready<'_>, &Roots, &mut Rows)) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("config", config::PATH, problem_text(&p).as_str());
    }
    let reg = match which {
        None => c.in_use(),
        Some(name) => match c.find(name) {
            Some(reg) => reg,
            None => return r.fail(name, "not known", "the registry it came from is no longer in `godspeed registry`"),
        },
    };
    let url = match config::url(reg.url) {
        Ok(u) => u,
        Err(why) => return r.fail("registry", reg.url, why),
    };
    let Some((now, _)) = clock(ctx, r) else { return };
    let mut roots = Roots::new();
    if !read_roots(ctx, r, c.get("tls-roots"), &mut roots) { return; }
    let ready = packages::Ready {
        reg, url, now,
        timeout: config::number_of(c.get("timeout-secs")),
        max_bytes: config::number_of(c.get("max-package-mib")) as u64 * 1024 * 1024,
    };
    f(&ready, &roots, r);
}

/// `godspeed list updates`: each fetched package against the registry IT came from (an installed package
/// will update from its origin, `utilities/61_godspeed.md` 2), one registry at a time.
#[inline(never)]
fn list_updates(ctx: &ServiceContext, r: &mut Rows) {
    let mut all = [packages::Fetched::EMPTY_PUB; 32];
    let mut more = false;
    let n = match packages::fetched(ctx, &mut all, &mut more) {
        Ok(n) => n,
        Err(e) => return r.fail("fetched", packages::DIR, cell(format_args!("could not be listed: {:?}", e)).as_str()),
    };
    if n == 0 {
        return r.note(format_args!("nothing is fetched here, so nothing can be newer"));
    }
    let mut done = [false; 32];
    for i in 0..n {
        if done[i] { continue; }
        let reg_name = cell(format_args!("{}", all[i].registry()));
        for j in i..n { if all[j].registry() == reg_name.as_str() { done[j] = true; } }
        // Trusted by the operator, from a file: no registry has a newer one to offer.
        if reg_name.as_str() == check::OPERATOR {
            r.note(format_args!("an unpublished package trusted by the operator is updated by installing a newer file"));
            continue;
        }
        with_ready(ctx, r, Some(reg_name.as_str()), |ready, roots, r| packages::updates(ctx, r, ready, roots, &all[..n]));
    }
}

/// `godspeed trust`: what this machine has accepted from every registry it knows.
#[inline(never)]
fn trust_show(ctx: &ServiceContext, r: &mut Rows) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    if let Some(p) = c.problem {
        return r.fail("problem", config::PATH, problem_text(&p).as_str());
    }
    chain::show(ctx, r, c.registries(), c.in_use().name);
    keys::show(ctx, r);
}

/// `godspeed trust reset <registry>`. The shell asked `[y/N]` first.
#[inline(never)]
fn trust_reset(ctx: &ServiceContext, r: &mut Rows, name: &str) {
    let mut buf = [0u8; config::FILE_MAX];
    let n = match load(ctx, r, &mut buf) {
        Ok(n) => n,
        Err(why) => return r.fail("config", "unreadable", why.as_str()),
    };
    let c = config::parse(Some(&buf[..n]));
    match c.find(name) {
        Some(reg) => chain::reset(ctx, r, &reg),
        None => r.fail(name, "not known", "`godspeed registry` lists the registries this machine knows"),
    }
}

fn first_line(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == b'\r' || c == b'\n').unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).unwrap_or("(not text)")
}

/// Which step failed, and why, in words a person can act on.
pub(crate) fn failure(f: &https::Fail, folder: &str, clock: &str) -> (&'static str, Cell) {
    use https::Fail::*;
    match *f {
        NoRandom => ("tls", cell(format_args!("this machine has no hardware random source, and a key share must not be guessable"))),
        // Which of the ways a name fails, because they are acted on differently: a name nobody has is not
        // fixed by checking the cable, and a channel that answered out of step is not fixed by either.
        Resolve(gs::Error::NotFound) => ("address", cell(format_args!("no such name - the DNS server answered, with no address for it"))),
        Resolve(gs::Error::Unavailable) => ("address", cell(format_args!("no DNS server answered - is the network up? (`net`)"))),
        Resolve(e) => ("address", cell(format_args!("the name was not resolved: net-stack's answer was {:?} - its log says why", e))),
        Connect => ("connect", cell(format_args!("no TCP connection - net-stack's log says why"))),
        Transport => ("connect", cell(format_args!("the connection broke before the answer was complete"))),
        Timeout => ("connect", cell(format_args!("no answer within timeout-secs"))),
        Closed => ("connect", cell(format_args!("the server closed the connection before the answer was complete"))),
        Http(why) => ("http", cell(format_args!("the answer is not HTTP this client reads: {}", why))),
        Refused => ("http", cell(format_args!("the answer was refused part way (too large, or not what was asked for)"))),
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
            // 40 is handshake_failure: nothing offered was acceptable. This client offers ECDSA signatures only, so a
            // server whose only certificate is RSA - common still - answers exactly this.
            TlsError::AlertReceived(40) => cell(format_args!("the server refused the handshake (alert 40) - most often it has only an RSA certificate, and this client checks ECDSA only")),
            TlsError::AlertReceived(a) => cell(format_args!("the server ended the handshake (alert {})", a)),
            TlsError::HelloRetryRequest => cell(format_args!("the server does not take X25519, the one key exchange offered")),
            TlsError::ProtocolVersion => cell(format_args!("the server does not speak TLS 1.3")),
            other => cell(format_args!("{:?}", other)),
        }),
    }
}
