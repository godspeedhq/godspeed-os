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

use core::sync::atomic::{Ordering};
use portable_atomic::AtomicU64;

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
// Synchronous-CALL reply tracking (§8.6 reply-side death-wake; Commandment VIII).
// ---------------------------------------------------------------------------

/// The endpoint id each task is blocked-in-CALL awaiting a reply from (0 = no in-flight call),
/// indexed by the caller's task slot. **Bounded**: exactly one entry per task - a blocked task has at
/// most one in-flight call - so it never grows (no allocation, no list).
///
/// This is the reply-side twin of `RoutingEntry::blocked_sender`. When a task makes a synchronous
/// CALL (send request + block awaiting the reply), it records the **target** endpoint here. If that
/// endpoint later dies, `take_call_waiter` finds the caller and the kill path wakes it with
/// `ReplyDead` - exactly as `kill_endpoint` returns a blocked *sender* to wake with `EndpointDead`.
///
/// Race-freedom mirrors `blocked_sender` (no new lock): every read and write below happens while the
/// `TABLE` lock is held, so registration (`call_dequeue`) and the death scan (`take_call_waiter`) are
/// mutually exclusive and ordered with `kill_endpoint`'s liveness bump. A registration only happens
/// after observing the target **alive** under the lock; once `kill_endpoint` has marked it dead, a
/// later registration refuses (returns `ReplyDead`) - so no caller can register-then-miss the wake.
/// `AtomicU64` (not `static mut`) keeps `ipc/` unsafe-free (see ipc/CLAUDE.md).
static CALL_AWAIT_EP: [AtomicU64; crate::task::scheduler::MAX_TASKS] =
    [const { AtomicU64::new(0) }; crate::task::scheduler::MAX_TASKS];

/// Record that `caller_slot` is now blocked-in-CALL awaiting a reply from `target` (TABLE held).
#[inline]
fn set_call_await(caller_slot: usize, target: EndpointId) {
    if caller_slot < CALL_AWAIT_EP.len() {
        CALL_AWAIT_EP[caller_slot].store(target.0, Ordering::Relaxed);
    }
}

/// The endpoint `slot` is blocked-in-CALL awaiting a reply from, or 0 if it has no call in flight.
///
/// **Reads state the kernel already keeps for correctness; it records nothing new.** `CALL_AWAIT_EP`
/// exists so a dead replier wakes its caller with `ReplyDead` (§8.6). That same record answers "why is
/// this task not progressing?", because the chain of who-awaits-whom IS the causal chain - so the
/// The `events` views read this; the kernel is not a tracer (`utilities/46_events.md`).
///
/// Relaxed, and deliberately so: this is a best-effort snapshot for an operator, on the same contract
/// as `task_stat`. A value read the instant before the awaited endpoint replies is stale, and that is
/// acceptable - the alternative is taking the `TABLE` lock on an introspection path, which would let
/// an observer perturb the thing it observes.
pub fn call_await_endpoint(slot: usize) -> u64 {
    if slot < CALL_AWAIT_EP.len() {
        CALL_AWAIT_EP[slot].load(Ordering::Relaxed)
    } else {
        0
    }
}

/// Clear `caller_slot`'s outstanding-call record (TABLE held, or a lone store on the kill path).
#[inline]
fn clear_call_await_inner(caller_slot: usize) {
    if caller_slot < CALL_AWAIT_EP.len() {
        CALL_AWAIT_EP[caller_slot].store(0, Ordering::Relaxed);
    }
}

/// Clear a task's outstanding-call record. Called from the task-kill path so a dying caller's stale
/// entry cannot, after its slot is reused, cause a future `take_call_waiter` to spuriously wake the
/// slot's new occupant. A lone relaxed store is sufficient: the slot is not reused until `TASK_VALID`
/// is set false later in the kill path (with its own release fence), and a stale entry is otherwise
/// harmless (`wake_by_slot` guards on `TASK_VALID`).
pub fn clear_call_await(caller_slot: usize) {
    clear_call_await_inner(caller_slot);
}

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
        // BOOT-TIME CALLERS ONLY. Spawning a task goes through `try_register` and REFUSES the spawn
        // when the table is full, because a chaos kill storm reaching this panic took the whole
        // machine down - and nothing above the kernel may do that. What is left here is the boot
        // path, where the table is empty by construction and a failure is fatal by §11.3 anyway. If
        // a new runtime caller ever needs an endpoint, it uses `try_register` and handles `false`.
        None => panic!("routing: endpoint table full at boot (MAX_ENDPOINTS={})", MAX_ENDPOINTS),
    }
}

/// `register`, but returns `false` instead of panicking when the table is full.
///
/// For endpoints a task can do WITHOUT. The primary endpoint is not one of those - a service with no
/// mailbox cannot be talked to, and failing quietly there would produce a service that exists and
/// answers nothing, so that path still panics. The reply-only endpoint IS optional: without it a task
/// falls back to awaiting replies on its shared endpoint, which is what every task did until now.
///
/// The distinction matters because the table is sized for services and the probe builds spawn ~178 of
/// them. Handing every task a second endpoint unconditionally would have taken `osdev test identity`
/// from working to a boot panic - the table holds 96.
/// How many table slots stay reserved for endpoints a service CANNOT do without.
///
/// A service's own receive endpoint is mandatory - without one it cannot be spawned at all. The
/// reply-only endpoint is a convenience: a task that has none simply awaits replies on its shared
/// endpoint, which is what every task did before reply endpoints existed.
///
/// Those two were competing for the same slots on equal terms, and the optional one was winning
/// because it is taken while slots remain. The result was a MANDATORY registration failing - "spawn
/// REFUSED - IPC routing table full" - while optional endpoints held slots they could have done
/// without. That is what broke property P5 (§8.3): under the concurrent spawn load of a probe
/// build, real services could not be spawned.
///
/// Enlarging the table is the wrong answer: each entry carries a 16-deep queue of 4 KiB messages,
/// so 96 entries is already about 6 MB of static footprint (§26.6 - the bound must stay visible and
/// affordable). Reserving part of it costs nothing and fixes the priority inversion directly.
/// Sized from the table's OWN stated peak, one line up: "70 services hold recv endpoints at peak".
/// A reserve smaller than that peak does not reserve anything - it just moves the failure later,
/// which a quarter-table reserve did: P5 passed and P7 still failed with the same "routing table
/// full" refusal, because optional endpoints could still take 72 of the 96.
const OPTIONAL_RESERVE: usize = MAX_ENDPOINTS * 3 / 4;

/// How many optional registrations have been refused. Diagnostic only - owned here, where the
/// refusal happens, so the count cannot disagree with the decision.
static REFUSED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// `try_register` for an endpoint the caller can do WITHOUT.
///
/// Refuses once free slots fall to the reserve, so a convenience can never consume what a mandatory
/// registration needs. Degraded, not broken - and degraded in the direction that keeps services
/// spawnable.
pub fn try_register_optional(id: EndpointId, core_id: u32, generation: Generation) -> bool {
    // Count under the lock, decide and REPORT outside it. A serial write is ~9 ms on the ARM ports
    // and it is not preemptible, so logging while holding the routing table would stall every IPC on
    // the machine for the duration - the routing table is on the path of every send. The scope here
    // is deliberate and not stylistic.
    let free = {
        let table = TABLE.lock_irq();
        table.iter()
            .filter(|e| !e.valid || e.liveness == EndpointLiveness::Dead)
            .count()
    };
    if free <= OPTIONAL_RESERVE {
        // LOUD, because a silent refusal leaves no trace of a real degradation (invariant 12). The
        // caller does fall back correctly - it awaits replies on its shared endpoint - but that
        // fallback is the very hazard the reply mailbox exists to remove (a service cannot drain
        // client traffic while waiting on the endpoint it also serves), so it is a fact an operator
        // needs rather than an implementation detail.
        //
        // IT FIRES. This said "never observed firing" on the strength of the shell, identity and
        // property suites and a full arm32 boot - and that is no longer true: a bare-metal T630 boot
        // refuses three times during `selfcheck` (71 of 96 free against a reserve of 72), and the
        // probe-heavy builds refuse considerably more. The claim is corrected rather than deleted,
        // because the log did its job: it turned a suspicion into a measurement, which is what it was
        // added for.
        //
        // The threshold is still unchanged, deliberately. What the evidence shows is a reserve that
        // is tight for a system with ~25 live endpoints, not one that is starving anything: the
        // refused callers fall back to awaiting replies on their own endpoint and the T630 run was
        // 377/0 with no panic. Raising it is a real change with its own measurement, not a reflex.
        let n = REFUSED.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
        if n <= 3 || n % 64 == 0 {
            crate::kprintln!(
                "routing: reply endpoint refused - {} of {} slots free, reserve {} ({} refused so far)",
                free, MAX_ENDPOINTS, OPTIONAL_RESERVE, n);
        }
        return false;
    }
    try_register(id, core_id, generation)
}

pub fn try_register(id: EndpointId, core_id: u32, generation: Generation) -> bool {
    let mut table = TABLE.lock_irq();
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
            true
        }
        None => false,
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

/// Dequeue the oldest queued message on `endpoint` that was SENT BY the task owning `target` - the
/// only message a blocked `Call` may take as its reply. Everything else (an unrelated client's
/// request, a kernel notification, a stale reply from a dead incarnation whose endpoint id differs)
/// stays queued, in order, for the ordinary `recv` loop. Same liveness check and blocked-sender
/// promotion as `dequeue_locked`; never registers a blocked receiver (the caller does that itself,
/// under the same lock, in `call_dequeue` step 3).
fn dequeue_reply_locked(
    table: &mut [RoutingEntry; MAX_ENDPOINTS],
    endpoint: EndpointId,
    cap_gen: Generation,
    target: EndpointId,
) -> Result<(Message, Option<usize>), IpcError> {
    let idx = find_index(table, endpoint).ok_or(IpcError::EndpointDead)?;
    check_live(&table[idx], cap_gen)?;
    let msg = match table[idx].queue.dequeue_matching(target.0) {
        Some(m) => m,
        None => return Err(IpcError::QueueEmpty),
    };
    // A slot was freed: promote a blocked sender's pending message, exactly as dequeue_locked does.
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

/// Dequeue a CALL reply, or register the caller as blocked-in-CALL awaiting `target` (§8.6
/// reply-side death-wake). Like `dequeue` on the caller's own endpoint `recv_ep`, but with the
/// reply-side liveness guarantee that closes the hang:
///
/// 1. If the REPLY is already queued on `recv_ep`, return it (`Ok`) - a delivered reply always wins,
///    even if `target` has since died. Only a message stamped as sent by `target`'s owner counts
///    (`dequeue_reply_locked`): the caller's endpoint also receives unrelated requests, and taking
///    the bare queue head handed a blocked Call whichever message arrived first - the protocol
///    desync behind the "MALFORMED reply" line and the false root-block CRC failures.
/// 2. Else, if `target` (the would-be replier) is already dead, return `Err(ReplyDead)` at once -
///    the reply can never come, so the caller must not block.
/// 3. Else, record the caller as `blocked_receiver` of `recv_ep` AND register the outstanding call
///    against `target`, then return `Err(QueueEmpty)` so the caller blocks. A subsequent death of
///    `target` is caught by `take_call_waiter` (called from the kill path) which wakes the caller.
///
/// All three steps run under the one `TABLE` lock, so they are atomic with respect to
/// `kill_endpoint(target)` - the registration-vs-death race is closed exactly as it is for a blocked
/// sender. `Err(EndpointDead)` here means the caller's *own* endpoint died (it is being killed).
pub fn call_dequeue(
    recv_ep: EndpointId,
    recv_gen: Generation,
    target: EndpointId,
    caller_slot: usize,
) -> Result<(Message, Option<usize>), IpcError> {
    let mut table = TABLE.lock_irq();

    // 1. Take an already-delivered REPLY - and only a reply - without registering as blocked.
    match dequeue_reply_locked(&mut *table, recv_ep, recv_gen, target) {
        Ok(got) => {
            clear_call_await_inner(caller_slot);
            return Ok(got);
        }
        Err(IpcError::QueueEmpty) => {}                 // fall through to block-or-die
        Err(e) => {                                     // our own endpoint died, etc.
            clear_call_await_inner(caller_slot);
            return Err(e);
        }
    }

    // 2. Queue empty: if the would-be replier is already dead, the reply can never arrive.
    let target_alive = matches!(
        find_index(&*table, target),
        Some(i) if table[i].liveness == EndpointLiveness::Alive
    );
    if !target_alive {
        clear_call_await_inner(caller_slot);
        return Err(IpcError::ReplyDead);
    }

    // 3. Register as blocked receiver of our own endpoint AND record the outstanding call, both under
    //    this lock - ordered with kill_endpoint(target) + take_call_waiter (the death-wake).
    if let Some(j) = find_index(&*table, recv_ep) {
        table[j].blocked_receiver = Some(caller_slot);
    }
    set_call_await(caller_slot, target);
    Err(IpcError::QueueEmpty)
}

/// Pop one task slot blocked-in-CALL awaiting a reply from `dead_ep`, clearing its record; `None`
/// when none remain. Called repeatedly from the task-kill path (after `kill_endpoint` has marked
/// `dead_ep` dead) to wake every such caller with `ReplyDead` - the reply-side twin of the
/// blocked-sender wake `kill_endpoint` returns. Bounded: at most one entry per task, so the kill
/// path's drain loop runs at most `MAX_TASKS` times. Holding `TABLE` orders this scan after every
/// registration that observed `dead_ep` alive (those happened before the liveness bump), and any
/// registration racing in after the bump refuses (sees `dead_ep` dead in `call_dequeue` step 2).
pub fn take_call_waiter(dead_ep: EndpointId) -> Option<usize> {
    let _table = TABLE.lock_irq(); // ordering with call_dequeue registration; guards the scan
    for slot in 0..CALL_AWAIT_EP.len() {
        if CALL_AWAIT_EP[slot].load(Ordering::Relaxed) == dead_ep.0 {
            CALL_AWAIT_EP[slot].store(0, Ordering::Relaxed);
            return Some(slot);
        }
    }
    None
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

/// Kernel-originated delivery that applies BACK-PRESSURE instead of dropping.
///
/// Same shape as [`enqueue_from_interrupt`] - no capability, no generation check, because the caller is
/// the kernel rather than a task holding a cap - but on a full queue it records `sender_slot` as a
/// blocked sender and returns `QueueFull`, exactly as a userspace `send` does. The caller must then
/// immediately `block_and_reschedule(BlockedOnSend)`.
///
/// This exists for the console path. Dropping on a full queue is right for an INTERRUPT (the event has
/// already happened and the driver is behind), and wrong for console output: a queue 16 deep cannot hold
/// a burst of thirty writes no matter how fast the renderer is, so the excess was lost every time
/// regardless of speed. Blocking the WRITER is the bounded-queue contract working as designed (§8.5,
/// §8.6) - the kernel itself never waits, the task that produced the output does, and it wakes with
/// `EndpointDead` if the terminal dies rather than hanging.
pub fn enqueue_from_kernel_blocking(
    endpoint: EndpointId,
    msg: Message,
    sender_slot: usize,
) -> Result<Option<usize>, IpcError> {
    let mut table = TABLE.lock_irq();
    let idx = find_index(&*table, endpoint).ok_or(IpcError::EndpointDead)?;
    if table[idx].liveness == EndpointLiveness::Dead {
        return Err(IpcError::EndpointDead);
    }
    if let Some(slot) = table[idx].blocked_receiver.take() {
        table[idx].queue.enqueue(msg).ok();
        return Ok(Some(slot));
    }
    match table[idx].queue.enqueue(msg) {
        Ok(()) => Ok(None),
        Err(_) => {
            table[idx].blocked_sender = Some(sender_slot);
            table[idx].pending_send = Some(msg);
            Err(IpcError::QueueFull)
        }
    }
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
