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
