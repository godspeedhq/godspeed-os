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
        // Pass interrupted RIP ([RSP+72]), CS ([RSP+80]), and user RSP ([RSP+96])
        // as arguments to timer_tick_from_irq(rdi=rip, rsi=cs, rdx=user_rsp).
        // The hardware interrupt frame (from ring-3) lays out as:
        //   [RSP+72]=RIP  [RSP+80]=CS  [RSP+88]=RFLAGS  [RSP+96]=RSP  [RSP+104]=SS
        // The saved values of rdi, rsi, rdx are on the stack at [RSP+24],[RSP+32],
        // [RSP+16]; they are restored after the call, so overwriting them here is safe.
        // 9 pushes × 8 bytes = 72 bytes between current RSP and the interrupt frame.
        "mov rdi, [rsp + 72]",
        "mov rsi, [rsp + 80]",
        "mov rdx, [rsp + 96]",
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
// UART RX ISR (vector 36 = PIC offset 32 + IRQ 4) — COM1 console input.
// ---------------------------------------------------------------------------

/// Naked ISR stub for COM1 UART RX (IRQ 4, vector 36).
///
/// Structure mirrors `timer_isr_stub`: conditional swapgs, save caller-saved
/// registers, call handler, restore, conditional swapgs, iretq.
/// No context switch occurs here — we just drain the FIFO, push to the ring
/// buffer, and optionally wake a blocked ConsoleRead syscall.
#[no_mangle]
#[unsafe(naked)]
pub unsafe extern "C" fn uart_rx_isr_stub() {
    // SAFETY: raw interrupt entry; all register saves are explicit.
    core::arch::naked_asm!(
        "test byte ptr [rsp + 8], 3",
        "jz 1f",
        "swapgs",
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
        "call uart_rx_irq_handler",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rsi",
        "pop rdi",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "test byte ptr [rsp + 8], 3",
        "jz 2f",
        "swapgs",
        "2:",
        "iretq",
    )
}

/// COM1 UART RX IRQ handler — drains FIFO, wakes blocked ConsoleRead task.
///
/// # Safety
/// Called from raw interrupt context (IF=0).
#[no_mangle]
unsafe extern "C" fn uart_rx_irq_handler() {
    use core::sync::atomic::Ordering;
    // SAFETY: called from ISR with IF=0; uart_rx_drain_fifo reads COM1 FIFO.
    unsafe { crate::arch::x86_64::uart_rx_drain_fifo(); }

    // Wake any task blocked on ConsoleRead.
    let waiter = crate::arch::x86_64::CONSOLE_READ_WAITER.load(Ordering::Acquire);
    if waiter != u32::MAX {
        crate::task::scheduler::wake_by_slot(waiter as usize, 0);
    }

    // EOI to local APIC.
    send_eoi();
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

/// Enable interrupts and return immediately (pure busy-spin, no C-state hint).
///
/// Used in the idle loop. On Goldmont+ (Apollo Lake / Gemini Lake), both `hlt`
/// and `pause` trigger firmware C-state promotion that power-gates the local
/// APIC, silencing both APIC timer ticks and cross-core IPIs.  Issuing only
/// `sti` — with no low-power hint of any kind — keeps the core fully active
/// and prevents C-state entry entirely.  The outer scheduler loop's
/// `compiler_fence(SeqCst)` ensures every iteration re-reads TASK_STATE,
/// so wakeups written by other cores are not missed.
/// True when idle cores may safely `hlt` — set once at boot from the ARAT CPUID
/// bit (CPUID.06H:EAX[2], "Always Running APIC Timer"). ARAT is the hardware's
/// guarantee that the LAPIC timer keeps ticking through C-states, so a halted
/// core still receives its scheduler tick (and IPIs/IRQs wake it). When ARAT is
/// absent (e.g. some Goldmont parts) we keep the legacy sti-only spin.
static IDLE_CAN_HALT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Record (once, at BSP boot) whether idle cores may halt. See `IDLE_CAN_HALT`.
pub fn set_idle_can_halt(v: bool) {
    IDLE_CAN_HALT.store(v, core::sync::atomic::Ordering::Relaxed);
}

/// Whether idle cores halt (true) or spin (false). See `IDLE_CAN_HALT`.
pub fn idle_can_halt() -> bool {
    IDLE_CAN_HALT.load(core::sync::atomic::Ordering::Relaxed)
}

#[inline]
pub fn wait_for_interrupt() {
    if IDLE_CAN_HALT.load(core::sync::atomic::Ordering::Relaxed) {
        // ARAT present: halting is safe — the LAPIC timer survives the C-state,
        // so the next tick/IPI/IRQ wakes the core. Draws near-zero power, so an
        // idle core runs cool instead of spinning. `sti; hlt` is atomic w.r.t.
        // interrupt delivery (an interrupt cannot fire in the 1-instruction
        // window after STI), so there is no lost-wakeup race.
        // SAFETY: STI then HLT in ring-0; HLT wakes on any interrupt.
        unsafe { core::arch::asm!("sti; hlt", options(nostack, nomem)) }
    } else {
        // No ARAT: HLT/PAUSE let firmware power-gate the LAPIC, dropping timer
        // ticks and IPIs (observed on Goldmont+). Spin with STI only — keeps the
        // core hot but the scheduler correct.
        // SAFETY: STI is always safe in ring-0; no other instructions follow.
        unsafe { core::arch::asm!("sti", options(nostack, nomem)) }
    }
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

