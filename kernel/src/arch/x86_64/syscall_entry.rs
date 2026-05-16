//! SYSCALL entry stub and per-core context data (§8.2).
//!
//! The SYSCALL instruction (LSTAR target) swaps GS, saves the user RSP,
//! loads the per-task kernel RSP, then calls `syscall_handler`. On return
//! the inverse happens and SYSRETQ re-enters ring-3.

const MAX_CORES: usize = crate::smp::core::MAX_CORES;

/// Per-core data accessed via the GS base register in the SYSCALL stub.
///
/// Layout is fixed: offset 0 = user_rsp, offset 8 = kernel_rsp.
/// The naked assembly stub references these fields as `gs:[0]`, `gs:[8]`.
#[repr(C)]
pub struct PerCoreSyscallData {
    pub user_rsp:   u64,   // offset 0 — saved on SYSCALL entry, restored on SYSRETQ
    pub kernel_rsp: u64,   // offset 8 — pre-loaded kernel stack top for current task
}

// Lives in .data (writable) — the stub writes user_rsp on every SYSCALL entry.
#[link_section = ".data"]
pub static mut PER_CORE_SYSCALL: [PerCoreSyscallData; MAX_CORES] =
    [const { PerCoreSyscallData { user_rsp: 0, kernel_rsp: 0 } }; MAX_CORES];

// ---------------------------------------------------------------------------
// Lock-free serial helpers — used during early init when LOG_LOCK may not
// be safe to acquire (e.g., concurrent AP bring-up, fault handlers).
// ---------------------------------------------------------------------------

#[inline]
unsafe fn ser_putc(c: u8) {
    // SAFETY: port I/O in ring-0; COM1 LSR (0x3FD) and THR (0x3F8) are standard.
    unsafe {
        loop {
            let lsr: u8;
            core::arch::asm!(
                "in al, dx",
                out("al") lsr,
                in("dx") 0x3FDu16,
                options(nostack, nomem),
            );
            if lsr & 0x20 != 0 { break; }
        }
        core::arch::asm!(
            "out dx, al",
            in("dx") 0x3F8u16,
            in("al") c,
            options(nostack, nomem),
        );
    }
}

#[inline]
unsafe fn ser_puts(s: &[u8]) {
    for &c in s {
        // SAFETY: called within an unsafe fn; serial port exclusively owned in early-boot context.
        unsafe { ser_putc(c) };
    }
}

#[inline]
unsafe fn ser_hex64(val: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..16 {
        let nibble = ((val >> ((15 - i) * 4)) & 0xF) as u8;
        buf[2 + i] = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
    }
    // SAFETY: called within an unsafe fn; serial port exclusively owned in early-boot context.
    unsafe { ser_puts(&buf) };
}

#[inline]
unsafe fn ser_u8_dec(val: u8) {
    let h = val / 10;
    let l = val % 10;
    if h > 0 {
        // SAFETY: called within an unsafe fn; serial port exclusively owned in early-boot context.
        unsafe { ser_putc(b'0' + h) };
    }
    // SAFETY: called within an unsafe fn; serial port exclusively owned in early-boot context.
    unsafe { ser_putc(b'0' + l) };
}

/// Initialise per-core GS MSRs for the SYSCALL stub.
///
/// GS invariant (enforced here and maintained by every ISR/trampoline):
///   ring-0: GS.base = &PER_CORE_SYSCALL[core_id]   (kernel per-core data)
///   ring-3: GS.base = 0                             (user's GS; no ring-3 GS use)
///
/// MSR layout:
///   MSR_GS_BASE      (0xC000_0101) = kernel ptr  — active in ring-0
///   IA32_KERNEL_GS_BASE (0xC000_0102) = 0         — active in ring-3; swapgs exchanges them
///
/// `swapgs` on SYSCALL entry: GS.base(0) ↔ KERNEL_GS_BASE(kernel_ptr) → kernel ptr in GS.base ✓
/// `swapgs` on SYSRETQ exit:  GS.base(kernel_ptr) ↔ KERNEL_GS_BASE(0) → 0 in GS.base ✓
/// ring3_entry_trampoline also does `swapgs` to restore user GS before SYSRETQ.
/// timer_isr_stub does conditional `swapgs` when interrupting ring-3.
///
/// # Safety
/// Called once per core during init, before any ring-3 task runs.
pub unsafe fn init_per_core_syscall(core_id: usize) {
    // SAFETY: WRMSR in ring-0 is always valid.
    unsafe {
        let ptr = &raw const PER_CORE_SYSCALL[core_id] as u64;

        // MSR_GS_BASE = kernel ptr — establishes the ring-0 GS invariant.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0101u32,
            in("eax") ptr as u32,
            in("edx") (ptr >> 32) as u32,
            options(nostack, nomem),
        );

        // IA32_KERNEL_GS_BASE = 0 — user's GS value; swapgs exchanges it with GS.base.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0102u32,
            in("eax") 0u32,
            in("edx") 0u32,
            options(nostack, nomem),
        );

        // Diagnostic readback of both MSRs.
        let (gs_lo, gs_hi): (u32, u32);
        let (kgs_lo, kgs_hi): (u32, u32);
        core::arch::asm!("rdmsr",
            in("ecx") 0xC000_0101u32, out("eax") gs_lo, out("edx") gs_hi,
            options(nostack, nomem));
        core::arch::asm!("rdmsr",
            in("ecx") 0xC000_0102u32, out("eax") kgs_lo, out("edx") kgs_hi,
            options(nostack, nomem));
        let gs_rb  = ((gs_hi  as u64) << 32) | (gs_lo  as u64);
        let kgs_rb = ((kgs_hi as u64) << 32) | (kgs_lo as u64);

        ser_puts(b"syscall-init: core ");
        ser_u8_dec(core_id as u8);
        ser_puts(b" GS_BASE=");
        ser_hex64(gs_rb);
        ser_puts(b" KGSB=");
        ser_hex64(kgs_rb);
        ser_putc(b'\n');
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
