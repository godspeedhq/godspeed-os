//! `dwc2` - the Raspberry Pi 2's USB host controller, as a userspace service.
//!
//! **This is a SKELETON.** It holds the hardware a driver needs - the DWC2 register window, a DMA
//! arena, and the USB interrupt - and does nothing with it but report. It exists to answer one
//! question before 3981 lines of driver are moved onto it:
//!
//!   *does a device interrupt actually arrive in userspace on arm32?*
//!
//! `CLAUDE.md` §6.4 says it cannot ("ARM does not yet route device IRQs to userspace"), and Phase 1
//! established that the claim describes an unwritten branch rather than the hardware. This service is
//! the proof, and it is deliberately cheap: if the answer is no, one boot says so, and nothing has
//! been ported yet. See `docs/arm32-usb-userspace.md`.
//!
//! **Claiming the vector takes the controller away from the kernel.** `arm_irq_dispatch` routes to
//! whoever registered for `USB_VECTOR`, falling back to the in-kernel stack only when nobody has. The
//! moment this service is spawned it IS the registrant, so the in-kernel driver stops receiving
//! interrupts. That is intended - it is the whole point of the phase - but it means USB is expected
//! to be degraded on a boot with this service running, and that is not a regression to chase.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

/// How often to report, in ms. Long enough not to be noise, short enough that a boot answers the
/// question without waiting around.
const REPORT_MS: u64 = 5_000;

/// Bound on messages retired per drain. The endpoint queue is 16 deep, so this is a storm detector
/// rather than a throttle - the same bound, for the same reason, as the one the `xhci` service
/// needed after an unbounded drain was found there (userspace audit A8-1). A sender enqueuing as
/// fast as we dequeue would otherwise keep this loop running forever.
const MSG_DRAIN_MAX: u32 = 256;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // PREFIX `dwc2-svc:`, not `dwc2:`.
    //
    // The IN-KERNEL driver already owns the `dwc2:` prefix and prints heavily - hub ports, MSC
    // capacity, FUA. With both writing to one serial console, an identical prefix makes the log
    // unreadable exactly when it matters: the whole question this service exists to answer is which
    // of the two is receiving the interrupt, and a shared prefix would hide that.
    ctx.log("dwc2-svc: starting (SKELETON - proves the IRQ path, drives nothing)");

    // The two hardware grants, reported rather than assumed. A driver that cannot reach its
    // registers is not a degraded driver, it is not a driver at all, and the boot log should say
    // which of the two is missing rather than leaving the reader to infer it from silence.
    match ctx.mmio() {
        Some(m) => ctx.log_fmt(format_args!(
            "dwc2-svc: MMIO window granted - {} bytes (DWC2 core registers)", m.len())),
        None => ctx.log("dwc2-svc: NO MMIO window - the kernel granted none (hw_device mismatch?)"),
    }
    match ctx.dma_region() {
        Some(d) => ctx.log_fmt(format_args!(
            "dwc2-svc: DMA arena granted - {} bytes at phys {:#x}", d.len(), d.phys_at(0))),
        None => ctx.log("dwc2-svc: NO DMA arena - the kernel granted none"),
    }

    // Interrupts arrive as ordinary IPC on this service's receive endpoint: the kernel's neutral
    // router enqueues a one-byte message carrying the vector. That is the same delivery the `xhci`
    // service receives on the Pi 4, which is what makes this test meaningful - it exercises the
    // shared path, not an arm32-only shim.
    const USB_VECTOR: u8 = 0x29;
    let mut irqs: u64 = 0;
    let mut msgs: u64 = 0;
    let mut last_report = ctx.read_tsc();
    let mut first_logged = false;

    loop {
        // Block until something arrives, with a deadline so the report still lands on a quiet
        // machine. A service that only speaks when the hardware speaks cannot report that the
        // hardware is silent - which is the single most interesting outcome here.
        let _ = ctx.recv_timeout(ctx.duration_cycles(REPORT_MS));

        let mut drained = 0u32;
        while let Some(m) = ctx.try_recv() {
            drained += 1;
            if drained >= MSG_DRAIN_MAX {
                ctx.log("dwc2-svc: message drain hit its bound - a sender is enqueuing as fast as we retire (storm?)");
                break;
            }
            msgs = msgs.wrapping_add(1);
            let p = m.payload_bytes();
            if p.len() == 1 && p[0] == USB_VECTOR {
                irqs = irqs.wrapping_add(1);
                if !first_logged {
                    first_logged = true;
                    // THE LINE THIS SERVICE EXISTS TO PRINT.
                    ctx.log("dwc2-svc: *** USB INTERRUPT DELIVERED TO USERSPACE *** - arm32 device IRQ routing works");
                }
                // DO NOT UNMASK. This is the whole difference between a proof and a livelock.
                //
                // The first version unmasked here, reasoning that the count would then "climb fast".
                // It does not climb - it livelocks. DWC2's interrupt is LEVEL-triggered: it stays
                // asserted until the DEVICE condition is cleared, and a skeleton that drives nothing
                // clears nothing. So unmask re-asserts instantly, the core re-enters the handler, and
                // core 0 makes no forward progress at all:
                //
                //   KERNEL PANIC: LIVENESS WEDGE: core 0 made NO progress for 10007781 ticks;
                //                 last running task slot 8; detected by core 3.
                //
                // The watchdog was right and the design was wrong. A real driver earns its unmask by
                // servicing the device first; this one has not earned it.
                //
                // Leaving the line masked costs nothing HERE, because one delivered interrupt is the
                // entire question: it proves the arm32 route reaches userspace. Counting to a hundred
                // would prove nothing further and cost the machine.
            }
        }

        if ctx.read_tsc().wrapping_sub(last_report) > ctx.duration_cycles(REPORT_MS) {
            last_report = ctx.read_tsc();
            // Report BOTH counts, because they answer different questions: `irqs` says the routing
            // works, `msgs` says whether anything else is arriving on this endpoint. Zero irqs with
            // nonzero msgs would mean the endpoint is live and the ROUTE is not - a different fault
            // from silence, and one that silence alone could not distinguish.
            ctx.log_fmt(format_args!(
                "dwc2-svc: alive - {} USB IRQ(s), {} message(s) total", irqs, msgs));
        }
    }
}
