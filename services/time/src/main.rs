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
//! **Slice 1 of 3** (`docs/commandment-audit.md`): the service exists and owns the policy, reading the
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
    /// The clock never reads below this. Persisted by whoever holds storage, handed back on boot.
    floor: i64,
}

impl Clock {
    const fn new() -> Self {
        Self { last: 0, source: SRC_UNSET, synced_at: 0, floor: 0 }
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
    fn now(&mut self, ctx: &ServiceContext) -> i64 {
        let raw = ctx.datetime().epoch_secs();
        // Implausible readings are not clocks. Fall back to what we last believed rather than
        // publishing a number that will be wrong in a way nobody notices until a file is dated 1970.
        let candidate = if (MIN_PLAUSIBLE..MAX_PLAUSIBLE).contains(&raw) {
            self.deglitch(raw)
        } else {
            self.last
        };
        let v = if candidate < self.floor { self.floor } else { candidate };
        if v > self.last {
            self.last = v;
            if self.source == SRC_UNSET {
                self.source = SRC_RTC;
            }
        }
        self.last
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
/// **Eight seconds, not two.** The floor lives on a USB stick reached through `dwc2`, which
/// time-shares one host channel with the keyboard - a write there is nothing like a local disk. At two
/// seconds every attempt timed out while the write was very likely landing anyway, so the retry below
/// wrote the same file over and over and blocked this service for two seconds each time.
const FS_SECS: i64 = 8;
/// How often to retry loading the floor while it has not been loaded yet.
const FLOOR_RETRY_MS: u64 = 2_000;
/// How many times to retry PERSISTING the floor before giving up until the clock is set again.
///
/// Bounded, and the bound is the point. The first version of this retried forever: with `fs` answering
/// slower than the deadline it produced an fs write every two seconds for the life of the machine, a log
/// line for each, and - because this service is single-threaded - a `date` that waited behind it. That
/// is not persistence, it is a storm (26.6: bounded behaviour, and a retry that never stops is not
/// bounded). Three attempts covers `fs` still mounting; past that the honest answer is to say so once.
const FLOOR_STORE_TRIES: u32 = 3;

/// `fs` replies `[tag, status, ...]`: the correlation tag it was given, THEN the status byte.
///
/// Both indices matter and getting them wrong is silent, because the tag this service sends is 0 and
/// `FS_OK` is also 0 - so reading the tag as the status "succeeds" no matter what actually happened.
/// That is exactly what the first version did, in both directions.
const R_TAG: usize = 0;
const R_STATUS: usize = 1;
/// A READ reply is `[tag, status, len:u32 LE, bytes..]`, so the file's bytes start here.
const R_READ_DATA: usize = 6;

/// Ask `fs` for the persisted floor and adopt it.
///
/// Returns false while `fs` is not answering yet - this service starts before it, so the first
/// attempts are expected to fail and are not an error.
fn floor_load(ctx: &ServiceContext, clock: &mut Clock) -> bool {
    let mut req = [0u8; 64];
    req[0] = 0;                                   // tag: this service has one request outstanding
    req[1] = FS_OP_READ;
    req[2] = FLOOR_PATH.len() as u8;
    req[3..3 + FLOOR_PATH.len()].copy_from_slice(FLOOR_PATH);
    let n = 3 + FLOOR_PATH.len();
    let reply = match ctx.request_with_reply_deadline("fs", &Message::from_bytes(&req[..n]), FS_SECS) {
        Some(r) => r,
        None => return false,                     // fs not up yet, or busy: try again later
    };
    let p = reply.payload_bytes();
    // Check the STATUS byte, not the tag. `p[R_TAG]` is the tag we sent (0) and `FS_OK` is also 0, so
    // testing the first byte passes unconditionally and reports success for a missing file.
    if p.len() <= R_READ_DATA || p[R_STATUS] != FS_OK {
        return false;                             // no file yet: nothing to adopt, and not a failure
    }
    // The file is the epoch in ASCII, as it was written. It starts AFTER the 4-byte length that the
    // read reply carries - parsing from index 1 read a length byte as a digit and always failed.
    let mut v: i64 = 0;
    let mut any = false;
    for &b in &p[R_READ_DATA..] {
        if !b.is_ascii_digit() { break; }
        v = v.saturating_mul(10).saturating_add((b - b'0') as i64);
        any = true;
    }
    if !any {
        return false;
    }
    if v > clock.floor {
        clock.floor = v;
        ctx.log_fmt(format_args!("time: adopted clock floor {} from {}", v,
                                 core::str::from_utf8(FLOOR_PATH).unwrap_or("?")));
    }
    true
}

/// Persist the floor, so the next boot starts no earlier than this moment.
fn floor_store(ctx: &ServiceContext, epoch: i64) -> bool {
    let mut num = [0u8; 24];
    let mut i = num.len();
    let mut v = if epoch < 0 { 0u64 } else { epoch as u64 };
    if v == 0 { i -= 1; num[i] = b'0'; }
    while v > 0 { i -= 1; num[i] = b'0' + (v % 10) as u8; v /= 10; }
    let digits = &num[i..];

    let mut req = [0u8; 64];
    req[0] = 0;
    req[1] = FS_OP_WRITE;
    req[2] = FLOOR_PATH.len() as u8;
    req[3..3 + FLOOR_PATH.len()].copy_from_slice(FLOOR_PATH);
    let off = 3 + FLOOR_PATH.len();
    req[off..off + digits.len()].copy_from_slice(digits);
    let n = off + digits.len();
    match ctx.request_with_reply_deadline("fs", &Message::from_bytes(&req[..n]), FS_SECS) {
        Some(r) if { let b = r.payload_bytes(); b.len() > R_STATUS && b[R_STATUS] == FS_OK } => {
            ctx.log_fmt(format_args!("time: clock floor {} recorded", epoch));
            true
        }
        // Silent HERE on purpose: the caller reports once when the bounded attempts are spent. Logging
        // per attempt is what turned a retry into a storm on hardware.
        _ => false,   // the CALLER reports, once, when the attempts are spent - see the loop
    }
}

/// Try to persist the floor, at most `FLOOR_STORE_TRIES` times, and say what happened exactly once.
///
/// Returns true when it is settled - written, or given up on - so the caller stops waking for it.
fn floor_store_bounded(ctx: &ServiceContext, epoch: i64, tries: &mut u32) -> bool {
    if floor_store(ctx, epoch) {
        return true;
    }
    *tries -= 1;
    if *tries == 0 {
        // One line, at the end, naming the consequence rather than the attempt. The clock is CORRECT
        // right now; what is lost is only the head start on the next boot.
        ctx.log("time: could not persist the clock floor - the next boot will start with no floor \
                 (the clock itself is set and unaffected)");
        return true;                              // settled: stop retrying
    }
    false
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
    let mut floor_loaded = false;
    let mut floor_stored = true;                  // nothing to write until the clock is actually set
    let mut store_tries = FLOOR_STORE_TRIES;
    let mut tries_left = 15u32;                   // ~30 s of retries, then stop asking
    loop {
        let req = if (floor_loaded || tries_left == 0) && floor_stored {
            ctx.recv()
        } else {
            match ctx.recv_timeout(ctx.duration_cycles(FLOOR_RETRY_MS)) {
                Some(m) => m,
                None => {
                    // A pending floor WRITE takes priority: the clock is already known, and what is
                    // missing is only its record on disk for the next boot.
                    if !floor_stored {
                        floor_stored = floor_store_bounded(&ctx, clock.last, &mut store_tries);
                        continue;
                    }
                    if tries_left > 0 {
                        tries_left -= 1;
                    }
                    if floor_load(&ctx, &mut clock) {
                        floor_loaded = true;
                    } else if tries_left == 0 {
                        ctx.log("time: no persisted clock floor on disk - the clock stays unset until the network sets it");
                    }
                    continue;
                }
            }
        };
        // The reply cap is taken ONCE, here, so no arm can take it twice or forget to. A request with
        // none is dropped LOUDLY: the caller is blocked waiting, and silence would leave it to time out
        // against a clean log.
        let cap = match ctx.take_pending_cap() {
            Some(c) => c,
            None => {
                ctx.log("time: request had no reply cap - dropping (cannot answer without one)");
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
                    store_tries = FLOOR_STORE_TRIES;
                    floor_stored = floor_store_bounded(&ctx, clock.last, &mut store_tries);
                    floor_loaded = true;         // the floor is now ours; stop retrying the read
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
