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
/// ring3_entry_trampoline and syscall_entry both do `swapgs` before IRETQ to
/// restore user GS.  timer_isr_stub does conditional `swapgs` when interrupting ring-3.
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

// ---------------------------------------------------------------------------
// Safe user-pointer wrappers (arch permitted layer).
// ---------------------------------------------------------------------------

/// The highest valid user virtual address (exclusive) — top of the lower half.
const USER_END: u64 = 0x0000_8000_0000_0000;

/// Return `true` iff the byte range `[ptr, ptr+len)` lies entirely within the
/// user address space and does not wrap.
#[inline]
pub fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if ptr == 0 || len == 0 { return false; }
    if ptr >= USER_END { return false; }
    match ptr.checked_add(len as u64) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

/// Borrow a user-space byte slice.  Returns `None` if the range is invalid.
///
/// The returned slice is valid for the current syscall frame lifetime; the
/// caller must not retain it across a reschedule point.
#[inline]
pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
    if !validate_user_ptr(ptr, len) { return None; }
    // SAFETY: range validated above; lies in user-space VA (below USER_END),
    // which is disjoint from all kernel mappings.  Caller is in syscall
    // context (IF=0); no task migration can occur.
    Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
}

/// Copy `src` into the user-space address `dst`.  Returns `false` if the
/// destination range is invalid; the copy is a no-op in that case.
#[inline]
pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
    if !validate_user_ptr(dst, src.len()) { return false; }
    // SAFETY: range validated above; user-space VA disjoint from kernel.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
    true
}

/// Read the processor's time-stamp counter.
#[inline]
pub fn read_cycle_counter() -> u64 {
    // SAFETY: RDTSC is always available on x86_64 in ring-0 and has no
    // side effects other than reading the counter.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// SYSCALL entry point — installed as the LSTAR MSR target.
///
/// On hardware SYSCALL entry:
///   RCX = saved user RIP (return address for ring-3 resume)
///   R11 = saved user RFLAGS
///   RSP = user RSP (CPU does NOT change it)
///   RFLAGS masked by SFMASK (IF cleared by our SFMASK = 0x200)
///
/// We return to ring-3 via IRETQ (not SYSRETQ) because on KVM with AMD host
/// CPUs, SYSRETQ sets SS = STAR+8 = 0x20 (RPL=0) instead of 0x23 (RPL=3).
/// The next hardware interrupt from ring-3 then pushes SS=0x20 into the
/// interrupt frame; the IRETQ in the timer ISR sees SS.RPL(0) ≠ SS.DPL(3)
/// and faults with #GP(0x20).  An explicit IRETQ frame hard-codes SS=0x23,
/// bypassing SYSRETQ's AMD-specific SS behaviour entirely.
///
/// # Safety
/// Called at the ring-3 → ring-0 SYSCALL boundary. Interrupts disabled
/// (SFMASK). All user-supplied register values are untrusted.
#[unsafe(naked)]
pub unsafe extern "C" fn syscall_entry() {
    // SAFETY: swapgs exchanges GS.base ↔ IA32_KERNEL_GS_BASE, making our
    // per-core PerCoreSyscallData pointer live in GS.  We save user RSP to
    // gs:[0] and load the kernel RSP from gs:[8].  cli before the exit
    // sequence ensures no interrupt fires while we build the IRETQ frame.
    //
    // Exit register roles:
    //   rcx  = user RIP  (saved by hardware on SYSCALL; restored from kstack)
    //   r11  = user RFLAGS (saved by hardware on SYSCALL; restored from kstack)
    //   r10  = user RSP   (read from PER_CORE_SYSCALL.user_rsp before swapgs)
    //   rax  = syscall return value (untouched so ring-3 sees the result)
    //
    // IRETQ frame built on kernel stack (high → low):
    //   [RSP+32] SS    = 0x23  (user data, DPL=3, RPL=3)
    //   [RSP+24] RSP   = user_rsp
    //   [RSP+16] RFLAGS = user_rflags (IF=1 → re-enables interrupts atomically)
    //   [RSP+8]  CS    = 0x2b  (user code, DPL=3, RPL=3, L=1)
    //   [RSP+0]  RIP   = user_rip
    //
    // GS invariant: swapgs before iretq sets GS.base=0 (user) and
    // KERNEL_GS_BASE=kernel_ptr, so the next SYSCALL's swapgs restores it.
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
        // Build IRETQ frame to restore ring-3.  cli ensures no interrupt fires
        // while the frame is partially built on the kernel stack.
        "cli",
        "pop rcx",              // user RIP → rcx
        "pop r11",              // user RFLAGS → r11 (IF=1; iretq re-enables atomically)
        "mov r10, gs:[0]",      // user RSP → r10 (must read while kernel GS is live)
        "swapgs",               // GS.base: kernel_ptr → 0 (user); KERNEL_GS_BASE: 0 → kernel_ptr
        "push 0x23",            // SS  = 0x23 (user data, DPL=3, RPL=3)
        "push r10",             // RSP = user_rsp
        "push r11",             // RFLAGS (IF=1)
        "push 0x2b",            // CS  = 0x2b (user code, DPL=3, RPL=3, L=1)
        "push rcx",             // RIP = user_rip
        "iretq",                // → ring-3: RIP=user_rip, CS=0x2b, RFLAGS, RSP=user_rsp, SS=0x23
    )
}
