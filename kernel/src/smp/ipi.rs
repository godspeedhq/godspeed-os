// SPDX-License-Identifier: GPL-2.0-only
//! Inter-Processor Interrupts - §9.4, §8.4.
//!
//! Two purposes:
//!   1. Wake a task blocked on `recv` after a cross-core `send` enqueues.
//!   2. Cross-core scheduler preemption (timer overflow).
//!
//! **There is no TLB shootdown here, and that is deliberate (§10.5, `backlog/80` K20).** A software
//! broadcast - a single-page and a full-flush form, and the per-core request slots behind them -
//! lived here with no caller, and was deleted on 2026-10-10. Translation coherence does
//! not need it, because nothing unmaps a live user mapping: memory is reclaimed only after the task
//! has died on every core. The kill path waits until no core is running the task or still leaving it
//! (`core_still_using`, `CORE_LEAVING` in `task/scheduler.rs`), every context switch invalidates on
//! its own core, and AArch64 - whose idle core keeps the dead root installed - also invalidates
//! inner-shareable in `free_page_table_root` before the root is freed. A future unmap of a mapping a
//! live task can reach is a new requirement, and it brings its own coherence mechanism with it.

/// Vector numbers for each IPI purpose.
pub mod vectors {
    pub const WAKE_RECEIVER:  u8 = 0xF0;
    pub const SCHEDULER_TICK: u8 = 0xF2;
}

/// Send a fixed-delivery IPI to a specific core.
///
/// # Safety
/// `core_id` must refer to a ready core; the local APIC must be mapped.
pub unsafe fn send_ipi(core_id: u32, vector: u8) {
    // Resolve the core -> LAPIC mapping (smp owns it) and hand the raw send to the arch seam (arch owns
    // the APIC MMIO). The protocol logic below stays here; only the register programming lives in arch.
    let lapic_id = crate::smp::core::core_lapic_id(core_id);
    // SAFETY: local APIC mapped after init_local_apic; lapic_id is a ready core's id.
    unsafe { crate::arch::imp::boot::send_ipi_to_lapic(lapic_id, vector); }
}

/// IPI handler - invoked from the IDT stub (`ipi_dispatch`) on the receiving core.
///
/// # Safety
/// Called from raw interrupt context with interrupts disabled.
pub unsafe fn ipi_handler(vector: u8) {
    match vector {
        vectors::WAKE_RECEIVER => {
            // ipi_wake_stub now calls timer_tick_from_irq directly (with the
            // same swapgs protocol as timer_isr_stub) so it never reaches this
            // branch.  Left as a no-op for robustness if another path routes
            // here unexpectedly.
        }
        vectors::SCHEDULER_TICK => {
            // Same reasoning: just let the loop pick up new work after iretq.
        }
        _ => {}
    }
    // Send EOI to allow the next interrupt to be delivered.
    // SAFETY: called from interrupt context; APIC is mapped.
    unsafe { crate::arch::imp::boot::apic_send_eoi() }
}
