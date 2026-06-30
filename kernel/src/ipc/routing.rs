// SPDX-License-Identifier: GPL-2.0-only
//! Routing table: EndpointId → (CoreId, Generation, Liveness, Queue) - §8.3.
//!
//! Every `send` syscall consults this table to validate the target endpoint's
//! generation and liveness before touching the queue. The generation here must
//! match the cap generation or the send returns `EndpointDead` (§8.7).
//!
//! SMP note (§7.8): a global spinlock serialises all routing table operations.
//! This is the "single global RwLock" approach approved for v1. The lock is
//! never held across a `block_and_reschedule` call.

use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::{IpcError, Message};
use crate::ipc::queue::MessageQueue;
use crate::smp::SpinLock;

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

// `SpinLock::ZEROED` keeps the `unsafe` zeroing in smp/spinlock.rs (a permitted
// layer, §18.1); ipc/ stays unsafe-free (see ipc/CLAUDE.md). The all-zeroes value
// is valid here: every RoutingEntry has valid=false, liveness=Alive(discriminant
// 0), generation=0, queue slots=None, blocked fields=None; lock=AtomicBool(false).
// This avoids the undef padding bytes that LLD rejects for a `.bss` symbol; Limine
// zeroes `.bss` before entry.
#[link_section = ".bss"]
static TABLE: SpinLock<[RoutingEntry; MAX_ENDPOINTS]> = SpinLock::ZEROED;

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
    let mut table = TABLE.lock_irq();
    // Endpoint ids are reclaimed and reused (ipc::free_endpoint_id, §14.2). Prefer THIS id's own
    // (now-dead) entry, so a reused id overwrites its old slot instead of creating a *second* entry
    // with the same id - `find_index` returns the first match, so a duplicate would be ambiguous.
    // Fall back to any free/dead slot for a never-seen id.
    let slot = table.iter().position(|e| e.valid && e.id == id)
        .or_else(|| table.iter().position(|e| !e.valid || e.liveness == EndpointLiveness::Dead));
    match slot {
        Some(idx) => {
            let entry = &mut table[idx];
            entry.valid            = true;
            entry.id               = id;
            entry.core_id          = core_id;
            entry.generation       = generation;
            entry.liveness         = EndpointLiveness::Alive;
            entry.queue.reset();
            entry.blocked_receiver = None;
            entry.blocked_sender   = None;
            entry.pending_send     = None;
        }
        None => panic!("routing: endpoint table full (MAX_ENDPOINTS={})", MAX_ENDPOINTS),
    }
}

/// Return the number of endpoints currently alive in the routing table.
///
/// Used by InspectKernel query 1 (P5 property test - §8.3).
pub fn count_live_endpoints() -> u32 {
    let table = TABLE.lock_irq();
    table.iter()
        .filter(|e| e.valid && e.liveness == EndpointLiveness::Alive)
        .count() as u32
}

/// Return the current generation of `id` in the routing table, or INITIAL if not found.
///
/// Used by `spawn_service_with_config` to seed the new endpoint's generation from the
/// killed endpoint's bumped generation, ensuring monotonicity across kill/respawn (P2, §7.5).
pub fn get_generation(id: EndpointId) -> Generation {
    let table = TABLE.lock_irq();
    table.iter()
        .find(|e| e.valid && e.id == id)
        .map(|e| e.generation)
        .unwrap_or(Generation::INITIAL)
}

/// Try to enqueue `msg` on `endpoint`.
///
/// `blocked_sender_slot`: if `Some(slot)`, this is a blocking `send` - if the
/// queue is full the sender is atomically recorded as blocked (under the same
/// lock), and the caller must immediately call `block_and_reschedule`.
/// If `None`, behaves like `try_send`: returns `Err(QueueFull)` directly.
///
/// Returns:
/// - `Ok(Some(rx))` - blocked receiver woken; caller must call `wake_by_slot`.
/// - `Ok(None)` - message queued; no blocked receiver.
/// - `Err(QueueFull)` - queue full; if `blocked_sender_slot` was `Some`, the
///   sender is now recorded as blocked and must call `block_and_reschedule`.
/// - `Err(EndpointDead)` - dead endpoint or generation mismatch.
pub fn enqueue(
    endpoint: EndpointId,
    msg: Message,
    cap_gen: Generation,
    blocked_sender_slot: Option<usize>,
) -> Result<Option<usize>, IpcError> {
    let mut table = TABLE.lock_irq();
    enqueue_locked(&mut *table, endpoint, msg, cap_gen, blocked_sender_slot)
}

fn enqueue_locked(
    table: &mut [RoutingEntry; MAX_ENDPOINTS],
    endpoint: EndpointId,
    msg: Message,
    cap_gen: Generation,
    blocked_sender_slot: Option<usize>,
) -> Result<Option<usize>, IpcError> {
    let idx = find_index(table, endpoint).ok_or(IpcError::EndpointDead)?;
    check_live(&table[idx], cap_gen)?;

    if let Some(slot) = table[idx].blocked_receiver.take() {
        // Queue was empty; a receiver was waiting - deliver directly.
        table[idx].queue.enqueue(msg).ok();
        return Ok(Some(slot));
    }

    match table[idx].queue.enqueue(msg) {
        Ok(()) => Ok(None),
        Err(_) => {
            // Queue full.
            if let Some(slot) = blocked_sender_slot {
                // Atomically record the sender as blocked under the same lock,
                // preventing a concurrent dequeue from missing the wakeup.
                table[idx].blocked_sender = Some(slot);
                table[idx].pending_send   = Some(msg);
            }
            Err(IpcError::QueueFull)
        }
    }
}

/// Try to dequeue the oldest message from `endpoint`.
///
/// `blocked_receiver_slot`: if `Some(slot)`, this is a blocking `recv` - if
/// the queue is empty the receiver is atomically recorded as blocked (under
/// the same lock), and the caller must immediately call `block_and_reschedule`.
/// If `None`, returns `Err(QueueEmpty)` directly.
///
/// Returns:
/// - `Ok((msg, Some(tx)))` - message dequeued; blocked sender to wake.
/// - `Ok((msg, None))` - message dequeued; no blocked sender.
/// - `Err(QueueEmpty)` - queue empty; if `blocked_receiver_slot` was `Some`,
///   the receiver is now recorded and must call `block_and_reschedule`.
/// - `Err(EndpointDead)` - dead endpoint or generation mismatch.
pub fn dequeue(
    endpoint: EndpointId,
    cap_gen: Generation,
    blocked_receiver_slot: Option<usize>,
) -> Result<(Message, Option<usize>), IpcError> {
    let mut table = TABLE.lock_irq();
    dequeue_locked(&mut *table, endpoint, cap_gen, blocked_receiver_slot)
}

fn dequeue_locked(
    table: &mut [RoutingEntry; MAX_ENDPOINTS],
    endpoint: EndpointId,
    cap_gen: Generation,
    blocked_receiver_slot: Option<usize>,
) -> Result<(Message, Option<usize>), IpcError> {
    let idx = find_index(table, endpoint).ok_or(IpcError::EndpointDead)?;
    check_live(&table[idx], cap_gen)?;

    let msg = match table[idx].queue.dequeue() {
        Some(m) => m,
        None => {
            // Queue empty.
            if let Some(slot) = blocked_receiver_slot {
                // Atomically record the receiver as blocked under the same lock.
                table[idx].blocked_receiver = Some(slot);
            }
            return Err(IpcError::QueueEmpty);
        }
    };

    // If a sender was blocked, move its pending message into the freed slot.
    let sender_slot = if let Some(slot) = table[idx].blocked_sender.take() {
        if let Some(pending) = table[idx].pending_send.take() {
            table[idx].queue.enqueue(pending).ok();
        }
        Some(slot)
    } else {
        None
    };

    Ok((msg, sender_slot))
}

/// Kernel-internal interrupt delivery path. No capability or generation check -
/// the caller is the kernel IDT, not a user task holding a capability.
///
/// Try-send semantics: if the queue is full the interrupt is silently discarded
/// (driver overloaded; the APIC EOI still fires unconditionally in the caller).
///
/// Returns the blocked receiver slot if a task was waiting on `recv`, so the
/// caller can call `scheduler::wake_by_slot` (which handles the cross-core IPI).
pub fn enqueue_from_interrupt(endpoint: EndpointId, msg: Message) -> Option<usize> {
    let mut table = TABLE.lock_irq();
    let idx = find_index(&*table, endpoint)?;

    if table[idx].liveness == EndpointLiveness::Dead {
        return None;
    }

    if let Some(slot) = table[idx].blocked_receiver.take() {
        table[idx].queue.enqueue(msg).ok();
        return Some(slot);
    }

    table[idx].queue.enqueue(msg).ok();
    None
}

/// Returns `true` if `endpoint` is registered and alive in the routing table.
///
/// Used by `invariants::assertions::assert_tcb_alive` (§6.2).
pub fn is_endpoint_alive(endpoint: EndpointId) -> bool {
    let table = TABLE.lock_irq();
    table.iter().any(|e| e.valid && e.id == endpoint && e.liveness == EndpointLiveness::Alive)
}

/// Return the current queue depth for `endpoint`, or 0 if not found.
pub fn endpoint_queue_depth(endpoint: EndpointId) -> u8 {
    let table = TABLE.lock_irq();
    table.iter()
        .find(|e| e.valid && e.id == endpoint)
        .map(|e| e.queue.depth() as u8)
        .unwrap_or(0)
}

/// Mark the endpoint dead: bump generation, drain queue, return blocked slots.
///
/// Returns `(blocked_receiver_slot, blocked_sender_slot)` - the caller must
/// wake both (if `Some`) with `EndpointDead` via `scheduler::wake_by_slot`.
pub fn kill_endpoint(endpoint: EndpointId) -> (Option<usize>, Option<usize>) {
    let mut table = TABLE.lock_irq();
    let idx = match find_index(&*table, endpoint) {
        Some(i) => i,
        None    => return (None, None),
    };
    table[idx].liveness   = EndpointLiveness::Dead;
    table[idx].generation = table[idx].generation.bump();
    table[idx].queue.drain();
    let rx = table[idx].blocked_receiver.take();
    let tx = table[idx].blocked_sender.take();
    table[idx].pending_send = None;
    (rx, tx)
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Linear scan to find the index of a valid entry with the given id.
fn find_index(table: &[RoutingEntry; MAX_ENDPOINTS], id: EndpointId) -> Option<usize> {
    for (i, entry) in table.iter().enumerate() {
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
