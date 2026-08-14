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

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("time: starting - the wall clock is a service now (C1-6)");
    let mut clock = Clock::new();

    // Take a first reading so `date` answers immediately rather than after the first request.
    let first = clock.now(&ctx);
    ctx.log_fmt(format_args!("time: serving; first reading {} (source {})", first, clock.source));

    loop {
        let req = ctx.recv();
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
            OP_SET if p.len() >= 9 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&p[1..9]);
                let ok = clock.set_network(&ctx, i64::from_le_bytes(b));
                reply(&ctx, cap, &[u8::from(ok)]);
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
