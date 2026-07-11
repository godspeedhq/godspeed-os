// SPDX-License-Identifier: GPL-2.0-only
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
    pub user_rsp:   u64,   // offset 0 - saved on SYSCALL entry, restored on SYSRETQ
    pub kernel_rsp: u64,   // offset 8 - pre-loaded kernel stack top for current task
}

// Lives in .data (writable) - the stub writes user_rsp on every SYSCALL entry.
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
///   MSR_GS_BASE      (0xC000_0101) = kernel ptr  - active in ring-0
///   IA32_KERNEL_GS_BASE (0xC000_0102) = 0         - active in ring-3; swapgs exchanges them
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

        // MSR_GS_BASE = kernel ptr - establishes the ring-0 GS invariant.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0101u32,
            in("eax") ptr as u32,
            in("edx") (ptr >> 32) as u32,
            options(nostack, nomem),
        );

        // IA32_KERNEL_GS_BASE = the SAME per-core ptr (not 0). swapgs then swaps
        // per-core ↔ per-core, so GS.base can never become 0 regardless of swapgs
        // parity. Without this, an AP whose swapgs parity is left imbalanced by
        // interrupt activity (observed on the T630, core 1) flips GS.base to 0,
        // and the #UD syscall stub's `mov %r10, %gs:0x0` writes to address 0 →
        // kernel #PF loop → reboot. The BSP stays balanced so never hit it.
        // Cost: a ring-3 task can read its core's PER_CORE_SYSCALL via gs:[...]
        // (the kernel_rsp pointer) - a minor info leak, acceptable in v1; user
        // services don't use GS. Finding the imbalanced swapgs handler so this
        // can return to 0 is a follow-up.
        core::arch::asm!(
            "wrmsr",
            in("ecx") 0xC000_0102u32,
            in("eax") ptr as u32,
            in("edx") (ptr >> 32) as u32,
            options(nostack, nomem),
        );
    }
}

// ---------------------------------------------------------------------------
// Safe user-pointer wrappers (arch permitted layer).
// ---------------------------------------------------------------------------

/// The highest valid user virtual address (exclusive) - top of the lower half.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// Return `true` iff the byte range `[ptr, ptr+len)` lies entirely within the
/// user address space and does not wrap.
///
/// NOTE: this is a RANGE check only - it does NOT verify the pages are mapped or
/// writable. A range-valid-but-unmapped pointer still faults when the kernel copies
/// it; that fault is made recoverable by the `USER_COPY_ACTIVE` guard below (V1,
/// kernel-audit-2), so it kills the caller instead of halting the machine.
#[inline]
pub fn validate_user_ptr(ptr: u64, len: usize) -> bool {
    if ptr == 0 || len == 0 { return false; }
    if ptr >= USER_END { return false; }
    match ptr.checked_add(len as u64) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// User-copy fault guard (V1, kernel-audit-2).
//
// validate_user_ptr only range-checks; it cannot know whether a page is actually
// mapped/writable. When the kernel copies to/from a range-valid-but-unmapped (or
// read-only) user pointer, the copy faults - and because the copy runs at CPL0 the
// #PF error-code U/S bit is 0, so a naive pf_handler reads it as "KERNEL PF" and
// halts every core. That is a whole-machine DoS reachable by any service passing a
// bad pointer to a syscall.
//
// Fix: the copy helpers below set a per-core "user-copy in progress" flag around the
// ONLY instruction that touches user memory. pf_handler consults it: a CPL0 fault at
// a user VA while the flag is set is a bad USER pointer, so it kills the caller (like
// a ring-3 fault) and the system continues. The flag is set NARROWLY (never for a
// whole syscall), so a genuine kernel bug that faults elsewhere still halts loudly.
// IF=0 in syscall context => no same-core nesting; per-core => cross-core copies are
// independent.
// ---------------------------------------------------------------------------

use core::sync::atomic::{AtomicBool, Ordering};
use crate::smp::percpu::{num_cores, PerCore, PerCoreMut};

/// Per-core "user-copy in progress" flag - a boot-sized arena (§26.6.1), one slot per core Limine
/// reported, NOT a fixed `[_; MAX_CORES]`. `pf_handler` consults it to attribute a CPL0 user-VA fault to
/// a bad user pointer (kill the caller) rather than kernel corruption.
static USER_COPY_ACTIVE: PerCore<AtomicBool> = PerCore::new();

/// Per-core scratch for `read_user_bytes` - a boot-sized arena (one message page per core). User bytes
/// are copied here under the guard and a slice into this KERNEL buffer is returned, so no caller ever
/// dereferences raw user memory. Larger reads are rejected (`build_message` caps at `MAX_MESSAGE_SIZE`).
/// This was a 1 MiB fixed `[[u8; 4096]; MAX_CORES]`; as an arena it costs `num_cores * 4 KiB`.
static USER_READ_SCRATCH: PerCoreMut<[u8; crate::ipc::message::MAX_MESSAGE_SIZE]> = PerCoreMut::new();

/// Allocate the per-core user-copy arenas. Called once at boot (`smp::percpu_init`), after the frame
/// allocator is up and before any syscall (the only caller of the copy helpers) or ring-3 fault.
pub fn init_percore_arenas(n: usize) {
    USER_COPY_ACTIVE.init_with(n, |_| AtomicBool::new(false));
    USER_READ_SCRATCH.init_with(n, |_| [0u8; crate::ipc::message::MAX_MESSAGE_SIZE]);
}

/// True iff `core` is mid user-copy - consulted by `pf_handler` to attribute a CPL0 user-VA fault to a
/// bad user pointer (kill the caller) rather than kernel corruption. Returns false before the arena is
/// initialised (an early kernel fault, before `percpu_init`) - such a fault is genuine and must halt.
#[inline]
pub fn user_copy_active(core: usize) -> bool {
    USER_COPY_ACTIVE.initialised() && core < num_cores() && USER_COPY_ACTIVE.get(core).load(Ordering::SeqCst)
}

/// Clear the guard for `core`. Called by `pf_handler` after it attributes a fault to a
/// user-copy and kills the caller: the copy faulted and never returned, so the helper's
/// own clear was skipped and the flag must not leak to the next task on this core.
#[inline]
pub fn clear_user_copy_active(core: usize) {
    if USER_COPY_ACTIVE.initialised() && core < num_cores() {
        USER_COPY_ACTIVE.get(core).store(false, Ordering::SeqCst);
    }
}

/// Read a user-space byte range into this core's kernel scratch and return a slice into
/// the SCRATCH (never raw user memory).  Returns `None` if the range is invalid or
/// larger than one message page.
///
/// The returned slice is valid until the next `read_user_bytes` on this core; callers
/// must consume it before the next read / any reschedule (the prior borrowed-user-slice
/// return required the same).
#[inline]
pub fn read_user_bytes(ptr: u64, len: usize) -> Option<&'static [u8]> {
    if !validate_user_ptr(ptr, len) { return None; }
    if len > crate::ipc::message::MAX_MESSAGE_SIZE { return None; }
    let core = crate::task::scheduler::current_core_id();
    if core >= num_cores() { return None; }
    // Base of this core's scratch arena slot (single-owner: only this core, inside an IF=0 syscall,
    // touches its slot - no aliasing, no same-core nesting).
    let base = USER_READ_SCRATCH.as_mut_ptr(core).cast::<u8>();
    USER_COPY_ACTIVE.get(core).store(true, Ordering::SeqCst);
    // SAFETY: `[ptr, ptr+len)` is a validated user range; `base` is this core's scratch
    // of MAX_MESSAGE_SIZE >= len bytes. If a source page is unmapped the CPL0 #PF is
    // caught by pf_handler (USER_COPY_ACTIVE set -> the caller is killed), so this copy
    // never faults the kernel and never returns from the fault.
    unsafe { core::ptr::copy_nonoverlapping(ptr as *const u8, base, len) };
    USER_COPY_ACTIVE.get(core).store(false, Ordering::SeqCst);
    // SAFETY: `base` points to `len` bytes just initialised in the arena scratch slot.
    Some(unsafe { core::slice::from_raw_parts(base as *const u8, len) })
}

/// Copy `src` into the user-space address `dst`.  Returns `false` if the destination
/// range is invalid; the copy is a no-op in that case.
#[inline]
pub fn write_user_bytes(dst: u64, src: &[u8]) -> bool {
    if !validate_user_ptr(dst, src.len()) { return false; }
    let core = crate::task::scheduler::current_core_id();
    if core >= num_cores() { return false; }
    USER_COPY_ACTIVE.get(core).store(true, Ordering::SeqCst);
    // SAFETY: range validated; user-space VA disjoint from kernel. If `dst` is unmapped
    // or read-only the CPL0 #PF is caught by pf_handler (USER_COPY_ACTIVE set -> the
    // caller is killed), so this copy never halts the kernel.
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
    USER_COPY_ACTIVE.get(core).store(false, Ordering::SeqCst);
    true
}

/// Read the processor's time-stamp counter.
#[inline]
pub fn read_cycle_counter() -> u64 {
    // SAFETY: RDTSC is always available on x86_64 in ring-0 and has no
    // side effects other than reading the counter.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// CSTAR entry point - installed as the CSTAR MSR target (AMD compat-mode SYSCALL).
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

/// SYSCALL entry point - installed as the LSTAR MSR target.
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

/// INT 0x80 syscall entry - kept for reference; superseded by `ud2_syscall_entry`
/// on AMD GX-420GI where int $0x80 also stalls.
///
/// The CPU pushes a full hardware frame onto the kernel stack (via TSS.rsp0)
/// before jumping here, so no manual stack switch is needed:
///   [RSP+0]  saved RIP   (user RIP - return address)
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

/// UD2 syscall entry - IDT[6] (#UD hardware exception).
///
/// AMD GX-420GI (Jaguar/Puma+): both `syscall` and `int N` (software interrupt
/// dispatch) silently stall the core from ring-3.  `ud2` (0x0F 0x0B) is decoded
/// by the CPU as an explicitly undefined instruction and takes the hardware
/// exception pathway - the same one used by #PF and #GP - which does work on
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
        // --- Re-base onto the dedicated syscall stack (Bug 2 fix) ---
        // The #UD CPU frame lives at the top of the kstack (entered via
        // TSS.rsp0 = K0T), which is the SAME region the timer ISR's context-switch
        // path descends into (~K0T-504). Running the whole syscall chain there let
        // the timer-switch zero-write a suspended recv syscall's return address
        // (Bug 2). Switch to PER_CORE_SYSCALL.kernel_rsp (= K0T-2048, set by
        // prepare_ring3_switch) so the syscall chain lives well below the timer's
        // reach. The LSTAR path already does this; the #UD path didn't.
        //
        // The #UD frame (needed for the final iretq) stays at the top of the
        // kstack and is safe across the call: while in the syscall the task is
        // CPL0, so any interrupt uses the current (kernel_rsp) stack, never
        // TSS.rsp0 - nothing overwrites the top-of-kstack frame.
        "mov r10, rsp",            // r10 = #UD frame ptr (top of kstack)
        "mov rsp, gs:[8]",         // rsp = kernel_rsp (dedicated syscall stack)
        "sub rsp, 16",             // reserve a 16-byte slot, keep ABI alignment
        "mov [rsp], r10",          // stash #UD frame ptr (r10 is caller-saved)
        "call syscall_handler",
        "mov r10, [rsp]",          // recover #UD frame ptr
        "mov rsp, r10",            // switch back to the #UD frame
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
