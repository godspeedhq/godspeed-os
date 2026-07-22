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

/// SVC is a *syscall*, not a fault: it marshals `(r0-r3) = (number, args)` into the neutral
/// dispatcher and returns, rather than reporting and halting like the fault vectors.
///
/// `LR_svc`/`SPSR_svc` are saved FIRST (the kernel already runs in SVC, so the next `bl` would
/// clobber the return address otherwise), then `arm_svc_dispatch` is called with `r0-r3` already in
/// place, and `movs pc, lr` returns to the caller restoring its CPSR from the saved SPSR. `r0:r1`
/// carry the `i64` result out untouched by the restore (only `r4-r12`/`lr` are popped).
#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_svc() {
    core::arch::naked_asm!(
        // Mask IRQs for the duration of the syscall. ARM's `svc` does NOT auto-mask (unlike x86's
        // syscall entry disabling IF), but the neutral handlers ASSUME interrupts are off - notably the
        // spinlocks: an IRQ taken mid-`ldrex`/`strex` does an implicit CLREX, so with interrupts left
        // enabled a lock livelocks (the exact timing-dependent hang the first service spawn hit). The
        // return via `movs pc, lr` restores the caller's CPSR from SPSR, re-enabling IRQs in USR.
        "cpsid i",
        "push {{r4-r12, lr}}",          // save callee-saved + the SVC return address (LR_svc)
        // Save the caller's USER-banked SP_usr/LR_usr on ITS OWN kernel stack. A syscall that blocks
        // (recv/console_read/yield) switches to another USER task, which runs in ring 3 and clobbers
        // the shared USER bank; without this the caller resumes on the WRONG user stack (the shell,
        // woken from console_read, resumed on the logger's shallow SP and faulted just above the stack
        // top). Stacking it here (not in switch_context) keeps it per-task, so it survives every switch
        // inside the syscall and is restored on the way out - the syscall-path twin of stub_irq's
        // trap-frame USER-bank save. `r6` is callee-saved (pushed above), free to use as a scratch base;
        // the `^` user-register transfer forbids sp in the list + writeback, so a base register carries
        // the address and sp is adjusted separately.
        "sub  sp, sp, #8",              // room for usr_sp, usr_lr below the callee-saved frame
        "mov  r6, sp",                  // r6 = base
        "stmia r6, {{sp, lr}}^",        // store USER bank r13/r14 at [sp], [sp+4]
        "mrs  r4, spsr",                // the caller's CPSR (SPSR_svc): carries the caller's mode
        "ldr  r5, ={spsr_save}",        // publish it so a syscall can see the caller's privilege level
        "str  r4, [r5]",                //   (used to prove PL0 in the user-mode selftest)
        "bl   {dispatch}",              // arm_svc_dispatch(r0..r3) -> i64 in r0:r1
        // Re-mask IRQs for the exit. The neutral scheduler re-enables interrupts inside a
        // yield/block dispatch (enable_interrupts after switch_context), so on return IRQs are ON.
        // But SPSR_svc is a SINGLE shared banked register, and the exit's `movs pc, lr` restores the
        // caller's CPSR from it - so a timer taken between `msr spsr` and `movs pc` would let another
        // task's syscall clobber SPSR_svc, and the caller would resume with a corrupt CPSR (the wild-PC
        // fault the ARM IPC path hit). Masking here makes the SPSR-restore -> movs-pc window
        // uninterruptible; `movs pc` then re-enables IRQs from the restored USER CPSR. (x86 has no
        // analogue: SYSRET restores RIP/RFLAGS atomically from registers, nothing banked or shared.)
        "cpsid i",
        "msr  spsr_cxsf, r4",           // restore SPSR for the exception return (now uninterruptible)
        // Restore the caller's USER-banked SP_usr/LR_usr (clobbered if the syscall switched tasks).
        "mov  r6, sp",                  // r6 = base (still points at usr_sp/usr_lr)
        "ldmia r6, {{sp, lr}}^",        // restore USER bank r13/r14
        "nop",                          // banked-register hazard spacer
        "add  sp, sp, #8",              // drop the user-bank words
        "pop  {{r4-r12, lr}}",          // restore callee-saved + LR_svc; r0:r1 (result) untouched
        "movs pc, lr",                  // return to caller, restoring CPSR (IRQs on for USR) from SPSR
        dispatch = sym crate::arch::arm::syscall::arm_svc_dispatch,
        spsr_save = sym crate::arch::arm::usermode::USER_SPSR_SAVE,
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
/// - `push {{r0-r12, lr}}` - the rest of the SVC-visible frame. `lr` here is `LR_svc`, the task's own
///   link register, no longer the IRQ one.
/// - **`stmdb r0, {{sp, lr}}^` - save the interrupted task's USER-banked `SP_usr`/`LR_usr`.** This is
///   what makes preempting a *user* task correct once more than one exists: user mode banks its own
///   `sp`/`lr`, and if task B runs in ring 3 between task A's preemption and resume it clobbers those
///   banked registers. Without stacking them per-task, A would resume on B's user stack. The `^`
///   suffix is the only way SVC mode can reach the USER bank; it forbids base-register writeback and
///   `sp` in the list, so a scratch base (`r0`) carries the address and `sp` is adjusted separately.
///   (For a *kernel* task these are the unused System-bank `r13`/`r14`: saved and restored harmlessly.)
/// - The dispatcher receives the frame pointer and **returns the frame to resume**. Returning a
///   different pointer is the entire mechanism of preemption: `mov sp, r0` adopts another task's
///   stack, and everything below restores *that* task instead.
/// - `rfeia sp!` - Return From Exception: reloads PC and CPSR from the frame in one instruction,
///   atomically resuming the target task in its own mode with its own interrupt state.
///
/// The frame is 18 words (72 bytes), a multiple of 8, so AAPCS stack alignment survives the call. Its
/// layout is mirrored by `context::TrapFrame` (`usr_sp, usr_lr, r0..r12, lr_svc, pc, spsr`).
#[unsafe(naked)]
#[no_mangle]
unsafe extern "C" fn stub_irq() {
    core::arch::naked_asm!(
        "sub   lr, lr, #4",
        "srsdb sp!, #0x13",             // push resume PC + SPSR onto the SVC stack
        "cps   #0x13",                  // switch to SVC: the interrupted task's stack
        "push  {{r0-r12, lr}}",         // r0-r12 + LR_svc
        "mov   r0, sp",                 // r0 -> the r0 slot (top of the pushed regs)
        "stmdb r0, {{sp, lr}}^",        // save USER-banked SP_usr/LR_usr just below (no writeback)
        "sub   sp, sp, #8",             // sp -> usr_sp slot: the frame now includes them
        "mov   r0, sp",                 // r0 = &TrapFrame
        "bl    {dispatch}",             // -> returns the frame to resume (maybe a different task)
        "mov   sp, r0",                 // adopt it: THIS is the task switch
        "mov   r0, sp",                 // r0 = &TrapFrame (base for the ^ load; must not be sp)
        "add   sp, sp, #8",             // advance SVC sp past usr_sp/usr_lr
        "ldmia r0, {{sp, lr}}^",        // restore USER-banked SP_usr/LR_usr from the frame
        "nop",                          // banked-register hazard spacer (one instr before touching sp)
        "pop   {{r0-r12, lr}}",         // restore r0-r12 + LR_svc
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

/// Per-core exception mode stacks for the SECONDARY cores (SMP). Core 0 uses the single linker-symbol
/// stacks (`__irq_stack_top` etc.); an AP must NOT share them - two cores taking a timer IRQ at once
/// would both push onto the one IRQ stack and corrupt each other. Each AP gets its own 8 KiB per mode.
const AP_MODE_STACK: usize = 8 * 1024;
#[repr(C, align(16))]
struct ApModeStacks {
    abt: [u8; AP_MODE_STACK],
    und: [u8; AP_MODE_STACK],
    irq: [u8; AP_MODE_STACK],
    fiq: [u8; AP_MODE_STACK],
}
static mut AP_MODE_STACKS: [ApModeStacks; 3] = [const {
    ApModeStacks { abt: [0; AP_MODE_STACK], und: [0; AP_MODE_STACK],
                   irq: [0; AP_MODE_STACK], fiq: [0; AP_MODE_STACK] }
}; 3];

/// Install the exception vectors and prime the banked mode stacks on the CALLING core. Core 0 uses
/// the shared linker stacks (`install`); a secondary core (`core` in 1..=3) uses its own BSS stacks so
/// concurrent exceptions on different cores never share a mode stack. The vector TABLE is shared (VBAR
/// points every core at the same `arm_vectors`); only the banked SPs differ per core.
pub fn install_for_core(core: u32) {
    if core == 0 {
        install();
        return;
    }
    // SAFETY: `core` in 1..=3 selects this core's dedicated mode stacks; no other core touches them.
    let (abt, und, irq, fiq) = unsafe {
        let m = &raw const AP_MODE_STACKS[(core - 1) as usize];
        let top = |field: *const [u8; AP_MODE_STACK]| field as u32 + AP_MODE_STACK as u32;
        (top(&raw const (*m).abt), top(&raw const (*m).und),
         top(&raw const (*m).irq), top(&raw const (*m).fiq))
    };
    // SAFETY: mirrors `install` - VBAR write then per-mode banked-SP loads, ending back in SVC. The
    // stacks are this core's own, 16-byte aligned, in BSS. No banked register other than each mode's
    // own SP is touched, and the caller's SVC SP/LR are untouched (we return in SVC by name).
    unsafe {
        core::arch::asm!(
            "mcr  p15, 0, {vec}, c12, c0, 0", // VBAR = shared table
            "isb",
            "msr  cpsr_c, #0xD7", "mov sp, {abt}",   // ABT
            "msr  cpsr_c, #0xDB", "mov sp, {und}",   // UND
            "msr  cpsr_c, #0xD2", "mov sp, {irq}",   // IRQ
            "msr  cpsr_c, #0xD1", "mov sp, {fiq}",   // FIQ
            "msr  cpsr_c, #0xD3",                    // back to SVC by name
            vec = in(reg) arm_vectors as *const () as u32,
            abt = in(reg) abt, und = in(reg) und, irq = in(reg) irq, fiq = in(reg) fiq,
        );
    }
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
