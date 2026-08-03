// SPDX-License-Identifier: GPL-2.0-only
//! EL0 entry and the `svc` syscall path - the last mechanism before the neutral kernel can host a task.
//!
//! Dropping to EL0 is an `eret` with `SPSR_EL1.M = 0b0000` (EL0t) instead of the EL1h the boot drop
//! used. EL0 has its own stack pointer, `SP_EL0`, which must be set separately - a detail that fails
//! in a confusing way if missed, because the CPU happily enters EL0 with a garbage SP and faults on
//! the first push, pointing at the user code rather than at the entry that set it up.
//!
//! **The syscall number comes from `ESR_EL1`, not from a register.** The hardware records the `imm16`
//! the `svc` instruction carried, so userspace cannot claim a different call by clobbering a register
//! on the way in. Arguments still arrive in registers, and those are untrusted exactly as §18's syscall
//! rules say - every one is validated before use.
//!
//! This is bring-up scaffolding: it proves EL0 entry, the `svc` round trip, a return value, and a
//! clean exit back to the kernel. It is not the syscall ABI - that arrives with the neutral kernel's
//! `syscall_handler`, which this path will feed once tasks exist.

use super::context::TaskContext;
use super::exceptions::TrapFrame;
use super::{put_dec, put_hex, put_str};

/// Demo syscall numbers, chosen to be obviously not-the-real-ABI.
const SYS_ECHO: u64 = 0;
const SYS_EXIT: u64 = 1;
const SYS_VERDICT: u64 = 2;

/// A magic the demo user task sends, and the value the kernel hands back, so the round trip is checked
/// rather than assumed - a syscall that returns nothing observable proves only that it did not crash.
const ECHO_ARG: u64 = 0x4242_4242;
const ECHO_REPLY: u64 = 0x1234_5678;

/// 16 KiB EL0 stack, in the linker's `.el0` region so it shares a 2 MiB block with the EL0 code and
/// NOT with kernel code - see `mmu::el0_region` for why that separation is mandatory rather than tidy.
/// Sized once and visible (§26.6.1). It costs image space rather than `.bss` because the region has to
/// be placed, and that is the price of the separation.
const USER_STACK: usize = 16 * 1024;
#[link_section = ".el0.data"]
static mut EL0_STACK: [u8; USER_STACK] = [0; USER_STACK];

/// Where to resume when the user task exits. Saved before dropping to EL0.
static mut CTX_KERNEL: TaskContext = TaskContext::empty();
/// Scratch context the exit path saves into; never resumed.
static mut CTX_DISCARD: TaskContext = TaskContext::empty();

/// Set by the syscall handler when the echo round trip carried the right value both ways.
static mut ECHO_OK: bool = false;
/// Number of syscalls serviced.
static mut CALLS: u64 = 0;

/// Handle one `svc`. Called from the synchronous-lower-EL vector with the user's saved frame.
///
/// Mutating `frame.x[0]` is how a return value reaches userspace: the vector's restore path reloads
/// x0 from the frame on its way back out, so writing it here IS the return.
pub fn syscall(number: u64, frame: &mut TrapFrame) {
    // SAFETY: single-threaded bring-up; these statics are owned by this module and the demo is the
    // only caller.
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
            frame.x[0] = ECHO_REPLY; // the restore path reloads x0 from here
        }
        SYS_VERDICT => {
            if frame.x[0] == 1 {
                put_str(b"    [EL0] round trip OK - kernel reply arrived in x0\r\n");
            } else {
                // SAFETY: single-threaded bring-up.
                unsafe { ECHO_OK = false };
                put_str(b"    [EL0] CORRUPT - wrong value came back in x0\r\n");
            }
        }
        SYS_EXIT => {
            put_str(b"    [EL0] svc #1 (exit) - leaving EL0\r\n");
            // Do not `eret` back to a task that asked to stop. Switch to the kernel context saved
            // before we entered EL0; the abandoned EL0 frame goes with the stack we leave behind.
            // SAFETY: CTX_KERNEL was saved by `run` immediately before the drop to EL0, and nothing
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

/// The EL0 task. Runs unprivileged: any system register it touched would trap.
///
/// `extern "C"` and `naked`-free on purpose - it is ordinary compiled code, which is the point. If a
/// plain Rust function can run at EL0 and come back through `svc`, the mechanism is real.
#[link_section = ".el0.text"]
extern "C" fn el0_task() -> ! {
    let reply: u64;
    // SAFETY: `svc` is the architectural syscall instruction; at EL0 it traps to EL1's synchronous
    // lower-EL vector, which services it and returns with x0 set. The clobbers say the kernel may use
    // the caller-saved set.
    unsafe {
        core::arch::asm!(
            "mov x0, {arg}",
            "svc #0",
            "mov {out}, x0",
            arg = in(reg) ECHO_ARG,
            out = out(reg) reply,
            out("x0") _,
            options(nostack),
        );
    }

    // Report the verdict THROUGH the kernel, as a syscall argument, rather than by calling a kernel
    // print function. EL0 cannot execute kernel `.text` - that separation is the entire reason this
    // task lives in its own 2 MiB region - so calling into it would fault immediately. This is the
    // first place the EL0/EL1 boundary is real rather than notional.
    let verdict: u64 = if reply == ECHO_REPLY { 1 } else { 0 };
    // SAFETY: `svc #2` hands the verdict to the kernel, which prints it and returns.
    unsafe {
        core::arch::asm!(
            "mov x0, {v}",
            "svc #2",
            v = in(reg) verdict,
            out("x0") _,
            options(nostack),
        );
    }

    // SAFETY: `svc #1` asks the kernel to stop this task; it does not return.
    unsafe { core::arch::asm!("svc #1", options(nostack)) };
    loop {
        // SAFETY: WFE is always valid. Unreachable - the exit syscall does not come back.
        unsafe { core::arch::asm!("wfe") };
    }
}

/// Drop to EL0, run the demo task, and come back. Returns true if the round trip checked out.
pub fn run() -> bool {
    // SAFETY: single-threaded bring-up; the statics are this module's.
    unsafe {
        let sp = (&raw const EL0_STACK as u64) + USER_STACK as u64;
        let sp = sp & !0xF; // EL0 faults on a misaligned SP the moment anything pushes

        put_str(b"aarch64: dropping to EL0 (SPSR M=0b0000, SP_EL0 set separately)\r\n");

        // Save where to come back to, then enter EL0. `el0_entry` never returns here directly - the
        // exit syscall switches back into CTX_KERNEL, which resumes just after this call.
        super::context::switch(&raw mut CTX_KERNEL, &raw const CTX_KERNEL_ENTRY_SHIM);

        ECHO_OK && CALLS >= 2
    }
}

/// A context whose `lr` is the EL0 entry stub, so `run` can reach EL0 through the same switch
/// machinery it will use for real tasks. Built lazily by `prepare`.
static mut CTX_KERNEL_ENTRY_SHIM: TaskContext = TaskContext::empty();
/// Stack for the shim (it only runs the few instructions that `eret` into EL0).
static mut SHIM_STACK: [u8; 4096] = [0; 4096];

/// Prepare the shim context. Split from `run` so the unsafe setup is one place.
pub fn prepare() {
    // SAFETY: single-threaded bring-up; module-owned statics.
    unsafe {
        let top = ((&raw const SHIM_STACK as u64) + 4096) & !0xF;
        (*(&raw mut CTX_KERNEL_ENTRY_SHIM)).init(enter_el0, top);
    }
}

/// Perform the actual drop to EL0.
extern "C" fn enter_el0() -> ! {
    // SAFETY: sets SP_EL0 to the demo stack, ELR_EL1 to the EL0 entry, SPSR_EL1 to EL0t with DAIF
    // clear so the timer keeps ticking at EL0, then `eret`. All three must be set before the eret;
    // entering EL0 with an unset SP_EL0 faults on the first push and points at the user code rather
    // than here.
    unsafe {
        let sp = ((&raw const EL0_STACK as u64) + USER_STACK as u64) & !0xF;
        core::arch::asm!(
            "msr sp_el0, {sp}",
            "msr elr_el1, {entry}",
            "msr spsr_el1, {spsr}",
            "eret",
            sp = in(reg) sp,
            entry = in(reg) el0_task as usize as u64,
            spsr = in(reg) 0u64,   // M[3:0]=0000 (EL0t), DAIF clear - IRQs stay live at EL0
            options(noreturn, nostack),
        );
    }
}
