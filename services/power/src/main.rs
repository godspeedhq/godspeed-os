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
//!   and answers `[status, lease, hz:u32]`. While any lease is open the clock is at its maximum.
//! - `OP_RELEASE [2, lease]` closes it early and answers `[status]`. When none is open, the minimum.
//! - A lease nobody releases EXPIRES. A holder that dies or hangs mid-way therefore cannot pin the
//!   machine at full power: it lasts `MAX_SECS` at most, and the expiry is said (26.7). No death
//!   notification is needed for that, which is why there is none.
//!
//! **Restart.** This service is restartable like any other. A respawned instance knows of no lease, so
//! it puts the clock at its minimum - the honest state for "nobody has asked" - and a holder whose lease
//! was lost simply runs slower until it asks again. Bounded, loud, and never stuck fast.

// The stdlib for everything it covers - receiving, the reply capability, answering on it - and the
// SDK only for the one call that is this service's alone (`cpu_clock`, a privileged syscall the stdlib
// deliberately does not wrap).
use godspeed as gs;
use godspeed_sdk::{Message, ServiceContext};

/// The protocol, one opcode byte. Replies lead with a status byte.
const OP_HOLD: u8 = 1;
const OP_RELEASE: u8 = 2;

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

    let mut no_cap: u32 = 0;
    loop {
        // WAIT FOR THE EARLIEST EXPIRY, or block outright when nothing is open: an idle machine costs
        // this service nothing at all.
        let now = gs::driver::wait::ticks(&ctx);
        let next = leases.slots.iter().flatten().map(|l| l.until).min();
        // Whole seconds, rounded UP, because `gs::ipc::recv_within` counts in seconds: waking a fraction
        // late is a lease running a fraction long, and waking early would only loop back here.
        let msg = match next {
            None => Some(gs::ipc::recv(&ctx)),
            Some(until) => {
                let left = until.saturating_sub(now);
                if left == 0 {
                    None
                } else {
                    let per_sec = cycles(&ctx, 1000).max(1);
                    gs::ipc::recv_within(&ctx, ((left + per_sec - 1) / per_sec) as i64)
                }
            }
        };

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
        if before > 0 && leases.open() == 0 {
            let _ = apply(&ctx, false, "the last lease expired", &mut no_control_said);
        }

        let Some(req) = msg else { continue };
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
                let first = leases.open() == 0;
                leases.slots[free] = Some(Lease { id, until, secs });
                ctx.log_fmt(format_args!("power: lease {} opened for {} s ({} open)", id, secs, leases.open()));
                let hz = if first {
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
                        if leases.open() == 0 {
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
