// SPDX-License-Identifier: GPL-2.0-only
//! Hardware interrupt routing to userspace driver services - §12.
//!
//! The kernel IDT invokes `deliver(irq)` for every hardware IRQ. This module
//! looks up the registered driver endpoint for that IRQ and delivers the
//! interrupt as an IPC message. If the driver is on a different core than the
//! IRQ-receiving core, delivery goes through the cross-core IPC path (§12.2).
//!
//! A driver's IRQ lines are routed at spawn time: the kernel derives the vectors
//! from the device CLASS the spawn request names (`task::hw_irqs_for`, or a PCI
//! MSI vector it allocates) and inserts the route here (§12.3, §14.1). Neither
//! the contract nor the spawner names a vector.

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


/// Register a driver endpoint to receive interrupts for `irq`, and OPEN the line.
/// Called by the spawn path for each vector the device class resolved to.
///
/// A driver's death releases its routes through `unregister_endpoint`, which MASKS each line, so a
/// level source cannot storm with no one to service it. The respawned driver's registration is the
/// moment someone can again, so this unmasks: until 2026-10-10 nothing did, and a respawned
/// level-triggered driver that waited for its first interrupt before calling `IrqUnmask` waited on a
/// line switched off at the controller (`backlog/fixed/80` K17). Whoever holds the route owes the unmask;
/// a new holder takes the debt on by holding it. `deliver` masks again on the first interrupt, as it
/// always has, and a no-op for edge and MSI vectors, where masking never applied.
pub fn register(irq: u8, endpoint: EndpointId) {
    {
        let mut table = IRQ_TABLE.lock_irq();
        // SEC-16: never SILENTLY steal an IRQ line. On a clean driver restart the death path calls
        // `unregister_endpoint` first, so the slot is None here; a Some for a DIFFERENT endpoint
        // means either a second driver claiming an already-owned line or a missed release - surface
        // it loudly (invariant 12) rather than a silent overwrite. The new registration still wins
        // (a respawn's fresh endpoint must take over its line); today it is unreachable (distinct
        // per-device vectors).
        if let Some(existing) = table[irq as usize] {
            if existing != endpoint {
                crate::kprintln!(
                    "interrupt: IRQ {} already routed - overwriting (second claim or a missed release?)", irq);
            }
        }
        table[irq as usize] = Some(endpoint);
    }
    // Touches the interrupt controller, so OUTSIDE the table lock - as `unregister_endpoint` does.
    crate::arch::imp::ioapic::unmask_vector(irq);
}

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

/// The driver endpoint registered for `irq`, if any. Used to gate the `IrqUnmask` syscall:
/// only the driver that owns the route may re-open its IOAPIC gate (§12).
pub fn registered_endpoint(irq: u8) -> Option<EndpointId> {
    IRQ_TABLE.lock_irq()[irq as usize]
}

/// One bit per IDT vector: has `deliver` ever run for it? Read and set by the one-shot inside
/// `deliver`, below.
static VECTOR_SEEN: [portable_atomic::AtomicU64; 4] =
    [const { portable_atomic::AtomicU64::new(0) }; 4];

/// Deliver IRQ `irq` to the registered driver as an IPC message.
///
/// # Safety
/// Called from interrupt context with IF=0. The APIC EOI is sent unconditionally
/// at the end; missing the EOI would leave the IRQ line permanently masked.
pub unsafe fn deliver(irq: u8) {
    // FIRST delivery of each vector, logged once - generalised from the EHCI-only one-shot this
    // replaces, because the question it answered for the EHCI is the one now being asked of the xHCI
    // and there was no reason it should ever have been asked about one device only.
    //
    // It exists to split a failure nothing else in the kernel can split. When a driver reports no
    // interrupts, either the CPU never took the vector (not arriving - device, APIC or fabric) or it
    // took it and the notification did not reach the driver (routing). Disjoint fixes, identical
    // symptoms. On the T630 three theories were spent guessing between them - the IOMMU's IntCtl, a
    // masked MSI vector, and a destination APIC id that was never published. Two were wrong and the
    // third was a real defect that was not this machine's cause. All three guessed at the SENDING
    // end, because nothing observed the receiving end.
    //
    // Bounded: one line per distinct vector for the life of the boot, 256 maximum, in practice a
    // handful. `fetch_or` returns the previous word, so the test is one atomic.
    {
        use core::sync::atomic::Ordering;
        let w = (irq >> 6) as usize;
        let b = 1u64 << (irq & 63);
        if VECTOR_SEEN[w].fetch_or(b, Ordering::Relaxed) & b == 0 {
            let routed = IRQ_TABLE.lock_irq()[irq as usize].is_some();
            crate::kprintln!(
                "irq: FIRST delivery of vector {:#x} on core {} (routed: {})",
                irq, crate::task::scheduler::current_core_id(),
                if routed { "yes" } else { "NO - discarded" }
            );
        }
    }
    // For a level-triggered IOAPIC route (legacy INTx, e.g. the EHCI), mask the source now so
    // it does not re-fire while the userspace driver handles it (the line stays asserted until
    // the driver clears the device's interrupt status). The driver unmasks via the IrqUnmask
    // syscall after acking. No-op for edge/MSI vectors (the xHCI), which need no masking.
    crate::arch::imp::ioapic::mask_vector(irq);

    let endpoint = IRQ_TABLE.lock_irq()[irq as usize];
    if let Some(ep) = endpoint {
        // `irq-miss-test`: the first notification of each vector is skipped, as queue pressure would
        // skip it, so the retry below is exercised on every boot of a test build (backlog/90 D18).
        #[cfg(feature = "irq-miss-test")]
        {
            use core::sync::atomic::Ordering;
            static SKIPPED: [portable_atomic::AtomicU64; 4] =
                [const { portable_atomic::AtomicU64::new(0) }; 4];
            let b = 1u64 << (irq & 63);
            if SKIPPED[(irq >> 6) as usize].fetch_or(b, Ordering::Relaxed) & b == 0 {
                crate::kprintln!("irq-miss-test: skipping vector {:#x}'s first notification", irq);
                note_missed(irq);
                crate::arch::imp::interrupts::send_eoi();
                return;
            }
        }
        if !try_notify(irq, ep) {
            note_missed(irq);
        }
    }
    // EOI must fire unconditionally - even on discard and even on full queue.
    // If the APIC is not re-armed here, the IRQ line stays masked and the system hangs.
    crate::arch::imp::interrupts::send_eoi();
}

/// Send `irq`'s notification to `ep`, unless the queue is under pressure. True if it went out (or the
/// endpoint is gone, which leaves nothing to owe); false if it was skipped or dropped, and is owed.
fn try_notify(irq: u8, ep: EndpointId) -> bool {
    // COALESCE interrupt notifications, but only against QUEUE PRESSURE - never against the mere
    // presence of unrelated work.
    //
    // This read `endpoint_queue_depth(ep) == 0`, so ANY queued message suppressed the interrupt.
    // A driver whose endpoint carries more than interrupts - `xhci` receives block requests from
    // `block-driver` on the same endpoint - therefore lost IRQ notifications whenever it happened
    // to have a request pending. The comment claimed "the kernel re-notifies once the queue
    // drains"; there was NO such path. There is one now: a notification skipped here is OWED, and
    // `retry_missed` sends it from core 0's tick once the queue has room (backlog/90 D18).
    //
    // It is currently masked by those drivers ALSO polling their event ring every pass, which is
    // why it has never been seen. It becomes a lost keystroke - or a lost disk completion - the
    // moment a driver stops polling, which is exactly the direction xhci is moving.
    //
    // Half the queue is the reserve. Below it, always notify: a pending block request must never
    // cost an interrupt. At or above it, skip for now: that is a genuine storm and the queue is the
    // thing under pressure. The skip is recorded and retried on the tick, because a LEVEL line stays
    // masked until its driver hears of it - "the driver reads its ring on the next pass" was true only
    // of a driver that polls. So the
    // pathology this was written for (notifications piling up until `observe` shows a full queue
    // after a max-carnage interrupt storm) is still bounded, and the correctness hole is closed.
    //
    // (The EOI below still fires unconditionally, on every path.)
    const IRQ_NOTIFY_RESERVE: u8 = (crate::ipc::queue::QUEUE_DEPTH / 2) as u8;
    static IRQ_DROP_SAID: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
    if crate::ipc::routing::endpoint_queue_depth(ep) >= IRQ_NOTIFY_RESERVE {
        return false;
    }
    let msg = crate::ipc::message::Message::interrupt_event(irq);
    match crate::ipc::routing::enqueue_from_interrupt(ep, msg) {
        // wake_by_slot marks the receiver Ready and sends a WAKE_RECEIVER IPI
        // to its core if it lives on a different core than the one handling
        // this IRQ (§12.2 cross-core delivery path).
        crate::ipc::routing::Enqueue::Woke(receiver_slot) => {
            crate::task::scheduler::wake_by_slot(receiver_slot, 0);
            true
        }
        crate::ipc::routing::Enqueue::Queued
        | crate::ipc::routing::Enqueue::NoEndpoint => true,
        // Should be unreachable: the depth guard above reserves half the queue precisely so
        // this cannot happen. Said ONCE if it ever does, because a silently lost interrupt is
        // a driver that stops for no visible reason - and an impossible case that turns out to
        // be possible is worth hearing about the first time, not the hundredth.
        crate::ipc::routing::Enqueue::QueueFull => {
            if !IRQ_DROP_SAID.swap(true, core::sync::atomic::Ordering::Relaxed) {
                crate::kprintln!(
                    "kernel: IRQ {} to endpoint {} DROPPED - queue full despite the reserve; owed, and retried on the tick",
                    irq, ep.0);
            }
            false
        }
    }
}

/// One bit per vector: a notification was skipped or dropped and has not been sent since.
///
/// A level line is MASKED by `deliver` and opened again only by its driver's `IrqUnmask`, which the
/// driver calls after it handles the notification. So a notification that is skipped under queue
/// pressure, or dropped by a full queue, was a line masked FOREVER, with nothing said: the device
/// keeps it asserted, the controller holds it shut, and no later interrupt comes to try again
/// (backlog/90 D18). The bit makes the notification owed rather than lost, and `retry_missed` pays it
/// on core 0's tick once the queue has room. For an edge or MSI vector it is one extra notification at
/// most, which the driver treats as any other: it reads its ring and finds whatever is there.
static NOTIFY_MISSED: [portable_atomic::AtomicU64; 4] =
    [const { portable_atomic::AtomicU64::new(0) }; 4];

fn note_missed(irq: u8) {
    use core::sync::atomic::Ordering;
    NOTIFY_MISSED[(irq >> 6) as usize].fetch_or(1u64 << (irq & 63), Ordering::Relaxed);
}

/// Send every owed notification whose queue now has room. Called on core 0's tick, with interrupts
/// masked as `deliver` runs. Bounded: four loads when nothing is owed, and at most one notification per
/// owed vector per tick. A vector whose route has gone is dropped - its owner is dead, and the next
/// holder's `register` opens the line itself.
pub fn retry_missed() {
    use core::sync::atomic::Ordering;
    for w in 0..NOTIFY_MISSED.len() {
        // A load first: the common case is nothing owed, and a swap would write the line every tick.
        if NOTIFY_MISSED[w].load(Ordering::Relaxed) == 0 {
            continue;
        }
        let mut bits = NOTIFY_MISSED[w].swap(0, Ordering::Relaxed);
        while bits != 0 {
            let b = bits.trailing_zeros();
            bits &= bits - 1;
            let irq = ((w as u32) << 6 | b) as u8;
            let endpoint = IRQ_TABLE.lock_irq()[irq as usize];
            if let Some(ep) = endpoint {
                if try_notify(irq, ep) {
                    // Said once a boot: a skip is the reserve doing its job, and this is the line that
                    // shows the debt being paid rather than a driver going quiet for no visible reason.
                    static PAID_SAID: core::sync::atomic::AtomicBool =
                        core::sync::atomic::AtomicBool::new(false);
                    if !PAID_SAID.swap(true, Ordering::Relaxed) {
                        crate::kprintln!(
                            "irq: vector {:#x}'s notification was skipped under queue pressure and sent on the tick (further ones are not logged)",
                            irq);
                    }
                } else {
                    note_missed(irq);
                }
            }
        }
    }
}

