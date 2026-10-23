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
//! up to [`MEMORY_MAX`]; a driver, its one device - a PCI device by class, with the registers, the interrupt its
//! contract asks for and a DMA arena the operator granted (`memory`), or a USB device by vendor and product, through
//! its host - taken over from a built-in driver if one holds it. It runs under its package name, `publisher/name`,
//! which no built-in service can hold.
//!
//! **What it is not, recorded rather than hidden (26.7):** the grant file and the trust record are as protected as
//! `/system` is. Anything that can write there can widen a grant up to what the signed contract requests, or swap the
//! trust record the signature is checked against. The supervisor narrows to the contract, so a grant can never exceed
//! what the publisher signed; it cannot tell an operator's answer from a forged one.

use godspeed::{self as gs, ServiceContext};
use godspeed_package::{Arch, Kind};
use godspeed::memory::Region;
use godspeed_sdk::service_context::{usbdev, SpawnRequest, SPAWN_FLAG_HANDOVER, SPAWN_FLAG_REQ_CONSOLE, SPAWN_FLAG_REQ_RECV, SPAWN_FLAG_WATCHED};

/// The word an app's grant uses for the console - the screen and the keyboard while it runs - beside its peers.
pub const CONSOLE_GRANT: &str = "console";
/// ...and the word a driver's uses for its device.
pub const DEVICE_GRANT: &str = "device";
/// ...and for its DMA arena, which the operator answers by a question of its own (`docs/developer-services-design.md`
/// 13.1): where no IOMMU confines the device, a driver with an arena can reach all of memory.
pub const MEMORY_GRANT: &str = "memory";

use crate::check::{self, Text, MAX_PEERS, PEER_MAX};

/// The installed packages, one per line: `publisher/name version`.
pub const LIST: &str = "/system/packages/installed";
/// The most installed packages one boot starts in order (the list is 1 KiB, a line at least 8 bytes).
const BOOT_MAX: usize = 128;
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
/// The largest DMA arena an installed driver is given, whatever its contract asks (`hw_dma`).
pub const DMA_MAX: u64 = 2 * 1024 * 1024;
/// How long a built-in driver is given to end when a package takes its device over, before the package is refused.
const STAND_DOWN_MS: u64 = 2000;

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
    /// A driver's device class, once it has been started with it: one installed driver per class.
    pci_class: Option<u32>,
    /// ...or the USB device (vendor, product) a driver HOLDS, through its host: one installed driver per device. Taken
    /// away when it gives the device back (`give_back`).
    usb: Option<(u16, u16)>,
    /// The USB device its contract NAMES, kept when it gives the device back: a replug finds the package by it, and
    /// starts it again (`usb_report`).
    usb_named: Option<(u16, u16)>,
    /// The running version's contract declares `handover`: it can hand its state to a successor and end.
    handover: bool,
    /// The key it was started with (`gs::handover::mode`): an offer must carry it, so nobody else can make it hand over.
    key: u32,
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
    /// The installed list is still to be read at boot: `fs` has not answered yet (`try_boot`).
    boot_pending: bool,
    /// When the boot read was first asked, in seconds of uptime, and whether the wait has been said.
    boot_since: i64,
    boot_said: bool,
    /// A built-in driver an installed package has just given its device back to, for the reconcile sweep to say
    /// why it starts it - not a death notice it missed (`returned`).
    returned: Option<&'static str>,
    /// The device each USB host has bound for a driver of its own, as it last reported: (vendor, product), by the
    /// host's number (`usbdev::HOST_*`). A PC has two such hosts, and one saying it has nothing says nothing of the other.
    usb_attached: [Option<(u16, u16)>; usbdev::HOSTS],
}

/// How long the boot read waits for `fs` to answer before it says it gave up. `fs` mounts, and replays its
/// journal, after the supervisor is ready; on a USB stick that is seconds, and a read asked before it is
/// done is not answered in time. Not answered is not a "no": the list is asked again until `fs` says what
/// is there, for this long at most.
const BOOT_WAIT_SECS: i64 = 120;

impl Installed {
    pub fn new() -> Installed {
        Installed { e: [None; MAX_INSTALLED], image: None, boot_pending: false, boot_since: 0, boot_said: false, returned: None, usb_attached: [None; usbdev::HOSTS] }
    }

    /// Is the installed list still to be read (`fs` has not answered yet)? The supervisor's loop then waits
    /// with a bound rather than forever, and asks again (`try_boot`).
    pub fn boot_pending(&self) -> bool { self.boot_pending }

    fn find(&self, name: &str) -> Option<usize> {
        self.e.iter().position(|x| x.is_some_and(|x| x.name() == name))
    }

    /// Is `task` an installed package's? Its death is this module's to answer.
    pub fn has(&self, task: &str) -> bool { self.find(task).is_some() }

    /// Record whether the version of `pkg` now running can hand over, and the key it was started with.
    fn note_handover(&mut self, pkg: &str, (can, key): (bool, u32)) {
        if let Some(i) = self.find(pkg) {
            if let Some(mut e) = self.e[i] { e.handover = can; e.key = key; self.e[i] = Some(e); }
        }
    }

    /// Replace the running `pkg` with `version` LIVE: start it beside the running one as its successor and tell the
    /// running one to hand over. True when that happened; false when it could not - nothing running, or the running
    /// version cannot hand over - and the caller restarts instead. An error is a successor that would not start.
    pub fn hand_over(&mut self, ctx: &ServiceContext, pkg: &str, version: &str) -> Result<bool, Text> {
        let Some((publisher, name)) = split(pkg) else { return Err(text(format_args!("`{}` is not a package name", pkg))) };
        // Every way out to a restart says why: a fallback that is not said is the silent kind (26.7).
        let cold = |why: &str| {
            ctx.log_fmt(format_args!("supervisor: installed {} -> {}: not live - {}; stopped and started instead", pkg, version, why));
            Ok(false)
        };
        let Some(e) = self.find(pkg).and_then(|i| self.e[i]) else { return cold("it is not installed here") };
        let i = self.find(pkg).unwrap_or(0);
        if e.utility { return cold("it is a utility or an app, which is not replaced live"); }
        if !e.handover { return cold("the running version cannot hand over (no `handover` in its contract, or no key)"); }
        if !alive(ctx, pkg) { return cold("it is not running"); }
        // The running one's endpoint, by its name, BEFORE the successor takes the name.
        let Ok(old) = gs::cap::acquire(ctx, pkg) else { return cold("its endpoint could not be found by its name") };
        let started = self.run_as(ctx, publisher, name, pkg, version, None, true);
        let (_, successor, can) = match started {
            Ok(x) => x,
            Err(why) => { gs::cap::remove(ctx, old); return Err(why); }
        };
        // The new version cannot take over: nothing was started, and the caller stops and starts instead.
        if !can.0 { gs::cap::remove(ctx, old); return Ok(false); }
        let Some(succ) = successor else {
            gs::cap::remove(ctx, old);
            return Err(text(format_args!("the successor started with no endpoint to hand to")));
        };
        // The offer, with the successor's endpoint attached: the running version's key, which it checks, and the
        // successor's, which the state must carry.
        let mut offer = [0u8; 16];
        offer[..8].copy_from_slice(godspeed::handover::OFFER);
        offer[8..12].copy_from_slice(&e.key.to_le_bytes());
        offer[12..16].copy_from_slice(&can.1.to_le_bytes());
        let sent = gs::ipc::send_granting(ctx, old, succ.into(), &gs::ipc::Message::from_bytes(&offer)).is_ok();
        if !sent { gs::cap::remove(ctx, succ.into()); }
        gs::cap::remove(ctx, old);
        let restarts = e.restarts;
        let slot = i;
        self.remember(slot, pkg, version, restarts, (false, false));
        self.note_handover(pkg, can);
        ctx.log_fmt(format_args!("supervisor: installed {} {} -> {}: handover {}", pkg, e.version(), version,
            if sent { "offered - the running version hands its state over and ends" } else { "could NOT be offered - the old version keeps running beside it" }));
        Ok(true)
    }

    /// Is `driver`, a built-in driver, stood down because an installed package holds its device? The death notice,
    /// the reconcile sweep and the startup convergence all ask, so none of them starts it beside the package.
    pub fn holds(&self, driver: &str) -> bool {
        self.e.iter().flatten().any(|x| x.pci_class.is_some_and(|c| crate::in_tree_driver_of(c) == Some(driver))
            || x.usb.is_some_and(|d| self.in_tree_usb_driver(d) == Some(driver)))
    }

    /// The host that has `dev` bound, as it last reported - `None` while none has it.
    fn host_of(&self, dev: (u16, u16)) -> Option<u8> {
        self.usb_attached.iter().position(|&a| a == Some(dev)).map(|h| h as u8)
    }

    /// The built-in driver of USB device `dev` where it is bound: one wired to the host that has it, or - while no
    /// host has said, as after a supervisor restart - the one that names it on any host.
    fn in_tree_usb_driver(&self, dev: (u16, u16)) -> Option<&'static str> {
        crate::in_tree_usb_driver_of(dev.0, dev.1, self.host_of(dev))
    }

    /// `pkg` no longer drives its device: its class or USB device is forgotten, so a built-in driver it took the device
    /// from is wanted again, and said.
    fn give_back(&mut self, ctx: &ServiceContext, pkg: &str) {
        let Some(i) = self.find(pkg) else { return };
        let Some(mut e) = self.e[i] else { return };
        let class = e.pci_class.take();
        let usb = e.usb.take();
        self.e[i] = Some(e);
        if let Some(holder) = class.and_then(crate::in_tree_driver_of) {
            ctx.log_fmt(format_args!("supervisor: {}'s device (class {:06x}) goes back to {}", pkg, class.unwrap_or(0), holder));
            self.returned = Some(holder);
        }
        if let Some(((v, p), holder)) = usb.and_then(|d| self.in_tree_usb_driver(d).map(|h| (d, h))) {
            ctx.log_fmt(format_args!("supervisor: {}'s USB device {:04x}:{:04x} goes back to {}", pkg, v, p, holder));
            self.returned = Some(holder);
        }
    }

    /// Record the USB device `pkg` holds - and, when it holds one, that its contract names it.
    fn usb_of(&mut self, pkg: &str, usb: Option<(u16, u16)>) {
        if let Some(i) = self.find(pkg) {
            if let Some(mut e) = self.e[i] {
                e.usb = usb;
                if usb.is_some() { e.usb_named = usb; }
                self.e[i] = Some(e);
            }
        }
    }

    /// USB host `host` reported what it has bound for a driver of its own (`usbdev`). True when an installed package
    /// drives that device - it is started now if it is not running, and the caller starts no built-in driver for it.
    /// The device this host had before and has no more - unplugged, or replaced by another - has its installed driver
    /// stopped, and kept installed, to be started again when its device returns: on THIS host's word only, so a
    /// report from one host never stops the driver of a device bound on the other.
    pub fn usb_report(&mut self, ctx: &ServiceContext, host: u8, present: bool, vid: u16, pid: u16) -> bool {
        let Some(slot) = self.usb_attached.get_mut(host as usize) else { return false };
        let before = *slot;
        let now = present.then_some((vid, pid));
        *slot = now;
        if let Some(gone) = before.filter(|&d| Some(d) != now && self.host_of(d).is_none()) {
            for x in self.e.iter().flatten() {
                if x.usb_named == Some(gone) && alive(ctx, x.name()) {
                    let _ = ctx.kill(x.name());
                    ctx.log_fmt(format_args!("supervisor: installed {} stopped - its USB device is not attached", x.name()));
                }
            }
        }
        if !present {
            return false;
        }
        let Some(i) = self.e.iter().position(|x| x.is_some_and(|x| x.usb_named == Some((vid, pid)))) else { return false };
        let Some(mut e) = self.e[i] else { return false };
        if alive(ctx, e.name()) { return true; }
        // Its device attached again - a replug, or a host that bound it afresh: a new start, with a fresh allowance of
        // restarts, whether it had ended itself or been left down before.
        e.restarts = 0;
        self.e[i] = Some(e);
        let (pkg, version) = (e.name(), e.version());
        let Some((publisher, name)) = split(pkg) else { return true };
        let mut p = [0u8; NAME_MAX];
        p[..pkg.len()].copy_from_slice(pkg.as_bytes());
        let pkg = core::str::from_utf8(&p[..pkg.len()]).unwrap_or("");
        let mut v = [0u8; 24];
        v[..version.len()].copy_from_slice(version.as_bytes());
        let version = core::str::from_utf8(&v[..version.len()]).unwrap_or("");
        if let Err(why) = self.run(ctx, publisher, name, pkg, version, None) {
            ctx.log_fmt(format_args!("supervisor: installed {} could not be started for its USB device {:04x}:{:04x}: {}", pkg, vid, pid, why.as_str()));
        }
        true
    }

    /// Was `driver` given its device back since the sweep last asked? Asked once: the answer is taken.
    pub fn returned(&mut self, driver: &str) -> bool {
        if self.returned == Some(driver) { self.returned = None; true } else { false }
    }

    /// Record the device class `pkg` was started with.
    fn class_of(&mut self, pkg: &str, class: Option<u32>) {
        if let Some(i) = self.find(pkg) {
            if let Some(mut e) = self.e[i] { e.pci_class = class; self.e[i] = Some(e); }
        }
    }

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
    /// Check `version` of `pkg` from the disk exactly as a start would, and start nothing. Run BEFORE a cold
    /// replacement stops the running version, so a package that would not start leaves the running one alone
    /// rather than costing a stop, a failed start and a restore. The start checks again: nothing is trusted from
    /// this check but the decision to stop.
    pub fn verify(&mut self, ctx: &ServiceContext, pkg: &str, version: &str) -> Result<(), Text> {
        let Some((publisher, name)) = split(pkg) else { return Err(text(format_args!("`{}` is not a package name", pkg))) };
        if !version_ok(version) { return Err(text(format_args!("`{}` is not a version", version))); }
        let arch = this_arch().ok_or_else(|| text(format_args!("this machine's architecture has no package spelling")))?;
        if self.image.is_none() {
            self.image = Some(Region::alloc(ctx, IMAGE_MAX)
                .map_err(|e| text(format_args!("no memory to hold an image ({:?})", e)))?);
        }
        let Some(region) = self.image.as_mut() else { return Err(text(format_args!("no memory to hold an image"))) };
        let buf = region.bytes_mut();
        check::package(ctx, publisher, name, version, Some(arch), &mut |off, bytes| {
            let (o, end) = (off as usize, off as usize + bytes.len());
            if end > buf.len() { return false; }
            buf[o..end].copy_from_slice(bytes);
            true
        }).map(|_| ())
    }

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
            self.adopt_device(ctx, publisher, name, pkg, version);
            return Ok(());
        }
        // Listed before it starts, so a device it takes over is recorded against it from the first moment.
        let new = self.e[slot].is_none();
        if new { self.remember(slot, pkg, version, restarts, (false, false)); }
        let started = self.run_as(ctx, publisher, name, pkg, version, None, false).map(|(k, c, h)| ((k, c), h));
        let ((kind, _), handover) = match started {
            Ok(x) => x,
            Err(why) => { if new { self.e[slot] = None; } return Err(why); }
        };
        self.remember(slot, pkg, version, restarts, kind);
        self.note_handover(pkg, handover);
        Ok(())
    }

    /// A driver package found running by a respawned supervisor: its contract read from the disk again, so the device
    /// it holds is recorded and the built-in driver this supervisor's startup brought back is stood down once more.
    fn adopt_device(&mut self, ctx: &ServiceContext, publisher: &str, name: &str, pkg: &str, version: &str) {
        let Ok(c) = check::package(ctx, publisher, name, version, this_arch(), &mut |_, _| true) else { return };
        if c.kind != Kind::Driver { return; }
        let holder = match (c.contract.pci_class, c.contract.usb) {
            (Some(class), _) => { self.class_of(pkg, Some(class)); crate::in_tree_driver_of(class) }
            (None, Some((v, p))) => { self.usb_of(pkg, Some((v, p))); self.in_tree_usb_driver((v, p)) }
            (None, None) => return,
        };
        if let Some(holder) = holder {
            if alive(ctx, holder) {
                let _ = ctx.kill(holder);
                ctx.log_fmt(format_args!("supervisor: {} stood down again - the installed {} still drives its device", holder, pkg));
            }
        }
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
        let pci_class = self.e[slot].and_then(|x| x.pci_class);
        let usb = self.e[slot].and_then(|x| x.usb);
        let usb_named = self.e[slot].and_then(|x| x.usb_named);
        let handover = self.e[slot].is_some_and(|x| x.handover);
        let key = self.e[slot].map_or(0, |x| x.key);
        let mut e = Entry { name: [0; NAME_MAX], nl: pkg.len(), version: [0; 24], vl: version.len(), restarts, utility, app, pci_class,
                            usb, usb_named, handover, key };
        e.name[..pkg.len()].copy_from_slice(pkg.as_bytes());
        e.version[..version.len()].copy_from_slice(version.as_bytes());
        self.e[slot] = Some(e);
    }

    /// Check the fetched copy, read its grant, and spawn it. Whether it is a utility, and an app. A utility (and an
    /// app) is spawned only when `first` is given - the shell asking to run it, with its own peers first - and
    /// otherwise only checked.
    fn run(&mut self, ctx: &ServiceContext, publisher: &str, name: &str, pkg: &str, version: &str,
           first: Option<&[&str]>) -> Result<(bool, bool), Text> {
        // Every start records its handover key, a restart's included: an offer to it must carry the key it runs with.
        let (kind, _, handover) = self.run_as(ctx, publisher, name, pkg, version, first, false)?;
        self.note_handover(pkg, handover);
        Ok(kind)
    }

    /// [`run`](Installed::run), and when `successor` is set, started BESIDE the running version as its successor
    /// (`SPAWN_FLAG_HANDOVER`, the handover spawn mode). The new endpoint's capability comes back for the offer.
    #[allow(clippy::too_many_arguments)]
    fn run_as(&mut self, ctx: &ServiceContext, publisher: &str, name: &str, pkg: &str, version: &str,
              first: Option<&[&str]>, successor: bool)
        -> Result<((bool, bool), Option<godspeed_sdk::capability::CapHandle>, (bool, u32)), Text> {
        // A package runs under its name and is reached by it, so a name longer than the kernel's directory
        // holds would start and then be unreachable by its own clients. Refused, never truncated.
        if pkg.len() > godspeed_sdk::service_context::NAME_MAX {
            return Err(text(format_args!("its name is {} bytes, and a package name is at most {} - the longest this machine reaches a program by",
                pkg.len(), godspeed_sdk::service_context::NAME_MAX)));
        }
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
        let driver = checked.kind == Kind::Driver;
        let utility = checked.kind == Kind::Utility || app;
        if utility && first.is_none() {
            ctx.log_fmt(format_args!("supervisor: installed {} {} checked from disk - {}, run when typed", pkg, version,
                if app { "an app" } else { "a utility" }));
            return Ok(((utility, app), None, (false, 0)));
        }
        // The console: for an app, and only when the operator's answer gave it. The device, likewise, for a driver.
        let mut console = false;
        let mut device = false;
        let mut memory = false;
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
            if p == DEVICE_GRANT {
                if !driver { return Err(text(format_args!("its grant names a device, which only a driver is given"))); }
                device = true;
                continue;
            }
            if p == MEMORY_GRANT {
                if !driver { return Err(text(format_args!("its grant names DMA memory, which only a driver is given"))); }
                memory = true;
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
        // A built-in driver this package takes a device over from: stood down below, just before the spawn.
        let mut stood_down: Option<&'static str> = None;
        // A USB driver's host, which it reaches its device through: its peer, beside the ones its grant names.
        let mut usb_host: Option<&'static str> = None;
        if driver && checked.contract.usb.is_some() {
            // ONE USB DEVICE, BY VENDOR AND PRODUCT. A USB device has no registers of its own: the USB host it is
            // plugged into binds it, reports it, and serves its transfers to the one driver given that host as a peer
            // (`gs::driver::bus`). So it is started only while the host reports it attached, and no interrupt or DMA
            // arena is its to have - those are the host's.
            let Some((vid, pid)) = checked.contract.usb else { return Err(text(format_args!("no USB device"))) };
            if checked.contract.pci_class.is_some() {
                return Err(text(format_args!("its contract names both a PCI class and a USB device - a driver drives one device")));
            }
            if !device { return Err(text(format_args!("it is a driver, and its grant does not give it its device"))); }
            if checked.contract.pci_irq || checked.contract.dma_bytes > 0 {
                return Err(text(format_args!("it drives a USB device and asks for an interrupt or DMA memory, which are its host's - it reaches its device through the host")));
            }
            if let Some(other) = self.e.iter().flatten().find(|x| x.name() != pkg && x.usb_named == Some((vid, pid))) {
                return Err(text(format_args!("its USB device {:04x}:{:04x} is driven already by the installed {}", vid, pid, other.name())));
            }
            if crate::usb_host().is_none() {
                return Err(text(format_args!("this machine has no USB host that binds a device for a driver of its own")));
            }
            self.usb_of(pkg, Some((vid, pid)));
            // Its peer is the host that HAS its device - on a PC, `xhci` or `ehci` - not one named in advance.
            let Some(h) = self.host_of((vid, pid)) else {
                ctx.log_fmt(format_args!("supervisor: installed {} {} checked from disk - it starts when a USB host reports its USB device {:04x}:{:04x}",
                    pkg, version, vid, pid));
                return Ok(((utility, app), None, (false, 0)));
            };
            let host = usbdev::host_name(h).unwrap_or("xhci");
            if np < peers.len() && !peers[..np].contains(&host) { peers[np] = host; np += 1; }
            // Its host's answers and notices arrive as messages, so it needs an endpoint of its own to receive them.
            req.flags |= SPAWN_FLAG_REQ_RECV;
            usb_host = Some(host);
            stood_down = crate::in_tree_usb_driver_of(vid, pid, Some(h));
        } else if driver {
            // ONE DEVICE, BY CLASS. The class is the contract's, signed; the operator approved it ("device"); the
            // kernel finds which device of that class this is from its own bus scan (a BDF of 0), exactly as for an
            // in-tree driver named by class (docs/developer-services-design.md 13.1). An interrupt and a DMA arena
            // are given when the contract asks, the arena only when the operator answered its own question.
            let Some(class) = checked.contract.pci_class else {
                return Err(text(format_args!("it is a driver, and its contract names no device (hw_pci_class)")));
            };
            if !device { return Err(text(format_args!("it is a driver, and its grant does not give it its device"))); }
            if let Some(other) = self.e.iter().flatten().find(|x| x.name() != pkg && x.pci_class == Some(class)) {
                return Err(text(format_args!("its device class {:06x} is driven already by the installed {}", class, other.name())));
            }
            let dma = checked.contract.dma_bytes;
            if dma > DMA_MAX {
                return Err(text(format_args!("its contract asks for {} KiB of DMA memory, and a package is given at most {} KiB",
                    dma / 1024, DMA_MAX / 1024)));
            }
            if dma > 0 && !memory {
                return Err(text(format_args!("its contract asks for DMA memory, and its grant does not give it - its device could not reach memory")));
            }
            // Confined where an IOMMU can confine it: the device then reaches its arena and nothing else (6.4).
            req.hw_flags = if checked.contract.pci_irq {
                godspeed_sdk::service_context::hwclass::pci_irq(class, checked.contract.pci_bar, true)
            } else {
                godspeed_sdk::service_context::hwclass::pci(class, checked.contract.pci_bar, true)
            };
            // An interrupt is delivered as a message, so it needs an endpoint: the kernel routes the vector to the task's
            // endpoint at spawn, and to nothing when it has none (`irq: ... routed: NO - discarded`).
            if checked.contract.pci_irq { req.flags |= SPAWN_FLAG_REQ_RECV; }
            req.bdf = 0;
            req.dma_pages = dma.div_ceil(4096) as u32;
            stood_down = crate::in_tree_driver_of(class);
        }
        req.memory_limit = match checked.contract.memory_limit { 0 => MEMORY_DEFAULT, m => m.min(MEMORY_MAX) };
        // A handover-capable start carries a fresh KEY in its spawn mode, which only this supervisor can set: an offer
        // must carry it, so no client of the service can make it hand its state away (`gs::handover`).
        // No hardware random source, no key, and so no handover for this start: it is recorded as unable, and an upgrade
        // stops and starts it instead. A clock-derived key would be guessable (the commit's security review).
        let key = if checked.contract.handover { handover_key(ctx) } else { None };
        if checked.contract.handover && key.is_none() {
            ctx.log_fmt(format_args!("supervisor: installed {} {}: no hardware random source for a handover key - it will be stopped and started on an upgrade, not handed over", pkg, version));
        }
        let can_hand_over = key.is_some();
        let key = key.unwrap_or(0);
        if can_hand_over { req.probe_mode = godspeed::handover::mode(key, successor); }
        if successor {
            // A version that cannot take over - its contract does not declare `handover`, or there is no key - is NOT
            // started here: the caller stops the running one and starts this one instead, and says so.
            if !can_hand_over {
                ctx.log_fmt(format_args!("supervisor: installed {} {} cannot take over live ({}) - stopped and started instead", pkg, version,
                    if checked.contract.handover { "no hardware random source for a key" } else { "its contract does not declare `handover`" }));
                return Ok(((utility, app), None, (false, 0)));
            }
            req.flags |= SPAWN_FLAG_HANDOVER | SPAWN_FLAG_REQ_RECV;
        }
        req.privileges = 0;
        if let Some(holder) = stood_down {
            // TAKEN OVER, never shared: the built-in driver is stopped and kept stopped while this package holds its
            // device. The device is recorded FIRST, so the death notice and the reconcile sweep both find it held and
            // leave its driver down; it is given back when the package is uninstalled, ends, or is left down (`holds`).
            if usb_host.is_none() { self.class_of(pkg, checked.contract.pci_class); }
            if alive(ctx, holder) {
                let _ = ctx.kill(holder);
                let budget = godspeed::driver::wait::Budget::ms(STAND_DOWN_MS);
                if godspeed::driver::wait::until(ctx, budget, || !alive(ctx, holder)).is_err() {
                    if usb_host.is_none() { self.class_of(pkg, None); } else { self.usb_of(pkg, None); }
                    return Err(text(format_args!("`{}` drives its device and did not stop within {} ms", holder, STAND_DOWN_MS)));
                }
                ctx.log_fmt(format_args!("supervisor: {} stood down - the installed {} drives its device until it is uninstalled",
                    holder, pkg));
            }
        }
        let mut out = [0u8; 128];
        match ctx.spawn_image(&mut req, &mut out, &peers[..np]) {
            Ok(cap) => {
                // Nobody asked for a capability to it here; clients reach it by name, as every service. A successor's
                // goes back to the caller, which hands it to the running version with the offer.
                let kept = if successor { cap } else { if let Some(c) = cap { gs::cap::remove(ctx, c.into()); } None };
                let gave = if !driver { "" }
                    else if usb_host.is_some() { ", its USB device through its host" }
                    else if req.dma_pages > 0 && checked.contract.pci_irq { ", its device with its interrupt and DMA memory" }
                    else if req.dma_pages > 0 { ", its device with DMA memory" }
                    else if checked.contract.pci_irq { ", its device with its interrupt" }
                    else { ", its device" };
                ctx.log_fmt(format_args!("supervisor: installed {} {} started ({} bytes, {} peer(s){}, checked from disk)",
                    pkg, version, checked.size, np, gave));
                if driver && usb_host.is_none() { self.class_of(pkg, checked.contract.pci_class); }
                Ok(((utility, app), kept, (can_hand_over, key)))
            }
            Err(e) => {
                // Not started: a device taken over goes back to its built-in driver, which the reconcile sweep restarts.
                if let Some(holder) = stood_down {
                    if usb_host.is_none() { self.class_of(pkg, None); } else { self.usb_of(pkg, None); }
                    ctx.log_fmt(format_args!("supervisor: installed {} did not start - its device goes back to {}", pkg, holder));
                }
                Err(text(format_args!("the kernel refused to start it: {:?}", e)))
            }
        }
    }

    /// Stop `pkg` and forget it. False when it was not installed here.
    pub fn stop(&mut self, ctx: &ServiceContext, pkg: &str) -> bool {
        let Some(i) = self.find(pkg) else { return false };
        self.give_back(ctx, pkg);
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
            self.give_back(ctx, task);
            return;
        }
        if e.usb_named.is_some_and(|d| self.host_of(d).is_none()) {
            // Stopped because its device left (`usb_report`), or died after it did: started again when it returns.
            ctx.log_fmt(format_args!("supervisor: installed {} ended{} - not restarted, its USB device is not attached", task, how));
            return;
        }
        if e.restarts >= RESTARTS_MAX {
            ctx.log_fmt(format_args!("supervisor: installed {} died{} - restarted {} times already, left down",
                task, how, RESTARTS_MAX));
            self.give_back(ctx, task);
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
        self.boot_pending = true;
        self.boot_since = gs::task::uptime_secs(ctx);
        self.try_boot(ctx);
    }

    /// Read the installed list and start what is on it, if it is still owed. An answer from `fs` - the list,
    /// or that there is none - settles it. NO answer does not: `fs` may still be mounting, so it is asked
    /// again on the next pass, said once, for `BOOT_WAIT_SECS` at most, and then given up on out loud.
    pub fn try_boot(&mut self, ctx: &ServiceContext) {
        if !self.boot_pending { return; }
        let waited = gs::task::uptime_secs(ctx) - self.boot_since;
        let mut list = [0u8; 1024];
        let n = match gs::fs::Fs::new(ctx).read_into(LIST, &mut list) {
            Ok(n) => n,
            Err(gs::Error::NotFound) => { self.boot_pending = false; return; }
            // `Malformed` only once a read has gone unanswered: it is that read's answer, arriving late and
            // taken as this one's (seen 2026-10-18, the boot right after a reboot) - stale, not a "no".
            Err(e @ (gs::Error::OutcomeUnknown | gs::Error::Unreachable | gs::Error::Busy | gs::Error::Unavailable))
                if waited < BOOT_WAIT_SECS => {
                if !self.boot_said {
                    ctx.log_fmt(format_args!("supervisor: {} - fs has not answered yet ({:?}); installed packages start when it does (waiting {} s at most)",
                        LIST, e, BOOT_WAIT_SECS));
                    self.boot_said = true;
                }
                return;
            }
            Err(gs::Error::Malformed) if self.boot_said && waited < BOOT_WAIT_SECS => return,
            Err(e) => {
                self.boot_pending = false;
                ctx.log_fmt(format_args!("supervisor: {} could not be read ({:?}) after {} s - no installed package started", LIST, e, waited));
                return;
            }
        };
        self.boot_pending = false;
        if self.boot_said {
            ctx.log_fmt(format_args!("supervisor: {} read after {} s - starting the installed packages", LIST, waited));
        }
        // IN DEPENDENCY ORDER (`docs/developer-services-design.md` 16.5): the list's order is the order things were
        // installed, and an upgrade moves a package to the end - after what needs it. So each pass starts every
        // package whose requirements are running, until a pass starts nothing; what is left waits on something
        // that is not running, and says which. Each pass starts at least one or ends the loop, so there are at most
        // as many passes as lines; and a cycle cannot be installed, so nothing is left waiting on one.
        let text = core::str::from_utf8(&list[..n]).unwrap_or("");
        let mut done = [false; BOOT_MAX];
        let entries = || text.lines().filter_map(|l| { let mut it = l.split_whitespace(); Some((it.next()?, it.next()?)) }).take(BOOT_MAX);
        loop {
            let mut progressed = false;
            for (i, (pkg, version)) in entries().enumerate() {
                if done[i] { continue; }
                if self.waiting_on(ctx, pkg, version).is_some() { continue; }
                done[i] = true;
                progressed = true;
                if let Err(why) = self.start(ctx, pkg, version) {
                    ctx.log_fmt(format_args!("supervisor: installed {} {} NOT started: {}", pkg, version, why.as_str()));
                }
            }
            if !progressed { break; }
        }
        for (i, (pkg, version)) in entries().enumerate() {
            if done[i] { continue; }
            if let Some(waiting) = self.waiting_on(ctx, pkg, version) {
                ctx.log_fmt(format_args!("supervisor: installed {} {} NOT started: it requires {}, which is not running", pkg, version, waiting.as_str()));
            }
        }
    }

    /// The first requirement of `pkg` `version` that is not running, read from its manifest, or `None` when all are -
    /// or when the manifest cannot be read, which `start` then refuses on its own, saying why.
    fn waiting_on(&self, ctx: &ServiceContext, pkg: &str, version: &str) -> Option<Text> {
        let mut path = Text::new();
        let _ = core::fmt::Write::write_fmt(&mut path, format_args!("{}{}/{}/manifest.toml", check::DIR, pkg, version));
        let mut buf = [0u8; check::MANIFEST_MAX];
        let n = gs::fs::Fs::new(ctx).read_into(path.as_str(), &mut buf).ok()?;
        let text_ = core::str::from_utf8(&buf[..n]).ok()?;
        let mut arts = crate::meta::Artifacts::new();
        let m = crate::meta::manifest(text_, &mut arts).ok()?.manifest;
        for r in m.requires {
            let mut name = Text::new();
            let _ = core::fmt::Write::write_fmt(&mut name, format_args!("{}", r.name()));
            if !alive(ctx, name.as_str()) { return Some(name); }
        }
        None
    }
}

fn gs_ok() -> u8 { godspeed_sdk::service_context::supcmd::OK }

/// A fresh handover key, 30 bits from the hardware's random source - or `None` where there is none, which fails closed:
/// that start cannot hand over. Never derived from the clock, which an attacker can guess.
fn handover_key(ctx: &ServiceContext) -> Option<u32> {
    let r = gs::task::hw_random(ctx)?;
    Some((r & godspeed::handover::KEY_MASK).max(1))
}
