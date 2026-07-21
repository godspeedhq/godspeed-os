// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7 SVC syscall entry - the gateway from a task into the kernel.
//!
//! A userspace task issues `svc #0`; the CPU takes a supervisor-call exception and lands here. This
//! is the ARM twin of x86's `ud2`/syscall entry: marshal the number and arguments, call the
//! **neutral** `syscall::dispatch::syscall_handler`, and return its `i64` result to the caller.
//!
//! **The ABI is chosen to match AAPCS so the call is almost free.** The caller sets `r0 = number`,
//! `r1 = arg0`, `r2 = arg1`, `r3 = arg2` and executes `svc #0`. Those are exactly the registers
//! `syscall_handler(number, arg0, arg1, arg2)` takes, and its `i64` result comes back in `r0:r1` -
//! which is where an AAPCS caller expects a 64-bit return. So the entry mostly just has to preserve
//! the right registers around the call.
//!
//! **The ARM trap: `svc` targets SVC mode, and the kernel already runs in SVC mode.** So on entry the
//! banked `LR_svc` (the return address) and `SPSR_svc` (the caller's CPSR) are live and must be saved
//! *first thing*, exactly as a nested exception would - otherwise the next kernel `bl` clobbers the
//! return address. Done that way, the same entry works whether the caller was a ring-3 task (USR) or,
//! as in this milestone's selftest, kernel code (SVC): the saved SPSR carries the caller's mode and
//! `movs pc, lr` restores it.
//!
//! **No user tasks exist yet (that is increment 3).** Every real syscall handler touches the current
//! task's cap table, which needs a running task and scheduler. So this milestone proves the *entry
//! mechanism* - registers in, result out, clean resume across the mode switch - through a test
//! dispatch, and leaves the real `syscall_handler` wired for when tasks arrive.

use core::sync::atomic::{AtomicBool, Ordering};

use super::pl011_write;
use super::timer::write_dec_pub;

/// When set, `arm_svc_dispatch` routes to a self-contained echo instead of the real syscall handler.
/// The real handler dereferences per-task state that does not exist until tasks do; the selftest
/// needs to prove the *entry path* without that, so it flips this for the duration of one call.
static SVC_TEST_MODE: AtomicBool = AtomicBool::new(false);

/// The C-ABI dispatch the SVC entry calls with `(number, arg0, arg1, arg2)` in `r0-r3`.
///
/// **Arguments are `u32`, not `u64`, and that is load-bearing on 32-bit ARM.** AAPCS passes each
/// `u32` in one register, so `(r0, r1, r2, r3)` map straight to the four parameters. The neutral
/// `syscall_handler` takes `u64`s - on a 32-bit target each of *those* is a register pair, so passing
/// `r0-r3` to a `u64`-parameter function directly would read the arguments shifted (the bug that
/// first showed up as a wrong echo). We take `u32`s here and widen to `u64` for the neutral call,
/// which the compiler then marshals correctly. Every syscall argument on this arch (pointers,
/// handles, lengths) fits in 32 bits, so the widening is loss-free.
///
/// Returns the `i64` the caller receives in `r0:r1`. In test mode it returns a value mixing all four
/// inputs, so a correct result *proves every argument survived the mode switch*, not just that
/// control returned.
#[no_mangle]
pub(super) extern "C" fn arm_svc_dispatch(number: u32, arg0: u32, arg1: u32, arg2: u32) -> i64 {
    if SVC_TEST_MODE.load(Ordering::Relaxed) {
        return (number as i64 * 1000) + (arg0 as i64 * 100) + (arg1 as i64 * 10) + arg2 as i64;
    }
    // SAFETY: the neutral handler is `unsafe extern "C"`; it reads the current task's kernel state,
    // valid once a task is running (the only way a real `svc` reaches here). Widen each 32-bit arg to
    // the u64 the neutral ABI uses - correct marshalling on a 32-bit target.
    unsafe {
        crate::syscall::dispatch::syscall_handler(number as u64, arg0 as u64, arg1 as u64, arg2 as u64)
    }
}

/// Issue a syscall via `svc #0` - the exact sequence the SDK will use, here to drive the selftest.
///
/// # Safety
/// Traps into the kernel; valid for any syscall number the handler accepts. In this milestone it is
/// only called under `SVC_TEST_MODE`, so it cannot reach a real handler with no task context.
unsafe fn issue_svc(number: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `svc #0` is the supervisor-call instruction; the handler preserves callee-saved
    // registers and returns the i64 result in r0:r1. r0-r3 are set to the ABI inputs and marked
    // clobbered (the handler owns them); r12 is caller-saved and may be used by the handler.
    unsafe {
        core::arch::asm!(
            "svc #0",
            inout("r0") number as u32 => lo,
            inout("r1") a0 as u32 => hi,
            inout("r2") a1 as u32 => _,
            inout("r3") a2 as u32 => _,
            lateout("r12") _,
            // Manual clobber list rather than clobber_abi("C"): the soft-float target has no VFP
            // registers, so the C ABI's `s0..` clobbers do not exist and rustc rejects them. The
            // handler preserves r4-r11 and sp itself, so only the caller-saved integer regs matter.
            options(nostack),
        );
    }
    ((hi as u64) << 32 | lo as u64) as i64
}

/// Prove the SVC entry path: issue real `svc #0` traps and confirm the arguments arrive and the
/// result returns, across the mode save/restore.
///
/// The result is a mix of *all four* inputs, so this checks more than "we came back": a wrong value
/// means an argument was dropped or a register was clobbered by the mode switch, which is exactly the
/// class of bug an entry stub gets wrong. A second call confirms the path is re-entrant (the first
/// did not corrupt `LR_svc`/`SPSR_svc` for the next).
pub fn selftest() {
    SVC_TEST_MODE.store(true, Ordering::Relaxed);

    // SAFETY: test mode is on, so the SVC handler routes to the echo, never a real syscall.
    let r1 = unsafe { issue_svc(7, 3, 4, 5) };     // expect 7*1000 + 3*100 + 4*10 + 5 = 7345
    let r2 = unsafe { issue_svc(1, 2, 3, 4) };     // expect 1234 - proves the path is re-entrant

    SVC_TEST_MODE.store(false, Ordering::Relaxed);

    pl011_write(b"arm32: svc selftest - trap 1 returned ");
    write_dec_pub(r1 as u32);
    pl011_write(b" (want 7345), trap 2 returned ");
    write_dec_pub(r2 as u32);
    pl011_write(b" (want 1234)\r\n");

    if r1 == 7345 && r2 == 1234 {
        pl011_write(b"arm32: svc PASS (svc #0 traps to dispatch, all args survive, result returns)\r\n");
    } else {
        pl011_write(b"arm32: svc FAIL - the syscall entry dropped an argument or the return path is wrong\r\n");
    }
}
