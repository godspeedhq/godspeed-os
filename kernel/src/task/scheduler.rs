//! Per-core round-robin scheduler — §9.1, §9.3.
//!
//! Each core has its own run queue. Tasks never migrate between cores (§9.1).
//! The 10 ms preemption quantum is enforced by the local APIC timer interrupt;
//! `yield()` is advisory and does not bypass preemption (§9.3).
//!
//! This module is called from:
//!   - `kernel_main` / `ap_main` — initial scheduler entry (never returns).
//!   - The timer ISR — preempts the current task after 10 ms.
//!   - IPC send/recv — blocks/wakes tasks on queue state.

use crate::task::task::{Task, TaskId};
use crate::task::state::TaskState;

/// Enter the scheduler loop on the calling core. Never returns.
pub fn run() -> ! {
    loop {
        let next = pick_next();
        if let Some(task) = next {
            switch_to(task);
        } else {
            // No runnable tasks; idle until the next interrupt.
            // SAFETY: `hlt` with IF=1 is safe in the idle path.
            unsafe { core::arch::asm!("sti; hlt", options(nostack, nomem)) };
        }
    }
}

/// Called from the timer ISR every 10 ms to preempt the current task.
pub fn timer_tick() {
    todo!("mark current task Ready, call pick_next, context-switch if a different task is chosen")
}

/// Wake a task that was blocked on recv (called after IPC enqueue).
pub fn wake(task_id: TaskId) {
    todo!("find task by id on this core's queue, transition BlockedOnRecv → Ready")
}

/// Block the currently-running task on a send (queue full).
pub fn block_on_send(endpoint: crate::ipc::endpoint::EndpointId) {
    todo!("transition current task Running → BlockedOnSend, record endpoint, call pick_next")
}

fn pick_next() -> Option<TaskId> {
    todo!("round-robin over Ready tasks on this core's run queue")
}

fn switch_to(task_id: TaskId) {
    todo!("call arch::x86_64::context_switch::switch_context(current, next)")
}
