//! Routing table: EndpointId → (CoreId, Generation, Liveness, Queue) — §8.3.
//!
//! Every `send` syscall consults this table to validate the target endpoint's
//! generation and liveness before touching the queue. The generation here must
//! match the cap generation or the send returns `EndpointDead` (§8.7).
//!
//! v1: a simple linear-scan table of up to MAX_ENDPOINTS entries, protected
//! by IF=0 (syscall path) on the same core. SMP locking is Milestone 6.

use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message};
use crate::ipc::queue::MessageQueue;

// ---------------------------------------------------------------------------
// Entry layout.
// ---------------------------------------------------------------------------

const MAX_ENDPOINTS: usize = 16;

/// Per-endpoint liveness (distinct from the global cap-table Liveness, which
/// tracks the resource registration; this tracks the queue's own state).
#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointLiveness {
    Alive,
    Dead,
}

/// One row of the routing table.
///
/// Derives Copy only so it can be used in the const-initialised static below.
/// Never shallow-copy a live entry.
#[derive(Copy, Clone)]
struct RoutingEntry {
    valid: bool,
    id: EndpointId,
    core_id: u32,
    generation: Generation,
    liveness: EndpointLiveness,
    queue: MessageQueue,
    /// Task-slot of the task blocked on `recv` (waiting for a message to arrive).
    blocked_receiver: Option<usize>,
    /// Task-slot of the task blocked on `send` (queue was full).
    blocked_sender: Option<usize>,
    /// The message the blocked sender wants to deliver; held until a dequeue
    /// frees space and the sender is woken.
    pending_send: Option<Message>,
}

impl RoutingEntry {
    const fn empty() -> Self {
        Self {
            valid: false,
            id: EndpointId(0),
            core_id: 0,
            generation: Generation::INITIAL,
            liveness: EndpointLiveness::Alive,
            queue: MessageQueue::new(),
            blocked_receiver: None,
            blocked_sender: None,
            pending_send: None,
        }
    }
}

// SAFETY: TABLE is accessed exclusively from syscall context (IF=0, same core)
// in v1. No concurrent modification from other cores in Milestone 5.
static mut TABLE: [RoutingEntry; MAX_ENDPOINTS] = {
    const E: RoutingEntry = RoutingEntry::empty();
    [E; MAX_ENDPOINTS]
};

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

pub fn init() {
    // Static is zero-initialised; nothing to do in v1.
}

/// Register a newly-created endpoint in the routing table.
pub fn register(id: EndpointId, core_id: u32, generation: Generation) {
    // SAFETY: called before scheduling starts (single-core, IF=0 context).
    unsafe {
        for entry in TABLE.iter_mut() {
            if !entry.valid {
                entry.valid      = true;
                entry.id         = id;
                entry.core_id    = core_id;
                entry.generation = generation;
                entry.liveness   = EndpointLiveness::Alive;
                return;
            }
        }
        panic!("routing: endpoint table full (MAX_ENDPOINTS={})", MAX_ENDPOINTS);
    }
}

/// Try to enqueue `msg` on `endpoint`, checking `cap_gen` against the stored
/// generation.
///
/// Returns:
/// - `Ok(Some(receiver_slot))` — a task was blocked on `recv`; caller must
///   wake it via `scheduler::wake_by_slot`.
/// - `Ok(None)` — message queued; no blocked receiver.
/// - `Err(QueueFull)` — queue is full; caller should record itself as a
///   blocked sender (§8.9) and call `scheduler::block_and_reschedule`.
/// - `Err(EndpointDead)` — endpoint is dead or generation mismatch.
pub fn enqueue(
    endpoint: EndpointId,
    msg: Message,
    cap_gen: Generation,
) -> Result<Option<usize>, IpcError> {
    // SAFETY: IF=0 in syscall context.
    unsafe {
        let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
        check_live(&TABLE[idx], cap_gen)?;

        // If a receiver is already blocked, deliver directly into the queue
        // (queue was empty — receiver was waiting for exactly this).
        if let Some(slot) = TABLE[idx].blocked_receiver.take() {
            // Queue must be empty when a receiver is blocked, so enqueue is
            // infallible here.
            TABLE[idx].queue.enqueue(msg).ok();
            return Ok(Some(slot));
        }

        match TABLE[idx].queue.enqueue(msg) {
            Ok(()) => Ok(None),
            Err(_) => Err(IpcError::QueueFull),
        }
    }
}

/// Record that `slot` is blocked trying to deliver `msg` (queue was full).
///
/// Called after `enqueue` returns `Err(QueueFull)` and before
/// `block_and_reschedule`. Interrupts must be disabled by the caller to prevent
/// a concurrent dequeue from missing the wakeup.
pub fn record_blocked_sender(
    endpoint: EndpointId,
    slot: usize,
    msg: Message,
) -> Result<(), IpcError> {
    // SAFETY: IF=0 (caller ensures this).
    unsafe {
        let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
        if TABLE[idx].liveness == EndpointLiveness::Dead {
            return Err(IpcError::EndpointDead);
        }
        TABLE[idx].blocked_sender  = Some(slot);
        TABLE[idx].pending_send    = Some(msg);
        Ok(())
    }
}

/// Record that `slot` is blocked waiting for a message (queue was empty).
///
/// Interrupts must be disabled by the caller to prevent a concurrent enqueue
/// from missing the wakeup.
pub fn record_blocked_receiver(endpoint: EndpointId, slot: usize) -> Result<(), IpcError> {
    // SAFETY: IF=0 (caller ensures this).
    unsafe {
        let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
        if TABLE[idx].liveness == EndpointLiveness::Dead {
            return Err(IpcError::EndpointDead);
        }
        TABLE[idx].blocked_receiver = Some(slot);
        Ok(())
    }
}

/// Dequeue the oldest message from `endpoint`, checking `cap_gen`.
///
/// Returns:
/// - `Ok((msg, Some(sender_slot)))` — message dequeued; a sender was blocked.
///   Caller must wake it via `scheduler::wake_by_slot` and the sender's
///   pending message is now in the queue.
/// - `Ok((msg, None))` — message dequeued; no blocked sender.
/// - `Err(QueueEmpty)` — queue empty; caller should record itself as a
///   blocked receiver and call `scheduler::block_and_reschedule`.
/// - `Err(EndpointDead)` — endpoint dead or generation mismatch.
pub fn dequeue(
    endpoint: EndpointId,
    cap_gen: Generation,
) -> Result<(Message, Option<usize>), IpcError> {
    // SAFETY: IF=0 in syscall context.
    unsafe {
        let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
        check_live(&TABLE[idx], cap_gen)?;

        let msg = match TABLE[idx].queue.dequeue() {
            Some(m) => m,
            None    => return Err(IpcError::QueueEmpty),
        };

        // If a sender was blocked waiting for space, move its pending message
        // into the now-freed slot and return the sender slot to wake.
        let sender_slot = if let Some(slot) = TABLE[idx].blocked_sender.take() {
            if let Some(pending) = TABLE[idx].pending_send.take() {
                // Space just freed; enqueue can't fail.
                TABLE[idx].queue.enqueue(pending).ok();
            }
            Some(slot)
        } else {
            None
        };

        Ok((msg, sender_slot))
    }
}

/// Mark the endpoint dead: bump generation, drain queue, clear blocked tasks.
///
/// Returns `(blocked_receiver_slot, blocked_sender_slot)` — the caller must
/// wake both (if `Some`) with `EndpointDead` via `scheduler::wake_by_slot`.
pub fn kill_endpoint(endpoint: EndpointId) -> (Option<usize>, Option<usize>) {
    // SAFETY: IF=0.
    unsafe {
        let idx = match find_index(endpoint) {
            Some(i) => i,
            None    => return (None, None),
        };
        TABLE[idx].liveness   = EndpointLiveness::Dead;
        TABLE[idx].generation = TABLE[idx].generation.bump();
        TABLE[idx].queue.drain();
        let rx = TABLE[idx].blocked_receiver.take();
        let tx = TABLE[idx].blocked_sender.take();
        TABLE[idx].pending_send = None;
        (rx, tx)
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Linear scan to find the index of a valid entry with the given id.
unsafe fn find_index(id: EndpointId) -> Option<usize> {
    for (i, entry) in unsafe { TABLE.iter().enumerate() } {
        if entry.valid && entry.id == id {
            return Some(i);
        }
    }
    None
}

fn check_live(entry: &RoutingEntry, cap_gen: Generation) -> Result<(), IpcError> {
    if entry.liveness == EndpointLiveness::Dead {
        return Err(IpcError::EndpointDead);
    }
    if !cap_gen.matches(entry.generation) {
        return Err(IpcError::EndpointDead);
    }
    Ok(())
}
