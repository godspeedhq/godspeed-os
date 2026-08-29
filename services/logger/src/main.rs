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

/// Events retained. 192 x 22 B is a little over 4 KiB, inside this service's existing footprint.
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
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mut ring = [Ev::default(); RING];
    let mut next = 0usize; // write cursor
    let mut total = 0u64; // events ever accepted
    let mut dropped = 0u64; // events overwritten before being read

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
                if next == RING {
                    next = 0;
                }
                // Overwriting a slot that was never read IS a loss. Counted, and reported.
                if total >= RING as u64 {
                    dropped += 1;
                }
                let mut peer = [0u8; PEER_LEN];
                peer.copy_from_slice(&e[8..8 + PEER_LEN]);
                ring[next] = Ev {
                    seq: u32::from_le_bytes([e[0], e[1], e[2], e[3]]),
                    at_s: u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
                    peer,
                    op: e[8 + PEER_LEN],
                    kind: e[9 + PEER_LEN],
                };
                next += 1;
                total += 1;
            }
            // `trace ipc` / `trace failures` - the most recent events, oldest of the tail first.
            TRACE_OP_DUMP => {
                let want = if b.len() >= 2 { b[1] as usize } else { 32 };
                let have = (total as usize).min(RING);
                let n = want.min(have).min(32); // 32 x 22 = 704 B, inside one message
                let mut out = [0u8; 1 + 32 * EV_LEN];
                out[0] = n as u8;
                for i in 0..n {
                    let idx = (next + RING - n + i) % RING;
                    let e = &ring[idx];
                    let o = 1 + i * EV_LEN;
                    out[o..o + 4].copy_from_slice(&e.seq.to_le_bytes());
                    out[o + 4..o + 8].copy_from_slice(&e.at_s.to_le_bytes());
                    out[o + 8..o + 8 + PEER_LEN].copy_from_slice(&e.peer);
                    out[o + 8 + PEER_LEN] = e.op;
                    out[o + 9 + PEER_LEN] = e.kind;
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
