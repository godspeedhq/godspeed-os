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
//! keep satisfying accesses that no longer exist. The switch issues `sfence.vma` after the write, on
//! EVERY switch into an address space and with no same-root shortcut, so the neutral kill path's
//! x86-shaped assumption ("a page-table reload flushes non-global entries") holds here too.
//! `arch/CLAUDE.md` names this as an obligation a port must meet by construction; it is met here
//! rather than deferred. The shortcut that used to sit here, and why comparing root ADDRESSES is not
//! comparing address SPACES once frames are recycled, is written out at the fence itself.

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
    ///
    /// **Zero INHERITS, and inheriting is only safe while the inherited space cannot be torn down.**
    /// A task entered with zero runs through whatever root the previous task left in `satp`; if that
    /// task later dies, its root frame returns to the allocator and this one is executing through a
    /// page table something else now owns. Nothing detects it. The neutral scheduler never asks for
    /// this - it seeds each core's scheduler context with that core's real root - and the only caller
    /// that does is the selftest below, which runs at boot with no user task in existence and no
    /// address space that can be freed. A future caller owes that same argument or a real root.
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
    /// Exercised by `usermode::task_selftest`, which switches into a user task in an address space
    /// of its own and is answered by an `ecall` that switches back out of the trap handler - the
    /// shape a blocking syscall has. What is still NOT exercised is a task reached through the
    /// neutral scheduler rather than by a direct `switch_context`, which is `spawn_supervisor`.
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
/// The `csrw sscratch, sp` is not a nicety. Without it a trap from this task builds its frame on the
/// USER stack, which is a `U` page, which S-mode may not write while `sstatus.SUM` is clear - so the
/// trap entry's first store faults, re-enters, and faults again. Silent, unrecoverable, before any
/// handler runs. Demonstrated by deleting the line; see `usermode::task_selftest`.
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
        // PUBLISH THIS TASK'S CODE TO THIS HART, one instruction before it runs any of it.
        //
        // A service's text arrived as DATA - the loader wrote it with ordinary stores - and on
        // RISC-V a store is invisible to instruction fetch until `fence.i`, which is HART-LOCAL. So
        // the fence has to happen on the hart that will execute the code, and this trampoline is the
        // only place that is true by construction: it runs once per task, on that hart, immediately
        // before `sret` hands over.
        //
        // It replaces an `sbi_remote_fence_i` broadcast issued from inside the spawn, which was
        // correct and DEADLOCKED. A broadcast waits for every other hart to acknowledge, inside a
        // path that runs with interrupts off - exactly the hazard `task/scheduler.rs` already
        // documents for TLB shootdowns ("if a remote core is mid-syscall with IF=0, e.g. loading an
        // ELF for a concurrent spawn, it cannot ACK the IPI, causing the caller to spin
        // indefinitely"). It survived three supervisor respawns and hung the machine on the fourth,
        // three rounds into a chaos run, with the log stopping between "respawning" and "spawned OK".
        //
        // Local is also strictly CHEAPER: no ecall, no IPI, no wait, and no dependency on the
        // firmware carrying the RFENCE extension. And it is enough, because the only code that can
        // be stale is code freshly written into recycled frames, and no hart can reach that code
        // except through this line. A task resuming after preemption needs nothing - its bytes were
        // published when it first entered.
        "fence",
        "fence.i",
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
        // A root of zero means "no opinion" and leaves translation alone, which is what a kernel task
        // that shares whatever map is live wants. See `new_kernel` for the constraint that puts on
        // such a task.
        "ld t0, {off_cr3}(a1)",
        "beqz t0, 2f",
        // Build the `satp` encoding the field does not carry: PPN in the low 44 bits, MODE 8 (Sv39)
        // in the top four. Done here so no caller has to know the register's shape.
        "srli t2, t0, 12",
        "li   t3, 8",
        "slli t3, t3, 60",   // MODE = 8 (Sv39)
        "or   t2, t2, t3",
        // WRITTEN AND FENCED UNCONDITIONALLY, and the missing branch here is the point.
        //
        // This used to read `satp` first and skip both the write and the fence when the incoming root
        // already matched: "already the live space, nothing to do". That is an ADDRESS-SPACE identity
        // test dressed up as a register comparison, and the two are not the same thing. A root is a
        // physical frame; when a task dies its frames go back to the allocator, and the very next
        // spawn can be handed that same frame as ITS root. `satp` then holds the right number for the
        // wrong address space, the comparison says "no change", and the hart keeps translating
        // through a TLB filled from a page table that has since been overwritten. There is no fault
        // to catch that: the entries are valid, they simply describe a service that no longer exists.
        //
        // The saving it bought was already zero. The neutral scheduler seeds each core's scheduler
        // context with that core's live root (`task/scheduler.rs`, `run`), so a switch is always
        // task -> scheduler -> task and the root always changes; there is no path through the loop on
        // which the branch was taken. So it removed no work while leaving an unsound assumption in
        // the one place - hand-written assembly under a naked function - where it is least likely to
        // be re-examined. It is deleted rather than corrected because there is nothing to correct: a
        // fence on a switch that did not need one is a few hundred cycles, and being wrong here is a
        // page table read after free.
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

// ============================ per-task address space selftest ============================
//
// The switch's OTHER half. Everything above ran with `cr3` zero, so the `satp` write and its fence
// never executed - a whole branch of `switch_context`, written and unrun. This runs it.
//
// Four claims, and the last is the one that matters:
//
//   satp installed     the task read back the root it was given, not the kernel's. Checked FIRST, so
//                      a switch that failed to install it reports rather than faulting on the next
//                      line and taking the machine down with a message about the wrong thing.
//   private page seen  a page mapped ONLY in the task's root is readable from the task. That is the
//                      inherited kernel map and the task's own map both working, in one read.
//   satp restored      switching back put the kernel's root in place, so boot did not continue in a
//                      space that is about to be freed.
//   NOT in the kernel  the same address does not translate in the kernel's root. This is invariant 2
//                      for this ISA: not "a task has a page table" but "a task has a page table the
//                      rest of the system cannot see through".

/// Where the private page lives: above every gigapage the kernel maps, and clear of the two other
/// high addresses this boot uses (192 GiB for the user pages, 128 GiB for the fault probe). Three
/// tests sharing one address would be three tests measuring each other.
///
/// 224 GiB, and the ceiling is the point: **Sv39's positive half ends at 256 GiB**, because an
/// address is 39 bits sign-extended. This started at 256 GiB exactly, which is one past the end -
/// the mapping was written into root index 256, which is a real slot but belongs to the HIGH half,
/// and the read faulted at an address that looked exactly like the one that had been mapped.
/// `sv39::va_is_canonical` now refuses that at the map rather than letting it fault at the use.
const PRIVATE_VA: u64 = 0x0000_0038_0000_0000;
/// Written into the private frame and read back by the task. Arbitrary, but not zero or all-ones:
/// both are what unmapped or uninitialised memory tends to read as.
const PRIVATE_SENTINEL: u64 = 0x5a5a_c3c3_0f0f_1234;

static mut C_CTX: TaskContext = TaskContext::ZERO;
static mut BOOT2_CTX: TaskContext = TaskContext::ZERO;
static C_ROOT: AtomicU64 = AtomicU64::new(0);
static C_SAW_SATP: AtomicU64 = AtomicU64::new(0);
static C_SAW_VALUE: AtomicU64 = AtomicU64::new(0);

/// The task that runs in its own address space.
unsafe extern "C" fn task_c() -> ! {
    let live = super::page_tables::read_page_table_base();
    C_SAW_SATP.store(live, Ordering::Relaxed);

    // Only dereference the private address once the root is confirmed. Reading it under the WRONG
    // root is a page fault, and a fault here would halt the machine while reporting a load fault at
    // an address that means nothing to a reader - burying the actual finding, which is that the
    // switch did not install the table.
    if live == C_ROOT.load(Ordering::Relaxed) {
        // SAFETY: `PRIVATE_VA` is mapped read-write in exactly this address space, which the check
        // above confirms is the live one, and the frame behind it was written before the switch.
        C_SAW_VALUE.store(unsafe { (PRIVATE_VA as *const u64).read_volatile() }, Ordering::Relaxed);
    }

    // SAFETY: `BOOT2_CTX` was filled by the switch that started this task. Its `cr3` is the kernel
    // root, so the switch installs it on the way back - which is the third claim.
    unsafe { switch_context(&raw mut C_CTX, &raw const BOOT2_CTX) };
    super::print_str("riscv64: addrspace TASK C RESUMED AFTER HANDING BACK\n");
    super::halt()
}

/// Prove a task can run in an address space of its own.
pub fn address_space_selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame};
    use super::sv39;
    use super::{print_hex, print_str};

    let kernel_root = super::page_tables::read_page_table_base();
    if kernel_root == 0 {
        print_str("riscv64: addrspace SKIPPED - translation is off\n");
        return;
    }

    let (Some(root), Some(stack), Some(page)) = (sv39::new_root(), alloc_frame(), alloc_frame())
    else {
        print_str("riscv64: addrspace FAIL - no frames for a root, a stack and a page\n");
        return;
    };
    let stack_pa = stack.phys_addr().0;
    let page_pa = page.phys_addr().0;

    // The kernel's own mapping goes in first. Everything after the switch - including any timer
    // interrupt taken while the task runs - executes through this table.
    // SAFETY: `root` is a fresh zeroed root this function owns.
    unsafe { super::page_tables::finalize_service_address_space(root) };

    // SAFETY: the frame is identity-mapped and freshly ours, so writing at its physical address is
    // writing the page the mapping below will point at.
    unsafe { (page_pa as *mut u64).write_volatile(PRIVATE_SENTINEL) };

    // Kernel permissions, no USER bit: this is a task's PRIVATE page, not a userspace one. The point
    // being tested is the address SPACE, and mixing in a privilege question would make a failure
    // ambiguous between the two.
    let bits = sv39::PTE_V | sv39::PTE_R | sv39::PTE_W | sv39::PTE_A | sv39::PTE_D;
    if sv39::map_page(root, PRIVATE_VA, page_pa, bits).is_err() {
        print_str("riscv64: addrspace FAIL - could not map the private page\n");
        return;
    }

    C_ROOT.store(root, Ordering::Relaxed);
    // SAFETY: a fresh stack frame this function owns, a real diverging entry point, and a root that
    // now carries both the kernel's mapping and the task's own.
    unsafe { C_CTX = TaskContext::new_kernel(task_c, (stack_pa + 4096) as *mut u8, root) };
    // The boot context must carry the KERNEL root, or the switch back would leave `satp` pointing at
    // an address space this function is about to free.
    // SAFETY: writing a static this hart alone touches, before the switch reads it.
    unsafe { BOOT2_CTX.cr3 = kernel_root };

    let was = super::interrupts::local_irq_save();
    // SAFETY: `C_CTX` is primed with a real entry, its own stack, and a root that maps the kernel.
    unsafe { switch_context(&raw mut BOOT2_CTX, &raw const C_CTX) };
    super::interrupts::local_irq_restore(was);

    let installed = C_SAW_SATP.load(Ordering::Relaxed) == root;
    let saw = C_SAW_VALUE.load(Ordering::Relaxed) == PRIVATE_SENTINEL;
    let restored = super::page_tables::read_page_table_base() == kernel_root;
    let isolated = sv39::translate(kernel_root, PRIVATE_VA).is_none();


    // Reclaim. The root goes back through the seam's own free path, which is the other half of
    // `finalize_service_address_space` and is equally untested until something calls it.
    // SAFETY: task C is finished and suspended forever, `satp` is back on the kernel root (checked
    // above), and the sentinel frame and stack are this function's own.
    unsafe {
        super::page_tables::reclaim_user_frames(root);
        super::page_tables::free_page_table_root(root);
        free_frame(sv39::frame_of(page_pa));
        free_frame(sv39::frame_of(stack_pa));
    }

    if installed && saw && restored && isolated {
        print_str(
            "riscv64: addrspace PASS - a task ran in its own address space, invisible to the kernel's\n",
        );
    } else {
        print_str("riscv64: addrspace FAIL - see the line above\n");
    }
}
