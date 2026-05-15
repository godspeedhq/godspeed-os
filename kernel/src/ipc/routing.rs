//! Routing table: EndpointId → (CoreId, Generation, Liveness, Queue) — §8.3.
//!
//! Every `send` syscall consults this table to validate the target endpoint's
//! generation and liveness before touching the queue. The generation here must
//! match the cap generation or the send returns `EndpointDead` (§8.7).
//!
//! SMP note (§7.8): a global spinlock serialises all routing table operations.
//! This is the "single global RwLock" approach approved for v1. The lock is
//! never held across a `block_and_reschedule` call.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message};
use crate::ipc::queue::MessageQueue;

// ---------------------------------------------------------------------------
// Spinlock — protects TABLE against concurrent access from multiple cores.
// ---------------------------------------------------------------------------

static ROUTE_LOCKED: AtomicBool = AtomicBool::new(false);

fn lock() {
    while ROUTE_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

fn unlock() {
    ROUTE_LOCKED.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Entry layout.
// ---------------------------------------------------------------------------

const MAX_ENDPOINTS: usize = 96; // raised from 64; 70 services hold recv endpoints at peak

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
    /// Task-slot of the task blocked on `recv` (waiting for a message).
    blocked_receiver: Option<usize>,
    /// Task-slot of the task blocked on `send` (queue was full).
    blocked_sender: Option<usize>,
    /// Message the blocked sender wants to deliver.
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

// SAFETY: TABLE is protected by ROUTE_LOCKED spinlock for all multi-core access.
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
///
/// Dead entries are recycled, so kill + respawn of a service does not exhaust
/// the table.
pub fn register(id: EndpointId, core_id: u32, generation: Generation) {
    lock();
    // SAFETY: lock held; single-writer.
    unsafe {
        for entry in TABLE.iter_mut() {
            if !entry.valid || entry.liveness == EndpointLiveness::Dead {
                entry.valid            = true;
                entry.id               = id;
                entry.core_id          = core_id;
                entry.generation       = generation;
                entry.liveness         = EndpointLiveness::Alive;
                entry.queue.reset();
                entry.blocked_receiver = None;
                entry.blocked_sender   = None;
                entry.pending_send     = None;
                unlock();
                return;
            }
        }
    }
    unlock();
    panic!("routing: endpoint table full (MAX_ENDPOINTS={})", MAX_ENDPOINTS);
}

/// Return the number of endpoints currently alive in the routing table.
///
/// Used by InspectKernel query 1 (P5 property test — §8.3).
pub fn count_live_endpoints() -> u32 {
    lock();
    // SAFETY: lock held; read-only scan.
    let count = unsafe {
        TABLE.iter()
            .filter(|e| e.valid && e.liveness == EndpointLiveness::Alive)
            .count() as u32
    };
    unlock();
    count
}

/// Return the current generation of `id` in the routing table, or INITIAL if not found.
///
/// Used by `spawn_service_with_config` to seed the new endpoint's generation from the
/// killed endpoint's bumped generation, ensuring monotonicity across kill/respawn (P2, §7.5).
pub fn get_generation(id: EndpointId) -> Generation {
    lock();
    // SAFETY: lock held; read-only.
    let gen = unsafe {
        TABLE.iter().find(|e| e.valid && e.id == id).map(|e| e.generation)
    };
    unlock();
    gen.unwrap_or(Generation::INITIAL)
}

/// Try to enqueue `msg` on `endpoint`.
///
/// `blocked_sender_slot`: if `Some(slot)`, this is a blocking `send` — if the
/// queue is full the sender is atomically recorded as blocked (under the same
/// lock), and the caller must immediately call `block_and_reschedule`.
/// If `None`, behaves like `try_send`: returns `Err(QueueFull)` directly.
///
/// Returns:
/// - `Ok(Some(rx))` — blocked receiver woken; caller must call `wake_by_slot`.
/// - `Ok(None)` — message queued; no blocked receiver.
/// - `Err(QueueFull)` — queue full; if `blocked_sender_slot` was `Some`, the
///   sender is now recorded as blocked and must call `block_and_reschedule`.
/// - `Err(EndpointDead)` — dead endpoint or generation mismatch.
pub fn enqueue(
    endpoint: EndpointId,
    msg: Message,
    cap_gen: Generation,
    blocked_sender_slot: Option<usize>,
) -> Result<Option<usize>, IpcError> {
    lock();
    let result = unsafe { enqueue_locked(endpoint, msg, cap_gen, blocked_sender_slot) };
    unlock();
    result
}

unsafe fn enqueue_locked(
    endpoint: EndpointId,
    msg: Message,
    cap_gen: Generation,
    blocked_sender_slot: Option<usize>,
) -> Result<Option<usize>, IpcError> {
    let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
    check_live(&TABLE[idx], cap_gen)?;

    if let Some(slot) = TABLE[idx].blocked_receiver.take() {
        // Queue was empty; a receiver was waiting — deliver directly.
        TABLE[idx].queue.enqueue(msg).ok();
        return Ok(Some(slot));
    }

    match TABLE[idx].queue.enqueue(msg) {
        Ok(()) => Ok(None),
        Err(_) => {
            // Queue full.
            if let Some(slot) = blocked_sender_slot {
                // Atomically record the sender as blocked under the same lock,
                // preventing a concurrent dequeue from missing the wakeup.
                TABLE[idx].blocked_sender = Some(slot);
                TABLE[idx].pending_send   = Some(msg);
            }
            Err(IpcError::QueueFull)
        }
    }
}

/// Try to dequeue the oldest message from `endpoint`.
///
/// `blocked_receiver_slot`: if `Some(slot)`, this is a blocking `recv` — if
/// the queue is empty the receiver is atomically recorded as blocked (under
/// the same lock), and the caller must immediately call `block_and_reschedule`.
/// If `None`, returns `Err(QueueEmpty)` directly.
///
/// Returns:
/// - `Ok((msg, Some(tx)))` — message dequeued; blocked sender to wake.
/// - `Ok((msg, None))` — message dequeued; no blocked sender.
/// - `Err(QueueEmpty)` — queue empty; if `blocked_receiver_slot` was `Some`,
///   the receiver is now recorded and must call `block_and_reschedule`.
/// - `Err(EndpointDead)` — dead endpoint or generation mismatch.
pub fn dequeue(
    endpoint: EndpointId,
    cap_gen: Generation,
    blocked_receiver_slot: Option<usize>,
) -> Result<(Message, Option<usize>), IpcError> {
    lock();
    let result = unsafe { dequeue_locked(endpoint, cap_gen, blocked_receiver_slot) };
    unlock();
    result
}

unsafe fn dequeue_locked(
    endpoint: EndpointId,
    cap_gen: Generation,
    blocked_receiver_slot: Option<usize>,
) -> Result<(Message, Option<usize>), IpcError> {
    let idx = find_index(endpoint).ok_or(IpcError::EndpointDead)?;
    check_live(&TABLE[idx], cap_gen)?;

    let msg = match TABLE[idx].queue.dequeue() {
        Some(m) => m,
        None => {
            // Queue empty.
            if let Some(slot) = blocked_receiver_slot {
                // Atomically record the receiver as blocked under the same lock.
                TABLE[idx].blocked_receiver = Some(slot);
            }
            return Err(IpcError::QueueEmpty);
        }
    };

    // If a sender was blocked, move its pending message into the freed slot.
    let sender_slot = if let Some(slot) = TABLE[idx].blocked_sender.take() {
        if let Some(pending) = TABLE[idx].pending_send.take() {
            TABLE[idx].queue.enqueue(pending).ok();
        }
        Some(slot)
    } else {
        None
    };

    Ok((msg, sender_slot))
}

/// Mark the endpoint dead: bump generation, drain queue, return blocked slots.
///
/// Returns `(blocked_receiver_slot, blocked_sender_slot)` — the caller must
/// wake both (if `Some`) with `EndpointDead` via `scheduler::wake_by_slot`.
pub fn kill_endpoint(endpoint: EndpointId) -> (Option<usize>, Option<usize>) {
    lock();
    let result = unsafe {
        let idx = match find_index(endpoint) {
            Some(i) => i,
            None    => { unlock(); return (None, None); }
        };
        TABLE[idx].liveness   = EndpointLiveness::Dead;
        TABLE[idx].generation = TABLE[idx].generation.bump();
        TABLE[idx].queue.drain();
        let rx = TABLE[idx].blocked_receiver.take();
        let tx = TABLE[idx].blocked_sender.take();
        TABLE[idx].pending_send = None;
        (rx, tx)
    };
    unlock();
    result
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Linear scan to find the index of a valid entry with the given id.
/// Caller must hold `ROUTE_LOCKED`.
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
