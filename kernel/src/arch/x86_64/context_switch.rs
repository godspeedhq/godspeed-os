//! Task context save/restore — §9.
//!
//! A context switch saves the outgoing task's callee-saved registers + RSP
//! to its `TaskContext` and restores the incoming task's saved state.
//! The saved RSP must point at the task's next instruction pointer (pushed
//! on the stack before the first switch, or by the previous `call` that
//! led into `switch_context`).

/// Saved register state for a suspended task.
#[repr(C)]
pub struct TaskContext {
    // Callee-saved general-purpose registers (System V AMD64 ABI).
    pub rbx: u64,    // offset 0x00
    pub rbp: u64,    // offset 0x08
    pub r12: u64,    // offset 0x10
    pub r13: u64,    // offset 0x18
    pub r14: u64,    // offset 0x20
    pub r15: u64,    // offset 0x28
    // Padding / informational: entry point stored here for new tasks (not used by asm).
    pub rip: u64,    // offset 0x30
    // Stack pointer at point of suspension. For new tasks: points at entry address on stack.
    pub rsp: u64,    // offset 0x38
    // CR3 for this task's address space.
    pub cr3: u64,    // offset 0x40
}

/// First-entry trampoline for new kernel tasks.
///
/// The scheduler's initial `switch_context` disables interrupts with `cli`
/// before the switch, so a brand-new task would start with IF=0.  This
/// trampoline is jumped to (via `ret`) on the very first switch into a task;
/// it re-enables interrupts and then `ret`s to the real entry function, which
/// was stacked directly above the trampoline address.
#[unsafe(naked)]
unsafe extern "C" fn task_entry_trampoline() -> ! {
    // SAFETY: the task entry address is on the stack above us, placed there
    //         by `new_kernel`.  `sti` reverses the `cli` from the initial
    //         context switch.  `ret` pops the real entry into RIP.
    core::arch::naked_asm!(
        "sti",
        "ret",
    )
}

/// First-entry trampoline for new ring-3 tasks.
///
/// On entry (via `switch_context`'s `ret`), the kernel stack contains:
///   [RSP+0]  user_rip   — ring-3 entry point
///   [RSP+8]  user_rsp   — initial user-space stack pointer
///
/// GS invariant: this function always runs in ring-0 with GS.base = kernel ptr.
/// `swapgs` restores the user's GS (0) into GS.base before SYSRETQ, so that the
/// ring-3 task sees GS.base=0 and its first SYSCALL's `swapgs` correctly loads
/// the kernel ptr back into GS.base.
///
/// The `cli` from `switch_context` is in effect throughout, so no interrupt fires
/// while RSP holds a user-space address.
#[unsafe(naked)]
unsafe extern "C" fn ring3_entry_trampoline() -> ! {
    // SAFETY: stack layout guaranteed by `new_user`.  GS invariant: ring-0 holds
    // GS.base=kernel_ptr; `swapgs` exchanges it with KERNEL_GS_BASE=0 (user GS).
    // SYSRETQ uses RCX as the new RIP and R11 as the new RFLAGS; it restores the
    // ring-3 selector pair from STAR, setting CPL=3 atomically.
    core::arch::naked_asm!(
        "swapgs",           // GS.base: kernel_ptr → 0 (user); KERNEL_GS_BASE: 0 → kernel_ptr
        "pop rcx",          // user_rip → rcx (SYSRETQ new RIP)
        "pop rsp",          // user_rsp → rsp (switches to user stack; still ring-0)
        "mov r11, 0x202",   // RFLAGS: IF=1 (bit 9) + reserved bit 1; SYSRETQ restores
        "sysretq",          // → ring-3 at rcx, rsp=user_rsp, rflags=0x202
    )
}

impl TaskContext {
    /// Build the initial context for a kernel task.
    ///
    /// `entry`     — function the task will start executing.
    /// `stack_top` — one-past-end of the task's stack buffer (16-byte aligned).
    /// `cr3`       — page-table root to load on first switch.
    ///
    /// # Safety
    /// `stack_top` must point to writable memory with at least 24 bytes below it.
    pub unsafe fn new_kernel(
        entry: unsafe extern "C" fn() -> !,
        stack_top: *mut u8,
        cr3: u64,
    ) -> Self {
        // Stack layout built here (high → low addresses):
        //   [stack_top -  8]: alignment padding  (ensures ABI-correct RSP at entry)
        //   [stack_top - 16]: real entry fn addr  (trampoline's `ret` target)
        //   [stack_top - 24]: trampoline addr     (switch_context's `ret` target)
        //
        // When switch_context `ret`s:  RSP = stack_top-16, RIP = trampoline
        // When trampoline `ret`s:      RSP = stack_top-8,  RIP = entry
        //   → RSP at entry = stack_top-8, which is 16n-8 ✓ (System V AMD64 ABI)
        //
        // SAFETY: caller guarantees stack_top is valid and writable.
        let sp = unsafe {
            let sp = (stack_top as *mut u64).sub(1);
            sp.write(0u64);                            // alignment padding
            let sp = sp.sub(1);
            sp.write(entry as u64);                    // real task entry
            let sp = sp.sub(1);
            sp.write(task_entry_trampoline as u64);    // first ret target
            sp
        };
        TaskContext {
            rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            rip: entry as u64,
            rsp: sp as u64,
            cr3,
        }
    }

    /// Build the initial context for a ring-3 task.
    ///
    /// `kernel_stack_top` — one-past-end of the **kernel** stack for this task
    ///                       (16-byte aligned; used for SYSCALL and hardware interrupts).
    /// `user_entry`       — ring-3 entry point virtual address.
    /// `user_stack_top`   — initial user-space RSP (one-past-end of user stack).
    /// `cr3`              — user page-table root.
    ///
    /// # Safety
    /// `kernel_stack_top` must point to writable kernel memory with at least
    /// 512 bytes available below it.  `user_entry` and `user_stack_top` must be
    /// valid addresses in the user address space mapped by `cr3`.
    pub unsafe fn new_user(
        kernel_stack_top: *mut u8,
        user_entry:       u64,
        user_stack_top:   u64,
        cr3:              u64,
    ) -> Self {
        // Kernel-stack layout built here (high → low addresses, K0T = kernel_stack_top):
        //
        //   [K0T-368]: user_rsp  — ring3_entry_trampoline's `pop rsp` target
        //   [K0T-376]: user_rip  — ring3_entry_trampoline's `pop rcx` target
        //   [K0T-384]: ring3_entry_trampoline — switch_context `ret` target; ctx.rsp
        //
        // WHY K0T-384, not the obvious K0T-32:
        //
        // When the timer fires from ring-3 the CPU uses TSS.rsp0 = K0T and pushes
        // the interrupt frame (SS, RSP, RFLAGS, CS, RIP), placing CS = 0x2b at
        // exactly [K0T-32].  The ISR stub then saves 9 caller-saved registers
        // ([K0T-40]..[K0T-112]) and calls timer_tick_from_irq, whose own frame
        // reaches approximately K0T-260 on the early-return path (no context switch).
        //
        // Placing ring3_entry_trampoline at K0T-32 puts it in the CPU interrupt
        // frame's CS slot: the very first timer tick from ring-3 overwrites it with
        // 0x2b, and any subsequent zero-init in the kernel stack clobbers it to 0x0,
        // causing the scheduler's next `ret` to jump to rip=0 → page fault → crash.
        //
        // K0T-384 is:
        //   • Below the early-return ISR depth (~K0T-260) — the timer ISR on the
        //     no-switch path cannot reach it, so [K0T-384] is never overwritten
        //     before the first real context switch updates TASK_CTX[slot].rsp.
        //   • Above the SYSCALL kernel_rsp (K0T-512) — SYSCALL grows downward
        //     from K0T-512, so it can never write upward to K0T-384.
        //   • After the first real context switch, switch_context saves the current
        //     RSP (inside the ISR frame, ~K0T-200) into TASK_CTX[slot].rsp, making
        //     [K0T-384] dead data that is never consulted again.
        //
        // switch_context `ret` with RSP = K0T-384:
        //   → RIP = ring3_entry_trampoline, RSP = K0T-376
        //
        // ring3_entry_trampoline:
        //   swapgs
        //   pop rcx  → rcx = user_rip,  RSP = K0T-368
        //   pop rsp  → rsp = user_rsp   (switches to user stack)
        //   sysretq  → ring-3 at user_rip, rsp=user_rsp, rflags=0x202
        //
        // SAFETY: caller guarantees kernel_stack_top is valid and writable.
        let sp = unsafe {
            let sp = (kernel_stack_top as *mut u64).sub(46); // K0T-368
            sp.write(user_stack_top);                        // user_rsp
            let sp = sp.sub(1);                              // K0T-376
            sp.write(user_entry);                            // user_rip
            let sp = sp.sub(1);                              // K0T-384
            sp.write(ring3_entry_trampoline as u64);         // first ret target
            sp
        };
        TaskContext {
            rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0,
            rip: user_entry,
            rsp: sp as u64,
            cr3,
        }
    }
}

/// Switch from `current` to `next`.
///
/// Saves callee-saved registers + RSP into `*current`; restores from `*next`;
/// changes CR3 if they differ; returns via `ret` (pops RIP from new stack).
///
/// # Safety
/// - Both pointers must be valid and 8-byte aligned.
/// - Called with interrupts disabled on this core.
/// - `current != next`.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {
    core::arch::naked_asm!(
        // Save callee-saved registers of the outgoing task (rdi = current).
        "mov [rdi + 0x00], rbx",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], r12",
        "mov [rdi + 0x18], r13",
        "mov [rdi + 0x20], r14",
        "mov [rdi + 0x28], r15",
        "mov [rdi + 0x38], rsp",   // save RSP to the rsp field (offset 0x38)
        // Restore callee-saved registers of the incoming task (rsi = next).
        "mov rbx, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov r12, [rsi + 0x10]",
        "mov r13, [rsi + 0x18]",
        "mov r14, [rsi + 0x20]",
        "mov r15, [rsi + 0x28]",
        "mov rsp, [rsi + 0x38]",   // restore RSP from the rsp field (offset 0x38)
        // Switch address space only if CR3 changes (avoids a costly TLB flush).
        "mov rax, [rsi + 0x40]",
        "mov rcx, cr3",
        "cmp rax, rcx",
        "je 2f",
        "mov cr3, rax",
        "2:",
        "ret",                     // pops RIP from the new stack
    )
}
