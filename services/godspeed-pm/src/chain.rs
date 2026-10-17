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
use godspeed_package::trust::{self, Namespaces, Role, Root, Snapshot, Timestamp, TrustError};
use godspeed_package::Sha256Digest;

use crate::config::Registry;
use crate::https::{Fail, Session};

/// The largest of each document read. Measured on the registry, 2026-10-17: root 1 is 1.1 KiB and grows only
/// with keys; a signature file is about 200 bytes a signature; the snapshot holds a line per package (about
/// 100 bytes), so it has the most room, some 150 packages. They live on the stack, which is 256 KiB for every
/// service and already carries a TLS session, so each is sized to what it holds rather than generously: a
/// document past its size is refused by name ([`Why::TooLarge`]), never cut.
pub const ROOT_MAX: usize = 4 * 1024;
pub const SIG_MAX: usize = 2 * 1024;
const SMALL_SIG_MAX: usize = 1024;
pub const TIMESTAMP_MAX: usize = 1024;
pub const SNAPSHOT_MAX: usize = 16 * 1024;
pub const NAMESPACES_MAX: usize = 8 * 1024;

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
pub fn doc(s: &mut Session<'_, '_, '_, '_>, base: &str, rel: &str, buf: &mut [u8], what: &'static str)
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
pub fn required(s: &mut Session<'_, '_, '_, '_>, base: &str, rel: &str, buf: &mut [u8], what: &'static str)
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
pub struct Docs {
    pub root: [u8; ROOT_MAX], pub root_len: usize,
    sig: [u8; SIG_MAX],
    ts_sig: [u8; SMALL_SIG_MAX],
    snap_sig: [u8; SMALL_SIG_MAX],
    pub timestamp: [u8; TIMESTAMP_MAX], pub timestamp_len: usize,
    pub snapshot: [u8; SNAPSHOT_MAX], pub snapshot_len: usize,
    pub namespaces: [u8; NAMESPACES_MAX], pub namespaces_len: usize,
}

impl Docs {
    pub fn new() -> Docs {
        Docs {
            root: [0; ROOT_MAX], root_len: 0, sig: [0; SIG_MAX], ts_sig: [0; SMALL_SIG_MAX], snap_sig: [0; SMALL_SIG_MAX],
            timestamp: [0; TIMESTAMP_MAX], timestamp_len: 0,
            snapshot: [0; SNAPSHOT_MAX], snapshot_len: 0,
            namespaces: [0; NAMESPACES_MAX], namespaces_len: 0,
        }
    }
}

/// What a verified chain says, borrowed from [`Docs`].
pub struct Verified<'d> {
    pub root: Root<'d>,
    pub timestamp: Timestamp<'d>,
    pub snapshot: Snapshot<'d>,
    pub namespaces: Namespaces<'d>,
    pub snapshot_text: &'d str,
    /// True when this run reached the registry for the first time.
    pub first_contact: bool,
}

fn store_dir(reg: &Registry<'_>) -> Path {
    let mut p = Path::new();
    p.push(TRUST_DIR).push(reg.name).push("/");
    p
}

/// The versions this machine has accepted: `timestamp N` and `namespaces N`, one per line.
fn read_seen(text: &str) -> (Option<u64>, Option<u64>) {
    let (mut t, mut n) = (None, None);
    for line in text.lines() {
        let mut w = line.split_whitespace();
        match (w.next(), w.next().and_then(|v| v.parse().ok())) {
            (Some("timestamp"), Some(v)) => t = Some(v),
            (Some("namespaces"), Some(v)) => n = Some(v),
            _ => {}
        }
    }
    (t, n)
}

/// Verify the chain through namespaces for the registry in use, at `now` (`YYYY-MM-DDTHH:MM:SSZ`).
#[inline(never)]
pub fn verify<'d>(ctx: &ServiceContext, s: &mut Session<'_, '_, '_, '_>, base: &str, reg: &Registry<'_>, now: &str,
                  d: &'d mut Docs) -> Result<Verified<'d>, Why> {
    let mut fs = gs::fs::Fs::new(ctx);
    let dir = store_dir(reg);
    let at = |name: &str| { let mut p = dir; p.push(name); p };

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
    let mut seen_buf = [0u8; 128];
    let (seen_ts, seen_ns) = match fs.read_into(at("seen").as_str(), &mut seen_buf) {
        Ok(n) => read_seen(core::str::from_utf8(&seen_buf[..n]).unwrap_or("")),
        Err(gs::Error::NotFound) => (None, None),
        Err(e) => return Err(Why::Store(e)),
    };

    // 2. TIMESTAMP -> SNAPSHOT -> NAMESPACES.
    d.timestamp_len = required(s, base, "trust/timestamp.toml", &mut d.timestamp, "the timestamp")?;
    let ts_sig = required(s, base, "trust/timestamp.sig", &mut d.ts_sig, "the timestamp's signatures")?;
    d.snapshot_len = required(s, base, "trust/snapshot.toml", &mut d.snapshot, "the snapshot")?;
    let snap_sig = required(s, base, "trust/snapshot.sig", &mut d.snap_sig, "the snapshot's signatures")?;
    d.namespaces_len = required(s, base, "trust/namespaces.toml", &mut d.namespaces, "the namespaces")?;
    let ns_sig = required(s, base, "trust/namespaces.sig", &mut d.sig, "the namespaces' signatures")?;

    let d: &'d Docs = d;
    let root = Root::parse(text(&d.root[..d.root_len], "the root")?).map_err(|err| Why::Trust { what: "the root", err })?;
    root.header.check_fresh(now).map_err(|err| Why::Trust { what: "the root", err })?;
    let timestamp = trust::verify_timestamp(&root, text(&d.timestamp[..d.timestamp_len], "the timestamp")?,
                                            text(&d.ts_sig[..ts_sig], "the timestamp's signatures")?, now, seen_ts)
        .map_err(|err| Why::Trust { what: "the timestamp", err })?;
    let snapshot_text = text(&d.snapshot[..d.snapshot_len], "the snapshot")?;
    let snapshot = trust::verify_snapshot(&root, &timestamp, snapshot_text, text(&d.snap_sig[..snap_sig], "the snapshot's signatures")?, now)
        .map_err(|err| Why::Trust { what: "the snapshot", err })?;
    let namespaces = trust::verify_namespaces(&root, &snapshot, text(&d.namespaces[..d.namespaces_len], "the namespaces")?,
                                              text(&d.sig[..ns_sig], "the namespaces' signatures")?, now, seen_ns)
        .map_err(|err| Why::Trust { what: "the namespaces", err })?;

    // The metadata held together: remember how far this machine has seen, so it never goes back.
    fs.create_dir_all(dir.as_str()).map_err(Why::Store)?;
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
    seen.push("timestamp ").num(timestamp.header.version).push("\nnamespaces ").num(namespaces.header.version).push("\n");
    fs.write(at("seen").as_str(), &seen.b[..seen.n]).map_err(Why::Store)?;

    Ok(Verified { root, timestamp, snapshot, namespaces, snapshot_text, first_contact })
}

/// `godspeed trust`: what this machine has accepted from each registry the configuration knows - the root, the
/// pin it was reached under, the newest timestamp and namespaces versions, and the publishers' keys - read from
/// its own record, with no network.
#[inline(never)]
pub fn show(ctx: &ServiceContext, r: &mut crate::Rows, regs: &[Registry<'_>], using: &str) {
    let mut fs = gs::fs::Fs::new(ctx);
    for reg in regs {
        let dir = store_dir(reg);
        let at = |name: &str| { let mut p = dir; p.push(name); p };
        let in_use = if reg.name == using { "in use; " } else { "" };
        let mut pin_buf = [0u8; 128];
        let pin = match fs.read_into(at("pin").as_str(), &mut pin_buf) {
            Ok(n) => core::str::from_utf8(&pin_buf[..n]).unwrap_or("").trim(),
            Err(_) => {
                r.row(reg.name, "never reached", crate::cell(format_args!("{}its root is met at the next command that reaches it", in_use)).as_str());
                continue;
            }
        };
        let mut root_buf = [0u8; ROOT_MAX];
        let root_len = fs.read_into(at("root.toml").as_str(), &mut root_buf).unwrap_or(0);
        let mut seen_buf = [0u8; 128];
        let seen_len = fs.read_into(at("seen").as_str(), &mut seen_buf).unwrap_or(0);
        let (ts, ns) = read_seen(core::str::from_utf8(&seen_buf[..seen_len]).unwrap_or(""));
        let root = core::str::from_utf8(&root_buf[..root_len]).ok().and_then(|t| Root::parse(t).ok());
        let pin_note = if pin == reg.pin { "the pin in the configuration" } else { "NOT the pin now in the configuration - commands refuse it until `godspeed trust reset`" };
        match root {
            Some(root) => r.row(reg.name, crate::cell(format_args!("root {}, timestamp {}, namespaces {}", root.header.version,
                ts.unwrap_or(0), ns.unwrap_or(0))).as_str(),
                crate::cell(format_args!("{}root expires {}; reached under {}", in_use, root.header.expires, pin_note)).as_str()),
            None => r.fail(reg.name, "unreadable", "this machine's record of it does not read: `godspeed trust reset` starts again"),
        }
        let mut ns_buf = [0u8; NAMESPACES_MAX];
        let ns_len = fs.read_into(at("namespaces.toml").as_str(), &mut ns_buf).unwrap_or(0);
        if let Some(n) = core::str::from_utf8(&ns_buf[..ns_len]).ok().and_then(|t| Namespaces::parse(t).ok()) {
            for (publisher, status, key) in n.entries() {
                let st = match status {
                    godspeed_package::trust::KeyStatus::Active => crate::cell(format_args!("active")),
                    godspeed_package::trust::KeyStatus::Retired(at) => crate::cell(format_args!("retired {}", at)),
                    godspeed_package::trust::KeyStatus::Revoked(at) => crate::cell(format_args!("REVOKED {}", at)),
                };
                r.row(crate::cell(format_args!("{} {}/", reg.name, publisher)).as_str(),
                      crate::cell(format_args!("{}", key)).as_str(), st.as_str());
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

/// Every package the verified snapshot lists, by name, each confirmed against the snapshot's own lookup.
pub fn packages<'v, 'd: 'v>(v: &'v Verified<'d>) -> impl Iterator<Item = &'d str> + 'v {
    v.snapshot_text.lines().filter_map(move |l| {
        let inner = l.strip_prefix("index = \"")?.strip_suffix('"')?;
        let name = inner.split_whitespace().next()?;
        v.snapshot.index(name).map(|_| name)
    })
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
