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
    VECTOR_ENTRY 3                 //                      SError
    VECTOR_SYNC_CURRENT 4          // Current EL, SP_ELx:  Synchronous (user-copy faults land here)
    VECTOR_IRQ   5                 //                      IRQ
    VECTOR_ENTRY 6                 //                      FIQ
    VECTOR_ENTRY 7                 //                      SError
    VECTOR_SYNC_LOWER 8            // Lower EL, AArch64:   Synchronous (svc lands here)
    VECTOR_IRQ   9                 //                      IRQ
    VECTOR_ENTRY 10                //                      FIQ
    VECTOR_ENTRY 11                //                      SError
    VECTOR_ENTRY 12                // Lower EL, AArch32:   Synchronous
    VECTOR_IRQ   13                //                      IRQ
    VECTOR_ENTRY 14                //                      FIQ
    VECTOR_ENTRY 15                //                      SError

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
    str  x3,       [sp, #256]      // SPSR
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
    ldr  x3,       [sp, #256]      // SPSR
    ldp  x30, x2,  [sp, #240]      // x30, ELR
    msr  elr_el1,  x2
    msr  spsr_el1, x3
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

/// IRQ entry. Acknowledge at the GIC, handle, end-of-interrupt, return.
///
/// The EOI is not optional bookkeeping: the CPU interface keeps a priority active for every
/// acknowledged interrupt, so skipping it silently blocks all later interrupts of equal or lower
/// priority. The symptom is "interrupts stopped" with nothing to point at, which is why the retire
/// happens on every path out of here including the spurious one.
#[no_mangle]
extern "C" fn aarch64_irq_dispatch(_vector: u64, _frame: *mut TrapFrame) {
    let id = super::gic::acknowledge();
    if id == super::gic::SPURIOUS {
        return; // nothing pending - do NOT EOI an ID that was never raised
    }
    if id == super::timer::TIMER_PPI {
        TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // Re-arm: the generic timer is one-shot, so a periodic tick means reloading it every time.
        super::timer::arm(super::TIMER_INTERVAL.load(core::sync::atomic::Ordering::Relaxed));

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
/// So this follows the same shape Linux uses on AArch64: number in `x8`, arguments in `x0`-`x2`, result
/// back in `x0`. Keeping `x0`-`x2` free for arguments is the practical reason for `x8` specifically.
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
    let nr = f.x[8];

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
    super::put_str(b"\r\n    halting.\r\n");
    loop {
        // SAFETY: WFE is always valid; park forever.
        unsafe { core::arch::asm!("wfe") };
    }
}
