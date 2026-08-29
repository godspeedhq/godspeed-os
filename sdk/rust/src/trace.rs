// SPDX-License-Identifier: GPL-2.0-only
//! IPC trace emission - the client half of the trace ring that `logger` holds (`utilities/46_trace.md`).
//!
//! # Why the instrumentation is HERE and not in the kernel
//!
//! The kernel is at the routing point holding sender, receiver and endpoint, so a trace ring there
//! looks obvious. It is the wrong place, and the reason is worth stating where the code is: the kernel
//! is FORBIDDEN to know what a message means (§4.4, §26.10), so a kernel ring can only ever record
//! `endpoint 7, op 11`. A service knows its own protocol exactly, so instrumenting the SDK - the layer
//! every service already calls - is the only place `fs.read` can come from. The constraint the
//! constitution imposes and the feature the requirement wanted point the same way.
//!
//! It also means the kernel gains nothing at all: no ring, no retention policy, no message-identity
//! scheme, no control syscall, no new capability, and no write on the IPC fast path.
//!
//! # Cost when not tracing
//!
//! One `Relaxed` load, branch not taken. A service is tracing only if its contract granted it
//! `ipc_send = ["logger"]` and the lazy resolution below found that cap - so tracing is AUTHORITY,
//! visible in `caps <service>`, revocable, and absent by default (§3.1: no ambient anything).
//!
//! # Never blocks, never fails loudly
//!
//! Emission is `try_send` and the result is discarded. An observer must not be able to slow, block or
//! break the thing it observes: a full logger queue costs the emitting service nothing and loses one
//! event, which the ring counts and `trace status` reports. That is the correct trade here, and the
//! opposite of the one made on a correctness path.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Bytes of the peer's NAME carried in an event. Short on purpose: it identifies a service, and the
/// longest that matters here is `block-driver` (12).
pub const PEER_LEN: usize = 12;

/// Wire length of one event: seq(4) + at_s(4) + peer[12] + op(1) + kind(1).
///
/// The peer is carried as a NAME, not an endpoint or a cap slot, and that is the whole point. A cap
/// slot is local to the emitter and means nothing to a reader; an endpoint id needs a second lookup.
/// The emitter called `request_with_reply("fs", ...)` - it KNOWS the name, and a name is what a reader
/// wants. This is the "symbolic" half the requirement asked for, and it is only reachable out here:
/// the kernel may not interpret a protocol, but the service that owns one may (§4.4, §26.10).
pub const EV_LEN: usize = 4 + 4 + PEER_LEN + 1 + 1;

/// The service that holds the ring. A call to THIS peer is never traced: the reader reaches the ring
/// through it, so recording those calls would fill the ring with the reader's own questions.
pub const SINK_NAME: &str = "logger";

/// Message opcodes understood by the ring in `logger` (byte 0 of the payload).
pub const TRACE_OP_EVENT: u8 = 1;
/// Ask for the most recent events; byte 1 = how many are wanted.
pub const TRACE_OP_DUMP: u8 = 2;
/// Ask for ring capacity / accepted / dropped.
pub const TRACE_OP_STATUS: u8 = 3;

/// A request was sent and the caller is now awaiting a reply.
pub const KIND_REQUEST: u8 = 1;
/// A reply came back.
pub const KIND_REPLY: u8 = 2;
/// The call reached its deadline with no reply - the peer is alive but did not answer in time.
pub const KIND_TIMEOUT: u8 = 3;
/// The peer's endpoint died while the call was outstanding (`ReplyDead`, §8.6), or the send failed.
pub const KIND_PEER_LOST: u8 = 4;

/// Whether resolution has been attempted yet. Emission arms itself LAZILY, on the first call, because
/// a service is handed a context and has no init hook to run in - so there is nowhere to arm it from.
static RESOLVED: AtomicBool = AtomicBool::new(false);
/// The cap slot for `logger`: `u32::MAX` means "resolved, and this service has no logger send cap", which
/// is the normal case and costs one relaxed load per call forever after.
static TRACER_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
/// Per-service event counter, so a reader can see gaps where events were dropped.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// True once resolution has run - the fast path's only check on the common (untraced) call.
#[inline]
pub fn resolved() -> bool {
    RESOLVED.load(Ordering::Relaxed)
}

/// Record the outcome of resolution. `u32::MAX` means this service holds no `logger` send cap, which is
/// remembered so the lookup happens exactly once per service lifetime.
#[inline]
pub fn set_sink_slot(slot: u32) {
    TRACER_SLOT.store(slot, Ordering::Relaxed);
    RESOLVED.store(true, Ordering::Release);
}

/// The cap slot to emit on, or `u32::MAX` if this service is not tracing.
#[inline]
pub fn sink_slot() -> u32 {
    TRACER_SLOT.load(Ordering::Relaxed)
}

/// Build one event payload. Split out from the send so the caller owns the buffer and this stays
/// allocation-free (§26.6.1 - fixed stack, no heap).
#[inline]
pub fn encode(at_s: u32, peer: &str, op: u8, kind: u8) -> [u8; 1 + EV_LEN] {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut b = [0u8; 1 + EV_LEN];
    b[0] = TRACE_OP_EVENT;
    b[1..5].copy_from_slice(&seq.to_le_bytes());
    b[5..9].copy_from_slice(&at_s.to_le_bytes());
    let n = peer.len().min(PEER_LEN);
    b[9..9 + n].copy_from_slice(&peer.as_bytes()[..n]);
    b[9 + PEER_LEN] = op;
    b[10 + PEER_LEN] = kind;
    b
}
