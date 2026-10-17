// SPDX-License-Identifier: GPL-2.0-only
//! INSTALLED PACKAGES: services this machine starts from `/system/packages`, not from an image it was built with.
//!
//! `godspeed install` fetches nothing and starts nothing itself. It records the operator's answer - which of the
//! send peers the package's contract requests they approved - in the package's folder (`grant`), adds the package
//! to [`LIST`], and asks this supervisor to start it (`supcmd::INSTALL`). Starting it is here because only the
//! supervisor holds `IMAGE_SPAWN` (CLAUDE.md 14.1), and that authority is not delegatable.
//!
//! **What is checked, every time an installed package is started** - at install, at every boot, and at every restart:
//! the fetched copy, from the disk alone, by the same code `godspeed verify` runs (`godspeed-pm`'s `check.rs`, by
//! `#[path]`): the signature over its manifest, the signing key's standing in this machine's trust record, and the
//! contract and this machine's binary against the digests the manifest names. The binary is read ONCE, into
//! [`Region`] as it is hashed, and the bytes the kernel is handed are those bytes. Nothing is cached between starts,
//! so a copy damaged on disk stops starting rather than going on running from memory.
//!
//! **What it is given:** no privilege at all, a send capability to each peer that the grant names AND the contract
//! requests (never `supervisor`), an endpoint if its contract declares `ipc_receive`, and its contract's memory limit
//! up to [`MEMORY_MAX`]. It runs under its package name, `publisher/name`, which no built-in service can hold.
//!
//! **What it is not, recorded rather than hidden (26.7):** the grant file and the trust record are as protected as
//! `/system` is. Anything that can write there can widen a grant up to what the signed contract requests, or swap the
//! trust record the signature is checked against. The supervisor narrows to the contract, so a grant can never exceed
//! what the publisher signed; it cannot tell an operator's answer from a forged one.

use godspeed::{self as gs, ServiceContext};
use godspeed_package::{Arch, Kind};
use godspeed::memory::Region;
use godspeed_sdk::service_context::{SpawnRequest, SPAWN_FLAG_REQ_CONSOLE, SPAWN_FLAG_REQ_RECV, SPAWN_FLAG_WATCHED};

/// The word an app's grant uses for the console - the screen and the keyboard while it runs - beside its peers.
pub const CONSOLE_GRANT: &str = "console";

use crate::check::{self, Text, MAX_PEERS, PEER_MAX};

/// The installed packages, one per line: `publisher/name version`.
pub const LIST: &str = "/system/packages/installed";
/// The largest image an installed package may have. One region of this size is asked for, once, at the first install.
pub const IMAGE_MAX: usize = 1024 * 1024;
/// The most memory an installed package is given, whatever its contract says.
pub const MEMORY_MAX: u64 = 64 * 1024 * 1024;
/// ...and what it is given when its contract names no limit.
const MEMORY_DEFAULT: u64 = 32 * 1024 * 1024;
/// The most packages installed at once.
pub const MAX_INSTALLED: usize = 8;
/// How many times an installed package is restarted after it dies, per boot of this supervisor, before it is left
/// down. A package that crashes as it starts would otherwise be read, checked and started forever (26.6).
pub const RESTARTS_MAX: u32 = 5;
/// The longest package name, `publisher/name`.
const NAME_MAX: usize = 80;

/// This machine's architecture, in the package format's spelling.
fn this_arch() -> Option<&'static str> {
    Arch::ALL.into_iter().find(|a| env!("SUPERVISOR_TARGET").starts_with(a.as_str())).map(|a| a.as_str())
}

#[derive(Clone, Copy)]
struct Entry {
    name: [u8; NAME_MAX],
    nl: usize,
    version: [u8; 24],
    vl: usize,
    restarts: u32,
    /// A utility: checked at install and at boot, started only when the shell asks, never restarted.
    utility: bool,
    /// ...and an app: a utility that owns the console while it runs.
    app: bool,
}

impl Entry {
    fn name(&self) -> &str { core::str::from_utf8(&self.name[..self.nl]).unwrap_or("") }
    fn version(&self) -> &str { core::str::from_utf8(&self.version[..self.vl]).unwrap_or("") }
}

fn text(args: core::fmt::Arguments<'_>) -> Text {
    let mut t = Text::new();
    let _ = core::fmt::Write::write_fmt(&mut t, args);
    t
}

/// `publisher/name`, each part a package-name part: lower-case letters, digits and `-`.
fn split(pkg: &str) -> Option<(&str, &str)> {
    let (p, n) = pkg.split_once('/')?;
    let part = |s: &str| !s.is_empty() && s.len() <= 39 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if part(p) && part(n) { Some((p, n)) } else { None }
}

fn version_ok(v: &str) -> bool {
    !v.is_empty() && v.len() <= 24 && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'+')
}

/// Is a task called `name` running? (The supervisor's own `name_alive`, which this module does not reach.)
fn alive(ctx: &ServiceContext, name: &str) -> bool {
    (0..256u32).any(|slot| {
        let st = ctx.task_stat(slot);
        st.valid && st.state != 4 && st.name_str() == name
    })
}

pub struct Installed {
    e: [Option<Entry>; MAX_INSTALLED],
    image: Option<Region>,
}

impl Installed {
    pub fn new() -> Installed { Installed { e: [None; MAX_INSTALLED], image: None } }

    fn find(&self, name: &str) -> Option<usize> {
        self.e.iter().position(|x| x.is_some_and(|x| x.name() == name))
    }

    /// Is `task` an installed package's? Its death is this module's to answer.
    pub fn has(&self, task: &str) -> bool { self.find(task).is_some() }

    /// Is `task` an installed UTILITY - one the shell may ask to run (`supcmd::SPAWN`)?
    pub fn is_utility(&self, task: &str) -> bool {
        self.find(task).and_then(|i| self.e[i]).is_some_and(|e| e.utility)
    }

    /// `[OK, count, (len, name, state)*]` of what is installed - state 0 not running, 1 running, 2 a utility, run
    /// when typed, 3 an app, the same with the console: for `godspeed list installed` and the shell's lookup.
    pub fn answer(&self, ctx: &ServiceContext, out: &mut [u8]) -> usize {
        out[0] = gs_ok();
        let mut n = 2usize;
        let mut count = 0u8;
        for x in self.e.iter().flatten() {
            let name = x.name().as_bytes();
            if n + 2 + name.len() > out.len() { break; }
            out[n] = name.len() as u8;
            out[n + 1..n + 1 + name.len()].copy_from_slice(name);
            out[n + 1 + name.len()] = if x.app { 3 } else if x.utility { 2 } else { alive(ctx, x.name()) as u8 };
            n += 2 + name.len();
            count += 1;
        }
        out[1] = count;
        n
    }

    /// Start `pkg` (`publisher/name`) at `version`, checked from the disk; remember it so its death restarts it.
    pub fn start(&mut self, ctx: &ServiceContext, pkg: &str, version: &str) -> Result<(), Text> {
        let Some((publisher, name)) = split(pkg) else { return Err(text(format_args!("`{}` is not a package name", pkg))) };
        if !version_ok(version) { return Err(text(format_args!("`{}` is not a version", version))); }
        let slot = match self.find(pkg) {
            Some(i) => i,
            None => self.e.iter().position(|x| x.is_none())
                .ok_or_else(|| text(format_args!("{} packages are installed already, the most this machine runs", MAX_INSTALLED)))?,
        };
        let restarts = self.e[slot].map_or(0, |x| x.restarts);
        if alive(ctx, pkg) {
            // A respawned supervisor finds what the last one started still running: adopt it, do not start a second.
            let kind = self.e[slot].map_or((false, false), |x| (x.utility, x.app));
            self.remember(slot, pkg, version, restarts, kind);
            return Ok(());
        }
        let kind = self.run(ctx, publisher, name, pkg, version, None)?;
        self.remember(slot, pkg, version, restarts, kind);
        Ok(())
    }

    /// Run the installed utility `task`, for the shell: `peers` are the shell's own (it reads the output), and
    /// its grant adds the rest. One at a time: a utility still running is refused rather than started twice.
    pub fn run_utility(&mut self, ctx: &ServiceContext, task: &str, peers: &[&str]) -> Result<(), Text> {
        let Some(e) = self.find(task).and_then(|i| self.e[i]) else { return Err(text(format_args!("`{}` is not installed", task))) };
        if !e.utility { return Err(text(format_args!("`{}` is a service, started at boot - not run on demand", task))); }
        if peers.iter().any(|p| *p != "shell") {
            return Err(text(format_args!("a utility is started for the shell, with the shell as its peer and nothing else asked")));
        }
        if alive(ctx, task) { return Err(text(format_args!("`{}` is already running", task))); }
        let Some((publisher, name)) = split(e.name()) else { return Err(text(format_args!("`{}` is not a package name", task))) };
        self.run(ctx, publisher, name, e.name(), e.version(), Some(peers)).map(|_| ())
    }

    fn remember(&mut self, slot: usize, pkg: &str, version: &str, restarts: u32, (utility, app): (bool, bool)) {
        let mut e = Entry { name: [0; NAME_MAX], nl: pkg.len(), version: [0; 24], vl: version.len(), restarts, utility, app };
        e.name[..pkg.len()].copy_from_slice(pkg.as_bytes());
        e.version[..version.len()].copy_from_slice(version.as_bytes());
        self.e[slot] = Some(e);
    }

    /// Check the fetched copy, read its grant, and spawn it. Whether it is a utility, and an app. A utility (and an
    /// app) is spawned only when `first` is given - the shell asking to run it, with its own peers first - and
    /// otherwise only checked.
    fn run(&mut self, ctx: &ServiceContext, publisher: &str, name: &str, pkg: &str, version: &str,
           first: Option<&[&str]>) -> Result<(bool, bool), Text> {
        let arch = this_arch().ok_or_else(|| text(format_args!("this machine's architecture has no package spelling")))?;
        if self.image.is_none() {
            self.image = Some(Region::alloc(ctx, IMAGE_MAX)
                .map_err(|e| text(format_args!("no memory to hold an image ({:?})", e)))?);
        }
        let Some(region) = self.image.as_mut() else { return Err(text(format_args!("no memory to hold an image"))) };
        let buf = region.bytes_mut();
        let checked = check::package(ctx, publisher, name, version, Some(arch), &mut |off, bytes| {
            let (o, end) = (off as usize, off as usize + bytes.len());
            if end > buf.len() { return false; }
            buf[o..end].copy_from_slice(bytes);
            true
        })?;

        // The operator's answer, narrowed to what the signed contract requests.
        let mut grant = [0u8; 512];
        let gl = {
            let mut p = Text::new();
            let _ = core::fmt::Write::write_fmt(&mut p, format_args!("{}{}/{}/{}/grant", check::DIR, publisher, name, version));
            match gs::fs::Fs::new(ctx).read_into(p.as_str(), &mut grant) {
                Ok(n) => n,
                Err(gs::Error::NotFound) => 0,
                Err(e) => return Err(text(format_args!("its grant could not be read: {:?}", e))),
            }
        };
        let app = checked.kind == Kind::App;
        let utility = checked.kind == Kind::Utility || app;
        if utility && first.is_none() {
            ctx.log_fmt(format_args!("supervisor: installed {} {} checked from disk - {}, run when typed", pkg, version,
                if app { "an app" } else { "a utility" }));
            return Ok((utility, app));
        }
        // The console: for an app, and only when the operator's answer gave it.
        let mut console = false;
        let mut peers: [&str; MAX_PEERS + 1] = [""; MAX_PEERS + 1];
        let mut np = 0usize;
        // The shell first: a utility's output goes to its first peer (`gs::program::Output`).
        for p in first.unwrap_or(&[]) { if np < peers.len() { peers[np] = p; np += 1; } }
        for p in core::str::from_utf8(&grant[..gl]).unwrap_or("").lines().map(str::trim).filter(|p| !p.is_empty()) {
            if p == CONSOLE_GRANT {
                if !app { return Err(text(format_args!("its grant names the console, which only an app is given"))); }
                console = true;
                continue;
            }
            if p == "supervisor" { return Err(text(format_args!("its grant names `supervisor`, which no package is given"))); }
            if !checked.contract.peers().any(|q| q == p) {
                return Err(text(format_args!("its grant names `{}`, which its contract does not request", p)));
            }
            if np < peers.len() && p.len() <= PEER_MAX && !peers[..np].contains(&p) { peers[np] = p; np += 1; }
        }

        let region = self.image.as_ref().ok_or_else(|| text(format_args!("no memory to hold an image")))?;
        let mut req = SpawnRequest::new(&region.bytes()[..checked.size as usize], pkg);
        req.core = u32::MAX;
        // A utility always has an endpoint: its arguments arrive on it.
        req.flags = SPAWN_FLAG_WATCHED | if checked.contract.receives || utility { SPAWN_FLAG_REQ_RECV } else { 0 };
        if app {
            // An app without the console it was approved for cannot be what it is: refused, not run blind.
            if !console { return Err(text(format_args!("it is an app, and its grant does not give it the console"))); }
            req.flags |= SPAWN_FLAG_REQ_CONSOLE;
        }
        req.memory_limit = match checked.contract.memory_limit { 0 => MEMORY_DEFAULT, m => m.min(MEMORY_MAX) };
        req.privileges = 0;
        let mut out = [0u8; 128];
        match ctx.spawn_image(&mut req, &mut out, &peers[..np]) {
            Ok(cap) => {
                // Nobody asked for a capability to it here; clients reach it by name, as every service.
                if let Some(c) = cap { gs::cap::remove(ctx, c.into()); }
                ctx.log_fmt(format_args!("supervisor: installed {} {} started ({} bytes, {} peer(s), checked from disk)",
                    pkg, version, checked.size, np));
                Ok((utility, app))
            }
            Err(e) => Err(text(format_args!("the kernel refused to start it: {:?}", e))),
        }
    }

    /// Stop `pkg` and forget it. False when it was not installed here.
    pub fn stop(&mut self, ctx: &ServiceContext, pkg: &str) -> bool {
        let Some(i) = self.find(pkg) else { return false };
        // Forgotten FIRST, so its death notice finds nothing to restart.
        self.e[i] = None;
        let _ = ctx.kill(pkg);
        ctx.log_fmt(format_args!("supervisor: installed {} stopped and forgotten", pkg));
        true
    }

    /// An installed package died: start it again from the disk, up to [`RESTARTS_MAX`] times - unless it ENDED
    /// ITSELF (`exited`), which is a program that finished, not one that failed, and is not started again.
    pub fn on_death(&mut self, ctx: &ServiceContext, task: &str, how: &dyn core::fmt::Display, exited: bool) {
        let Some(i) = self.find(task) else { return };
        let Some(mut e) = self.e[i] else { return };
        if alive(ctx, task) { return; }
        if exited || e.utility {
            // Finished (it said so), or a utility, which runs for one command and is never kept running.
            ctx.log_fmt(format_args!("supervisor: installed {} ended{} - not restarted", task, how));
            return;
        }
        if e.restarts >= RESTARTS_MAX {
            ctx.log_fmt(format_args!("supervisor: installed {} died{} - restarted {} times already, left down",
                task, how, RESTARTS_MAX));
            return;
        }
        e.restarts += 1;
        self.e[i] = Some(e);
        ctx.log_fmt(format_args!("supervisor: installed {} died{}, restarting ({} of {})", task, how, e.restarts, RESTARTS_MAX));
        let Some((publisher, name)) = split(e.name()) else { return };
        if let Err(why) = self.run(ctx, publisher, name, e.name(), e.version(), None) {
            ctx.log_fmt(format_args!("supervisor: installed {} restart FAILED: {}", task, why.as_str()));
        }
    }

    /// At boot: start every package [`LIST`] names. A missing list is nothing installed.
    pub fn boot(&mut self, ctx: &ServiceContext) {
        let mut list = [0u8; 1024];
        let n = match gs::fs::Fs::new(ctx).read_into(LIST, &mut list) {
            Ok(n) => n,
            Err(gs::Error::NotFound) => return,
            Err(e) => { ctx.log_fmt(format_args!("supervisor: {} could not be read ({:?}) - no installed package started", LIST, e)); return; }
        };
        for line in core::str::from_utf8(&list[..n]).unwrap_or("").lines() {
            let mut it = line.split_whitespace();
            let (Some(pkg), Some(version)) = (it.next(), it.next()) else { continue };
            if let Err(why) = self.start(ctx, pkg, version) {
                ctx.log_fmt(format_args!("supervisor: installed {} {} NOT started: {}", pkg, version, why.as_str()));
            }
        }
    }
}

fn gs_ok() -> u8 { godspeed_sdk::service_context::supcmd::OK }
