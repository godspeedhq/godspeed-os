// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
#![no_std]
#![no_main]
//! `power` - the machine's power policy, as a service rather than a kernel decision.
//!
//! **Why this exists.** The Pi firmware runs the Arm cores at turbo for the first minute after boot and
//! then at their minimum for good, because no OS asks it for a rate. That made the WiFi chip's firmware
//! upload 2.3x slower after the first minute, and a firmware loaded that slowly traps at start
//! (`docs/wifi.md` 55). Holding the cores at turbo forever fixes it and wastes power the rest of the
//! time. The answer is to be fast exactly when something needs it.
//!
//! **What the kernel does and what this does.** The kernel can set the clock to the firmware's minimum
//! or maximum (`CpuClock`, syscall 55) and knows nothing else. This service owns the POLICY (26.10): who
//! may ask, for how long, and when to go back. The rule is a LEASE - see `docs/power.md`:
//!
//! - `OP_HOLD [1, secs]` opens a lease of `secs` seconds (0 means `DEFAULT_SECS`, capped at `MAX_SECS`)
//!   and answers `[status, lease, hz:u32]` - `hz` is the rate this lease set, and 0 when another lease
//!   already held the clock at its maximum. While any lease is open the clock is at its maximum.
//! - `OP_RELEASE [2, lease]` closes it early and answers `[status]`. When none is open, the minimum.
//! - A lease nobody releases EXPIRES. A holder that dies or hangs mid-way therefore cannot pin the
//!   machine at full power: it lasts `MAX_SECS` at most, and the expiry is said (26.7). No death
//!   notification is needed for that, which is why there is none.
//!
//! **The governor (2026-10-21).** Leases are for what knows it needs speed; the governor is for load,
//! and it is Raspberry Pi OS's: the kernel's `ondemand` governor (`drivers/cpufreq/cpufreq_ondemand.c`,
//! `od_update`) with the tunables `raspi-config`'s start-up script writes (`debian/raspi-config.init`).
//! Every `SAMPLE_MS` the busiest core's share of busy ticks is read; over `UP_THRESHOLD` the clock goes
//! to its maximum and is not looked at again for `SAMPLING_DOWN_FACTOR` samples. At or under it,
//! `ondemand` picks the step nearest `min + load * (max - min) / 100`; the kernel offers only the two
//! ends, and for a load at or under half the nearer one is the minimum, so the rule is the same. The
//! clock is at its maximum while a lease is open OR the governor says so.
//!
//! **Shutdown (`docs/shutdown.md`).** This service holds no `POWER_OFF`; the shell does. It READS the one pending
//! power-off or reboot the kernel holds (InspectKernel query 28, `power_pending`), warns at 1 h, 30 m, 15 m, 5 m, 1 m and
//! each of the last ten seconds, and three seconds out (`TIDY_AT`) cancels a copy in flight and has `fs` quiesce. The
//! kernel fires the deadline whether or not this service is alive. `OP_SHUTDOWN_CHANGED` and `OP_COPIER_CHANGED` are
//! cues with no reply and no authority.
//!
//! **Restart.** This service is restartable like any other. A respawned instance knows of no lease, so
//! it puts the clock at its minimum - the honest state for "nobody has asked" - and a holder whose lease
//! was lost simply runs slower until it asks again. Bounded, loud, and never stuck fast.

// The stdlib for everything it covers - receiving, the reply capability, answering on it - and the
// SDK only for what the stdlib does not wrap: `cpu_clock` (a privileged syscall, this service's alone) and
// `power_pending` (query 28).
use godspeed as gs;
use godspeed_sdk::{Message, ServiceContext};

/// The protocol, one opcode byte. Replies to `OP_HOLD` and `OP_RELEASE` lead with a status byte; the two cues are
/// not answered.
const OP_HOLD: u8 = 1;
const OP_RELEASE: u8 = 2;
/// `[3, copier]`, no reply: the shell set or cancelled the machine's pending power-off. A CUE, not a request - it
/// carries no authority and is never trusted: this service only re-reads the deadline the kernel holds, so a
/// forged cue from anyone changes nothing but how soon that read happens (`docs/shutdown.md` 6). `copier` is
/// as in `OP_COPIER_CHANGED`.
const OP_SHUTDOWN_CHANGED: u8 = 3;
/// `[4, copier]`, no reply: the shell started `copier` (1) or ended it (0) while a shutdown is pending. It says
/// whether the tidy-up has a copy to cancel, because `copier` runs only while a job does and asking a name
/// nobody holds is a refusal the kernel logs (`acquire: 'copier' is not in the name directory`). A forged one
/// costs at most that line, or a copy left for recovery: the same as a lost one.
const OP_COPIER_CHANGED: u8 = 4;
/// `copier`'s cancel (`services/copier`): stop a copy at its next chunk and remove the partial destination.
const CP_OP_CANCEL: u8 = 3;

const ST_OK: u8 = 0;
/// Every lease slot is taken. The holder runs at whatever the clock is; nothing is pinned.
const ST_FULL: u8 = 1;
/// This machine gives the OS no control over its clock. The lease is not recorded, because it could
/// change nothing; the holder proceeds exactly as it would have.
const ST_NO_CONTROL: u8 = 2;
/// `OP_RELEASE` named a lease that is not open - already expired, already released, or from before a
/// restart of this service. Not an error to the holder: the lease is gone either way.
const ST_UNKNOWN: u8 = 3;
const ST_BAD_REQUEST: u8 = 4;

/// A lease asked for with no length.
const DEFAULT_SECS: u64 = 15;
/// The longest any lease may be, whatever was asked. The bound that makes a dead holder harmless.
const MAX_SECS: u64 = 30;
/// Open leases at once. A handful of drivers loading firmware at the same moment is the realistic most.
const SLOTS: usize = 8;

/// The governor's tunables, `raspi-config`'s (`debian/raspi-config.init`): `CPU_ONDEMAND_SAMPLING_RATE`
/// 100000 us, `CPU_ONDEMAND_UP_THRESHOLD` 50, `CPU_ONDEMAND_DOWN_SAMPLING_FACTOR` 50. So a busy machine is
/// fast within 100 ms and stays so for at least 5 s before it is looked at again.
const SAMPLE_MS: u64 = 100;
const UP_THRESHOLD: u64 = 50;
const SAMPLING_DOWN_FACTOR: u64 = 50;
/// Cores the governor reads. A core past this is not looked at, which on every board here is none.
const GOV_CORES: usize = 8;

/// The governor's state: whether load has the clock at its maximum, when it looks next, and each core's
/// busy and total ticks at the last look (InspectKernel queries 6 and 7, the counters `observe` shows).
struct Governor {
    fast: bool,
    next: u64,
    primed: bool,
    off: bool,
    active: [u64; GOV_CORES],
    total: [u64; GOV_CORES],
}

impl Governor {
    /// Look at the load: whether the busiest core is over `UP_THRESHOLD`, its percentage and which core it
    /// is. `None` when the counters cannot be read, which turns the governor off for good, said once.
    fn sample(&mut self, ctx: &ServiceContext) -> Option<(bool, u64, usize)> {
        let n = (ctx.inspect_core_count() as usize).min(GOV_CORES);
        let (mut busiest, mut which) = (0u64, 0usize);
        for c in 0..n {
            let (Some(a), Some(t)) = (ctx.inspect_core_active_ticks(c as u32), ctx.inspect_core_total_ticks(c as u32)) else {
                self.off = true;
                ctx.log("power: the per-core load cannot be read (no INTROSPECT) - the governor is off, leases still work");
                return None;
            };
            let (da, dt) = (a.saturating_sub(self.active[c]), t.saturating_sub(self.total[c]));
            self.active[c] = a;
            self.total[c] = t;
            if dt > 0 {
                let load = (da.min(dt) * 100) / dt;
                if load > busiest {
                    busiest = load;
                    which = c;
                }
            }
        }
        // The first look has nothing to compare with: it only records where the counters stand.
        if !self.primed {
            self.primed = true;
            return Some((false, 0, 0));
        }
        Some((busiest > UP_THRESHOLD, busiest, which))
    }
}

#[derive(Clone, Copy)]
struct Lease {
    id: u8,
    /// The cycle-counter value at which it expires.
    until: u64,
    secs: u64,
}

struct Leases {
    slots: [Option<Lease>; SLOTS],
    next_id: u8,
}

impl Leases {
    fn open(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    fn take_id(&mut self) -> u8 {
        // 1..=255, skipping any id still open, so a stale RELEASE cannot close somebody else's lease.
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id == 0 {
                continue;
            }
            let id = self.next_id;
            if !self.slots.iter().flatten().any(|l| l.id == id) {
                return id;
            }
        }
    }
}

/// Set the clock for `open` leases and say so. `fast` is only ever the answer to "is any lease open".
fn apply(ctx: &ServiceContext, fast: bool, why: &str, no_control_said: &mut bool) -> Option<u32> {
    match ctx.cpu_clock(fast) {
        Some(hz) => {
            ctx.log_fmt(format_args!("power: Arm clock -> {} ({} MHz) - {}",
                if fast { "maximum" } else { "minimum" }, hz / 1_000_000, why));
            Some(hz)
        }
        None => {
            // ONCE, not per lease: on a machine with no clock control every request would otherwise log.
            if !*no_control_said {
                *no_control_said = true;
                ctx.log("power: this machine gives the OS no control over its clock - leases change nothing here");
            }
            None
        }
    }
}

/// Counter ticks in `ms` milliseconds - the unit a lease's expiry is kept in, so a lease compares with
/// one counter read. Never zero, and 1 on a machine whose counter the kernel could not calibrate,
/// where no tick count is a duration.
fn cycles(ctx: &ServiceContext, ms: u64) -> u64 {
    let per_10ms = gs::driver::wait::ticks_per_10ms(ctx);
    if per_10ms == 0 { 1 } else { (per_10ms.saturating_mul(ms) / 10).max(1) }
}

/// Answer on the client's one-shot reply capability, then give the slot back - a reply cap that is
/// answered and kept is a slot leaked per request (CLAUDE.md 8.5). `try_send`, because a client that
/// stopped waiting must not stall this service; a failed answer is that client's timeout, not ours.
fn reply(ctx: &ServiceContext, cap: gs::cap::Cap, body: &[u8]) {
    let _ = gs::ipc::reply(ctx, cap, &Message::from_bytes(body));
}

/// The machine's pending shutdown or reboot, as this service follows it (`docs/shutdown.md` 4). Both ride the one
/// deadline, and the warnings and the tidy-up are the same for either; only the words differ. THE KERNEL HOLDS THE
/// DEADLINE, and fires it whether or not this service is alive: a polite power cut becomes a plain one, never a
/// lost one. What this adds is the warnings and the tidy-up in the last seconds. It holds no authority, keeps no
/// clock of its own for the shutdown, and reads the deadline rather than trusting anyone's word for it.
///
/// WHEN IS A DEADLINE NEW? Only on a cue from the shell, which sends one for every set and cancel, or when this
/// service starts. A countdown's own progress is never read as a new deadline: an earlier version compared the
/// seconds left with the last read, and every scheduled wake on a long countdown announced the shutdown again.
struct Shutdown {
    /// The seconds left when last read; `None` when none is pending.
    left: Option<u64>,
    /// Whether the pending action, when last read, is a reboot rather than a power-off.
    reboot: bool,
    /// The smallest threshold already said or already behind, so each is said once and none is said late.
    warned: u64,
    /// Whether the tidy-up has run for this deadline (and so whether a cancel must resume `fs`).
    tidied: bool,
    /// Whether `copier` is running, as the shell last said. TRUE until it says: a respawned instance cannot know,
    /// and asking for a `copier` that is not there costs one logged refusal, while not asking for one that is
    /// leaves its partial file to recovery.
    copier: bool,
}

/// Warn at these many seconds left: an hour, half, a quarter, five minutes, one, then each of the last ten.
const WARN_AT: [u64; 15] = [3600, 1800, 900, 300, 60, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
/// When the tidy-up starts. Its two steps take up to a second each (`copier`, then `fs`), and the kernel's seconds
/// are rounded up, so three out leaves them two to three seconds - enough for both.
const TIDY_AT: u64 = 3;
/// The longest this service sleeps while a shutdown is pending, so a deadline brought earlier without a cue (a
/// cue lost to a full queue) is still seen within a minute - and before the tidy-up, for any but the last minute.
const SHUTDOWN_RECHECK_SECS: u64 = 60;

fn say_left(secs: u64) -> (u64, &'static str) {
    if secs >= 3600 && secs % 3600 == 0 { (secs / 3600, "h") }
    else if secs >= 60 && secs % 60 == 0 { (secs / 60, "m") }
    else { (secs, "s") }
}

/// The smallest warning threshold at or above `left`: thresholds above a new deadline are behind it, and are not
/// said (a `shutdown 10m` must not open with "15m").
fn passed_threshold(left: u64) -> u64 {
    WARN_AT.iter().copied().filter(|&t| t >= left).last().unwrap_or(u64::MAX)
}

impl Shutdown {
    fn new() -> Self { Shutdown { left: None, reboot: false, warned: u64::MAX, tidied: false, copier: true } }

    /// The words for the pending action: what it is, and what it is doing.
    fn noun(&self) -> &'static str { if self.reboot { "reboot" } else { "shutdown" } }
    fn doing(&self) -> &'static str { if self.reboot { "rebooting" } else { "shutting down" } }

    /// A cue, or this service's start: the deadline may be new, moved, or gone. Re-read it and say which.
    fn changed(&mut self, ctx: &ServiceContext) {
        let had = self.left;
        let was_tidied = self.tidied;
        let had_noun = self.noun();
        let now = ctx.power_pending();
        self.left = now.map(|p| p.secs);
        match now {
            Some(p) => {
                self.reboot = p.reboot;
                self.warned = passed_threshold(p.secs);
                self.tidied = false;
                let (n, u) = say_left(p.secs);
                let verb = if p.reboot { "reboot" } else { "shut down" };
                ctx.log_fmt(format_args!("power: the machine will {} in {}{} - `{} cancel` stops it",
                                         verb, n, u, self.noun()));
            }
            None if had.is_some() => {
                ctx.log_fmt(format_args!("power: the pending {} was cancelled", had_noun));
                // A cancel after the tidy-up: `fs` is refusing writes for a shutdown that is not coming.
                if was_tidied {
                    match gs::fs::Fs::new(ctx).patience_secs(1).resume() {
                        Ok(()) => ctx.log("power: fs takes writes again"),
                        Err(e) => ctx.log_fmt(format_args!(
                            "power: fs did not answer the resume ({:?}); it takes writes again within ten seconds", e)),
                    }
                }
            }
            None => {}
        }
    }

    /// Whole seconds until this service next has something to do for the shutdown - the next warning, the tidy-up,
    /// or the recheck - in the KERNEL's seconds. `None` when none is pending.
    fn secs_to_next(&self) -> Option<u64> {
        let left = self.left?;
        let mut target = WARN_AT.iter().copied().find(|&t| t < left && t < self.warned).unwrap_or(0);
        if !self.tidied && TIDY_AT < left { target = target.max(TIDY_AT); }
        Some(left.saturating_sub(target).clamp(1, SHUTDOWN_RECHECK_SECS))
    }

    /// Between cues: follow the countdown, warn at a threshold reached, and tidy up three seconds out.
    fn tick(&mut self, ctx: &ServiceContext) {
        if self.left.is_none() { return; }
        let now = ctx.power_pending();
        // The other kind than last read: one was cancelled and another set, with the cue lost - a new deadline.
        if now.map_or(false, |p| p.reboot != self.reboot) {
            self.changed(ctx);
            return;
        }
        match now.map(|p| p.secs) {
            // Gone without a cue: the kernel fired it (it clears the deadline as it does), or a cancel whose cue was
            // lost - which `changed` says and acts on.
            None => {
                // The kernel fires when the last second runs out, so only a deadline last read at one second left can have fallen due.
                if self.left.map_or(false, |l| l > 1) { self.changed(ctx); } else { self.left = None; }
            }
            Some(left) => {
                self.left = Some(left);
                if left < self.warned {
                    let t = passed_threshold(left);
                    if t < self.warned {
                        self.warned = t;
                        let (n, u) = say_left(t);
                        ctx.log_fmt(format_args!("power: {} in {}{}", self.doing(), n, u));
                    }
                }
                if left <= TIDY_AT && !self.tidied {
                    self.tidied = true;
                    tidy_up(ctx, self.copier, self.noun());
                }
            }
        }
    }
}

/// The last seconds before the power goes (`docs/shutdown.md` 4.3): a copy in flight is cancelled, so its partial
/// destination is removed, and `fs` flushes and stops taking writes. Each is bounded by about a second and an
/// unanswered one is said and skipped; whatever is not done is left to recovery, which is what it is for.
fn tidy_up(ctx: &ServiceContext, copier: bool, noun: &str) {
    ctx.log_fmt(format_args!("power: tidy-up - the last seconds before the {}", noun));
    // `copier` FIRST, because a cancelled copy removes its partial destination through `fs`, which must still take
    // that write. It runs only while a job does, so it is asked only when the shell has said it is running
    // (`OP_COPIER_CHANGED`) and its name resolves.
    if copier {
        if let Ok(cap) = gs::cap::acquire(ctx, "copier") {
            gs::cap::remove(ctx, cap);
            match gs::call::request_within(ctx, "copier", &Message::from_bytes(&[CP_OP_CANCEL]), 1) {
                Ok(_) => ctx.log("power: tidy-up - copier asked to stop its job"),
                Err(e) => ctx.log_fmt(format_args!("power: tidy-up - copier did not answer ({:?}); its job is cut", e)),
            }
        }
    }
    match gs::fs::Fs::new(ctx).patience_secs(1).quiesce() {
        Ok(true) => ctx.log("power: tidy-up - fs flushed and stopped taking writes"),
        Ok(false) => ctx.log("power: tidy-up - fs stopped taking writes; the device did not confirm a flush"),
        Err(e) => ctx.log_fmt(format_args!("power: tidy-up - fs did not answer ({:?}); the journal covers it", e)),
    }
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    gs::trace::as_name(&ctx, "power");
    let mut leases = Leases { slots: [None; SLOTS], next_id: 0 };
    let mut no_control_said = false;
    // NOBODY HAS ASKED YET, so the clock goes to its minimum. On a first boot this ends the firmware's
    // minute of turbo early, which is the point: from here on the machine is fast when something says
    // it needs to be and not otherwise. On a respawn it is the reconcile - any lease the dead instance
    // held is gone, and leaving the clock where it was could leave it fast with nobody to bring it down.
    let control = apply(&ctx, false, "no lease is open (start-up)", &mut no_control_said).is_some();
    ctx.log("power: serving - leases of up to 30 s hold the Arm clock at its maximum (docs/power.md)");
    // A shutdown the kernel already holds - set before this instance started, or before the last one died -
    // is picked up here and said, so a restart surfaces a pending shutdown rather than hiding it.
    let mut sd = Shutdown::new();
    sd.changed(&ctx);

    let mut gov = Governor { fast: false, next: 0, primed: false, off: !control, active: [0; GOV_CORES], total: [0; GOV_CORES] };
    // What the clock was last set to, so it is changed - and said - only when the answer changes.
    let mut clock_fast = false;
    if control {
        ctx.log_fmt(format_args!(
            "power: governor on - every {} ms, a core over {}% busy holds the clock at its maximum for {} s (Raspberry Pi OS's ondemand)",
            SAMPLE_MS, UP_THRESHOLD, SAMPLE_MS * SAMPLING_DOWN_FACTOR / 1000));
    }

    let mut no_cap: u32 = 0;
    loop {
        // WAIT FOR THE EARLIEST EVENT - a lease expiry, or the shutdown's next warning or tidy-up - or block outright
        // when there is none: an idle machine costs this service nothing at all.
        let now = gs::driver::wait::ticks(&ctx);
        // Milliseconds, rounded UP: waking a fraction late is a lease running a fraction long, and waking
        // early would only loop back here. The governor's next look is one more deadline.
        let per_ms = cycles(&ctx, 1).max(1);
        let lease_wait = leases.slots.iter().flatten().map(|l| l.until).min()
            .map(|until| until.saturating_sub(now).div_ceil(per_ms));
        let gov_wait = if gov.off { None } else { Some(gov.next.saturating_sub(now).div_ceil(per_ms)) };
        // The shutdown's next event is in the KERNEL's seconds, never this service's clock: on QEMU RISC-V the
        // two did not keep time together, and the deadline fired before the last-second tidy-up.
        let wait = [lease_wait, sd.secs_to_next().map(|s| s * 1000), gov_wait].into_iter().flatten().min();
        let msg = match wait {
            None => Some(gs::ipc::recv(&ctx)),
            Some(0) => None,
            Some(ms) => gs::ipc::recv_within_ms(&ctx, ms),
        };

        // THE GOVERNOR'S LOOK, when it is due.
        let now = gs::driver::wait::ticks(&ctx);
        if !gov.off && now >= gov.next {
            if let Some((fast, load, core)) = gov.sample(&ctx) {
                let samples = if fast { SAMPLING_DOWN_FACTOR } else { 1 };
                gov.next = now.wrapping_add(cycles(&ctx, SAMPLE_MS * samples));
                if fast != gov.fast {
                    gov.fast = fast;
                    let want = fast || leases.open() > 0;
                    if want != clock_fast {
                        clock_fast = want;
                        let why = if fast { "a core is busy" } else { "no core is busy and no lease is open" };
                        if apply(&ctx, want, why, &mut no_control_said).is_some() {
                            ctx.log_fmt(format_args!("power: governor - the busiest core, {}, was {}% busy", core, load));
                        }
                    }
                }
            }
        }

        // EXPIRE whatever is due, whether or not a message arrived - a busy endpoint must not keep a
        // lease alive past its bound.
        let now = gs::driver::wait::ticks(&ctx);
        let before = leases.open();
        for slot in leases.slots.iter_mut() {
            if let Some(l) = slot {
                if l.until <= now {
                    ctx.log_fmt(format_args!(
                        "power: lease {} EXPIRED after {} s - its holder never released it (died, hung, or ran long)",
                        l.id, l.secs));
                    *slot = None;
                }
            }
        }
        if before > 0 && leases.open() == 0 && !gov.fast && clock_fast {
            clock_fast = false;
            let _ = apply(&ctx, false, "the last lease expired", &mut no_control_said);
        }

        // The pending shutdown: re-read the kernel's deadline (a cancel, or one brought earlier), warn at the
        // thresholds, and tidy up three seconds out.
        // A CUE FIRST: a cancel that arrives as this service wakes must be read as a cancel, not mistaken by the
        // countdown step for the deadline falling due - which left `fs` refusing writes for a shutdown that was
        // not coming (`osdev test shutdown`).
        let op = msg.as_ref().and_then(|m| m.payload_bytes().first().copied());
        let cue = op == Some(OP_SHUTDOWN_CHANGED) || op == Some(OP_COPIER_CHANGED);
        if cue {
            // A cue without the byte is read as "running", for the reason `Shutdown::copier` gives.
            sd.copier = msg.as_ref().and_then(|m| m.payload_bytes().get(1).copied()) != Some(0);
        }
        if op == Some(OP_SHUTDOWN_CHANGED) { sd.changed(&ctx); } else { sd.tick(&ctx); }

        let Some(req) = msg else { continue };
        if cue { continue; }
        let Some(cap) = gs::ipc::take_sent_cap(&ctx) else {
            // A request with no reply cap cannot be answered. Rate-limited, because the sender chooses how
            // often this happens (a flood is exactly this), and still loud.
            no_cap = no_cap.saturating_add(1);
            if no_cap <= 3 || no_cap % 1000 == 0 {
                ctx.log_fmt(format_args!("power: {} request(s) with no reply cap - dropping", no_cap));
            }
            continue;
        };
        let p = req.payload_bytes();
        match p.first().copied() {
            Some(OP_HOLD) => {
                if !control {
                    reply(&ctx, cap, &[ST_NO_CONTROL, 0, 0, 0, 0, 0]);
                    continue;
                }
                let asked = p.get(1).copied().unwrap_or(0) as u64;
                let secs = if asked == 0 { DEFAULT_SECS } else { asked.min(MAX_SECS) };
                let Some(free) = leases.slots.iter().position(|s| s.is_none()) else {
                    ctx.log("power: every lease slot is open - refusing one more (the clock is already at its maximum)");
                    reply(&ctx, cap, &[ST_FULL, 0, 0, 0, 0, 0]);
                    continue;
                };
                let id = leases.take_id();
                let until = gs::driver::wait::ticks(&ctx).wrapping_add(cycles(&ctx, secs * 1000));
                leases.slots[free] = Some(Lease { id, until, secs });
                ctx.log_fmt(format_args!("power: lease {} opened for {} s ({} open)", id, secs, leases.open()));
                // 0 when the clock was already at its maximum - another lease, or the governor.
                let hz = if !clock_fast {
                    clock_fast = true;
                    apply(&ctx, true, "a lease is open", &mut no_control_said).unwrap_or(0)
                } else {
                    0
                };
                let h = hz.to_le_bytes();
                reply(&ctx, cap, &[ST_OK, id, h[0], h[1], h[2], h[3]]);
            }
            Some(OP_RELEASE) => {
                let Some(&id) = p.get(1) else {
                    reply(&ctx, cap, &[ST_BAD_REQUEST]);
                    continue;
                };
                let mut found = None;
                for slot in leases.slots.iter_mut() {
                    if let Some(l) = slot {
                        if l.id == id {
                            found = Some(l.secs);
                            *slot = None;
                        }
                    }
                }
                match found {
                    Some(_) => {
                        ctx.log_fmt(format_args!("power: lease {} released ({} open)", id, leases.open()));
                        if leases.open() == 0 && !gov.fast && clock_fast {
                            clock_fast = false;
                            let _ = apply(&ctx, false, "no lease is open", &mut no_control_said);
                        }
                        reply(&ctx, cap, &[ST_OK]);
                    }
                    None => reply(&ctx, cap, &[ST_UNKNOWN]),
                }
            }
            _ => reply(&ctx, cap, &[ST_BAD_REQUEST]),
        }
    }
}
