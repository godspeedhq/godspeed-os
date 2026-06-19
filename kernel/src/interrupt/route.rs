// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Hardware interrupt routing to userspace driver services — §12.
//!
//! The kernel IDT invokes `deliver(irq)` for every hardware IRQ. This module
//! looks up the registered driver endpoint for that IRQ and delivers the
//! interrupt as an IPC message. If the driver is on a different core than the
//! IRQ-receiving core, delivery goes through the cross-core IPC path (§12.2).
//!
//! Driver services register their IRQ lines at spawn time via their contract
//! `hw_interrupt` capability (§12.3). The kernel validates the capability and
//! inserts the route here.

use crate::ipc::endpoint::EndpointId;
use crate::smp::SpinLock;

const MAX_IRQ: usize = 256;

/// Registered driver endpoint for each IRQ line.
static IRQ_TABLE: SpinLock<[Option<EndpointId>; MAX_IRQ]> = SpinLock::new([None; MAX_IRQ]);

/// One-shot guard for the EHCI deliver() diagnostic (logs the first EHCI IRQ + its core).
static EHCI_DELIVER_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Register a driver endpoint to receive interrupts for `irq`.
/// Called at spawn time when the kernel processes a `hw_interrupt` capability.
pub fn register(irq: u8, endpoint: EndpointId) {
    IRQ_TABLE.lock()[irq as usize] = Some(endpoint);
}

/// The driver endpoint registered for `irq`, if any. Used to gate the `IrqUnmask` syscall:
/// only the driver that owns the route may re-open its IOAPIC gate (§12).
pub fn registered_endpoint(irq: u8) -> Option<EndpointId> {
    IRQ_TABLE.lock()[irq as usize]
}

/// Deliver IRQ `irq` to the registered driver as an IPC message.
///
/// # Safety
/// Called from interrupt context with IF=0. The APIC EOI is sent unconditionally
/// at the end; missing the EOI would leave the IRQ line permanently masked.
pub unsafe fn deliver(irq: u8) {
    // One-shot diagnostic: confirm the IDT actually receives the EHCI vector and on which core
    // (the EHCI's legacy INTx delivery has been the hard part on the T630). Logged once.
    if irq == crate::arch::x86_64::interrupts::EHCI_MSI_VECTOR
        && !EHCI_DELIVER_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed)
    {
        crate::kprintln!(
            "ehci: kernel deliver() vector={:#x} on core {}",
            irq, crate::task::scheduler::current_core_id()
        );
    }
    // For a level-triggered IOAPIC route (legacy INTx, e.g. the EHCI), mask the source now so
    // it does not re-fire while the userspace driver handles it (the line stays asserted until
    // the driver clears the device's interrupt status). The driver unmasks via the IrqUnmask
    // syscall after acking. No-op for edge/MSI vectors (the xHCI), which need no masking.
    crate::arch::x86_64::ioapic::mask_vector(irq);

    let endpoint = IRQ_TABLE.lock()[irq as usize];
    if let Some(ep) = endpoint {
        let msg = crate::ipc::message::Message::interrupt_event(irq);
        if let Some(receiver_slot) = crate::ipc::routing::enqueue_from_interrupt(ep, msg) {
            // wake_by_slot marks the receiver Ready and sends a WAKE_RECEIVER IPI
            // to its core if it lives on a different core than the one handling
            // this IRQ (§12.2 cross-core delivery path).
            crate::task::scheduler::wake_by_slot(receiver_slot, 0);
        }
        // If queue full: interrupt silently discarded; driver is overloaded (§12).
    }
    // EOI must fire unconditionally — even on discard and even on full queue.
    // If the APIC is not re-armed here, the IRQ line stays masked and the system hangs.
    crate::arch::x86_64::interrupts::send_eoi();
}
