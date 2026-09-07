// SPDX-License-Identifier: GPL-2.0-only
//! S-mode trap vector - what turns a fault from silence into a sentence.
//!
//! Until `stvec` is set, a fault in S-mode has nowhere to go. On this hardware that means the
//! machine stops with no output, which is the worst failure this project recognises: invariant 12
//! asks for loud failure, and an unhandled trap is the loudest thing a CPU can do reported as the
//! quietest thing a log can show.
//!
//! This is deliberately a REPORTER, not yet a recovery path. It decodes the cause, prints it with
//! the faulting PC and address, and halts. Killing the offending TASK instead of the machine needs
//! a task to kill - which needs `spawn_supervisor`, which is what this unblocks - so the honest
//! order is: make faults visible first, then make them survivable.
//!
//! `stvec` holds MODE in its low two bits, so the handler address must be four-byte aligned and
//! mode 0 (direct: every cause enters at the same place). Vectored mode exists and buys nothing
//! while there is one handler.

use core::sync::atomic::{AtomicBool, Ordering};

/// Set once a fault has been reported, so a fault INSIDE the reporter cannot recurse forever.
///
/// A trap handler that faults re-enters itself, and on a machine whose console is the thing that
/// faulted this shows up as either a hang or an endless partial line. One flag turns that into a
/// single truncated report, which is still readable.
static REPORTING: AtomicBool = AtomicBool::new(false);

/// Cause codes worth naming. The rest are printed as numbers, because a wrong name is worse than
/// an honest integer.
fn cause_name(code: u64, interrupt: bool) -> &'static str {
    if interrupt {
        return match code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "interrupt",
        };
    }
    match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store/AMO address misaligned",
        7 => "store/AMO access fault",
        8 => "ecall from user mode",
        9 => "ecall from supervisor mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store/AMO page fault",
        _ => "exception",
    }
}

/// Every integer register, as the entry stub laid them out.
///
/// Indexed by register number so the assembly is a straight `sd x{i}, 8*i(sp)` and the two halves
/// cannot drift apart. `x[0]` is the hardwired zero register and is never written; it is kept in
/// the array only so the indices mean what they say.
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 32],
    pub sepc: u64,
}

/// Rust side of a trap.
///
/// RETURNS for anything it can handle, so the stub restores and `sret`s back to what was
/// interrupted. Diverges only by halting, and only for a fault - which is honest while there is no
/// task to kill instead.
#[unsafe(no_mangle)]
extern "C" fn trap_dispatch(frame: &mut TrapFrame) {
    let (scause, stval): (u64, u64);
    // SAFETY: reading CSRs has no side effects.
    unsafe {
        core::arch::asm!(
            "csrr {0}, scause",
            "csrr {1}, stval",
            out(reg) scause, out(reg) stval,
            options(nomem, nostack)
        );
    }
    let interrupt = scause >> 63 != 0;
    let code = scause & 0x7fff_ffff_ffff_ffff;

    if interrupt && code == 5 {
        // Supervisor timer. Acknowledged by SCHEDULING THE NEXT ONE: there is no "clear" bit for
        // it, and leaving the deadline in the past re-raises the interrupt immediately - a live
        // lock that presents as a machine which boots and then does nothing.
        super::timer_tick(frame);
        return;
    }

    if REPORTING.swap(true, Ordering::Relaxed) {
        // A fault inside the reporter. Stop rather than recurse: on a machine whose console is what
        // faulted, recursion shows up as a hang or an endless partial line.
        super::halt();
    }

    super::print_str("
riscv64: TRAP - ");
    super::print_str(cause_name(code, interrupt));
    super::print_str("
  scause ");
    super::print_hex(scause);
    super::print_str("  sepc ");
    super::print_hex(frame.sepc);
    super::print_str("  stval ");
    super::print_hex(stval);
    super::print_str("
");
    // `stval` carries the faulting ADDRESS for a page or access fault and the offending INSTRUCTION
    // for an illegal-instruction trap, so it is printed raw and named by the cause rather than
    // labelled something it might not be.
    super::print_str("riscv64: halted - faults are not yet survivable (no task to kill)
");
    super::halt();
}

/// Bytes of stack a trap frame occupies: 32 registers plus `sepc`, rounded to keep the stack
/// 16-byte aligned as the ABI requires.
const FRAME_BYTES: usize = 34 * 8;

/// Trap entry: save everything, dispatch, restore, return.
///
/// SAVES ALL 31 WRITABLE REGISTERS, not just the caller-saved ones. The interrupted code is not a
/// caller - it did not agree to any calling convention with this handler and may be at any
/// instruction - so "the compiler will have spilled what it needed" is not available here. A
/// callee-saved register clobbered by the dispatcher would corrupt code that never called it, at a
/// point arbitrarily far away.
///
/// `sepc` is saved and restored explicitly: it holds where to resume, and a nested trap (or a
/// dispatcher that faults) would otherwise overwrite it before `sret` reads it.
///
/// The frame lives on the CURRENT stack, which is sound while every trap is taken in S-mode with a
/// valid kernel stack. When user mode arrives this needs `sscratch` to swap stacks first, because a
/// user trap must not push onto a user stack - that is a real change and it belongs with the commit
/// that introduces user mode rather than being half-built now.
#[unsafe(naked)]
unsafe extern "C" fn trap_entry() -> ! {
    core::arch::naked_asm!(
        ".p2align 2",
        "addi sp, sp, -{frame}",
        // t0 first, so it can be used to compute the original stack pointer.
        "sd x5, 40(sp)",
        "addi x5, sp, {frame}",
        "sd x5, 16(sp)",
        "sd x1, 8(sp)",
        "sd x3, 24(sp)",
        "sd x4, 32(sp)",
        "sd x6, 48(sp)",
        "sd x7, 56(sp)",
        "sd x8, 64(sp)",
        "sd x9, 72(sp)",
        "sd x10, 80(sp)",
        "sd x11, 88(sp)",
        "sd x12, 96(sp)",
        "sd x13, 104(sp)",
        "sd x14, 112(sp)",
        "sd x15, 120(sp)",
        "sd x16, 128(sp)",
        "sd x17, 136(sp)",
        "sd x18, 144(sp)",
        "sd x19, 152(sp)",
        "sd x20, 160(sp)",
        "sd x21, 168(sp)",
        "sd x22, 176(sp)",
        "sd x23, 184(sp)",
        "sd x24, 192(sp)",
        "sd x25, 200(sp)",
        "sd x26, 208(sp)",
        "sd x27, 216(sp)",
        "sd x28, 224(sp)",
        "sd x29, 232(sp)",
        "sd x30, 240(sp)",
        "sd x31, 248(sp)",
        "csrr x5, sepc",
        "sd x5, 256(sp)",
        // The frame IS the argument: a0 points at what was just saved.
        "mv a0, sp",
        "call {dispatch}",
        // Resume. `sepc` is restored from the frame so a handler may redirect execution by editing
        // it - which is how a survivable fault will eventually skip or replace a faulting
        // instruction rather than only reporting it.
        "ld x5, 256(sp)",
        "csrw sepc, x5",
        "ld x1, 8(sp)",
        "ld x3, 24(sp)",
        "ld x4, 32(sp)",
        "ld x6, 48(sp)",
        "ld x7, 56(sp)",
        "ld x8, 64(sp)",
        "ld x9, 72(sp)",
        "ld x10, 80(sp)",
        "ld x11, 88(sp)",
        "ld x12, 96(sp)",
        "ld x13, 104(sp)",
        "ld x14, 112(sp)",
        "ld x15, 120(sp)",
        "ld x16, 128(sp)",
        "ld x17, 136(sp)",
        "ld x18, 144(sp)",
        "ld x19, 152(sp)",
        "ld x20, 160(sp)",
        "ld x21, 168(sp)",
        "ld x22, 176(sp)",
        "ld x23, 184(sp)",
        "ld x24, 192(sp)",
        "ld x25, 200(sp)",
        "ld x26, 208(sp)",
        "ld x27, 216(sp)",
        "ld x28, 224(sp)",
        "ld x29, 232(sp)",
        "ld x30, 240(sp)",
        "ld x31, 248(sp)",
        // t0 last: it was the scratch for everything above.
        "ld x5, 40(sp)",
        "addi sp, sp, {frame}",
        "sret",
        frame = const FRAME_BYTES,
        dispatch = sym trap_dispatch,
    )
}

/// Point `stvec` at the entry stub.
///
/// Returns false rather than installing a handler the hardware would reinterpret: `stvec` steals
/// the low two bits for MODE, Rust has no stable way to align a function, and a misaligned address
/// would become a different mode with a truncated target - discovered only by a fault, which is the
/// thing this exists to catch. Checked rather than assumed for that reason.
pub fn init() -> bool {
    let addr = trap_entry as usize;
    if addr & 0x3 != 0 {
        return false;
    }
    // SAFETY: a real code address in the kernel image, proven above to have its low two bits clear,
    // so MODE is 0 (direct) and the address is not truncated.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) addr, options(nostack));
    }
    true
}

/// Enable supervisor timer interrupts and take the first one.
///
/// Two enables, and both are needed: `sie.STIE` admits the timer specifically, `sstatus.SIE` admits
/// interrupts at all. Setting one without the other is a machine that either never ticks or ticks
/// for everything.
pub fn enable_timer_interrupts() {
    // SAFETY: setting the two enable bits. Sound because `stvec` is already installed - doing this
    // first would mean the first tick had nowhere to go.
    unsafe {
        core::arch::asm!(
            "csrs sie, {stie}",
            "csrs sstatus, {sie}",
            stie = in(reg) 1u64 << 5,   // STIE
            sie = in(reg) 1u64 << 1,    // SIE
            options(nostack)
        );
    }
}
