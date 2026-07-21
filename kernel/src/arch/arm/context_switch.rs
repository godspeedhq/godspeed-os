// SPDX-License-Identifier: GPL-2.0-only
//! The **neutral** context-switch surface for ARMv7 - `arch::imp::context_switch`.
//!
//! This is the piece the arch-neutral scheduler actually calls: `TaskContext`, its two constructors,
//! and `switch_context`. Everything in `context.rs` was a local demo proving the *mechanism*; this is
//! the mechanism dressed in the interface `task/scheduler.rs` imports, so the neutral scheduler can
//! drive ARM kernel tasks without one arch-specific line.
//!
//! **Semantics are mirrored from x86, not invented.** The x86 switch is *return-based*: it saves the
//! outgoing callee-saved set and `rsp`, restores the incoming ones, and ends in `ret`, which pops the
//! resume address off the new stack. A brand-new task is started by priming its stack so that `ret`
//! lands in a trampoline that re-enables interrupts (the scheduler switches with them masked) and
//! then enters the task body. ARM reproduces this beat for beat with `ldmia`/`bx lr` and a `cpsie i`
//! trampoline.
//!
//! **The neutral scheduler reads exactly one field of this struct: `cr3`.** (Verified against
//! `task/scheduler.rs`.) Everything else is opaque to it, so the ARM layout is free to be
//! ARM-shaped - eight callee-saved registers, `sp`, `lr` - as long as `cr3` survives as the
//! address-space handle. On ARM that handle is **TTBR0**, stored in the low half of the `u64` `cr3`
//! field. Keeping the x86 field *name* is the documented leak (`arch/CLAUDE.md`); renaming it to
//! `page_table_base` is a neutral-scheduler change deferred to when a second arch forces it.
//!
//! **No per-task address spaces yet.** Every kernel task shares the one identity map from `mmu.rs`,
//! so `cr3` is identical across tasks and the switch compare-and-skips the `TTBR0` write - which also
//! sidesteps the SEC-26/27 TLB-maintenance obligation until real per-task page tables arrive.

use super::pl011_write;
use super::timer::write_dec_pub;

/// Saved state for a suspended ARM task. ARM-shaped, with `cr3` (TTBR0) kept for the neutral read.
///
/// **Field order is the switch's ABI**: `switch_context` addresses these by byte offset, so the
/// layout here and the offsets in the asm must move together. `repr(C)` pins it.
#[repr(C)]
pub struct TaskContext {
    pub r4: u32,   // 0x00
    pub r5: u32,   // 0x04
    pub r6: u32,   // 0x08
    pub r7: u32,   // 0x0c
    pub r8: u32,   // 0x10
    pub r9: u32,   // 0x14
    pub r10: u32,  // 0x18
    pub r11: u32,  // 0x1c
    pub sp: u32,   // 0x20
    pub lr: u32,   // 0x24  - the resume address the switch `bx`es to
    /// TTBR0 for this task's address space. `u64` to match the field the neutral scheduler reads;
    /// only the low 32 bits are meaningful on ARMv7 short descriptors.
    pub cr3: u64,  // 0x28
}

impl TaskContext {
    /// All-zero context. Neutral code builds zero contexts via this, naming no register.
    pub const ZERO: Self = Self { r4: 0, r5: 0, r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0, sp: 0, lr: 0, cr3: 0 };

    /// Build a fresh kernel task so the first `switch_context` into it enters `entry` with interrupts
    /// enabled.
    ///
    /// The stack is primed exactly as x86 primes it: the switch restores `lr` and `bx`es to it, so
    /// `lr` is set to the trampoline; the trampoline needs the real entry point, which is handed to it
    /// in `r4` (a saved register the switch restores just before the branch). This is the ARM analogue
    /// of x86 stacking [trampoline][entry] and letting two `ret`s walk them.
    ///
    /// # Safety
    /// `stack_top` must point to writable memory owned by this task. `cr3` must be a valid TTBR0
    /// value (or the shared identity map's, for a kernel task with no private address space).
    pub unsafe fn new_kernel(
        entry: unsafe extern "C" fn() -> !,
        stack_top: *mut u8,
        cr3: u64,
    ) -> Self {
        TaskContext {
            // r4 carries the entry point to the trampoline; the rest start zeroed.
            r4: entry as usize as u32,
            r5: 0, r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0,
            sp: (stack_top as usize & !7) as u32,
            lr: first_entry_trampoline as usize as u32,
            cr3,
        }
    }

    /// Build a context that enters **user mode** (PL0) on its first `switch_context`.
    ///
    /// The same priming trick as `new_kernel`, one privilege level lower. The switch restores `lr` and
    /// `bx`es to it, so `lr` is a trampoline; `r4`/`r5` carry the PL0 entry and the user stack the
    /// trampoline installs before dropping to ring 3 (mirroring `usermode::enter_pl0`). `sp` is the
    /// task's own **kernel** (SVC) stack - the stack a later timer IRQ builds its trap frame on, which
    /// is what makes the running user task preemptible.
    ///
    /// # Safety
    /// `kernel_stack_top` must point to writable memory owned by this task. `user_entry` must be mapped
    /// USER-executable and `user_stack_top` USER-writable in the address space `cr3` (TTBR0) selects,
    /// which the switch installs on entry.
    pub unsafe fn new_user(
        kernel_stack_top: *mut u8,
        user_entry: u64,
        user_stack_top: u64,
        cr3: u64,
    ) -> Self {
        TaskContext {
            r4: user_entry as u32,       // parked: the PL0 entry the trampoline drops to
            r5: user_stack_top as u32,   // parked: the user stack the trampoline installs
            r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0,
            sp: (kernel_stack_top as usize & !7) as u32,
            lr: user_entry_trampoline as usize as u32,
            cr3,
        }
    }
}

/// First-entry trampoline for a **user** task: the `bx lr` target the switch lands on, one privilege
/// level below `first_entry_trampoline`.
///
/// The scheduler switched in with IRQs masked. `switch_context` restored `r4` = the PL0 entry, `r5` =
/// the user stack, and `sp` = this task's kernel stack (where a future preemption's trap frame goes).
/// This installs the user stack in the USR bank and fabricates an exception return to ring 3 with IRQs
/// **enabled** (SPSR = USR), so the task is preemptible the instant it starts - the ARM analogue of
/// `usermode::enter_pl0`, reached through the scheduler rather than directly.
#[unsafe(naked)]
unsafe extern "C" fn user_entry_trampoline() -> ! {
    core::arch::naked_asm!(
        "cps  #0x1f",   // system mode shares the USR banked SP
        "mov  sp, r5",  // install the user stack
        "cps  #0x13",   // back to SVC (its sp = this task's kernel stack, untouched)
        "mov  r3, #0x10", // SPSR = USR mode, IRQs enabled (F left masked) - preemptible in ring 3
        "msr  spsr_cxsf, r3",
        "mov  lr, r4",  // LR = the service entry
        "movs pc, lr",  // drop to PL0, restoring CPSR (IRQs on) from SPSR
    )
}

/// First-entry trampoline: the `bx lr` target for a never-run task.
///
/// The scheduler switches with IRQs masked, so a fresh task would otherwise start unable to be
/// preempted. This re-enables them (x86's trampoline does the equivalent `sti`) and branches to the
/// entry point that `new_kernel` parked in `r4`.
#[unsafe(naked)]
unsafe extern "C" fn first_entry_trampoline() -> ! {
    core::arch::naked_asm!(
        "cpsie i",   // undo the scheduler's mask - a fresh task must be preemptible
        "bx   r4",   // enter the task body (parked in r4 by new_kernel)
    )
}

/// Save the outgoing task's callee-saved state into `*current`, restore `*next`, and resume `next`.
///
/// The ARM twin of x86's return-based switch. `ldmia` restores `r4-r11`, `sp`, `lr`; `bx lr` resumes
/// wherever `next` left off (its saved `lr`), or the trampoline for a fresh task. TTBR0 is switched
/// only when it changes - identical to x86's CR3 compare - so a switch between two kernel tasks
/// sharing the identity map never touches it.
///
/// **When TTBR0 *does* change (a user task enters or leaves), the whole TLB is flushed (`TLBIALL`).**
/// This is the SEC-26/27 obligation the `arch/CLAUDE.md` port contract names: unlike x86, an ARMv7
/// TTBR0 switch does **not** implicitly flush non-global entries, so without this a stale mapping from
/// the outgoing address space could be honoured under the incoming one. `TLBIALL` over-flushes (it
/// drops global kernel entries too, which merely re-walk) but is unconditionally correct;
/// per-ASID precision (`TLBIASID`) is a later optimisation, not a correctness need. The descriptors
/// themselves are made visible to the (non-cacheable) walker by a one-shot D-cache clean at spawn
/// (`page_tables::clean_invalidate_dcache_all`), so no per-switch cache maintenance is needed here.
///
/// # Safety
/// Both pointers must be valid, distinct, aligned `TaskContext`s. `next` must hold state saved by a
/// previous call here or built by a constructor above; its `sp` must point at a live stack. `next.cr3`
/// must be a valid TTBR0 whose page table's descriptors are already clean to the point of coherency.
#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {
    core::arch::naked_asm!(
        // Save outgoing callee-saved + sp + lr into *current (r0).
        "stmia r0, {{r4-r11}}",
        "str   sp, [r0, #0x20]",
        "str   lr, [r0, #0x24]",
        // Restore incoming (r1 = next).
        "ldr   r2, [r1, #0x28]",       // next.cr3 (low word = TTBR0)
        "mrc   p15, 0, r3, c2, c0, 0", // current TTBR0
        "cmp   r2, r3",
        "beq   1f",
        "mcr   p15, 0, r2, c2, c0, 0", // switch address space
        "mov   r3, #0",
        "mcr   p15, 0, r3, c8, c7, 0", // TLBIALL - ARM TTBR0 switch needs explicit TLB flush (SEC-26/27)
        "dsb",
        "isb",
        "1:",
        "ldmia r1, {{r4-r11}}",
        "ldr   sp, [r1, #0x20]",
        "ldr   lr, [r1, #0x24]",
        "bx    lr",                    // resume next
    )
}

// ---- Selftest: drive an ARM kernel task through the NEUTRAL surface ----

#[repr(align(8))]
struct NeutralStack([u8; 4096]);
static mut NEUTRAL_STACK: NeutralStack = NeutralStack([0; 4096]);

static mut SCHED_CTX: TaskContext = TaskContext {
    r4: 0, r5: 0, r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0, sp: 0, lr: 0, cr3: 0,
};
static mut TASK_CTX: TaskContext = TaskContext {
    r4: 0, r5: 0, r6: 0, r7: 0, r8: 0, r9: 0, r10: 0, r11: 0, sp: 0, lr: 0, cr3: 0,
};

static NEUTRAL_RAN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// A kernel task started through `new_kernel`: mark that it ran, then hand control back to the
/// "scheduler" context. Never returns - a scheduler task loops; it does not fall off its entry.
unsafe extern "C" fn neutral_task() -> ! {
    NEUTRAL_RAN.store(1, core::sync::atomic::Ordering::Relaxed);
    // SAFETY: SCHED_CTX holds the state saved by the switch that entered here, so switching back
    // resumes a live stack. Single-threaded boot context.
    unsafe {
        switch_context(core::ptr::addr_of_mut!(TASK_CTX), core::ptr::addr_of!(SCHED_CTX));
    }
    // The scheduler never switches back to us in this test; guard the fallthrough anyway.
    loop {
        unsafe { core::arch::asm!("wfi") }
    }
}

/// Prove the neutral surface works on ARM: build a task with `TaskContext::new_kernel`, enter it with
/// `switch_context`, and confirm it ran and returned - i.e. the exact API the scheduler uses drives
/// an ARM kernel task end to end.
pub fn selftest() {
    // SAFETY: Single-threaded boot context; these statics are touched only here and in `neutral_task`,
    // which cannot run until we switch into it. Reading current TTBR0 to give the task the shared
    // identity map, so its switch compare-and-skips rather than reloading a bogus address space.
    unsafe {
        let ttbr0: u32;
        core::arch::asm!("mrc p15, 0, {t}, c2, c0, 0", t = out(reg) ttbr0, options(nomem, nostack));

        let stack_top = core::ptr::addr_of_mut!(NEUTRAL_STACK) as *mut u8;
        let stack_top = stack_top.add(core::mem::size_of::<NeutralStack>());
        *core::ptr::addr_of_mut!(TASK_CTX) =
            TaskContext::new_kernel(neutral_task, stack_top, ttbr0 as u64);

        // Seed the scheduler context's cr3 with the LIVE TTBR0, exactly as the neutral `run()` does
        // (scheduler.rs, "switch_context never saves CR3, only loads it"). Without this, switching
        // back to SCHED_CTX would load its zero-initialised cr3 into TTBR0 and every translation would
        // fault. That this is necessary is the proof the ARM switch matches x86 semantics down to the
        // does-not-save-cr3 property - it reproduced the exact gotcha x86 documents.
        (*core::ptr::addr_of_mut!(SCHED_CTX)).cr3 = ttbr0 as u64;

        switch_context(core::ptr::addr_of_mut!(SCHED_CTX), core::ptr::addr_of!(TASK_CTX));
    }

    let ran = NEUTRAL_RAN.load(core::sync::atomic::Ordering::Relaxed);
    pl011_write(b"arm32: neutral-scheduler surface selftest - task ran: ");
    write_dec_pub(ran);
    pl011_write(b"\r\n");
    if ran == 1 {
        pl011_write(b"arm32: neutral surface PASS (TaskContext::new_kernel + switch_context drive an ARM task)\r\n");
    } else {
        pl011_write(b"arm32: neutral surface FAIL - the task did not run\r\n");
    }
}
