// SPDX-License-Identifier: GPL-2.0-only
//! RISC-V `ecall` syscall entry - the gateway from a task into the kernel.
//!
//! A userspace task executes `ecall`; the CPU takes an "environment call from U-mode" exception
//! (`scause` 8) and lands in the trap vector, which hands the frame here. This is the RISC-V twin of
//! ARM's `svc` entry and x86's `syscall`: marshal the number and arguments out of the frame, call the
//! **neutral** `syscall::dispatch::syscall_handler`, and put its `i64` result back where the caller
//! will look for it.
//!
//! **The ABI is the platform's own, so the marshalling is free.** The caller sets `a7 = number`,
//! `a0/a1/a2 = arguments` and executes `ecall`; the result comes back in `a0`. That is the standard
//! RISC-V Linux/SBI convention, which the SDK will already be written against, and it happens to be
//! exactly the shape `syscall_handler(number, arg0, arg1, arg2) -> i64` wants. On a 64-bit target
//! each `u64` parameter is one register, so there is no widening step - unlike ARMv7, where passing
//! 32-bit registers to a `u64`-parameter function read the arguments SHIFTED, and showed up as a
//! wrong echo. Named here because the same class of bug is what this file's selftest exists to catch.
//!
//! **The frame IS the ABI.** The trap entry has already saved every register, so reading an argument
//! is an array index and returning a result is a store - and `sepc` is a field too, which is how the
//! `ecall` gets stepped over. Nothing here writes a CSR or touches the user's stack.
//!
//! **No real task exists yet**, and every genuine handler in the neutral dispatcher reads the current
//! task's capability table. So the selftest proves what can honestly be proved now: that arguments
//! survive the privilege transition, that the path is re-entrant, and that the NEUTRAL dispatcher is
//! genuinely reached and returns - the last via a number it does not know, which it rejects before
//! touching any task state. What is left untested is the handlers themselves, which is a statement
//! about there being no tasks rather than about this path.

use super::trap::{TrapFrame, REG_A0, REG_A1, REG_A2, REG_A7};

/// The syscall number the boot selftest uses for its echo. Answered here rather than by the neutral
/// dispatcher, and ONLY while the user-mode selftest is armed; outside that window it is an ordinary
/// unknown number and the neutral dispatcher rejects it like any other.
pub(super) const ECHO_NUMBER: u64 = 0x5555_0003;

/// A number the neutral dispatcher does not implement. Its answer (`-1`) is the evidence that the
/// real dispatcher was entered and returned, and it is safe to issue with no task running because
/// the unknown-number arm returns before reading any task state.
pub(super) const UNKNOWN_NUMBER: u64 = 0x7fff;

/// The two argument sets the selftest sends.
///
/// SINGLE-SOURCED: the user stub loads these constants and the checker expects `echo` of the same
/// ones, so the two halves cannot drift into disagreeing about what was sent. What that deliberately
/// does NOT do is make the test self-fulfilling - the values travel through a privilege transition
/// and back between those two uses, which is the entire thing being measured.
pub(super) const ECHO_ARGS_1: [u64; 3] = [1, 2, 3];
pub(super) const ECHO_ARGS_2: [u64; 3] = [4, 5, 6];

/// Mix all three arguments into one value, so a correct result proves EVERY argument survived the
/// privilege transition rather than only that control came back. A dropped or shifted argument gives
/// a wrong digit, in the position that names which one.
pub(super) const fn echo(a0: u64, a1: u64, a2: u64) -> i64 {
    (a0 as i64) * 100 + (a1 as i64) * 10 + (a2 as i64)
}

/// Handle one `ecall` taken from user mode.
///
/// Steps over the `ecall` unconditionally: `sepc` points AT the instruction that trapped, not past
/// it, so returning without advancing would re-execute it forever - a live lock that presents as a
/// task making no progress with no fault to explain it. Four bytes exactly; `ecall` has no compressed
/// encoding, so this is a fact about the instruction rather than an assumption about the compiler.
pub(super) fn dispatch(frame: &mut TrapFrame) {
    let number = frame.x[REG_A7];
    let a0 = frame.x[REG_A0];
    let a1 = frame.x[REG_A1];
    let a2 = frame.x[REG_A2];

    let result = if number == ECHO_NUMBER && super::usermode::selftest_armed() {
        echo(a0, a1, a2)
    } else {
        // SAFETY: the neutral handler is `unsafe extern "C"` because it is entered from a privilege
        // transition and must trust nothing it is handed - which is the contract, not a hazard, and
        // it validates every argument itself. The four values come straight out of the trap frame,
        // so they are exactly what the caller placed in its own registers.
        unsafe { crate::syscall::dispatch::syscall_handler(number, a0, a1, a2) }
    };

    frame.x[REG_A0] = result as u64;
    frame.sepc = frame.sepc.wrapping_add(4);
}
