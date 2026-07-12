// SPDX-License-Identifier: Apache-2.0
//! Adversarial test primitives for the §22 red-team / fuzz / chaos harness (the `probe` service).
//!
//! These are the one thing a test service legitimately needs that a safe wrapper cannot otherwise
//! express: issuing a RAW syscall with arbitrary (fuzzed) number + arguments, and executing a
//! DELIBERATE ring-3 fault to prove the kernel kills the faulting task rather than wedging the machine
//! (invariant 12; §22 A14 / A15 / C2). Isolating that `unsafe` HERE - a designated, audited SDK module,
//! every block carrying a `// SAFETY:` comment - keeps the test SERVICES themselves (`probe`)
//! `unsafe`-free (§18.2), exactly as the SDK's ABI / MMIO / DMA modules keep the driver services
//! `unsafe`-free (§18.1). This module is **test-only**: no production service should call it (a fault
//! primitive merely kills the caller, which the kernel handles; the fuzz primitive is a raw trap the
//! kernel validates).

/// Issue a RAW syscall with an arbitrary number and arguments and return the raw `i64` result. This is
/// the fuzz/adversarial entry point (F1/F2/A10/A15 ...): the kernel **validates every syscall**, so from
/// the caller's side this is memory-safe - it never dereferences anything itself. A bad pointer passed
/// as `a1` is the KERNEL's to validate and reject (or to attribute to this caller and kill it); it can
/// never corrupt this task's memory. The `unsafe` is purely the inline-asm ABI trap, which is sound for
/// any argument values, so it is isolated in [`crate::syscall::raw_syscall`] and wrapped safely here.
#[inline]
pub fn fuzz_syscall(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    // SAFETY: `raw_syscall` is the ABI trap (a `ud2` into ring-0). It is sound for ANY argument values -
    // it just transitions to the kernel, which validates. Fuzzing arbitrary nr/args cannot violate this
    // task's Rust memory safety; rejecting bad input is the kernel's responsibility (exactly what the
    // fuzz/adversarial suite verifies). The worst outcome is that the kernel kills this task, not UB.
    unsafe { crate::syscall::raw_syscall(nr, a0, a1, a2) }
}

/// Deliberately read through a NULL pointer, raising a ring-3 page fault (#PF) - §22 Chaos C2 / A14. A
/// conforming kernel kills this task at the fault and continues; it must never halt the machine
/// (invariant 12). Control does not return on a conforming kernel.
#[inline]
pub fn fault_null_read() {
    // SAFETY: an intentional fault for the C2/A14 regression. A conforming kernel kills this task at the
    // ring-3 #PF before the read yields a value, so nothing is truly observed; even if a (broken) kernel
    // wrongly resumed us, `read_volatile` of address 0 is a defined faulting operation, not UB we rely on.
    unsafe { core::ptr::read_volatile(core::ptr::null::<u8>()); }
}

/// Deliberately read a NON-CANONICAL address (bit 47 != bit 63), raising a ring-3 general-protection
/// fault (#GP(0)) at CPL3 - §22 A14. The kernel must kill this task, not halt (kernel-audit C1).
#[inline]
pub fn fault_noncanonical_read() {
    // SAFETY: an intentional fault for the A14 regression; the kernel kills this task at the ring-3 #GP.
    // See `fault_null_read` - a conforming kernel never lets the read complete.
    unsafe { let _ = core::ptr::read_volatile(0x8000_0000_0000_0000 as *const u8); }
}

/// Deliberately divide by zero, raising a ring-3 divide-error (#DE, vector 0) at CPL3 - §22 A14. Uses
/// raw asm because Rust inserts a divide guard that would PANIC (unwind) instead of faulting, and the
/// threat model includes adversarial asm services. The kernel must kill this task, not halt.
#[inline]
pub fn fault_divide_by_zero() {
    // SAFETY: an intentional fault for the A14 regression; the kernel kills this task at the ring-3 #DE.
    // The asm touches no memory (nomem/nostack) and clobbers only the declared caller-saved registers.
    unsafe {
        core::arch::asm!(
            "xor eax, eax",
            "xor edx, edx",
            "xor ecx, ecx",
            "div ecx",   // (EDX:EAX)=0 / ECX=0 -> #DE (divide error)
            out("eax") _, out("edx") _, out("ecx") _,
            options(nostack, nomem),
        );
    }
}
