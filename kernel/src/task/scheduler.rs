//! Per-core round-robin scheduler — §9.1, §9.3.
//!
//! Each core has a static run queue of up to MAX_TASKS slots.  Tasks never
//! migrate between cores (§9.1).  The 10 ms preemption quantum is enforced
//! by the local APIC timer interrupt; `yield` is advisory (§9.3).
//!
//! Called from:
//!   - `kernel_main` / `ap_main` — initial entry (never returns).
//!   - Timer ISR via `timer_tick_from_irq` — preempts the current task.
//!   - IPC send/recv — blocks/wakes tasks on queue state (Milestone 5).

use core::mem::MaybeUninit;

use crate::arch::x86_64::context_switch::{switch_context, TaskContext};
use crate::capability::cap::{CapError, Capability};
use crate::capability::rights::Rights;
use crate::capability::table::CapTable;
use crate::task::state::TaskState;

// ---------------------------------------------------------------------------
// Per-core run queue.
// ---------------------------------------------------------------------------

const MAX_TASKS: usize = 8;

/// MAX_TASKS used as a sentinel meaning "no task running" (idle).
const IDLE: usize = MAX_TASKS;

// Split into parallel arrays so we can take a raw pointer to one context
// without conflicting with a mutable borrow on another.
static mut TASK_CTX:   [MaybeUninit<TaskContext>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
static mut TASK_CAP:   [MaybeUninit<CapTable>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
static mut TASK_STATE: [TaskState; MAX_TASKS]  = [TaskState::Dead; MAX_TASKS];
static mut TASK_NAME:  [&str; MAX_TASKS]       = [""; MAX_TASKS];
static mut TASK_VALID: [bool; MAX_TASKS]       = [false; MAX_TASKS];
static mut TASK_COUNT: usize = 0;

/// Index of the currently Running task; IDLE when the scheduler loop is active.
static mut CURRENT: usize = IDLE;

/// Saved context for the scheduler loop itself (used when all tasks are idle).
static mut SCHED_CTX: TaskContext = TaskContext {
    rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
    rip: 0, rsp: 0, cr3: 0,
};

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Add a task to the run queue.  Called before preemption is enabled.
pub fn enqueue(name: &'static str, ctx: TaskContext, caps: CapTable) {
    // SAFETY: single-core, called before timer is armed.
    unsafe {
        for i in 0..MAX_TASKS {
            if !TASK_VALID[i] {
                TASK_CTX[i].write(ctx);
                TASK_CAP[i].write(caps);
                TASK_STATE[i] = TaskState::Ready;
                TASK_NAME[i]  = name;
                TASK_VALID[i] = true;
                TASK_COUNT   += 1;
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
/// task's table. Returns the appropriate `CapError` on any failure.
pub fn current_task_lookup_cap(slot: usize, right: Rights) -> Result<Capability, CapError> {
    // SAFETY: IF=0 in syscall context; CURRENT is stable for this core.
    unsafe {
        if CURRENT < MAX_TASKS && TASK_VALID[CURRENT] {
            TASK_CAP[CURRENT].assume_init_ref().get(slot, right)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

/// Remove the capability at `slot` from the current task's table (GRANT transfer).
pub fn current_task_remove_cap(slot: usize) -> Option<Capability> {
    // SAFETY: IF=0; CURRENT stable.
    unsafe {
        if CURRENT < MAX_TASKS && TASK_VALID[CURRENT] {
            TASK_CAP[CURRENT].assume_init_mut().remove(slot)
        } else {
            None
        }
    }
}

/// Insert a capability into the current task's table (incoming GRANT).
pub fn current_task_insert_cap(cap: Capability) -> Result<usize, CapError> {
    // SAFETY: IF=0; CURRENT stable.
    unsafe {
        if CURRENT < MAX_TASKS && TASK_VALID[CURRENT] {
            TASK_CAP[CURRENT].assume_init_mut().insert(cap)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

/// Enter the scheduler loop on this core.  Never returns while any task exists.
pub fn run() -> ! {
    loop {
        match pick_next() {
            Some(next) => {
                // SAFETY: single-core; interrupts disabled around switch.
                unsafe {
                    core::arch::asm!("cli", options(nostack, nomem));
                    TASK_STATE[next] = TaskState::Running;
                    CURRENT = next;
                    let sched = &raw mut SCHED_CTX;
                    let next_ctx = TASK_CTX[next].assume_init_ref() as *const TaskContext;
                    switch_context(sched, next_ctx);
                    // Execution returns here only if every task has been preempted
                    // and the scheduler loop is re-selected (e.g., all tasks blocked).
                    CURRENT = IDLE;
                    core::arch::asm!("sti", options(nostack, nomem));
                }
            }
            None => {
                // No ready tasks; sleep until the next interrupt.
                // SAFETY: `hlt` with IF=1 is safe here.
                unsafe { core::arch::asm!("sti; hlt", options(nostack, nomem)) };
            }
        }
    }
}

/// Called by the timer ISR every ~10 ms to enforce the preemption quantum.
///
/// Marked `#[no_mangle]` so the naked ISR stub in `interrupts.rs` can call it
/// by name without going through a function pointer.
///
/// # Safety
/// Must only be called from the timer ISR with interrupts disabled (IF=0).
#[no_mangle]
pub extern "C" fn timer_tick_from_irq() {
    // SAFETY: IF=0 throughout (interrupt gate clears IF on entry).
    unsafe {
        // Acknowledge the interrupt first so the APIC can deliver the next tick
        // while we are doing the context switch.
        crate::arch::x86_64::boot::apic_send_eoi();

        let prev = CURRENT;

        // The running task used its quantum — transition it back to Ready.
        if prev < MAX_TASKS && TASK_VALID[prev] && TASK_STATE[prev] == TaskState::Running {
            TASK_STATE[prev] = TaskState::Ready;
        }

        // Select the next ready task.
        let next = match pick_next() {
            Some(i) => i,
            None => {
                // No other task ready; let the current one keep running.
                if prev < MAX_TASKS && TASK_VALID[prev] {
                    TASK_STATE[prev] = TaskState::Running;
                }
                return;
            }
        };

        // Nothing to switch if the same task won again (only one ready task).
        if next == prev {
            TASK_STATE[prev] = TaskState::Running;
            return;
        }

        // Arm the incoming task and record which context we're saving to.
        TASK_STATE[next] = TaskState::Running;
        CURRENT = next;

        let current_ctx: *mut TaskContext = if prev < MAX_TASKS && TASK_VALID[prev] {
            // SAFETY: prev is valid and occupied; we have exclusive access (IF=0).
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        } else {
            // We interrupted the scheduler idle loop.
            &raw mut SCHED_CTX
        };

        // SAFETY: both pointers are valid and distinct; IF=0 ensures no re-entry.
        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
        // Execution continues here when this task is rescheduled by a future
        // timer tick.  We return through the ISR stub which restores scratch
        // registers and executes `iretq`.
    }
}

/// Advisory yield: mark the current task Ready and reschedule.
/// Used by the `yield` syscall and cross-core IPI SCHEDULER_TICK (§9.3).
pub fn yield_current() {
    // Disable interrupts for the duration of the context switch.
    // SAFETY: single-core; we re-enable interrupts by returning through the
    // existing interrupt context (ISR or syscall stub).
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };

    // SAFETY: IF=0.
    unsafe {
        let prev = CURRENT;
        if prev < MAX_TASKS && TASK_VALID[prev] && TASK_STATE[prev] == TaskState::Running {
            TASK_STATE[prev] = TaskState::Ready;
        }

        let next = match pick_next() {
            Some(i) => i,
            None => {
                if prev < MAX_TASKS && TASK_VALID[prev] {
                    TASK_STATE[prev] = TaskState::Running;
                }
                core::arch::asm!("sti", options(nostack, nomem));
                return;
            }
        };

        if next == prev {
            TASK_STATE[prev] = TaskState::Running;
            core::arch::asm!("sti", options(nostack, nomem));
            return;
        }

        TASK_STATE[next] = TaskState::Running;
        CURRENT = next;

        let current_ctx: *mut TaskContext = if prev < MAX_TASKS && TASK_VALID[prev] {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        } else {
            &raw mut SCHED_CTX
        };
        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
        // Returns here when rescheduled; re-enable interrupts.
        core::arch::asm!("sti", options(nostack, nomem));
    }
}

/// Wake a task that was blocked on recv (called after IPC enqueue).
pub fn wake(task_id: crate::task::task::TaskId) {
    // SAFETY: single-core; called with IF=0 from IPC path.
    unsafe {
        for i in 0..MAX_TASKS {
            if TASK_VALID[i] {
                // Milestone 5: match by task_id stored in each slot.
                let _ = task_id;
                if TASK_STATE[i] == TaskState::BlockedOnRecv {
                    TASK_STATE[i] = TaskState::Ready;
                    return;
                }
            }
        }
    }
}

/// Block the currently-running task on a send (queue full).
pub fn block_on_send(_endpoint: crate::ipc::endpoint::EndpointId) {
    // Milestone 5: transition current task Running → BlockedOnSend, reschedule.
    todo!("block_on_send: Milestone 5")
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Round-robin: scan from (CURRENT+1) % MAX_TASKS for the next Ready slot.
fn pick_next() -> Option<usize> {
    // SAFETY: read-only scan; called with interrupts disabled or single-core.
    let start = unsafe {
        if CURRENT < MAX_TASKS { (CURRENT + 1) % MAX_TASKS } else { 0 }
    };
    for i in 0..MAX_TASKS {
        let idx = (start + i) % MAX_TASKS;
        // SAFETY: TASK_VALID / TASK_STATE are read atomically (single-core, IF=0).
        let ready = unsafe { TASK_VALID[idx] && TASK_STATE[idx] == TaskState::Ready };
        if ready {
            return Some(idx);
        }
    }
    None
}
