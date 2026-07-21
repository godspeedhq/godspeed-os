// SPDX-License-Identifier: GPL-2.0-only
//! Run a real GodspeedOS SERVICE *through* the neutral scheduler on ARM - preemptively.
//!
//! `spawn.rs` proved a service can run unprivileged, but entered it DIRECTLY (bypassing the scheduler)
//! and so could run exactly one, un-preempted. This is the merge: the same logger service, loaded into
//! a task slot and handed to `scheduler::run` as a preemptible USER task alongside spinning KERNEL
//! tasks. The timer preempts the user task in ring 3 (its trap frame lands on its own kernel stack),
//! the neutral scheduler round-robins, and it resumes where it left off - the foundation every
//! multi-service milestone (IPC, supervisor, shell) then builds on.
//!
//! **Why one user task here, and what the next increment adds.** With a single USER task, the ARM IRQ
//! trap frame need not save the banked `SP_usr`/`LR_usr`: no *other* user task runs to clobber them
//! across the round trip (the kernel spinners run in SVC and never touch the USER bank; the kernel
//! itself uses the SVC bank). The moment a SECOND user service exists (the IPC ping/pong pair),
//! `stub_irq` must also stack the user banked registers - deferred to that increment so this one
//! changes the least.
//!
//! **Per-task address spaces are real here.** The user task has its own page table (TTBR0); the kernel
//! spinners and the idle context share the boot identity map. `switch_context` re-points TTBR0 and
//! flushes the TLB (SEC-26/27) whenever the user task enters or leaves, and the service's descriptors
//! are made visible to the non-cacheable walker by a one-shot D-cache clean before `scheduler::run`.

use core::sync::atomic::Ordering;

use crate::arch::imp::context_switch::TaskContext;
use super::pl011_write;
use super::spawn::USER_STACK_TOP;

const NUM_KERNEL: usize = 2;
const KSTACK: usize = 8192;

/// One kernel stack per spinning kernel task, plus one for the user task's trap frames.
#[repr(align(8))]
struct Stacks([[u8; KSTACK]; NUM_KERNEL + 1]);
static mut STACKS: Stacks = Stacks([[0; KSTACK]; NUM_KERNEL + 1]);

/// A spinning KERNEL task: proves the user task is round-robined against something that never yields,
/// and that a user<->kernel switch (which changes TTBR0 and flushes the TLB) works both ways.
unsafe extern "C" fn kspin_a() -> ! { kspin(b'A') }
unsafe extern "C" fn kspin_b() -> ! { kspin(b'B') }

fn kspin(id: u8) -> ! {
    let mut n = 0u32;
    loop {
        let mut line = *b"sched: kernel task X tick ..... (spinning)\r\n";
        line[19] = id;
        let mut v = n; for k in 0..5 { line[29 - k] = b'0' + (v % 10) as u8; v /= 10; }
        pl011_write(&line);
        n += 1;
        // NO yield: only the timer takes the core away. If these advance WHILE the logger keeps
        // serving, the user task is being preempted and resumed correctly across the privilege change.
        for _ in 0..8_000_000 { core::hint::spin_loop(); }
    }
}

/// Bring up the neutral subsystems, load the logger as a scheduled USER task, add two spinning kernel
/// tasks, arm the neutral preemption path, and enter `scheduler::run(0)`. Does not return.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);

    // The boot (idle/scheduler) address space, shared by the kernel spinners.
    let boot_cr3 = super::page_tables::read_page_table_base();

    // --- The USER task: the logger, loaded into its own address space, committed to the scheduler. ---
    match super::spawn::load_logger_into_slot() {
        Some(svc) => {
            // SAFETY: single-threaded boot; STACKS entries are distinct statics; the slot is freshly
            // reserved by the loader. `new_user` builds a context whose first switch drops to PL0 at
            // the service entry, on its own kernel stack (for later trap frames), under its own TTBR0.
            unsafe {
                let kstack_top = (core::ptr::addr_of_mut!(STACKS.0[NUM_KERNEL]) as usize + KSTACK) as *mut u8;
                let ctx = TaskContext::new_user(
                    kstack_top,
                    svc.entry as u64,
                    USER_STACK_TOP as u64,
                    svc.pt_root as u64,
                );
                // is_user=false: on ARM a task's ring is encoded in its context (the PL0 trampoline)
                // and trap-frame SPSR, not in this flag. The flag only drives the x86 SYSRET user-RSP
                // plumbing, which ARM does not use; setting it false skips that dead path cleanly.
                crate::task::scheduler::commit_task(
                    svc.slot, "logger", ctx, false, kstack_top as u64, None,
                );
            }
            // Mark it a USER task so the timer runs its syscalls atomically (no mid-syscall preemption).
            super::irq::mark_task_user(svc.slot);
            pl011_write(b"sched-user: logger committed as a scheduled USER task.\r\n");
        }
        None => {
            pl011_write(b"sched-user: no logger - continuing with kernel tasks only.\r\n");
        }
    }

    // --- Two spinning KERNEL tasks, to prove round-robin across the privilege boundary. ---
    let entries: [unsafe extern "C" fn() -> !; NUM_KERNEL] = [kspin_a, kspin_b];
    let names: [&'static str; NUM_KERNEL] = ["kspin-a", "kspin-b"];
    for i in 0..NUM_KERNEL {
        let slot = match crate::task::scheduler::reserve_task_slot(0) {
            Some(s) => s,
            None => { pl011_write(b"sched-user: no task slot for kernel task\r\n"); break; }
        };
        // SAFETY: single-threaded boot; distinct static stack; freshly reserved slot.
        unsafe {
            let stack_top = (core::ptr::addr_of_mut!(STACKS.0[i]) as usize + KSTACK) as *mut u8;
            let ctx = TaskContext::new_kernel(entries[i], stack_top, boot_cr3);
            let _ = crate::task::scheduler::task_cap_init_empty(slot);
            crate::task::scheduler::commit_task(slot, names[i], ctx, false, stack_top as u64, None);
        }
    }

    // Make every service page-table descriptor visible to the (non-cacheable) walker ONCE, before any
    // switch_context installs the user task's TTBR0. Thereafter switch_context only re-points TTBR0 +
    // flushes the TLB; the descriptors do not change, so no per-switch cache maintenance is needed.
    // SAFETY: pure cache maintenance; all page tables are built by this point.
    unsafe { super::page_tables::clean_invalidate_dcache_all(); }

    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-user: entering scheduler::run(0) - a USER service now runs under preemption.\r\n");
    crate::task::scheduler::run(0)
}
