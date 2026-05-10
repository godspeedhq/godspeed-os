//! SYSCALL entry stub and per-core context data (§8.2).
//!
//! The SYSCALL instruction (LSTAR target) swaps GS, saves the user RSP,
//! loads the per-task kernel RSP, then calls `syscall_handler`. On return
//! the inverse happens and SYSRETQ re-enters ring-3.

const MAX_CORES: usize = crate::smp::core::MAX_CORES;

/// Per-core data accessed via the GS base register in the SYSCALL stub.
///
/// Layout is fixed: offset 0 = user_rsp, offset 8 = kernel_rsp.
/// The naked assembly stub references these fields as `gs:[0]` and `gs:[8]`.
#[repr(C)]
pub struct PerCoreSyscallData {
    pub user_rsp:   u64,   // offset 0 — saved on SYSCALL entry, restored on SYSRETQ
    pub kernel_rsp: u64,   // offset 8 — pre-loaded kernel stack top for current task
}

// Lives in .data (writable) — the stub writes user_rsp on every SYSCALL entry.
#[link_section = ".data"]
pub static mut PER_CORE_SYSCALL: [PerCoreSyscallData; MAX_CORES] =
    [const { PerCoreSyscallData { user_rsp: 0, kernel_rsp: 0 } }; MAX_CORES];

/// Write `IA32_KERNEL_GS_BASE` (MSR 0xC000_0102) for the calling core.
///
/// The SYSCALL stub uses `swapgs` to swap GS.base with this MSR value,
/// making `PER_CORE_SYSCALL[core_id]` reachable as `gs:[0]` / `gs:[8]`.
///
/// # Safety
/// Called once per core during init, before any ring-3 task runs.
pub unsafe fn init_per_core_syscall(core_id: usize) {
    // SAFETY: WRMSR in ring-0 is always valid; 0xC000_0102 is IA32_KERNEL_GS_BASE.
    unsafe {
        let ptr = &raw const PER_CORE_SYSCALL[core_id] as u64;
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0102u32,
            in("eax") ptr as u32,
            in("edx") (ptr >> 32) as u32,
            options(nostack, nomem),
        );
    }
}

/// SYSCALL entry point — installed as the LSTAR MSR target.
///
/// On hardware SYSCALL entry:
///   RCX = saved user RIP (return address for SYSRETQ)
///   R11 = saved user RFLAGS
///   RSP = user RSP (CPU does NOT change it)
///   RFLAGS masked by SFMASK (IF cleared by our SFMASK = 0x200)
///
/// # Safety
/// Called at the ring-3 → ring-0 SYSCALL boundary. Interrupts disabled
/// (SFMASK). All user-supplied register values are untrusted.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    // SAFETY: swapgs exchanges GS.base ↔ IA32_KERNEL_GS_BASE, making our
    // per-core PerCoreSyscallData pointer live in GS.  We save user RSP to
    // gs:[0] and load the kernel RSP from gs:[8].  cli before the exit
    // sequence ensures no interrupt fires while RSP points into user space
    // (prevents writing kernel interrupt frames into user memory).
    core::arch::naked_asm!(
        "swapgs",
        "mov gs:[0], rsp",      // save user RSP → PER_CORE_SYSCALL.user_rsp
        "mov rsp, gs:[8]",      // load kernel RSP ← PER_CORE_SYSCALL.kernel_rsp
        "push r11",             // save user RFLAGS (hardware placed in r11)
        "push rcx",             // save user RIP (hardware placed in rcx)
        // Rearrange to System V AMD64: syscall_handler(nr, a0, a1, a2)
        //   rdi=nr   rsi=a0   rdx=a1   rcx=a2
        // On entry:  rax=nr   rdi=a0   rsi=a1   rdx=a2
        // rcx/r11 are already saved on the stack; safe to overwrite now.
        "mov rcx, rdx",         // a2 → 4th param (rcx)
        "mov rdx, rsi",         // a1 → 3rd param (rdx)
        "mov rsi, rdi",         // a0 → 2nd param (rsi)
        "mov rdi, rax",         // nr → 1st param (rdi)
        "call syscall_handler",
        // Restore ring-3 state.  cli prevents an interrupt from firing
        // between loading user RSP and SYSRETQ (Meltdown-class safety).
        "cli",
        "pop rcx",              // user RIP → rcx (SYSRETQ uses this as new RIP)
        "pop r11",              // user RFLAGS → r11 (SYSRETQ restores; IF bit re-enables
                                //                    interrupts in ring-3)
        "mov rsp, gs:[0]",      // restore user RSP
        "swapgs",               // restore user GS.base
        "sysretq",
    )
}
