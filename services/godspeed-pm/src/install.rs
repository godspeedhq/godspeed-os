// SPDX-License-Identifier: GPL-2.0-only
//! `godspeed install`, `uninstall` and `list installed`: a fetched package made to run, and stopped again.
//!
//! **Installing is three steps, and this program does the first two.** It checks the fetched copy from the disk
//! (`check.rs`, as `godspeed verify` does) and answers what its contract requests; the shell asks the operator about
//! each send peer, [y/N], and runs it again with the answer (`grant <peer>,...` or `grant none`). Then this program
//! records the answer beside the package (`grant`), adds the package to the installed list, and asks the supervisor to
//! start it. The supervisor checks the copy AGAIN, with the same code, narrows the grant to what the contract
//! requests, and starts the bytes it checked (`services/supervisor/src/installed.rs`): only the supervisor holds the
//! authority to start an image, and it is not delegatable (CLAUDE.md 14.1).
//!
//! **No privilege is ever granted** - not asked, not grantable. A package gets send peers, and only the ones it requests
//! and the operator approved; `supervisor` is never one of them.

use godspeed::{self as gs, ipc::Message, ServiceContext};
use godspeed_sdk::service_context::supcmd;

use crate::check::{self, MAX_PEERS};
use crate::packages::{self, Fetched, MAX_FETCHED};
use crate::{cell, Cell, Rows, C};

/// The installed list the supervisor reads at boot (`installed.rs`'s `LIST`): `publisher/name version`, one per line.
pub const LIST: &str = "/system/packages/installed";
/// How long the supervisor is given to check and start a package. Reading and hashing a large image from a slow disk
/// is most of it.
const START_SECS: i64 = 60;

/// The newest fetched version `which` names (`publisher/name` or `publisher/name@version`).
#[inline(never)]
fn pick(ctx: &ServiceContext, which: &str) -> Result<Fetched, Cell> {
    let mut all = [Fetched::EMPTY_PUB; MAX_FETCHED];
    let mut more = false;
    let n = packages::fetched(ctx, &mut all, &mut more).map_err(|e| cell(format_args!("the fetched packages could not be listed: {:?}", e)))?;
    let (pkg, version) = match which.split_once('@') { Some((p, v)) => (p, Some(v)), None => (which, None) };
    let mut best: Option<Fetched> = None;
    for f in all.iter().take(n) {
        if cell(format_args!("{}/{}", f.publisher(), f.name())).as_str() != pkg { continue; }
        if version.is_some_and(|v| v != f.version()) { continue; }
        let newer = best.as_ref().is_none_or(|b| {
            f.version().parse::<godspeed_package::Version>().ok() > b.version().parse::<godspeed_package::Version>().ok()
        });
        if newer { best = Some(*f); }
    }
    best.ok_or_else(|| cell(format_args!("{} is not fetched here: `godspeed fetch {}` first", which, which)))
}

/// What is installed: `(package, version)` per line of [`LIST`], into `buf`.
fn read_list<'b>(ctx: &ServiceContext, buf: &'b mut [u8; 1024]) -> Result<&'b str, Cell> {
    match gs::fs::Fs::new(ctx).read_into(LIST, buf) {
        Ok(n) => Ok(core::str::from_utf8(&buf[..n]).unwrap_or("")),
        Err(gs::Error::NotFound) => Ok(""),
        Err(e) => Err(cell(format_args!("{} could not be read: {:?}", LIST, e))),
    }
}

/// [`LIST`] with `pkg`'s line replaced by `line`, or removed when `line` is empty.
#[inline(never)]
fn write_list(ctx: &ServiceContext, pkg: &str, line: &str) -> Result<(), Cell> {
    let mut buf = [0u8; 1024];
    let old = read_list(ctx, &mut buf)?;
    let mut out = [0u8; 1024];
    let mut n = 0usize;
    let mut put = |s: &str| -> bool {
        if n + s.len() + 1 > out.len() { return false; }
        out[n..n + s.len()].copy_from_slice(s.as_bytes());
        out[n + s.len()] = b'\n';
        n += s.len() + 1;
        true
    };
    let mut fits = true;
    for l in old.lines().filter(|l| !l.trim().is_empty()) {
        if l.split_whitespace().next() == Some(pkg) { continue; }
        fits &= put(l.trim());
    }
    if !line.is_empty() { fits &= put(line); }
    if !fits { return Err(cell(format_args!("{} is full", LIST))); }
    gs::fs::Fs::new(ctx).write(LIST, &out[..n]).map_err(|e| cell(format_args!("{} could not be written: {:?}", LIST, e)))
}

/// Is `pkg` in [`LIST`]? Its version, if so.
#[inline(never)]
pub fn installed_version(ctx: &ServiceContext, pkg: &str, out: &mut [u8; 24]) -> Option<usize> {
    let mut buf = [0u8; 1024];
    let list = read_list(ctx, &mut buf).ok()?;
    let v = list.lines().find_map(|l| { let mut it = l.split_whitespace(); (it.next() == Some(pkg)).then(|| it.next()).flatten() })?;
    let n = v.len().min(out.len());
    out[..n].copy_from_slice(&v.as_bytes()[..n]);
    Some(n)
}

/// `godspeed install <pkg>[@ver]` with no answer yet: what the contract requests, for the shell to ask about. Columns
/// `peer gives note`, one row per send peer, and the release in the status note; none is no rows. Three columns, so
/// the status row has a note to carry the release in.
#[inline(never)]
pub fn requests(ctx: &ServiceContext, r: &mut Rows, which: &str) {
    let f = match pick(ctx, which) { Ok(f) => f, Err(why) => return r.fail("fetched", which, why.as_str()) };
    let checked = match check::package(ctx, f.publisher(), f.name(), f.version(), packages::this_arch().map(|a| a.as_str()), &mut |_, _| true) {
        Ok(c) => c,
        Err(why) => return r.fail("check", cell(format_args!("{}/{} {}", f.publisher(), f.name(), f.version())).as_str(), why.as_str()),
    };
    r.columns(&["peer", "gives", "note"]);
    for p in checked.contract.peers() {
        r.entry(&[C::S(p), C::S("a send capability to it"), C::S(if p == "supervisor" { "never granted to a package" } else { "" })]);
    }
    r.note(format_args!("{}/{} {} requests these; the operator answers each", f.publisher(), f.name(), f.version()));
}

/// `godspeed install <pkg>[@ver] grant <peer>,...|none`: record the answer, list it, and have the supervisor start it.
#[inline(never)]
pub fn install(ctx: &ServiceContext, r: &mut Rows, which: &str, grant: &str) {
    let f = match pick(ctx, which) { Ok(f) => f, Err(why) => return r.fail("fetched", which, why.as_str()) };
    let pkg = cell(format_args!("{}/{}", f.publisher(), f.name()));
    let arch = packages::this_arch().map(|a| a.as_str());
    let checked = match check::package(ctx, f.publisher(), f.name(), f.version(), arch, &mut |_, _| true) {
        Ok(c) => c,
        Err(why) => return r.fail("check", cell(format_args!("{} {}", pkg.as_str(), f.version())).as_str(), why.as_str()),
    };
    // The answer, each peer one the contract requests, and never the supervisor.
    let mut granted: [&str; MAX_PEERS] = [""; MAX_PEERS];
    let mut ng = 0usize;
    if grant != "none" {
        for p in grant.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if p == "supervisor" { return r.fail("grant", p, "`supervisor` is never granted to a package"); }
            if !checked.contract.peers().any(|q| q == p) { return r.fail("grant", p, "its contract does not request it"); }
            if granted[..ng].contains(&p) { continue; }
            if ng == MAX_PEERS { return r.fail("grant", grant, "more peers than a package is given"); }
            granted[ng] = p;
            ng += 1;
        }
    }
    let mut file = [0u8; 512];
    let mut fl = 0usize;
    for p in &granted[..ng] {
        if fl + p.len() + 1 > file.len() { break; }
        file[fl..fl + p.len()].copy_from_slice(p.as_bytes());
        file[fl + p.len()] = b'\n';
        fl += p.len() + 1;
    }
    let gpath = cell(format_args!("{}{}/{}/{}/grant", check::DIR, f.publisher(), f.name(), f.version()));
    if let Err(e) = gs::fs::Fs::new(ctx).write(gpath.as_str(), &file[..fl]) {
        return r.fail("grant", gpath.as_str(), cell(format_args!("could not be written: {:?}", e)).as_str());
    }
    let line = cell(format_args!("{} {}", pkg.as_str(), f.version()));
    if let Err(why) = write_list(ctx, pkg.as_str(), line.as_str()) { return r.fail("installed", LIST, why.as_str()); }

    // Start it: a running older version first stops, so the one listed is the one that runs.
    let _ = supervisor(ctx, supcmd::UNINSTALL, pkg.as_str().as_bytes(), 10);
    let started = supervisor(ctx, supcmd::INSTALL, line.as_str().as_bytes(), START_SECS);
    let utility = checked.kind == godspeed_package::Kind::Utility;
    let short = f.name();
    let state = match started {
        Ok(()) if utility => cell(format_args!("a utility: type `{} <arguments>` to run it", short)),
        Ok(()) => cell(format_args!("running, as the task {}", pkg.as_str())),
        Err(why) => {
            // Not started is not installed: the list says only what runs, or will at the next boot.
            let _ = write_list(ctx, pkg.as_str(), "");
            return r.fail("start", line.as_str(), why.as_str());
        }
    };
    let peers = if ng == 0 { cell(format_args!("none")) } else {
        let mut c = Cell::new();
        for (i, p) in granted[..ng].iter().enumerate() {
            let _ = core::fmt::Write::write_fmt(&mut c, format_args!("{}{}", if i > 0 { ", " } else { "" }, p));
        }
        c
    };
    r.columns(&["key", "value", "note"]);
    r.row("title", line.as_str(), "installed");
    r.row("about", state.as_str(), if utility { "checked by the supervisor; it is checked again every time it runs" }
          else { "started by the supervisor from the copy it checked" });
    r.row("binary", cell(format_args!("{}, {} bytes", checked.arch, checked.size)).as_str(), "its size and digest the manifest's");
    r.row("peers", peers.as_str(), "what the operator approved of what its contract requests");
    r.row("privilege", "none", "a package is given none");
    if utility {
        r.row("runs", "when typed", "for one command line; its output is shown or piped, and its status said");
    } else {
        r.row("boot", "starts at every boot", cell(format_args!("listed in {}", LIST)).as_str());
    }
    r.note(format_args!("checked from the disk: signature -> key -> contract -> binary"));
}

/// `godspeed uninstall <pkg>`: stop it and take it off the list. Its fetched copy stays (`godspeed remove` deletes it).
#[inline(never)]
pub fn uninstall(ctx: &ServiceContext, r: &mut Rows, pkg: &str) {
    let mut v = [0u8; 24];
    let Some(vl) = installed_version(ctx, pkg, &mut v) else {
        return r.fail("installed", pkg, "not installed here: `godspeed list installed` says what is");
    };
    let version = core::str::from_utf8(&v[..vl]).unwrap_or("?");
    if let Err(why) = write_list(ctx, pkg, "") { return r.fail("installed", LIST, why.as_str()); }
    let stopped = supervisor(ctx, supcmd::UNINSTALL, pkg.as_bytes(), 10);
    r.columns(&["key", "value", "note"]);
    r.row("title", cell(format_args!("{} {}", pkg, version)).as_str(), "uninstalled");
    r.row("about", match stopped { Ok(()) => "stopped", Err(_) => "was not running" }, "and will not start at boot");
    r.row("kept", cell(format_args!("{}{}/{}", check::DIR, pkg, version)).as_str(), "the fetched copy; `godspeed remove` deletes it");
}

/// `godspeed list installed`: the list, with whether the supervisor has each running. Columns `package version state`.
#[inline(never)]
pub fn list(ctx: &ServiceContext, r: &mut Rows) {
    let mut buf = [0u8; 1024];
    let list = match read_list(ctx, &mut buf) { Ok(l) => l, Err(why) => return r.fail("installed", LIST, why.as_str()) };
    // What the supervisor says is running, by name.
    let reply = gs::call::request_within(ctx, "supervisor", &Message::from_bytes(&[supcmd::MARKER, supcmd::INSTALLED]), 10).ok();
    // 0 not running, 1 running, 2 a utility (run when typed).
    let running = |pkg: &str| -> Option<u8> {
        let r = reply.as_ref()?;
        let b = r.payload_bytes();
        if b.first() != Some(&supcmd::OK) || b.len() < 2 { return None; }
        let mut i = 2usize;
        for _ in 0..b[1] {
            let l = *b.get(i)? as usize;
            let name = b.get(i + 1..i + 1 + l)?;
            let state = *b.get(i + 1 + l)?;
            if name == pkg.as_bytes() { return Some(state); }
            i += 2 + l;
        }
        Some(0)
    };
    r.columns(&["package", "version", "state"]);
    let mut n = 0u32;
    for l in list.lines() {
        let mut it = l.split_whitespace();
        let (Some(pkg), Some(version)) = (it.next(), it.next()) else { continue };
        let short = cell(format_args!("utility: type `{}`", pkg.rsplit('/').next().unwrap_or(pkg)));
        let state = match running(pkg) {
            Some(1) => "running",
            Some(2) => short.as_str(),
            Some(_) => "NOT running",
            None => "unknown: the supervisor did not answer",
        };
        r.entry(&[C::S(pkg), C::S(version), C::S(state)]);
        n += 1;
    }
    if n == 0 { r.note(format_args!("`godspeed install <publisher/name>` installs a fetched package")); }
    else { r.note(format_args!("{} package{}; a service starts at boot, a utility when typed", n, if n == 1 { "" } else { "s" })); }
}

/// One command to the supervisor, answered `[OK]` or `[FAILED, reason...]`.
#[inline(never)]
fn supervisor(ctx: &ServiceContext, op: u8, arg: &[u8], secs: i64) -> Result<(), Cell> {
    let mut m = [0u8; 120];
    if arg.len() + 2 > m.len() { return Err(cell(format_args!("the request is too long"))); }
    m[0] = supcmd::MARKER;
    m[1] = op;
    m[2..2 + arg.len()].copy_from_slice(arg);
    let reply = gs::call::request_within(ctx, "supervisor", &Message::from_bytes(&m[..2 + arg.len()]), secs)
        .map_err(|e| cell(format_args!("the supervisor did not answer: {:?}", e)))?;
    let b = reply.payload_bytes();
    match b.first() {
        Some(&supcmd::OK) => Ok(()),
        Some(&supcmd::FAILED) if b.len() > 1 => Err(cell(format_args!("{}", core::str::from_utf8(&b[1..]).unwrap_or("refused")))),
        _ => Err(cell(format_args!("refused"))),
    }
}
