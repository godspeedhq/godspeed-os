// SPDX-License-Identifier: GPL-2.0-only
//! Neutral-scheduler PREEMPTION demo - the timer preempts non-yielding tasks on ARM.
//!
//! The `logger: ready` spawn (spawn.rs) entered ONE service directly, bypassing the scheduler. Full
//! operation needs the neutral `scheduler::run` itself, and - crucially - **preemption of tasks that
//! do not cooperate**: a real service blocks on `recv`, it does not `yield`, so only the timer can
//! take the core away from it. This demo commits three kernel tasks that **spin** (no yield), arms the
//! neutral preemption path (`irq::NEUTRAL_SCHED`), and enters `scheduler::run(0)`. If all three
//! counters advance, the timer really did preempt a non-cooperating task and the neutral scheduler
//! round-robined it.
//!
//! **How preemption reaches the neutral scheduler on ARM.** The timer IRQ (`exceptions::stub_irq`)
//! saves the full interrupted frame on the task's kernel (SVC) stack and calls `arm_irq_dispatch`,
//! which - when `NEUTRAL_SCHED` is set - invokes the neutral `timer_tick_from_irq`. That does the
//! preemptive `switch_context` INTERNALLY (swapping `sp` to the next task's kernel stack); on return
//! the same IRQ stub pops the (now-resumed task's) frame and `rfe`s back to it. The proof is visible
//! in the interleaved output: a task caught **mid-print** by the tick, another running, then the first
//! resuming its half-written line - preemption between arbitrary instructions, not at yield points.

use crate::arch::imp::context_switch::TaskContext;
use crate::arch::imp::{BootInfo, MemoryKind, MemoryRegion};
use super::pl011_write;

const NUM: usize = 3;
const KSTACK: usize = 8192;

#[repr(align(8))]
struct Stacks([[u8; KSTACK]; NUM]);
static mut STACKS: Stacks = Stacks([[0; KSTACK]; NUM]);

/// Each task logs its id + a counter and yields, forever. `unsafe extern "C" fn() -> !` to match
/// `TaskContext::new_kernel`. A spin delay keeps the output readable rather than a flood.
unsafe extern "C" fn task_a() -> ! { run_task(b'A') }
unsafe extern "C" fn task_b() -> ! { run_task(b'B') }
unsafe extern "C" fn task_c() -> ! { run_task(b'C') }

fn run_task(id: u8) -> ! {
    let mut n = 0u32;
    loop {
        let mut line = *b"sched: task X tick ..... (PREEMPTED)\r\n";
        line[12] = id;
        // crude 5-digit counter, no allocation
        let mut v = n; for k in 0..5 { line[22 - k] = b'0' + (v % 10) as u8; v /= 10; }
        pl011_write(&line);
        n += 1;
        // NO yield: the task spins. The timer preempts it (~10 ms) and the neutral scheduler
        // round-robins to the next task - the whole point. If all three counters advance, a
        // non-cooperating task really was preempted.
        for _ in 0..8_000_000 { core::hint::spin_loop(); }
    }
}

/// Bring up the neutral subsystems, commit three kernel tasks, and enter `scheduler::run(0)`.
/// Does not return - `scheduler::run` is the idle loop.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    // Neutral bootstrap (as spawn.rs does): per-core arenas, scheduler slots, capability resources.
    static REGIONS: [MemoryRegion; 2] = [
        MemoryRegion { base: 0, len: 0, kind: MemoryKind::Reserved },
        MemoryRegion { base: 0, len: 0, kind: MemoryKind::Usable },
    ];
    // SAFETY: single-threaded boot; REGIONS filled once before percpu_init reads the BootInfo.
    let regions = unsafe {
        let r = core::ptr::addr_of!(REGIONS) as *mut [MemoryRegion; 2];
        (*r)[0] = MemoryRegion { base: 0, len: reserve_end as u64, kind: MemoryKind::Reserved };
        (*r)[1] = MemoryRegion { base: reserve_end as u64, len: (ram_end - reserve_end) as u64, kind: MemoryKind::Usable };
        &*(core::ptr::addr_of!(REGIONS))
    };
    let boot_info = BootInfo {
        memory_map: regions, kernel_phys_start: 0x8000, kernel_phys_end: reserve_end as u64,
        hhdm_offset: 0, rsdp_addr: 0,
    };
    crate::smp::percpu_init(&boot_info);
    crate::task::scheduler::init_arenas(crate::smp::percpu::num_cores());
    crate::capability::init();

    let entries: [unsafe extern "C" fn() -> !; NUM] = [task_a, task_b, task_c];
    let names: [&'static str; NUM] = ["task-a", "task-b", "task-c"];

    // The tasks share the kernel identity address space (no per-task page tables here), so cr3 is the
    // live TTBR0 - a switch between them never changes address space (no D-cache dance needed).
    let cr3 = crate::arch::imp::page_tables::read_page_table_base();

    for i in 0..NUM {
        let slot = match crate::task::scheduler::reserve_task_slot(0) {
            Some(s) => s,
            None => { pl011_write(b"sched-demo: no task slot\r\n"); loop { core::hint::spin_loop(); } }
        };
        // SAFETY: single-threaded boot; the stack is a distinct static; the slot is freshly reserved.
        unsafe {
            let stack_top = (core::ptr::addr_of_mut!(STACKS.0[i]) as usize + KSTACK) as *mut u8;
            let ctx = TaskContext::new_kernel(entries[i], stack_top, cr3);
            let _ = crate::task::scheduler::task_cap_init_empty(slot); // no caps needed for the demo
            crate::task::scheduler::commit_task(slot, names[i], ctx, false, stack_top as u64, None);
        }
    }

    // Arm the neutral preemption path: from here the timer tick drives scheduler::run's tasks via
    // switch_context, not the early context.rs demo scheduler.
    super::irq::NEUTRAL_SCHED.store(true, core::sync::atomic::Ordering::Relaxed);
    pl011_write(b"sched-demo: 3 SPINNING kernel tasks committed; the TIMER will preempt them.\r\n");
    pl011_write(b"sched-demo: entering scheduler::run(0)...\r\n");
    crate::task::scheduler::run(0)
}
