// SPDX-License-Identifier: GPL-2.0-only
//! The **neutral** context-switch surface for RISC-V - `arch::imp::context_switch`.
//!
//! This is the piece `task/scheduler.rs` actually calls: `TaskContext`, its two constructors, and
//! `switch_context`. Until it exists the neutral scheduler cannot drive a single task on this ISA,
//! whatever else works.
//!
//! **Semantics are mirrored from x86 and ARM, not invented.** The switch is *return-based*: it saves
//! the outgoing callee-saved set plus `sp` and the return address, restores the incoming ones, and
//! ends in `ret`, which jumps to whatever return address the new context carried. A brand-new task is
//! started by priming that return address to a TRAMPOLINE, which enables interrupts (the scheduler
//! switches with them masked) and jumps to the task body. x86 primes a stack and lets `ret` walk it;
//! ARM sets `lr` and `bx`es to it; RISC-V sets `ra` and `ret`s to it. Same beat, three spellings.
//!
//! **The neutral scheduler reads exactly one field of this struct: `cr3`** - checked against
//! `task/scheduler.rs`, where it is the only field named. Everything else is opaque to it, so the
//! layout here is free to be RISC-V shaped: `ra`, `sp`, and `s0`-`s11`, which is precisely the set the
//! calling convention says a function must preserve. On this arch the address-space handle is the
//! ROOT PHYSICAL ADDRESS of an Sv39 table, matching what `page_tables::read_page_table_base` returns
//! and what `PageTable::cr3_value` hands out, so no caller has to know that `satp` wants it shifted
//! and mode-tagged. Keeping the x86 field NAME is the documented leak (`arch/CLAUDE.md`).
//!
//! **Why only the callee-saved set, when the trap entry saves all 31.** A trap interrupts code that
//! agreed to nothing; a context switch is a CALL, and the compiler has already spilled everything it
//! wanted to keep across one. Saving the caller-saved half here would be saving values the caller
//! itself has already declared dead. That is the whole difference between the two files.
//!
//! **The address-space switch flushes, because RISC-V does not do it for you (SEC-26/27).** Writing
//! `satp` does not implicitly invalidate anything - stale translations from the outgoing space would
//! keep satisfying accesses that no longer exist. The switch issues `sfence.vma` after the write, so
//! the neutral kill path's x86-shaped assumption ("a page-table reload flushes non-global entries")
//! holds here too. `arch/CLAUDE.md` names this as an obligation a port must meet by construction; it
//! is met here rather than deferred.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Saved state for a suspended RISC-V task. RISC-V shaped, with `cr3` kept for the neutral read.
///
/// **Field order is the switch's ABI**: `switch_context` addresses these by byte offset, so the
/// layout here and the offsets in the assembly must move together. `repr(C)` pins it, and the
/// constants below are the single place the offsets are written down.
#[repr(C)]
pub struct TaskContext {
    pub ra: u64,  // 0x00  the resume address the switch `ret`s to
    pub sp: u64,  // 0x08
    pub s0: u64,  // 0x10
    pub s1: u64,  // 0x18
    pub s2: u64,  // 0x20
    pub s3: u64,  // 0x28
    pub s4: u64,  // 0x30
    pub s5: u64,  // 0x38
    pub s6: u64,  // 0x40
    pub s7: u64,  // 0x48
    pub s8: u64,  // 0x50
    pub s9: u64,  // 0x58
    pub s10: u64, // 0x60
    pub s11: u64, // 0x68
    /// Sv39 root PHYSICAL address for this task's address space - not the `satp` encoding. Zero means
    /// "this context expresses no opinion", and the switch leaves translation alone.
    pub cr3: u64, // 0x70
}

/// Bytes of context. Asserted against the struct so the two cannot drift.
const CTX_BYTES: usize = 0x78;
const _: () = assert!(core::mem::size_of::<TaskContext>() == CTX_BYTES);
const OFF_CR3: usize = 0x70;

impl TaskContext {
    /// All-zero context. Neutral code builds zero contexts via this, naming no register.
    pub const ZERO: Self = Self {
        ra: 0, sp: 0, s0: 0, s1: 0, s2: 0, s3: 0, s4: 0, s5: 0, s6: 0, s7: 0, s8: 0, s9: 0,
        s10: 0, s11: 0, cr3: 0,
    };

    /// Build a fresh KERNEL task, so the first `switch_context` into it enters `entry` with
    /// interrupts enabled.
    ///
    /// The switch restores `ra` and `ret`s to it, so `ra` is the trampoline; the trampoline needs the
    /// real entry point, which is handed to it in `s0` - a callee-saved register the switch restores
    /// immediately before the branch. This is the RISC-V analogue of x86 stacking
    /// `[trampoline][entry]` and letting two `ret`s walk them.
    ///
    /// `sp` is rounded DOWN to sixteen bytes because the ABI requires it at a call, and the first
    /// thing the task body does is be called.
    ///
    /// # Safety
    /// `stack_top` must point to writable memory owned by this task, with room beneath it for the
    /// task's frames. `cr3` must be a valid Sv39 root physical address, or zero to keep the current
    /// address space (which is what a kernel task with no private space wants).
    pub unsafe fn new_kernel(
        entry: unsafe extern "C" fn() -> !,
        stack_top: *mut u8,
        cr3: u64,
    ) -> Self {
        Self {
            ra: first_entry_trampoline as *const () as u64,
            sp: (stack_top as u64) & !0xf,
            s0: entry as *const () as u64,
            cr3,
            ..Self::ZERO
        }
    }

    /// Build a context that enters **user mode** on its first `switch_context`.
    ///
    /// The same priming trick one privilege level down: `ra` is a trampoline, and `s0`/`s1` carry the
    /// user entry point and user stack it installs before `sret`ing to U-mode. `sp` is the task's own
    /// KERNEL stack - the stack a later timer interrupt builds its trap frame on, which is what makes
    /// a running user task preemptible, and the value the trampoline latches into `sscratch`.
    ///
    /// **UNEXERCISED.** Nothing spawns a user task on this arch yet, so this constructor and its
    /// trampoline have never run. They are written rather than stubbed because a stub in the seam
    /// returns a context that is wrong in a way nothing reports; this is at least wrong in a way the
    /// first spawn will report. Recorded here rather than claimed working (§26.7). The mechanism it
    /// mirrors - `usermode::enter_user` - IS proven on hardware.
    ///
    /// # Safety
    /// `kernel_stack_top` must point to writable memory owned by this task. `user_entry` must be
    /// mapped user-executable and `user_stack_top` user-writable in the address space `cr3` selects,
    /// which the switch installs before the trampoline runs.
    pub unsafe fn new_user(
        kernel_stack_top: *mut u8,
        user_entry: u64,
        user_stack_top: u64,
        cr3: u64,
    ) -> Self {
        Self {
            ra: user_entry_trampoline as *const () as u64,
            sp: (kernel_stack_top as u64) & !0xf,
            s0: user_entry,
            s1: user_stack_top,
            cr3,
            ..Self::ZERO
        }
    }
}

/// First-entry trampoline for a KERNEL task: the `ret` target the switch lands on.
///
/// The scheduler switched in with interrupts masked, so this enables them before handing over - a
/// task that started with them masked could never be preempted, and would hold its core until it
/// yielded. `s0` carries the entry point, restored by the switch two instructions ago.
///
/// `sstatus.SIE` is bit 1, so the immediate form sets exactly it and nothing else.
#[unsafe(naked)]
unsafe extern "C" fn first_entry_trampoline() -> ! {
    core::arch::naked_asm!(
        "csrsi sstatus, 2", // SIE
        "jr s0",
    )
}

/// First-entry trampoline for a USER task, one privilege level below the above.
///
/// `s0` is the user entry, `s1` the user stack, and `sp` this task's kernel stack. The kernel stack
/// is latched into `sscratch` first, because from the `sret` onward a trap must land there rather
/// than on the user's own stack - the discipline `trap.rs` documents. `SPP` cleared is the privilege
/// drop; `SPIE` set is what leaves supervisor interrupts enabled once the kernel is re-entered.
///
/// UNEXERCISED, for the reason `new_user` gives.
#[unsafe(naked)]
unsafe extern "C" fn user_entry_trampoline() -> ! {
    core::arch::naked_asm!(
        "csrw sscratch, sp",
        "csrw sepc, s0",
        "li   t0, {spp}",
        "csrc sstatus, t0",
        "li   t0, {spie}",
        "csrs sstatus, t0",
        "mv   sp, s1",
        "sret",
        spp = const 1u64 << 8,
        spie = const 1u64 << 5,
    )
}

/// Switch from `current` to `next`.
///
/// Saves the callee-saved set into `*current`, installs `next`'s address space if it differs from
/// the live one, restores `next`'s set, and returns - into `next`'s resume address rather than the
/// caller's.
///
/// **Does NOT save or restore the interrupt-enable state**, deliberately, exactly as the x86 and ARM
/// switches do not. Whether interrupts are on across a switch is the scheduler's business (it masks
/// around the switch and the trampolines re-enable), and a switch that silently restored a stale
/// `SIE` would fight it.
///
/// # Safety
/// Both pointers must be valid `TaskContext`s, and `next` must describe a resumable task: a real
/// return address, a stack that belongs to it, and either a valid Sv39 root or zero. Returning here
/// means some other context switched back.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {
    core::arch::naked_asm!(
        // ---- save the outgoing context into *a0 ----
        "sd ra, 0x00(a0)",
        "sd sp, 0x08(a0)",
        "sd s0, 0x10(a0)",
        "sd s1, 0x18(a0)",
        "sd s2, 0x20(a0)",
        "sd s3, 0x28(a0)",
        "sd s4, 0x30(a0)",
        "sd s5, 0x38(a0)",
        "sd s6, 0x40(a0)",
        "sd s7, 0x48(a0)",
        "sd s8, 0x50(a0)",
        "sd s9, 0x58(a0)",
        "sd s10, 0x60(a0)",
        "sd s11, 0x68(a0)",
        // ---- address space ----
        // Compared against the LIVE `satp` rather than against the outgoing context's field, because
        // the outgoing context may never have been filled in (the very first switch of a core comes
        // from a zeroed scheduler context). A root of zero means "no opinion" and leaves it alone,
        // which is what a kernel task that shares the kernel's map wants.
        "ld t0, {off_cr3}(a1)",
        "beqz t0, 2f",
        "csrr t1, satp",
        // Build the `satp` encoding the field does not carry: PPN in the low 44 bits, MODE 8 (Sv39)
        // in the top four. Done here so no caller has to know the register's shape.
        "srli t2, t0, 12",
        "li   t3, 8",
        "slli t3, t3, 60",   // MODE = 8 (Sv39)
        "or   t2, t2, t3",
        "beq  t1, t2, 2f",   // already the live space: no write, no fence
        "csrw satp, t2",
        // The fence is not optional and not a tidy-up: `satp` takes effect immediately, but stale
        // translations from the outgoing space would keep satisfying accesses that no longer exist.
        "sfence.vma",
        "2:",
        // ---- restore the incoming context from *a1 ----
        "ld ra, 0x00(a1)",
        "ld sp, 0x08(a1)",
        "ld s0, 0x10(a1)",
        "ld s1, 0x18(a1)",
        "ld s2, 0x20(a1)",
        "ld s3, 0x28(a1)",
        "ld s4, 0x30(a1)",
        "ld s5, 0x38(a1)",
        "ld s6, 0x40(a1)",
        "ld s7, 0x48(a1)",
        "ld s8, 0x50(a1)",
        "ld s9, 0x58(a1)",
        "ld s10, 0x60(a1)",
        "ld s11, 0x68(a1)",
        "ret",
        off_cr3 = const OFF_CR3,
    )
}

// ============================ selftest ============================
//
// Two kernel tasks on their own stacks, ping-ponging. What that proves, and what each piece is for:
//
//   both entered          the trampoline's `jr s0` reached a Rust function body on a stack this file
//                         allocated - so `ra`, `sp` and `s0` all survived the first switch.
//   interrupts on         the trampoline enabled them. A task entered with them masked would hold
//                         its core forever, which is a bug that presents as a hang much later.
//   counts               each task resumed exactly as many times as it was switched into, so the
//                         switch is re-entrant and neither task lost its place.
//   callee-saved         the load-bearing one. See `switch_preserves_callee_saved`.

const ROUNDS: u64 = 8;

static mut BOOT_CTX: TaskContext = TaskContext::ZERO;
static mut A_CTX: TaskContext = TaskContext::ZERO;
static mut B_CTX: TaskContext = TaskContext::ZERO;

static A_COUNT: AtomicU64 = AtomicU64::new(0);
static B_COUNT: AtomicU64 = AtomicU64::new(0);
static A_IRQ_ON: AtomicBool = AtomicBool::new(false);
static SAVED_REGS_BAD: AtomicU64 = AtomicU64::new(u64::MAX);

/// Is `sstatus.SIE` set right now?
fn interrupts_enabled() -> bool {
    let sstatus: u64;
    // SAFETY: reading a CSR has no side effects.
    unsafe { core::arch::asm!("csrr {}, sstatus", out(reg) sstatus, options(nomem, nostack)) };
    sstatus & 2 != 0
}

/// Switch away and back, and report whether the callee-saved registers came back unchanged.
///
/// Returns zero when every sentinel survived. Written as a naked function rather than an `asm!`
/// block inside Rust because the sentinels have to sit in registers the COMPILER also wants: asking
/// rustc to hold values in `s2`/`s6`/`s11` across a call it cannot see is exactly the situation
/// `naked` exists for, and doing it by hand means the test is measuring the switch rather than
/// measuring what the optimiser decided.
///
/// Three registers, not twelve, and chosen at the two ends and the middle of the saved range: an
/// offset error in the assembly shifts a whole run of registers, so a wrong offset anywhere between
/// `s2` and `s11` disturbs at least one of these. Twelve would cost thirty more instructions to
/// narrow a fault this file can already localise by reading it.
///
/// # Safety
/// Same contract as `switch_context`, which it calls.
#[unsafe(naked)]
unsafe extern "C" fn switch_preserves_callee_saved(
    current: *mut TaskContext,
    next: *const TaskContext,
) -> u64 {
    core::arch::naked_asm!(
        "addi sp, sp, -32",
        "sd ra, 0(sp)",
        "sd s2, 8(sp)",
        "sd s6, 16(sp)",
        "sd s11, 24(sp)",
        "li s2, 0x1111",
        "li s6, 0x6666",
        "li s11, 0x7bbb",
        "call {sw}",
        "li  t0, 0x1111",
        "xor t1, s2, t0",
        "li  t0, 0x6666",
        "xor t2, s6, t0",
        "or  t1, t1, t2",
        "li  t0, 0x7bbb",
        "xor t2, s11, t0",
        "or  a0, t1, t2",
        "ld ra, 0(sp)",
        "ld s2, 8(sp)",
        "ld s6, 16(sp)",
        "ld s11, 24(sp)",
        "addi sp, sp, 32",
        "ret",
        sw = sym switch_context,
    )
}

/// Task A: ping-pong with B, then check its own registers survived, then return to boot.
unsafe extern "C" fn task_a() -> ! {
    A_IRQ_ON.store(interrupts_enabled(), Ordering::Relaxed);
    for _ in 0..ROUNDS {
        A_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: both contexts are this module's statics; B is primed and resumable.
        unsafe { switch_context(&raw mut A_CTX, &raw const B_CTX) };
    }
    // SAFETY: as above. Also switches away and back, which is the point.
    let bad = unsafe { switch_preserves_callee_saved(&raw mut A_CTX, &raw const B_CTX) };
    SAVED_REGS_BAD.store(bad, Ordering::Relaxed);

    // Hand the core back to the boot context. This does not return.
    // SAFETY: `BOOT_CTX` was filled by the switch that started this task.
    unsafe { switch_context(&raw mut A_CTX, &raw const BOOT_CTX) };
    // Reached only if the switch returned, which would mean something resumed a task that asked to
    // be finished with. Say so rather than run off the end of a diverging function.
    super::print_str("riscv64: ctxsw TASK A RESUMED AFTER HANDING BACK\n");
    super::halt()
}

/// Task B: resume A forever. Left suspended when A stops switching to it, which is why its stack is
/// not reclaimed until the selftest is over.
unsafe extern "C" fn task_b() -> ! {
    loop {
        B_COUNT.fetch_add(1, Ordering::Relaxed);
        // SAFETY: both contexts are this module's statics; A is the task that switched here.
        unsafe { switch_context(&raw mut B_CTX, &raw const A_CTX) };
    }
}

/// Prove the context switch: two kernel tasks, their own stacks, and a round trip back to boot.
pub fn selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame};
    use super::{print_dec, print_str};

    let (Some(stack_a), Some(stack_b)) = (alloc_frame(), alloc_frame()) else {
        print_str("riscv64: ctxsw FAIL - no frames for the task stacks\n");
        return;
    };
    let (pa_a, pa_b) = (stack_a.phys_addr().0, stack_b.phys_addr().0);
    let (top_a, top_b) = ((pa_a + 4096) as *mut u8, (pa_b + 4096) as *mut u8);

    // `cr3` zero: both tasks share the kernel's one address space, so the switch has no page table to
    // install. That is the honest state of this port - per-task spaces arrive with spawn - and it
    // also means this test measures the REGISTER half of the switch without the MMU half in the way.
    // SAFETY: the stacks are fresh frames this function owns, identity-mapped and writable, and both
    // entry points are real diverging functions in this module.
    unsafe {
        A_CTX = TaskContext::new_kernel(task_a, top_a, 0);
        B_CTX = TaskContext::new_kernel(task_b, top_b, 0);
    }

    // Mask interrupts across the first switch, exactly as the scheduler does, so the trampoline's job
    // of re-enabling them is the thing under test rather than an accident of what was already set.
    let was = super::interrupts::local_irq_save();
    // SAFETY: `A_CTX` was just primed with a real entry, its own stack, and no address space.
    unsafe { switch_context(&raw mut BOOT_CTX, &raw const A_CTX) };
    super::interrupts::local_irq_restore(was);

    let a = A_COUNT.load(Ordering::Relaxed);
    let b = B_COUNT.load(Ordering::Relaxed);
    let bad = SAVED_REGS_BAD.load(Ordering::Relaxed);
    // B is resumed once per round plus once by the register check, and each resume switches back.
    let counts_ok = a == ROUNDS && b == ROUNDS + 1;
    let regs_ok = bad == 0;
    let irq_ok = A_IRQ_ON.load(Ordering::Relaxed);

    print_str("riscv64: ctxsw a=");
    print_dec(a);
    print_str(" b=");
    print_dec(b);
    print_str(if counts_ok { " ok" } else { " BAD" });
    print_str(" irq-enabled-on-entry=");
    print_str(if irq_ok { "ok" } else { "BAD" });
    print_str(" callee-saved=");
    print_str(if regs_ok { "ok" } else { "BAD" });
    print_str("\n");

    // SAFETY: both tasks are finished with their stacks - A returned here, and B is suspended and
    // will never be resumed because nothing holds its context any more.
    unsafe {
        free_frame(super::sv39::frame_of(pa_a));
        free_frame(super::sv39::frame_of(pa_b));
    }

    if counts_ok && regs_ok && irq_ok {
        print_str("riscv64: ctxsw PASS - two kernel tasks, own stacks, registers intact\n");
    } else {
        print_str("riscv64: ctxsw FAIL - see the line above\n");
    }
}
