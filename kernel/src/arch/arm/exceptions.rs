// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7-A exception vectors - the "failures are loud" floor for the 32-bit ARM port.
//!
//! Until this exists, ANY fault on ARMv7 is a silent lockup: no vector table means the CPU jumps to
//! whatever happens to sit at address 0 and wanders off. That is precisely the silent-failure mode
//! invariant 12 forbids (§3, "failures are loud, never silent"), and it makes every later bring-up
//! step - especially the MMU, which fails in subtle ways - undebuggable. So vectors come first, before
//! the MMU and before the timer.
//!
//! What ARMv7 wants here is quite unlike AArch64. There are **eight** vectors, each ONE instruction
//! wide (so each is a branch to the real handler), and the CPU switches to a **different processor
//! mode** per exception - each with its own banked `SP`. A handler that runs before its mode has a
//! stack will fault again inside the fault, so `install()` primes ABT/UND/IRQ/FIQ stacks up front.
//!
//! This milestone REPORTS and halts; it does not yet recover. Recovery (kill the faulting task, keep
//! the kernel alive - the C2/A14/A15 property on x86) needs the MMU, tasks, and a scheduler, none of
//! which exist on ARM yet. Reporting first is deliberate: it is the smallest thing that turns a
//! mystery hang into a diagnosis.

use super::{pl011_write, pl011_write_byte};

/// Exception kinds, in vector-table order. The value is passed in `r0` by each stub below.
const KIND_RESET: u32 = 0;
const KIND_UNDEF: u32 = 1;
const KIND_SVC: u32 = 2;
const KIND_PREFETCH_ABORT: u32 = 3;
const KIND_DATA_ABORT: u32 = 4;
const KIND_RESERVED: u32 = 5;
const KIND_IRQ: u32 = 6;
const KIND_FIQ: u32 = 7;

fn kind_name(k: u32) -> &'static [u8] {
    match k {
        KIND_RESET => b"RESET",
        KIND_UNDEF => b"UNDEFINED INSTRUCTION",
        KIND_SVC => b"SUPERVISOR CALL",
        KIND_PREFETCH_ABORT => b"PREFETCH ABORT",
        KIND_DATA_ABORT => b"DATA ABORT",
        KIND_RESERVED => b"RESERVED VECTOR",
        KIND_IRQ => b"IRQ",
        KIND_FIQ => b"FIQ",
        _ => b"UNKNOWN",
    }
}

/// Print `0x` + 8 hex digits. Fixed 8-byte stack buffer, no allocation (§26.6.1) - this runs on a
/// fault path where nothing about the machine's state can be assumed.
pub(super) fn write_hex32(v: u32) {
    let mut buf = [0u8; 8];
    for i in 0..8 {
        let nib = ((v >> ((7 - i) * 4)) & 0xF) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
    }
    pl011_write(b"0x");
    pl011_write(&buf);
}

/// Decode the ARMv7 fault-status encoding far enough to name the common cases. `status` is DFSR for a
/// data abort or IFSR for a prefetch abort; the status code is split across bits [3:0], [10] and the
/// domain in [7:4]. Naming just the frequent ones beats printing a bare number a porter must look up.
fn fault_cause(status: u32) -> &'static [u8] {
    let fs = (status & 0xF) | ((status >> 6) & 0x10);
    match fs {
        0x01 => b"alignment fault",
        0x05 => b"translation fault (section) - NOT MAPPED",
        0x07 => b"translation fault (page) - NOT MAPPED",
        0x09 => b"domain fault (section)",
        0x0B => b"domain fault (page)",
        0x0D => b"permission fault (section)",
        0x0F => b"permission fault (page)",
        0x08 => b"synchronous external abort",
        _ => b"see ARMv7-AR ARM, DFSR/IFSR fault status encoding",
    }
}

/// Common exception reporter. Called by every vector stub with the kind, the faulting PC, and (for
/// aborts) the fault status and address registers.
///
/// `extern "C"` and `-> !` because the stubs `b` here rather than `bl` - there is no return.
#[no_mangle]
extern "C" fn arm_exception_report(kind: u32, pc: u32, status: u32, addr: u32) -> ! {
    pl011_write(b"\r\n\r\n=== GodspeedOS arm32 EXCEPTION: ");
    pl011_write(kind_name(kind));
    pl011_write(b" ===\r\n");

    pl011_write(b"  pc (faulting instruction): ");
    write_hex32(pc);
    pl011_write(b"\r\n");

    if kind == KIND_DATA_ABORT || kind == KIND_PREFETCH_ABORT {
        pl011_write(b"  fault address:             ");
        write_hex32(addr);
        pl011_write(b"\r\n  fault status:              ");
        write_hex32(status);
        pl011_write(b"  (");
        pl011_write(fault_cause(status));
        pl011_write(b")\r\n");
    }

    pl011_write(b"  halting - the arm32 port cannot yet kill a task and continue.\r\n\r\n");
    loop {
        // SAFETY: WFI is always valid. Halt rather than return into a faulted context.
        unsafe { core::arch::asm!("wfi") }
    }
}

/// The vector table: eight entries, one instruction each, in architectural order.
///
/// `.text.vectors` + an `ALIGN(32)` in the linker script, because VBAR ignores the low 5 bits - a
/// misaligned table would be silently truncated to a 32-byte boundary and vector into the wrong code.
///
/// Each stub adjusts LR to the *faulting* instruction before reporting. ARMv7 leaves LR pointing PAST
/// the fault by a mode-specific amount, and the amount differs per exception: 8 for a data abort, 4
/// for prefetch abort / undefined / IRQ. Printing an unadjusted LR would send a porter hunting the
/// wrong instruction, which is worse than printing nothing.
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.vectors"]
pub unsafe extern "C" fn arm_vectors() -> ! {
    core::arch::naked_asm!(
        "b {reset}",
        "b {undef}",
        "b {svc}",
        "b {pabt}",
        "b {dabt}",
        "b {resv}",
        "b {irq}",
        "b {fiq}",
        reset = sym stub_reset,
        undef = sym stub_undef,
        svc   = sym stub_svc,
        pabt  = sym stub_pabt,
        dabt  = sym stub_dabt,
        resv  = sym stub_reserved,
        irq   = sym stub_irq,
        fiq   = sym stub_fiq,
    )
}

// Each stub loads (kind, pc, status, addr) into r0-r3 and branches to the reporter. No register save:
// we never return, so there is nothing to restore.
//
//   DFSR = c5,c0,0   IFSR = c5,c0,1   DFAR = c6,c0,0   IFAR = c6,c0,2

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_reset() -> ! {
    core::arch::naked_asm!(
        "mov r0, #0", "mov r1, lr", "mov r2, #0", "mov r3, #0",
        "b {rep}", rep = sym arm_exception_report,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_undef() -> ! {
    core::arch::naked_asm!(
        "sub lr, lr, #4",              // undefined: LR = faulting instr + 4
        "mov r0, #1", "mov r1, lr", "mov r2, #0", "mov r3, #0",
        "b {rep}", rep = sym arm_exception_report,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_svc() -> ! {
    core::arch::naked_asm!(
        "sub lr, lr, #4",
        "mov r0, #2", "mov r1, lr", "mov r2, #0", "mov r3, #0",
        "b {rep}", rep = sym arm_exception_report,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_pabt() -> ! {
    core::arch::naked_asm!(
        "sub lr, lr, #4",              // prefetch abort: LR = faulting instr + 4
        "mov r0, #3",
        "mov r1, lr",
        "mrc p15, 0, r2, c5, c0, 1",   // IFSR
        "mrc p15, 0, r3, c6, c0, 2",   // IFAR
        "b {rep}", rep = sym arm_exception_report,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_dabt() -> ! {
    core::arch::naked_asm!(
        "sub lr, lr, #8",              // data abort: LR = faulting instr + 8
        "mov r0, #4",
        "mov r1, lr",
        "mrc p15, 0, r2, c5, c0, 0",   // DFSR
        "mrc p15, 0, r3, c6, c0, 0",   // DFAR
        "b {rep}", rep = sym arm_exception_report,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_reserved() -> ! {
    core::arch::naked_asm!(
        "mov r0, #5", "mov r1, lr", "mov r2, #0", "mov r3, #0",
        "b {rep}", rep = sym arm_exception_report,
    )
}

/// IRQ is the one exception that **returns** - and now the one that can return somewhere *else*.
///
/// A cooperative switch (`context.rs`) gets to assume a function-call boundary, so AAPCS lets it save
/// ten registers. Preemption has no such luxury: the interrupt lands between two arbitrary
/// instructions with anything live, so the **entire** register file plus the return PC and `SPSR`
/// must be captured. That whole set is the *trap frame*, and it is built on the interrupted task's
/// own stack - which is what makes switching tasks a matter of switching `sp`.
///
/// The ARMv7 problem this dance solves: on IRQ entry the CPU is in IRQ mode, where the interrupted
/// mode's `sp` and `lr` are **banked away and unreachable**. Saving them means getting back into the
/// interrupted mode first, which is what `srsdb` + `cps` achieve between them:
///
/// - `sub lr, lr, #4` - on IRQ entry ARMv7 leaves LR one instruction past the resume point.
/// - `srsdb sp!, #0x13` - Store Return State: pushes `LR_irq` (the resume PC) and `SPSR_irq` onto the
///   **SVC** mode's stack, reaching across the mode banking rather than fighting it.
/// - `cps #0x13` - now switch to SVC, standing on the interrupted task's own stack.
/// - `push {{r0-r12, lr}}` - the rest of the frame. `lr` here is `LR_svc`, the task's own link
///   register, no longer the IRQ one.
/// - The dispatcher receives the frame pointer and **returns the frame to resume**. Returning a
///   different pointer is the entire mechanism of preemption: `mov sp, r0` adopts another task's
///   stack, and everything below restores *that* task instead.
/// - `rfeia sp!` - Return From Exception: reloads PC and CPSR from the frame in one instruction,
///   atomically resuming the target task in its own mode with its own interrupt state.
///
/// The frame is 16 words (64 bytes), a multiple of 8, so AAPCS stack alignment survives the call.
#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_irq() {
    core::arch::naked_asm!(
        "sub   lr, lr, #4",
        "srsdb sp!, #0x13",             // push resume PC + SPSR onto the SVC stack
        "cps   #0x13",                  // switch to SVC: the interrupted task's stack
        "push  {{r0-r12, lr}}",         // rest of the frame
        "mov   r0, sp",                 // r0 = &TrapFrame
        "bl    {dispatch}",             // -> returns the frame to resume (maybe a different task)
        "mov   sp, r0",                 // adopt it: THIS is the task switch
        "pop   {{r0-r12, lr}}",
        "rfeia sp!",                    // restore PC + CPSR together
        dispatch = sym crate::arch::arm::irq::arm_irq_dispatch,
    )
}

#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_fiq() -> ! {
    core::arch::naked_asm!(
        "sub lr, lr, #4",
        "mov r0, #7", "mov r1, lr", "mov r2, #0", "mov r3, #0",
        "b {rep}", rep = sym arm_exception_report,
    )
}

/// Give ABT/UND/IRQ/FIQ modes their own stacks, then point VBAR at the table.
///
/// The per-mode stacks are not optional. Each mode banks its own `SP`, and at reset those hold
/// garbage; a handler that pushes before its stack is set faults *inside* the fault handler, which on
/// ARMv7 is an unrecoverable loop with no output - the exact silent hang this module exists to
/// prevent. Each mode gets 4 KiB, which is ample for report-and-halt (§26.6.1: fixed, visible bound).
///
/// Interrupts stay masked throughout (the `0xD?` CPSR values below all set I and F).
///
/// **FIQ mode banks r8-r12.** This is the trap that bit the first version: it stashed the caller's
/// CPSR in `r12`, walked through FIQ mode to set its stack, then restored CPSR from `r12` - but
/// inside FIQ that name refers to a *different* physical register holding garbage, so the restore
/// loaded a nonsense mode and reset the CPU (the tell was the boot banner printing twice, and VBAR
/// reading back as 0). Hence: VBAR is programmed FIRST, while still in SVC with nothing banked, and
/// the walk ends by naming SVC explicitly rather than restoring a saved value. Nothing is carried
/// across a mode switch.
pub fn install() {
    // SAFETY: Runs once on the boot core with interrupts masked and no other core started.
    // `mcr p15, 0, _, c12, c0, 0` writes VBAR (architecturally valid at PL1) with our 32-byte-aligned
    // table - done first, before any mode switch, so no operand can be a banked register. Each
    // `msr cpsr_c` then switches mode solely to load that mode's banked SP from a linker symbol; the
    // sequence ends in SVC, the mode the caller runs in, so the caller's own SP and LR are untouched.
    unsafe {
        core::arch::asm!(
            "mcr  p15, 0, {vec}, c12, c0, 0", // VBAR = our table (while still in SVC)
            "isb",
            "msr  cpsr_c, #0xD7",          // ABT, I+F masked
            "ldr  sp, =__abt_stack_top",
            "msr  cpsr_c, #0xDB",          // UND
            "ldr  sp, =__und_stack_top",
            "msr  cpsr_c, #0xD2",          // IRQ
            "ldr  sp, =__irq_stack_top",
            "msr  cpsr_c, #0xD1",          // FIQ  (r8-r12 are banked from here)
            "ldr  sp, =__fiq_stack_top",
            "msr  cpsr_c, #0xD3",          // back to SVC by NAME, not from a saved register
            vec = in(reg) arm_vectors as *const () as u32,
        );
    }
    pl011_write(b"arm32: exception vectors installed (VBAR = ");
    write_hex32(arm_vectors as *const () as u32);
    pl011_write_byte(b')');
    pl011_write(b"\r\n");
}

/// Deliberately take a data abort, to prove the vectors actually fire.
///
/// Gated behind a build feature so it can never run in a normal boot. This is the ARM twin of the
/// x86 adversarial fault tests (§22, A14/A15/C2): a fault path that has not been *observed* firing is
/// not evidence of anything. With the MMU still off there is no unmapped low memory to touch, so we
/// read from the top of the 32-bit space, far above the Pi 2's 948 MiB of RAM.
#[cfg(feature = "arm-fault-test")]
pub fn trigger_test_fault() {
    pl011_write(b"arm32: [arm-fault-test] deliberately reading 0xFFFF_FFF0 ...\r\n");
    // SAFETY: Intentionally unsound - this read is EXPECTED to fault. That is the test: the data
    // abort vector must fire and report. Compiled only under the `arm-fault-test` feature.
    unsafe {
        let p = 0xFFFF_FFF0usize as *const u32;
        core::ptr::read_volatile(p);
    }
    pl011_write(b"arm32: [arm-fault-test] NO FAULT - vectors did NOT fire. This is a BUG.\r\n");
}
