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
///
/// GS invariant (§8.2): ring-0 code always runs with GS.base = kernel ptr;
/// ring-3 code runs with GS.base = 0.  An interrupt from ring-3 arrives with
/// GS.base = 0 (the user's GS), so we must `swapgs` to load the kernel ptr
/// before any `gs:`-relative access, and undo it before `iretq`.
/// Interrupts from ring-0 arrive with GS.base = kernel ptr and need no swap.
///
/// After a context switch inside `timer_tick_from_irq`, the interrupt frame
/// at RSP belongs to the newly scheduled task; its CS tells us whether to
/// swapgs before that task resumes.
#[no_mangle]
#[unsafe(naked)]
pub unsafe extern "C" fn timer_isr_stub() {
    // SAFETY: raw interrupt entry; all register saves are explicit.
    // CPU-pushed interrupt frame: [rsp]=RIP, [rsp+8]=CS, [rsp+16]=RFLAGS
    // (+RSP, SS if from ring-3).  CS low 2 bits = CPL.
    core::arch::naked_asm!(
        // If CPL == 0 the interrupt came from ring-0; GS already holds kernel ptr.
        "test byte ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",           // ring-3 → ring-0: load kernel ptr into GS.base
        "1:",
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
        // RSP is back at the interrupt frame (possibly the new task's after a switch).
        "test byte ptr [rsp + 8], 3",
        "jz 2f",
        "swapgs",           // returning to ring-3: restore user GS (0)
        "2:",
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

/// Enable hardware interrupts on the current core.
#[inline]
pub fn enable_interrupts() {
    // SAFETY: STI is always safe to execute in ring-0; caller controls timing.
    unsafe { core::arch::asm!("sti", options(nostack, nomem)) }
}

/// Disable hardware interrupts on the current core.
#[inline]
pub fn disable_interrupts() {
    // SAFETY: CLI is always safe to execute in ring-0.
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) }
}

/// Enable interrupts and halt until the next interrupt fires, then return.
///
/// Used in the idle loop. Interrupts are re-disabled by the interrupt handler
/// before this function's continuation executes.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: STI+HLT pair in idle loop; interrupts were previously disabled
    // by the caller (scheduler). The processor atomically enables interrupts
    // and halts, preventing a missed-wakeup race.
    unsafe { core::arch::asm!("sti; hlt", options(nostack)) }
}

/// Signal End-Of-Interrupt to the local APIC so the interrupt line is re-armed.
///
/// Must be called at the end of every hardware IRQ handler (timer, device IRQs,
/// IPIs). Calling it while interrupts are enabled is safe — it only writes
/// the APIC EOI register, which has no effect on the current interrupt state.
#[inline]
pub fn send_eoi() {
    // SAFETY: apic_send_eoi writes only the local APIC EOI register, which is
    // idempotent and has no memory-safety implications; APIC is mapped before
    // any IRQ fires.
    unsafe { crate::arch::x86_64::boot::apic_send_eoi() }
}

/// Fire a test IRQ synchronously from the control channel.
///
/// Disables interrupts, calls `deliver(irq)` (which requires IF=0), then
/// re-enables interrupts. Used only by the `FIRE_IRQ` COM2 control command
/// (§22 Tests IR1A/IR1B). EOI inside `deliver` is idempotent when no real
/// hardware interrupt is pending.
#[inline]
pub fn fire_test_irq(irq: u8) {
    disable_interrupts();
    // SAFETY: interrupts are disabled above (IF=0), satisfying deliver's calling
    // convention. EOI to the APIC is safe outside a real IRQ — the write is
    // idempotent and the APIC ignores spurious EOIs.
    unsafe { crate::interrupt::route::deliver(irq); }
    enable_interrupts();
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
