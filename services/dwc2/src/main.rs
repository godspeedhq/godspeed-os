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

mod chan;
mod core;
mod enumerate;
mod hid;
mod hub;
mod regs;

use godspeed_sdk::ServiceContext;

/// How often to report, in ms. Long enough not to be noise, short enough that a boot answers the
/// question without waiting around.
const REPORT_MS: u64 = 5_000;

/// Bound on messages retired per drain. The endpoint queue is 16 deep, so this is a storm detector
/// rather than a throttle - the same bound, for the same reason, as the one the `xhci` service
/// needed after an unbounded drain was found there (userspace audit A8-1). A sender enqueuing as
/// fast as we dequeue would otherwise keep this loop running forever.
const MSG_DRAIN_MAX: u32 = 256;

/// How long to wait before re-arming the USB line after an interrupt.
///
/// The skeleton cannot clear the device condition, so the line is still asserted when it unmasks and
/// the next interrupt is immediate. One second turns that into a metronome instead of a livelock:
/// enough to watch the count climb over a fifteen-second boot, slow enough that the core is idle
/// between them.
const REARM_MS: u64 = 1_000;

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

    // SLICE 1a: bring the controller up ourselves.
    //
    // The kernel driver has stood down (it gates on who holds the vector), so from here the service
    // owns this controller. Everything below is reported rather than assumed: a driver that cannot
    // reach or reset its hardware must say which step failed, not present as a driver that found no
    // devices.
    if let Some(m) = ctx.mmio() {
        if core::identify(&ctx, &m).is_some() {
            let ok = core::reset_and_host_mode(&ctx, &m);
            ctx.log_fmt(format_args!(
                "dwc2-svc: core bring-up {}", if ok { "OK" } else { "COMPLETED WITH WARNINGS (see above)" }));
            match core::port_bring_up(&ctx, &m) {
                Some(hprt) => ctx.log_fmt(format_args!(
                    "dwc2-svc: root port HPRT={:#010x} connected={} enabled={} speed={}",
                    hprt,
                    hprt & regs::HPRT_PRTCONNSTS != 0,
                    hprt & regs::HPRT_PRTENA != 0,
                    core::speed_name(hprt))),
                None => ctx.log("dwc2-svc: root port bring-up FAILED - no device will enumerate"),
            }
            // SLICE 1b: ask the attached device who it is.
            //
            // A device descriptor read is the smallest thing that exercises the whole transfer path -
            // channel programming, a DMA the controller performs against OUR granted arena, the bus
            // alias translation, and all three control stages. If the VID/PID come back matching what
            // the kernel driver reports, every one of those is right; if they do not, the failing
            // stage has already named itself.
            //
            // Address 0, MPS 8: the mandatory state of a just-reset device before SET_ADDRESS. 8 is
            // the safe minimum every device must accept for its first descriptor read.
            // SLICE 1c: address the root device and identify it.
            //
            // The acceptance test is a COMPARISON, not a plausible-looking line: the VID/PID and port
            // count must match what the in-kernel driver reports for the same hardware. Anything
            // else means the transfers worked and the parsing did not, which a lone "looks like a
            // hub" would hide.
            if let Some(d) = ctx.dma_region() {
                match enumerate::root_device(&ctx, &m, &d) {
                    Some(dev) => {
                        ctx.log_fmt(format_args!(
                            "dwc2-svc: ENUMERATION OK - {:04x}:{:04x} class={:#04x} ports={}",
                            dev.vid, dev.pid, dev.class,
                            match dev.hub_ports { Some(n) => n, None => 0 }));
                        // SLICE 1c-ii: survey the hub's downstream ports.
                        //
                        // Every request here is addressed to the HUB, which is direct - so this
                        // still needs no split transactions. What it produces is the map that says
                        // WHERE splits will be needed, which is the last piece of Slice 1.
                        if let Some(n) = dev.hub_ports {
                            hub::survey(&ctx, &m, &d, &dev.target, n);
                            // SLICE 1c-iii: reach a device BEHIND the hub, through a split.
                            //
                            // The first port reporting a device is enough to prove the path. Doing
                            // all four would prove the same thing four times and take four times as
                            // long to read when it fails.
                            // TRY EVERY CONNECTED PORT, not just the first.
                            //
                            // This is the discriminator for "hub or device?". There are four devices
                            // down there from four different vendors at two different speeds, and
                            // four different devices do not fail identically. So:
                            //
                            //   all four STALL the same way  -> not the device. The fault is ours or
                            //                                   the hub's, and it is systematic.
                            //   some work, some do not       -> device- or speed-specific, and the
                            //                                   ones that differ say which property
                            //                                   matters (port 4 is the only LOW-speed
                            //                                   device on this board).
                            //
                            // Costs three extra transfers on a path that runs once. Stopping at the
                            // first failure throws away the comparison that answers the question.
                            for p in 1..=n {
                                if let Some(st) = hub::port_status(&ctx, &m, &d, &dev.target, p) {
                                    if st.connected() {
                                        // SLICE 2: bind a boot keyboard if this is one.
                                        //
                                        // `bind` returns None for a device that is not a keyboard,
                                        // which is an ordinary outcome on three of these four ports
                                        // and must not be reported as a failure.
                                        if let Some((_, _, _, dt, dsplt)) =
                                            hub::enumerate_downstream(&ctx, &m, &d, &dev.target, p)
                                        {
                                            if let Some(k) = hid::bind(&ctx, &m, &d, &dt, dsplt) {
                                                let _ = k;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None => ctx.log("dwc2-svc: enumeration FAILED - see the step above"),
                }
            }
        }
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
        //
        // COUNT WHAT THIS RETURNS. It was `let _ = ...`, and `recv_timeout` does not peek - it
        // CONSUMES the message. So the one interrupt that arrived was received here and thrown away,
        // `try_recv` below then found an empty queue, and the service reported `0 USB IRQ(s)` on a
        // boot where the kernel's own `deliver() vector=0x29` line proves the interrupt had been
        // delivered. A discarded return value, reporting the opposite of what happened.
        let mut first = ctx.recv_timeout(ctx.duration_cycles(REPORT_MS));

        let mut drained = 0u32;
        while let Some(m) = first.take().or_else(|| ctx.try_recv()) {
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
                // THROTTLED RE-ARM: sleep, THEN unmask.
                //
                // Not unmasking at all proved DELIVERY (one interrupt, `*** DELIVERED ***`). It could
                // not prove REPEAT delivery, and Phase 3 rests on repeat: a driver that receives one
                // interrupt and never another is no driver. So the count has to climb.
                //
                // Unmasking IMMEDIATELY is what wedged core 0 three boots ago. DWC2's line is
                // level-triggered and stays asserted until the DEVICE condition is cleared; a skeleton
                // clears nothing, so an instant unmask re-asserts inside the handler and the core never
                // leaves it. The sleep is what makes the difference - the task is BLOCKED, not
                // spinning, so the core is free and the rate is bounded to one interrupt per
                // REARM_MS by construction rather than by hope.
                //
                // A real driver will not need this: it earns its unmask by servicing the device, which
                // deasserts the line. This is the cheapest way to ask "can it happen twice?" without
                // writing the driver first.
                ctx.sleep(ctx.duration_cycles(REARM_MS));
                ctx.irq_unmask(USB_VECTOR);

                // (Retained for the record - the reasoning that made the previous build a proof of
                // delivery rather than a livelock.)
                // DO NOT UNMASK IMMEDIATELY. This is the whole difference between a proof and a livelock.
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
            // Say what the number MEANS, not just what it is. The question this build exists to
            // answer is whether interrupts arrive more than once, and a reader should not have to
            // diff two log lines to find out.
            let verdict = if irqs >= 2 {
                "REPEAT DELIVERY WORKS"
            } else if irqs == 1 {
                "one only so far - not yet repeat"
            } else {
                "none yet"
            };
            ctx.log_fmt(format_args!(
                "dwc2-svc: alive - {} USB IRQ(s), {} message(s) total ({})", irqs, msgs, verdict));
        }
    }
}
