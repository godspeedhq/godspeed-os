// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
#![no_main]
//! `time` - the wall clock as a SERVICE, not a kernel responsibility.
//!
//! **Why this exists.** §4.3 names six kernel responsibilities and timekeeping is not among them, yet
//! `kernel/src/clock.rs` and `kernel/src/wallclock.rs` held the epoch conversion, the plausibility
//! window, the clock's provenance and its floor - 327 lines of policy in ring 0 (finding C1-6).
//!
//! **What the kernel keeps, and why.** The x86 CMOS RTC answers on port I/O (0x70/0x71), which a
//! service cannot reach; on ARM the equivalent is an MMIO register the kernel already maps. So the
//! kernel keeps the REGISTER READ - a hardware fact, like enumerating PCI - and this service owns what
//! the read MEANS: converting it, deciding whether to believe it, remembering where it came from, and
//! refusing to go backwards. Transport in the kernel, interpretation in a service, the same split that
//! took USB out of the kernel.
//!
//! **What it owns.** The current epoch, its source (RTC or network), when it was last synced, and the
//! floor. All of that is state with an owner: this task. None of it is a `static`.
//!
//! **Slice 1 of 3** (the C1-6 commandment walk; see `audits/userspace-audit.md` Audit 11): the service exists and owns the policy, reading the
//! raw RTC through the kernel query that already exists. Slice 2 moves the shell and `net-stack` onto
//! it; slice 3 deletes the kernel's two modules, the `SetClock` syscall and the wall-clock queries -
//! the first time in this audit a pinned kernel surface gets SMALLER.

use godspeed_sdk::{ServiceContext, Message};

/// The protocol. One byte of opcode, because the reply shape differs per op and a shared opcode space
/// is how two protocols on one endpoint collide (the lesson from `dwc2` serving block and frames).
pub const OP_NOW: u8 = 1; // -> [ok, epoch(8, le), source, age(8, le)]  age = -1 when never synced
pub const OP_SET: u8 = 2; // [epoch(8, le)] -> [ok]      network time (SNTP)
pub const OP_FLOOR_GET: u8 = 3; // -> [ok, floor(8, le)]
pub const OP_FLOOR_SET: u8 = 4; // [floor(8, le)] -> [ok]

pub const SRC_UNSET: u8 = 0;
pub const SRC_RTC: u8 = 1;
pub const SRC_NTP: u8 = 2;
/// The clock started from the persisted floor (`/clock.last`) - no RTC and no network yet. It is a real
/// reading, and knowing it came from the floor is what stops it being mistaken for a synced one.
pub const SRC_FLOOR: u8 = 3;

/// The plausible-epoch window: 2020-01-01 .. 2100-01-01.
///
/// A reading outside it is not a clock, it is a misread - and believing one is how a machine ends up
/// reporting a file from 1970 or 4383 days of uptime. Carried across from the kernel unchanged.
const MIN_PLAUSIBLE: i64 = 1_577_836_800;
const MAX_PLAUSIBLE: i64 = 4_102_444_800;

/// Everything this service owns. One struct, one owner, no statics (Invariant 9).
struct Clock {
    /// Last epoch we were willing to believe. 0 = nothing believed yet.
    last: i64,
    /// Where `last` came from.
    source: u8,
    /// Monotonic seconds at the last network sync, for reporting sync age.
    synced_at: i64,
    /// Monotonic seconds at the moment `last` was established. THE CLOCK ADVANCES FROM HERE.
    ///
    /// Without this the clock did not tick. `now` took the hardware reading, and on a board with no RTC
    /// that reading is implausible, so it fell back to `last` - which it then only ever replaced with
    /// something LARGER. Nothing produced anything larger, so `last` stayed exactly where it was set.
    /// `date sync` fetched the true time, stored it, reported success, and `date` returned that same
    /// second for the rest of the boot. The clock was correct once and frozen thereafter.
    ///
    /// A wall clock with no RTC is a base plus elapsed time, and this is the base's other half.
    base_mono: i64,
    /// The clock never reads below this. Persisted by whoever holds storage, handed back on boot.
    floor: i64,
}

impl Clock {
    const fn new() -> Self {
        Self { last: 0, source: SRC_UNSET, synced_at: 0, base_mono: 0, floor: 0 }
    }

    /// Reject a reading that moved BACKWARDS or jumped more than a day forward.
    ///
    /// A CMOS misread on an in-range year slips past a plausibility check but not past this: time does
    /// not run backwards, and a real clock does not gain a day between two reads seconds apart. Ported
    /// from the kernel's `clock::deglitch_epoch` unchanged, because it was already right - it was only
    /// in the wrong place.
    fn deglitch(&self, raw: i64) -> i64 {
        if self.last == 0 {
            return raw;
        }
        if raw < self.last || raw > self.last + 86_400 {
            return self.last;
        }
        raw
    }

    /// The current wall clock: the RTC, deglitched, floored, and remembered.
    /// The current epoch, in the order the operator asked for: hardware RTC, then whatever we were
    /// told (network), then the persisted floor, then nothing.
    fn now(&mut self, ctx: &ServiceContext) -> i64 {
        let mono = ctx.epoch_secs_monotonic();
        let raw = ctx.datetime().epoch_secs();

        // 1. A HARDWARE RTC, where there is one. It is battery-backed and authoritative, so it wins and
        //    re-bases everything else. An implausible reading is not a clock, it is a misread.
        if (MIN_PLAUSIBLE..MAX_PLAUSIBLE).contains(&raw) {
            let v = self.deglitch(raw).max(self.floor);
            self.last = v;
            self.base_mono = mono;
            if self.source == SRC_UNSET {
                self.source = SRC_RTC;
            }
            return self.last;
        }

        // 2/3. NO RTC, so the clock is a base plus the time elapsed since it was set - from the network
        //      if we have been told, otherwise from the persisted floor. This is the half that was
        //      missing: without it the reading never moved off whatever set it.
        if self.last == 0 && self.floor > 0 {
            self.last = self.floor;
            self.base_mono = mono;
            self.source = SRC_FLOOR;
        }
        if self.last == 0 {
            return 0;                       // 4. nothing to believe yet, and saying 0 says exactly that
        }
        let elapsed = (mono - self.base_mono).max(0);
        (self.last + elapsed).max(self.floor)
    }

    /// Accept a network reading (SNTP). Refused if implausible - a bad server does not get to move the
    /// clock to 1970, and saying so is better than silently ignoring it.
    fn set_network(&mut self, ctx: &ServiceContext, epoch: i64) -> bool {
        if !(MIN_PLAUSIBLE..MAX_PLAUSIBLE).contains(&epoch) {
            ctx.log_fmt(format_args!(
                "time: refusing network epoch {} - outside the plausible window", epoch));
            return false;
        }
        self.last = epoch;
        self.source = SRC_NTP;
        self.synced_at = ctx.epoch_secs_monotonic();
        // RE-BASE. `now` advances from (`last`, `base_mono`), so a new reading without a new base would
        // be instantly re-aged by however long the service had been up.
        self.base_mono = self.synced_at;
        ctx.log_fmt(format_args!("time: wall clock set from the network ({})", epoch));
        true
    }
}

fn reply(ctx: &ServiceContext, cap: godspeed_sdk::CapHandle, body: &[u8]) {
    let _ = ctx.try_send_by_handle(cap, &Message::from_bytes(body));
    ctx.remove_cap(cap);
}

/// Where the clock floor lives on disk.
const FLOOR_PATH: &[u8] = b"/clock.last";
/// fs opcodes this service uses. The wire format is [tag, op, path_len, path.., data..].
const FS_OP_WRITE: u8 = 10;
const FS_OP_READ: u8 = 11;
const FS_OK: u8 = 0;
/// How long an fs request may take before this service gives up on it for now.
///
/// Correlation tags for this service's own `fs` requests.
///
/// `fs` echoes the tag byte back, which is what lets the main loop recognise ITS OWN replies among the
/// requests it is serving - and that is what makes the floor I/O below non-blocking. A client request
/// arrives carrying a reply cap; an `fs` reply arrives without one and with one of these tags.
///
/// Distinct from 0 on purpose: 0 is what a caller sends who has not thought about tags, and it is also
/// `FS_OK`, a collision that has already hidden one bug in this file.
const TAG_FLOOR_READ: u8 = 0xF1;
const TAG_FLOOR_WRITE: u8 = 0xF2;
/// How often to retry loading the floor while it has not been loaded yet.
const FLOOR_RETRY_MS: u64 = 2_000;
/// How long between asking `net-stack` to fetch the network time, while we still have none.
///
/// PURSUING THE TIME IS THIS SERVICE'S JOB. It did not do it: `net-stack` pushed a result in when its
/// own dance happened to run, and otherwise the clock sat unset until an operator typed `date sync`.
/// That put resolution in the shell's hands and made the answer depend on somebody asking twice.
///
/// The nudge carries NO reply cap, which is what keeps it legal: `net-stack` calls this service after
/// SNTP, so a request in this direction would be two single-threaded services blocked on each other.
/// One-way, `try_send`, nothing awaited - so a full or dead `net-stack` cannot stall the clock (§8.9).
///
/// Bounded by its own success: the nudging stops the moment the clock is network-set, so a machine that
/// syncs at boot sends one or two and never another.
const SYNC_NUDGE_SECS: i64 = 20;
/// How long between network syncs once we already HAVE one.
///
/// Owning the clock does not stop at getting it once. A reading fetched at boot drifts, and a service
/// that fetched it once and never looked again is trusting a number that ages - so it re-checks, just
/// far less often than when it has nothing. Twenty seconds is the rhythm of a service with no answer;
/// an hour is the rhythm of one keeping a good answer good.
const RESYNC_SECS: i64 = 3_600;
/// How long to sleep between wake-ups once the clock is settled.
///
/// While there is no clock this service wakes every `FLOOR_RETRY_MS` because it has work to do. Once it
/// has one there is almost nothing to do, and waking every two seconds for an hour to notice that
/// nothing changed is exactly the noise the operator asked to avoid. A minute is far below the resync
/// interval, so nothing is late, and it costs sixty times less.
const SETTLED_WAKE_MS: u64 = 60_000;
/// How many times to retry PERSISTING the floor before giving up until the clock is set again.
///
/// Bounded, and the bound is the point. The first version of this retried forever: with `fs` answering
/// slower than the deadline it produced an fs write every two seconds for the life of the machine, a log
/// line for each, and - because this service is single-threaded - a `date` that waited behind it. That
/// is not persistence, it is a storm (26.6: bounded behaviour, and a retry that never stops is not
/// bounded). Three attempts covers `fs` still mounting; past that the honest answer is to say so once.
const FLOOR_STORE_TRIES: u32 = 3;
/// How stale the persisted floor may get before it is rewritten, in seconds.
///
/// The floor used to be written ONLY on a network sync, which made it as stale as the uptime since that
/// sync. A board that synced at boot, ran six hours and rebooted with no network came back six hours
/// behind - on top of however long it was powered off, which nothing can measure. The bound was correct
/// and needlessly loose.
///
/// Refreshing caps the avoidable half at this interval: a reboot then resumes within ten minutes of the
/// last known time, plus the unknowable off-period. It cannot do better than that and does not pretend
/// to - see `SRC_FLOOR`, this is a lower bound, not a clock.
///
/// REQUEST-DRIVEN, not a timer. The check rides on `OP_NOW`, which `date`, `observe` and every file
/// timestamp already ask for, so an idle machine writes nothing and this service keeps its property of
/// having no periodic wake-up. Ten minutes is one flash write an hour under any normal use.
const FLOOR_REFRESH_SECS: i64 = 600;

/// `fs` replies `[tag, status, ...]`: the correlation tag it was given, THEN the status byte.
///
/// Both indices matter and getting them wrong is silent, because the tag this service sends is 0 and
/// `FS_OK` is also 0 - so reading the tag as the status "succeeds" no matter what actually happened.
/// That is exactly what the first version did, in both directions.
const R_TAG: usize = 0;
const R_STATUS: usize = 1;
/// A READ reply is `[tag, status, len:u32 LE, bytes..]`, so the file's bytes start here.
const R_READ_DATA: usize = 6;

/// Send a request to `fs` WITHOUT waiting for the reply. **Non-blocking, not fire-and-forget:** the
/// reply is matched by tag in the main loop and a failure is retried there (bounded), so the floor does
/// get written - it just never holds anything up while it happens.
///
/// **This is the whole point of the file.** The wall clock lives in memory; `fs` is only where a copy is
/// kept for the next boot. A service that owns an in-memory answer must never become unable to give it
/// because a disk is slow - and this service is single-threaded, so any blocking call here is exactly
/// that. Earlier versions waited 2 s, then 8 s, then 1 s for `fs`; every one of them was a window in
/// which `date` had no answer despite the answer being known, and the 8 s version made `date` need
/// several attempts.
///
/// So the request goes out with a reply cap and the loop carries straight on. `fs` replies whenever it
/// can; that reply lands in this service's own queue like any other message, is recognised by its tag,
/// and is handled there. If `fs` is absent, slow, or never answers at all, the only consequence is that
/// the floor is not written - the clock keeps answering instantly throughout.
fn fs_send_noblock(ctx: &ServiceContext, req: &[u8]) -> bool {
    // ACQUIRE `fs` BY NAME, do not expect it to have been wired at spawn.
    //
    // This service starts BEFORE `fs` does (the supervisor spawns the clock early, because everything
    // else wants to timestamp), so at spawn there was no `fs` endpoint to wire a send-peer cap to and
    // the contract's peer entry resolves to nothing - permanently. That is why the floor had never once
    // been written on any boot: not a protocol fault, not a slow disk, simply no cap to send on.
    //
    // The kernel name directory is the answer to exactly this (§14.3): ask for the peer when you need
    // it, not when you started. Cached by the SDK after the first success, and re-acquired for free if
    // `fs` is restarted under us.
    let target = match ctx.send_peer_handle("fs") {
        Some(t) => t,
        None => {
            if !ctx.reacquire_by_name("fs") { return false; }
            match ctx.send_peer_handle("fs") { Some(t) => t, None => return false }
        }
    };
    let Some(self_grant) = ctx.self_grant_handle() else { return false };
    let Some(reply_cap) = ctx.derive_cap(self_grant) else { return false };
    // The reply cap is CONSUMED by `fs` when it answers. If the send itself fails, reclaim it here so a
    // dead `fs` cannot leak one cap-table slot per attempt (§8.5: a transfer that failed leaves the cap
    // with the sender, and it is the sender's job to notice).
    if ctx.send_with_cap_by_handle(target, reply_cap, &Message::from_bytes(req)).is_err() {
        ctx.remove_cap(reply_cap);
        // A HELD CAP CAN GO STALE, and that is not the same as not having one.
        //
        // The reacquire above only runs when there is NO handle. A handle we already hold keeps being
        // used after `fs` restarts, because the generation bumped underneath it - so every attempt
        // fails identically and the next one, two seconds later, fails the same way. On hardware that
        // was `cap::get: ResourceId(102) gen mismatch ... liveness=Alive` repeating every two seconds
        // for the rest of the boot: `fs` alive and well, and this service posting to its previous life.
        //
        // §14.3 is explicit that a client reacquires by name after a restart. Doing it only on absence
        // covers the peer that never existed and misses the peer that came back, which is the far more
        // common case under chaos.
        if !ctx.reacquire_by_name("fs") {
            return false;
        }
        let Some(target) = ctx.send_peer_handle("fs") else { return false };
        let Some(reply_cap) = ctx.derive_cap(self_grant) else { return false };
        if ctx.send_with_cap_by_handle(target, reply_cap, &Message::from_bytes(req)).is_err() {
            ctx.remove_cap(reply_cap);
            return false;
        }
    }
    true
}

/// Ask `fs` for the persisted floor. Non-blocking: the answer arrives later, tagged.
fn floor_load(ctx: &ServiceContext) -> bool {
    let mut req = [0u8; 64];
    req[0] = TAG_FLOOR_READ;
    req[1] = FS_OP_READ;
    req[2] = FLOOR_PATH.len() as u8;
    req[3..3 + FLOOR_PATH.len()].copy_from_slice(FLOOR_PATH);
    let n = 3 + FLOOR_PATH.len();
    fs_send_noblock(ctx, &req[..n])
}

/// Adopt a floor from an `fs` READ reply. Returns true if this reply settles the question either way -
/// a value adopted, or a definite "no such file" - so the caller stops asking.
fn floor_adopt(ctx: &ServiceContext, clock: &mut Clock, p: &[u8]) -> bool {
    // Check the STATUS byte, not the tag. `p[R_TAG]` is the tag and `FS_OK` is 0, so testing the first
    // byte passes unconditionally and reports success for a missing file - the bug this file had.
    if p.len() <= R_READ_DATA || p[R_STATUS] != FS_OK {
        return true;                              // fs answered "no file": settled, nothing to adopt
    }
    // The file is the epoch in ASCII. It starts AFTER the 4-byte length the read reply carries; parsing
    // from index 1 read a length byte as a digit and always failed.
    let mut v: i64 = 0;
    let mut any = false;
    for &b in &p[R_READ_DATA..] {
        if !b.is_ascii_digit() { break; }
        v = v.saturating_mul(10).saturating_add((b - b'0') as i64);
        any = true;
    }
    if !any {
        return true;                              // present but unreadable: settled, not retryable
    }
    if v > clock.floor {
        clock.floor = v;
        ctx.log_fmt(format_args!("time: adopted clock floor {} from {}", v,
                                 core::str::from_utf8(FLOOR_PATH).unwrap_or("?")));
    }
    true
}

/// Persist the floor, so the next boot starts no earlier than this moment. Non-blocking.
fn floor_store(ctx: &ServiceContext, epoch: i64) -> bool {
    let mut num = [0u8; 24];
    let mut i = num.len();
    let mut v = if epoch < 0 { 0u64 } else { epoch as u64 };
    if v == 0 { i -= 1; num[i] = b'0'; }
    while v > 0 { i -= 1; num[i] = b'0' + (v % 10) as u8; v /= 10; }
    let digits = &num[i..];

    let mut req = [0u8; 64];
    req[0] = TAG_FLOOR_WRITE;
    req[1] = FS_OP_WRITE;
    req[2] = FLOOR_PATH.len() as u8;
    req[3..3 + FLOOR_PATH.len()].copy_from_slice(FLOOR_PATH);
    let off = 3 + FLOOR_PATH.len();
    req[off..off + digits.len()].copy_from_slice(digits);
    let n = off + digits.len();
    fs_send_noblock(ctx, &req[..n])
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("time: starting - the wall clock is a service now (C1-6)");
    let mut clock = Clock::new();

    // Take a first reading so `date` answers immediately rather than after the first request.
    let first = clock.now(&ctx);
    ctx.log_fmt(format_args!("time: serving; first reading {} (source {})", first, clock.source));

    // RECONCILE IN THE BACKGROUND, AFTER THE SYSTEM IS UP.
    //
    // The Pi 2 has no RTC, so the only sources of truth are the persisted floor and the network - and
    // neither may hold up a boot. Nothing waits for this: services start, the prompt answers, and the
    // clock resolves itself afterwards if it can.
    //
    // While the floor is still unread this loop waits with a TIMEOUT so it wakes to retry; `fs` starts
    // after this service, so the first attempts are expected to fail and are not errors. Once the floor
    // is adopted (or the retries are spent) it reverts to a plain blocking `recv` and costs nothing at
    // all. Bounded, self-terminating, and confined to the service that owns the clock - as opposed to
    // the shell polling for it in front of the keyboard, which is what this replaces.
    let mut floor_settled = false;                // has fs answered the READ, either way?
    let mut store_left = 0u32;                    // writes still owed (0 = nothing to persist)
    let mut store_epoch = 0i64;                   // the value those writes are for
    let mut tries_left = 15u32;                   // ~30 s of asking, then stop
    let mut last_nudge = i64::MIN / 2;            // monotonic second of the last sync nudge
    let mut nudge_ok: Option<bool> = None;        // did the last nudge reach net-stack? None = never tried
    let mut no_cap: u32 = 0;                      // capless messages seen (a flood, usually)
    loop {
        // Wake on a timer only while there is still housekeeping OUTSTANDING - a floor to read, or a
        // floor to write that has not been acknowledged. Once both are settled this is a plain blocking
        // `recv` and the service costs nothing at all.
        // Keep waking while the clock is still unresolved, so the nudge below can go out. This is
        // bounded by success - once the network sets the clock, `unsynced` is false and this service
        // goes back to blocking on `recv` with no timer at all.
        let unsynced = clock.source != SRC_NTP;
        // WHEN IS THE NEXT NETWORK CHECK DUE? Two rhythms, because two situations: with no clock at all
        // this service asks often, and with a good one it asks rarely to keep it good. It never stops
        // asking - a clock fetched once at boot drifts, and never looking again is trusting a number
        // that ages.
        let mono_top = ctx.epoch_secs_monotonic();
        let interval = if unsynced { SYNC_NUDGE_SECS } else { RESYNC_SECS };
        let sync_due = mono_top - last_nudge >= interval;
        // ALWAYS a timed wait, never an indefinite block. The clock is never "settled" in the sense of
        // finished - it settles into a slower rhythm.
        //
        // Gating the timer on outstanding work was right when this service only reacted; it is wrong
        // now that it maintains something. The moment a sync succeeded every condition went false, the
        // loop blocked on `recv`, and the next re-sync could only happen if somebody happened to ask
        // the time - which on an idle machine is never. A clock that maintains itself has to wake up.
        let wake_ms = if unsynced || !floor_settled || store_left > 0 {
            FLOOR_RETRY_MS
        } else {
            SETTLED_WAKE_MS
        };
        let req = {
            match ctx.recv_timeout(ctx.duration_cycles(wake_ms)) {
                Some(m) => m,
                None => {
                    // A pending WRITE first: the clock is already known, and this is the copy that
                    // survives a power cycle.
                    //
                    // Retrying HERE, on the timer, is what makes this reliable rather than best-effort.
                    // A write whose reply says "failed" is retried from the reply arm below; a write
                    // whose SEND failed - `fs` not spawned yet, or dead and not yet respawned - has no
                    // reply coming at all, so without this it would be attempted once and silently
                    // never again. That is the difference between "we tried" and "it gets written".
                    // KEEP THE PERSISTED FLOOR FRESH, on this service's own heartbeat.
                    //
                    // The refresh used to ride on `OP_NOW`, which meant it only happened if somebody
                    // asked the time. A machine left alone therefore persisted nothing, and a reboot
                    // resumed from however old the last question was. Owning the clock includes owning
                    // the copy that survives a power cut, and that cannot depend on being asked.
                    //
                    // Only a clock worth persisting: a floor-derived reading re-persisting itself would
                    // just rewrite what it was handed, and an unset one has nothing to write.
                    if store_left == 0 && clock.source != SRC_UNSET && clock.source != SRC_FLOOR {
                        let reading = clock.now(&ctx);
                        if reading > 0 && reading - store_epoch >= FLOOR_REFRESH_SECS {
                            store_epoch = reading;
                            store_left = FLOOR_STORE_TRIES;
                            let _ = floor_store(&ctx, store_epoch);
                        }
                    }
                    // ASK FOR THE TIME, since nobody else will. See `SYNC_NUDGE_SECS`.
                    if sync_due {
                        {
                            // ACQUIRE BY NAME FIRST, and SAY whether it worked.
                            //
                            // `net-stack` is spawned AFTER this service, so at spawn time there was no
                            // endpoint to wire and `find_send_slot` finds nothing - forever. The first
                            // version of this sent into that hole every twenty seconds and reported
                            // nothing, so a clock that never resolved looked identical to a network
                            // that never answered. `nic-driver` documents this exact trap and I did not
                            // apply it here.
                            //
                            // The outcome is logged ONCE per state change rather than per attempt: a
                            // nudge every twenty seconds must not become a log every twenty seconds,
                            // but a send that never leaves must not be invisible either (§26.7).
                            let ok = ctx.try_send("net-stack", &Message::from_bytes(&[11u8])).is_ok()
                                || (ctx.reacquire_by_name("net-stack")
                                    && ctx.try_send("net-stack", &Message::from_bytes(&[11u8])).is_ok());
                            // ONLY A SENT NUDGE COUNTS AGAINST THE INTERVAL. This used to stamp
                            // `last_nudge` before the attempt, so a nudge that never left still bought
                            // twenty seconds of silence - and after a respawn the first attempt always
                            // fails (the peer's cap is stale), so a fresh `time` sat on a stale floor
                            // for twenty seconds before trying again. A failure should be retried at
                            // the heartbeat, not rewarded with the full interval.
                            if ok {
                                last_nudge = ctx.epoch_secs_monotonic();
                            }
                            // `Option`, so the FIRST outcome always speaks. A plain bool starting at
                            // `false` made a nudge that failed from the very first attempt log nothing
                            // at all - no transition - which is precisely the silence this line exists
                            // to break, and it hid this bug for a boot.
                            if nudge_ok != Some(ok) {
                                nudge_ok = Some(ok);
                                ctx.log(if ok {
                                    "time: asking net-stack for the network clock"
                                } else {
                                    "time: cannot reach net-stack to ask for the clock - retrying"
                                });
                            }
                            // One way, and the outcome is not awaited - there is nothing to await.
                            // `net-stack` pushes the answer back through OP_SET when it has
                            // one, which is the path that already works.

                        }
                    }
                    if store_left > 0 && !floor_store(&ctx, store_epoch) {
                        store_left -= 1;
                        if store_left == 0 {
                            ctx.log("time: cannot reach fs to persist the clock floor - the next boot starts with no floor (the clock itself is set and unaffected)");
                        }
                        continue;
                    }
                    if !floor_settled && tries_left > 0 {
                        tries_left -= 1;
                        // Send the READ and carry straight on - the answer arrives as a tagged message
                        // below. Nothing here waits on `fs`.
                        if !floor_load(&ctx) && tries_left == 0 {
                            ctx.log("time: no persisted clock floor on disk - the clock stays unset until the network sets it");
                        }
                    }
                    continue;
                }
            }
        };
        // OUR OWN fs REPLY, or a client request? A client request carries a reply cap; a reply to the
        // non-blocking floor I/O does not, and carries one of our tags. Handling it here - in the
        // ordinary receive loop - is what lets the floor be written reliably without ever blocking an
        // answer: the acknowledgement is read, and a failure is retried, on the same loop that serves
        // `date`.
        let cap = match ctx.take_pending_cap() {
            Some(c) => c,
            None => {
                let p = req.payload_bytes();
                match p.first().copied() {
                    Some(TAG_FLOOR_READ) => {
                        floor_settled = floor_adopt(&ctx, &mut clock, p);
                    }
                    Some(TAG_FLOOR_WRITE) => {
                        if p.len() > R_STATUS && p[R_STATUS] == FS_OK {
                            ctx.log_fmt(format_args!("time: clock floor {} recorded", store_epoch));
                            store_left = 0;       // acknowledged: nothing further owed
                        } else if store_left > 0 {
                            // `fs` answered and said no (read-only mount, no space, no disk). Count it
                            // and let the timer wake retry - immediately re-sending would spin against
                            // a service that has just told us it cannot.
                            store_left -= 1;
                            if store_left == 0 {
                                // Once, at the end, naming the CONSEQUENCE rather than the attempt: the
                                // clock is correct right now; only the next boot's head start is lost.
                                ctx.log("time: fs refused the clock floor - the next boot starts with no floor (the clock itself is set and unaffected)");
                            }
                        }
                    }
                    _ => {
                        // RATE-LIMITED, because the sender chooses how often this happens.
                        //
                        // A message with no reply cap is what a flood looks like (`chaos flood-storm`
                        // sends exactly that), and this logged one line per message: 839 of them in a
                        // single overnight run. On this port a serial write is an un-preemptible
                        // syscall of roughly 9 ms per line, so that is ~7 s of the core spent
                        // announcing that someone is shouting at us - the report becoming a bigger
                        // problem than the thing reported.
                        //
                        // Still LOUD (invariant 12), just not once per event: the first few say it is
                        // happening, then a running count at widening intervals says it still is.
                        no_cap = no_cap.saturating_add(1);
                        if no_cap <= 3 || no_cap % 1000 == 0 {
                            ctx.log_fmt(format_args!(
                                "time: {} request(s) with no reply cap - dropping (cannot answer without one)",
                                no_cap));
                        }
                    }
                }
                continue;
            }
        };
        let p = req.payload_bytes();
        if p.is_empty() {
            reply(&ctx, cap, &[0]);
            continue;
        }
        match p[0] {
            OP_NOW => {
                let now = clock.now(&ctx);
                // KEEP THE FLOOR FRESH, on the back of a question somebody was already asking. See
                // `FLOOR_REFRESH_SECS`. Only when the clock is worth persisting (a floor-derived
                // reading re-persisting itself would just rewrite what it was given) and only when
                // nothing is already owed, so this never competes with the retry machinery.
                if store_left == 0 && clock.source != SRC_UNSET && clock.source != SRC_FLOOR
                    && now > 0 && now - store_epoch >= FLOOR_REFRESH_SECS {
                    store_epoch = now;
                    store_left = FLOOR_STORE_TRIES;
                    let _ = floor_store(&ctx, store_epoch);
                }
                // The age is APPENDED, not squeezed in: every existing reader checks `len >= 10` and
                // indexes 0..10, so a longer reply is compatible by construction. The alternative -
                // a second opcode - would make "when was this set" a separate round trip from "what
                // is it", and the two can then disagree.
                //
                // -1 means NEVER SYNCED, which is not the same as "synced 0 seconds ago". Collapsing
                // the two would let a clock that was never set read as freshly authoritative.
                let age = if clock.source == SRC_NTP { ctx.epoch_secs_monotonic() - clock.synced_at }
                          else { -1 };
                let mut out = [0u8; 18];
                out[0] = 1;
                out[1..9].copy_from_slice(&now.to_le_bytes());
                out[9] = clock.source;
                out[10..18].copy_from_slice(&age.to_le_bytes());
                reply(&ctx, cap, &out);
            }
            // Persisting here, not in a client, is the whole point: the clock's owner records the
            // clock's floor at the moment it learns the time. `net-stack` PUSHES the SNTP result to
            // this op - the direction that avoids a call cycle between two single-threaded services.
            OP_SET if p.len() >= 9 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&p[1..9]);
                let ok = clock.set_network(&ctx, i64::from_le_bytes(b));
                reply(&ctx, cap, &[u8::from(ok)]);
                // The clock just became known: record the floor so the next boot starts no earlier
                // than now. Answer the caller FIRST - `net-stack` is blocked on that reply, and it
                // must not wait on a disk write to learn its own result.
                if ok {
                    // Hand the write to the loop rather than doing it here. The caller (`net-stack`) has
                    // its answer already and must not wait behind a disk, and neither must the next
                    // `date`. The loop sends it, watches for the acknowledgement, and retries on the
                    // timer if `fs` is not reachable yet - reliable, but never blocking.
                    // `now()`, not `last`: `last` is the BASE the clock advances from, not the current
                    // reading. They are equal at this instant because the sync just re-based, and
                    // writing the one that means "the time" keeps it correct if that ever changes.
                    store_epoch = clock.now(&ctx);
                    store_left = FLOOR_STORE_TRIES;
                    let _ = floor_store(&ctx, store_epoch);
                    floor_settled = true;        // the clock is set; the stored floor no longer matters
                }
            }
            OP_FLOOR_GET => {
                let mut out = [0u8; 9];
                out[0] = 1;
                out[1..9].copy_from_slice(&clock.floor.to_le_bytes());
                reply(&ctx, cap, &out);
            }
            OP_FLOOR_SET if p.len() >= 9 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&p[1..9]);
                let f = i64::from_le_bytes(b);
                // The floor only ever rises. A floor that could fall is not a floor, and the whole
                // point is that a fresh boot cannot report a time before the last one it recorded.
                if f > clock.floor {
                    clock.floor = f;
                }
                reply(&ctx, cap, &[1]);
            }
            other => {
                ctx.log_fmt(format_args!("time: unknown op {} - answering, not ignoring", other));
                reply(&ctx, cap, &[0]);
            }
        }
    }
}
