// SPDX-License-Identifier: GPL-2.0-only
//! The neutral scheduler, preempting non-cooperating tasks on the Pi 4.
//!
//! Milestone 5 proved a context switch between two tasks that *called* the switch. That is
//! cooperative scheduling, and it is not what a real service does: a service blocks on `recv`, it
//! never yields, so the only thing that can take the core away from it is the timer. This runs the
//! **neutral** `scheduler::run` over three kernel tasks that deliberately **spin** - no yield, no
//! syscall, nothing cooperative - and lets the 100 Hz tick preempt them.
//!
//! The proof is in the interleaving, not in the fact that it finishes. Each task prints a line and
//! then spins; if all three counters advance, a task that never gave up the core had it taken away.
//!
//! **How preemption reaches the neutral scheduler here.** The timer PPI is taken to the IRQ vector,
//! which saves the full interrupted frame on the task's own kernel stack and calls
//! `aarch64_irq_dispatch`. That retires the interrupt at the GIC (before anything else - see the note
//! there) and, once `NEUTRAL_SCHED` is set, calls the neutral `timer_tick_from_irq`, which performs
//! the preemptive `switch_context` internally. The switch swaps SP to the next task's kernel stack, so
//! the `ret` chain unwinds through *that* task's earlier trip through this same vector and restores
//! the frame it was interrupted at. Nothing here is aarch64-specific except the vector and the GIC
//! ordering; the scheduling decision is the same neutral code x86 runs.

use crate::arch::imp::context_switch::TaskContext;
use super::put_str;

const NUM: usize = 3;

/// 64 KiB per task, matching what the neutral scheduler assumes elsewhere. Fixed, visible, and
/// allocated once in `.bss` (§26.6.1) rather than from the frame allocator, so a failure here cannot
/// be confused with an allocator problem.
const KSTACK: usize = 64 * 1024;

#[repr(align(16))]
struct Stacks([[u8; KSTACK]; NUM]);
static mut STACKS: Stacks = Stacks([[0; KSTACK]; NUM]);

unsafe extern "C" fn task_a() -> ! { run_task(b'A') }
unsafe extern "C" fn task_b() -> ! { run_task(b'B') }
unsafe extern "C" fn task_c() -> ! { run_task(b'C') }

/// Print an id and a counter, then spin. Never yields - that is the whole point.
fn run_task(id: u8) -> ! {
    let mut n = 0u32;
    loop {
        let mut line = *b"sched: task X tick 00000 (never yielded)\r\n";
        line[12] = id;
        let mut v = n;
        for k in 0..5 {
            line[23 - k] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        put_str(&line);
        n += 1;
        // Long enough that the ~10 ms quantum expires several times inside it. If the counters still
        // advance for all three ids, the timer really did preempt a task mid-spin.
        for _ in 0..4_000_000 {
            core::hint::spin_loop();
        }
    }
}

/// Commit three spinning kernel tasks and enter `scheduler::run(0)`. Does not return - `run` IS the
/// idle loop from here on.
pub fn run(boot_info: &crate::arch::imp::BootInfo) -> ! {
    // The neutral bootstrap (per-core arenas, scheduler slots, capability table) now happens in the
    // main boot, because a real syscall needs it long before this demo runs.
    let _ = boot_info;

    let entries: [unsafe extern "C" fn() -> !; NUM] = [task_a, task_b, task_c];
    let names: [&'static str; NUM] = ["task-a", "task-b", "task-c"];

    // All three share the kernel identity address space, so the page-table base is simply the live
    // TTBR0 and a switch between them never changes address space. Per-task page tables are the next
    // milestone; until then `switch_context`'s TTBR0 branch is not taken (said plainly there too).
    let ttbr = crate::arch::imp::page_tables::read_page_table_base();

    for i in 0..NUM {
        let slot = match crate::task::scheduler::reserve_task_slot(0) {
            Some(s) => s,
            None => {
                put_str(b"sched-demo: WARN no task slot available - cannot run\r\n");
                loop { core::hint::spin_loop(); }
            }
        };
        // SAFETY: single-threaded boot. Each stack is a distinct static this module owns, the slot was
        // just reserved for this task alone, and `ttbr` is the live page-table base.
        unsafe {
            let stack_top = (&raw mut STACKS.0[i] as *mut u8).add(KSTACK);
            let ctx = TaskContext::new_kernel(entries[i], stack_top, ttbr);
            let _ = crate::task::scheduler::task_cap_init_empty(slot); // the demo needs no caps
            crate::task::scheduler::commit_task(slot, names[i], ctx, false, stack_top as u64, None);
        }
    }

    // Mask IRQs while arming, so a tick cannot land between setting the flag and entering `run` - at
    // that moment the scheduler owns preemption but has not yet been entered, and the switch would
    // target a core state that does not exist. `run` re-enables.
    super::interrupts::disable_interrupts();
    super::exceptions::NEUTRAL_SCHED.store(true, core::sync::atomic::Ordering::Relaxed);
    put_str(b"sched-demo: 3 SPINNING kernel tasks committed; the TIMER must preempt them\r\n");
    put_str(b"sched-demo: entering the NEUTRAL scheduler::run(0)\r\n");
    crate::task::scheduler::run(0)
}
