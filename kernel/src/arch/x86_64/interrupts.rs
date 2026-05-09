//! IDT entries and IRQ dispatch stubs — §12.
//!
//! The kernel IDT has two classes of entries:
//!   - CPU exceptions (vectors 0–31): handled entirely in kernel.
//!   - Hardware IRQs (vectors 32+): dispatched to `interrupt::route` which
//!     forwards them to the registered driver service via IPC.
//!
//! SAFETY boundary: raw interrupt frames are manipulated here and nowhere else.

/// CPU exception frame pushed by the processor on entry to an ISR.
#[repr(C)]
pub struct ExceptionFrame {
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// ---------------------------------------------------------------------------
// Timer ISR (vector 32) — §9.1 preemption quantum.
// ---------------------------------------------------------------------------

/// Naked ISR stub for the APIC timer (vector 32).
///
/// Saves all caller-saved registers, calls `timer_tick_from_irq`, restores,
/// then returns from interrupt.  The scheduler's `switch_context` may change
/// RSP inside `timer_tick_from_irq`; that is intentional — see §9.1.
#[no_mangle]
#[unsafe(naked)]
pub unsafe extern "C" fn timer_isr_stub() {
    // SAFETY: raw interrupt entry; all register saves are explicit.
    core::arch::naked_asm!(
        "push rax",
        "push rcx",
        "push rdx",
        "push rdi",
        "push rsi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "call timer_tick_from_irq",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rsi",
        "pop rdi",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
    )
}

// ---------------------------------------------------------------------------
// Dispatch helpers.
// ---------------------------------------------------------------------------

/// Dispatch a hardware IRQ to the userspace driver registered for it (§12.2).
///
/// # Safety
/// Called from raw interrupt context with interrupts disabled.
pub unsafe fn dispatch_irq(irq: u8) {
    // SAFETY: called only from the IDT stub with IF=0.
    crate::interrupt::route::deliver(irq);
}

/// Page-fault handler — kills the faulting task (§10.3).
///
/// # Safety
/// Called from IDT entry #14.
pub unsafe extern "C" fn page_fault_handler(frame: &ExceptionFrame, error_code: u64) {
    let fault_addr: u64;
    // SAFETY: CR2 holds the fault address on x86.
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) fault_addr) };

    crate::kprintln!(
        "service killed: protection violation at {:#x} (err={:#x})",
        fault_addr, error_code
    );
    crate::task::kill_current();
}
