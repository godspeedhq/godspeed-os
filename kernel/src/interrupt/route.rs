// SPDX-License-Identifier: GPL-2.0-only
//! Hardware interrupt routing to userspace driver services - §12.
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
/// **Every hold masks interrupts (`lock_irq`), because this table is read from the ISR.**
///
/// `deliver` runs in an interrupt handler and `register`/`unregister` run in a syscall. A spinlock is
/// not reentrant, so an unmasked hold in task context lets the interrupt fire on that same core and spin
/// on a lock its own core already owns - the exact deadlock that stopped a 1754-round soak in
/// `arch/arm/irq.rs::HIRES`. Same shape, same fix, found by the check that shape produced.
static IRQ_TABLE: SpinLock<[Option<EndpointId>; MAX_IRQ]> = SpinLock::new([None; MAX_IRQ]);

/// One-shot guard for the EHCI deliver() diagnostic (logs the first EHCI IRQ + its core).
static EHCI_DELIVER_LOGGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Register a driver endpoint to receive interrupts for `irq`.
/// Called at spawn time when the kernel processes a `hw_interrupt` capability.
pub fn register(irq: u8, endpoint: EndpointId) {
    let mut table = IRQ_TABLE.lock_irq();
    // SEC-16: never SILENTLY steal an IRQ line. On a clean driver restart the death path calls
    // `unregister` first, so the slot is None here; a Some for a DIFFERENT endpoint means either a
    // second driver claiming an already-owned line or a missed unregister - surface it loudly
    // (invariant 12) rather than a silent overwrite. The new registration still wins (a respawn's
    // fresh endpoint must take over its line); today it is unreachable (distinct per-device vectors).
    if let Some(existing) = table[irq as usize] {
        if existing != endpoint {
            crate::kprintln!(
                "interrupt: IRQ {} already routed - overwriting (second claim or a missed unregister?)", irq);
        }
    }
    table[irq as usize] = Some(endpoint);
}

/// The driver endpoint registered for `irq`, if any. Used to gate the `IrqUnmask` syscall:
/// only the driver that owns the route may re-open its IOAPIC gate (§12).
/// Release every IRQ routed to `endpoint`, and mask those lines. Returns how many were released.
///
/// BY ENDPOINT, NOT BY NAME. The kill path used to work out which line a dying task owned from a
/// hardcoded table of service names - "xhci" => 0x28, "ehci" => 0x29, anything else => nothing. A
/// name missing from that list simply kept its route forever: `dwc2` was missing, so every restart
/// of the ARM USB driver left its old claim behind and the kernel logged "IRQ 41 already routed -
/// overwriting (second claim or a missed unregister?)" - which was exactly right, and had been
/// reporting a real leak nobody read. Interrupt delivery to that service was erratic across
/// restarts as a result.
///
/// The kernel already knows which endpoint owns which line, because that is what this table IS.
/// Asking it directly cannot go stale when a service is added, renamed, or ported to another
/// architecture with a different vector, and it needs no list to be kept in step with reality.
pub fn unregister_endpoint(endpoint: EndpointId) -> usize {
    let mut released = 0usize;
    let mut irqs = [0u8; MAX_IRQ];
    {
        let mut table = IRQ_TABLE.lock_irq();
        for (irq, slot) in table.iter_mut().enumerate() {
            if *slot == Some(endpoint) {
                *slot = None;
                irqs[released] = irq as u8;
                released += 1;
            }
        }
    }
    // Masking touches the interrupt controller, so it happens OUTSIDE the table lock - the same
    // discipline the rest of this module keeps.
    for irq in irqs.iter().take(released) {
        crate::arch::imp::ioapic::mask_vector(*irq);
    }
    released
}

pub fn registered_endpoint(irq: u8) -> Option<EndpointId> {
    IRQ_TABLE.lock_irq()[irq as usize]
}

/// Remove the driver endpoint registered for `irq` (driver-death quiesce, §12).
///
/// Called on driver death so a route to the dead driver's endpoint is cleared before the
/// endpoint id is freed and REUSED. `IRQ_TABLE` stores a bare `EndpointId` (no generation),
/// so a reused id would otherwise inherit the dead driver's interrupts;
/// `enqueue_from_interrupt`'s liveness check only covers the still-Dead window, not a reused
/// id. Safe no-op if nothing was registered; the respawned driver re-registers.
pub fn unregister(irq: u8) {
    IRQ_TABLE.lock_irq()[irq as usize] = None;
    // UNMASK on release, or a dead driver leaves the line off FOREVER.
    //
    // `deliver` masks a level-triggered source so it cannot re-enter while the driver works, and the
    // driver unmasks through `IrqUnmask` once it has serviced the device. A driver that dies between
    // those two points - a fault, a `kill`, a chaos round - never reaches its unmask, and nothing else
    // was ever going to do it. The line then stays masked across the respawn, so the fresh instance
    // registers correctly, waits for an interrupt that is switched off at the controller, and looks
    // like a driver that cannot see its hardware.
    //
    // On arm32 it is worse than a stuck driver: the USB route falls back to the IN-KERNEL stack when
    // nobody is registered, so `kill dwc2` would hand USB back to a driver whose interrupt line had
    // been silently disabled - keyboard and storage dead, with the undo apparently applied.
    //
    // Releasing the route and releasing the mask are the same act: whoever holds the route owes the
    // unmask, and if they are gone the debt falls here. Harmless for edge/MSI vectors, where masking
    // was a no-op to begin with.
    crate::arch::imp::ioapic::unmask_vector(irq);
}

/// Deliver IRQ `irq` to the registered driver as an IPC message.
///
/// # Safety
/// Called from interrupt context with IF=0. The APIC EOI is sent unconditionally
/// at the end; missing the EOI would leave the IRQ line permanently masked.
pub unsafe fn deliver(irq: u8) {
    // One-shot diagnostic: confirm the IDT actually receives the EHCI vector and on which core
    // (the EHCI's legacy INTx delivery has been the hard part on the T630). Logged once.
    if irq == crate::arch::imp::interrupts::EHCI_MSI_VECTOR
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
    crate::arch::imp::ioapic::mask_vector(irq);

    let endpoint = IRQ_TABLE.lock_irq()[irq as usize];
    if let Some(ep) = endpoint {
        // COALESCE interrupt notifications, but only against QUEUE PRESSURE - never against the mere
        // presence of unrelated work.
        //
        // This read `endpoint_queue_depth(ep) == 0`, so ANY queued message suppressed the interrupt.
        // A driver whose endpoint carries more than interrupts - `xhci` receives block requests from
        // `block-driver` on the same endpoint - therefore lost IRQ notifications whenever it happened
        // to have a request pending. The comment claimed "the kernel re-notifies once the queue
        // drains"; there is NO such path. This is the only enqueue site, and nothing re-triggers it
        // but the next hardware interrupt.
        //
        // It is currently masked by those drivers ALSO polling their event ring every pass, which is
        // why it has never been seen. It becomes a lost keystroke - or a lost disk completion - the
        // moment a driver stops polling, which is exactly the direction xhci is moving.
        //
        // Half the queue is the reserve. Below it, always notify: a pending block request must never
        // cost an interrupt. At or above it, skip: that is a genuine storm, the queue is the thing
        // under pressure, and the driver reads its ring on the next pass regardless. So the
        // pathology this was written for (notifications piling up until `observe` shows a full queue
        // after a max-carnage interrupt storm) is still bounded, and the correctness hole is closed.
        //
        // (The EOI below still fires unconditionally, on every path.)
        const IRQ_NOTIFY_RESERVE: u8 = (crate::ipc::queue::QUEUE_DEPTH / 2) as u8;
        if crate::ipc::routing::endpoint_queue_depth(ep) < IRQ_NOTIFY_RESERVE {
            let msg = crate::ipc::message::Message::interrupt_event(irq);
            if let Some(receiver_slot) = crate::ipc::routing::enqueue_from_interrupt(ep, msg) {
                // wake_by_slot marks the receiver Ready and sends a WAKE_RECEIVER IPI
                // to its core if it lives on a different core than the one handling
                // this IRQ (§12.2 cross-core delivery path).
                crate::task::scheduler::wake_by_slot(receiver_slot, 0);
            }
        }
    }
    // EOI must fire unconditionally - even on discard and even on full queue.
    // If the APIC is not re-armed here, the IRQ line stays masked and the system hangs.
    crate::arch::imp::interrupts::send_eoi();
}
