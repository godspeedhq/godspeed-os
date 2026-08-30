// SPDX-License-Identifier: GPL-2.0-only
//! `logger` - the diagnostic sink (§11.4). Restartable.
//!
//! Two jobs, both "somewhere to put diagnostic data that someone reads later":
//!
//! 1. **Drain its endpoint.** The endpoint EXISTS, so anything sent here must be consumed or the
//!    16-deep queue sits full forever (a stub that only parks never recv's, and a `chaos flood-storm`
//!    then wedges it permanently).
//! 2. **Hold the IPC trace ring** (`utilities/46_trace.md` mechanism B) - a bounded history of
//!    request/reply events emitted by services that hold a send cap to this one, read back by the
//!    `trace` utility.
//!
//! # Why the ring is HERE and not in the kernel, and not in a service of its own
//!
//! The requirement asked for a bounded ring recording IPC, and the obvious home was the kernel: it is
//! already at the routing point holding sender, endpoint and generation. That reading answers "where
//! is the data?" instead of "whose responsibility is this?". A kernel ring would have added storage
//! with a lifecycle, a retention policy (what to discard is a JUDGEMENT, and judgement is policy -
//! §26.10), a message-identity scheme, a control syscall, an authority decision, and a write on the
//! hottest path in the system.
//!
//! The first fix was a `tracer` service. The enforcement layer refused it, and was right to: the
//! kernel holds a `service_config` for every service, and that catalogue is pinned as DEBT THAT MAY
//! ONLY SHRINK - so even a userspace ring would have cost ring 0 three lines (the config, the
//! death-notification set, the restart counter). "The kernel gains nothing" was *almost* true, and the
//! almost was the whole point.
//!
//! `logger` already exists in all three lists, is already managed and watched, and its entire purpose
//! is diagnostic data. Putting the ring here costs the kernel **exactly nothing**.
//!
//! It is worth being precise about one tension: `docs/logging.md` calls this service "a stateless
//! broker, not a store", and that is about PERSISTENCE - a logger that writes through `fs` makes
//! observing a storage failure depend on storage. A fixed in-memory ring is not persistence. It
//! survives nothing: a restarted logger starts empty, and that is correct, because the ring is
//! history and nothing depends on it.
//!
//! # Bounded, and loud about loss (§26.6, invariant 12)
//!
//! Fixed ring, fixed events, no heap. Full = overwrite the oldest and COUNT it; `trace status` reports
//! the count. A ring that discards silently is the bug this project just fixed in the x86 keyboard
//! path - an instrument that loses data without saying so is worse than no instrument.

#![no_std]
#![no_main]

use godspeed_sdk::trace::{EV_LEN, PEER_LEN, TRACE_OP_DUMP, TRACE_OP_EVENT, TRACE_OP_STATUS};
use godspeed_sdk::{Message, ServiceContext};

/// Events retained. 192 x 34 B is about 6.5 KiB, inside this service's existing footprint.
///
/// Sized for "what just happened", which is the question a stalled chain asks. Deliberately NOT sized
/// for "what happened a minute ago": under load that needs either a much larger ring or filtering at
/// the emitter, and filtering-in-the-middle is the first step toward putting a programmable VM
/// somewhere it does not belong. If longer history is ever wanted, the honest answer is a bigger arena
/// HERE, where it costs one service more memory and costs the kernel nothing.
const RING: usize = 192;

/// One recorded event - mirrors the wire format in `sdk::trace`.
#[derive(Clone, Copy, Default)]
struct Ev {
    seq: u32,
    at_s: u32,
    /// Who made the call, as that service declared itself (`ServiceContext::trace_as`). A service
    /// cannot ask what it is called - identity is not ambient - so a traced one says. Exactly as
    /// trustworthy as the two fields below, because the whole event is the emitter's testimony.
    caller: [u8; PEER_LEN],
    /// The PEER'S NAME, as the emitter knew it. Not an endpoint and not a cap slot: a slot is local to
    /// the emitter and means nothing here, and a name is what a reader actually wants.
    peer: [u8; PEER_LEN],
    op: u8,
    kind: u8,
}

/// Answer a query over the reply cap the request carried, NON-BLOCKING (§8.9).
///
/// A reader that has gone away must never block the logger: this service sits at the end of every
/// emitter's `try_send`, so a blocking reply here would let one stalled reader stall the sink for the
/// whole system. Dropped on failure - the caller retries, and a lost answer costs nothing.
fn reply(ctx: &ServiceContext, out: &[u8]) {
    if let Some(cap) = ctx.take_pending_cap() {
        let _ = ctx.try_send_by_handle(cap, &Message::from_bytes(out));
        // RECLAIM IT. A reply capability is a one-shot return address handed to us inside the request;
        // sending on it does not consume it, so leaving it behind burns a cap-table slot per reply
        // until the table is full. `block-driver`, `console` and `fs` all do this - this service was
        // the one that did not.
        //
        // It was visible before it was fatal: `trace deps fs` drew `logger -> shell`, because a
        // retained return address is indistinguishable from a wired peer (both SEND|GRANT to a live
        // task's endpoint). A leak that shows up as a wrong arrow in a diagram is a lucky leak.
        ctx.remove_cap(cap);
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut ring = [Ev::default(); RING];
    let mut next = 0usize; // write cursor
    let mut total = 0u64; // events ever accepted
    let mut dropped = 0u64; // events overwritten before being read

    // The event clock, CACHED. `epoch_secs_monotonic` is a CMOS RTC read on x86 - `wait_update_clear`
    // can spin ~1 ms before seven port-I/O reads - so calling it per event would cap this sink at
    // roughly a thousand events a second and drop the rest under a storm. The cycle counter is one
    // instruction, so read THAT every time and refresh the seconds only when a second has actually
    // passed. Events within the same second share a stamp, which is exactly the resolution the field
    // has anyway.
    let per_sec = ctx.duration_cycles(1000);
    let mut at_s = ctx.epoch_secs_monotonic() as u32;
    let mut at_tsc = ctx.read_tsc();

    ctx.trace_as("logger");
    ctx.log("logger: ready (drains its endpoint; holds the IPC trace ring)");

    loop {
        let msg = ctx.recv();
        let b = msg.payload_bytes();
        if b.is_empty() {
            continue;
        }
        match b[0] {
            // An emitted trace event. Fire-and-forget: the sender used `try_send` and did not wait, so
            // a full queue costs the emitter nothing and loses one event - the correct trade for an
            // observability path, and the opposite of the one made on a correctness path.
            TRACE_OP_EVENT if b.len() >= 1 + EV_LEN => {
                let e = &b[1..1 + EV_LEN];
                let tsc = ctx.read_tsc();
                if tsc.wrapping_sub(at_tsc) >= per_sec {
                    at_s = ctx.epoch_secs_monotonic() as u32;
                    at_tsc = tsc;
                }
                if next == RING {
                    next = 0;
                }
                // Overwriting a slot that was never read IS a loss. Counted, and reported.
                if total >= RING as u64 {
                    dropped += 1;
                }
                let mut caller = [0u8; PEER_LEN];
                caller.copy_from_slice(&e[8..8 + PEER_LEN]);
                let mut peer = [0u8; PEER_LEN];
                peer.copy_from_slice(&e[8 + PEER_LEN..8 + 2 * PEER_LEN]);
                ring[next] = Ev {
                    seq: u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                    // STAMPED HERE, not by the emitter: putting a clock read on every service's
                    // request path made the shell drop keystrokes. This service has to wake to
                    // receive the event anyway, so the cost lands where the job is - and it is the
                    // cached clock above, not a per-event RTC read.
                    at_s,
                    caller,
                    peer,
                    op: e[8 + 2 * PEER_LEN],
                    kind: e[9 + 2 * PEER_LEN],
                };
                next += 1;
                total += 1;
            }
            // `trace ipc` / `trace failures` - the most recent events, oldest of the tail first.
            TRACE_OP_DUMP => {
                let want = if b.len() >= 2 { b[1] as usize } else { 110 };
                let have = (total as usize).min(RING);
                let n = want.min(have).min(110); // 110 x 34 = 3740 B, inside one 4 KiB message
                let mut out = [0u8; 1 + 110 * EV_LEN];
                out[0] = n as u8;
                for i in 0..n {
                    let idx = (next + RING - n + i) % RING;
                    let e = &ring[idx];
                    let o = 1 + i * EV_LEN;
                    out[o..o + 4].copy_from_slice(&e.seq.to_le_bytes());
                    out[o + 4..o + 8].copy_from_slice(&e.at_s.to_le_bytes());
                    out[o + 8..o + 8 + PEER_LEN].copy_from_slice(&e.caller);
                    out[o + 8 + PEER_LEN..o + 8 + 2 * PEER_LEN].copy_from_slice(&e.peer);
                    out[o + 8 + 2 * PEER_LEN] = e.op;
                    out[o + 9 + 2 * PEER_LEN] = e.kind;
                }
                reply(&ctx, &out[..1 + n * EV_LEN]);
            }
            // `trace status` - capacity / recorded / dropped.
            TRACE_OP_STATUS => {
                let mut out = [0u8; 24];
                out[0..8].copy_from_slice(&(RING as u64).to_le_bytes());
                out[8..16].copy_from_slice(&total.to_le_bytes());
                out[16..24].copy_from_slice(&dropped.to_le_bytes());
                reply(&ctx, &out);
            }
            // Anything else is drained and dropped, which is the job this service already had: an
            // unconsumed endpoint fills at 16 and never empties.
            _ => {}
        }
    }
}
