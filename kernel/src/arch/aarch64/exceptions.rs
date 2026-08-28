// SPDX-License-Identifier: GPL-2.0-only
//! AArch64 exception vectors (`VBAR_EL1`) for the Raspberry Pi 4.
//!
//! **Why this comes before the timer, the GIC, or anything else.** Until `VBAR_EL1` points at a real
//! table, a fault is a silent lockup: the core takes the exception, branches into whatever happens to
//! be at the reset vector address, and wanders off. Every milestone after this one adds code that can
//! fault, so this is the difference between "the Pi stopped" and a message naming the cause. That is
//! invariant 12 applied to the port itself - failures loud, never silent.
//!
//! ## Table shape
//!
//! The architecture fixes the layout: `VBAR_EL1` must be 2 KiB aligned, and holds **16 entries of 128
//! bytes each**, in four groups of four (Synchronous, IRQ, FIQ, SError):
//!
//! | Group | Taken from |
//! |---|---|
//! | 0 | Current EL, using `SP_EL0` |
//! | 1 | Current EL, using `SP_ELx` - where kernel faults land |
//! | 2 | Lower EL, AArch64 - where userspace faults and `svc` will land |
//! | 3 | Lower EL, AArch32 - we never run 32-bit, so reaching it is itself a bug |
//!
//! 128 bytes is only 32 instructions, which is why each entry does the minimum (open a frame, record
//! which vector fired, branch) and the shared tail does the real work.
//!
//! ## What it does today
//!
//! Saves the general-purpose registers plus `ELR_EL1`/`SPSR_EL1`, reports the vector, `ESR_EL1`,
//! `FAR_EL1`, `ELR_EL1` and `SPSR_EL1`, and halts. It deliberately does **not** return: nothing here
//! can yet handle a fault, and returning to a faulting instruction would loop forever printing. When
//! the scheduler and userspace arrive, group 2 grows a real syscall path and a task-kill path; the
//! frame is already laid out for that.

use core::arch::global_asm;

/// Trap frame the assembly builds on the stack. `#[repr(C)]` because assembly writes it.
#[repr(C)]
pub struct TrapFrame {
    /// x0..x30.
    pub x: [u64; 31],
    /// Exception Link Register - the instruction to return to.
    pub elr: u64,
    /// Saved Program Status Register - the interrupted context's PSTATE.
    pub spsr: u64,
    /// The interrupted task's USER stack pointer.
    ///
    /// **`SP_EL0` is one register shared by every EL0 task**, and the kernel does not run on it - so
    /// nothing else saves it. Without it here, whichever task ran last leaves its user stack pointer
    /// behind and the next task to `eret` resumes on it. That is survivable with a single EL0 task,
    /// which is exactly why it stayed hidden through several milestones; with three services it made
    /// one of them build its stack frame on another's stack and write past the top of the mapped
    /// region. Offset 264, inside the 272-byte frame the vectors already reserve.
    pub sp_el0: u64,
}

global_asm!(
    r#"
// Each vector entry gets 128 bytes (.align 7). Do the minimum here: open the frame, stash x0/x1 so we
// have scratch, record which of the 16 vectors fired, and branch to the shared tail.
.macro VECTOR_ENTRY kind
    .align 7
    sub  sp, sp, #272
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    aarch64_trap_common
.endm

// Synchronous exceptions from a LOWER EL must return too - that is where `svc` lands, and a syscall
// that halts the machine is not a syscall.
.macro VECTOR_SYNC_LOWER kind
    .align 7
    sub  sp, sp, #272
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    aarch64_sync_lower_common
.endm

// Synchronous exceptions at the CURRENT EL must be able to return, because one of them is expected: a
// user-copy touching a range-valid but unmapped page. See `aarch64_sync_current_dispatch`.
.macro VECTOR_SYNC_CURRENT kind
    .align 7
    sub  sp, sp, #272
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    aarch64_sync_current_common
.endm

// SError vectors need a decision rather than a fixed answer. An SError is an ASYNCHRONOUS external
// abort - the interconnect reporting that some earlier posted write went nowhere - so it does not land
// on the instruction that caused it, and by default it is a halt, correctly: a machine that has been
// told a write vanished has no idea what state it is in.
//
// The exception is a DELIBERATE PROBE of hardware that may not be there. Bringing up the PCIe root
// complex writes registers that a board without one will refuse, and the refusal arrives later, as a
// pending SError that detonates on the next entry to EL0 - a trap report pointing at a userspace task
// that did nothing wrong. See `aarch64_serror_dispatch`.
.macro VECTOR_SERROR kind
    .align 7
    sub  sp, sp, #272
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    aarch64_serror_common
.endm

// IRQ vectors must RETURN, not report and halt - a timer tick that halts the machine is not a tick.
.macro VECTOR_IRQ kind
    .align 7
    sub  sp, sp, #272
    stp  x0, x1, [sp, #0]
    mov  x0, #\kind
    b    aarch64_irq_common
.endm

    .section .text
    .globl aarch64_vectors
    .align 11                      // VBAR_EL1 requires 2 KiB alignment
aarch64_vectors:
    VECTOR_ENTRY 0                 // Current EL, SP_EL0:  Synchronous
    VECTOR_IRQ   1                 //                      IRQ
    VECTOR_ENTRY 2                 //                      FIQ
    VECTOR_SERROR 3               //                      SError
    VECTOR_SYNC_CURRENT 4          // Current EL, SP_ELx:  Synchronous (user-copy faults land here)
    VECTOR_IRQ   5                 //                      IRQ
    VECTOR_ENTRY 6                 //                      FIQ
    VECTOR_SERROR 7               //                      SError
    VECTOR_SYNC_LOWER 8            // Lower EL, AArch64:   Synchronous (svc lands here)
    VECTOR_IRQ   9                 //                      IRQ
    VECTOR_ENTRY 10                //                      FIQ
    VECTOR_SERROR 11              //                      SError
    VECTOR_ENTRY 12                // Lower EL, AArch32:   Synchronous
    VECTOR_IRQ   13                //                      IRQ
    VECTOR_ENTRY 14                //                      FIQ
    VECTOR_SERROR 15              //                      SError

// Save x2..x30 plus ELR/SPSR into the frame the vector entry opened. Used by both tails.
.macro SAVE_REST
    stp  x2,  x3,  [sp, #16]
    stp  x4,  x5,  [sp, #32]
    stp  x6,  x7,  [sp, #48]
    stp  x8,  x9,  [sp, #64]
    stp  x10, x11, [sp, #80]
    stp  x12, x13, [sp, #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x19, [sp, #144]
    stp  x20, x21, [sp, #160]
    stp  x22, x23, [sp, #176]
    stp  x24, x25, [sp, #192]
    stp  x26, x27, [sp, #208]
    stp  x28, x29, [sp, #224]
    mrs  x2, elr_el1
    mrs  x3, spsr_el1
    stp  x30, x2,  [sp, #240]      // x30, then ELR
    mrs  x4, sp_el0
    stp  x3,  x4,  [sp, #256]      // SPSR, then the task's USER stack pointer
.endm

// Synchronous / FIQ / SError: report and stop. Nothing here can handle a fault yet, and returning to a
// faulting instruction would re-fault and re-print forever.
aarch64_trap_common:
    SAVE_REST
    mov  x1, sp                    // arg1 = &TrapFrame
    bl   aarch64_trap_report       // (vector: u64, frame: *const TrapFrame) -> !
1:  wfe
    b    1b

// IRQ: save, handle, restore, return. The restore order matters - x2/x3 carry ELR/SPSR into their
// system registers and are only reloaded with their real values afterwards.
// SError: ask whether it was expected. `aarch64_serror_dispatch` returns non-zero when the kernel was
// deliberately probing hardware that might not answer, in which case we resume; otherwise fall through
// to the same report-and-halt every other unexpected trap gets.
aarch64_serror_common:
    SAVE_REST
    mov  x19, x0                   // keep the vector number across the call
    mov  x1, sp
    bl   aarch64_serror_dispatch   // (vector: u64, frame: *mut TrapFrame) -> u64
    cbz  x0, 1f
    b    aarch64_exception_return
1:  mov  x0, x19
    mov  x1, sp
    bl   aarch64_trap_report
2:  wfe
    b    2b

aarch64_sync_current_common:
    SAVE_REST
    mov  x1, sp
    bl   aarch64_sync_current_dispatch // (vector: u64, frame: *mut TrapFrame) - may rewrite frame.elr
    b    aarch64_exception_return

aarch64_sync_lower_common:
    SAVE_REST
    mov  x1, sp
    bl   aarch64_sync_lower_dispatch  // (vector: u64, frame: *mut TrapFrame)
    b    aarch64_exception_return

aarch64_irq_common:
    SAVE_REST
    mov  x1, sp
    bl   aarch64_irq_dispatch      // (vector: u64, frame: *mut TrapFrame)
aarch64_exception_return:
    // MASK INTERRUPTS FOR THE WHOLE RETURN SEQUENCE.
    //
    // Below, ELR_EL1 and SPSR_EL1 are written and then a dozen more instructions run before the `eret`.
    // An interrupt taken in that window is not merely inconvenient: the hardware OVERWRITES ELR_EL1 and
    // SPSR_EL1 with the interrupted context, destroying the user state just loaded. The `eret` then
    // returns somewhere in this very function, at whatever EL the stale SPSR names - which is exactly
    // what was observed, an EL0 task with its PC inside `aarch64_exception_return`.
    //
    // Exception ENTRY masks interrupts automatically, so this window is closed for the common path.
    // It is not closed for a task resumed after blocking: the scheduler re-enables interrupts around
    // the switch, so that task unwinds back through here with IRQs live. Masking here costs nothing -
    // `eret` restores DAIF from SPSR - and makes the sequence atomic regardless of how it was reached.
    msr  daifset, #3               // mask IRQ and FIQ until the eret
    ldp  x3,  x4,  [sp, #256]      // SPSR, user SP
    ldp  x30, x2,  [sp, #240]      // x30, ELR
    msr  elr_el1,  x2
    msr  spsr_el1, x3
    // Restore the user stack ONLY when returning to EL0 (SPSR.M[3:0] == 0 selects EL0t).
    //
    // Restoring it unconditionally is wrong and was tried: an exception taken at EL1 captures whatever
    // SP_EL0 happens to hold - typically some other task's user SP, or zero before any task has run -
    // and re-imposing that on the way back out clobbers the value belonging to whichever task is
    // actually at EL0. The kernel never runs on SP_EL0, so for an EL1 return there is nothing to
    // restore and doing so can only do harm.
    tst  x3, #0xF
    b.ne 5f
    msr  sp_el0,   x4
5:
    ldp  x2,  x3,  [sp, #16]
    ldp  x4,  x5,  [sp, #32]
    ldp  x6,  x7,  [sp, #48]
    ldp  x8,  x9,  [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldp  x0,  x1,  [sp, #0]
    add  sp, sp, #272
    eret
"#
);

extern "C" {
    /// The vector table, defined in the assembly above.
    static aarch64_vectors: u8;
}

/// Install the table. Read back rather than assumed - see `installed`.
pub fn init() -> u64 {
    let vbar = core::ptr::addr_of!(aarch64_vectors) as u64;
    // SAFETY: `aarch64_vectors` is the 2 KiB-aligned table defined above, in this image's `.text`, and
    // writing VBAR_EL1 at EL1 only changes where exceptions are taken to.
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {v}",
            "isb",
            v = in(reg) vbar,
            options(nostack),
        );
    }
    vbar
}

/// The installed `VBAR_EL1`, read back from the register.
pub fn installed() -> u64 {
    let v: u64;
    // SAFETY: reading VBAR_EL1 at EL1 is a side-effect-free system-register read.
    unsafe { core::arch::asm!("mrs {}, vbar_el1", out(reg) v, options(nomem, nostack)) };
    v
}

/// Human-readable name for each of the 16 vectors, so a report says which one fired rather than
/// printing an index the reader has to look up.
fn vector_name(v: u64) -> &'static [u8] {
    match v {
        0 => b"CurrentEL/SP0 Synchronous",
        1 => b"CurrentEL/SP0 IRQ",
        2 => b"CurrentEL/SP0 FIQ",
        3 => b"CurrentEL/SP0 SError",
        4 => b"CurrentEL/SPx Synchronous",
        5 => b"CurrentEL/SPx IRQ",
        6 => b"CurrentEL/SPx FIQ",
        7 => b"CurrentEL/SPx SError",
        8 => b"LowerEL/A64 Synchronous",
        9 => b"LowerEL/A64 IRQ",
        10 => b"LowerEL/A64 FIQ",
        11 => b"LowerEL/A64 SError",
        12 => b"LowerEL/A32 Synchronous",
        13 => b"LowerEL/A32 IRQ",
        14 => b"LowerEL/A32 FIQ",
        15 => b"LowerEL/A32 SError",
        _ => b"unknown",
    }
}

/// Decode the Exception Class field of `ESR_EL1` (bits 31:26) for the cases this port can currently
/// hit. Not exhaustive - naming the common ones is what turns a hex dump into a diagnosis.
fn ec_name(esr: u64) -> &'static [u8] {
    match (esr >> 26) & 0x3F {
        0b000000 => b"unknown reason",
        0b000111 => b"SVE/SIMD/FP access trapped (CPACR/CPTR)",
        0b010101 => b"SVC from AArch64",
        0b100000 => b"instruction abort, lower EL",
        0b100001 => b"instruction abort, same EL",
        0b100010 => b"PC alignment fault",
        0b100100 => b"data abort, lower EL",
        0b100101 => b"data abort, same EL",
        0b100110 => b"SP alignment fault",
        0b101100 => b"trapped FP exception",
        0b111100 => b"BRK instruction",
        _ => b"see ARM ARM D17.2.37 for this EC",
    }
}

/// Ticks seen, so the boot can report progress instead of asserting the timer works.
pub static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Said once: an SPI too high to express in the neutral routing table's `u8` key.
/// The BCM2711 PCIe controller's MSI interrupt: GIC SPI 148, so INTID 32 + 148.
pub const PCIE_MSI_SPI: u32 = 32 + 148;
/// The neutral IRQ number the `xhci` service is granted (`hw_irqs: &[0x28]`). It began as an x86 MSI
/// vector; here it is simply the agreed name for "the USB controller's interrupt".
pub const XHCI_MSI_VECTOR: u8 = 0x28;

/// GENET's "macirq" - BCM2711 device tree GIC_SPI 157, so INTID 32 + 157. LEVEL-triggered: the line
/// stays asserted until the driver clears `INTRL2_0`, which is why it must be masked on delivery.
pub const GENET_SPI: u32 = 32 + 157;
/// The neutral vector `nic-driver` is granted for it.
pub const GENET_VECTOR: u8 = 0x2A;

static SPI_TOO_HIGH_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// IRQ entry. Acknowledge at the GIC, handle, end-of-interrupt, return.
///
/// The EOI is not optional bookkeeping: the CPU interface keeps a priority active for every
/// acknowledged interrupt, so skipping it silently blocks all later interrupts of equal or lower
/// priority. The symptom is "interrupts stopped" with nothing to point at, which is why the retire
/// happens on every path out of here including the spurious one.
/// Interrupts dispatched per core, and the last GIC interrupt ID each saw.
///
/// Exists for one question the liveness watchdog could not answer on this port: when a core stops
/// making progress, is it still taking interrupts? A frozen count and a climbing count are opposite
/// faults with opposite fixes - not taking interrupts at all, versus taking them and losing the tick
/// inside the handler - and without this the panic named the victim but never the mechanism.
///
/// This is the aarch64 twin of the 32-bit port's tally (`arch/arm/irq.rs`). It is not optional here:
/// `liveness_deadline_cycles` is ANSWERED on the Pi 4, so the watchdog is armed on this port and its
/// panic message reads these. They were missing, which is why the Pi 4 kernel did not build.
static IRQ_COUNT: [core::sync::atomic::AtomicU32; MAX_TALLY_CORES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAX_TALLY_CORES];
static IRQ_LAST_ID: [core::sync::atomic::AtomicU32; MAX_TALLY_CORES] =
    [const { core::sync::atomic::AtomicU32::new(0) }; MAX_TALLY_CORES];

/// Cores this tally covers. The Pi 4 is a quad-core A72; a core beyond this range folds into the last
/// slot rather than indexing out of bounds, because a DIAGNOSTIC must never be the thing that panics.
const MAX_TALLY_CORES: usize = 4;

/// (interrupts dispatched, last GIC interrupt ID) for `core` - what the liveness panic reports.
pub fn core_irq_debug(core: u32) -> (u32, u32) {
    let i = (core as usize).min(MAX_TALLY_CORES - 1);
    (
        IRQ_COUNT[i].load(core::sync::atomic::Ordering::Relaxed),
        IRQ_LAST_ID[i].load(core::sync::atomic::Ordering::Relaxed),
    )
}

#[no_mangle]
extern "C" fn aarch64_irq_dispatch(_vector: u64, _frame: *mut TrapFrame) {
    let id = super::gic::acknowledge();
    if id == super::gic::SPURIOUS {
        return; // nothing pending - do NOT EOI an ID that was never raised
    }
    // Stamp BEFORE any handling: the count must prove the interrupt was TAKEN, not that it completed.
    // A handler that hangs is exactly the case this is meant to distinguish, and it would never reach
    // a stamp placed at the end.
    // Indexed by LOGICAL core id, which is what the liveness watchdog passes to `core_irq_debug` -
    // so the tally is read back under the same identity it was written under. (Raw MPIDR would agree
    // on the Pi 4, where it is 0..3, and quietly disagree on any board where it is not.) The neutral
    // accessor is safe and cannot panic - it falls back to 0 for a core it does not know - which is
    // the property that matters in a handler whose whole job is to survive a machine in trouble.
    let core = crate::task::scheduler::current_core_id().min(MAX_TALLY_CORES - 1);
    IRQ_COUNT[core].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    IRQ_LAST_ID[core].store(id, core::sync::atomic::Ordering::Relaxed);
    // IDs 0..15 are Software Generated Interrupts - one core waking another. Acknowledging and retiring
    // them is the whole job: the wake's PURPOSE is to make this core leave `wfi` and re-enter the
    // scheduler, which it does on the way out of this handler. There is no per-vector work to do, which
    // is exactly why the 32-bit port runs four cores with its IPI senders still stubbed - every core
    // ticks on its own timer and picks up work then, so an IPI is a latency improvement rather than a
    // correctness requirement.
    if id < 16 {
        super::gic::eoi(id);
        return;
    }
    // DEVICE interrupts (GIC Shared Peripheral Interrupts, ID 32 and up) go to USERSPACE.
    //
    // This one branch is what `docs/aarch64.md` and the §6.4 amendment called the reason drivers must
    // live in the kernel on ARM: "the arch does not yet route device IRQs to userspace". It was never a
    // hardware limitation - the GIC-400 delivers SPIs perfectly well and the neutral router
    // (`interrupt::route`) is arch-agnostic and already complete. Nobody had connected the two. An
    // unimplemented branch was read as an architectural constraint, and a Commandment I violation was
    // accepted on the strength of it.
    //
    // IDs are bounded to a byte because the neutral routing table is keyed by `u8` (an x86 vector). The
    // GIC numbers SPIs up to 1020, so anything above 255 cannot be expressed and is retired rather than
    // silently dropped into the wrong bucket - loud about the limit instead of wrong about the IRQ.
    // The PCIe controller raises ONE SPI for all 32 of its MSIs, so this is a demultiplexer, not a
    // route. It must read the status register to learn which MSI fired and clear it before returning:
    // the SPI is level-triggered, so an unacknowledged bit re-raises it immediately and the core makes
    // no further progress - which the liveness watchdog turns into a panic.
    //
    // The MSI is translated into the NEUTRAL vector the service was granted (`hw_irqs: &[0x28]`)
    // rather than delivered as SPI 180. That number began life as an x86 MSI vector, and keeping it
    // means the `xhci` service's contract says the same thing on both architectures - the arch layer
    // maps its own interrupt onto the shared name, which is what the seam exists for.
    // GENET, translated to the neutral vector its contract names - the same shape as the MSI arm
    // below. `deliver` masks it first (it is registered LEVEL), so the line cannot re-enter while the
    // driver works; the driver unmasks with `IrqUnmask` once it has cleared `INTRL2_0`.
    if id == GENET_SPI {
        // SAFETY: in the IRQ handler with interrupts masked - `deliver`'s documented contract.
        unsafe { crate::interrupt::route::deliver(GENET_VECTOR) };
        super::gic::eoi(id);
        return;
    }
    if id == PCIE_MSI_SPI {
        let pending = super::pcie::msi_take_pending();
        if pending & 1 != 0 {
            // SAFETY: in the IRQ handler with interrupts masked - `deliver`'s documented contract.
            unsafe { crate::interrupt::route::deliver(XHCI_MSI_VECTOR) };
        }
        super::gic::eoi(id);
        return;
    }
    if id >= 32 {
        if id <= u8::MAX as u32 {
            // SAFETY: called from the IRQ handler with interrupts masked, which is the contract the x86
            // caller documents for this function.
            unsafe { crate::interrupt::route::deliver(id as u8) };
        } else if !SPI_TOO_HIGH_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::kprintln!(
                "gic: SPI {} is above the routing table's u8 range - not deliverable to userspace", id);
        }
        super::gic::eoi(id);
        return;
    }
    if id == super::timer::TIMER_PPI {
        let t = TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // Re-arm: the generic timer is one-shot, so a periodic tick means reloading it every time.
        super::timer::arm(super::TIMER_INTERVAL.load(core::sync::atomic::Ordering::Relaxed));

        let _ = t;

        // **EOI BEFORE handing control to the scheduler, not after.** The neutral tick performs a
        // preemptive `switch_context` INTERNALLY: it may not return here for a whole quantum, and on a
        // busy core it may never return on this task's stack at all. The GIC CPU interface keeps a
        // priority active until the interrupt is retired, so an EOI deferred past the switch leaves
        // this timer interrupt permanently active - blocking every later interrupt of equal or lower
        // priority. The machine would take exactly one tick and then go silent, with the scheduler
        // looking like the culprit.
        //
        // Retiring first is safe because the timer is already re-armed above: the interrupt this call
        // retires is finished with, and the next one is a fresh assertion.
        super::gic::eoi(id);

        if NEUTRAL_SCHED.load(core::sync::atomic::Ordering::Relaxed) {
            // Drives the neutral round-robin: it picks the next task and switches to it. Arguments are
            // the x86 interrupted-frame triple, which this port does not use - the frame is on the
            // task's own kernel stack and the vector below restores it.
            crate::task::scheduler::timer_tick_from_irq(0, 0, 0);
        }
        return;
    }
    super::gic::eoi(id);
}

/// Set once the neutral scheduler owns preemption.
///
/// Until then the timer only counts ticks (milestones 4-8 rely on that). Flipping this is what turns
/// the tick into a preemption point, and it is deliberately a runtime flag rather than a compile-time
/// one so the earlier milestones keep running unchanged in the same image.
pub static NEUTRAL_SCHED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Exception class for `SVC` taken from AArch64.
const EC_SVC64: u64 = 0b010101;
/// Data abort taken from the same EL (a kernel-mode access that faulted).
const EC_DATA_ABORT_SAME: u64 = 0b100101;

/// Synchronous exception at the CURRENT EL - a kernel-mode fault.
///
/// Almost all of these are genuine kernel bugs and must halt loudly. **One is expected**: the kernel
/// copying to or from a user pointer that passed the range check but is not actually mapped. That is
/// reachable by any service passing a bad pointer to a syscall, and halting on it would be a
/// whole-machine denial of service triggered by a single misbehaving task.
///
/// So when a user copy is in progress, a data abort here is treated as "that pointer was bad": the
/// return address is rewritten to the copy helper's recovery label, which reports the failure to the
/// syscall rather than to the machine. The window is **narrow by construction** - it covers only the
/// instruction that touches user memory - so a kernel bug faulting anywhere else still halts loudly,
/// which is the property that makes this safe rather than a blanket "ignore kernel faults".
/// Set while the kernel is deliberately poking hardware that may not be there.
static PROBING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// SErrors absorbed while probing. A count, so the probe can report what it cost.
static SERRORS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Open the probe window: SErrors are counted and resumed from rather than halting.
///
/// The window is closed by [`end_probe`], which also DRAINS: an SError raised by a posted write is
/// asynchronous and stays pending while `PSTATE.A` is masked, which it is throughout boot. Left
/// pending, it fires at the first `eret` into EL0 - where SPSR clears DAIF - and reports a userspace
/// task that did nothing wrong, at an address in a completely different subsystem. That is exactly how
/// the first PCIe bring-up presented: a clean "link did not train" line, and then an SError blamed on
/// the EL0 selftest 180 ms later.
pub fn begin_probe() {
    SERRORS.store(0, core::sync::atomic::Ordering::Relaxed);
    PROBING.store(true, core::sync::atomic::Ordering::Release);
}

/// Close the probe window, collecting anything still in flight. Returns how many were absorbed.
pub fn end_probe() -> u32 {
    // SAFETY: `dsb sy` waits for outstanding accesses to complete, so a write that is going to be
    // refused has been refused by the time the mask is lifted. Unmasking SError briefly lets any
    // pending one be delivered HERE, inside the window, rather than at the next entry to EL0. The
    // `nop`s give it somewhere to land; the mask goes straight back on.
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "isb",
            "msr daifclr, #4", // unmask SError (PSTATE.A)
            "nop", "nop", "nop", "nop",
            "isb",
            "msr daifset, #4", // mask it again
            options(nostack),
        );
    }
    PROBING.store(false, core::sync::atomic::Ordering::Release);
    SERRORS.load(core::sync::atomic::Ordering::Relaxed)
}

/// An asynchronous external abort. Returns non-zero to resume, zero to report and halt.
///
/// Resuming is right only inside a probe window (see [`begin_probe`]). Everywhere else an SError means
/// the interconnect has told us a write went nowhere, and a kernel that shrugs at that is a kernel
/// running on state it cannot account for - so the default stays halt.
#[no_mangle]
extern "C" fn aarch64_serror_dispatch(_vector: u64, _frame: *mut TrapFrame) -> u64 {
    if PROBING.load(core::sync::atomic::Ordering::Acquire) {
        SERRORS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return 1;
    }
    0
}

#[no_mangle]
extern "C" fn aarch64_sync_current_dispatch(vector: u64, frame: *mut TrapFrame) {
    let esr: u64;
    // SAFETY: reading ESR_EL1 at EL1 is a side-effect-free system-register read.
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack)) };

    if (esr >> 26) & 0x3F == EC_DATA_ABORT_SAME {
        if let Some(recovery) = super::uaccess::take_fixup() {
            // SAFETY: `frame` is the trap frame the vector assembly built on the current stack; the
            // return path reloads ELR_EL1 from it.
            unsafe { (*frame).elr = recovery };
            return;
        }
    }
    aarch64_trap_report(vector, frame);
}

/// Synchronous exception from a lower EL. Today that means `svc` from EL0.
///
/// **The syscall number comes from `x8`, not from the instruction's `imm16`.**
///
/// Milestone 6 read it from `ESR_EL1.imm16` and justified that as userspace being unable to lie about
/// which call it made. That justification was wrong twice over. First, `svc #N` encodes `N` in the
/// instruction, so it must be a compile-time constant - a real ABI whose caller selects the number at
/// runtime cannot express it at all, which is exactly what the SDK's `raw_syscall(nr, ...)` needs.
/// Second, there was no security property to lose: a task is *entitled* to request any syscall number,
/// and what stops it doing something it should not is the capability check inside the handler, which
/// trusts neither the register nor the immediate.
///
/// So the number comes from a register: **`x16`**, with arguments in `x0`-`x2` and the result back in
/// `x0`.
///
/// **Not `x8`, which is what Linux uses.** AAPCS64 makes `x8` the *indirect result register* - a
/// function returning a large struct receives its destination pointer there - and the SDK's
/// `raw_syscall` is `#[inline]`, so the `svc` lands inside functions doing exactly that (`recv()`
/// returns a ~4 KiB `Message` by value). The number would overwrite the return-value destination.
/// Linux is safe because its syscalls sit behind a non-inlined wrapper; ours are not.
///
/// Anything that is NOT an SVC is a genuine userspace fault (a bad address, an illegal instruction).
/// Those still report and halt: killing the task is what a real port does, and there is no task
/// structure to kill yet. Reporting beats returning to re-fault forever.
#[no_mangle]
extern "C" fn aarch64_sync_lower_dispatch(vector: u64, frame: *mut TrapFrame) {
    let esr: u64;
    // SAFETY: reading ESR_EL1 at EL1 is side-effect-free and describes the exception just taken.
    unsafe { core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack)) };
    if (esr >> 26) & 0x3F != EC_SVC64 {
        aarch64_trap_report(vector, frame);
    }
    // SAFETY: `frame` is the trap frame the vector assembly built on the current stack.
    let f = unsafe { &mut *frame };
    let nr = f.x[16];

    // Bring-up numbers are handled here; everything else goes to the NEUTRAL dispatcher - the same
    // `syscall_handler` x86 and arm32 call, with the same numbering. The bring-up range is deliberately
    // above every real syscall so the two cannot collide, and it disappears with the demo.
    if nr >= super::usermode::BRINGUP_SYSCALL_BASE {
        super::usermode::syscall(nr, f);
        return;
    }

    // The result goes back in x0, which the restore path reloads from the frame. Arguments come from
    // x0-x2 per the ABI the SDK emits.
    // SAFETY: this is the ring-3 -> ring-0 transition the handler is written for. It treats every
    // argument as untrusted and validates any pointer through the user-copy seam before use.
    let ret = unsafe { crate::syscall::dispatch::syscall_handler(nr, f.x[0], f.x[1], f.x[2]) };
    f.x[0] = ret as u64;
}

/// Report an exception and halt.
///
/// Deliberately does not return. Nothing here can handle a fault yet, and returning to the faulting
/// instruction would loop forever re-faulting and re-printing - a machine that looks busy while making
/// no progress, which is worse than one that stops with a reason.
#[no_mangle]
extern "C" fn aarch64_trap_report(vector: u64, frame: *const TrapFrame) -> ! {
    let (esr, far): (u64, u64);
    // SAFETY: reading ESR_EL1/FAR_EL1 at EL1 is side-effect-free; both describe the exception just
    // taken.
    unsafe {
        core::arch::asm!("mrs {}, esr_el1", out(reg) esr, options(nomem, nostack));
        core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));
    }
    // SAFETY: `frame` is the trap frame the vector assembly just built on the current stack; it is
    // valid for the life of this call and nothing else aliases it.
    let f = unsafe { &*frame };

    super::put_str(b"\r\n*** aarch64 EXCEPTION: ");
    super::put_str(vector_name(vector));
    super::put_str(b"\r\n    ESR_EL1  = ");
    super::put_hex(esr);
    super::put_str(b"  (");
    super::put_str(ec_name(esr));
    super::put_str(b")\r\n    FAR_EL1  = ");
    super::put_hex(far);
    super::put_str(b"\r\n    ELR_EL1  = ");
    super::put_hex(f.elr);
    super::put_str(b"\r\n    SPSR_EL1 = ");
    super::put_hex(f.spsr);
    super::put_str(b"\r\n    x0 = ");
    super::put_hex(f.x[0]);
    super::put_str(b"  x1 = ");
    super::put_hex(f.x[1]);

    // SP_EL0 too. A fault in userspace is very often the stack rather than the code, and without the
    // stack pointer there is no way to tell "wrote past its frame" from "SP was never set correctly" -
    // which look identical in FAR alone.
    let sp_el0: u64;
    // SAFETY: reading SP_EL0 at EL1 is a side-effect-free system-register read.
    unsafe { core::arch::asm!("mrs {}, sp_el0", out(reg) sp_el0, options(nomem, nostack)) };
    super::put_str(b"\r\n    SP_EL0   = ");
    super::put_hex(sp_el0);

    // And WHICH task. A user-mode fault reported without the task's name makes the reader guess from
    // the address, and a guess about which service faulted is the wrong place to start.
    let slot = crate::task::scheduler::current_task_slot();
    if slot < 64 {
        super::put_str(b"\r\n    task     = ");
        super::put_str(crate::task::scheduler::task_stat(slot).name.as_bytes());
        super::put_str(b" (slot ");
        super::put_dec(slot as u64);
        super::put_str(b")");
    }
    // Every register. A fault localised to one instruction is usually solved by seeing which register
    // held the bad address and which held the length, and choosing two to print in advance is guessing
    // the answer before the question.
    for i in 0..31 {
        if i % 4 == 0 {
            super::put_str(b"\r\n    ");
        }
        super::put_str(b"x");
        super::put_dec(i as u64);
        super::put_str(b"=");
        super::put_hex(f.x[i]);
        super::put_str(b" ");
    }
    // A fault from EL0 is the SERVICE's fault, not the kernel's - so the SERVICE dies, and the
    // machine keeps running.
    //
    // This handler used to park the core here unconditionally, for every exception, including a
    // userspace one. That made ANY service fault on this port fatal to the whole machine: the core
    // stopped, the liveness watchdog saw it make no progress, and the kernel panicked. The Pi 4
    // showed the whole sequence when the `xhci` service took an alignment fault:
    //
    //     task = xhci (slot 5) ... halting.
    //     KERNEL PANIC: LIVENESS WEDGE: core 2 made NO progress ... last running task slot 5
    //
    // §10.4 says a protection violation terminates the SERVICE; invariant 6 says services are
    // restartable; §6.2 says the kernel is the only thing that cannot be restarted. x86 and ARMv7
    // both already do this - x86 from its #PF handler, ARMv7 from `arch/arm/exceptions.rs` - so
    // AArch64 was the odd one out, and only because no service had faulted on it yet. A recovery
    // path that has never run is indistinguishable from one that does not exist.
    //
    // The EL0 test is deliberately belt-and-braces. `vector` 8..=11 is the "lower EL, AArch64" set of
    // vector slots, and `SPSR_EL1.M[4:0] == 0` is EL0t - the state the CPU was in when it faulted.
    // Either alone would do; requiring both means a wrong vector-table index cannot be read as a
    // userspace fault and silently kill a task over a KERNEL bug, which would hide the far more
    // serious problem behind a restart.
    let from_el0 = (8..=11).contains(&vector) && (f.spsr & 0x1F) == 0;

    // THE FRAME ITSELF, when the fault came from EL0.
    //
    // The registers above say WHAT was in flight; they cannot say whether the frame under it is
    // intact. That distinction is the whole question in the class of fault this was added for: the
    // Pi 4 shell died repeatedly with `ELR_EL1 = 0`, `x29 = 0` and `x30 = 0` at a stack only 27% deep,
    // which is a branch to address zero from a frame that looks ZEROED rather than one that ran out of
    // room. Registers alone cannot separate "something wrote zeros over the saved LR" from "a null
    // function pointer was called with a legitimately empty frame" - and those are different bugs with
    // different fixes. The words at SP say which.
    //
    // Read through the USER-COPY SEAM, never a raw dereference. The stack we are asked to print
    // belongs to a task that just faulted, so assuming any of it is mapped is exactly the assumption
    // that is already known to be false here. `read_user_bytes` validates the range is user-space and
    // copies under the fault fixup: an unmapped window returns `None` and prints a line saying so,
    // rather than taking a fault INSIDE the fault handler and turning a dead service into a dead
    // machine. A diagnostic that can panic the kernel is not worth the diagnosis.
    //
    // Gated on `from_el0`, the binding computed just above, rather than a second copy of the
    // vector/SPSR test. Two spellings of one condition drift, and the drift here would be silent:
    // the dump would print for a case the kill path does not treat as userspace, or stay quiet for
    // one it does.
    //
    // Bounded at 16 words: enough to cover a small frame's saved-register area, small enough that a
    // fault storm cannot flood the console with a service's stack.
    if from_el0 {
        #[cfg(not(feature = "fault-dump-wide"))]
        const WORDS: usize = 16;
        // Wider under the diagnostic feature: 16 words showed the frame is flattened but not how far
        // the damage runs, and the extent is what names the buffer that overran.
        #[cfg(feature = "fault-dump-wide")]
        const WORDS: usize = 64;
        super::put_str(b"\r\n    stack at SP_EL0 (");
        super::put_dec(WORDS as u64);
        super::put_str(b" words, low to high):");
        match crate::arch::imp::uaccess::read_user_bytes(sp_el0, WORDS * 8) {
            Some(bytes) => {
                for i in 0..WORDS {
                    if i % 2 == 0 {
                        super::put_str(b"\r\n      ");
                        super::put_hex(sp_el0 + (i as u64) * 8);
                        super::put_str(b": ");
                    }
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&bytes[i * 8..i * 8 + 8]);
                    super::put_hex(u64::from_le_bytes(w));
                    super::put_str(b" ");
                }
            }
            // Not a failure of the dump - a FACT about the task, and one worth as much as the words
            // would have been: a stack pointer whose own frame is unreadable is itself the answer.
            None => super::put_str(b"\r\n      unreadable (SP is outside the task's mapped stack)"),
        }

        // HOW FAR THE ZEROS GO.
        //
        // The window above shows the frame is flattened; it cannot show how MUCH of it, and that is
        // the number that identifies the culprit. Three faults on this port shared exact register
        // geometry - `x0 = SP+5`, `x1 = SP-0x1051` - across three unrelated commands, which is a copy
        // overrunning a stack buffer rather than corruption arriving from elsewhere. The EXTENT of
        // the zeroed run is the size of that overrun, and a size is enough to name the buffer.
        //
        // Scans upward from SP for the first non-zero word, in 512-byte bites through the same
        // user-copy seam. Bounded at 8 KiB - twice the 4 KiB request buffers these paths build, so a
        // full overrun of one is inside the window and a runaway scan is not. An unmapped bite ends
        // the scan and says where, which is itself the answer if the stack simply stops there.
        const SCAN_MAX: u64 = 8 * 1024;
        const BITE: usize = 512;
        let mut off: u64 = 0;
        let mut first_nonzero: Option<u64> = None;
        let mut stopped_at: Option<u64> = None;
        while off < SCAN_MAX && first_nonzero.is_none() {
            match crate::arch::imp::uaccess::read_user_bytes(sp_el0 + off, BITE) {
                Some(b) => {
                    for w in 0..BITE / 8 {
                        let mut t = [0u8; 8];
                        t.copy_from_slice(&b[w * 8..w * 8 + 8]);
                        if u64::from_le_bytes(t) != 0 {
                            first_nonzero = Some(off + (w as u64) * 8);
                            break;
                        }
                    }
                    off += BITE as u64;
                }
                None => { stopped_at = Some(off); break; }
            }
        }
        super::put_str(b"\r\n    zeroed run above SP: ");
        match (first_nonzero, stopped_at) {
            (Some(n), _) => {
                super::put_dec(n);
                super::put_str(b" bytes, then a non-zero word at SP+");
                super::put_hex(n);
            }
            (None, Some(end)) => {
                super::put_dec(end);
                super::put_str(b" bytes, then the stack becomes UNREADABLE at SP+");
                super::put_hex(end);
            }
            (None, None) => {
                super::put_str(b"the whole 8 KiB scan window is zero");
            }
        }

        // THE SAME MEASUREMENT, FOR A FILL THAT IS NOT ZERO.
        //
        // The scan above answers "how far do the ZEROS go", and that is the right question only when
        // the overrun was written with zeros. Twice on this port it was not: the frame came back full
        // of 0x20 - ASCII spaces, the padding of a line the shell had just printed - and the report
        // read "zeroed run above SP: 0 bytes", which is true and useless. An instrument that only
        // recognises one fill value reports nothing at all for any other, and reports it confidently.
        //
        // So: take whatever byte is actually AT SP and measure how far THAT repeats. Zero still works
        // (it is just one fill value among others), and a space-filled smash now yields the number
        // that matters - the extent of the run, which is the size of the overrun, which is enough to
        // name the buffer that produced it.
        #[cfg(feature = "fault-dump-wide")]
        {
            let fill = match crate::arch::imp::uaccess::read_user_bytes(sp_el0, 8) {
                Some(b) => b[0],
                None    => 0,
            };
            let mut run: u64 = 0;
            let mut foff: u64 = 0;
            'outer: while foff < SCAN_MAX {
                match crate::arch::imp::uaccess::read_user_bytes(sp_el0 + foff, BITE) {
                    Some(b) => {
                        for k in 0..BITE {
                            if b[k] != fill { break 'outer; }
                            run += 1;
                        }
                        foff += BITE as u64;
                    }
                    None => break,
                }
            }
            super::put_str(b"
    fill run above SP: byte 0x");
            super::put_hex(fill as u64);
            super::put_str(b" repeats for ");
            super::put_dec(run);
            super::put_str(b" bytes");
        }

        // A BACKTRACE, the way an oops has one.
        //
        // Registers say what was in flight and the window says the frame is flattened; neither
        // names the function, and that name is the whole question. `x29` is zero here, so the frame
        // CHAIN is broken and cannot be walked - which is exactly the case Linux `dump_backtrace`
        // falls back to scanning for. Older frames further up the stack still hold their saved link
        // registers, and a link register is a code address.
        //
        // So: scan upward and print every word that could be one. The service text is mapped at
        // 0x400000 and the user stack top is 0x8000_0000, so a candidate is a 4-byte-aligned word
        // inside the text window - a heuristic, and named as one: some of these are stale slots from
        // calls that already returned. It is a list of suspects, not a call chain, and that is still
        // the difference between reading a name and guessing one.
        //
        // Bounded the same way as the scan above: 8 KiB, 512-byte bites, same user-copy seam, and at
        // most 12 printed so a deep stack cannot flood the console.
        const TEXT_LO: u64 = 0x0040_0000;
        // 5 MiB, not 8. The first window ran to 0x0080_0000 and its one and only "hit" was
        // 0x747874 - the ASCII bytes "txt", the tail of a filename sitting in a buffer. A service's
        // .text is ~200 KiB (the shell measures 0x34800), so nothing legitimate lives above 5 MiB and
        // a looser bound only invites string data to impersonate a return address. A backtrace that
        // reports one false name is worse than one that reports none, because a name gets believed.
        const TEXT_HI: u64 = 0x0050_0000;
        // Scan the LIVE STACK, not a fixed window. The first version borrowed the 8 KiB cap from the
        // zero-run scan and reported "none found" - true of those 8 KiB, false of the stack: SP sat
        // 71,664 bytes below the top with every caller frame above it, and a 6,208-byte
        // zero-initialised local in the faulting frame alone pushed the nearest saved link register
        // out of range. A window that cannot reach the answer returns a confident nothing.
        //
        // The extent now comes from the stack itself - SP to the user stack top - capped at 64 KiB so
        // a bogus SP cannot walk forever.
        const USER_STACK_TOP: u64 = 0x8000_0000;
        let room = USER_STACK_TOP.saturating_sub(sp_el0).min(64 * 1024);
        #[cfg(not(feature = "fault-dump-wide"))]
        const BT_MAX: u32 = 12;
        // More suspects under the diagnostic feature: the production cap of 12 is right for a report a
        // human reads, but these get symbolized offline against the service ELF, where a longer list of
        // candidates is strictly more to work with.
        #[cfg(feature = "fault-dump-wide")]
        const BT_MAX: u32 = 48;
        let mut shown = 0u32;
        let mut boff: u64 = 0;
        let mut scanned: u64 = 0;
        super::put_str(b"\r\n    return addresses on the stack (newest first, may include stale slots):");
        while boff < room && shown < BT_MAX {
            let Some(b) = crate::arch::imp::uaccess::read_user_bytes(sp_el0 + boff, BITE) else { break };
            scanned = boff + BITE as u64;   // how far we ACTUALLY got
            for w in 0..BITE / 8 {
                if shown >= BT_MAX { break; }
                let mut t = [0u8; 8];
                t.copy_from_slice(&b[w * 8..w * 8 + 8]);
                let v = u64::from_le_bytes(t);
                if v >= TEXT_LO && v < TEXT_HI && v & 3 == 0 {
                    super::put_str(b"\r\n      SP+");
                    super::put_hex(boff + (w as u64) * 8);
                    super::put_str(b" -> ");
                    super::put_hex(v);
                    shown += 1;
                }
            }
            boff += BITE as u64;
        }
        if shown == 0 {
            // REPORT WHAT WAS SCANNED, not what was intended. This printed `room` - the distance from
            // SP to the stack top, computed BEFORE the loop - so a scan that broke early on an
            // unreadable page still claimed to have covered all of it, and "no return addresses" read
            // as a fact about the stack when it might only be a fact about the first 512 bytes.
            super::put_str(b"
      none found in ");
            super::put_dec(scanned);
            super::put_str(b" bytes ACTUALLY READ (of ");
            super::put_dec(room);
            super::put_str(b" above SP)");
        }

        // IS THE SHELL`S CANARY STILL THERE?
        //
        // `run_lines` plants 1024 bytes of 0x5A in its frame and checks them AFTER each statement -
        // which is useless when the fault kills the task DURING one, because the check never runs. So
        // "canary not reported" has never meant "canary intact"; it meant nobody looked.
        //
        // The kernel can look, and the answer decides the shape of the bug. A surviving 0x5A run means
        // the damage is LOCAL to a deeper frame. No 0x5A anywhere means the overwrite swallowed
        // `run_lines` frame as well, which - with no return address left in 35 KB of stack - would say
        // the whole call chain has been flattened rather than one saved register pair clobbered.
        {
            let mut best: u64 = 0;      // longest 0x5A run found
            let mut best_at: u64 = 0;
            let mut cur: u64 = 0;
            let mut coff: u64 = 0;
            while coff < room {
                let Some(b) = crate::arch::imp::uaccess::read_user_bytes(sp_el0 + coff, BITE) else { break };
                for k in 0..BITE {
                    if b[k] == 0x5A {
                        cur += 1;
                        if cur > best { best = cur; best_at = coff + k as u64 + 1 - cur; }
                    } else { cur = 0; }
                }
                coff += BITE as u64;
            }
            super::put_str(b"
    shell canary (0x5A): longest run ");
            super::put_dec(best);
            super::put_str(b" bytes at SP+");
            super::put_hex(best_at);
            super::put_str(b" (planted: 1024)");
        }

        // The SECOND canary (0xC3), planted in `execute` - a frame BELOW `run_lines`, i.e. between SP
        // and the first canary. Which of the two survives brackets where the upward write begins.
        {
            let mut best: u64 = 0;
            let mut best_at: u64 = 0;
            let mut cur: u64 = 0;
            let mut coff: u64 = 0;
            while coff < room {
                let Some(b) = crate::arch::imp::uaccess::read_user_bytes(sp_el0 + coff, BITE) else { break };
                for k in 0..BITE {
                    if b[k] == 0xC3 {
                        cur += 1;
                        if cur > best { best = cur; best_at = coff + k as u64 + 1 - cur; }
                    } else { cur = 0; }
                }
                coff += BITE as u64;
            }
            super::put_str(b"
    execute canary (0xC3): longest run ");
            super::put_dec(best);
            super::put_str(b" bytes at SP+");
            super::put_hex(best_at);
            super::put_str(b" (planted: 256 per depth)");
        }

        // WHAT HAS THE KERNEL WRITTEN INTO USER MEMORY?
        //
        // Everything else is ruled out: the stack is fully mapped, no other task maps its frames, the
        // shell has no `unsafe`, and safe-Rust writes are bounds-checked. The kernel writing through a
        // user pointer is the one remaining writer that can cross those guarantees, so this reports the
        // LARGEST such write and where it landed. A multi-kilobyte write into this stack`s range names
        // the syscall outright; a small maximum exonerates the kernel and the bug is stranger still.
        {
            use core::sync::atomic::Ordering::Relaxed;
            let n = super::uaccess::UW_MAX_LEN.load(Relaxed);
            let d = super::uaccess::UW_MAX_DST.load(Relaxed);
            super::put_str(b"
    kernel->user writes: ");
            super::put_dec(super::uaccess::UW_COUNT.load(Relaxed));
            super::put_str(b" totalling ");
            super::put_dec(super::uaccess::UW_TOTAL.load(Relaxed));
            super::put_str(b" bytes; largest ");
            super::put_dec(n);
            super::put_str(b" bytes to ");
            super::put_hex(d);
            if d >= 0x7FFC_0000 && d < 0x8000_0000 {
                super::put_str(b" - WHICH IS INSIDE A USER STACK");
            }
        }
    }

    if from_el0 && slot < crate::task::scheduler::MAX_TASKS {
        super::put_str(b"\r\n    EL0 fault - killing the task; the kernel and every other service continue.\r\n");
        crate::task::kill_current();
        // `kill_current` marks the task Dead and reschedules, so it does not come back. If it ever
        // does, falling through to the halt below is right: resuming a corpse's context is worse
        // than stopping, and the line above will be in the log to say what happened.
    }

    // A fault at EL1 IS the kernel's, and there is nothing above it to recover into. Halting loudly
    // is the correct end of that path (§26.7), not a missing feature.
    super::put_str(b"\r\n    halting.\r\n");
    loop {
        // SAFETY: WFE is always valid; park forever.
        unsafe { core::arch::asm!("wfe") };
    }
}
