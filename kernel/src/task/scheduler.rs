//! Per-core round-robin scheduler — §9.1, §9.3.
//!
//! Each core has a static run queue of up to MAX_TASKS slots.  Tasks are
//! pinned to cores at enqueue time and never migrate (§9.1).  The 10 ms
//! preemption quantum is enforced by the local APIC timer; `yield` is
//! advisory (§9.3).
//!
//! Cross-core wakeups (§8.4, §9.4): `wake_by_slot` sends a WAKE_RECEIVER
//! IPI when the target task lives on a different core.

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::arch::x86_64::context_switch::{switch_context, TaskContext};
use crate::capability::cap::{CapError, Capability};
use crate::capability::rights::Rights;
use crate::capability::table::CapTable;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::Message;
use crate::task::state::TaskState;

// ---------------------------------------------------------------------------
// Flat task table (all cores share one array; tasks are pinned by TASK_CORE).
// ---------------------------------------------------------------------------

pub const MAX_TASKS: usize = 224;
const MAX_CORES: usize = crate::smp::core::MAX_CORES;

/// Sentinel meaning "no task running" (scheduler idle loop active).
const IDLE: usize = MAX_TASKS;

static mut TASK_CTX:   [MaybeUninit<TaskContext>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
static mut TASK_CAP:   [MaybeUninit<CapTable>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
/// Per-task state stored as AtomicU8 so `wake_by_slot` (cross-core write)
/// and `block_and_reschedule` (CAS) are race-free (§8.4 lost-wakeup fix).
static TASK_STATE: [AtomicU8; MAX_TASKS] =
    [const { AtomicU8::new(TaskState::Dead as u8) }; MAX_TASKS];
static mut TASK_NAME:  [&str; MAX_TASKS]       = [""; MAX_TASKS];
static mut TASK_VALID: [bool; MAX_TASKS]       = [false; MAX_TASKS];
/// Which core each task is pinned to (set at enqueue time; immutable after).
static mut TASK_CORE:  [u32; MAX_TASKS]        = [0u32; MAX_TASKS];
/// Wakeup result written by `wake_by_slot`; returned by `block_and_reschedule`.
static mut TASK_WAKEUP_ERR: [i64; MAX_TASKS]  = [0i64; MAX_TASKS];
/// Last message received by each task (filled by `store_recv_message`).
static mut TASK_RECV_BUF: [Option<Message>; MAX_TASKS] =
    [const { None }; MAX_TASKS];
/// Whether each task runs in ring-3 (true) or ring-0 (false).
static mut TASK_IS_USER: [bool; MAX_TASKS] = [false; MAX_TASKS];
/// Top of each ring-3 task's kernel stack (used to set TSS.rsp0 and
/// PER_CORE_SYSCALL.kernel_rsp before every switch to that task).
/// Zero for ring-0 tasks.
static mut TASK_KERNEL_STACK_TOP: [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// The recv endpoint owned by each task (None if the task has no endpoint).
static mut TASK_ENDPOINT: [Option<EndpointId>; MAX_TASKS] =
    [const { None }; MAX_TASKS];

/// Saved user-space RSP for each ring-3 task.  Updated whenever the task is
/// switched away from mid-SYSCALL so the SYSRETQ exit path sees the correct
/// per-task RSP instead of another task's value written to PER_CORE_SYSCALL.
static mut TASK_USER_RSP: [u64; MAX_TASKS] = [0u64; MAX_TASKS];

/// Pending received cap slots for each task, filled when handle_recv
/// processes a message that contained embedded capabilities (§7.6, §8.5).
const MAX_PENDING_RECV_CAPS: usize = 4;
static mut TASK_PENDING_RECV_CAPS: [[u32; MAX_PENDING_RECV_CAPS]; MAX_TASKS] =
    [[0u32; MAX_PENDING_RECV_CAPS]; MAX_TASKS];
static mut TASK_PENDING_RECV_CAP_COUNT: [usize; MAX_TASKS] = [0; MAX_TASKS];

// ---------------------------------------------------------------------------
// Per-task memory budget (§10.3, §22 Tests 7A/7B).
// ---------------------------------------------------------------------------

/// Base virtual address for task-requested dynamic allocations (AllocMem syscall).
/// Placed well above the ELF load region (~2 MiB), user stack (≤ 0x8000_0000),
/// and ServiceContextData page (0x3ff000) to avoid collisions.
pub const TASK_HEAP_VA_START: u64 = 0x1_0000_0000; // 4 GiB

/// Bytes dynamically allocated so far by each task (via AllocMem).
static mut TASK_ALLOC_BYTES:   [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// Maximum bytes each task may allocate (set from contract at spawn).
static mut TASK_LIMIT_BYTES:   [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// Next virtual address available for dynamic allocation in each task's space.
static mut TASK_NEXT_ALLOC_VA: [u64; MAX_TASKS] = [0u64; MAX_TASKS];


// ---------------------------------------------------------------------------
// Per-core scheduler state.
// ---------------------------------------------------------------------------

/// Index of the task currently running on each core (IDLE if none).
static mut CORE_CURRENT: [usize; MAX_CORES] = [IDLE; MAX_CORES];

/// Sticky round-robin scan pointer per core (§9.1, §9.3).
///
/// `pick_next` starts scanning from `CORE_RR_SLOT[cid]` rather than
/// `CORE_CURRENT[cid]+1`.  After selecting task at slot X it advances to
/// (X+1) % MAX_TASKS.  This guarantees that every slot is visited in turn
/// before the pointer wraps back to an earlier slot, preventing high-numbered
/// task slots (e.g. ping at slot 185) from being permanently starved by a
/// dense band of ready tasks at lower slots (the root cause of 8B flakiness).
static mut CORE_RR_SLOT: [usize; MAX_CORES] = [0usize; MAX_CORES];

/// Per-core queue of kernel stack tops awaiting deferred free after a self-kill.
///
/// When a task kills itself (CORE_CURRENT[core] == slot), RSP is still on that
/// task's kernel stack K_a.  Freeing K_a immediately risks a concurrent alloc +
/// crash on K_a while this core is still executing — KERNEL PF from stack
/// corruption.  Instead, only the kstack free is deferred; the slot itself is
/// released immediately (TASK_VALID=false) so reserve_task_slot can reuse it
/// without the up-to-10ms slot-starvation window the zombie approach caused.
///
/// The queue is drained by `drain_pending_kstack`, called from:
/// - `timer_tick_from_irq` (every ~10 ms, RSP is on the CURRENT live task's kstack)
/// - the scheduler `run()` loop (RSP is on the per-core BSS scheduler stack)
///
/// Safety: draining only happens when RSP is NOT on any pending kstack.
/// The self-kill path runs with IF=0 (interrupt gate / syscall context) from
/// kill_task_by_slot through yield_current's switch_context.  After switch_context
/// RSP is on a different stack; IF=1 is restored in the incoming task.  The timer
/// ISR can only fire after that point, by which time RSP is not on K_a.
const PENDING_KSTACK_CAP: usize = 8;
static mut CORE_PENDING_KSTACK:     [[u64; PENDING_KSTACK_CAP]; MAX_CORES] = [[0u64; PENDING_KSTACK_CAP]; MAX_CORES];
static mut CORE_PENDING_KSTACK_LEN: [usize;                      MAX_CORES] = [0;                         MAX_CORES];

/// Per-core PML4 frame (physical address) awaiting deferred free after a self-kill.
///
/// Root cause of KERNEL PF under concurrent load (6B, 8A failures):
/// In the self-kill path the dying task's CR3 is still active on this core
/// when reclaim_user_frames hands the PML4 frame to free_frame.  Another
/// core can immediately alloc that frame (for a new PageTable::new) and
/// write_volatile-zero it.  If this core then suffers a TLB miss on a kernel
/// VA (e.g. reading a .rodata format string), the hardware page-walker reads
/// the now-zeroed PML4 → PML4[511] = 0 → "not present" → KERNEL PF.
///
/// Fix: skip freeing the PML4 frame during the reclaim loop for self-kills;
/// store it here and release it in drain_pending_pml4, which is called from
/// the scheduler loop and timer tick — both run with a different CR3.
///
/// AtomicU64 (not static mut) so reads and writes at call sites are safe.
/// The value is always written by one core and read by the same core, so
/// Relaxed/Release/Acquire ordering is sufficient.
static CORE_PENDING_PML4: [AtomicU64; MAX_CORES] =
    [const { AtomicU64::new(0) }; MAX_CORES];

/// Per-core save area for dead task context during self-kill.
///
/// When a task kills itself and immediately releases TASK_VALID=false, a
/// concurrent spawn on another core can claim TASK_CTX[slot] before
/// yield_current's switch_context runs.  Saving the dead task's registers into
/// CORE_DEAD_CTX instead avoids the write-after-claim race.  CORE_DEAD_CTX is
/// never used as a load source; dead tasks are never resumed.
static mut CORE_DEAD_CTX: [TaskContext; MAX_CORES] = [const {
    TaskContext { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
                  rip: 0, rsp: 0, cr3: 0 }
}; MAX_CORES];

/// Saved context for each core's idle scheduler loop.
static mut CORE_SCHED_CTX: [TaskContext; MAX_CORES] = [const {
    TaskContext { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
                  rip: 0, rsp: 0, cr3: 0 }
}; MAX_CORES];

// ---------------------------------------------------------------------------
// Helper: identify the current core.
// ---------------------------------------------------------------------------

/// Read the local APIC ID register and look up the assigned core index.
///
/// Must only be called after `init_local_apic` on this core.
fn current_core_id() -> usize {
    // SAFETY: APIC is mapped before the scheduler ever runs on any core.
    let lapic_id = unsafe { crate::arch::x86_64::boot::get_lapic_id() };
    crate::smp::core::lapic_to_core_id(lapic_id) as usize
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Return the calling core's ID (0-based).
pub fn current_core() -> usize { current_core_id() }

// SMP spinlock for the task-slot table — concurrent spawns on different cores
// (e.g. supervisor on Core 0 and a prop-p2 respawn on Core 3) both scan
// TASK_VALID; the scan-and-set must be atomic across cores.
static TASK_SLOT_LOCKED: AtomicBool = AtomicBool::new(false);

#[inline]
fn task_slot_lock() {
    while TASK_SLOT_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

#[inline]
fn task_slot_unlock() {
    TASK_SLOT_LOCKED.store(false, Ordering::Release);
}

/// Reserve a free task slot, pinned to `core_id`.
///
/// Marks the slot VALID (but state remains Dead) so subsequent calls to
/// `task_cap_init_empty` and `commit_task` can use it.  The slot will not
/// be scheduled until `commit_task` sets its state to Ready.
///
/// Returns `None` if all slots are occupied.
pub fn reserve_task_slot(core_id: u32) -> Option<usize> {
    task_slot_lock();
    // SAFETY: lock held; exclusive access to TASK_VALID/TASK_CORE across all cores.
    let result = unsafe {
        let mut found = None;
        for i in 0..MAX_TASKS {
            if !TASK_VALID[i] {
                TASK_VALID[i] = true;
                TASK_CORE[i]  = core_id;
                found = Some(i);
                break;
            }
        }
        found
    };
    task_slot_unlock();
    result
}

/// Initialise the CapTable for a reserved slot **in-place in BSS** and return
/// a mutable reference to it.
///
/// Using `write_bytes(0)` avoids placing a 1 536-byte `CapTable` on the
/// caller's kernel stack (which would corrupt the timer-interrupt return
/// address saved at K0T-200 — the rip=0 root cause).
///
/// # Safety
/// * `slot` must have been reserved via `reserve_task_slot`.
/// * IF=0 (syscall context).
/// * `Option<Capability>` None is represented as all-zero bytes in Rust's
///   enum layout for variants without niches; this is verified by the existing
///   diagnostic that confirmed the corruption IS a zero-write.
pub unsafe fn task_cap_init_empty(slot: usize) -> &'static mut CapTable {
    // SAFETY: slot is reserved; write_bytes zeros all Option<Capability> discriminants
    // to 0 (= None) with no intermediate kstack allocation.
    unsafe {
        core::ptr::write_bytes(
            TASK_CAP[slot].as_mut_ptr() as *mut u8,
            0,
            core::mem::size_of::<CapTable>(),
        );
        TASK_CAP[slot].assume_init_mut()
    }
}

/// Release a previously-reserved slot without committing (called on spawn error).
pub fn release_task_slot(slot: usize) {
    task_slot_lock();
    // SAFETY: lock held; slot was reserved by this core.
    unsafe { TASK_VALID[slot] = false; }
    task_slot_unlock();
}

/// Finalise a reserved task slot: write context + metadata and mark Ready.
///
/// # Safety
/// * `slot` must have been reserved and its CapTable initialised.
/// * IF=0 (syscall context).
pub unsafe fn commit_task(
    slot:             usize,
    name:             &'static str,
    ctx:              TaskContext,
    is_user:          bool,
    kernel_stack_top: u64,
    endpoint_id:      Option<EndpointId>,
) {
    // SAFETY: slot is reserved; IF=0 prevents concurrent modification.
    unsafe {
        TASK_CTX[slot].write(ctx);
        TASK_NAME[slot]             = name;
        TASK_IS_USER[slot]          = is_user;
        TASK_KERNEL_STACK_TOP[slot] = kernel_stack_top;
        TASK_ENDPOINT[slot]         = endpoint_id;
        // TASK_STATE must be last: once Ready is visible to other cores, every
        // other field in this slot must already be correctly set.  A concurrent
        // kill_task_by_slot that observes Ready will immediately read
        // TASK_IS_USER and TASK_KERNEL_STACK_TOP; if those still hold the
        // previous occupant's values the kill frees the wrong kernel stack.
        // Release ordering publishes all preceding stores before this one.
        TASK_STATE[slot].store(TaskState::Ready as u8, Ordering::Release);
    }
}

/// Add a task to the run queue, pinned to `core_id`.
///
/// Legacy single-call path kept for kernel-internal use.  New spawn code
/// should use `reserve_task_slot` + `task_cap_init_empty` + `commit_task`
/// to avoid a 1 536-byte `CapTable` on the kernel stack.
#[allow(dead_code)]
pub fn enqueue(
    name:             &'static str,
    ctx:              TaskContext,
    caps:             CapTable,
    core_id:          u32,
    is_user:          bool,
    kernel_stack_top: u64,
    endpoint_id:      Option<EndpointId>,
) {
    // SAFETY: called from BSP before any AP scheduler starts.
    unsafe {
        for i in 0..MAX_TASKS {
            if !TASK_VALID[i] {
                TASK_CTX[i].write(ctx);
                TASK_CAP[i].write(caps);
                TASK_STATE[i].store(TaskState::Ready as u8, Ordering::Relaxed);
                TASK_NAME[i]             = name;
                TASK_VALID[i]            = true;
                TASK_CORE[i]             = core_id;
                TASK_IS_USER[i]          = is_user;
                TASK_KERNEL_STACK_TOP[i] = kernel_stack_top;
                TASK_ENDPOINT[i]         = endpoint_id;
                return;
            }
        }
        panic!("scheduler: run queue full");
    }
}

// ---------------------------------------------------------------------------
// Per-task capability access — used by syscall dispatch (§8.2, §7.5).
// ---------------------------------------------------------------------------

/// Validate and return a copy of the capability at `slot` in the current
/// task's table.
pub fn current_task_lookup_cap(slot: usize, right: Rights) -> Result<Capability, CapError> {
    let cid  = current_core_id();
    // SAFETY: IF=0 in syscall context; CORE_CURRENT is stable for this core.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS && TASK_VALID[cur] {
            TASK_CAP[cur].assume_init_ref().get(slot, right)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

/// Remove the capability at `slot` from the current task's table (GRANT).
pub fn current_task_remove_cap(slot: usize) -> Option<Capability> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS && TASK_VALID[cur] {
            TASK_CAP[cur].assume_init_mut().remove(slot)
        } else {
            None
        }
    }
}

/// Read the rights of the cap at `slot` without validating the generation.
///
/// Used by `QueryCapRights` (syscall 14) — read-only, no side effects.
pub fn current_task_read_cap_rights(slot: usize) -> Option<Rights> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS && TASK_VALID[cur] {
            crate::capability::cap_read_rights(TASK_CAP[cur].assume_init_ref(), slot)
        } else {
            None
        }
    }
}

/// Push a cap slot into the current task's pending-received-caps buffer.
///
/// Called by handle_recv when it installs an embedded cap into the receiver's
/// table. The slot is retrieved by the service via syscall 12 (TakePendingCap).
pub fn push_pending_recv_cap(cap_slot: u32) {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS {
            let count = TASK_PENDING_RECV_CAP_COUNT[cur];
            if count < MAX_PENDING_RECV_CAPS {
                TASK_PENDING_RECV_CAPS[cur][count] = cap_slot;
                TASK_PENDING_RECV_CAP_COUNT[cur]   = count + 1;
            }
        }
    }
}

/// Pop the next pending cap slot from the current task's buffer.
/// Returns `None` if no pending caps remain.
pub fn pop_pending_recv_cap() -> Option<u32> {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS {
            let count = TASK_PENDING_RECV_CAP_COUNT[cur];
            if count > 0 {
                let slot = TASK_PENDING_RECV_CAPS[cur][0];
                // Shift remaining entries left.
                for i in 0..count - 1 {
                    TASK_PENDING_RECV_CAPS[cur][i] = TASK_PENDING_RECV_CAPS[cur][i + 1];
                }
                TASK_PENDING_RECV_CAP_COUNT[cur] = count - 1;
                return Some(slot);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Memory budget API (§10.3, §22 Tests 7A/7B).
// ---------------------------------------------------------------------------

/// Set the memory budget for `slot` at spawn time.
///
/// Resets alloc_bytes to 0 and seeds the first heap VA.
pub fn set_task_memory_budget(slot: usize, limit: u64) {
    if slot >= MAX_TASKS { return; }
    // SAFETY: called from spawn path with IF=0; single writer for this slot.
    unsafe {
        TASK_ALLOC_BYTES[slot]   = 0;
        TASK_LIMIT_BYTES[slot]   = limit;
        TASK_NEXT_ALLOC_VA[slot] = TASK_HEAP_VA_START;
    }
}

/// Reserve `size` bytes from the current task's memory budget.
///
/// Returns the virtual address at which the caller should map the new pages,
/// or `None` if the allocation would exceed the task's limit (AllocDenied).
///
/// `size` is rounded up to a 4 KiB page boundary before the budget check so
/// that the VA region is always page-aligned.
pub fn current_task_claim_alloc(size: u64) -> Option<u64> {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur >= MAX_TASKS || !TASK_VALID[cur] { return None; }

        // Overflow guard: (size + 4095) wraps for very large values (e.g. u64::MAX).
        // saturating to u64::MAX guarantees the budget check rejects the request.
        let aligned = size.checked_add(4095).map(|v| v & !4095).unwrap_or(u64::MAX);
        let already = TASK_ALLOC_BYTES[cur];
        let limit   = TASK_LIMIT_BYTES[cur];
        if already.saturating_add(aligned) > limit { return None; }

        let va = TASK_NEXT_ALLOC_VA[cur];
        TASK_ALLOC_BYTES[cur]   = already.saturating_add(aligned);
        TASK_NEXT_ALLOC_VA[cur] = va.saturating_add(aligned);
        Some(va)
    }
}

/// Return the bytes dynamically allocated so far by the current task.
///
/// Used by InspectKernel query 0 (P4 property test — §10.3).
pub fn current_task_alloc_bytes() -> u64 {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core reader for this slot.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur >= MAX_TASKS || !TASK_VALID[cur] { return 0; }
        TASK_ALLOC_BYTES[cur]
    }
}

/// Insert a capability into the current task's table (incoming GRANT).
pub fn current_task_insert_cap(cap: Capability) -> Result<usize, CapError> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS && TASK_VALID[cur] {
            TASK_CAP[cur].assume_init_mut().insert(cap)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

// ---------------------------------------------------------------------------
// Ring-3 switch preparation.
// ---------------------------------------------------------------------------

/// Update per-core SYSCALL data and TSS.rsp0 before switching to a ring-3 task.
///
/// Must be called (with IF=0) immediately before every `switch_context` whose
/// incoming task is a ring-3 task.  This ensures:
///   • `PER_CORE_SYSCALL[cid].kernel_rsp` → correct kernel stack for SYSCALL.
///   • `TSS[cid].rsp0`                   → correct kernel stack for hardware
///                                          interrupts that hit ring-3 code.
///
/// # Safety
/// IF must be 0.  `slot` must be a valid ring-3 task slot.
unsafe fn prepare_ring3_switch(core_id: usize, slot: usize) {
    // SAFETY: TASK_KERNEL_STACK_TOP[slot] is set at enqueue; we have IF=0.
    let ksp = unsafe { TASK_KERNEL_STACK_TOP[slot] };

    // SYSCALL entry must start 512 bytes below K0T, NOT at K0T.
    //
    // Both the timer ISR (via TSS.rsp0 → K0T) and SYSCALL entry would otherwise
    // start from the same K0T, making their stack frames overlap.  The timer ISR
    // saves switch_context's return address at K0T-200; spawn_service_with_config's
    // frame (which starts around K0T-192 for the SYSCALL path) covers K0T-200 and
    // any zero-init within the frame writes 0 there — corrupting the saved return
    // address and causing the rip=0 crash on the next resume.
    //
    // By starting SYSCALL at K0T-512, all SYSCALL frames live below K0T-512.
    // K0T-200 (the timer ISR's deepest save point) is above K0T-512 and is
    // therefore never touched by any SYSCALL frame.
    let syscall_rsp = ksp - 512;

    // SAFETY: PER_CORE_SYSCALL lives in .data; single writer (this core).
    unsafe {
        crate::arch::x86_64::syscall_entry::PER_CORE_SYSCALL[core_id].kernel_rsp = syscall_rsp;
        // Restore per-task user RSP so SYSRETQ loads the correct stack pointer for
        // this task, not the value left by the last task that ran on this core.
        crate::arch::x86_64::syscall_entry::PER_CORE_SYSCALL[core_id].user_rsp =
            TASK_USER_RSP[slot];
    }
    // TSS.rsp0 stays at K0T so hardware interrupts (timer ISR) still enter at the top.
    // SAFETY: set_tss_rsp0 writes only to TSS_PER_CORE[core_id].rsp0.
    unsafe { crate::arch::x86_64::boot::set_tss_rsp0(core_id, ksp) };
}

/// Enter the scheduler loop on the calling core. Never returns.
pub fn run(core_id: u32) -> ! {
    let cid = core_id as usize;

    // Seed the scheduler context's CR3 with the current value.  `switch_context`
    // never saves CR3 (only loads it), so without this, switching back to the
    // scheduler context after `block_and_reschedule` would load CR3=0 and
    // triple-fault on the next instruction fetch.
    // SAFETY: run() is called once per core before any task switch; CORE_SCHED_CTX[cid] exclusively accessible.
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
        CORE_SCHED_CTX[cid].cr3 = cr3;
    }

    loop {
        // Free any deferred kstack from a prior self-kill on this core.
        // RSP is on CORE_SCHED_CTX's stack (per-core BSS), not any kstack.
        drain_pending_kstack(cid);

        // Compiler barrier: force a reload of all scheduler statics (TASK_STATE,
        // TASK_VALID, TASK_CORE) on every iteration.  Without this, the compiler
        // is free to cache the pick_next result across the `sti; hlt` boundary
        // because it sees no intervening writes in this compilation unit.
        // The IPI handler on a remote core writes TASK_STATE, which the
        // compiler cannot observe without this barrier.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

        match pick_next(cid) {
            Some(next) => {
                // SAFETY: IF disabled around the switch to prevent preemption.
                unsafe {
                    core::arch::asm!("cli", options(nostack, nomem));
                    TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
                    CORE_CURRENT[cid] = next;
                    if TASK_IS_USER[next] {
                        prepare_ring3_switch(cid, next);
                    }
                    let sched    = &raw mut CORE_SCHED_CTX[cid];
                    let next_ctx = TASK_CTX[next].assume_init_ref() as *const TaskContext;
                    switch_context(sched, next_ctx);
                    // Execution returns here after the task is preempted and
                    // the scheduler loop is re-entered.
                    CORE_CURRENT[cid] = IDLE;
                    core::arch::asm!("sti", options(nostack, nomem));
                }
            }
            None => {
                // Core 0 drains the COM2 control channel when idle (§17).
                if cid == 0 {
                    crate::control::process_pending();
                }
                // No ready tasks for this core; sleep until the next interrupt.
                // `wait_for_interrupt` atomically enables interrupts and halts;
                // the next interrupt (timer or IPI) will wake the core.  The
                // function is a memory clobber: after hlt returns the IPI handler
                // will have written TASK_STATE; the compiler must not cache the
                // previous None result across this boundary.
                crate::arch::x86_64::wait_for_interrupt();
            }
        }
    }
}

/// Called by the timer ISR every ~10 ms to enforce the preemption quantum.
///
/// # Safety
/// Must only be called from the timer ISR with interrupts disabled (IF=0).
#[no_mangle]
pub extern "C" fn timer_tick_from_irq() {
    let cid = current_core_id();
    // Free any deferred kstack from a prior self-kill on this core.
    // RSP is now on the current task's kstack, not the dead task's, so
    // freeing the pending kstack is safe.  IF=0 (interrupt gate).
    drain_pending_kstack(cid);
    // SAFETY: IF=0 throughout (interrupt gate clears IF on entry).
    unsafe {
        crate::arch::x86_64::boot::apic_send_eoi();

        // Poll the COM2 control channel on every core-0 timer tick (§17).
        // The idle branch can't be relied on when core 0 always has ready tasks.
        if cid == 0 {
            crate::control::process_pending();
        }

        let prev = CORE_CURRENT[cid];

        if prev < MAX_TASKS && TASK_VALID[prev]
            && TaskState::from(TASK_STATE[prev].load(Ordering::Relaxed)) == TaskState::Running
        {
            TASK_STATE[prev].store(TaskState::Ready as u8, Ordering::Relaxed);
        }

        let next = match pick_next(cid) {
            Some(i) => i,
            None => {
                // If the current task was killed (state=Dead) and no other task
                // is runnable, switch to the scheduler idle context so that
                // kill_task_by_slot's spin-wait on CORE_CURRENT sees us leave.
                // Without this, the ISR returns to the dead task, CORE_CURRENT
                // never changes, and the spin-wait deadlocks.
                // Check STATE alone (not TASK_VALID): with the deferred-kstack
                // approach TASK_VALID is false immediately after self-kill, but
                // STATE=Dead is still the signal we need.
                let is_dead = prev < MAX_TASKS
                    && TASK_STATE[prev].load(Ordering::Relaxed) == TaskState::Dead as u8;
                if is_dead {
                    CORE_CURRENT[cid] = IDLE;
                    // Save into CORE_DEAD_CTX — not TASK_CTX[prev] — to avoid a
                    // write-after-claim race if a concurrent spawn has already
                    // reserved TASK_CTX[prev] (possible now that TASK_VALID=false
                    // immediately).  CORE_DEAD_CTX is never used as a load source.
                    let dead_ctx = &raw mut CORE_DEAD_CTX[cid];
                    let sched_ctx = &raw const CORE_SCHED_CTX[cid];
                    switch_context(dead_ctx, sched_ctx);
                    // Unreachable: dead tasks are never rescheduled.
                } else {
                    // Restore Running only if we changed the state to Ready above.
                    // CAS fails if state is Blocked* or Dead, leaving it untouched.
                    if prev < MAX_TASKS && TASK_VALID[prev] {
                        TASK_STATE[prev]
                            .compare_exchange(
                                TaskState::Ready as u8,
                                TaskState::Running as u8,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .ok();
                    }
                }
                return;
            }
        };

        if next == prev {
            TASK_STATE[prev].store(TaskState::Running as u8, Ordering::Relaxed);
            return;
        }

        TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
        CORE_CURRENT[cid] = next;

        // Save BEFORE prepare_ring3_switch so we capture the value from the last
        // SYSCALL entry for `prev`, not the value prepare_ring3_switch writes for `next`.
        if prev < MAX_TASKS && TASK_VALID[prev] && TASK_IS_USER[prev] {
            TASK_USER_RSP[prev] =
                crate::arch::x86_64::syscall_entry::PER_CORE_SYSCALL[cid].user_rsp;
        }

        if TASK_IS_USER[next] {
            prepare_ring3_switch(cid, next);
        }

        let current_ctx: *mut TaskContext = if prev >= MAX_TASKS {
            &raw mut CORE_SCHED_CTX[cid]
        } else if !TASK_VALID[prev] {
            // Slot was immediately released by a self-kill (deferred-kstack
            // approach).  Save into CORE_DEAD_CTX to avoid a write-after-claim
            // race with a concurrent spawn.  CORE_DEAD_CTX is never resumed.
            &raw mut CORE_DEAD_CTX[cid]
        } else {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        };

        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
    }
}

/// Advisory yield: mark the current task Ready and reschedule.
/// Also called from the IPI handler for cross-core WAKE_RECEIVER (§9.4).
pub fn yield_current() {
    crate::arch::x86_64::disable_interrupts();

    let cid = current_core_id();

    // SAFETY: IF=0.
    unsafe {
        let prev = CORE_CURRENT[cid];
        if prev < MAX_TASKS && TASK_VALID[prev]
            && TaskState::from(TASK_STATE[prev].load(Ordering::Relaxed)) == TaskState::Running
        {
            TASK_STATE[prev].store(TaskState::Ready as u8, Ordering::Relaxed);
        }

        let next = match pick_next(cid) {
            Some(i) => i,
            None => {
                // Same Dead-detection as timer_tick_from_irq: if the current task
                // was killed with no other runnable task, switch to the scheduler
                // so kill_task_by_slot's spin-wait can exit.
                // Check STATE alone (not TASK_VALID): TASK_VALID is immediately
                // false after a self-kill (deferred-kstack approach), but
                // STATE=Dead is still the signal we need.
                let is_dead = prev < MAX_TASKS
                    && TASK_STATE[prev].load(Ordering::Relaxed) == TaskState::Dead as u8;
                if is_dead {
                    CORE_CURRENT[cid] = IDLE;
                    // Save into CORE_DEAD_CTX, not TASK_CTX[prev], to avoid a
                    // write-after-claim race with a concurrent spawn that may
                    // have already reserved the now-available slot.
                    let dead_ctx = &raw mut CORE_DEAD_CTX[cid];
                    let sched_ctx = &raw const CORE_SCHED_CTX[cid];
                    switch_context(dead_ctx, sched_ctx);
                    // Unreachable: dead tasks are never rescheduled.
                } else {
                    if prev < MAX_TASKS && TASK_VALID[prev] {
                        TASK_STATE[prev].store(TaskState::Running as u8, Ordering::Relaxed);
                    }
                    core::arch::asm!("sti", options(nostack, nomem));
                }
                return;
            }
        };

        if next == prev {
            TASK_STATE[prev].store(TaskState::Running as u8, Ordering::Relaxed);
            core::arch::asm!("sti", options(nostack, nomem));
            return;
        }

        TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
        CORE_CURRENT[cid] = next;

        // Save BEFORE prepare_ring3_switch so we capture the value from SYSCALL
        // entry, not the value prepare_ring3_switch is about to write for `next`.
        if prev < MAX_TASKS && TASK_VALID[prev] && TASK_IS_USER[prev] {
            TASK_USER_RSP[prev] =
                crate::arch::x86_64::syscall_entry::PER_CORE_SYSCALL[cid].user_rsp;
        }

        if TASK_IS_USER[next] {
            prepare_ring3_switch(cid, next);
        }

        // Save the current execution state.
        // Three cases:
        //   prev >= MAX_TASKS  → scheduler idle context (save to CORE_SCHED_CTX)
        //   TASK_VALID=false   → self-killed task (save to CORE_DEAD_CTX; slot
        //                        already reclaimed, TASK_CTX[prev] may be reused
        //                        by a concurrent spawn — must not write there)
        //   live task          → normal case (save to TASK_CTX[prev])
        // Saving a kstack RSP into CORE_SCHED_CTX would corrupt the scheduler's
        // BSP stack pointer, causing a KERNEL PF on the next scheduler resume.
        // CORE_DEAD_CTX is never used as a load source (dead tasks not resumed).
        let current_ctx: *mut TaskContext = if prev >= MAX_TASKS {
            &raw mut CORE_SCHED_CTX[cid]
        } else if !TASK_VALID[prev] {
            &raw mut CORE_DEAD_CTX[cid]
        } else {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        };
        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
        core::arch::asm!("sti", options(nostack, nomem));
    }
}

/// Return the slot index of the currently-running task on this core.
///
/// Returns `IDLE` (== MAX_TASKS) if the scheduler loop is active.
pub fn current_task_slot() -> usize {
    let cid = current_core_id();
    // SAFETY: read-only; stable within a syscall (IF=0).
    unsafe { CORE_CURRENT[cid] }
}

/// Wake the task at `slot` with the given result code.
///
/// If the task lives on a different core, sends a WAKE_RECEIVER IPI to that
/// core so it exits `hlt` and can reschedule (§8.4, §9.4).
pub fn wake_by_slot(slot: usize, result: i64) {
    // SAFETY: IF=0 from IPC/syscall path.
    unsafe {
        if slot < MAX_TASKS && TASK_VALID[slot] {
            // Do not revive a task that kill_task_by_slot has already marked Dead.
            // If we overwrite Dead with Ready, the scheduler re-animates a task
            // whose kernel stack may already be freed — KERNEL PF on next entry.
            // Use CAS so that a concurrent Dead-store (from kill_task_by_slot
            // under the slot lock) wins rather than being silently overwritten.
            // We must read current state first; if it's already Dead, bail.
            let current = TASK_STATE[slot].load(Ordering::Acquire);
            if current == TaskState::Dead as u8 { return; }
            TASK_WAKEUP_ERR[slot] = result;
            // CAS: only transition to Ready from the observed non-Dead state.
            // If kill raced and set Dead between our load and here, the CAS
            // fails and we correctly leave the task Dead.
            if TASK_STATE[slot]
                .compare_exchange(
                    current,
                    TaskState::Ready as u8,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_err()
            {
                return;
            }

            let task_core = TASK_CORE[slot] as usize;
            let my_core   = current_core_id();
            if task_core != my_core {
                // SAFETY: APIC is initialised; task_core is a ready core.
                unsafe {
                    crate::smp::ipi::send_ipi(
                        task_core as u32,
                        crate::smp::ipi::vectors::WAKE_RECEIVER,
                    );
                }
            }
        }
    }
}

/// Find the slot of a live task by name. Returns `None` if not found or dead.
pub fn find_task_by_name(name: &str) -> Option<usize> {
    // SAFETY: read-only scan; caller holds no locks.
    unsafe {
        for i in 0..MAX_TASKS {
            if TASK_VALID[i]
                && TASK_NAME[i] == name
                && TaskState::from(TASK_STATE[i].load(Ordering::Acquire)) != TaskState::Dead
            {
                return Some(i);
            }
        }
    }
    None
}

/// Free all deferred kernel stacks from a prior self-kill on this core.
///
/// Safe to call from timer_tick_from_irq (IF=0, RSP on the current live task's
/// kstack or TSS.rsp0 of the incoming timer frame — either way, NOT on any
/// pending dead kstack) and from the scheduler run() loop (RSP on the per-core
/// BSS scheduler stack).
///
/// Must NOT be called while RSP is on one of the pending kstacks.  This is
/// guaranteed by the self-kill invariant: the window from kill_task_by_slot to
/// yield_current's switch_context runs with IF=0, so the timer cannot fire
/// during that window; after switch_context RSP is on a different stack.
fn drain_pending_kstack(cid: usize) {
    // SAFETY: CORE_PENDING_KSTACK_LEN[cid] is written only by this core.
    let n = unsafe { CORE_PENDING_KSTACK_LEN[cid] };
    if n != 0 {
        // Clear before processing so re-entrant callers see an empty queue.
        unsafe { CORE_PENDING_KSTACK_LEN[cid] = 0; }
        for i in 0..n {
            let kstack = unsafe { CORE_PENDING_KSTACK[cid][i] };
            // SAFETY: RSP is NOT on this kstack (see above).  kstack is the top
            // of a TASK_KSTACK_MAX-sized block allocated from the kstack pool.
            if kstack != 0 { super::free_kstack(kstack); }
        }
    }

    // Free any deferred PML4 frame from a prior self-kill.  By the time this
    // runs the core has switched to a different CR3, so freeing the old PML4
    // frame no longer risks a concurrent zeroing race (see CORE_PENDING_PML4).
    // AtomicU64 load/store: no unsafe needed here.
    let pml4_phys = CORE_PENDING_PML4[cid].load(Ordering::Acquire);
    if pml4_phys != 0 {
        CORE_PENDING_PML4[cid].store(0, Ordering::Relaxed);
        // SAFETY: pml4_phys was the task's own PML4 frame; CR3 has since been
        // switched away so no core's page-walker will read from it.
        unsafe {
            let frame = crate::memory::frame::Frame::from_phys(
                crate::memory::frame::PhysAddr(pml4_phys)
            );
            crate::memory::allocator::free_frame(frame);
        }
    }
}

/// Kill a task by slot: mark Dead, kill its endpoint (if any), notify blocked tasks.
pub fn kill_task_by_slot(slot: usize) {
    // Serialize the TASK_VALID check against concurrent kills and spawns.
    // Two cores calling kill_task_by_slot(slot) simultaneously would otherwise
    // both pass the !TASK_VALID guard, both reach free_kstack, and double-free
    // the kernel stack.  The slot can also be claimed by reserve_task_slot between
    // free_kstack (line ~807) and TASK_VALID=false (line ~817), causing the second
    // killer to free a live kernel stack → KERNEL PF.
    //
    // Fix: hold task_slot_lock while checking TASK_VALID and while atomically
    // claiming the kill (TASK_STATE=Dead).  Release the lock before the long
    // cleanup so spawn on other cores can proceed concurrently.
    task_slot_lock();
    // SAFETY: lock held; exclusive access to TASK_VALID/TASK_STATE across all cores.
    let already_dead = unsafe {
        if slot >= MAX_TASKS || !TASK_VALID[slot] {
            task_slot_unlock();
            return;
        }
        let s = TASK_STATE[slot].load(Ordering::Acquire);
        if s == TaskState::Dead as u8 {
            // Another core is already killing this slot — bail.
            task_slot_unlock();
            return;
        }
        // Atomically claim this kill: set Dead under the lock so no concurrent
        // call can also proceed past this point for the same slot.
        TASK_STATE[slot].store(TaskState::Dead as u8, Ordering::Release);
        false
    };
    task_slot_unlock();
    let _ = already_dead; // always false here; kept for clarity

    // SAFETY: IF=0 or lock-free path; TASK_VALID[slot] and Dead state claimed above.
    unsafe {

        // Capture identity before any slot state changes.
        let task_name = TASK_NAME[slot];
        let task_ep   = TASK_ENDPOINT[slot];

        // Free the task's name-table slot so it can be claimed by a future service.
        crate::ipc::names::unregister(task_name);

        // Kill the task's endpoint if it has one.
        if let Some(ep_id) = task_ep {
            // Bump generation in routing table and wake any blocked tasks.
            let (rx_slot, tx_slot) = crate::ipc::routing::kill_endpoint(ep_id);
            // Skip waking `slot` itself: the killed task's rx/tx slot is often
            // its own slot (the task was blocked on recv of its own endpoint).
            // Calling wake_by_slot(slot, -7) would overwrite the Dead state with
            // Ready, causing the scheduler to re-animate the dying task with its
            // freed page tables — the root cause of the use-after-free cascade.
            if let Some(s) = rx_slot { if s != slot { wake_by_slot(s, -7); } }
            if let Some(s) = tx_slot { if s != slot { wake_by_slot(s, -7); } }

            // Mark resource dead in global cap table so generation check fails.
            let resource_id = crate::capability::cap::ResourceId::from(ep_id);
            crate::capability::table::mark_dead_resource(resource_id);
        }

        // SMP safety: spin until no other core has CORE_CURRENT[c] == slot.
        //
        // A core may have selected this slot from pick_next (observing STATE=Ready)
        // before our STATE=Dead store propagated.  That core has set
        // CORE_CURRENT[c]=slot and is about to call switch_context, loading this
        // task's cr3.  We must not free the page-table frames until that core has
        // moved on (CORE_CURRENT[c] changes to a different task), because after
        // switch_context the core is in kernel mode with the shared higher-half
        // mappings — it will load the new cr3 next, and kernel code does not
        // touch user-half frames.  Bounded by one preemption quantum (~10 ms).
        //
        // We skip the calling core: either it holds a different slot (the common
        // cross-core kill path), or this is kill_current where the caller switches
        // away immediately after returning — in both cases the skip is safe.
        //
        // Send a WAKE_RECEIVER IPI to any core currently running the dead task so
        // it immediately calls yield_current → detects Dead → switches to the
        // scheduler, allowing the spin-wait below to exit before the next timer
        // tick (otherwise we spin up to one full 10 ms quantum).  The IPI is sent
        // after STATE=Dead is visible (SeqCst fence above), so the receiving core
        // will observe the Dead state in its yield_current Dead-detection branch.
        {
            let my_core = current_core_id();
            for cid in 0..MAX_CORES {
                if cid != my_core && CORE_CURRENT[cid] == slot {
                    // SAFETY: cid is a valid core index (loop bound), APIC is mapped.
                    unsafe {
                        crate::smp::ipi::send_ipi(
                            cid as u32,
                            crate::smp::ipi::vectors::WAKE_RECEIVER,
                        );
                    }
                }
            }
        }
        {
            let my_core = current_core_id();
            for cid in 0..MAX_CORES {
                if cid == my_core { continue; }
                loop {
                    // Compiler + hardware barrier: reload CORE_CURRENT[cid] from
                    // memory on every iteration; do not use a cached register value.
                    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                    if CORE_CURRENT[cid] != slot { break; }
                    core::hint::spin_loop();
                }
            }
        }

        // Detect self-kill before reclaim so we can defer the PML4 frame.
        //
        // Self-kill PML4 race (root cause of 6B/8A KERNEL PF):
        //   In the self-kill path this core's CR3 is still the dying task's
        //   PML4.  If we free the PML4 frame here, another core's
        //   PageTable::new() can immediately alloc + write_volatile-zero it.
        //   The hardware page-walker on this core then reads a zeroed PML4 on
        //   any TLB miss (e.g. reading a .rodata format string) → KERNEL PF.
        //
        //   Fix: skip freeing the PML4 frame in the self-kill path; store it
        //   in CORE_PENDING_PML4[my_core] and release it in drain_pending_pml4
        //   (called from the scheduler loop / timer tick) where CR3 has already
        //   been switched to a different page table.
        let my_core = current_core_id();
        let is_self_kill = CORE_CURRENT[my_core] == slot;

        // Reclaim all user-space frames: walk the page table and return each
        // frame to the allocator (§10.5).
        //
        // TLB coherence: the spin-wait above guarantees every other core has
        // loaded a *different* CR3 since last running this task.  A CR3 reload
        // flushes all non-global TLB entries, so no core retains stale
        // translations for this task's virtual addresses.  A separate TLB
        // shootdown IPI would therefore be redundant — and dangerous: if a
        // remote core is mid-syscall with IF=0 (e.g. loading an ELF for a
        // concurrent spawn), it cannot ACK the IPI, causing the caller to spin
        // indefinitely (deadlock).  We skip the broadcast and rely solely on
        // the spin-wait guarantee.
        let freed_frames: usize;
        if TASK_IS_USER[slot] {
            let cr3 = TASK_CTX[slot].assume_init_ref().cr3;
            if cr3 != 0 {
                let pml4_phys = cr3 & !0xFFF_u64;
                // SAFETY: cr3 is the task's PML4 set at spawn and immutable
                // until now.  Task is Dead; all other cores have moved past this
                // slot (spin-wait above); no core will load this cr3 hereafter.
                let buf = crate::arch::x86_64::page_tables::reclaim_user_frames(cr3);
                for &phys_addr in buf.as_slice() {
                    // In the self-kill path, skip the PML4 frame: our CR3
                    // still points to it, and freeing it now lets another core
                    // zero it (PageTable::new) while we hold that CR3, causing
                    // a TLB-miss → zeroed PML4 → KERNEL PF (see CORE_PENDING_PML4).
                    if is_self_kill && phys_addr == pml4_phys { continue; }
                    // SAFETY: phys_addr came from this task's page table, so it
                    // was allocated from the frame allocator and is now ours to free.
                    let frame = crate::memory::frame::Frame::from_phys(
                        crate::memory::frame::PhysAddr(phys_addr)
                    );
                    crate::memory::allocator::free_frame(frame);
                }
                if is_self_kill && pml4_phys != 0 {
                    CORE_PENDING_PML4[my_core].store(pml4_phys, Ordering::Release);
                }
                freed_frames = buf.as_slice().len();
            } else {
                freed_frames = 0;
            }
        } else {
            freed_frames = 0;
        }
        crate::kprintln!("kill_task: slot={} '{}' freed {} frames", slot, task_name, freed_frames);

        // Kstack free and slot release.
        //
        // Self-kill: RSP is on K_a (page-fault ISR pushed to TSS.RSP0 = K_a).
        // Freeing K_a immediately lets another core alloc K_a for a new task
        // that crashes, pushing its ISR frame to K_a while this core is still
        // executing there — corrupting both stacks (KERNEL PF).
        //
        // Fix — deferred kstack only: release the slot immediately (TASK_VALID=false)
        // so reserve_task_slot can reuse it without the zombie 10ms starvation
        // window, but enqueue the kstack pointer for free at the next timer tick
        // or scheduler idle loop, where RSP is on a different stack.
        //
        // Context save safety: yield_current (called immediately after this return)
        // detects TASK_VALID=false and saves the dead task's registers into
        // CORE_DEAD_CTX[cid] rather than TASK_CTX[slot], preventing a
        // write-after-claim race if a concurrent spawn has already reserved
        // TASK_CTX[slot] via reserve_task_slot.
        //
        // Cross-kill: this core's RSP is on the supervisor's kstack, not K_a.
        // Free immediately and release the slot now.
        if is_self_kill {
            // Self-kill: defer only the kstack free; release slot immediately.
            if TASK_IS_USER[slot] {
                let kstack = TASK_KERNEL_STACK_TOP[slot];
                if kstack != 0 {
                    let len = CORE_PENDING_KSTACK_LEN[my_core];
                    if len < PENDING_KSTACK_CAP {
                        CORE_PENDING_KSTACK[my_core][len] = kstack;
                        CORE_PENDING_KSTACK_LEN[my_core] = len + 1;
                    } else {
                        // Queue overflow (>8 sequential self-kills): free immediately.
                        // Bounded risk — less likely than permanently leaking the stack.
                        super::free_kstack(kstack);
                    }
                }
            }
            // Release slot now — no zombie period, no starvation.
            task_slot_lock();
            TASK_VALID[slot] = false;
            task_slot_unlock();
            return;
        }

        // Cross-kill: free kstack immediately (RSP is on a different kstack).
        if TASK_IS_USER[slot] {
            // SAFETY: Cross-kill — our RSP is on the supervisor's kstack, not K_a.
            super::free_kstack(TASK_KERNEL_STACK_TOP[slot]);
        }

        // Release the slot under the lock.  Re-store Dead to guard against a
        // concurrent wake_by_slot that may have set state=Ready between the
        // top-of-function store and now.
        task_slot_lock();
        TASK_STATE[slot].store(TaskState::Dead as u8, Ordering::Release);
        TASK_VALID[slot] = false;
        task_slot_unlock();
    }
}

/// Block the currently-running task and switch to the next ready task.
///
/// Returns the wakeup result code (0 = success, negative = IpcError).
/// Re-enables interrupts before returning.
///
/// The caller must have already recorded the blocking reason in the routing
/// table (under its spinlock) *before* calling this function. The routing
/// spinlock must be released *before* this call.
pub fn block_and_reschedule(state: TaskState) -> i64 {
    // SAFETY: IF=0 (caller ensures this; double-cli is a no-op).
    unsafe {
        core::arch::asm!("cli", options(nostack, nomem));

        let cid  = current_core_id();
        let slot = CORE_CURRENT[cid];
        assert!(slot < MAX_TASKS && TASK_VALID[slot],
                "block_and_reschedule: no running task");

        // Atomically transition Running → Blocked.
        //
        // If wake_by_slot already set state to Ready (between the routing-table
        // unlock and here), compare_exchange sees Running→Ready mismatch and
        // fails → we return immediately instead of overwriting Ready with
        // Blocked (lost-wakeup prevention, §8.4).
        //
        // AcqRel on success, Acquire on failure: in both cases we synchronise
        // with wake_by_slot's Release store so TASK_WAKEUP_ERR is visible.
        if TASK_STATE[slot].compare_exchange(
            TaskState::Running as u8,
            state as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ).is_err() {
            // CAS failed: wake_by_slot already set state to Ready (lost-wakeup prevention).
            core::arch::asm!("sti", options(nostack, nomem));
            return TASK_WAKEUP_ERR[slot];
        }

        let current_ctx = TASK_CTX[slot].assume_init_mut() as *mut TaskContext;

        // Save user_rsp before switching away: the SYSRETQ exit on resume must
        // load this task's RSP, not the value another task wrote to PER_CORE_SYSCALL.
        if TASK_IS_USER[slot] {
            TASK_USER_RSP[slot] =
                crate::arch::x86_64::syscall_entry::PER_CORE_SYSCALL[cid].user_rsp;
        }

        match pick_next(cid) {
            Some(next) => {
                TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
                CORE_CURRENT[cid] = next;
                if TASK_IS_USER[next] {
                    prepare_ring3_switch(cid, next);
                }
                let next_ctx = TASK_CTX[next].assume_init_ref() as *const TaskContext;
                switch_context(current_ctx, next_ctx);
            }
            None => {
                CORE_CURRENT[cid] = IDLE;
                let sched = &raw mut CORE_SCHED_CTX[cid];
                switch_context(current_ctx, sched);
            }
        }

        core::arch::asm!("sti", options(nostack, nomem));
        TASK_WAKEUP_ERR[slot]
    }
}

/// Store `msg` as the last received message for the current task.
pub fn store_recv_message(msg: Message) {
    let cid = current_core_id();
    // SAFETY: CORE_CURRENT[cid] and TASK_RECV_BUF are written only by this core's scheduler path.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS {
            TASK_RECV_BUF[cur] = Some(msg);
        }
    }
}

/// Take (consume) the last received message for the current task.
pub fn take_recv_message() -> Option<Message> {
    let cid = current_core_id();
    // SAFETY: CORE_CURRENT[cid] and TASK_RECV_BUF are written only by this core's scheduler path.
    unsafe {
        let cur = CORE_CURRENT[cid];
        if cur < MAX_TASKS {
            TASK_RECV_BUF[cur].take()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Round-robin scan for the next Ready task pinned to `core_id`.
///
/// Uses `CORE_RR_SLOT` as a sticky scan pointer that advances *past* the
/// selected slot after each pick.  This guarantees every slot in [0, MAX_TASKS)
/// is visited before the pointer wraps, preventing high-numbered slots from
/// being permanently starved by a dense band of ready tasks at lower indices
/// (§9.1 — no service may monopolise a core; §9.3 — yield is advisory).
fn pick_next(core_id: usize) -> Option<usize> {
    // SAFETY: CORE_RR_SLOT is written only by this core's scheduler path.
    let start = unsafe { CORE_RR_SLOT[core_id] };
    for i in 0..MAX_TASKS {
        let idx = (start + i) % MAX_TASKS;
        // Acquire: sees the Ready write from wake_by_slot's Release store.
        // SAFETY: TASK_VALID, TASK_STATE, TASK_CORE are static arrays; idx < MAX_TASKS; read access is safe.
        let ready = unsafe {
            TASK_VALID[idx]
                && TaskState::from(TASK_STATE[idx].load(Ordering::Acquire)) == TaskState::Ready
                && TASK_CORE[idx] == core_id as u32
        };
        if ready {
            // Advance past the selected slot so the next call starts after it.
            unsafe { CORE_RR_SLOT[core_id] = (idx + 1) % MAX_TASKS; }
            return Some(idx);
        }
    }
    None
}
