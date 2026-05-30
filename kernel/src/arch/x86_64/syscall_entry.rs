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

/// CSTAR entry point — installed as the CSTAR MSR target (AMD compat-mode SYSCALL).
///
/// On AMD processors, a `syscall` from a ring-3 task in compatibility mode
/// (CS.L=0) dispatches here instead of LSTAR.  GodspeedOS runs all ring-3 code
/// in 64-bit mode (CS.L=1) and uses `ud2` (IDT[6]) as the syscall mechanism, so
/// this entry should never be reached.  If it ever is, halt loudly rather than
/// execute with an unexpected register state.
///
/// # Safety
/// Called at the compat-mode ring-3 → ring-0 SYSCALL boundary. Same register
/// state as LSTAR entry (RCX=user_RIP, R11=user_RFLAGS, RSP=user_RSP).
#[unsafe(naked)]
pub unsafe extern "C" fn cstar_entry() {
    // SAFETY: no stack or GS access; just disables interrupts and halts.
    core::arch::naked_asm!(
        "cli",
        "1: hlt",
        "jmp 1b",
    )
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

/// INT 0x80 syscall entry — kept for reference; superseded by `ud2_syscall_entry`
/// on AMD GX-420GI where int $0x80 also stalls.
///
/// The CPU pushes a full hardware frame onto the kernel stack (via TSS.rsp0)
/// before jumping here, so no manual stack switch is needed:
///   [RSP+0]  saved RIP   (user RIP — return address)
///   [RSP+8]  saved CS    (0x2b: user code, L=1, DPL=3)
///   [RSP+16] saved RFLAGS
///   [RSP+24] saved RSP   (user RSP)
///   [RSP+32] saved SS    (0x23: user data, DPL=3)
///
/// IF is cleared on entry (interrupt gate, type_attr=0xEE).
/// GS invariant: on entry from ring-3, GS.base=0 (user); swapgs installs
/// the kernel pointer so any kernel code that needs per-core data via GS works.
///
/// IDT entry uses DPL=3 so ring-3 code can raise this vector via `int 0x80`.
///
/// # Safety
/// Called at the ring-3 → ring-0 boundary via `int 0x80`. IF=0 on entry.
#[unsafe(naked)]
pub unsafe extern "C" fn int80_entry() {
    core::arch::naked_asm!(
        // Install kernel GS so per-core data is accessible.
        "swapgs",
        // Mirror the SYSCALL path: save user RSP into PER_CORE_SYSCALL.user_rsp
        // at gs:[0] so pf_handler diagnostics and any GS-based user-RSP readers
        // see the correct value.  User RSP is at [RSP+24] in the CPU frame.
        "mov r10, [rsp + 24]",
        "mov gs:[0], r10",
        // Rearrange into syscall_handler(nr, a0, a1, a2) System V convention:
        //   rdi=nr  rsi=a0  rdx=a1  rcx=a2
        // On entry from ring-3: rax=nr  rdi=a0  rsi=a1  rdx=a2
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call syscall_handler",
        // rax = return value (syscall_handler's return value, untouched by iretq).
        // Restore user GS before returning to ring-3.
        "swapgs",
        // iretq pops RIP, CS, RFLAGS, RSP, SS from the CPU-pushed frame and
        // resumes ring-3 at the instruction after the `int 0x80`.
        "iretq",
    )
}

/// UD2 syscall entry — IDT[6] (#UD hardware exception).
///
/// AMD GX-420GI (Jaguar/Puma+): both `syscall` and `int N` (software interrupt
/// dispatch) silently stall the core from ring-3.  `ud2` (0x0F 0x0B) is decoded
/// by the CPU as an explicitly undefined instruction and takes the hardware
/// exception pathway — the same one used by #PF and #GP — which does work on
/// this hardware.
///
/// CPU frame on #UD (no error code):
///   [RSP+ 0] saved RIP   (address of the ud2 opcode; advanced +2 before iretq)
///   [RSP+ 8] saved CS    (0x2b = ring-3, 0x08 = ring-0)
///   [RSP+16] saved RFLAGS
///   [RSP+24] saved RSP   (user RSP)
///   [RSP+32] saved SS
///
/// # Safety
/// Installed at IDT[6]; called automatically by the CPU on any `ud2` instruction.
/// IF=0 on entry (interrupt gate).
#[unsafe(naked)]
pub unsafe extern "C" fn ud2_syscall_entry() {
    core::arch::naked_asm!(
        // CPL check: saved CS is at [rsp+8] in the CPU exception frame.
        // CS=0x08 → ring-0 kernel ud2 (unexpected crash); CS=0x2b → ring-3 syscall.
        "mov r11, [rsp + 8]",
        "cmp r11, 0x08",
        "je 3f",
        // --- Ring-3 syscall path ---
        // Install kernel GS so per-core data is accessible.
        "swapgs",
        // Save user RSP (at [rsp+24]) into PER_CORE_SYSCALL.user_rsp at gs:[0].
        "mov r10, [rsp + 24]",
        "mov gs:[0], r10",
        // Advance saved RIP past the ud2 opcode (2 bytes: 0x0F 0x0B) so iretq
        // resumes ring-3 at the instruction *after* ud2, not on ud2 again.
        "add qword ptr [rsp], 2",
        // Rearrange into syscall_handler(nr, a0, a1, a2) System V convention:
        //   rdi=nr   rsi=a0   rdx=a1   rcx=a2
        // On entry: rax=nr   rdi=a0   rsi=a1   rdx=a2
        "mov rcx, rdx",
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "call syscall_handler",
        // rax = return value; restore user GS and re-enter ring-3.
        "swapgs",
        "iretq",
        // --- Kernel ud2 crash path (ring-0 ud2, should never happen) ---
        "3:",
        "cli",
        "4: hlt",
        "jmp 4b",
    )
}
