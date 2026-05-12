//! Raw SYSCALL instruction wrapper — shared by all syscall wrappers in the SDK.
//!
//! This is the only place in the SDK that issues raw assembly.
//! Calling convention matches `syscall_entry.rs`:
//!   rax = syscall number, rdi = arg0, rsi = arg1, rdx = arg2.
//!
//! SYSCALL clobbers rcx (saves user RIP) and r11 (saves user RFLAGS).
//! The kernel also does NOT restore rdi, rsi, rdx, r8, r9, r10 on return
//! (those are caller-saved in the C ABI the kernel uses internally).
//! All six must be declared inout/lateout so the compiler never assumes they
//! retain their pre-syscall values across a call boundary.

/// Issue a three-argument SYSCALL and return the i64 result in rax.
///
/// # Safety
/// Caller must pass valid arguments for the given syscall number.
#[inline]
pub(crate) unsafe fn raw_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    // SAFETY: SYSCALL transitions to ring-0 and back; valid from ring-3 at any time.
    // The inout(...) => _ constraints tell the compiler the kernel may modify
    // rdi/rsi/rdx on return, preventing it from assuming those values survive.
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => ret,
            inout("rdi") a0 => _,
            inout("rsi") a1 => _,
            inout("rdx") a2 => _,
            lateout("rcx") _,   // SYSCALL stores user RIP here
            lateout("r11") _,   // SYSCALL stores user RFLAGS here
            lateout("r8")  _,   // caller-saved; kernel may modify
            lateout("r9")  _,
            lateout("r10") _,
            options(nostack),
        );
    }
    ret
}
