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

/// Register a driver endpoint to receive interrupts for `irq`.
/// Called at spawn time when the kernel processes a `hw_interrupt` capability.
pub fn register(irq: u8, endpoint: EndpointId) {
    IRQ_TABLE.lock()[irq as usize] = Some(endpoint);
}

/// Deliver IRQ `irq` to the registered driver as an IPC message.
///
/// # Safety
/// Called from interrupt context with IF=0.
pub unsafe fn deliver(irq: u8) {
    let endpoint = IRQ_TABLE.lock()[irq as usize];
    if let Some(ep) = endpoint {
        todo!("build an IPC interrupt-event message; call ipc::routing::enqueue(ep, msg)")
    }
    // If no driver is registered, discard the interrupt. No panic — a driver
    // may not have started yet during early boot.
}
