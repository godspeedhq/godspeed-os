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
