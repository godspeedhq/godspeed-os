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

use godspeed_package::{Kind, Requirement, Version, VersionReq, MAX_REQUIRES};

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

// ---- Dependencies (`docs/developer-services-design.md` 16) ------------------------------------------------------

/// How deep requirements may nest in one plan, and how many packages one plan may install.
const PLAN_DEPTH: usize = 4;
const PLAN_MAX: usize = 8;

/// What an installed or fetched release requires, and its kind, from the manifest beside it. Read, not verified: it
/// was verified when it was fetched, and the supervisor verifies it again before anything of it runs.
#[inline(never)]
fn manifest_of(ctx: &ServiceContext, pkg: &str, version: &str) -> Result<([Requirement; MAX_REQUIRES], usize, Kind), Cell> {
    let path = cell(format_args!("{}{}/{}/manifest.toml", check::DIR, pkg, version));
    let mut buf = [0u8; check::MANIFEST_MAX];
    let n = gs::fs::Fs::new(ctx).read_into(path.as_str(), &mut buf)
        .map_err(|e| cell(format_args!("{} could not be read: {:?}", path.as_str(), e)))?;
    let text = core::str::from_utf8(&buf[..n]).map_err(|_| cell(format_args!("{} is not text", path.as_str())))?;
    let mut arts = crate::meta::Artifacts::new();
    let m = crate::meta::manifest(text, &mut arts).map_err(|b| cell(format_args!("{} line {}: {}", path.as_str(), b.line, b.why)))?.manifest;
    let mut reqs = [Requirement::default(); MAX_REQUIRES];
    let n = m.requires.len().min(MAX_REQUIRES);
    reqs[..n].copy_from_slice(&m.requires[..n]);
    Ok((reqs, n, m.kind))
}

/// The installed version of `pkg`, read as a version.
fn installed(ctx: &ServiceContext, pkg: &str) -> Option<Version> {
    let mut v = [0u8; 24];
    let l = installed_version(ctx, pkg, &mut v)?;
    core::str::from_utf8(&v[..l]).ok()?.parse().ok()
}

/// Every installed package other than `pkg` that requires `pkg`, handed to `each` with its version and the range it
/// requires; `each` returns true to stop.
#[inline(never)]
fn installed_requiring(ctx: &ServiceContext, pkg: &str, each: &mut dyn FnMut(&str, &str, VersionReq) -> bool) -> Result<(), Cell> {
    let mut buf = [0u8; 1024];
    let list = read_list(ctx, &mut buf)?;
    for l in list.lines() {
        let mut it = l.split_whitespace();
        let (Some(p), Some(v)) = (it.next(), it.next()) else { continue };
        if p == pkg { continue; }
        let (reqs, n, _) = manifest_of(ctx, p, v)?;
        for r in &reqs[..n] {
            if cell(format_args!("{}", r.name())).as_str() == pkg && each(p, v, r.range) { return Ok(()); }
        }
    }
    Ok(())
}

/// The newest FETCHED version of `pkg` in `range`.
#[inline(never)]
fn fetched_in(ctx: &ServiceContext, pkg: &str, range: VersionReq) -> Option<Version> {
    let mut all = [Fetched::EMPTY_PUB; MAX_FETCHED];
    let mut more = false;
    let n = packages::fetched(ctx, &mut all, &mut more).ok()?;
    all.iter().take(n)
        .filter(|f| cell(format_args!("{}/{}", f.publisher(), f.name())).as_str() == pkg)
        .filter_map(|f| f.version().parse::<Version>().ok())
        .filter(|v| range.matches(v))
        .max()
}

/// One package a plan installs, or fetches first.
#[derive(Clone, Copy)]
struct Step {
    pkg: Cell,
    version: Option<Version>,
    range: Option<VersionReq>,
    kind: Kind,
    asks: Cell,
    by: Cell,
}

/// A plan: the packages to install, dependencies first, and those that must be fetched before it can be made.
struct Plan {
    steps: [Option<Step>; PLAN_MAX],
    n: usize,
    /// Names being resolved, deepest last: one met again is a cycle.
    path: [Cell; PLAN_DEPTH + 1],
    depth: usize,
    kept: u32,
}

/// What a checked release asks for, as the question shows it: `console` for an app, `device` for a driver, then
/// its contract's send peers; `nothing` when there are none.
fn asks_of(c: &check::Checked) -> Cell {
    let mut a = Cell::new();
    let mut first = true;
    let mut add = |s: &str| { let _ = core::fmt::Write::write_fmt(&mut a, format_args!("{}{}", if first { "" } else { ", " }, s)); first = false; };
    if c.kind == Kind::App { add("console"); }
    if c.kind == Kind::Driver { add("device"); }
    for p in c.contract.peers() { add(p); }
    if first { a = cell(format_args!("nothing")); }
    a
}

impl Plan {
    fn has(&self, pkg: &str) -> Option<&Step> {
        self.steps[..self.n].iter().flatten().find(|s| s.pkg.as_str() == pkg)
    }

    /// Resolve `req`, needed by `by`: kept if installed in range, refused if installed out of range, planned if
    /// fetched in range, to be fetched if not here at all. Refused too for a cycle, nesting past `PLAN_DEPTH`, more
    /// than `PLAN_MAX` packages, or a requirement that is not a service or a driver. Its own requirements first, so
    /// the plan's order is the order to install in.
    fn resolve(&mut self, ctx: &ServiceContext, req: &Requirement, by: &str) -> Result<(), Cell> {
        let name = cell(format_args!("{}", req.name()));
        if let Some(s) = self.has(name.as_str()) {
            return match s.version {
                Some(v) if req.range.matches(&v) => Ok(()),
                Some(v) => Err(cell(format_args!("{} needs {} {}, and this plan already takes {} {} for another package - no one version will do",
                    by, name.as_str(), req.range, name.as_str(), v))),
                None => Ok(()),
            };
        }
        if self.path[..self.depth].iter().any(|p| p.as_str() == name.as_str()) {
            return Err(cell(format_args!("{} and {} require each other, which cannot be installed in any order", by, name.as_str())));
        }
        if let Some(v) = installed(ctx, name.as_str()) {
            if req.range.matches(&v) { self.kept += 1; return Ok(()); }
            return Err(cell(format_args!("{} needs {} {}, and {} is installed - `godspeed upgrade {}` first, or install a {} that needs that version",
                by, name.as_str(), req.range, v, name.as_str(), by)));
        }
        if self.depth == PLAN_DEPTH {
            return Err(cell(format_args!("requirements nest deeper than {} packages at {}", PLAN_DEPTH, name.as_str())));
        }
        if self.n == PLAN_MAX {
            return Err(cell(format_args!("this needs more than {} packages installed at once", PLAN_MAX)));
        }
        let Some(v) = fetched_in(ctx, name.as_str(), req.range) else {
            // Fetched first, then planned again: the shell fetches the newest release in range.
            self.steps[self.n] = Some(Step { pkg: name, version: None, range: Some(req.range), kind: Kind::Service,
                                             asks: Cell::new(), by: cell(format_args!("{}", by)) });
            self.n += 1;
            return Ok(());
        };
        let vt = cell(format_args!("{}", v));
        let (publisher, short) = (req.name().publisher(), req.name().name());
        let c = check::package(ctx, publisher, short, vt.as_str(), packages::this_arch().map(|a| a.as_str()), &mut |_, _| true)
            .map_err(|why| cell(format_args!("{} {}, needed by {}: {}", name.as_str(), vt.as_str(), by, why.as_str())))?;
        if c.kind != Kind::Service && c.kind != Kind::Driver {
            return Err(cell(format_args!("{} needs {}, which is {} - only a service or a driver runs to be needed",
                by, name.as_str(), if c.kind == Kind::App { "an app" } else { "a utility" })));
        }
        self.path[self.depth] = name;
        self.depth += 1;
        for r in c.requires() { self.resolve(ctx, r, name.as_str())?; }
        self.depth -= 1;
        if self.n == PLAN_MAX {
            return Err(cell(format_args!("this needs more than {} packages installed at once", PLAN_MAX)));
        }
        self.steps[self.n] = Some(Step { pkg: name, version: Some(v), range: Some(req.range), kind: c.kind,
                                         asks: asks_of(&c), by: cell(format_args!("{}", by)) });
        self.n += 1;
        Ok(())
    }
}

/// `godspeed plan <pkg>[@ver]`: what installing it takes, for the shell's one question. One row per package, in the
/// order to install them - its requirements first and itself last; columns `package version kind asks state`, state
/// `install` (fetched, checked) or `fetch` (to be fetched first: version is the range). Installed packages that
/// already satisfy a range are kept, and counted in the note. A plan that cannot be made fails, saying why.
#[inline(never)]
pub fn plan(ctx: &ServiceContext, r: &mut Rows, which: &str) {
    let f = match pick(ctx, which) { Ok(f) => f, Err(why) => return r.fail("fetched", which, why.as_str()) };
    let pkg = cell(format_args!("{}/{}", f.publisher(), f.name()));
    let root = match check::package(ctx, f.publisher(), f.name(), f.version(), packages::this_arch().map(|a| a.as_str()), &mut |_, _| true) {
        Ok(c) => c,
        Err(why) => return r.fail("check", cell(format_args!("{} {}", pkg.as_str(), f.version())).as_str(), why.as_str()),
    };
    let mut p = Plan { steps: [None; PLAN_MAX], n: 0, path: [Cell::new(); PLAN_DEPTH + 1], depth: 1, kept: 0 };
    p.path[0] = pkg;
    for req in root.requires() {
        if let Err(why) = p.resolve(ctx, req, pkg.as_str()) { return r.fail("requires", pkg.as_str(), why.as_str()); }
    }
    r.columns(&["package", "version", "kind", "asks", "state"]);
    let kind = |k: Kind| match k { Kind::Utility => "a utility", Kind::App => "an app", Kind::Driver => "a driver", _ => "a service" };
    let mut fetch = 0u32;
    for s in p.steps[..p.n].iter().flatten() {
        match s.version {
            Some(v) => r.entry(&[C::S(s.pkg.as_str()), C::S(cell(format_args!("{}", v)).as_str()), C::S(kind(s.kind)), C::S(s.asks.as_str()),
                                 C::S(cell(format_args!("install, needed by {}", s.by.as_str())).as_str())]),
            None => {
                fetch += 1;
                let range = s.range.map(|r| cell(format_args!("{}", r))).unwrap_or_else(Cell::new);
                r.entry(&[C::S(s.pkg.as_str()), C::S(range.as_str()), C::S("-"), C::S("-"),
                          C::S(cell(format_args!("fetch, needed by {}", s.by.as_str())).as_str())])
            }
        }
    }
    r.entry(&[C::S(pkg.as_str()), C::S(f.version()), C::S(kind(root.kind)), C::S(asks_of(&root).as_str()), C::S("install")]);
    r.note(format_args!("{} {}: {} to install, {} to fetch first, {} already installed and kept",
        pkg.as_str(), f.version(), p.n as u32 + 1 - fetch, fetch, p.kept));
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
    if checked.kind == godspeed_package::Kind::App {
        r.entry(&[C::S("console"), C::S("the screen and the keyboard while it runs"), C::S("an app is given it by its kind")]);
    }
    if checked.kind == godspeed_package::Kind::Driver {
        let class = checked.contract.pci_class.map_or(cell(format_args!("none named")), |c| cell(format_args!("{:06x}", c)));
        r.entry(&[C::S("device"), C::S(cell(format_args!("the PCI device of class {}: its registers", class.as_str())).as_str()),
                  C::S("where no IOMMU confines it, what the device itself can do, its driver can tell it to")]);
    }
    for p in checked.contract.peers() {
        r.entry(&[C::S(p), C::S("a send capability to it"), C::S(if p == "supervisor" { "never granted to a package" } else { "" })]);
    }
    // The kind rides in the note, so the shell's question says what installing it does: a service starts at
    // every boot, a utility becomes a command.
    let kind = match checked.kind {
        godspeed_package::Kind::Utility => "a utility",
        godspeed_package::Kind::App => "an app",
        godspeed_package::Kind::Driver => "a driver",
        _ => "a service",
    };
    let pkg = cell(format_args!("{}/{}", f.publisher(), f.name()));
    let mut iv = [0u8; 24];
    let now = installed_version(ctx, pkg.as_str(), &mut iv).map(|l| cell(format_args!("; installed {}", core::str::from_utf8(&iv[..l]).unwrap_or("?"))))
        .unwrap_or_else(Cell::new);
    r.note(format_args!("{}/{} {} requests these; {}{}; the operator answers each", f.publisher(), f.name(), f.version(), kind, now.as_str()));
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
    // ITS REQUIREMENTS ARE INSTALLED, in range, before it is - the shell's plan installs them first, and this holds
    // for any other way of asking too (`docs/developer-services-design.md` 16.5).
    for req in checked.requires() {
        let name = cell(format_args!("{}", req.name()));
        match installed(ctx, name.as_str()) {
            Some(v) if req.range.matches(&v) => {}
            Some(v) => return r.fail("requires", name.as_str(), cell(format_args!("{} needs {} {}, and {} is installed", pkg.as_str(), name.as_str(), req.range, v)).as_str()),
            None => return r.fail("requires", name.as_str(), cell(format_args!("{} needs {} {}, which is not installed - `godspeed install {}` installs both", pkg.as_str(), name.as_str(), req.range, pkg.as_str())).as_str()),
        }
    }
    // NO INSTALLED PACKAGE IS LEFT WITHOUT A VERSION IT NEEDS (16.6): an upgrade, a downgrade or a replacement of this
    // one is refused if an installed dependent's range does not take the new version.
    let new_v: Option<Version> = f.version().parse().ok();
    let mut broken: Option<Cell> = None;
    if let Err(why) = installed_requiring(ctx, pkg.as_str(), &mut |dep, dv, range| {
        if new_v.is_some_and(|v| !range.matches(&v)) {
            broken = Some(cell(format_args!("{} {} needs {} {}, and this is {} - upgrade or uninstall {} first", dep, dv, pkg.as_str(), range, f.version(), dep)));
            true
        } else { false }
    }) { return r.fail("requires", LIST, why.as_str()); }
    if let Some(why) = broken { return r.fail("requires", pkg.as_str(), why.as_str()); }
    // The answer, each peer one the contract requests, and never the supervisor.
    let mut granted: [&str; MAX_PEERS] = [""; MAX_PEERS];
    let mut ng = 0usize;
    if grant != "none" {
        for p in grant.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if p == "supervisor" { return r.fail("grant", p, "`supervisor` is never granted to a package"); }
            let console = p == "console" && checked.kind == godspeed_package::Kind::App;
            let device = p == "device" && checked.kind == godspeed_package::Kind::Driver;
            if !console && !device && !checked.contract.peers().any(|q| q == p) { return r.fail("grant", p, "its contract does not request it"); }
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
    // What was installed before, read before the list changes: a start that fails puts it back, rather than leaving
    // nothing installed.
    let mut before = [0u8; 24];
    let prev = installed_version(ctx, pkg.as_str(), &mut before)
        .map(|l| cell(format_args!("{} {}", pkg.as_str(), core::str::from_utf8(&before[..l]).unwrap_or(""))));
    if let Err(why) = write_list(ctx, pkg.as_str(), line.as_str()) { return r.fail("installed", LIST, why.as_str()); }

    // Start it: a running other version first stops, so the one listed is the one that runs.
    // Another version running: replaced LIVE where both versions can hand over, stopped and started where they cannot
    // (the supervisor decides, and says which). Nothing running: started.
    let mut how = 0u8;
    let started = if prev.as_ref().is_some_and(|p| p.as_str() != line.as_str()) {
        supervisor_how(ctx, supcmd::HANDOVER, line.as_str().as_bytes(), START_SECS).map(|h| { how = h; })
    } else {
        let _ = supervisor(ctx, supcmd::UNINSTALL, pkg.as_str().as_bytes(), 10);
        supervisor(ctx, supcmd::INSTALL, line.as_str().as_bytes(), START_SECS)
    };
    if let (Err(why), Some(p)) = (&started, &prev) {
        if p.as_str() != line.as_str() {
            // The new one did not start: the previous one back, on the list and running, and both said.
            let restored = write_list(ctx, pkg.as_str(), p.as_str()).is_ok()
                && supervisor(ctx, supcmd::INSTALL, p.as_str().as_bytes(), START_SECS).is_ok();
            return r.fail("start", line.as_str(), cell(format_args!("{} - {}", why.as_str(),
                if restored { "the version installed before is back, and running" }
                else { "and the version installed before could not be started again either" })).as_str());
        }
    }
    let app = checked.kind == godspeed_package::Kind::App;
    let utility = checked.kind == godspeed_package::Kind::Utility || app;
    let short = f.name();
    let state = match started {
        Ok(()) if app => cell(format_args!("an app: type `{} <arguments>` to run it; it has the screen until it ends", short)),
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
        match how {
            b'H' => r.row("replaced", "live, with no restart", "the running version handed its state to this one and ended"),
            b'R' => r.row("replaced", "stopped and started", "the running version, or this one, cannot hand over (`handover` in its contract)"),
            _ => {}
        }
    }
    r.note(format_args!("checked from the disk: signature -> key -> contract -> binary"));
}

/// `godspeed uninstall <pkg>`: stop it and take it off the list. Its fetched copy stays (`godspeed remove` deletes it).
#[inline(never)]
pub fn uninstall(ctx: &ServiceContext, r: &mut Rows, pkg: &str, purge: bool) {
    let mut v = [0u8; 24];
    let Some(vl) = installed_version(ctx, pkg, &mut v) else {
        return r.fail("installed", pkg, "not installed here: `godspeed list installed` says what is");
    };
    let version = core::str::from_utf8(&v[..vl]).unwrap_or("?");
    // NOT WHILE SOMETHING INSTALLED NEEDS IT (16.6): the dependent first.
    let mut needed: Option<Cell> = None;
    if let Err(why) = installed_requiring(ctx, pkg, &mut |dep, dv, range| {
        needed = Some(cell(format_args!("{} {} requires it ({}) - uninstall {} first", dep, dv, range, dep)));
        true
    }) { return r.fail("installed", LIST, why.as_str()); }
    if let Some(why) = needed { return r.fail("requires", pkg, why.as_str()); }
    if let Err(why) = write_list(ctx, pkg, "") { return r.fail("installed", LIST, why.as_str()); }
    let stopped = supervisor(ctx, supcmd::UNINSTALL, pkg.as_bytes(), 10);
    r.columns(&["key", "value", "note"]);
    r.row("title", cell(format_args!("{} {}", pkg, version)).as_str(), "uninstalled");
    r.row("about", match stopped { Ok(()) => "stopped", Err(_) => "was not running" }, "and will not start at boot, or run when typed");
    if !purge {
        r.row("kept", cell(format_args!("{}{}/{}", check::DIR, pkg, version)).as_str(), "the fetched copy; `godspeed uninstall ... purge` or `godspeed remove` deletes it");
    }
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
        let app_short = cell(format_args!("app: type `{}`", pkg.rsplit('/').next().unwrap_or(pkg)));
        let state = match running(pkg) {
            Some(1) => "running",
            Some(2) => short.as_str(),
            Some(3) => app_short.as_str(),
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
/// [`supervisor`], for a command that answers `[OK, how]`: the `how` byte.
fn supervisor_how(ctx: &ServiceContext, op: u8, arg: &[u8], secs: i64) -> Result<u8, Cell> {
    let mut m = [0u8; 120];
    if arg.len() + 2 > m.len() { return Err(cell(format_args!("the request is too long"))); }
    m[0] = supcmd::MARKER;
    m[1] = op;
    m[2..2 + arg.len()].copy_from_slice(arg);
    let reply = gs::call::request_within(ctx, "supervisor", &Message::from_bytes(&m[..2 + arg.len()]), secs)
        .map_err(|e| cell(format_args!("the supervisor did not answer: {:?}", e)))?;
    let b = reply.payload_bytes();
    match b.first() {
        Some(&supcmd::OK) => Ok(b.get(1).copied().unwrap_or(0)),
        Some(&supcmd::FAILED) if b.len() > 1 => Err(cell(format_args!("{}", core::str::from_utf8(&b[1..]).unwrap_or("refused")))),
        _ => Err(cell(format_args!("refused"))),
    }
}

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
