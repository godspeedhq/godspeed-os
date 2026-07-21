// SPDX-License-Identifier: GPL-2.0-only
//! Neutral-scheduler demo - proving `scheduler::run` round-robins real tasks on ARM.
//!
//! The `logger: ready` spawn (spawn.rs) entered ONE service directly, bypassing the scheduler. Full
//! operation needs the neutral `scheduler::run` itself: its task table, `pick_next`, and
//! `switch_context` driving multiple tasks. This demo commits three kernel tasks that log and
//! `yield_current`, then hands control to `scheduler::run(0)` - if they interleave forever, the
//! neutral scheduler runs on ARM, which is the foundation the supervisor and every service stand on.
//!
//! Cooperative first, on purpose: the tasks yield (a syscall-driven scheduling point), so this
//! exercises the scheduler's task management + `switch_context` without the timer-preemption rework
//! (running the IRQ on per-task kernel stacks) that real, non-yielding services need. That is the
//! next increment; this proves the layer beneath it.

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
        let mut line = *b"sched: task X tick ..... (yields)\r\n";
        line[12] = id;
        // crude 5-digit counter, no allocation
        let mut v = n; for k in 0..5 { line[22 - k] = b'0' + (v % 10) as u8; v /= 10; }
        pl011_write(&line);
        n += 1;
        for _ in 0..3_000_000 { core::hint::spin_loop(); } // slow the cadence for readable output
        crate::task::scheduler::yield_current();           // hand the core back to the scheduler
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

    pl011_write(b"sched-demo: 3 kernel tasks committed; entering scheduler::run(0)...\r\n");
    crate::task::scheduler::run(0)
}
