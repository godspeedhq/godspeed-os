//! Task context save/restore — §9.
//!
//! A context switch saves the outgoing task's callee-saved registers to its
//! `TaskContext` and restores the incoming task's saved state.
//! Called only from `scheduler::run` on the current core's run queue.

/// Saved register state for a suspended task.
#[repr(C)]
pub struct TaskContext {
    // Callee-saved general-purpose registers (System V AMD64 ABI).
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    // Instruction pointer restored via `ret` from switch stub.
    pub rip: u64,
    // Stack pointer at point of suspension.
    pub rsp: u64,
    // CR3 for this task's address space.
    pub cr3: u64,
}

/// Switch from `current` to `next`.
///
/// # Safety
/// Both contexts must be valid, properly aligned, and live for the duration
/// of the switch. Called with interrupts disabled on this core.
#[unsafe(naked)]
pub unsafe extern "C" fn switch_context(current: *mut TaskContext, next: *const TaskContext) {
    // naked_asm! is required in naked functions (Rust 1.82+).
    // SAFETY: caller saves/restores rdi/rsi per System V AMD64 ABI.
    core::arch::naked_asm!(
        // Save callee-saved registers of current task (rdi = current).
        "mov [rdi + 0x00], rbx",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], r12",
        "mov [rdi + 0x18], r13",
        "mov [rdi + 0x20], r14",
        "mov [rdi + 0x28], r15",
        "mov [rdi + 0x30], rsp",
        // Restore callee-saved registers of next task (rsi = next).
        "mov rbx, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov r12, [rsi + 0x10]",
        "mov r13, [rsi + 0x18]",
        "mov r14, [rsi + 0x20]",
        "mov r15, [rsi + 0x28]",
        "mov rsp, [rsi + 0x30]",
        // Switch address space if CR3 differs (avoids TLB flush on same space).
        "mov rax, [rsi + 0x40]",
        "mov rcx, cr3",
        "cmp rax, rcx",
        "je 2f",
        "mov cr3, rax",
        "2:",
        "ret",
    )
}
