// SPDX-License-Identifier: GPL-2.0-only
//! IPC trace emission - the client half of the trace ring that `events` holds (`utilities/46_trace.md`).
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
//! `ipc_send = ["events"]` and the lazy resolution below found that cap - so tracing is AUTHORITY,
//! visible in `caps <service>`, revocable, and absent by default (§3.1: no ambient anything).
//!
//! # Never blocks, never fails loudly
//!
//! Emission is `try_send` and the result is discarded. An observer must not be able to slow, block or
//! break the thing it observes: a full events queue costs the emitting service nothing and loses one
//! event, which the ring counts and `trace status` reports. That is the correct trade here, and the
//! opposite of the one made on a correctness path.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Bytes of the peer's NAME carried in an event. Short on purpose: it identifies a service, and the
/// longest that matters here is `block-driver` (12).
pub const PEER_LEN: usize = 12;

/// Wire length of one event:
/// seq(4) + at_s(4, STAMPED BY THE SINK) + caller[12] + peer[12] + op(1) + kind(1).
///
/// The peer is carried as a NAME, not an endpoint or a cap slot, and that is the whole point. A cap
/// slot is local to the emitter and means nothing to a reader; an endpoint id needs a second lookup.
/// The emitter called `request_with_reply("fs", ...)` - it KNOWS the name, and a name is what a reader
/// wants. This is the "symbolic" half the requirement asked for, and it is only reachable out here:
/// the kernel may not interpret a protocol, but the service that owns one may (§4.4, §26.10).
pub const EV_LEN: usize = 4 + 4 + PEER_LEN + PEER_LEN + 1 + 1;

/// The service that holds the ring. A call to THIS peer is never traced: the reader reaches the ring
/// through it, so recording those calls would fill the ring with the reader's own questions.
pub const SINK_NAME: &str = "events";

/// Message opcodes understood by the ring in `events` (byte 0 of the payload).
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
/// The peer is ALIVE and its queue is FULL - congestion, not absence. A distinct kind because
/// answering it like a lost peer is a real bug this project has already paid for once (`net-stack`
/// reacquiring a capability that was never stale; see `DeadlineOutcome::QueueFull`).
pub const KIND_QUEUE_FULL: u8 = 5;
/// The USER abandoned the wait (`q` at a `ReqOutcome::Aborted` call). Not a failure of anything, and
/// recorded so a gap in a chain is explained rather than mysterious.
pub const KIND_ABORTED: u8 = 6;

/// The emitting service's OWN name, as it declared it (`ServiceContext::trace_as`).
///
/// # Why the caller is self-declared, and why that costs nothing
///
/// A service cannot ask what it is called: there is no name in its context page and no query for it,
/// because identity is not ambient in this system (3.1). The kernel DOES know - every syscall send
/// stamps `Message.sender_ep`, the sender's primary endpoint - but that is deliberately
/// kernel-internal ("never crosses to userspace ... so no ABI change"), and surfacing it would grow
/// the syscall surface for a diagnostic. It is not worth that.
///
/// So the caller says who it is. The objection writes itself - a self-declared name is a claim, not a
/// fact - and it does not survive contact with what the event already is: a service holding
/// `ipc_send = ["events"]` can already write any `peer` and any `outcome` it likes, because the whole
/// event is its testimony. A `caller` field is exactly as trustworthy as the two fields beside it,
/// which is to say as trustworthy as the service you granted trace authority to. It opens nothing.
///
/// Unset (a service that never called `trace_as`) reads as `?` rather than a guess.
static CALLER: spin_name::Name = spin_name::Name::new();

/// A fixed 12-byte name cell. No lock: a service is single-threaded, and this is written once at
/// startup and read on every emit.
pub mod spin_name {
    use core::sync::atomic::{AtomicU8, Ordering};
    /// A `PEER_LEN`-byte name, byte-atomic so a torn read is impossible even in principle.
    pub struct Name {
        b: [AtomicU8; super::PEER_LEN],
    }
    impl Name {
        #[allow(clippy::declare_interior_mutable_const)]
        pub const fn new() -> Self {
            const Z: AtomicU8 = AtomicU8::new(0);
            Self { b: [Z; super::PEER_LEN] }
        }
        pub fn set(&self, s: &str) {
            let n = s.len().min(super::PEER_LEN);
            for i in 0..super::PEER_LEN {
                let v = if i < n { s.as_bytes()[i] } else { 0 };
                self.b[i].store(v, Ordering::Relaxed);
            }
        }
        pub fn read_into(&self, out: &mut [u8]) {
            for i in 0..super::PEER_LEN.min(out.len()) {
                out[i] = self.b[i].load(Ordering::Relaxed);
            }
        }
        pub fn is_set(&self) -> bool { self.b[0].load(Ordering::Relaxed) != 0 }
    }
}

/// Where a given PEER's protocol keeps its opcode, when it is not byte 0.
///
/// # Why this exists
///
/// The trace recorded byte 0 of the request and called it the opcode, because that is what most
/// protocols do. Two of ours do not, and they are the two you actually see: `shell -> fs` and
/// `fs -> block-driver` both PREPEND a one-byte correlation tag (added to stop replies being matched
/// by arrival order, which had `fs` accepting one block's data as another's). So byte 0 was a request
/// id and the column was showing noise while claiming to show opcodes.
///
/// The SDK cannot know this - it is generic across every peer - and the KERNEL certainly cannot, since
/// it may not interpret a payload at all. The service that owns the protocol can, and that is exactly
/// where the rest of this design already puts protocol knowledge. So it declares it, once, per peer.
///
/// Bounded: four entries, no heap, a linear scan of at most four short names per emit. A fifth
/// declaration is dropped rather than growing - and dropped LOUDLY is not possible here (this is the
/// emit path), so the bound is set well above the two entries the system actually needs.
const OP_AT_MAX: usize = 4;
static OP_AT_NAMES: [spin_name::Name; OP_AT_MAX] = [
    spin_name::Name::new(), spin_name::Name::new(),
    spin_name::Name::new(), spin_name::Name::new(),
];
static OP_AT_OFFSETS: [AtomicU32; OP_AT_MAX] = [
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
];

/// Declare that `peer`'s protocol keeps its opcode at byte `at` of the request.
pub fn set_op_offset(peer: &str, at: u8) {
    for i in 0..OP_AT_MAX {
        if !OP_AT_NAMES[i].is_set() {
            OP_AT_NAMES[i].set(peer);
            OP_AT_OFFSETS[i].store(at as u32, Ordering::Relaxed);
            return;
        }
    }
}

/// The byte of a request to `peer` that carries its opcode. 0 unless declared otherwise.
pub fn op_offset(peer: &str) -> usize {
    let mut want = [0u8; PEER_LEN];
    let n = peer.len().min(PEER_LEN);
    want[..n].copy_from_slice(&peer.as_bytes()[..n]);
    for i in 0..OP_AT_MAX {
        if !OP_AT_NAMES[i].is_set() { break; }
        let mut have = [0u8; PEER_LEN];
        OP_AT_NAMES[i].read_into(&mut have);
        if have == want { return OP_AT_OFFSETS[i].load(Ordering::Relaxed) as usize; }
    }
    0
}

/// Declare the emitting service's own name; see [`CALLER`].
#[inline]
pub fn set_caller(name: &str) { CALLER.set(name); }

/// True once this service has declared a name.
#[inline]
pub fn caller_set() -> bool { CALLER.is_set() }

/// Whether resolution has been attempted yet. Emission arms itself LAZILY, on the first call, because
/// a service is handed a context and has no init hook to run in - so there is nowhere to arm it from.
static RESOLVED: AtomicBool = AtomicBool::new(false);
/// The cap slot for `events`: `u32::MAX` means "resolved, and this service has no events send cap", which
/// is the normal case and costs one relaxed load per call forever after.
static TRACER_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
/// Per-service event counter, so a reader can see gaps where events were dropped.
static SEQ: AtomicU32 = AtomicU32::new(0);

/// True once resolution has run - the fast path's only check on the common (untraced) call.
#[inline]
pub fn resolved() -> bool {
    RESOLVED.load(Ordering::Relaxed)
}

/// Record the outcome of resolution. `u32::MAX` means this service holds no `events` send cap, which is
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
/// Build one event. **The timestamp field is left ZERO and stamped by the SINK** - reading a clock
/// here is a CMOS RTC read on the caller's hot path (see `ServiceContext::trace_emit`), and an
/// observer that costs the observed a millisecond of port I/O per IPC is not an observer, it is a
/// brake.
pub fn encode(peer: &str, op: u8, kind: u8) -> [u8; 1 + EV_LEN] {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut b = [0u8; 1 + EV_LEN];
    b[0] = TRACE_OP_EVENT;
    b[1..5].copy_from_slice(&seq.to_le_bytes());
    CALLER.read_into(&mut b[9..9 + PEER_LEN]);
    let n = peer.len().min(PEER_LEN);
    b[9 + PEER_LEN..9 + PEER_LEN + n].copy_from_slice(&peer.as_bytes()[..n]);
    b[9 + 2 * PEER_LEN] = op;
    b[10 + 2 * PEER_LEN] = kind;
    b
}
