// SPDX-License-Identifier: GPL-2.0-only
//! A real EL0 task, in its own `TTBR0` address space.
//!
//! This replaces the milestone-6 demo, which put its code in a linker-placed `.el0` region and reached
//! it through the kernel's identity map. That only worked while kernel and user shared one address
//! space; once the kernel moved to `TTBR1`, EL0 could not reach it at all (EL0 has no access to the
//! high half), and the demo took a lower-EL instruction abort. This is what it becomes.
//!
//! ## The shape, and why each piece is what it is
//!
//! The task gets **frames**, not linker sections: one page of code and one of stack, allocated from the
//! frame allocator and mapped into a page table that contains **nothing else** - no kernel entries at
//! all, because the kernel lives in `TTBR1` where no `TTBR0` switch can disturb it. The task therefore
//! runs at whatever low virtual addresses we choose, and those addresses are genuinely its own.
//!
//! The code is a small **position-independent** blob, copied into its frame. It has to be
//! position-independent because it is linked as part of the kernel (at a high address) and executed at
//! a low one; it is trivially so, being register operations and `svc` with no memory reference and no
//! branch outside itself.
//!
//! ## What the kernel does NOT have to do
//!
//! It does not have to map itself into the task, grant EL0 access to anything of its own, or arrange
//! for the vectors to be reachable. When the task traps, `TTBR1` still holds the kernel - it was never
//! part of the switch. That absence is the milestone: the address spaces are genuinely separate, and
//! the separation is enforced by hardware rather than by leaving the right bits clear.

use super::context::TaskContext;
use super::exceptions::TrapFrame;
use super::ptables::PageTable;
use super::{put_dec, put_hex, put_str};
use crate::memory::allocator::{alloc_frame, free_frame};
use crate::memory::frame::Frame;

/// Where the task's code and stack live in ITS address space. Low addresses it owns outright - the
/// kernel is not in this table, so nothing here can collide with anything.
const USER_CODE_VA: u64 = 0x1_0000;
const USER_STACK_VA: u64 = 0x2_0000;
const PAGE: u64 = 4096;

/// Demo syscall numbers, deliberately not the real ABI.
const SYS_ECHO: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_VERDICT: u64 = 2;

/// A magic the task sends and the value the kernel returns, so the round trip is checked in both
/// directions rather than assumed - a syscall returning nothing observable proves only that it did not
/// crash.
const ECHO_ARG: u64 = 0x4242_4242;
const ECHO_REPLY: u64 = 0x1234_5678;

core::arch::global_asm!(
    r#"
    .section .rodata
    .balign 4
    .globl el0_blob_start
    .globl el0_blob_end
// Position-independent EL0 payload. Copied into a task frame and executed there, so it must not
// reference any address: every operand below is an immediate or a register.
el0_blob_start:
    movz x0, #0x4242                 // x0 = ECHO_ARG
    movk x0, #0x4242, lsl #16
    svc  #0                          // -> kernel; returns ECHO_REPLY in x0

    movz x1, #0x5678                 // x1 = ECHO_REPLY (what we expect back)
    movk x1, #0x1234, lsl #16
    cmp  x0, x1
    cset x0, eq                      // x0 = 1 if the reply came back intact, else 0
    svc  #2                          // report the verdict THROUGH the kernel

    svc  #1                          // ask the kernel to stop this task; does not return
1:  b    1b                          // unreachable
el0_blob_end:
"#
);

extern "C" {
    static el0_blob_start: u8;
    static el0_blob_end: u8;
}

/// Set by the syscall handler when the echo round trip carried the right value both ways.
static mut ECHO_OK: bool = false;
/// Set when the task reported its own verdict as correct.
static mut VERDICT_OK: bool = false;
/// Number of syscalls serviced.
static mut CALLS: u64 = 0;
/// Where to resume when the task exits, and a scratch to save the dying context into.
static mut CTX_KERNEL: TaskContext = TaskContext::empty();
static mut CTX_DISCARD: TaskContext = TaskContext::empty();

/// Handle one `svc` from the task.
///
/// Writing `frame.x[0]` IS the return value: the vector's restore path reloads x0 from the frame on
/// its way back out.
pub fn syscall(number: u64, frame: &mut TrapFrame) {
    // SAFETY: single-threaded bring-up; these statics belong to this module and the demo is the only
    // caller.
    unsafe { CALLS += 1 };

    match number {
        SYS_ECHO => {
            let arg = frame.x[0];
            put_str(b"    [EL0] svc #0 (echo) arg=");
            put_hex(arg);
            put_str(b" -> returning ");
            put_hex(ECHO_REPLY);
            put_str(b"\r\n");
            // SAFETY: as above.
            unsafe { ECHO_OK = arg == ECHO_ARG };
            frame.x[0] = ECHO_REPLY;
        }
        SYS_VERDICT => {
            let ok = frame.x[0] == 1;
            // SAFETY: as above.
            unsafe { VERDICT_OK = ok };
            if ok {
                put_str(b"    [EL0] round trip OK - kernel reply arrived intact in x0\r\n");
            } else {
                put_str(b"    [EL0] CORRUPT - wrong value came back in x0\r\n");
            }
        }
        SYS_EXIT => {
            put_str(b"    [EL0] svc #1 (exit) - leaving EL0\r\n");
            // Do not return to a task that asked to stop. Switch to the kernel context saved before we
            // entered EL0; `run` resumes just after its switch.
            // SAFETY: `CTX_KERNEL` was saved by `run` immediately before entering the task, and nothing
            // has switched since, so it is a valid context to resume.
            unsafe { super::context::switch(&raw mut CTX_DISCARD, &raw const CTX_KERNEL) };
        }
        _ => {
            put_str(b"    [EL0] WARN unknown svc #");
            put_dec(number);
            put_str(b"\r\n");
            frame.x[0] = u64::MAX; // an unknown call must not look like success
        }
    }
}

/// The task's address space and the frames backing it, kept so `run` can tear them down.
struct UserTask {
    space: PageTable,
    code: Frame,
    stack: Frame,
    ttbr: u64,
}

/// Build the task: a page table containing exactly two pages, with the payload copied into one.
fn build() -> Option<UserTask> {
    let mut space = PageTable::new().ok()?;
    let code = alloc_frame()?;
    let stack = alloc_frame()?;

    // Copy the payload into the code frame, reached through the kernel's high direct map (a physical
    // address is not a pointer here - the low identity map is gone).
    // SAFETY: `code` is a frame the allocator just handed us; the blob symbols bracket a
    // position-independent instruction sequence in `.rodata` that this module owns.
    unsafe {
        let src = core::ptr::addr_of!(el0_blob_start);
        let end = core::ptr::addr_of!(el0_blob_end);
        let len = end as usize - src as usize;
        if len == 0 || len as u64 > PAGE {
            return None; // a blob that does not fit a page is a build error, not a runtime case
        }
        let dst = (super::mmu::KERNEL_VA_BASE + code.phys_addr().0) as *mut u8;
        core::ptr::copy_nonoverlapping(src, dst, len);
    }

    // Code: user, read-only, executable. Stack: user, writable, NOT executable.
    space.map(USER_CODE_VA, code.phys_addr().0, true, false, true).ok()?;
    space.map(USER_STACK_VA, stack.phys_addr().0, true, true, false).ok()?;

    let ttbr = space.ttbr();
    Some(UserTask { space, code, stack, ttbr })
}

/// Run the task and come back. Returns true if the round trip checked out.
pub fn run() -> bool {
    let Some(task) = build() else {
        put_str(b"aarch64: WARN could not build the EL0 task address space\r\n");
        return false;
    };

    put_str(b"aarch64: EL0 task space built - TTBR0=");
    put_hex(task.ttbr);
    put_str(b", code at ");
    put_hex(USER_CODE_VA);
    put_str(b", stack at ");
    put_hex(USER_STACK_VA);
    put_str(b" (NO kernel entries - the kernel is in TTBR1)\r\n");

    // SAFETY: single-threaded bring-up; the statics are this module's.
    unsafe {
        ENTER_TTBR = task.ttbr;
        (*(&raw mut CTX_SHIM)).init(enter_el0, shim_stack_top());
        // Save where to come back to, then enter the task. The exit syscall switches back into
        // CTX_KERNEL, which resumes right here.
        super::context::switch(&raw mut CTX_KERNEL, &raw const CTX_SHIM);
    }

    // Back in the kernel. Retire the task's address space before freeing anything it mapped.
    // SAFETY: we are executing from the high half, so clearing TTBR0 removes only the task's map.
    unsafe { super::mmu::drop_low_map() };

    // SAFETY: both frames came from the allocator and are freed exactly once; the page table's own
    // frames go with `space` when it drops.
    unsafe {
        free_frame(task.code);
        free_frame(task.stack);
    }
    drop(task.space);

    // SAFETY: module-owned statics, single-threaded.
    unsafe { ECHO_OK && VERDICT_OK && CALLS >= 3 }
}

/// The address space to install on the way into EL0.
static mut ENTER_TTBR: u64 = 0;
/// A context whose `lr` is the entry stub, so `run` reaches EL0 through the same switch machinery real
/// tasks will use.
static mut CTX_SHIM: TaskContext = TaskContext::empty();
static mut SHIM_STACK: [u8; 8192] = [0; 8192];

fn shim_stack_top() -> u64 {
    // SAFETY: taking the address of this module's static.
    unsafe { ((&raw const SHIM_STACK as u64) + 8192) & !0xF }
}

/// Install the task's address space and drop to EL0.
extern "C" fn enter_el0() -> ! {
    // SAFETY: installs TTBR0 with the task's table and invalidates (writing TTBR0 flushes nothing on
    // AArch64), then sets SP_EL0, ELR_EL1 and SPSR_EL1 and `eret`s. All three must be set before the
    // eret: entering EL0 with an unset SP_EL0 faults on the first push and points at the user code
    // rather than here. DAIF is left clear so the timer keeps ticking at EL0.
    //
    // The kernel is untouched by the TTBR0 write - it lives in TTBR1 - so the instruction stream, the
    // stack and the UART all survive the switch. That is the property this milestone is proving.
    unsafe {
        let ttbr = ENTER_TTBR;
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            "msr sp_el0, {sp}",
            "msr elr_el1, {entry}",
            "msr spsr_el1, {spsr}",
            "eret",
            ttbr = in(reg) ttbr,
            sp = in(reg) USER_STACK_VA + PAGE,   // stack grows down from the top of its page
            entry = in(reg) USER_CODE_VA,
            spsr = in(reg) 0u64,                 // M[3:0]=0000 (EL0t), DAIF clear
            options(noreturn, nostack),
        );
    }
}
