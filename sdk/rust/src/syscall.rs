//! Raw SYSCALL instruction wrapper — shared by all syscall wrappers in the SDK.
//!
//! This is the only place in the SDK that issues raw assembly.
//! Calling convention matches `syscall_entry.rs`:
//!   rax = syscall number, rdi = arg0, rsi = arg1, rdx = arg2.
//! SYSCALL clobbers rcx (saves user RIP) and r11 (saves user RFLAGS).

/// Issue a three-argument SYSCALL and return the i64 result in rax.
///
/// # Safety
/// Caller must pass valid arguments for the given syscall number.
#[inline]
pub(crate) unsafe fn raw_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    // SAFETY: SYSCALL transitions to ring-0 and back; valid from ring-3 at any time.
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => ret,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,   // clobbered: SYSCALL stores user RIP here
            out("r11") _,   // clobbered: SYSCALL stores user RFLAGS here
            options(nostack),
        );
    }
    ret
}
