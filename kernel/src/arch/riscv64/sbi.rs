// SPDX-License-Identifier: GPL-2.0-only
//! SBI - the Supervisor Binary Interface, this kernel's calls into the firmware beneath it.
//!
//! GodspeedOS runs in S-MODE on RISC-V, which is a genuinely different posture from its other
//! ports: on x86 it owns ring 0, on the Pi it owns EL1 with nothing above it, but here OpenSBI sits
//! in M-mode underneath and owns the timer, inter-hart interrupts and hart start/stop. Those are not
//! hardware pokes on this ISA, they are calls - which is a fact about the machine that the seam has
//! to absorb so nothing above `arch::imp` learns it.
//!
//! An `ecall` from S-mode traps to M-mode with the extension id in `a7`, the function id in `a6`,
//! arguments from `a0`, and a two-word return: an error code in `a0` and a value in `a1`.
//!
//! CAPABILITIES ARE PROBED, NEVER ASSUMED. The two machines this port runs on disagree about what
//! their firmware offers - QEMU carries OpenSBI v1.8 and advertises `sstc`, the JH7110 carries v1.2
//! and reports no ISA extensions at all - so an extension is asked about before it is used, and a
//! missing one is reported rather than called into.

/// Base extension: always present, and how everything else is discovered.
const EXT_BASE: u64 = 0x10;
const FID_GET_SPEC_VERSION: u64 = 0;
const FID_PROBE_EXTENSION: u64 = 3;

/// Timer extension ("TIME"), the one this kernel needs for a scheduler tick.
pub const EXT_TIME: u64 = 0x5449_4D45;
const FID_SET_TIMER: u64 = 0;

/// Hart State Management ("HSM"), which is how a secondary hart is STARTED on RISC-V.
///
/// There is no trampoline to write and no INIT/SIPI dance to time. OpenSBI parks every hart but the
/// boot one, and this asks it to release a named hart at a named address - so the whole of x86's
/// `ap_boot.rs` real-mode trampoline is replaced by one firmware call. What the firmware will NOT do
/// is set up that hart's stack, page table or trap vector; those arrive in the same state the boot
/// hart did, which is why the AP entry has to repeat the work `_start` does.
pub const EXT_HSM: u64 = 0x0048_534D;
const FID_HART_START: u64 = 0;

/// Inter-processor interrupts ("sPI").
///
/// **The IPI carries no vector**, unlike an APIC's, so it says only "someone poked you". The vector
/// has to travel out of band, which is what the per-core pending mask in `arch/riscv64/mod.rs` is
/// for. Named here because it is the difference that shapes the receiving side.
pub const EXT_IPI: u64 = 0x0073_5049;
const FID_SEND_IPI: u64 = 0;

/// Result of an SBI call: a firmware error code and a value.
pub struct SbiRet {
    pub error: i64,
    pub value: i64,
}

/// Make an SBI call.
///
/// # Safety
/// Always safe in the memory sense - `ecall` traps to firmware and returns - but marked `unsafe`
/// because WHAT the firmware does depends entirely on the extension and function asked for, and a
/// wrong pair can reset the machine (the legacy shutdown extension is one number away from several
/// harmless ones).
pub unsafe fn call(eid: u64, fid: u64, a0: u64, a1: u64) -> SbiRet {
    let (err, val): (i64, i64);
    // SAFETY: contract delegated to the caller above. `ecall` clobbers nothing this function relies
    // on, and the register assignment is the SBI calling convention.
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") a0 as i64 => err,
            inlateout("a1") a1 as i64 => val,
            in("a6") fid,
            in("a7") eid,
            options(nostack)
        );
    }
    SbiRet { error: err, value: val }
}

/// The SBI specification version the firmware implements, as (major, minor).
pub fn spec_version() -> (u64, u64) {
    // SAFETY: the Base extension is mandatory in every SBI version, and this function has no
    // side effects.
    let r = unsafe { call(EXT_BASE, FID_GET_SPEC_VERSION, 0, 0) };
    let v = r.value as u64;
    ((v >> 24) & 0x7f, v & 0xff_ffff)
}

/// Is `eid` implemented? Asked rather than assumed - see the module comment.
pub fn probe(eid: u64) -> bool {
    // SAFETY: probing is defined for any id and has no side effects.
    let r = unsafe { call(EXT_BASE, FID_PROBE_EXTENSION, eid, 0) };
    r.error == 0 && r.value != 0
}

/// Schedule the next timer interrupt for absolute time `when`, on the `time` counter's scale.
///
/// Also the way to CANCEL one: the specification defines an absolute time far in the future as
/// "no timer", which is why this takes an absolute value rather than a delay.
pub fn set_timer(when: u64) -> bool {
    // SAFETY: the TIME extension's only function; the caller has probed for it.
    let r = unsafe { call(EXT_TIME, FID_SET_TIMER, when, 0) };
    r.error == 0
}

/// The machine's monotonic counter, at the rate the device tree calls `timebase-frequency`.
///
/// Readable from S-mode only if the firmware allows it (`mcounteren`). If it does not, this traps
/// as an illegal instruction - which the trap vector installed before this point reports by name,
/// rather than the machine simply stopping.
pub fn time() -> u64 {
    let t: u64;
    // SAFETY: a CSR read with no side effects. Whether it is PERMITTED is a firmware policy, and
    // the trap vector reports the answer if it is not.
    unsafe { core::arch::asm!("csrr {}, time", out(reg) t, options(nomem, nostack)) };
    t
}

/// Ask the firmware to start `hartid` at `start_addr`, with `opaque` handed to it in `a1`.
///
/// Returns false on any firmware error - hart already started, invalid address, extension absent -
/// rather than assuming success, because a hart that never starts is otherwise indistinguishable
/// from one that started and hung, and those have opposite fixes.
///
/// `start_addr` is a PHYSICAL address: the hart begins with `satp` zero, exactly as the boot hart
/// did, so it is not running under the kernel's page table until it installs it itself.
pub fn hart_start(hartid: u64, start_addr: u64, opaque: u64) -> bool {
    if !probe(EXT_HSM) {
        return false;
    }
    // SAFETY: HSM function 0 (HART_START) with a real hart id and a physical entry address in this
    // kernel's image. It cannot affect the calling hart.
    let r = unsafe { call3(EXT_HSM, FID_HART_START, hartid, start_addr, opaque) };
    r.error == 0
}

/// Send an interrupt to every hart selected by `mask`, based at `mask_base`.
///
/// Returns false if the firmware refuses or the extension is absent, so a lost wake is reported
/// rather than assumed delivered.
pub fn send_ipi(mask: u64, mask_base: u64) -> bool {
    if !probe(EXT_IPI) {
        return false;
    }
    // SAFETY: IPI function 0 (SEND_IPI). It raises a supervisor software interrupt on the selected
    // harts and does nothing else.
    let r = unsafe { call(EXT_IPI, FID_SEND_IPI, mask, mask_base) };
    r.error == 0
}

/// Three-argument SBI call, for the one extension that needs a third.
///
/// # Safety
/// Same contract as `call`: the firmware routine invoked is decided entirely by `eid`/`fid`.
pub unsafe fn call3(eid: u64, fid: u64, a0: u64, a1: u64, a2: u64) -> SbiRet {
    let (err, val): (i64, i64);
    // SAFETY: contract delegated to the caller above; the register assignment is the SBI calling
    // convention, which passes arguments in a0.. and returns (error, value) in a0/a1.
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("a0") a0 as i64 => err,
            inlateout("a1") a1 as i64 => val,
            in("a2") a2,
            in("a6") fid,
            in("a7") eid,
            options(nostack)
        );
    }
    SbiRet { error: err, value: val }
}
