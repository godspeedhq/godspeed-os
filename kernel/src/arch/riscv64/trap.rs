// SPDX-License-Identifier: GPL-2.0-only
//! S-mode trap vector - what turns a fault from silence into a sentence, and the one door user mode
//! comes back through.
//!
//! Until `stvec` is set, a fault in S-mode has nowhere to go. On this hardware that means the
//! machine stops with no output, which is the worst failure this project recognises: invariant 12
//! asks for loud failure, and an unhandled trap is the loudest thing a CPU can do reported as the
//! quietest thing a log can show.
//!
//! **A fault from USER mode kills the task; a fault in the KERNEL halts the machine.** The hardware
//! decides which, and cannot be argued with: `sstatus.SPP` records the privilege the trap came from.
//! A faulting service has a name, an owner and a supervisor that will restart it, so killing it is
//! both possible and correct - "a service dies, the system continues" has to hold for a FAULT and
//! not only for a deliberate kill, because a fault is the case nobody planned for. A faulting KERNEL
//! has none of that: the thing that faulted is the thing that would do the killing, so it stops
//! loudly instead.
//!
//! This file was a pure REPORTER until there was a task to kill, which is the honest order: make
//! faults visible first, then make them survivable.
//!
//! `stvec` holds MODE in its low two bits, so the handler address must be four-byte aligned and
//! mode 0 (direct: every cause enters at the same place). Vectored mode exists and buys nothing
//! while there is one handler.
//!
//! **`sscratch` is the kernel-stack latch, and it is what makes user mode possible.** A trap taken
//! from U-mode must not push its frame onto the USER stack: the user chose that pointer, and it may
//! be unmapped, unaligned, or aimed at something the kernel is about to read back. The architectural
//! answer is the one register a trap handler may use before it has a stack - `sscratch` - under a
//! single discipline held everywhere in this file:
//!
//! > **`sscratch` holds this hart's kernel stack pointer while U-mode runs, and ZERO while the
//! > kernel runs.**
//!
//! One `csrrw` then swaps and tests in the same instruction, so entry costs a swap and a branch and
//! needs no scratch register at all. Zeroing it on the way in matters as much as loading it on the
//! way out: a fault INSIDE the handler must take the kernel path, or it would "swap in" a stack it
//! is already standing on and overwrite the frame it is building.

use core::sync::atomic::{AtomicBool, Ordering};

/// `sstatus.SPP` - the privilege the trap came FROM. 0 is user, 1 is supervisor.
///
/// This bit is written by HARDWARE at every trap and cannot be forged by the code that trapped,
/// which is what makes it evidence rather than a claim: it is how `usermode` proves its stub really
/// ran unprivileged, and how the epilogue below knows whether to re-arm `sscratch`.
pub const SSTATUS_SPP: u64 = 1 << 8;

/// Exception cause: an `ecall` executed in user mode.
pub const CAUSE_ECALL_U: u64 = 8;
/// Exception cause: a load that had no valid translation.
pub const CAUSE_LOAD_PAGE_FAULT: u64 = 13;

/// Register numbers this kernel refers to by name, rather than by the index the ABI happens to use.
///
/// `a0`-`a2` and `a7` are the syscall ABI (arguments and number, result back in `a0`). `s0`-`s4` are
/// callee-saved, which is why the boot selftest's user stub parks its evidence there: they are the
/// registers a `ecall` is guaranteed not to disturb, so a value placed in one before a syscall is
/// still there after it.
pub const REG_SP: usize = 2;
pub const REG_S0: usize = 8;
pub const REG_S1: usize = 9;
pub const REG_A0: usize = 10;
pub const REG_A1: usize = 11;
pub const REG_A2: usize = 12;
pub const REG_A7: usize = 17;
pub const REG_S2: usize = 18;
pub const REG_S3: usize = 19;
pub const REG_S4: usize = 20;

/// Set once a fault has been reported, so a fault INSIDE the reporter cannot recurse forever.
///
/// A trap handler that faults re-enters itself, and on a machine whose console is the thing that
/// faulted this shows up as either a hang or an endless partial line. One flag turns that into a
/// single truncated report, which is still readable.
static REPORTING: AtomicBool = AtomicBool::new(false);

/// Cause codes worth naming. The rest are printed as numbers, because a wrong name is worse than
/// an honest integer.
fn cause_name(code: u64, interrupt: bool) -> &'static str {
    if interrupt {
        return match code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "interrupt",
        };
    }
    match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        4 => "load address misaligned",
        5 => "load access fault",
        6 => "store/AMO address misaligned",
        7 => "store/AMO access fault",
        8 => "ecall from user mode",
        9 => "ecall from supervisor mode",
        12 => "instruction page fault",
        13 => "load page fault",
        15 => "store/AMO page fault",
        _ => "exception",
    }
}

/// Every integer register, as the entry stub laid them out, plus the two CSRs that decide where and
/// how execution resumes.
///
/// Indexed by register number so the assembly is a straight `sd x{i}, 8*i(sp)` and the two halves
/// cannot drift apart. `x[0]` is the hardwired zero register and is never written; it is kept in
/// the array only so the indices mean what they say.
///
/// `sepc` and `sstatus` are FIELDS, not live CSRs, so a handler redirects execution by editing the
/// frame rather than by writing a control register behind the epilogue's back. That is what lets
/// `usermode` turn one `ecall` into a return to the kernel: it sets the resume address, sets `SPP`
/// to supervisor, and points `x[2]` at the kernel stack it saved - three ordinary stores, and the
/// epilogue below does the rest.
#[repr(C)]
pub struct TrapFrame {
    pub x: [u64; 32],
    pub sepc: u64,
    pub sstatus: u64,
}

impl TrapFrame {
    /// True when this trap was taken from user mode, per the hardware-written `SPP`.
    pub fn from_user(&self) -> bool {
        self.sstatus & SSTATUS_SPP == 0
    }
}

/// Rust side of a trap.
///
/// RETURNS for anything it can handle, so the stub restores and `sret`s back to what was
/// interrupted. Diverges only by halting, and only for a fault - which is honest while there is no
/// task to kill instead.
#[unsafe(no_mangle)]
extern "C" fn trap_dispatch(frame: &mut TrapFrame) {
    let (scause, stval): (u64, u64);
    // SAFETY: reading CSRs has no side effects.
    unsafe {
        core::arch::asm!(
            "csrr {0}, scause",
            "csrr {1}, stval",
            out(reg) scause, out(reg) stval,
            options(nomem, nostack)
        );
    }
    let interrupt = scause >> 63 != 0;
    let code = scause & 0x7fff_ffff_ffff_ffff;

    // COUNT IT HERE, before any dispatch decides what it was. The liveness watchdog asks "did this
    // core take interrupts at all", and a counter placed inside one handler answers a narrower
    // question than the one being asked - it would read zero for a core that is taking timer
    // interrupts and losing them on the way to the scheduler, which is one of the two cases the
    // watchdog exists to tell apart.
    if interrupt {
        super::note_irq(code as u32);
    }
    super::note_stage(super::stage::TRAP_ENTRY);

    if interrupt && code == 1 {
        // A SUPERVISOR SOFTWARE INTERRUPT: another hart poked this one. It carries no vector - SBI's
        // `send_ipi` says only "someone poked you" - so the vectors were left in this core's pending
        // mask and are drained here.
        //
        // The pending bit in `sip` is cleared FIRST. Clearing it after would race with a sender who
        // set a new vector in between: the bit would be wiped while the mask still held work, and the
        // hart would not be interrupted again to notice. Clearing first can only cause a spurious
        // wake, which costs a loop and is always safe.
        super::clear_software_interrupt();
        super::note_stage(super::stage::IPI_DRAIN);
        super::drain_ipis();
        super::note_stage(super::stage::TRAP_EXIT);
        return;
    }

    if interrupt && code == 5 {
        // Supervisor timer. Acknowledged by SCHEDULING THE NEXT ONE: there is no "clear" bit for
        // it, and leaving the deadline in the past re-raises the interrupt immediately - a live
        // lock that presents as a machine which boots and then does nothing.
        super::timer_tick(frame);
        super::note_stage(super::stage::TRAP_EXIT);
        return;
    }

    // The boot's deliberate `rdcycle`, and only while the probe is executing it. Armed for one
    // instruction, so an illegal instruction anywhere else still halts loudly.
    if !interrupt && code == 2 && !frame.from_user() && super::claim_rdcycle_probe() {
        // `csrr` has no compressed encoding, so four bytes is exactly the probe's instruction.
        frame.sepc = frame.sepc.wrapping_add(4);
        return;
    }

    // The user-mode selftest, and only while it is running: the `ecall` its stub uses to hand
    // control back, and the one deliberate fault it makes on the way. Both are refused outright
    // once the selftest is over, so neither is a door a real task could later walk through.
    if !interrupt && super::usermode::claim_trap(frame, code, stval) {
        return;
    }

    // Every other `ecall` from user mode is a SYSCALL. This is the line that makes the trap vector a
    // gateway rather than only a reporter: from here a task asks the kernel for something and is
    // answered, instead of the machine stopping to describe what it did.
    // The user-TASK selftest, gated separately and equally narrowly. It is offered before the
    // syscall path for the same reason as above: while it is armed its magic number is its own, and
    // once it is not, the number is an ordinary unknown syscall. This one never returns when it
    // fires - it switches away, the way a blocking syscall does.
    if !interrupt && super::usermode::claim_task_trap(frame, code) {
        return;
    }

    if !interrupt && code == CAUSE_ECALL_U {
        super::syscall::dispatch(frame);
        return;
    }

    // A FAULT. What happens next is the difference between a bug that kills a SERVICE and a bug that
    // kills the MACHINE, and the hardware has already said which one this is: `SPP` records the
    // privilege the trap came from, and it cannot be forged by the code that trapped.
    if !interrupt && frame.from_user() {
        // A user task faulted. It has a name, an owner and a supervisor that will restart it, so the
        // honest response is to kill IT - not to stop the machine on its behalf. This is the property
        // every other port has and this one did not, and it is what `4.4`'s restartability rests on:
        // "a service dies, the system continues" has to be true of a FAULT and not only of a
        // deliberate kill, because a fault is the case nobody planned for.
        report_fault(frame, scause, code, stval, true);
        crate::task::kill_current();
        // `kill_current` marks the task Dead and reschedules, so it does not come back for a corpse.
        // If it somehow does, halting beats returning into a task that no longer exists.
        super::print_str("riscv64: kill_current RETURNED for a dead task - halting rather than resuming it\n");
        super::halt();
    }

    // A KERNEL fault, or an interrupt nothing claimed. There is no task to kill: the thing that
    // faulted IS the thing that would do the killing, so the only honest move is to stop loudly.
    if REPORTING.swap(true, Ordering::Relaxed) {
        // A fault inside the reporter. Stop rather than recurse: on a machine whose console is what
        // faulted, recursion shows up as a hang or an endless partial line.
        super::halt();
    }
    report_fault(frame, scause, code, stval, false);
    super::print_str("riscv64: halted - the KERNEL faulted, so there is nothing left to kill instead\n");
    super::halt();
}

/// Describe a fault: what, where, from which privilege, and what the page table actually says.
///
/// Shared by both outcomes so a killed task and a halted kernel are reported in the same words - a
/// diagnosis should not depend on which of the two happened to occur.
fn report_fault(frame: &TrapFrame, scause: u64, code: u64, stval: u64, from_user: bool) {
    let interrupt = scause >> 63 != 0;
    super::print_str("\nriscv64: TRAP - ");
    super::print_str(cause_name(code, interrupt));
    if from_user {
        // NAME the task, do not just number it. Slots are RECYCLED: a service killed and respawned
        // during a chaos run lands in whatever slot is free, so "slot 1" means one binary at boot and
        // a different one ten seconds later. Reporting only the slot sent an entire ARM diagnosis
        // after the wrong ELF. The name is the stable identity (invariant 11); the slot is only where
        // it happens to be living.
        let slot = crate::task::scheduler::current_task_slot();
        super::print_str(" in USER task '");
        let name = crate::task::scheduler::task_stat(slot).name;
        super::print_str(name);
        super::print_str("' (slot ");
        super::print_dec(slot as u64);
        super::print_str(")");
    }
    super::print_str("\n  scause ");
    super::print_hex(scause);
    super::print_str("  sepc ");
    super::print_hex(frame.sepc);
    super::print_str("  stval ");
    super::print_hex(stval);
    // WHAT THE PAGE TABLE ACTUALLY SAYS about the faulting address. A page fault has three quite
    // different causes that look identical in `scause` - nothing mapped, mapped without the
    // permission the access needed, or mapped without `U` for a user access - and the PTE separates
    // them in one line. Guessing between them costs a boot each time.
    if !interrupt && matches!(code, 12 | 13 | 15) {
        super::print_str("\n  pte ");
        match super::page_tables::entry_for_va(stval) {
            Some(pte) => {
                super::print_hex(pte);
                super::print_str(if pte & super::sv39::PTE_V != 0 { " V" } else { " -" });
                super::print_str(if pte & super::sv39::PTE_R != 0 { "R" } else { "-" });
                super::print_str(if pte & super::sv39::PTE_W != 0 { "W" } else { "-" });
                super::print_str(if pte & super::sv39::PTE_X != 0 { "X" } else { "-" });
                super::print_str(if pte & super::sv39::PTE_U != 0 { "U" } else { "-" });
                super::print_str(if pte & super::sv39::PTE_A != 0 { "A" } else { "-" });
                super::print_str(if pte & super::sv39::PTE_D != 0 { "D" } else { "-" });
                super::print_str("  phys ");
                super::print_hex(super::sv39::pte_phys(pte));
            }
            None => super::print_str("NONE - nothing maps that address in the live table"),
        }
        super::print_str("  satp-root ");
        super::print_hex(super::page_tables::read_page_table_base());
    }
    super::print_str("\n");
    if from_user {
        super::print_str("riscv64: killing it; the kernel continues\n");
    }
}

/// Bytes of stack a trap frame occupies. Deliberately larger than the struct so `sp` stays 16-byte
/// aligned, as the RISC-V ABI requires at a call - and `trap_dispatch` is a call.
const FRAME_BYTES: usize = 288;
const _: () = assert!(core::mem::size_of::<TrapFrame>() <= FRAME_BYTES);
const _: () = assert!(FRAME_BYTES % 16 == 0);

/// Byte offsets of the two CSR fields, so the assembly below and `TrapFrame` cannot drift.
const OFF_SEPC: usize = 32 * 8;
const OFF_SSTATUS: usize = 33 * 8;

/// Trap entry: land on a kernel stack, save everything, dispatch, restore, return.
///
/// **The stack comes first, and it comes from `sscratch`.** `csrrw sp, sscratch, sp` swaps the two
/// in one instruction: `sp` becomes what `sscratch` held (this hart's kernel stack if the trap came
/// from U-mode, zero if it came from the kernel) and `sscratch` becomes the interrupted `sp`. A
/// single `bnez` then separates the two cases without a scratch register - which matters, because at
/// this instant there is no register free to use and no stack to spill one onto.
///
/// SAVES ALL 31 WRITABLE REGISTERS, not just the caller-saved ones. The interrupted code is not a
/// caller - it did not agree to any calling convention with this handler and may be at any
/// instruction - so "the compiler will have spilled what it needed" is not available here. A
/// callee-saved register clobbered by the dispatcher would corrupt code that never called it, at a
/// point arbitrarily far away. From U-mode the same rule is a SECURITY property rather than a
/// correctness one: the kernel must hand back every register exactly as it found it.
///
/// `sepc` and `sstatus` are saved and restored explicitly. `sepc` holds where to resume; `sstatus`
/// holds `SPP`, which decides WHICH PRIVILEGE it resumes in. Both are fields the dispatcher may
/// edit, which is how a trap becomes a control transfer rather than only a return.
#[unsafe(naked)]
unsafe extern "C" fn trap_entry() -> ! {
    core::arch::naked_asm!(
        ".p2align 2",
        // Swap in the kernel stack, if this trap came from user mode.
        "csrrw sp, sscratch, sp",
        "bnez  sp, 1f",
        // Zero came back, so the kernel was already running and `sscratch` held nothing. The
        // interrupted `sp` is the one we just parked there; take it back.
        "csrr  sp, sscratch",
        "1:",
        "addi sp, sp, -{frame}",
        // t0 first, so it can be the scratch for everything below.
        "sd x5, 40(sp)",
        // The interrupted stack pointer is in `sscratch` in BOTH cases, which is why the two paths
        // above converge here rather than each saving their own.
        "csrr x5, sscratch",
        "sd x5, 16(sp)",
        // We are the kernel now. A nested trap must take the kernel path above, so the latch reads
        // zero for as long as kernel code is running. Nothing between the swap and this write can
        // trap: interrupts are off by hardware on entry, and these are register moves.
        "csrw sscratch, zero",
        "sd x1, 8(sp)",
        "sd x3, 24(sp)",
        "sd x4, 32(sp)",
        "sd x6, 48(sp)",
        "sd x7, 56(sp)",
        "sd x8, 64(sp)",
        "sd x9, 72(sp)",
        "sd x10, 80(sp)",
        "sd x11, 88(sp)",
        "sd x12, 96(sp)",
        "sd x13, 104(sp)",
        "sd x14, 112(sp)",
        "sd x15, 120(sp)",
        "sd x16, 128(sp)",
        "sd x17, 136(sp)",
        "sd x18, 144(sp)",
        "sd x19, 152(sp)",
        "sd x20, 160(sp)",
        "sd x21, 168(sp)",
        "sd x22, 176(sp)",
        "sd x23, 184(sp)",
        "sd x24, 192(sp)",
        "sd x25, 200(sp)",
        "sd x26, 208(sp)",
        "sd x27, 216(sp)",
        "sd x28, 224(sp)",
        "sd x29, 232(sp)",
        "sd x30, 240(sp)",
        "sd x31, 248(sp)",
        "csrr x5, sepc",
        "sd x5, {sepc}(sp)",
        "csrr x5, sstatus",
        "sd x5, {sstatus}(sp)",
        // The frame IS the argument: a0 points at what was just saved.
        "mv a0, sp",
        "call {dispatch}",
        // Resume. Both CSRs come back from the frame, so a handler may redirect execution by
        // editing `sepc` and may change the privilege it resumes in by editing `SPP` - which is
        // exactly how the user-mode selftest returns to the kernel from a user `ecall`.
        "ld x5, {sstatus}(sp)",
        "csrw sstatus, x5",
        "ld x5, {sepc}(sp)",
        "csrw sepc, x5",
        // Re-arm the latch if and only if we are going back to user mode: `sp + frame` is where the
        // kernel stack stood on entry, which is where the NEXT user trap must land. Returning to
        // S-mode leaves it zero, per the discipline at the top of this file.
        "ld x5, {sstatus}(sp)",
        "andi x5, x5, {spp}",
        "bnez x5, 2f",
        "addi x5, sp, {frame}",
        "csrw sscratch, x5",
        "2:",
        "ld x1, 8(sp)",
        "ld x3, 24(sp)",
        "ld x4, 32(sp)",
        "ld x6, 48(sp)",
        "ld x7, 56(sp)",
        "ld x8, 64(sp)",
        "ld x9, 72(sp)",
        "ld x10, 80(sp)",
        "ld x11, 88(sp)",
        "ld x12, 96(sp)",
        "ld x13, 104(sp)",
        "ld x14, 112(sp)",
        "ld x15, 120(sp)",
        "ld x16, 128(sp)",
        "ld x17, 136(sp)",
        "ld x18, 144(sp)",
        "ld x19, 152(sp)",
        "ld x20, 160(sp)",
        "ld x21, 168(sp)",
        "ld x22, 176(sp)",
        "ld x23, 184(sp)",
        "ld x24, 192(sp)",
        "ld x25, 200(sp)",
        "ld x26, 208(sp)",
        "ld x27, 216(sp)",
        "ld x28, 224(sp)",
        "ld x29, 232(sp)",
        "ld x30, 240(sp)",
        "ld x31, 248(sp)",
        // t0 second to last: it was the scratch for everything above. Then the stack pointer itself,
        // read out of the frame it is still addressing - which is why it can only go last.
        "ld x5, 40(sp)",
        "ld x2, 16(sp)",
        "sret",
        frame = const FRAME_BYTES,
        sepc = const OFF_SEPC,
        sstatus = const OFF_SSTATUS,
        spp = const SSTATUS_SPP,
        dispatch = sym trap_dispatch,
    )
}

/// Point `stvec` at the entry stub, and establish the `sscratch` discipline.
///
/// Returns false rather than installing a handler the hardware would reinterpret: `stvec` steals
/// the low two bits for MODE, Rust has no stable way to align a function, and a misaligned address
/// would become a different mode with a truncated target - discovered only by a fault, which is the
/// thing this exists to catch. Checked rather than assumed for that reason.
///
/// **`sscratch` is zeroed here, and that is not tidiness.** Its reset value is not architecturally
/// specified, and the firmware beneath us runs in M-mode with its own `mscratch`, so nothing has
/// promised to leave this register alone. If it held anything non-zero the FIRST S-mode trap would
/// read it as "a user trap, here is your kernel stack" and build a frame at an address nobody chose.
/// QEMU hands over a zeroed register and would never show this; the emulator supplying the value you
/// assumed is how the whole class of bug hides.
pub fn init() -> bool {
    let addr = trap_entry as *const () as usize;
    if addr & 0x3 != 0 {
        return false;
    }
    // SAFETY: a real code address in the kernel image, proven above to have its low two bits clear,
    // so MODE is 0 (direct) and the address is not truncated. Zeroing `sscratch` establishes the
    // invariant the entry stub relies on, and is done before the vector so no trap can observe the
    // register between the two writes.
    unsafe {
        core::arch::asm!(
            "csrw sscratch, zero",
            "csrw stvec, {}",
            in(reg) addr,
            options(nostack)
        );
    }
    true
}

/// Enable supervisor timer interrupts and take the first one.
///
/// Two enables, and both are needed: `sie.STIE` admits the timer specifically, `sstatus.SIE` admits
/// interrupts at all. Setting one without the other is a machine that either never ticks or ticks
/// for everything.
/// Admit inter-processor interrupts on this hart.
///
/// Separate from the timer because the two are needed at different moments: the boot hart wants the
/// timer long before any other hart exists, and a secondary wants both the instant it starts.
pub fn enable_software_interrupts() {
    // SAFETY: setting `sie.SSIE`. Sound because `stvec` is installed before any hart enables this -
    // an IPI admitted with no vector installed would have nowhere to go.
    unsafe {
        core::arch::asm!("csrs sie, {ssie}", ssie = in(reg) 1u64 << 1, options(nostack));
    }
}

pub fn enable_timer_interrupts() {
    // SAFETY: setting the two enable bits. Sound because `stvec` is already installed - doing this
    // first would mean the first tick had nowhere to go.
    unsafe {
        core::arch::asm!(
            "csrs sie, {stie}",
            "csrs sstatus, {sie}",
            stie = in(reg) 1u64 << 5,   // STIE
            sie = in(reg) 1u64 << 1,    // SIE
            options(nostack)
        );
    }
}
