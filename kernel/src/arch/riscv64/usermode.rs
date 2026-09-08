// SPDX-License-Identifier: GPL-2.0-only
//! RISC-V user mode (U-mode) - the increment where code first runs UNPRIVILEGED on this ISA.
//!
//! Everything on this port so far has run in S-mode, which is kernel-privileged. This is the pivot:
//! leave S-mode, run instructions that cannot reach kernel memory, and come back in through the
//! trap vector. It is the same increment the ARM32 port made at `arm/usermode.rs`, deliberately in
//! the same shape - map user pages, enter unprivileged, prove it with the hardware's own record of
//! the caller's privilege, and resume the kernel through a magic syscall. What differs is only the
//! ISA's spelling, which is the whole point (§26.14: borrow the mechanism, never the model).
//!
//! **Entering U-mode is a trap return you never trapped from.** There is no "drop privilege"
//! instruction. You set `sepc` to where user code should start, clear `sstatus.SPP` to say the
//! trap you are "returning from" came from user mode, point `sp` at a user stack, and `sret`. The
//! CPU restores the privilege `SPP` names and jumps to `sepc`, atomically. This is the RISC-V
//! analogue of x86's IRETQ-to-ring-3 and ARM's `movs pc, lr`.
//!
//! **The proof it is real is `sstatus.SPP` at the `ecall`.** Running the stub proves nothing by
//! itself - a kernel that failed to drop privilege would run the same instructions and print the
//! same "pass". But hardware writes `SPP` on every trap with the privilege the trap came FROM, and
//! nothing in U-mode can forge it. `SPP == 0` at the stub's `ecall` is unforgeable evidence the
//! code executed unprivileged, exactly as ARM reads `SPSR.mode == USR`.
//!
//! **Isolation is proved by a fault, because RISC-V has nothing else to prove it with.** ARM could
//! ask its MMU a hypothetical question - `ATS1CPUR` runs an unprivileged translation and reports
//! the answer in a register without faulting. RISC-V has no such instruction: the only way to learn
//! whether U-mode may read a kernel page is to have U-mode try. So the stub deliberately loads from
//! a kernel address, and the kernel expects the fault, checks it is the exact address it handed
//! over, records the denial and steps over the instruction. A caught fault is weaker-looking than a
//! probe and is in fact the stronger evidence: it is the real access, refused by the real MMU.
//!
//! **The syscall path is exercised from HERE, from real user mode.** The ARM32 port proved its `svc`
//! entry by issuing the instruction from kernel mode, because user mode did not exist yet when that
//! increment landed. It does here, so the stub issues three `ecall`s of its own and the evidence is
//! about the actual privilege transition rather than a rehearsal of it (`syscall.rs`).
//!
//! **Getting back out** without a scheduler: `enter_user` saves the kernel's callee-saved registers
//! and `sp` before it drops privilege, and the magic `ecall` is answered by EDITING THE TRAP FRAME -
//! resume address into `sepc`, `SPP` set to supervisor, saved stack into `x[2]`. The ordinary trap
//! epilogue then does the transfer, so there is no second hand-written return path to keep correct.
//! That frame-editing IS what a context switch will be; doing it here first means the mechanism is
//! exercised before anything depends on it.
//!
//! Everything here is torn down when it finishes: the pages are unmapped, the frames returned, and
//! the magic syscall disarmed. A U-mode-readable page left in the kernel's address space is a hole,
//! and a magic syscall left live is a door.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::sv39;
use super::syscall::{echo, ECHO_ARGS_1, ECHO_ARGS_2};
use super::trap::{
    TrapFrame, CAUSE_ECALL_U, CAUSE_LOAD_PAGE_FAULT, REG_A2, REG_A7, REG_S0, REG_S1, REG_S2, REG_S3,
    REG_S4,
    REG_SP, SSTATUS_SPP,
};

/// `sstatus.SPIE` - the interrupt-enable to restore on `sret`. Set before entering U-mode so that
/// when the kernel is eventually re-entered, `SIE` comes back on and the scheduler tick survives
/// the excursion.
const SSTATUS_SPIE: u64 = 1 << 5;

/// The two syscall numbers the user stub raises. Far outside any real syscall range so they could
/// never collide with a genuine call, and inert unless `ARMED` is set.
///
/// `MAGIC_RAN` says "here is my evidence" and is answered by RESUMING user mode, which is the half of
/// the trap path a report-and-stop handler never exercises. `MAGIC_STUCK` is the bound: the stub
/// spins after it, waiting to be preempted, and says so loudly rather than spinning forever if no
/// timer interrupt ever arrives. A hang is the one failure this project will not accept quietly.
const MAGIC_RAN: u64 = 0x5555_0001;
const MAGIC_STUCK: u64 = 0x5555_0002;

/// Iterations the stub spins waiting to be preempted, before it gives up and reports.
///
/// A COUNT IS NOT A DURATION, so this is not how long the test waits - it is only the bound past
/// which waiting has clearly failed. Two quanta is 20 ms, which is about 15 million iterations of a
/// two-instruction loop on the board; this is an order of magnitude beyond that, so reaching it means
/// the timer is not arriving from user mode rather than that the machine was slow.
const SPIN_BOUND: u64 = 200_000_000;

/// How many timer interrupts have been taken while the stub was running unprivileged.
///
/// TWO are required to pass, not one. One proves a tick can be taken from user mode at all; the
/// second proves the epilogue put the kernel-stack latch back on the way out, because a second user
/// trap has nowhere to land if it did not. One tick would pass on a kernel that re-armed `sscratch`
/// wrongly and would fail the first time a real task was preempted twice.
static USER_TICKS: AtomicU64 = AtomicU64::new(0);

/// Two spare virtual addresses for the user pages, chosen ABOVE every gigapage the kernel identity
/// maps so the root slot they land in is guaranteed empty.
///
/// The identity map covers zero to the top of RAM: 3 GiB on QEMU `virt`, 9 on the VisionFive (8 GiB
/// of LPDDR4 based at 0x4000_0000). 192 GiB is clear of both, and clear of the 128 GiB address the
/// boot uses to prove the trap vector fires - two unmapped-address tests that collided would test
/// each other. It is canonical for Sv39 (bit 38 clear, so bits 63:39 must be too).
const USER_CODE_VA: u64 = 0x0000_0030_0000_0000;
const USER_STACK_VA: u64 = 0x0000_0030_0000_1000;

/// True ONLY while the selftest is running.
///
/// Without this gate the magic syscall and the expected-fault path would remain reachable forever,
/// and a real task could use either to divert the kernel into stale boot state - a wild control-flow
/// transfer, reachable from U-mode, which is the most valuable thing an unprivileged attacker could
/// find. Armed for the duration of one round trip and cleared immediately after; outside that
/// window the magic number is an ordinary unknown syscall and a fault is an ordinary fault. The
/// ARM32 port learned this the same way (kernel-audit Audit 5, HIGH).
static ARMED: AtomicBool = AtomicBool::new(false);

/// The kernel stack pointer to resume on, saved by `enter_user` before it drops privilege.
static RESUME_SP: AtomicU64 = AtomicU64::new(0);

/// The kernel address the stub is told to poke, so the expected-fault path can insist on the EXACT
/// address rather than forgiving any fault that happens to arrive while armed.
static PROBE_VA: AtomicU64 = AtomicU64::new(0);

/// Evidence gathered during the round trip. Each is a distinct claim, so each is reported by name
/// rather than collapsed into one boolean that cannot say which half failed.
static RAN_UNPRIVILEGED: AtomicBool = AtomicBool::new(false);
static STORE_ROUNDTRIP: AtomicBool = AtomicBool::new(false);
static KERNEL_DENIED: AtomicBool = AtomicBool::new(false);
static SYSCALLS_OK: AtomicBool = AtomicBool::new(false);

// The user stub itself: position-independent instructions, assembled into rodata and COPIED into a
// user page rather than executed where they sit. `.option norvc` forbids compressed instructions so
// every one of them is exactly four bytes - which is what lets the kernel step over the deliberate
// fault by adding 4 to `sepc` and know it landed on an instruction boundary.
//
//   mv s4, a0       keep the probe address: a0 is about to be a syscall argument register.
//   sd a0, -8(sp)   a user page mapped writable must accept a store. `-8` rather than `0` because
//                   the stack pointer is handed in at the END of the page, and one byte past it is
//                   a different page that is not mapped.
//   ld s3, -8(sp)   ... and give it back, parked in a callee-saved register so a syscall cannot
//                   disturb it before the kernel reads it.
//   ld a1, 0(s4)    s4 holds a KERNEL address. This MUST fault; the kernel steps over it.
//   <3 x ecall>     the syscall path, exercised from REAL user mode rather than from the kernel:
//                   two echoes with different arguments (all three must survive the transition, and
//                   the second proves the path is re-entrant) and one number the kernel does not
//                   know, whose -1 proves the NEUTRAL dispatcher was entered and returned. Results
//                   land in s0, s1, s2 - callee-saved, so each survives the calls after it.
//   li a7, MAGIC_RAN
//   ecall           hand the evidence over. The kernel RESUMES us at the next instruction.
//   <spin>          wait to be preempted. This is the point of the second half: a timer interrupt
//                   taken from U-mode is the path every future scheduler quantum takes, and it is
//                   the path where a mishandled `sscratch` corrupts a stack rather than faulting.
//   li a7, MAGIC_STUCK
//   ecall           the bound ran out with no preemption. Report it rather than spin forever.
core::arch::global_asm!(
    ".section .rodata.rv_user_stub,\"a\"",
    ".option push",
    ".option norvc",
    ".balign 4",
    ".globl __rv_user_stub",
    "__rv_user_stub:",
    "mv   s4, a0",
    "sd   a0, -8(sp)",
    "ld   s3, -8(sp)",
    "ld   a1, 0(s4)",
    // Syscall entry, from user mode. Distinct argument sets so a dropped or shifted argument shows
    // up as a wrong digit in the position that names which one.
    "li   a7, {echo}",
    "li   a0, {one_a0}",
    "li   a1, {one_a1}",
    "li   a2, {one_a2}",
    "ecall",
    "mv   s0, a0",
    "li   a7, {echo}",
    "li   a0, {two_a0}",
    "li   a1, {two_a1}",
    "li   a2, {two_a2}",
    "ecall",
    "mv   s1, a0",
    // A number the kernel does not implement, so this one goes all the way to the NEUTRAL dispatcher
    // and comes back with its rejection rather than being answered here.
    "li   a7, {unknown}",
    "ecall",
    "mv   s2, a0",
    "li   a7, {ran}",
    "ecall",
    "li   t0, {spin}",
    "98:",
    "addi t0, t0, -1",
    "bnez t0, 98b",
    "li   a7, {stuck}",
    "ecall",
    "99:",
    "j    99b",
    ".globl __rv_user_stub_end",
    "__rv_user_stub_end:",
    ".option pop",
    ".previous",
    ran = const MAGIC_RAN,
    stuck = const MAGIC_STUCK,
    spin = const SPIN_BOUND,
    echo = const super::syscall::ECHO_NUMBER,
    unknown = const super::syscall::UNKNOWN_NUMBER,
    one_a0 = const super::syscall::ECHO_ARGS_1[0],
    one_a1 = const super::syscall::ECHO_ARGS_1[1],
    one_a2 = const super::syscall::ECHO_ARGS_1[2],
    two_a0 = const super::syscall::ECHO_ARGS_2[0],
    two_a1 = const super::syscall::ECHO_ARGS_2[1],
    two_a2 = const super::syscall::ECHO_ARGS_2[2],
);

unsafe extern "C" {
    static __rv_user_stub: u8;
    static __rv_user_stub_end: u8;
    /// Where the kernel resumes after the magic `ecall`. Defined inside `enter_user`, below.
    fn __rv_usermode_resume();
}

/// Save the kernel's context, then drop to U-mode at `entry` with stack `user_sp` and `probe_va` in
/// `a0`.
///
/// Hand-written and naked because it must not touch the stack between recording `sp` and the
/// privilege drop, and because the resume label has to be a real address the trap handler can put
/// into `sepc`. `s0`-`s11` and `ra` are saved here rather than left to the compiler: the return path
/// comes back through the trap epilogue, which restores every register from the USER's frame, so
/// anything this function still needs afterwards has to be on the stack rather than in a register.
///
/// # Safety
/// `entry` and `user_sp` must be mapped user-accessible in the ACTIVE address space, and the trap
/// vector must be installed - a U-mode fault with no `stvec` is a machine that stops without a word.
#[unsafe(naked)]
unsafe extern "C" fn enter_user(entry: u64, user_sp: u64, probe_va: u64) {
    core::arch::naked_asm!(
        "addi sp, sp, -112",
        "sd ra, 0(sp)",
        "sd s0, 8(sp)",
        "sd s1, 16(sp)",
        "sd s2, 24(sp)",
        "sd s3, 32(sp)",
        "sd s4, 40(sp)",
        "sd s5, 48(sp)",
        "sd s6, 56(sp)",
        "sd s7, 64(sp)",
        "sd s8, 72(sp)",
        "sd s9, 80(sp)",
        "sd s10, 88(sp)",
        "sd s11, 96(sp)",
        // Where the magic ecall must put the stack back.
        "la t0, {resume_sp}",
        "sd sp, 0(t0)",
        // The kernel-stack latch: from here until the kernel is re-entered, a trap from U-mode
        // lands on THIS stack rather than on whatever the user left in `sp`.
        "csrw sscratch, sp",
        // Where to start, and at which privilege. Clearing SPP is the whole privilege drop; setting
        // SPIE is what re-enables supervisor interrupts on the eventual way back in.
        "csrw sepc, a0",
        "li t0, {spp}",
        "csrc sstatus, t0",
        "li t0, {spie}",
        "csrs sstatus, t0",
        // The stub's one input, then its stack. `sp` goes last: after this the kernel stack is only
        // reachable through `sscratch`.
        "mv a0, a2",
        "mv sp, a1",
        "sret",
        // Re-entered by the trap epilogue, with `sp` restored from the frame the handler edited.
        ".globl __rv_usermode_resume",
        "__rv_usermode_resume:",
        "ld ra, 0(sp)",
        "ld s0, 8(sp)",
        "ld s1, 16(sp)",
        "ld s2, 24(sp)",
        "ld s3, 32(sp)",
        "ld s4, 40(sp)",
        "ld s5, 48(sp)",
        "ld s6, 56(sp)",
        "ld s7, 64(sp)",
        "ld s8, 72(sp)",
        "ld s9, 80(sp)",
        "ld s10, 88(sp)",
        "ld s11, 96(sp)",
        "addi sp, sp, 112",
        "ret",
        resume_sp = sym RESUME_SP,
        spp = const SSTATUS_SPP,
        spie = const SSTATUS_SPIE,
    )
}

/// Whether the boot selftest is running.
///
/// ONE flag, asked by everything that needs to know - the magic syscalls here and the echo in
/// `syscall.rs`. Two gates meaning the same thing is two things to get wrong, and the one that gets
/// left armed is a door into the kernel from user mode.
pub(super) fn selftest_armed() -> bool {
    ARMED.load(Ordering::Acquire)
}

/// Hand control back to `enter_user`'s caller by EDITING THE TRAP FRAME.
///
/// Three stores and the ordinary epilogue does the transfer: `sepc` says where to land, `SPP` says
/// at which privilege, and `x[2]` says on which stack. There is no second hand-written return path
/// to keep correct, and the mechanism is the one a context switch will use.
fn resume_kernel(frame: &mut TrapFrame) {
    frame.sepc = __rv_usermode_resume as *const () as u64;
    frame.sstatus |= SSTATUS_SPP;
    frame.x[REG_SP] = RESUME_SP.load(Ordering::Relaxed);
}

/// Offered every timer interrupt, so the selftest can count the ones taken from user mode.
///
/// THIS IS THE PREEMPTION PATH, and counting it here is what makes the test deterministic: the stub
/// does not guess how long a quantum is, it simply waits, and the kernel ends the excursion once it
/// has been preempted twice. A tick that never arrives is caught by the stub's own bound rather than
/// by hanging.
pub(super) fn on_timer_tick(frame: &mut TrapFrame) {
    if !ARMED.load(Ordering::Acquire) || !frame.from_user() {
        return;
    }
    if USER_TICKS.fetch_add(1, Ordering::Relaxed) + 1 >= 2 {
        resume_kernel(frame);
    }
}

/// Offered every exception the dispatcher could not otherwise handle. Returns true when this module
/// has taken responsibility for it and execution may resume.
///
/// THREE CONDITIONS, ALL REQUIRED, and the narrowness is the security argument: the selftest must be
/// running, the trap must have come from user mode, and it must be one of the two things the stub is
/// known to do - its magic `ecall`, or a load of the exact address it was handed. Anything else,
/// including a real fault at a nearby address while armed, falls through to the reporter.
pub(super) fn claim_trap(frame: &mut TrapFrame, code: u64, stval: u64) -> bool {
    if !ARMED.load(Ordering::Acquire) || !frame.from_user() {
        return false;
    }

    if code == CAUSE_ECALL_U && frame.x[REG_A7] == MAGIC_RAN {
        // The stub finished its checks. Record what they proved, then RESUME IT: `SPP` was already
        // checked by `from_user` above, and the stub's `a2` should carry back what its `a0` stored
        // through the user stack. Stepping over the `ecall` and returning is a user-mode resume,
        // which is the direction a handler that only ever reports never takes.
        RAN_UNPRIVILEGED.store(true, Ordering::Relaxed);
        // `s3` is what came back off the user stack, `s4` is what was written to it. Both are
        // callee-saved, so the three syscalls in between could not have disturbed them.
        STORE_ROUNDTRIP.store(frame.x[REG_S3] == frame.x[REG_S4], Ordering::Relaxed);
        // The three syscall results, each a different claim: arguments survived (twice, with
        // different values, so a stuck register cannot pass both), and the NEUTRAL dispatcher was
        // entered and rejected a number it does not know.
        let (one, two) = (ECHO_ARGS_1, ECHO_ARGS_2);
        SYSCALLS_OK.store(
            frame.x[REG_S0] as i64 == echo(one[0], one[1], one[2])
                && frame.x[REG_S1] as i64 == echo(two[0], two[1], two[2])
                && frame.x[REG_S2] as i64 == -1,
            Ordering::Relaxed,
        );
        frame.sepc = frame.sepc.wrapping_add(4);
        return true;
    }

    if code == CAUSE_ECALL_U && frame.x[REG_A7] == MAGIC_STUCK {
        // The stub waited out its whole bound and was never preempted. `USER_TICKS` will say zero or
        // one, so the report names the failure; what matters here is that the machine comes back
        // instead of spinning in user mode forever.
        resume_kernel(frame);
        return true;
    }

    let probe = PROBE_VA.load(Ordering::Relaxed);
    if code == CAUSE_LOAD_PAGE_FAULT && probe != 0 && stval == probe {
        // The MMU refused U-mode a kernel page. That is the result being tested, so it is recorded
        // and the load is stepped over: four bytes exactly, which `.option norvc` guarantees.
        KERNEL_DENIED.store(true, Ordering::Relaxed);
        frame.sepc = frame.sepc.wrapping_add(4);
        return true;
    }

    false
}

/// Map user code and stack, run the stub unprivileged, and report what the hardware said.
///
/// Runs AFTER paging is on and the trap vector is installed, and maps into the ACTIVE address space
/// rather than building a fresh one. A second root would be testing two things at once - whether the
/// kernel survives a `satp` switch, and whether U-mode works - and only the second is this
/// increment. Per-task address spaces arrive with `spawn_supervisor`, which is the change that needs
/// them.
pub fn selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame};
    use super::page_tables::PageFlags;
    use super::{print_hex, print_str};

    let root = super::page_tables::read_page_table_base();
    if root == 0 {
        print_str("riscv64: usermode SKIPPED - paging is not on, so USER permissions mean nothing\n");
        return;
    }

    let (Some(code_frame), Some(stack_frame)) = (alloc_frame(), alloc_frame()) else {
        print_str("riscv64: usermode FAIL - no frames for the user pages\n");
        return;
    };
    let code_pa = code_frame.phys_addr().0;
    let stack_pa = stack_frame.phys_addr().0;

    // Copy the stub into the code frame. Physical addresses are still identity-mapped, so the frame
    // is writable at its own address - which is the ELF loader's job in miniature.
    let stub = (&raw const __rv_user_stub) as usize;
    let stub_end = (&raw const __rv_user_stub_end) as usize;
    let stub_len = stub_end - stub;
    if stub_len == 0 || stub_len > 4096 {
        print_str("riscv64: usermode FAIL - the stub does not fit a page\n");
        return;
    }
    // SAFETY: `code_frame` is a fresh frame this kernel owns and is identity-mapped, so writing at
    // its physical address is writing the page. The source is `stub_len` bytes of rodata bounded by
    // the two symbols the assembler emitted around it, and the regions cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(stub as *const u8, code_pa as *mut u8, stub_len) };
    // Instructions written as DATA are in the data path, not the instruction path. `fence.i` is what
    // makes the store visible to a fetch; without it the hart may execute whatever the page held
    // before, which on a recycled frame is arbitrary and on a fresh one is zeros.
    // SAFETY: `fence.i` is a local ordering instruction with no operands and no memory effects.
    unsafe { core::arch::asm!("fence", "fence.i", options(nostack)) };

    // Code is readable and executable but NOT writable; the stack is writable but NOT executable.
    // A page that is both would prove less than either, because the stub would run whether or not
    // the permission bits meant anything.
    let code_flags = PageFlags::PRESENT | PageFlags::USER;
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    // SAFETY: mapping two fresh frames at addresses above every region the kernel maps, into the
    // active root, on the single hart that exists at boot. `map_page` refuses to overwrite a live
    // leaf, so a wrong address is an error rather than a silent clobber.
    let mapped = unsafe {
        super::page_tables::map_in_active_tables(USER_CODE_VA, code_pa, code_flags.bits()).is_ok()
            && super::page_tables::map_in_active_tables(USER_STACK_VA, stack_pa, stack_flags.bits())
                .is_ok()
    };
    if !mapped {
        print_str("riscv64: usermode FAIL - could not map the user pages\n");
        return;
    }

    // A kernel page for the stub to be refused. The kernel's own first page: mapped by the identity
    // gigapages, and mapped WITHOUT `U`, which is precisely the bit under test.
    let probe = (&raw const super::__kernel_start) as u64;
    PROBE_VA.store(probe, Ordering::Relaxed);

    // Run it. Armed only for the round trip, so the magic syscall and the expected-fault path are
    // inert either side of this line.
    // SAFETY: both pages are mapped user-accessible in the active space and the trap vector is
    // installed, which is what `enter_user` requires. It saves the kernel context before dropping
    // privilege and is returned to through the trap epilogue.
    ARMED.store(true, Ordering::SeqCst);
    unsafe { enter_user(USER_CODE_VA, USER_STACK_VA + 4096, probe) };
    ARMED.store(false, Ordering::SeqCst);

    let ran = RAN_UNPRIVILEGED.load(Ordering::Relaxed);
    let stored = STORE_ROUNDTRIP.load(Ordering::Relaxed);
    let denied = KERNEL_DENIED.load(Ordering::Relaxed);
    let syscalls = SYSCALLS_OK.load(Ordering::Relaxed);
    let ticks = USER_TICKS.load(Ordering::Relaxed);
    let preempted = ticks >= 2;

    print_str("riscv64: usermode ran=");
    print_str(if ran { "ok" } else { "BAD" });
    print_str(" (SPP=0 at the ecall, so the stub was unprivileged) user-write=");
    print_str(if stored { "ok" } else { "BAD" });
    print_str(" kernel-page-denied=");
    print_str(if denied { "ok" } else { "BAD" });
    print_str(" syscall=");
    print_str(if syscalls { "ok" } else { "BAD" });
    print_str(" preempted=");
    super::print_dec(ticks);
    print_str(if preempted { " ok" } else { " BAD" });
    print_str(" probe ");
    print_hex(probe);
    print_str("\n");

    // Take the pages back. A user-readable page left in the kernel's address space is a hole, and
    // this one has no owner now that the stub has finished.
    let unmapped = sv39::unmap_page(root, USER_CODE_VA).is_ok()
        && sv39::unmap_page(root, USER_STACK_VA).is_ok();
    // SAFETY: single hart, and the two pages were just removed from the only live page table.
    unsafe {
        super::page_tables::invalidate_tlb_page(USER_CODE_VA);
        super::page_tables::invalidate_tlb_page(USER_STACK_VA);
    }
    if unmapped {
        // SAFETY: both frames were allocated by this function, are no longer mapped anywhere, and
        // are not referenced by any table after the unmap and fence above.
        unsafe {
            free_frame(sv39::frame_of(code_pa));
            free_frame(sv39::frame_of(stack_pa));
        }
    }

    if ran && stored && denied && syscalls && preempted && unmapped {
        print_str("riscv64: usermode PASS - ran in U-mode, USER pages honoured, kernel refused, syscalls answered, preempted twice\n");
    } else {
        print_str("riscv64: usermode FAIL - see the line above\n");
    }
}

// ============================ user task, in its own address space ============================
//
// THE JOIN. Everything above proved one half at a time: `selftest` runs user mode in the KERNEL's
// address space, and `context_switch::address_space_selftest` runs a task in its own space but in
// S-mode. Neither is what a service is. This is both at once, and it is the shape `spawn_supervisor`
// will have:
//
//   switch_context installs the task's `satp` and lands on `user_entry_trampoline`
//     -> trampoline latches the task's KERNEL stack in `sscratch`, clears SPP, `sret`s
//       -> user code runs, unprivileged, in a space of its own
//         -> `ecall` traps to S-mode, onto the task's kernel stack
//           -> the handler switches back to the scheduler
//
// That last step is deliberately a CONTEXT SWITCH out of the trap handler, not a frame edit. A frame
// edit is what `selftest` does because it has no task to be; a real blocking syscall returns into
// kernel code on the task's own kernel stack and switches away from there, leaving the trap frame in
// place so a later switch back resumes the handler and returns to user mode through the epilogue.
// Doing it that way here means the mechanism is exercised in the shape it will actually be used.
//
// Four claims:
//
//   ran unprivileged   `SPP == 0` at the `ecall`, as before - hardware's record, not the kernel's.
//   own address space  the live `satp` inside the trap handler is the TASK's root, not the kernel's.
//                      Read in the handler rather than in the task, because that also proves a trap
//                      taken from a task's own space lands somewhere the kernel is still mapped.
//   user stack works   a store and load through a `U|W` page mapped only in that root.
//   own kernel stack   the trap frame sits inside the frame this test allocated for the task, which
//                      is the evidence that `user_entry_trampoline` latched `sscratch`.
//
// **What happens without that latch is worth knowing, because it is not a wrong value - it is a dead
// machine.** Removing `csrw sscratch, sp` from the trampoline was expected to give
// `own-kernel-stack=BAD`; it gives NO OUTPUT AT ALL, and the boot stops at this test. The reason is
// a second isolation rule meeting the first: with the latch gone the trap entry builds its frame on
// the USER stack, and that page is mapped `U` - which S-mode may not write while `sstatus.SUM` is
// clear, and it is clear. So the first store of the trap entry faults, which re-enters the trap
// entry, which stores again. An unrecoverable fault loop, silent, before any handler runs.
//
// So this check is not what catches a MISSING latch (nothing in software could - the machine is gone
// before any code observes it). What it catches is a latch pointing at the WRONG stack, which does
// not fault and would otherwise be found much later as one task quietly corrupting another.

/// The magic the user TASK raises. A fourth number, distinct from the three above, so a mistake in
/// one selftest cannot be answered by another's handler.
const MAGIC_TASK: u64 = 0x5555_0004;

/// Sentinel the task stores through its user stack and hands back in `a2`.
const TASK_SENTINEL: u64 = 0x2718_2818_2845_9045;

/// Virtual addresses for the task's user pages. Different from the ones `selftest` uses, because
/// both sets exist at once in different roots and reusing an address would make a mix-up invisible.
/// Both are in Sv39's low half, well under the 256 GiB ceiling (`sv39::va_is_canonical`).
const TASK_CODE_VA: u64 = 0x0000_0028_0000_0000;
const TASK_STACK_VA: u64 = 0x0000_0028_0000_1000;

core::arch::global_asm!(
    ".section .rodata.rv_user_task_stub,\"a\"",
    ".option push",
    ".option norvc",
    ".balign 4",
    ".globl __rv_user_task_stub",
    "__rv_user_task_stub:",
    "li   a1, {sentinel}",
    "sd   a1, -8(sp)",
    "ld   a2, -8(sp)",
    "li   a7, {magic}",
    "ecall",
    "99:",
    "j    99b",
    ".globl __rv_user_task_stub_end",
    "__rv_user_task_stub_end:",
    ".option pop",
    ".previous",
    sentinel = const TASK_SENTINEL,
    magic = const MAGIC_TASK,
);

unsafe extern "C" {
    static __rv_user_task_stub: u8;
    static __rv_user_task_stub_end: u8;
}

static TASK_ARMED: AtomicBool = AtomicBool::new(false);
static TASK_CTX_ROOT: AtomicU64 = AtomicU64::new(0);
static TASK_KSTACK: AtomicU64 = AtomicU64::new(0);
static TASK_SAW_SATP: AtomicU64 = AtomicU64::new(0);
static TASK_SAW_SENTINEL: AtomicU64 = AtomicU64::new(0);
static TASK_FRAME_ADDR: AtomicU64 = AtomicU64::new(0);
static TASK_RAN_USER: AtomicBool = AtomicBool::new(false);

/// The five `uaccess` claims, gathered while the task's address space is the live one - which is the
/// only moment they can be asked, because a user pointer means nothing outside it.
static UA_READ: AtomicBool = AtomicBool::new(false);
static UA_WRITE: AtomicBool = AtomicBool::new(false);
static UA_DENY_KERNEL: AtomicBool = AtomicBool::new(false);
static UA_DENY_UNMAPPED: AtomicBool = AtomicBool::new(false);
static UA_DENY_RO_WRITE: AtomicBool = AtomicBool::new(false);

/// Exercise `uaccess` against a REAL user address space, from inside a trap taken from a real user
/// task. Nothing else can do this honestly: a user pointer is only meaningful while the space that
/// defines it is installed, and `sstatus.SUM` only matters when the page is genuinely marked `U`.
///
/// Five claims, and three of them are DENIALS. A copy routine that works is half a copy routine; the
/// half that matters is the one that refuses.
fn check_uaccess() {
    use super::uaccess::{read_user_bytes, validate_user_ptr, write_user_bytes};

    // The stub left the sentinel at the top of its user stack. Reading it back proves the whole
    // path: range check, page walk, `SUM` window, copy into the per-core scratch.
    let sp_top = TASK_STACK_VA + 4096;
    if let Some(bytes) = read_user_bytes(sp_top - 8, 8) {
        let mut v = [0u8; 8];
        v.copy_from_slice(bytes);
        UA_READ.store(u64::from_le_bytes(v) == TASK_SENTINEL, Ordering::Relaxed);
    }

    // Write into the user stack and read it back through the same machinery.
    const PATTERN: [u8; 8] = [0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04];
    if write_user_bytes(sp_top - 32, &PATTERN) {
        if let Some(back) = read_user_bytes(sp_top - 32, 8) {
            UA_WRITE.store(back == PATTERN, Ordering::Relaxed);
        }
    }

    // A KERNEL address must not become readable just because a syscall argument named it.
    //
    // **And on THIS arch the range check cannot say so.** The kernel is identity-mapped low -
    // 0x8020_0000 on QEMU, 0x4020_0000 on the board - which is squarely inside the user half, so
    // `validate_user_ptr` answers TRUE for it and is right to: that address is a perfectly legal
    // user address, and a task may legitimately have its own page mapped there in its own space.
    // x86's identical check rejects a kernel pointer only because its kernel lives higher-half; the
    // check is a property of that layout, not of the idea. Borrowing it and assuming the rejection
    // came with it would be importing their model (§26.14).
    //
    // What actually separates the two here is the `U` BIT, checked by the walk. So the claim is
    // stated against the thing that protects: the range check passes, and the read is refused
    // anyway. Both halves are asserted, because a future change that made the range check reject
    // this address would silently turn this into a test of nothing.
    let kernel_addr = (&raw const super::__kernel_start) as u64;
    let in_range = validate_user_ptr(kernel_addr, 8);
    let refused = read_user_bytes(kernel_addr, 8).is_none();
    UA_DENY_KERNEL.store(in_range && refused, Ordering::Relaxed);

    // A user address in range but NOT MAPPED. This is what the walk exists for: refused rather than
    // faulted, which is what lets a kernel with no kill path survive a bad pointer at all.
    UA_DENY_UNMAPPED.store(
        read_user_bytes(TASK_CODE_VA + 0x10_0000, 8).is_none(),
        Ordering::Relaxed,
    );

    // The task's CODE page is mapped `U|R|X` and NOT writable. A read must succeed and a write must
    // be refused, so the walk is checking the permission it was asked about rather than presence.
    let ro_read_ok = read_user_bytes(TASK_CODE_VA, 8).is_some();
    let ro_write_refused = !write_user_bytes(TASK_CODE_VA, &PATTERN);
    UA_DENY_RO_WRITE.store(ro_read_ok && ro_write_refused, Ordering::Relaxed);
}

static mut TASK_CTX: super::context_switch::TaskContext = super::context_switch::TaskContext::ZERO;
static mut TASK_BOOT_CTX: super::context_switch::TaskContext =
    super::context_switch::TaskContext::ZERO;

/// Offered every user `ecall` while the task selftest is armed. Never returns when it fires: it
/// switches back to the boot context, abandoning the trap frame on the task's kernel stack exactly
/// as a blocking syscall would leave it for a later resume.
pub(super) fn claim_task_trap(frame: &mut TrapFrame, code: u64) -> bool {
    if !TASK_ARMED.load(Ordering::Acquire)
        || code != CAUSE_ECALL_U
        || frame.x[REG_A7] != MAGIC_TASK
        || !frame.from_user()
    {
        return false;
    }
    TASK_RAN_USER.store(true, Ordering::Relaxed);
    TASK_SAW_SATP.store(super::page_tables::read_page_table_base(), Ordering::Relaxed);
    TASK_SAW_SENTINEL.store(frame.x[REG_A2], Ordering::Relaxed);
    TASK_FRAME_ADDR.store(frame as *const TrapFrame as u64, Ordering::Relaxed);
    check_uaccess();

    // Leave user mode the way a scheduler does. This does not return: nothing switches back into
    // this task, so the trap frame under us is simply never resumed.
    // SAFETY: `TASK_BOOT_CTX` was filled by the switch that started this task, and carries the
    // kernel's root, so the switch restores the kernel address space on the way out.
    unsafe {
        super::context_switch::switch_context(&raw mut TASK_CTX, &raw const TASK_BOOT_CTX)
    };
    true
}

/// Run one user task in an address space of its own.
pub fn task_selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame};
    use super::context_switch::TaskContext;
    use super::page_tables::PageFlags;
    use super::{print_hex, print_str};

    let kernel_root = super::page_tables::read_page_table_base();
    if kernel_root == 0 {
        print_str("riscv64: usertask SKIPPED - paging is not on\n");
        return;
    }

    let (Some(root), Some(code), Some(ustack), Some(kstack)) =
        (sv39::new_root(), alloc_frame(), alloc_frame(), alloc_frame())
    else {
        print_str("riscv64: usertask FAIL - no frames for a root, code, user stack and kernel stack\n");
        return;
    };
    let (code_pa, ustack_pa, kstack_pa) =
        (code.phys_addr().0, ustack.phys_addr().0, kstack.phys_addr().0);

    // The kernel's mapping first, or the switch that installs this root stops the machine.
    // SAFETY: `root` is a fresh zeroed root this function owns.
    unsafe { super::page_tables::finalize_service_address_space(root) };

    let stub = (&raw const __rv_user_task_stub) as usize;
    let stub_len = (&raw const __rv_user_task_stub_end) as usize - stub;
    if stub_len == 0 || stub_len > 4096 {
        print_str("riscv64: usertask FAIL - the stub does not fit a page\n");
        return;
    }
    // SAFETY: `code_pa` is a fresh frame this function owns, identity-mapped, so writing at its
    // physical address is writing the page. The source is bounded by the two symbols the assembler
    // emitted around the stub, and the regions cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(stub as *const u8, code_pa as *mut u8, stub_len) };
    // SAFETY: `fence.i` makes instructions written as data visible to a fetch. No operands, no
    // memory effects. QEMU does not need it and this board does.
    unsafe { core::arch::asm!("fence", "fence.i", options(nostack)) };

    let code_flags = PageFlags::PRESENT | PageFlags::USER;
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    let mapped = sv39::map_page(
        root,
        TASK_CODE_VA,
        code_pa,
        sv39::flags_to_pte_bits(code_flags.bits()),
    )
    .is_ok()
        && sv39::map_page(
            root,
            TASK_STACK_VA,
            ustack_pa,
            sv39::flags_to_pte_bits(stack_flags.bits()),
        )
        .is_ok();
    if !mapped {
        print_str("riscv64: usertask FAIL - could not map the task's user pages\n");
        return;
    }

    TASK_CTX_ROOT.store(root, Ordering::Relaxed);
    TASK_KSTACK.store(kstack_pa, Ordering::Relaxed);
    // SAFETY: a fresh kernel-stack frame this function owns; user entry and stack mapped
    // user-accessible in `root`, which the switch installs before the trampoline runs.
    unsafe {
        TASK_CTX = TaskContext::new_user(
            (kstack_pa + 4096) as *mut u8,
            TASK_CODE_VA,
            TASK_STACK_VA + 4096,
            root,
        );
        TASK_BOOT_CTX.cr3 = kernel_root;
    }

    TASK_ARMED.store(true, Ordering::SeqCst);
    let was = super::interrupts::local_irq_save();
    // SAFETY: `TASK_CTX` is primed by `new_user` with a real user entry, its own user and kernel
    // stacks, and a root that carries the kernel's mapping as well as the task's.
    unsafe {
        super::context_switch::switch_context(&raw mut TASK_BOOT_CTX, &raw const TASK_CTX)
    };
    super::interrupts::local_irq_restore(was);
    TASK_ARMED.store(false, Ordering::SeqCst);

    let ran = TASK_RAN_USER.load(Ordering::Relaxed);
    let own_space = TASK_SAW_SATP.load(Ordering::Relaxed) == root;
    let stack_ok = TASK_SAW_SENTINEL.load(Ordering::Relaxed) == TASK_SENTINEL;
    let frame = TASK_FRAME_ADDR.load(Ordering::Relaxed);
    let kstack_ok = frame >= kstack_pa && frame < kstack_pa + 4096;

    print_str("riscv64: usertask ran=");
    print_str(if ran { "ok" } else { "BAD" });
    print_str(" own-address-space=");
    print_str(if own_space { "ok" } else { "BAD" });
    print_str(" user-stack=");
    print_str(if stack_ok { "ok" } else { "BAD" });
    print_str(" own-kernel-stack=");
    print_str(if kstack_ok { "ok" } else { "BAD" });
    print_str(" root ");
    print_hex(root);
    print_str("\n");

    let ua = [
        ("read", UA_READ.load(Ordering::Relaxed)),
        ("write", UA_WRITE.load(Ordering::Relaxed)),
        ("deny-kernel-ptr", UA_DENY_KERNEL.load(Ordering::Relaxed)),
        ("deny-unmapped", UA_DENY_UNMAPPED.load(Ordering::Relaxed)),
        ("deny-write-to-ro", UA_DENY_RO_WRITE.load(Ordering::Relaxed)),
    ];
    print_str("riscv64: uaccess");
    let mut uaccess_ok = true;
    for (name, ok) in ua {
        print_str(" ");
        print_str(name);
        print_str("=");
        print_str(if ok { "ok" } else { "BAD" });
        uaccess_ok &= ok;
    }
    print_str("\n");

    // Reclaim through the SEAM'S OWN PATH, which is what a dying task uses - so this selftest
    // exercises the reclaim as well as the entry. `reclaim_user_frames` takes the code and
    // user-stack pages (they carry `U`, which is how it tells a task's pages from the kernel's)
    // and the tables built to reach them; the root and the KERNEL stack are separate, because
    // neither is a `U` page in this space. Freeing code and stack by hand as well would be a
    // double free - which is why the explicit frees that were here are gone.
    // SAFETY: the task is abandoned and nothing can resume it - its context is a local static no
    // scheduler knows about, and `satp` is back on the kernel root.
    let reclaimed = unsafe {
        let n = super::page_tables::reclaim_user_frames(root);
        super::page_tables::free_page_table_root(root);
        free_frame(sv39::frame_of(kstack_pa));
        n
    };
    // Two user pages, plus the tables that reached them. Reported rather than assumed: a reclaim
    // that silently answers zero is exactly what let a fault-restart loop leak an address space
    // per cycle until the machine died of `FrameAllocFailed` several hundred restarts later.
    let reclaim_ok = reclaimed >= 2;
    print_str("riscv64: usertask reclaimed ");
    super::print_dec(reclaimed as u64);
    print_str(if reclaim_ok { " frame(s) ok\n" } else { " frame(s) BAD\n" });

    if ran && own_space && stack_ok && kstack_ok && uaccess_ok && reclaim_ok {
        print_str(
            "riscv64: usertask PASS - unprivileged, in its own address space, on its own kernel stack\n",
        );
    } else {
        print_str("riscv64: usertask FAIL - see the line above\n");
    }
}
