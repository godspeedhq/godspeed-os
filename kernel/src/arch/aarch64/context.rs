// SPDX-License-Identifier: GPL-2.0-only
//! AArch64 context switch for the Raspberry Pi 4.
//!
//! Saves the **callee-saved** registers only, which is what AAPCS64 requires of a function that
//! behaves like a call. `switch_context` is reached by a `bl`, so the compiler has already spilled
//! anything caller-saved it cared about; duplicating that here would cost cycles on every switch to
//! preserve values nobody is going to read.
//!
//! Per AAPCS64 the callee-saved set is:
//!
//! - **`x19`-`x28`** - general purpose
//! - **`x29`** (frame pointer) and **`x30`** (link register)
//! - **`SP`**
//! - **`d8`-`d15`** - the low 64 bits of `v8`-`v15`
//!
//! **The `d8`-`d15` half is easy to skip and wrong to skip.** The kernel is compiled for a target with
//! FP/SIMD available, and LLVM emits NEON for bulk copies and struct moves without being asked; the
//! Pi 2 port hit exactly that with `memcpy`. Omitting them produces corruption that appears only when
//! a switch lands between a NEON spill and its reload - rare, load-dependent, and close to impossible
//! to attribute later. The eight extra pairs cost far less than that bug.
//!
//! Switching is *not* the same as taking an exception. The IRQ path in `exceptions.rs` saves the
//! **full** register set because it interrupts arbitrary code at an arbitrary instruction; this saves
//! only the callee-saved set because it happens at a call boundary the compiler already knows about.

use core::arch::global_asm;

/// A saved execution context. `#[repr(C)]` and the field order are load-bearing: the assembly below
/// addresses these by byte offset, so reordering the struct silently corrupts the switch.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct TaskContext {
    /// x19-x28, at offsets 0..80.
    pub x: [u64; 10],
    /// x29 (frame pointer), offset 80.
    pub fp: u64,
    /// x30 (link register) - where the switch returns to, offset 88.
    pub lr: u64,
    /// Stack pointer, offset 96.
    pub sp: u64,
    /// Padding, so the `d8`-`d15` pairs below stay 16-byte aligned. Offset 104.
    _pad: u64,
    /// d8-d15, at offsets 112..176.
    pub d: [u64; 8],
    /// The task's page-table base - `TTBR0_EL1` here, not an x86 CR3.
    ///
    /// **The name is a deliberate compromise, not an oversight.** The neutral scheduler reads this
    /// field by name (`scheduler.rs` sets `CORE_SCHED_CTX[cid].cr3`), so renaming it would mean
    /// touching neutral code to accommodate an arch - exactly the direction the seam exists to
    /// prevent. The 32-bit ARM port made the same call. Cleaning it up is a neutral-side rename of
    /// one field across every arch at once, which is worth doing and is not this port's job.
    ///
    /// Offset 176. Not touched by the assembly below, which only moves registers; the address-space
    /// swap is done in `switch_context` before the register switch.
    pub cr3: u64,
}

impl TaskContext {
    pub const fn empty() -> Self {
        TaskContext { x: [0; 10], fp: 0, lr: 0, sp: 0, _pad: 0, d: [0; 8], cr3: 0 }
    }

    /// All-zero context, named so neutral code can build one without naming a register.
    pub const ZERO: Self = Self::empty();

    /// Build a context that begins executing `entry` on `stack_top` in the address space `cr3`.
    ///
    /// The neutral scheduler's constructor. It differs from [`TaskContext::init`] only in taking the
    /// address-space base and an `unsafe extern "C"` entry, which is the signature `task/` uses.
    ///
    /// # Safety
    /// `stack_top` must be the top of a stack this task exclusively owns, and `cr3` must be a live
    /// page-table base (or the current one, for a task sharing the kernel address space).
    pub unsafe fn new_kernel(entry: unsafe extern "C" fn() -> !, stack_top: *mut u8, cr3: u64) -> Self {
        let mut c = Self::empty();
        // Enter through the trampoline, not the entry directly - see `aarch64_task_entry_trampoline`.
        // The real entry rides in x19, which the switch restores just before jumping.
        c.lr = aarch64_task_entry_trampoline as usize as u64;
        c.x[0] = entry as usize as u64; // x[0] IS x19 (offset 0), per the assembly's `stp x19, x20`
        c.sp = (stack_top as u64) & !0xF; // AArch64 faults on a misaligned SP the moment anything pushes
        c.cr3 = cr3;
        c
    }

    /// Build a context that, when the scheduler switches to it, ends up running at **EL0**.
    ///
    /// Switching and exception-returning are different mechanisms: `switch_context` ends in `ret`,
    /// which stays at EL1, while entering EL0 needs an `eret` with `ELR_EL1`, `SPSR_EL1` and `SP_EL0`
    /// set. So the context does not point at the user code at all - it points at a **trampoline** that
    /// performs the `eret`, and carries the user entry and stack in `x19`/`x20`, which the switch
    /// restores on the way in. x86 solves the same mismatch the same way (`ring3_entry_trampoline` and
    /// an `iretq`), differing only in where the values ride.
    ///
    /// `sp` is the task's **kernel** stack, and that is load-bearing beyond the first entry: after the
    /// `eret` it stays in `SP_EL1`, so it is the stack every later trap from this task lands on. A task
    /// whose kernel stack were wrong would run correctly at EL0 and corrupt something the first time it
    /// made a syscall.
    ///
    /// # Safety
    /// Same contract as `new_kernel`, plus: `user_entry` and `user_stack_top` must be mapped in `cr3`
    /// with EL0 access, and `kernel_stack_top` must be a stack this task exclusively owns.
    pub unsafe fn new_user(
        kernel_stack_top: *mut u8,
        user_entry: u64,
        user_stack_top: u64,
        cr3: u64,
    ) -> Self {
        let mut c = Self::empty();
        c.lr = aarch64_user_entry_trampoline as usize as u64;
        c.x[0] = user_entry; // x19
        c.x[1] = user_stack_top; // x20
        c.sp = (kernel_stack_top as u64) & !0xF;
        c.cr3 = cr3;
        c
    }

    /// Prepare a context that, when switched to, begins executing `entry` on `stack_top`.
    ///
    /// `lr` is the whole trick: `switch_context` ends in `ret`, which jumps to whatever `lr` holds. A
    /// fresh context therefore just needs `lr` pointing at the entry function and `sp` at its stack,
    /// and the first switch into it "returns" into a function that was never called.
    ///
    /// `stack_top` is rounded down to a 16-byte boundary because AArch64 faults on a misaligned SP the
    /// moment anything uses it - a failure that surfaces as an unrelated-looking exception inside the
    /// new task rather than at the point the stack was set.
    pub fn init(&mut self, entry: extern "C" fn() -> !, stack_top: u64) {
        *self = TaskContext::empty();
        self.lr = entry as usize as u64;
        self.sp = stack_top & !0xF;
    }
}

global_asm!(
    r#"
    .section .text
    .globl aarch64_switch_context
// aarch64_switch_context(current: *mut TaskContext, next: *const TaskContext)
//   x0 = where to save the outgoing context, x1 = the context to resume.
// Returns into `next`'s lr, so from the caller's point of view this returns only when something
// switches back.
aarch64_switch_context:
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]
    mov  x2, sp
    str  x2,       [x0, #96]
    stp  d8,  d9,  [x0, #112]
    stp  d10, d11, [x0, #128]
    stp  d12, d13, [x0, #144]
    stp  d14, d15, [x0, #160]

    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]
    ldr  x2,       [x1, #96]
    mov  sp, x2
    ldp  d8,  d9,  [x1, #112]
    ldp  d10, d11, [x1, #128]
    ldp  d12, d13, [x1, #144]
    ldp  d14, d15, [x1, #160]
    ret
"#
);

global_asm!(
    r#"
    .section .text
    .globl aarch64_task_entry_trampoline
// First-entry trampoline for a brand-new kernel task.
//   x19 = the real entry point, placed there by TaskContext::new_kernel.
// Unmask IRQs, then branch to the entry. Never returns.
aarch64_task_entry_trampoline:
    msr  daifclr, #2
    br   x19

    .globl aarch64_user_entry_trampoline
// First-entry trampoline for a brand-new EL0 task.
//   x19 = user entry point, x20 = user stack top, placed there by TaskContext::new_user.
// TTBR0 was already installed by switch_context (it swaps on a cr3 change), so the user addresses
// below already translate by the time this runs. SP_EL1 keeps the task's kernel stack, which is what
// every later trap from this task lands on.
aarch64_user_entry_trampoline:
    msr  sp_el0, x20               // the user stack - EL0 has its own SP and it must be set separately
    msr  elr_el1, x19              // where EL0 starts executing
    msr  spsr_el1, xzr             // M[3:0]=0000 (EL0t), DAIF clear so the timer can preempt it
    eret
"#
);

extern "C" {
    fn aarch64_switch_context(current: *mut TaskContext, next: *const TaskContext);
    /// See the assembly above and [`TaskContext::new_kernel`].
    ///
    /// **Why a trampoline rather than entering the task directly.** The scheduler masks IRQs before
    /// its initial `switch_context`, so a task whose `lr` pointed straight at its entry would begin
    /// running with `DAIF.I` still set - and, never yielding, would never have it cleared. The symptom
    /// is exact and misleading: the first task runs correctly and forever, every other task starves,
    /// and the scheduler looks broken when in fact it was never given a tick. x86 solves this the same
    /// way (`task_entry_trampoline` does `sti` then `ret`); this port carries the entry in x19 rather
    /// than on the stack, because `ret` here jumps to `lr` rather than popping.
    fn aarch64_task_entry_trampoline() -> !;
    /// See the assembly above and [`TaskContext::new_user`]. Performs the `eret` that a
    /// callee-saved switch cannot.
    fn aarch64_user_entry_trampoline() -> !;
}

/// Save the running context into `current` and resume `next`.
///
/// # Safety
/// Both pointers must be valid `TaskContext`s that outlive the switch, and `next` must either be a
/// context previously saved by this function or one prepared by [`TaskContext::init`] - anything else
/// resumes into a `ret` with garbage in `lr`. `next` must not alias `current`: switching a context to
/// itself would save over the registers mid-restore.
pub unsafe fn switch(current: *mut TaskContext, next: *const TaskContext) {
    // SAFETY: the caller's contract above.
    unsafe { aarch64_switch_context(current, next) }
}

/// The neutral scheduler's switch: swap the address space if it changes, then swap registers.
///
/// **The address-space half is an obligation, not an optimisation (SEC-26).** The neutral kill path
/// elides a cross-core TLB shootdown for a pinned task on the grounds that "a CR3 reload flushes
/// non-global TLB entries" - an *x86* semantic. Writing `TTBR0_EL1` flushes nothing on AArch64, so
/// this port takes route (a) from `arch/CLAUDE.md`: the context switch itself invalidates on an
/// address-space change. Omitting it would leave a dead task's translations live for the next one,
/// which is the same use-after-free class as SEC-1 and would not show up until something reused the
/// frames.
///
/// **The invalidate is not yet exercised**, and that is stated rather than glossed: every task on this
/// port currently shares the kernel identity map, so `next.cr3` always equals the live `TTBR0_EL1` and
/// the branch is not taken. It must be proven when per-task page tables land - which is precisely the
/// milestone that makes it load-bearing. Recorded per §26.3 rather than assumed correct.
///
/// # Safety
/// As [`switch`], plus: `next.cr3` must be a live page-table base, since it is installed before the
/// register switch and the very next instruction fetch translates through it.
pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {
    // SAFETY: `next` is a valid context per the caller's contract.
    let next_ttbr = unsafe { (*next).cr3 };
    if next_ttbr != 0 {
        let cur: u64;
        // SAFETY: reading TTBR0_EL1 at EL1 is a side-effect-free system-register read.
        unsafe { core::arch::asm!("mrs {}, ttbr0_el1", out(reg) cur, options(nomem, nostack)) };
        if cur != next_ttbr {
            // SAFETY: installing a live page-table base, then invalidating the stale translations it
            // replaces. `dsb ish` before the invalidate orders the TTBR write ahead of it; `isb` after
            // ensures the next instruction fetch uses the new map.
            unsafe {
                core::arch::asm!(
                    "msr ttbr0_el1, {t}",
                    "dsb ish",
                    "tlbi vmalle1",
                    "dsb ish",
                    "isb",
                    t = in(reg) next_ttbr,
                    options(nostack),
                );
            }
        }
    }
    // SAFETY: the caller's contract.
    unsafe { aarch64_switch_context(current, next) }
}
