// SPDX-License-Identifier: GPL-2.0-only
//! Two USER services under the scheduler at once - the banked-register trap-frame proof (increment 3a).
//!
//! `sched_user.rs` ran ONE user task, which let `stub_irq` skip saving the USER-banked `SP_usr`/
//! `LR_usr` (nothing else touched the USER bank across a round trip). IPC needs *two* services talking,
//! so two user tasks must coexist in ring 3 - and now `stub_irq` stacks the banked registers per task
//! (see `exceptions::stub_irq`). This is the isolation test for that change: load **two** independent
//! USER tasks (two `logger` instances, each in its own address space) plus spinning kernel tasks, run
//! them all under `scheduler::run`, and confirm both reach PL0 and keep running with no corruption. If
//! the banked frame were wrong, the second user task's ring-3 execution would clobber the first's user
//! stack and one would fault - so "both `logger: ready`, no fault, system stays live" is the proof.
//!
//! Once this holds, `sched_ipc` grows into real send/recv between the two (increment 3b): an endpoint,
//! a send cap for one and a receive cap for the other, and a message crossing between them.

use core::sync::atomic::Ordering;

use crate::arch::imp::context_switch::TaskContext;
use super::pl011_write;
use super::spawn::USER_STACK_TOP;

const NUM_USER: usize = 2;
const NUM_KERNEL: usize = 2;
const KSTACK: usize = 8192;

/// One kernel stack per task (user tasks need one for their trap frames; kernel tasks run on theirs).
#[repr(align(8))]
struct Stacks([[u8; KSTACK]; NUM_USER + NUM_KERNEL]);
static mut STACKS: Stacks = Stacks([[0; KSTACK]; NUM_USER + NUM_KERNEL]);

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
        for _ in 0..8_000_000 { core::hint::spin_loop(); }
    }
}

/// Bring up the neutral subsystems, load TWO logger instances as scheduled USER tasks, add two
/// spinning kernel tasks, arm preemption, and enter `scheduler::run(0)`. Does not return.
pub fn run(ram_end: u32, reserve_end: u32) -> ! {
    super::spawn::neutral_bootstrap(ram_end, reserve_end);
    let boot_cr3 = super::page_tables::read_page_table_base();

    // --- Two independent USER tasks (each its own address space, kernel stack, LOG_WRITE cap). ---
    for u in 0..NUM_USER {
        match super::spawn::load_logger_into_slot() {
            Some(svc) => {
                // SAFETY: single-threaded boot; distinct static kernel stack per task; the slot is
                // freshly reserved by the loader. `new_user`'s first switch drops to PL0.
                unsafe {
                    let kstack_top = (core::ptr::addr_of_mut!(STACKS.0[u]) as usize + KSTACK) as *mut u8;
                    let ctx = TaskContext::new_user(
                        kstack_top,
                        svc.entry as u64,
                        USER_STACK_TOP as u64,
                        svc.pt_root as u64,
                    );
                    crate::task::scheduler::commit_task(
                        svc.slot, "logger", ctx, false, kstack_top as u64, None,
                    );
                }
            }
            None => { pl011_write(b"sched-ipc: a logger failed to load\r\n"); }
        }
    }
    pl011_write(b"sched-ipc: TWO logger instances committed as scheduled USER tasks.\r\n");

    // --- Two spinning KERNEL tasks, so the round-robin spans both privilege levels and both users. ---
    let entries: [unsafe extern "C" fn() -> !; NUM_KERNEL] = [kspin_a, kspin_b];
    let names: [&'static str; NUM_KERNEL] = ["kspin-a", "kspin-b"];
    for i in 0..NUM_KERNEL {
        let slot = match crate::task::scheduler::reserve_task_slot(0) {
            Some(s) => s,
            None => { pl011_write(b"sched-ipc: no task slot for kernel task\r\n"); break; }
        };
        // SAFETY: single-threaded boot; distinct static stack; freshly reserved slot.
        unsafe {
            let stack_top = (core::ptr::addr_of_mut!(STACKS.0[NUM_USER + i]) as usize + KSTACK) as *mut u8;
            let ctx = TaskContext::new_kernel(entries[i], stack_top, boot_cr3);
            let _ = crate::task::scheduler::task_cap_init_empty(slot);
            crate::task::scheduler::commit_task(slot, names[i], ctx, false, stack_top as u64, None);
        }
    }

    // Make every service page-table descriptor visible to the non-cacheable walker ONCE.
    // SAFETY: pure cache maintenance; all page tables are built by this point.
    unsafe { super::page_tables::clean_invalidate_dcache_all(); }

    super::irq::NEUTRAL_SCHED.store(true, Ordering::Relaxed);
    pl011_write(b"sched-ipc: entering scheduler::run(0) - TWO user services now run under preemption.\r\n");
    crate::task::scheduler::run(0)
}
