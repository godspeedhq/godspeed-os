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
use core::sync::atomic::{AtomicU8, Ordering};

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

pub const MAX_TASKS: usize = 32;
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


// ---------------------------------------------------------------------------
// Per-core scheduler state.
// ---------------------------------------------------------------------------

/// Index of the task currently running on each core (IDLE if none).
static mut CORE_CURRENT: [usize; MAX_CORES] = [IDLE; MAX_CORES];

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

/// Reserve a free task slot, pinned to `core_id`.
///
/// Marks the slot VALID (but state remains Dead) so subsequent calls to
/// `task_cap_init_empty` and `commit_task` can use it.  The slot will not
/// be scheduled until `commit_task` sets its state to Ready.
///
/// Returns `None` if all slots are occupied.
pub fn reserve_task_slot(core_id: u32) -> Option<usize> {
    // SAFETY: IF=0 in syscall context; single writer for this core.
    unsafe {
        for i in 0..MAX_TASKS {
            if !TASK_VALID[i] {
                TASK_VALID[i] = true;
                TASK_CORE[i]  = core_id;
                return Some(i);
            }
        }
        None
    }
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
    // SAFETY: IF=0; slot was reserved by this core.
    unsafe { TASK_VALID[slot] = false; }
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
        TASK_STATE[slot].store(TaskState::Ready as u8, Ordering::Relaxed);
        TASK_NAME[slot]             = name;
        TASK_IS_USER[slot]          = is_user;
        TASK_KERNEL_STACK_TOP[slot] = kernel_stack_top;
        TASK_ENDPOINT[slot]         = endpoint_id;
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
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
        CORE_SCHED_CTX[cid].cr3 = cr3;
    }

    loop {
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
                // SAFETY: `sti; hlt` atomically enables interrupts and halts;
                //         the next interrupt (timer or IPI) will wake the core.
                // `options(nostack)` only — omitting `nomem` so the compiler
                // treats this asm as a memory clobber.  After hlt returns, the
                // IPI handler will have written TASK_STATE; the compiler must
                // not cache the previous None result across this boundary.
                unsafe { core::arch::asm!("sti; hlt", options(nostack)) };
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

        let current_ctx: *mut TaskContext = if prev < MAX_TASKS && TASK_VALID[prev] {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        } else {
            &raw mut CORE_SCHED_CTX[cid]
        };

        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
    }
}

/// Advisory yield: mark the current task Ready and reschedule.
/// Also called from the IPI handler for cross-core WAKE_RECEIVER (§9.4).
pub fn yield_current() {
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };

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
                if prev < MAX_TASKS && TASK_VALID[prev] {
                    TASK_STATE[prev].store(TaskState::Running as u8, Ordering::Relaxed);
                }
                core::arch::asm!("sti", options(nostack, nomem));
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

        let current_ctx: *mut TaskContext = if prev < MAX_TASKS && TASK_VALID[prev] {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        } else {
            &raw mut CORE_SCHED_CTX[cid]
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
            TASK_WAKEUP_ERR[slot] = result;
            // Release ordering: ensures TASK_WAKEUP_ERR is visible to any
            // core that subsequently reads this state with Acquire.
            TASK_STATE[slot].store(TaskState::Ready as u8, Ordering::Release);

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

/// Kill a task by slot: mark Dead, kill its endpoint (if any), notify blocked tasks.
pub fn kill_task_by_slot(slot: usize) {
    // SAFETY: IF=0 or lock-free path; TASK_VALID[slot] checked by caller.
    unsafe {
        if slot >= MAX_TASKS || !TASK_VALID[slot] { return; }

        crate::kprintln!("kill_task: slot={} name='{}' endpoint={:?}",
            slot, TASK_NAME[slot], TASK_ENDPOINT[slot]);

        // Mark Dead atomically — this stops the scheduler from picking it.
        TASK_STATE[slot].store(TaskState::Dead as u8, Ordering::Release);

        // Kill the task's endpoint if it has one.
        if let Some(ep_id) = TASK_ENDPOINT[slot] {
            // Bump generation in routing table and wake any blocked tasks.
            let (rx_slot, tx_slot) = crate::ipc::routing::kill_endpoint(ep_id);
            if let Some(s) = rx_slot { wake_by_slot(s, -7); } // -7 = EndpointDead
            if let Some(s) = tx_slot { wake_by_slot(s, -7); }

            // Mark resource dead in global cap table so generation check fails.
            let resource_id = crate::capability::cap::ResourceId::from(ep_id);
            crate::kprintln!("kill_task: marking ResourceId({}) dead", resource_id.0);
            crate::capability::table::mark_dead_resource(resource_id);
        }

        // Reclaim all user-space frames: walk the page table, flush all TLBs,
        // then return each frame to the allocator (§10.5).
        if TASK_IS_USER[slot] {
            let cr3 = TASK_CTX[slot].assume_init_ref().cr3;
            if cr3 != 0 {
                // SAFETY: cr3 is the task's PML4 set at spawn and immutable
                // until now.  Task is Dead; no core will schedule it.
                let buf = crate::arch::x86_64::page_tables::reclaim_user_frames(cr3);
                // SAFETY: APIC is initialised; broadcast_full_tlb_flush manages
                // its own interrupt-flag save/restore.
                crate::smp::ipi::broadcast_full_tlb_flush();
                for &phys_addr in buf.as_slice() {
                    // SAFETY: phys_addr came from this task's page table, so it
                    // was allocated from the frame allocator and is now ours to free.
                    let frame = crate::memory::frame::Frame::from_phys(
                        crate::memory::frame::PhysAddr(phys_addr)
                    );
                    crate::memory::allocator::free_frame(frame);
                }
                crate::kprintln!(
                    "kill_task: slot={} reclaimed {} frames", slot, buf.as_slice().len()
                );
            }
        }

        // Free the kernel stack back to the pool so it can be reused.
        if TASK_IS_USER[slot] {
            super::free_kstack(TASK_KERNEL_STACK_TOP[slot]);
        }

        // Release the slot so reserve_task_slot can reuse it.
        TASK_VALID[slot] = false;
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
fn pick_next(core_id: usize) -> Option<usize> {
    let current = unsafe { CORE_CURRENT[core_id] };
    let start = if current < MAX_TASKS { (current + 1) % MAX_TASKS } else { 0 };
    for i in 0..MAX_TASKS {
        let idx = (start + i) % MAX_TASKS;
        // Acquire: sees the Ready write from wake_by_slot's Release store.
        let ready = unsafe {
            TASK_VALID[idx]
                && TaskState::from(TASK_STATE[idx].load(Ordering::Acquire)) == TaskState::Ready
                && TASK_CORE[idx] == core_id as u32
        };
        if ready {
            return Some(idx);
        }
    }
    None
}
