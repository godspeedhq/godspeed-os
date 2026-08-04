// SPDX-License-Identifier: GPL-2.0-only
//! A real EL0 task running **under the neutral scheduler**, preempted by the timer.
//!
//! Every previous EL0 excursion was a one-shot: the boot dropped to EL0, the task ran to completion,
//! and control came back. That proves the mechanism but not the thing an OS needs, which is an EL0 task
//! that is *scheduled* - entered by the scheduler, interrupted mid-instruction by the timer, resumed
//! later, and sharing the core with others.
//!
//! What has to hold for that, and did not have to hold before:
//!
//! - **Entry through the scheduler.** `switch_context` ends in `ret` and stays at EL1; entering EL0
//!   needs an `eret`. `TaskContext::new_user` bridges that with a trampoline (see `context`).
//! - **The task's kernel stack must be right.** After the `eret`, `SP_EL1` is whatever the switch left,
//!   and that is the stack every later trap from this task lands on. A wrong one runs fine at EL0 and
//!   corrupts on the first syscall.
//! - **Preemption across the EL boundary.** The timer fires while the core is at EL0; the IRQ vector
//!   saves the frame on the task's kernel stack, the neutral tick switches away, and resuming unwinds
//!   back through that vector to `eret` into EL0 again.
//! - **A separate address space per task.** `switch_context` installs `TTBR0` on a change; the kernel
//!   is in `TTBR1` and is untouched.
//!
//! The user payload counts and reports through a syscall, so its progress is visible. If its counter
//! keeps rising while the kernel tasks also run, an EL0 task really is sharing the core.

use super::page_tables::{PageFlags, PageTable, VirtAddr};
use super::{put_dec, put_str};
use crate::arch::imp::context_switch::TaskContext;
use crate::memory::allocator::alloc_frame;

const USER_CODE_VA: u64 = 0x1_0000;
const USER_STACK_VA: u64 = 0x2_0000;
const PAGE: u64 = 4096;

/// The bring-up syscall this task uses to report progress. Above every real number, like the rest of
/// the bring-up range.
pub const SYS_TICK: u64 = super::usermode::BRINGUP_SYSCALL_BASE + 4;

/// 64 KiB kernel stack, matching what the neutral scheduler assumes elsewhere.
const KSTACK: usize = 64 * 1024;
#[repr(align(16))]
struct KStack([u8; KSTACK]);
static mut USER_KSTACK: KStack = KStack([0; KSTACK]);

core::arch::global_asm!(
    r#"
    .section .rodata
    .balign 4
    .globl el0_loop_start
    .globl el0_loop_end
// Position-independent: it is linked into the kernel at a high address and executed at a low one, so
// it may not reference any address. Counts, reports through a syscall, spins, repeats - and never
// yields, so only the timer can take the core away from it.
el0_loop_start:
    mov  x21, #0                     // the counter, in a callee-saved register so the switch keeps it
1:  mov  x8, #0x1004                 // SYS_TICK
    mov  x0, x21
    svc  #0
    add  x21, x21, #1
    mov  x22, #0x300000              // spin, long enough for several quanta to expire inside it
2:  subs x22, x22, #1
    b.ne 2b
    b    1b
el0_loop_end:
"#
);

extern "C" {
    static el0_loop_start: u8;
    static el0_loop_end: u8;
}

/// How many times the EL0 task has reported in. Read by the kernel tasks to prove it is progressing.
static mut EL0_TICKS: u64 = 0;

/// Handle the EL0 task's progress syscall.
pub fn tick(frame: &mut super::exceptions::TrapFrame) {
    // SAFETY: single-threaded bring-up; this static is this module's.
    unsafe { EL0_TICKS += 1 };
    let n = frame.x[0];
    if n < 6 {
        put_str(b"    [EL0 task] tick ");
        put_dec(n);
        put_str(b" (scheduled, preemptible, in its own address space)\r\n");
    }
    frame.x[0] = 0;
}

/// Ticks the EL0 task has reported.
pub fn el0_ticks() -> u64 {
    // SAFETY: as above.
    unsafe { EL0_TICKS }
}

/// Build the EL0 task's address space and commit it to a scheduler slot.
///
/// Returns false if anything could not be allocated - loudly, rather than committing a half-built task
/// the scheduler would then run.
pub fn spawn_el0_task() -> bool {
    let Ok(mut space) = PageTable::new() else { return false };
    let (Some(code), Some(stack)) = (alloc_frame(), alloc_frame()) else { return false };

    // SAFETY: `code` is a frame the allocator just handed us, reached through the kernel's direct map;
    // the blob symbols bracket a position-independent sequence in this image's `.rodata`.
    unsafe {
        let src = core::ptr::addr_of!(el0_loop_start);
        let len = core::ptr::addr_of!(el0_loop_end) as usize - src as usize;
        if len == 0 || len as u64 > PAGE {
            return false;
        }
        let dst = (super::mmu::KERNEL_VA_BASE + code.phys_addr().0) as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, len);
        // The bytes just written are CODE. Without this the core may fetch whatever the
        // I-cache still holds for this physical frame - which, when the allocator recycles a
        // frame another task executed from, is that task's instructions.
        super::mmu::sync_instruction_cache(dst as u64, len);
    }

    if space
        .map(VirtAddr(USER_CODE_VA), code.phys_addr(), PageFlags::PRESENT | PageFlags::USER)
        .is_err()
        || space
            .map(
                VirtAddr(USER_STACK_VA),
                stack.phys_addr(),
                PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC,
            )
            .is_err()
    {
        return false;
    }

    let Some(slot) = crate::task::scheduler::reserve_task_slot(0) else { return false };
    let ttbr = space.into_cr3();

    // SAFETY: single-threaded boot. The kernel stack is a distinct static this module owns; the slot
    // was just reserved for this task alone; and the user entry and stack are mapped with EL0 access in
    // `ttbr`, which the switch installs before the trampoline `eret`s.
    unsafe {
        let kstack_top = (&raw mut USER_KSTACK.0 as *mut u8).add(KSTACK);
        let ctx = TaskContext::new_user(kstack_top, USER_CODE_VA, USER_STACK_VA + PAGE, ttbr);
        let _ = crate::task::scheduler::task_cap_init_empty(slot);
        crate::task::scheduler::commit_task(slot, "el0-task", ctx, true, kstack_top as u64, None);
    }
    true
}
