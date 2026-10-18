// SPDX-License-Identifier: GPL-2.0-only
//! The registry's trust chain, verified on this machine: root, timestamp, snapshot, namespaces, and then a
//! package's index (`docs/developer-services-design.md` 10). Every check is the shared crate's own
//! (`godspeed_package::trust`), so this machine and the developer CLI that published the package cannot
//! disagree about what verifies; the order is the CLI's `godspeed fetch`.
//!
//! **First contact** fetches root 1 and requires its SHA-256 to be the pin on the registry's line in the
//! configuration (10.8). Every root since must be signed by the one before it and by itself. **What this
//! machine has seen is kept**, per registry, in `/system/services/godspeed/trust/<registry>/`: the newest
//! root, the pin it was reached under, and the newest timestamp and namespaces versions, so a registry can
//! never show this machine an older view than one it has already accepted (a rollback).
//!
//! **A pin that changed** since the root was first reached is refused until the operator removes that
//! folder: changing the pin is trusting another registry, which 10.8 says must never happen quietly.

use godspeed::{self as gs, ServiceContext};
use godspeed_package::trust::{self, IndexLines, Namespaces, Role, Root, SnapshotHead, Timestamp, TrustError};
use godspeed_package::{Sha256Digest, Sha256Stream};

use crate::config::Registry;
use crate::https::Fail;
use crate::source::Source;

/// The largest of each document HELD. Measured on the registry, 2026-10-17: root 1 is 1.1 KiB and grows only
/// with keys; a signature file is about 200 bytes a signature. They live on the stack, which is 256 KiB for every
/// service and already carries a TLS session, so each is sized to what it holds rather than generously: a
/// document past its size is refused by name ([`Why::TooLarge`]), never cut.
///
/// **The snapshot is not held.** It is a line per package (about 100 bytes), so it grows with the registry; it is
/// streamed to `snapshot.toml` in this registry's trust folder as it arrives, hashed on the way, verified from its
/// digest (its signatures cover the digest, not the bytes), and read back a line at a time. What this machine
/// reads of it is bounded by [`SNAPSHOT_FILE_MAX`] on disk and [`SNAP_HEAD_MAX`] and [`SNAP_LINE_MAX`] in memory, so the
/// registry's size no longer decides whether a command runs (the CLI repository's backlog, item 2, its first half).
pub const ROOT_MAX: usize = 4 * 1024;
pub const SIG_MAX: usize = 2 * 1024;
const SMALL_SIG_MAX: usize = 1024;
pub const TIMESTAMP_MAX: usize = 1024;
pub const NAMESPACES_MAX: usize = 8 * 1024;
/// The largest snapshot kept: about two and a half million packages at 100 bytes a line. Past it, refused by name.
pub const SNAPSHOT_FILE_MAX: u64 = 256 * 1024 * 1024;
/// The most of the snapshot's start read to find its head - everything before its `index` lines.
const SNAP_HEAD_MAX: usize = 1024;
/// The longest `index` line read back: a 128-byte name, its digest and the line's own words fit with room.
const SNAP_LINE_MAX: usize = 320;

const TRUST_DIR: &str = "/system/services/godspeed/trust/";

/// Why the chain stopped.
#[derive(Clone, Copy, Debug)]
pub enum Why {
    /// The connection, the TLS handshake or the HTTP answer.
    Net(Fail),
    /// The registry answered a required document with a status other than 200.
    Missing { path: Path, status: u16 },
    /// A document larger than this machine reads.
    TooLarge(&'static str),
    /// A document is not text.
    NotText(&'static str),
    /// A trust rule failed. `what` names the document.
    Trust { what: &'static str, err: TrustError },
    /// Root 1 is not the root the configuration pins.
    PinMismatch,
    /// The pin changed since this machine first reached the registry.
    PinChanged,
    /// This machine's record of the registry could not be read or written.
    Store(gs::Error),
}

/// A short path, bounded.
#[derive(Clone, Copy, Debug)]
pub struct Path { pub b: [u8; 200], pub n: usize }

impl Path {
    pub fn new() -> Path { Path { b: [0; 200], n: 0 } }
    pub fn push(&mut self, s: &str) -> &mut Path {
        let take = s.len().min(self.b.len() - self.n);
        self.b[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        self
    }
    pub fn num(&mut self, v: u64) -> &mut Path {
        let mut d = [0u8; 20];
        let mut i = d.len();
        let mut x = v;
        loop {
            i -= 1;
            d[i] = b'0' + (x % 10) as u8;
            x /= 10;
            if x == 0 { break; }
        }
        self.push(core::str::from_utf8(&d[i..]).unwrap_or("0"))
    }
    pub fn as_str(&self) -> &str { core::str::from_utf8(&self.b[..self.n]).unwrap_or("") }
}

/// GET `base + rel` into `buf`. `Ok(None)` for a 404, `Ok(Some(len))` for a 200; anything else, or a body
/// larger than `buf`, is a [`Why`].
pub fn doc(s: &mut dyn Source, base: &str, rel: &str, buf: &mut [u8], what: &'static str)
    -> Result<Option<usize>, Why> {
    let mut path = Path::new();
    path.push(base).push(rel);
    let mut len = 0usize;
    let mut over = false;
    let cap = buf.len();
    let head = s.get(path.as_str(), &mut |b: &[u8]| {
        if len + b.len() > cap {
            over = true;
            return false;
        }
        buf[len..len + b.len()].copy_from_slice(b);
        len += b.len();
        true
    });
    if over {
        return Err(Why::TooLarge(what));
    }
    let head = head.map_err(Why::Net)?;
    match head.status {
        200 => Ok(Some(len)),
        404 => Ok(None),
        status => Err(Why::Missing { path, status }),
    }
}

/// [`doc`], where a 404 is as much a failure as any other status.
pub fn required(s: &mut dyn Source, base: &str, rel: &str, buf: &mut [u8], what: &'static str)
    -> Result<usize, Why> {
    let mut path = Path::new();
    path.push(base).push(rel);
    match doc(s, base, rel, buf, what)? {
        Some(n) => Ok(n),
        None => Err(Why::Missing { path, status: 404 }),
    }
}

pub fn text<'a>(buf: &'a [u8], what: &'static str) -> Result<&'a str, Why> {
    core::str::from_utf8(buf).map_err(|_| Why::NotText(what))
}

/// The buffers the chain's documents live in, so the verified views can borrow them. A rotated root is read
/// into `namespaces` first, which is not in use until the rotations are done.
pub struct Docs<'r> {
    pub root: [u8; ROOT_MAX], pub root_len: usize,
    sig: [u8; SIG_MAX],
    ts_sig: [u8; SMALL_SIG_MAX],
    snap_sig: [u8; SMALL_SIG_MAX],
    pub timestamp: [u8; TIMESTAMP_MAX], pub timestamp_len: usize,
    pub namespaces: [u8; NAMESPACES_MAX], pub namespaces_len: usize,
    /// Offline, the publishers' keys as this machine last accepted them online, when that record is newer than
    /// the file's. Lent only by a check from a file: online, the TLS session is on the stack, and this has no use.
    record: Option<&'r mut [u8; NAMESPACES_MAX]>, record_len: usize,
}

impl<'r> Docs<'r> {
    /// For an offline check: `record` holds this machine's own record of the keys while it is compared.
    pub fn with_record(record: &'r mut [u8; NAMESPACES_MAX]) -> Docs<'r> {
        let mut d = Docs::new();
        d.record = Some(record);
        d
    }

    pub fn new() -> Docs<'r> {
        Docs {
            root: [0; ROOT_MAX], root_len: 0, sig: [0; SIG_MAX], ts_sig: [0; SMALL_SIG_MAX], snap_sig: [0; SMALL_SIG_MAX],
            timestamp: [0; TIMESTAMP_MAX], timestamp_len: 0,
            namespaces: [0; NAMESPACES_MAX], namespaces_len: 0,
            record: None, record_len: 0,
        }
    }
}

/// The verified snapshot, kept on disk: where, how long, which version, and its `index` lines' offset.
#[derive(Clone, Copy)]
pub struct SnapshotFile {
    pub version: u64,
    pub path: Path,
    pub len: u64,
    /// Where the `index` lines start.
    pub body: u64,
    /// True when this command downloaded it; false when the one already here is the one the timestamp names.
    pub fetched: bool,
}

/// What a verified chain says, borrowed from [`Docs`]; the snapshot is on disk.
pub struct Verified<'d> {
    pub root: Root<'d>,
    pub timestamp: Timestamp<'d>,
    pub snapshot: SnapshotFile,
    pub namespaces: Namespaces<'d>,
    /// True when this run reached the registry for the first time.
    pub first_contact: bool,
    /// Checked from a file, not the registry: the chain was valid until its timestamp's `expires`, and was checked
    /// as of just before then. Nothing of this machine's record of the registry was changed.
    pub offline: bool,
    /// Offline, this machine's own newer record of the publishers' keys, which a release must ALSO satisfy: a
    /// key revoked since the file was made is refused (`docs/developer-services-design.md` 15.3, rule 1).
    pub record: Option<Namespaces<'d>>,
}

impl Verified<'_> {
    /// Does the registry vouch for `key` signing a release of `publisher`'s published at `published`? Online, the
    /// namespaces just verified decide. Offline, the file's must, AND this machine's newer record when it has one.
    pub fn trusts_release(&self, publisher: &str, key: &godspeed_package::PublicKey, published: &str) -> Result<(), TrustError> {
        self.namespaces.trusts_release(publisher, key, published)?;
        match &self.record {
            Some(r) => r.trusts_release(publisher, key, published),
            None => Ok(()),
        }
    }
}

/// An instant just before `expires` (`YYYY-MM-DDTHH:MM:SSZ`): the same digits without the `Z`, which sorts below
/// it and above every earlier time. A file's chain is checked as of this, the last moment it was valid.
fn just_before(expires: &str) -> &str { expires.strip_suffix('Z').unwrap_or(expires) }

fn store_dir(reg: &Registry<'_>) -> Path {
    let mut p = Path::new();
    p.push(TRUST_DIR).push(reg.name).push("/");
    p
}

/// The versions this machine has accepted: `timestamp N` and `namespaces N`, one per line - and `snapshot N
/// <digest> <bytes>`, the snapshot kept beside them, so a command whose timestamp names it does not download it
/// again.
fn read_seen(text: &str) -> (Option<u64>, Option<u64>, Option<(u64, Sha256Digest, u64)>) {
    let (mut t, mut n, mut s) = (None, None, None);
    for line in text.lines() {
        let mut w = line.split_whitespace();
        match (w.next(), w.next().and_then(|v| v.parse().ok())) {
            (Some("timestamp"), Some(v)) => t = Some(v),
            (Some("namespaces"), Some(v)) => n = Some(v),
            (Some("snapshot"), Some(v)) => {
                let d = w.next().and_then(|d| d.parse::<Sha256Digest>().ok());
                let len = w.next().and_then(|l| l.parse::<u64>().ok());
                if let (Some(d), Some(len)) = (d, len) { s = Some((v, d, len)); }
            }
            _ => {}
        }
    }
    (t, n, s)
}

/// GET the snapshot into `partial`, sized by its `Content-Length` before a byte of it arrives, hashed as it is
/// written. Its digest and length. A snapshot that does not say its size, or says more than
/// [`SNAPSHOT_FILE_MAX`], is refused by name; nothing is guessed at.
fn fetch_snapshot(ctx: &ServiceContext, s: &mut dyn Source, base: &str, fs: &mut gs::fs::Fs<'_>,
                  partial: &str) -> Result<(Sha256Digest, u64), Why> {
    let mut path = Path::new();
    path.push(base).push("trust/snapshot.toml");
    // Both closures reach the file and the size, one before the body and one during it.
    let fs = core::cell::RefCell::new(fs);
    let size: core::cell::Cell<Option<u64>> = core::cell::Cell::new(None);
    let store: core::cell::Cell<Option<gs::Error>> = core::cell::Cell::new(None);
    let too_large = core::cell::Cell::new(false);
    let mut hash = Sha256Stream::new();
    let mut chunk = [0u8; gs::fs::IO_CHUNK];
    let (mut held, mut written) = (0usize, 0u64);
    let mut shown = u64::MAX;
    let t0 = gs::task::uptime_secs(ctx);
    let head = s.get_sized(path.as_str(), &mut |h| {
        match h.length {
            Some(n) if n <= SNAPSHOT_FILE_MAX && n > 0 => {
                if let Err(e) = fs.borrow_mut().create_sized(partial, n) { store.set(Some(e)); return false; }
                size.set(Some(n));
                true
            }
            _ => { too_large.set(true); false }
        }
    }, &mut |bytes: &[u8]| {
        hash.update(bytes);
        let total = size.get().unwrap_or(1).max(1);
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = rest.len().min(chunk.len() - held);
            chunk[held..held + take].copy_from_slice(&rest[..take]);
            held += take;
            rest = &rest[take..];
            if held == chunk.len() {
                if let Err(e) = fs.borrow_mut().write_at(partial, written, &chunk) { store.set(Some(e)); return false; }
                written += held as u64;
                held = 0;
            }
        }
        // A large snapshot takes time: say how far it has got, by twentieths.
        let step = (written + held as u64) * 20 / total;
        if total > 64 * 1024 && step != shown {
            shown = step;
            let done = written + held as u64;
            crate::progress(ctx, format_args!("  downloading the snapshot  {}%  {} / {} KiB  {}   [q] quit",
                done * 100 / total, done / 1024, total / 1024,
                crate::packages::time_text(gs::task::uptime_secs(ctx) - t0, done, total).as_str()));
        }
        true
    });
    if too_large.get() { return Err(Why::TooLarge("the snapshot")); }
    if let Some(e) = store.get() { return Err(Why::Store(e)); }
    let head = head.map_err(Why::Net)?;
    if head.status != 200 { return Err(Why::Missing { path, status: head.status }); }
    if held > 0 { fs.borrow_mut().write_at(partial, written, &chunk[..held]).map_err(Why::Store)?; written += held as u64; }
    if Some(written) != size.get() { return Err(Why::Net(Fail::Closed)); }
    Ok((hash.finish(), written))
}

/// The snapshot's head, read from the start of the file at `path`.
fn read_head<'b>(fs: &mut gs::fs::Fs<'_>, path: &str, buf: &'b mut [u8; SNAP_HEAD_MAX]) -> Result<SnapshotHead<'b>, Why> {
    let mut n = 0usize;
    while n < buf.len() {
        let got = fs.read_at(path, n as u64, &mut buf[n..]).map_err(Why::Store)?;
        if got == 0 { break; }
        n += got;
    }
    // Whole lines only: the head is parsed up to its last line, and must end inside what was read.
    let end = buf[..n].iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let t = text(&buf[..end], "the snapshot")?;
    SnapshotHead::parse(t).map_err(|err| Why::Trust { what: "the snapshot", err })
}

/// Every `index` line of the kept snapshot, in order, to `f`: its package name and index digest, each checked as
/// the whole snapshot would be (`IndexLines`). `f` returning false stops the walk. Read from the disk a chunk at a
/// time, a line never longer than [`SNAP_LINE_MAX`].
pub fn each_index(ctx: &ServiceContext, snap: &SnapshotFile, f: &mut dyn FnMut(&str, &Sha256Digest) -> bool) -> Result<(), Why> {
    let mut fs = gs::fs::Fs::new(ctx);
    let mut lines = IndexLines::new();
    let mut chunk = [0u8; gs::fs::IO_CHUNK];
    let mut line = [0u8; SNAP_LINE_MAX];
    let mut ll = 0usize;
    // `read_at` takes offsets in whole 508-byte blocks, so the walk starts at the block holding the body and skips
    // into it.
    let block = 508u64;
    let mut off = snap.body / block * block;
    let mut skip = (snap.body - off) as usize;
    while off < snap.len {
        let got = fs.read_at(snap.path.as_str(), off, &mut chunk).map_err(Why::Store)?;
        if got == 0 { break; }
        for &b in &chunk[skip..got] {
            if b == b'\n' {
                let t = text(&line[..ll], "the snapshot")?;
                let (name, d) = lines.next(t).map_err(|err| Why::Trust { what: "the snapshot", err })?;
                if !f(name, &d) { return Ok(()); }
                ll = 0;
            } else {
                if ll == line.len() { return Err(Why::TooLarge("a snapshot line")); }
                line[ll] = b;
                ll += 1;
            }
        }
        skip = 0;
        off += got as u64;
    }
    if ll != 0 { return Err(Why::Trust { what: "the snapshot", err: TrustError::Malformed("a line is missing, or the final newline") }); }
    Ok(())
}

/// The digest of `name`'s index, if the kept snapshot lists it. Its lines are in order, so the walk stops at the
/// first name past it.
pub fn index_of(ctx: &ServiceContext, v: &Verified<'_>, name: &str) -> Result<Option<Sha256Digest>, Why> {
    let mut found = None;
    each_index(ctx, &v.snapshot, &mut |n, d| {
        if n == name { found = Some(*d); return false; }
        n < name
    })?;
    Ok(found)
}

/// Verify the chain through namespaces for the registry in use, at `now` (`YYYY-MM-DDTHH:MM:SSZ`).
#[inline(never)]
pub fn verify<'d>(ctx: &ServiceContext, s: &mut dyn Source, base: &str, reg: &Registry<'_>, now: &str,
                  d: &'d mut Docs<'_>) -> Result<Verified<'d>, Why> {
    let mut fs = gs::fs::Fs::new(ctx);
    let dir = store_dir(reg);
    let at = |name: &str| { let mut p = dir; p.push(name); p };
    // A FILE is as of the day it was made: checked as of then, against nothing this machine has seen, and
    // recording nothing - what a file says never moves this machine's view of the registry (15.3).
    let offline = s.offline();

    // 1. THE ROOT: the one this machine already trusts, or root 1 checked against the pin.
    let mut pin_buf = [0u8; 128];
    let stored_pin = match fs.read_into(at("pin").as_str(), &mut pin_buf) {
        Ok(n) => Some(n),
        Err(gs::Error::NotFound) => None,
        Err(e) => return Err(Why::Store(e)),
    };
    let mut first_contact = false;
    let mut root_changed = false;
    let stored_root = match stored_pin {
        None => None,
        Some(n) => {
            if core::str::from_utf8(&pin_buf[..n]).unwrap_or("").trim() != reg.pin {
                return Err(Why::PinChanged);
            }
            match fs.read_into(at("root.toml").as_str(), &mut d.root) {
                Ok(len) => Some(len),
                Err(gs::Error::NotFound) => None,
                Err(e) => return Err(Why::Store(e)),
            }
        }
    };
    d.root_len = match stored_root {
        Some(len) => len,
        None => {
            let len = required(s, base, "trust/root/1.toml", &mut d.root, "root 1")?;
            let sig_len = required(s, base, "trust/root/1.sig", &mut d.sig, "root 1's signatures")?;
            let mut want = Path::new();
            let got = Sha256Digest::of(&d.root[..len]);
            let _ = core::fmt::write(&mut PathWriter(&mut want), format_args!("{}", got));
            if want.as_str() != reg.pin {
                return Err(Why::PinMismatch);
            }
            let root_text = text(&d.root[..len], "root 1")?;
            let root = Root::parse(root_text).map_err(|err| Why::Trust { what: "root 1", err })?;
            trust::verify_by(&root, Role::Root, root_text.as_bytes(), text(&d.sig[..sig_len], "root 1's signatures")?)
                .map_err(|err| Why::Trust { what: "root 1", err })?;
            first_contact = true;
            root_changed = true;
            len
        }
    };
    // Every rotation since: root N+1 must be signed by root N's keys and by its own.
    loop {
        let version = {
            let t = text(&d.root[..d.root_len], "the root")?;
            Root::parse(t).map_err(|err| Why::Trust { what: "the root", err })?.header.version
        };
        let mut rel = Path::new();
        rel.push("trust/root/").num(version + 1).push(".toml");
        let Some(len) = doc(s, base, rel.as_str(), &mut d.namespaces[..ROOT_MAX], "the next root")? else { break };
        let mut sig_rel = Path::new();
        sig_rel.push("trust/root/").num(version + 1).push(".sig");
        let sig_len = required(s, base, sig_rel.as_str(), &mut d.sig, "the next root's signatures")?;
        {
            let current = Root::parse(text(&d.root[..d.root_len], "the root")?).map_err(|err| Why::Trust { what: "the root", err })?;
            trust::verify_next_root(&current, text(&d.namespaces[..len], "the next root")?, text(&d.sig[..sig_len], "its signatures")?)
                .map_err(|err| Why::Trust { what: "the next root", err })?;
        }
        d.root[..len].copy_from_slice(&d.namespaces[..len]);
        d.root_len = len;
        root_changed = true;
    }

    // What this machine has seen of this registry, so nothing older is accepted.
    let mut seen_buf = [0u8; 256];
    let (seen_ts, seen_ns, kept) = match fs.read_into(at("seen").as_str(), &mut seen_buf) {
        Ok(n) => read_seen(core::str::from_utf8(&seen_buf[..n]).unwrap_or("")),
        Err(gs::Error::NotFound) => (None, None, None),
        Err(e) => return Err(Why::Store(e)),
    };
    // Offline, an older timestamp or namespaces than this machine has seen is not a rollback: the file is simply
    // older than the machine's last look. A newer record of the keys is applied instead, after the chain.
    let (seen_ts, seen_ns_check, kept) = if offline { (None, None, None) } else { (seen_ts, seen_ns, kept) };

    // 2. TIMESTAMP -> SNAPSHOT (on disk) -> NAMESPACES.
    d.timestamp_len = required(s, base, "trust/timestamp.toml", &mut d.timestamp, "the timestamp")?;
    let ts_sig = required(s, base, "trust/timestamp.sig", &mut d.ts_sig, "the timestamp's signatures")?;
    let snap_sig = required(s, base, "trust/snapshot.sig", &mut d.snap_sig, "the snapshot's signatures")?;
    d.namespaces_len = required(s, base, "trust/namespaces.toml", &mut d.namespaces, "the namespaces")?;
    let ns_sig = required(s, base, "trust/namespaces.sig", &mut d.sig, "the namespaces' signatures")?;

    // This machine's record of the publishers' keys, for an offline check to answer to as well.
    if offline {
        d.record_len = match d.record.as_deref_mut() {
            None => 0,
            Some(buf) => match fs.read_into(at("namespaces.toml").as_str(), buf) {
                Ok(n) => n,
                Err(gs::Error::NotFound) => 0,
                Err(e) => return Err(Why::Store(e)),
            },
        };
    }

    let d: &'d Docs<'_> = d;
    // Offline, "now" is the last moment the file's chain was valid.
    let ts_text = text(&d.timestamp[..d.timestamp_len], "the timestamp")?;
    let now = if offline {
        just_before(Timestamp::parse(ts_text).map_err(|err| Why::Trust { what: "the timestamp", err })?.header.expires)
    } else {
        now
    };
    let root = Root::parse(text(&d.root[..d.root_len], "the root")?).map_err(|err| Why::Trust { what: "the root", err })?;
    root.header.check_fresh(now).map_err(|err| Why::Trust { what: "the root", err })?;
    let timestamp = trust::verify_timestamp(&root, ts_text,
                                            text(&d.ts_sig[..ts_sig], "the timestamp's signatures")?, now, seen_ts)
        .map_err(|err| Why::Trust { what: "the timestamp", err })?;

    // The snapshot the timestamp names: the one kept here already, or downloaded to a staging file beside it.
    fs.create_dir_all(dir.as_str()).map_err(Why::Store)?;
    // A file's snapshot is staged apart and never replaces the one kept: it may be older.
    let kept_path = at(if offline { "snapshot.offline" } else { "snapshot.toml" });
    let staged = at(if offline { "snapshot.offline.partial" } else { "snapshot.partial" });
    let reuse = match kept {
        Some((v, dg, len)) if v == timestamp.snapshot_version && dg == timestamp.snapshot_digest =>
            fs.stat(kept_path.as_str()).ok().filter(|st| st.size == len).map(|_| (dg, len)),
        _ => None,
    };
    let (digest, len, read_from, fetched) = match reuse {
        Some((dg, len)) => (dg, len, kept_path, false),
        None => {
            let _ = fs.delete(staged.as_str());
            let (dg, len) = fetch_snapshot(ctx, s, base, &mut fs, staged.as_str())?;
            (dg, len, staged, true)
        }
    };
    let mut head_buf = [0u8; SNAP_HEAD_MAX];
    let head = read_head(&mut fs, read_from.as_str(), &mut head_buf)?;
    trust::verify_snapshot_stream(&root, &timestamp, &digest, &head, text(&d.snap_sig[..snap_sig], "the snapshot's signatures")?, now)
        .map_err(|err| Why::Trust { what: "the snapshot", err })?;
    let namespaces = trust::verify_namespaces_by_head(&root, &head, text(&d.namespaces[..d.namespaces_len], "the namespaces")?,
                                                      text(&d.sig[..ns_sig], "the namespaces' signatures")?, now, seen_ns_check)
        .map_err(|err| Why::Trust { what: "the namespaces", err })?;
    let snapshot = SnapshotFile { version: head.header.version, path: kept_path, len, body: head.len as u64, fetched };
    if fetched {
        // Every `index` line checked once, as it would be in memory, before the file replaces the one kept.
        each_index(ctx, &SnapshotFile { path: staged, ..snapshot }, &mut |_, _| true)?;
        let _ = fs.delete(kept_path.as_str());
        fs.rename(staged.as_str(), if offline { "snapshot.offline" } else { "snapshot.toml" }).map_err(Why::Store)?;
    }
    if offline {
        // THE NEWER RECORD OF THE KEYS WINS (15.3, rule 1). This machine's, when it is newer than the file's, must
        // vouch for the release too. When the file's is newer, or the machine has none (never online), the file's
        // becomes this machine's record: it was verified above, and it is what the supervisor checks an installed
        // package's key against at every start. An older record never replaces a newer one, and nothing else of
        // the registry's view - root, timestamp, the versions seen - is moved by a file.
        let mine = match d.record_len {
            0 => None,
            n => {
                let t = text(d.record.as_deref().map_or(&[][..], |b| &b[..n]), "this machine's record of the keys")?;
                Some(Namespaces::parse(t).map_err(|err| Why::Trust { what: "this machine's record of the keys", err })?)
            }
        };
        let record = match mine {
            Some(m) if m.header.version > namespaces.header.version => Some(m),
            Some(m) if m.header.version == namespaces.header.version => None,
            _ => {
                fs.write(at("namespaces.toml").as_str(), &d.namespaces[..d.namespaces_len]).map_err(Why::Store)?;
                None
            }
        };
        let _ = seen_ns;
        return Ok(Verified { root, timestamp, snapshot, namespaces, first_contact, offline, record });
    }

    // The metadata held together: remember how far this machine has seen, so it never goes back.
    if root_changed {
        fs.write(at("root.toml").as_str(), &d.root[..d.root_len]).map_err(Why::Store)?;
        fs.write(at("pin").as_str(), reg.pin.as_bytes()).map_err(Why::Store)?;
    }
    // The namespaces document too, when it is new to this machine, so `godspeed trust` can show the
    // publishers' keys without the network. Verified above; kept only as a record of what was accepted.
    if root_changed || seen_ns != Some(namespaces.header.version) {
        fs.write(at("namespaces.toml").as_str(), &d.namespaces[..d.namespaces_len]).map_err(Why::Store)?;
    }
    let mut seen = Path::new();
    seen.push("timestamp ").num(timestamp.header.version).push("\nnamespaces ").num(namespaces.header.version)
        .push("\nsnapshot ").num(snapshot.version).push(" ");
    let _ = core::fmt::write(&mut PathWriter(&mut seen), format_args!("{}", digest));
    seen.push(" ").num(len).push("\n");
    fs.write(at("seen").as_str(), &seen.b[..seen.n]).map_err(Why::Store)?;

    Ok(Verified { root, timestamp, snapshot, namespaces, first_contact, offline, record: None })
}

/// `godspeed trust`: what this machine has accepted from each registry the configuration knows - the root, the
/// pin it was reached under, the newest timestamp and namespaces versions, and the publishers' keys - read from
/// its own record, with no network.
#[inline(never)]
pub fn show(ctx: &ServiceContext, r: &mut crate::Rows, regs: &[Registry<'_>], using: &str) {
    let mut fs = gs::fs::Fs::new(ctx);
    // A CARD per registry: `title` starts each, the shell indents the rest under it.
    r.columns(&["key", "value", "note"]);
    for reg in regs {
        let dir = store_dir(reg);
        let at = |name: &str| { let mut p = dir; p.push(name); p };
        let in_use = if reg.name == using { "in use" } else { "" };
        let mut pin_buf = [0u8; 128];
        let pin = match fs.read_into(at("pin").as_str(), &mut pin_buf) {
            Ok(n) => core::str::from_utf8(&pin_buf[..n]).unwrap_or("").trim(),
            Err(_) => {
                r.row("title", reg.name, in_use);
                r.row("state", "never reached", "its root is met at the next command that reaches it");
                continue;
            }
        };
        let mut root_buf = [0u8; ROOT_MAX];
        let root_len = fs.read_into(at("root.toml").as_str(), &mut root_buf).unwrap_or(0);
        let mut seen_buf = [0u8; 128];
        let seen_len = fs.read_into(at("seen").as_str(), &mut seen_buf).unwrap_or(0);
        let (ts, ns, _) = read_seen(core::str::from_utf8(&seen_buf[..seen_len]).unwrap_or(""));
        let root = core::str::from_utf8(&root_buf[..root_len]).ok().and_then(|t| Root::parse(t).ok());
        r.row("title", reg.name, in_use);
        let Some(root) = root else {
            r.fail(reg.name, "unreadable", "this machine's record of it does not read: `godspeed trust reset` starts again");
            continue;
        };
        r.row("root", crate::cell(format_args!("{}", root.header.version)).as_str(),
              crate::cell(format_args!("expires {}", root.header.expires)).as_str());
        r.row("timestamp", crate::cell(format_args!("{}", ts.unwrap_or(0))).as_str(), "the newest accepted; an older one is refused");
        r.row("namespaces", crate::cell(format_args!("{}", ns.unwrap_or(0))).as_str(), "the newest accepted; an older one is refused");
        r.row("pin", pin, if pin == reg.pin { "the configuration's" }
              else { "NOT the pin now in the configuration - commands refuse it until `godspeed trust reset`" });
        let mut ns_buf = [0u8; NAMESPACES_MAX];
        let ns_len = fs.read_into(at("namespaces.toml").as_str(), &mut ns_buf).unwrap_or(0);
        if let Some(n) = core::str::from_utf8(&ns_buf[..ns_len]).ok().and_then(|t| Namespaces::parse(t).ok()) {
            for (publisher, status, key) in n.entries() {
                let st = match status {
                    godspeed_package::trust::KeyStatus::Active => crate::cell(format_args!("active")),
                    godspeed_package::trust::KeyStatus::Retired(at) => crate::cell(format_args!("retired {}", at)),
                    godspeed_package::trust::KeyStatus::Revoked(at) => crate::cell(format_args!("REVOKED {}", at)),
                };
                r.row(crate::cell(format_args!("key {}/", publisher)).as_str(), crate::cell(format_args!("{}", key)).as_str(), st.as_str());
            }
        }
    }
}

/// `godspeed trust reset <registry>`: forget what this machine accepted from it, so the next command meets it
/// again from root 1, checked against the pin in the configuration. The shell has asked first.
pub fn reset(ctx: &ServiceContext, r: &mut crate::Rows, reg: &Registry<'_>) {
    let dir = store_dir(reg);
    let mut fs = gs::fs::Fs::new(ctx);
    match fs.exists(dir.as_str()) {
        Ok(false) => return r.row(reg.name, "nothing to forget", "this machine has not reached it yet"),
        Ok(true) => {}
        Err(e) => return r.fail(reg.name, dir.as_str(), crate::cell(format_args!("could not be checked: {:?}", e)).as_str()),
    }
    match fs.delete_all(dir.as_str()) {
        Ok(()) => r.row(reg.name, "forgotten", "the next command meets it again from root 1, checked against its pin"),
        Err(e) => r.fail(reg.name, dir.as_str(), crate::cell(format_args!("could not be removed: {:?} - {}", e, fs.reason())).as_str()),
    }
}

struct PathWriter<'p>(&'p mut Path);
impl core::fmt::Write for PathWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.push(s);
        Ok(())
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` for epoch seconds, the form trust metadata is dated in.
pub fn iso(now: u64, out: &mut [u8; 20]) -> &str {
    let (y, mo, d, h, mi) = crate::config::civil(now);
    let sec = now % 60;
    let mut i = 0usize;
    let mut put = |v: u64, w: usize, sep: u8| {
        let mut x = v;
        for k in (0..w).rev() {
            out[i + k] = b'0' + (x % 10) as u8;
            x /= 10;
        }
        i += w;
        if sep != 0 { out[i] = sep; i += 1; }
    };
    put(y, 4, b'-'); put(mo, 2, b'-'); put(d, 2, b'T'); put(h, 2, b':'); put(mi, 2, b':'); put(sec, 2, b'Z');
    core::str::from_utf8(&out[..]).unwrap_or("")
}
