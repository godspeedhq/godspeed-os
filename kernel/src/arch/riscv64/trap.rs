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

/// Rust side of a trap. Reports and halts.
///
/// Takes nothing and reads the CSRs itself, because the entry stub deliberately saves no registers:
/// this path does not return, so preserving state it will never restore would be ceremony. When
/// faults become survivable that changes, and the stub grows a context save at the same time.
extern "C" fn trap_report() -> ! {
    let (scause, sepc, stval): (u64, u64, u64);
    // SAFETY: reading CSRs has no side effects.
    unsafe {
        core::arch::asm!(
            "csrr {0}, scause",
            "csrr {1}, sepc",
            "csrr {2}, stval",
            out(reg) scause, out(reg) sepc, out(reg) stval,
            options(nomem, nostack)
        );
    }

    if REPORTING.swap(true, Ordering::Relaxed) {
        // Already reporting: a fault inside the reporter. Stop rather than recurse.
        super::halt();
    }

    let interrupt = scause >> 63 != 0;
    let code = scause & 0x7fff_ffff_ffff_ffff;

    super::print_str("\nriscv64: TRAP - ");
    super::print_str(cause_name(code, interrupt));
    super::print_str("\n  scause ");
    super::print_hex(scause);
    super::print_str("  sepc ");
    super::print_hex(sepc);
    super::print_str("  stval ");
    super::print_hex(stval);
    super::print_str("\n");
    // `stval` carries the faulting ADDRESS for a page or access fault and the offending
    // INSTRUCTION for an illegal-instruction trap, so it is printed raw and named by the cause
    // above rather than labelled something it might not be.
    super::print_str("riscv64: halted - faults are not yet survivable (no task to kill)\n");
    super::halt();
}

/// Trap entry.
///
/// Saves nothing: `trap_report` does not return. A recovering handler would need the full integer
/// context saved here first, and this stub is where that goes when it exists.
///
/// `.p2align 2` is emitted INSIDE the body rather than requested as an attribute, because Rust has
/// no stable way to align a function and `stvec` steals the low two bits for MODE. A misaligned
/// handler address would silently become a different mode with a truncated target - which is
/// checked below rather than assumed, since a wrong `stvec` is only discovered by a fault, and a
/// fault is exactly what it is supposed to catch.
#[unsafe(naked)]
unsafe extern "C" fn trap_entry() -> ! {
    core::arch::naked_asm!(".p2align 2", "j {report}", report = sym trap_report)
}

/// Point `stvec` at the reporter.
///
/// Called as early as there is a UART to report through - which is the whole value of it. Every
/// fault before this line is silent, and every fault after it names itself.
pub fn init() -> bool {
    let addr = trap_entry as usize;
    if addr & 0x3 != 0 {
        // Refuse rather than install a handler the hardware would reinterpret. Reported by the
        // caller, which still has a working UART at this point; installing it would trade a
        // visible refusal for an invisible one.
        return false;
    }
    // SAFETY: `trap_entry` is a real code address in the kernel image and the check above proves
    // its low two bits are clear, so MODE is 0 (direct) and the address is not truncated.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) addr, options(nostack));
    }
    true
}
