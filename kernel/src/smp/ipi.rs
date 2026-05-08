//! Inter-Processor Interrupts — §9.4, §8.4, §10.5.
//!
//! Used for three distinct purposes:
//!   1. Waking a task blocked on `recv` after a cross-core `send` enqueues a message.
//!   2. TLB shootdown after a page is unmapped (§10.5).
//!   3. Cross-core scheduler preemption (timer overflow).
//!
//! All three go through the local APIC ICR (Interrupt Command Register).

/// Vector numbers for each IPI purpose.
pub mod vectors {
    pub const WAKE_RECEIVER:  u8 = 0xF0;
    pub const TLB_SHOOTDOWN:  u8 = 0xF1;
    pub const SCHEDULER_TICK: u8 = 0xF2;
}

/// Send an IPI to a specific core.
///
/// # Safety
/// `core_id` must refer to a ready core; APIC base must be mapped.
pub unsafe fn send_ipi(core_id: u32, vector: u8) {
    // SAFETY: caller guarantees core_id is valid and APIC is mapped.
    unsafe {
        todo!("write ICR high (destination APIC ID), then low (vector + fixed delivery) to APIC MMIO")
    }
}

/// Broadcast a TLB shootdown IPI to all other cores and wait for acknowledgment.
///
/// # Safety
/// Interrupts should be disabled on the calling core before issuing a shootdown.
pub unsafe fn broadcast_tlb_shootdown(virt_addr: u64) {
    // SAFETY: caller manages interrupt state.
    unsafe {
        todo!("send IPI to all other cores, spin until ack count == ready_count - 1")
    }
}

/// IPI handler — invoked from the IDT stub on the receiving core.
///
/// # Safety
/// Called from raw interrupt context with interrupts disabled.
pub unsafe extern "C" fn ipi_handler(vector: u8) {
    match vector {
        vectors::WAKE_RECEIVER  => crate::task::scheduler::timer_tick(),
        vectors::TLB_SHOOTDOWN  => handle_tlb_shootdown(),
        vectors::SCHEDULER_TICK => crate::task::scheduler::timer_tick(),
        _ => {}
    }
    // SAFETY: EOI write to APIC.
    unsafe { todo!("write EOI to local APIC") }
}

fn handle_tlb_shootdown() {
    todo!("invalidate local TLB; signal ack to the broadcasting core")
}
