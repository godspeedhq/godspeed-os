//! `dwc2` - the Raspberry Pi 2's USB host controller, as a userspace service.
//!
//! This began as a skeleton that proved the IRQ path and drove nothing. It now drives the controller:
//! it holds the hardware a driver needs - the DWC2 register window, a DMA
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
mod msc;
mod net;
mod regs;

use godspeed_sdk::ServiceContext;

/// How often to report, in ms. Long enough not to be noise, short enough that a boot answers the
/// question without waiting around.
const REPORT_MS: u64 = 5_000;
/// How often the keyboard poll reports it is alive.
const HEARTBEAT_MS: u64 = 15_000;

/// Bound on messages retired per drain. The endpoint queue is 16 deep, so this is a storm detector
/// rather than a throttle - the same bound, for the same reason, as the one the `xhci` service
/// needed after an unbounded drain was found there (userspace audit A8-1). A sender enqueuing as
/// fast as we dequeue would otherwise keep this loop running forever.
const MSG_DRAIN_MAX: u32 = 256;

/// How long one serve pass may hold the thread before the keyboard is polled again.
///
/// This service is single-threaded: it serves block and frame IPC, and it polls the keyboard's
/// interrupt endpoint, on the same pass. Whatever the serving half spends, the keyboard waits.
/// Hardware showed the extreme of that - a storm of slowly-failing transmits held the thread for
/// ninety seconds, the keyboard went dead, `q` never reached the shell, and no heartbeat was printed
/// because a heartbeat only happens on an idle pass, so the busiest service looked like the deadest.
///
/// 20 ms is two keyboard poll intervals: late enough to be worth serving a batch, short enough that
/// a missed report is the worst case rather than a dead input path.
const PASS_BUDGET_MS: u64 = 20;

/// How long to wait before re-arming the USB line after an interrupt.
///
/// The skeleton cannot clear the device condition, so the line is still asserted when it unmasks and
/// the next interrupt is immediate. One second turns that into a metronome instead of a livelock:
/// enough to watch the count climb over a fifteen-second boot, slow enough that the core is idle
/// between them.
const REARM_MS: u64 = 1_000;

/// Route one request to the block server or the frame server.
///
/// ONE endpoint carries both protocols, so the opcode decides. The reply cap is taken HERE, once, so
/// neither server can take it twice or forget to - and a request with none is dropped loudly, because
/// it cannot be answered and silence would leave the client to time out against a clean log.

/// Answer a block request when there is NO DISK, instead of consuming the message and going quiet.
///
/// `recv_timeout` CONSUMES. The two call sites below used to take a message and then drop it whenever
/// `disk` was None, so a client blocked in request/reply waited forever for an answer that was never
/// coming. `block-driver` asks for capacity as the very first thing it does, so with no stick in the
/// machine it hung before printing a single line - and `fs` hung behind it - which reads as a dead
/// service rather than an empty drive bay.
///
/// The net path in `dispatch` already got this right ("a net request with no NIC bound is answered,
/// not ignored"). This is the same rule for storage: a missing dependency must RETURN, loudly, never
/// hang. A short/failed reply is exactly what `sectors_now` treats as "no disk", which is the truth.
fn answer_no_disk(ctx: &ServiceContext, capless: &mut bool) {
    match ctx.take_pending_cap() {
        Some(reply) => {
            // STATUS_ERR, and one byte only: shorter than any real reply, so a reader that checks
            // the status and a reader that checks the length both take their error path.
            let _ = ctx.try_send_by_handle(
                reply, &godspeed_sdk::Message::from_bytes(&[crate::msc::STATUS_ERR]));
            ctx.remove_cap(reply);
        }
        None => {
            if !*capless {
                *capless = true;
                ctx.log("dwc2-svc: request had no reply cap - dropping (cannot answer without one)");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    ctx: &ServiceContext, m: &godspeed_sdk::Mmio, d: &godspeed_sdk::Dma,
    dt: &chan::Target, dk: &mut msc::Disk, nic: Option<&mut (net::Nic, chan::Target)>,
    msg: &godspeed_sdk::Message, sectors: u64, capless: &mut bool,
) -> bool {
    let p = msg.payload_bytes();
    if p.is_empty() {
        return false;
    }
    let reply = match ctx.take_pending_cap() {
        Some(c) => c,
        None => {
            if !*capless {
                *capless = true;
                ctx.log("dwc2-svc: request had no reply cap - dropping (cannot answer without one)");
            }
            return true;
        }
    };
    if p[0] >= net::OP_NET_INFO {
        if let Some((n, nt)) = nic {
            return net::serve(ctx, m, d, nt, n, msg, reply);
        }
        // A net request with no NIC bound is answered, not ignored: the client is blocked waiting,
        // and an empty reply degrades it where silence would hang it.
        let _ = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&[0u8]));
        ctx.remove_cap(reply);
        return true;
    }
    msc::serve(ctx, m, d, dt, dk, msg, sectors, reply, capless)
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // PREFIX `dwc2-svc:`, not `dwc2:`.
    //
    // The IN-KERNEL driver already owns the `dwc2:` prefix and prints heavily - hub ports, MSC
    // capacity, FUA. With both writing to one serial console, an identical prefix makes the log
    // unreadable exactly when it matters: the whole question this service exists to answer is which
    // of the two is receiving the interrupt, and a shared prefix would hide that.
    ctx.log("dwc2-svc: starting - USB host (hub, keyboard, storage, ethernet)");

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
    // Declared out here so the poll loop below can see it: the bring-up borrows `mmio`, and the
    // keyboard it finds has to outlive that borrow.
    let mut kbd: Option<(hid::Keyboard, chan::Target, u32)> = None;
    let mut disk: Option<(msc::Disk, chan::Target, u64)> = None;
    let mut nic: Option<(net::Nic, chan::Target)> = None;
    // Owned by the enumeration loop, not a static (C6-1). The bus hands out addresses in sequence and
    // this is the thing that runs the bus, so this is where the counter belongs.
    let mut next_addr: u8 = hub::FIRST_DOWNSTREAM_ADDR;
    // Whether the root hub has a TT per port or one shared translator - it decides where a TT remedy
    // must be addressed (see `hub::tt_windex`). Defaults to the single-TT reading, which is the
    // conservative one: a request aimed at TT 1 on a multi-TT hub is at worst the wrong translator,
    // whereas assuming per-port translators on a single-TT hub sends every remedy into a void.
    let mut hub_multi_tt = false;
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
                        hub_multi_tt = dev.hub_multi_tt;
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
                            let (port_status, n_status) = hub::survey(&ctx, &m, &d, &dev.target, n);
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
                            // LOW-SPEED (the keyboard) FIRST - see `hub::visit_order`. A storm
                            // restarts this service faster than a full bring-up completes, so a
                            // partial enumeration is the normal case during chaos, and whatever is
                            // last is what the operator loses. Storage and network reacquire and
                            // retry; a keyboard has nobody to type its retry.
                            let (order, n_order) = hub::visit_order(&port_status, n_status);
                            for &p in order[..n_order].iter() {
                                if let Some(st) = hub::port_status(&ctx, &m, &d, &dev.target, p) {
                                    if st.connected() {
                                        // SLICE 2: bind a boot keyboard if this is one.
                                        //
                                        // `bind` returns None for a device that is not a keyboard,
                                        // which is an ordinary outcome on three of these four ports
                                        // and must not be reported as a failure.
                                        if let Some((dvid, dpid, _, dt, dsplt)) =
                                            hub::enumerate_downstream(&ctx, &m, &d, &dev.target, p, &mut next_addr)
                                        {
                                            // A device is one thing or the other, so try the
                                            // keyboard binding first and the disk only if that
                                            // declined. Both return None for "not mine", which is
                                            // the ordinary answer on most ports and is not logged.
                                            if let Some(k) = hid::bind(&ctx, &m, &d, &dt, dsplt) {
                                                // TELL THE USER INPUT IS LIVE, once, by making the
                                                // shell reprint its prompt. The prompt appears about
                                                // 2.5 s before this - deliberately, so it never waits
                                                // on hardware bring-up - and a prompt you cannot type
                                                // at looks like a broken machine.
                                                //
                                                // Only on the FIRST bind. This lived inside `bind`,
                                                // which the recovery path also calls, so every
                                                // re-enumeration printed another prompt: four binds in
                                                // one boot gave four `gsh>` lines. A re-bind is not
                                                // news to someone already typing; the first one is.
                                                // NO CONSOLE SIGNAL HERE. Binding the
                                                // keyboard used to push a newline so the shell
                                                // reprinted `gsh>` - a marker that input was live,
                                                // since the prompt appears about 2.5 s earlier and a
                                                // prompt you cannot type at looks broken.
                                                //
                                                // It is removed because the cure was worse than the
                                                // complaint: it produced a second prompt at every
                                                // boot, which reads as the shell glitching. The fact
                                                // is already in the log ("BOOT KEYBOARD bound"), and
                                                // an operator who is typing finds out by typing.
                                                //
                                                // Worth remembering if this is ever revisited: the
                                                // right signal would be one that does not imitate a
                                                // shell event, because anything the shell also emits
                                                // is indistinguishable from the shell misbehaving.
                                                kbd = Some((k, dt, dsplt));
                                            } else if dvid == 0x0424 && dpid == 0xec00 {
                                                // The LAN9514's ethernet function. Matched by VID:PID
                                                // rather than by class, because it reports 0xff
                                                // (vendor-specific) - there is no class to match on,
                                                // and guessing from "has two bulk endpoints" would
                                                // also match the disk.
                                                if let Some(n) = net::bind(&ctx, &m, &d, &dt) {
                                                    nic = Some((n, dt));
                                                }
                                            } else if let Some(mut dk) = msc::bind(&ctx, &m, &d, &dt, dsplt) {
                                                // Prove the bulk path the way the kernel driver does:
                                                // ask the device its size, then read block 0. Capacity
                                                // alone would prove the command path; reading a block
                                                // proves the DATA path, which is where a short
                                                // transfer would silently corrupt.
                                                match msc::read_capacity(&ctx, &m, &d, &dt, &mut dk) {
                                                    Some((sectors, block)) => {
                                                        ctx.log_fmt(format_args!(
                                                            "dwc2-svc: USB DISK - {} sectors of {} B ({} MiB)",
                                                            sectors, block,
                                                            sectors.saturating_mul(block as u64) / (1024 * 1024)));
                                                        // Read BEFORE handing `dk` to `disk`. The
                                                        // previous order moved it in and then borrowed
                                                        // it straight back out with `unwrap()` - an
                                                        // unwrap that existed only because of the
                                                        // ordering, in the one service the Pi 2's
                                                        // storage, keyboard and network all depend on.
                                                        // Provably safe today; a machine-halt the first
                                                        // time someone moves the assignment.
                                                        if msc::read_block(&ctx, &m, &d, &dt, &mut dk, 0, block) {
                                                            let b0 = d.read8(msc::DATA_OFF);
                                                            let b1 = d.read8(msc::DATA_OFF + 1);
                                                            let b2 = d.read8(msc::DATA_OFF + 2);
                                                            let b3 = d.read8(msc::DATA_OFF + 3);
                                                            ctx.log_fmt(format_args!(
                                                                "dwc2-svc: sector 0 first bytes {:02x} {:02x} {:02x} {:02x}",
                                                                b0, b1, b2, b3));
                                                        } else {
                                                            ctx.log("dwc2-svc: sector 0 read FAILED");
                                                        }
                                                        disk = Some((dk, dt, sectors));
                                                    }
                                                    None => ctx.log("dwc2-svc: READ CAPACITY FAILED"),
                                                }
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
    let boot_tsc = ctx.read_tsc();
    let mut quiet_swept = false;
    let mut first_logged = false;

    // SLICE 2b: poll the keyboard, if one was bound.
    //
    // A periodic split is ONE attempt per poll and reschedules on any failure, so this loop is the
    // schedule: at the keyboard's requested interval it asks once, and an idle keyboard's NAK is a
    // positive "nothing happened" rather than an error. That structure is what makes a
    // microframe-scheduled transfer safe in a PREEMPTIBLE task - being descheduled at the wrong
    // moment costs one attempt, not correctness.
    // ONE LOOP, BOTH JOBS. The keyboard is polled and block requests are served from the same pass,
    // because they share one controller and one set of channels - two loops would have to exclude
    // each other anyway, and the exclusion would be the bug.
    //
    // Serving comes FIRST in the pass. A block request is a client blocked waiting on an answer,
    // where a keyboard poll that slips a period costs nothing a human can perceive.
    if let (Some(m), Some(d)) = (ctx.mmio(), ctx.dma_region()) {
        let mut state = hid::KeyState::new(&ctx);
        // Consecutive recoveries with no healthy polling in between, and whether we have already
        // announced giving up. Both reset the moment the keyboard actually works again.
        let mut recoveries_in_a_row = 0u32;
        let mut recovery_gave_up = false;
        let mut gave_up_at = 0u64;
        // When the hub's transaction translator was last reset. Rate-limits the rung so a fault it
        // cannot fix falls through to the port reset instead of looping on it.
        let mut tt_reset_at = 0u64;
        // Set when the keyboard has been re-bound but has not yet delivered a report.
        //
        // The driver used to announce "RECOVERED by re-enumeration" the instant the device re-bound.
        // Twelve of those lines were printed in one minute while not a single keystroke arrived: the
        // bind proves the control path works, which was never the broken part. Recovery is claimed
        // on EVIDENCE now - a delivered report - so the log cannot say the repair worked while the
        // operator is looking at a dead keyboard.
        let mut awaiting_report = false;
        /// How long to leave a hopeless keyboard alone before trying once more. Long enough that a
        /// dead device costs nothing, short enough that a transient storm is not a permanent loss.
        const RECOVERY_BACKOFF_MS: u64 = 10_000;
        let mut reports: u64 = 0;
        let mut last_beat = ctx.read_tsc();
        // The interval the DEVICE asked for, floored to something a service can actually schedule.
        // `cycles_to_ticks` clamps sub-quantum sleeps to one 10 ms tick, so anything finer is
        // fiction here - and saying so beats pretending to honour a 1 ms interval.
        // The interval the DEVICE asked for, floored to what a service can actually schedule, and 10
        // when there is no keyboard - the loop still has block requests to serve.
        let period_ms = kbd.as_ref().map(|(k, _, _)| (k.interval as u64).max(10)).unwrap_or(10);
        ctx.log_fmt(format_args!("dwc2-svc: serving block I/O; keyboard poll every {} ms", period_ms));
        let mut capless = false;
        let mut served: u64 = 0;
        // WHERE THE TIME GOES, split by segment - and how many PASSES it took.
        //
        // The per-stage busy counters came back all zero, so the stall is not in BOT: transfers are
        // not retrying, not failing, and not recovering. Yet throughput collapses from 19 requests a
        // second to one in forty-four, which is something ACCUMULATING rather than a slow device.
        //
        // Counting passes as well as requests is the part that matters. "1 request in 44 seconds"
        // could be ONE pass taking 44 s or 4400 passes each serving nothing, and those are completely
        // different faults - the current log cannot tell them apart, and every guess at the cause
        // implicitly assumes one of them.
        //
        // `read_tsc` measures WALL time, so a segment that blocks reads the same as one that spins.
        // That is the right unit here: the question is where the pass goes, not who is burning CPU.
        let mut passes: u64 = 0;
        let mut seg_serve: u64 = 0;
        let mut seg_kbd: u64 = 0;
        let mut seg_sleep: u64 = 0;
        loop {
            passes = passes.wrapping_add(1);
            let t_pass = ctx.read_tsc();
            let mut served_this_pass = 0u32;
            // Drain block requests, BOUNDED - the endpoint queue is 16 deep, so this is a storm
            // detector rather than a throttle (the same bound the xhci service needed after an
            // unbounded drain was found there).
            // With NO DISK this used to skip the drain entirely, which is its own failure: requests
            // then sat in the 16-deep endpoint queue until it filled, and every later sender blocked
            // on a full queue instead of being told there is no disk. Drain either way, and answer
            // either way.
            if disk.is_none() {
                let mut drained = 0u32;
                while ctx.try_recv().is_some() {
                    drained += 1;
                    answer_no_disk(&ctx, &mut capless);
                    if drained >= MSG_DRAIN_MAX {
                        ctx.log("dwc2-svc: no-disk drain hit its bound - a sender is enqueuing as fast as we retire");
                        break;
                    }
                }
            }
            if let Some((dk, dt, sectors)) = disk.as_mut() {
                let mut drained = 0u32;
                // TIME-BOUND THE DRAIN, not just its length - and test it BEFORE taking a message.
                //
                // The keyboard is polled once per pass, after this loop, and its endpoint has a 10 ms
                // interval, so however long this loop runs is how late the next keystroke is
                // collected. A COUNT cannot bound that: sixteen queued requests are trivial when each
                // is a sector read and catastrophic when each is a transmit that fails slowly, which
                // is exactly what a NIC with a stuck receive path produces.
                //
                // The check sits here, not after `try_recv`, and the difference matters. `try_recv`
                // CONSUMES: a budget tested after it would leave a taken request that must be answered
                // with an error, which would report "this failed" about a request that was merely
                // unlucky in its timing - and for a NET request it is not even the right shape of
                // reply. Declining to TAKE work leaves it in the endpoint queue for the next pass,
                // which is the only version of "pick it up later" that is true.
                //
                // What this cannot bound alone is a SINGLE slow operation, which is why the transmit
                // budget was cut to match. The two together bound the pass.
                while ctx.read_tsc().wrapping_sub(t_pass) <= ctx.duration_cycles(PASS_BUDGET_MS) {
                    let msg = match ctx.try_recv() {
                        Some(m) => m,
                        None => break,
                    };
                    drained += 1;
                    if drained >= MSG_DRAIN_MAX {
                        // ANSWER THIS ONE BEFORE BREAKING. `try_recv` above already CONSUMED it, so
                        // breaking here retired a request and threw it away - and its owner
                        // (block-driver, then fs, then the shell) waited forever for a reply that was
                        // never coming. The no-disk drain twenty lines up gets this right; this arm
                        // was the storm bound, and a storm is exactly when a dropped reply is least
                        // likely to be noticed.
                        answer_no_disk(&ctx, &mut capless);
                        ctx.log("dwc2-svc: block drain hit its bound - a sender is enqueuing as fast as we retire");
                        break;
                    }
                    if dispatch(&ctx, &m, &d, dt, dk, nic.as_mut(), &msg, *sectors, &mut capless) {
                        served = served.wrapping_add(1);
                        served_this_pass += 1;
                    }
                }
            }
            let t_kbd = ctx.read_tsc();
            seg_serve = seg_serve.wrapping_add(t_kbd.wrapping_sub(t_pass));
            if let Some((k, kt, ksplt)) = kbd.as_ref() {
            // AN ENDPOINT STUCK IN XACTERR IS RE-ENUMERATED, not polled forever.
            //
            // Measured at the moment the keyboard died: 1512 consecutive XACTERR, zero data, zero
            // NAK. Clear_TT_Buffer had already fired and been accepted, so the transaction translator
            // was not the thing stuck this time - the DEVICE's endpoint was, and no number of further
            // transfers changes that. Enumeration ran once at startup and there was no path back, so
            // the keyboard stayed dead for the life of the service. That is the actual gap.
            //
            // Recovery is the same sequence that bound it in the first place: reset its hub port
            // (which returns the device to address 0 and clears its endpoint state, toggles
            // included), re-address it, and re-bind. It touches ONLY that port, so storage and the
            // NIC on their own ports are undisturbed.
            //
            // Bounded and loud: ~3 s of solid errors triggers one attempt, and the attempt reports
            // its own outcome. If it fails the run restarts, so a permanently absent keyboard costs
            // one recovery attempt every few seconds and says so, rather than a silent error storm.
            // TWO THRESHOLDS, because a cold error storm and one that follows a failed TT clear are
            // not the same evidence.
            //
            // Measured across three wedges on hardware, every one of them ran: TT wedged -> clear
            // accepted -> STILL broken -> re-enumerate -> recovered. The clear has never once fixed
            // it on its own. So after a clear we already know the endpoint - not the translator - is
            // the thing stuck, and spending the full cold budget re-proving that was three of the
            // roughly four seconds the operator sees as a pause. The repair itself takes 0.31 s.
            //
            // Cold, with no clear behind it, the generous budget stays: ordinary XACTERR noise on a
            // low-speed device behind a translator must never trigger a port reset.
            let recently_cleared = state.cleared_at != 0
                && ctx.read_tsc().wrapping_sub(state.cleared_at) < ctx.duration_cycles(2000);
            // 30 after a failed clear, not 10. Ten was part of the same regression: paired with a
            // NYET threshold below the noise floor it turned every ordinary error run into a port
            // reset, 394 times over. Thirty (~300 ms) still shortens the pause materially against the
            // cold 300, without treating routine XACTERR noise on a low-speed device behind a
            // translator as a stuck endpoint.
            let stuck_at = if recently_cleared { 30 } else { 300 };
            // A NYET STORM REACHES THIS RUNG TOO.
            //
            // The condition below is an XACTERR count, so a wedge that produces only NYET could never
            // get here: it looped on the TT clear instead, three hundred times, while the keyboard sat
            // at "0 data, 0 nak, 1497 nyet" - the remedy running continuously and none of it working.
            //
            // Six clears with no report in between is the same evidence an XACTERR run is: the device
            // is not delivering and the cheaper rungs have had their turn. `hid` resets the translator
            // at three; if six go by, the translator was not it either, and the port gets reset.
            const CLEARS_STUCK: u32 = 6;
            // A PORT RESET RUNS AT MOST ONCE EVERY REENUM_MIN_MS, whatever any counter says.
            //
            // The reset itself takes ~310 ms and the device needs time afterwards to settle, so a
            // remedy driven faster than that cannot work by construction - it interrupts its own
            // recovery. Hardware showed the loop precisely: re-enumerate, wedge, re-enumerate, every
            // 500 ms without end. Three attempts at tuning the THRESHOLD produced this same shape
            // three times; the floor is what none of them had.
            const REENUM_MIN_MS: u64 = 10_000;
            let reenum_ready = state.reenum_at == 0
                || ctx.read_tsc().wrapping_sub(state.reenum_at) >= ctx.duration_cycles(REENUM_MIN_MS);
            let clears_stuck = state.clears_since_data >= CLEARS_STUCK && reenum_ready;
            if (state.xacterr_run >= stuck_at && reenum_ready) || clears_stuck {
                state.xacterr_run = 0;
                let hub_port = (*ksplt & 0x7F) as u8;
                let hub_addr = ((*ksplt >> 7) & 0x7F) as u8;
                let hub_t = chan::Target { addr: hub_addr, mps: 64, low_speed: false };
                // RUNG 2: RESET THE TRANSLATOR, before resetting the device.
                //
                // A buffer clear has just been tried and did not help, and the next rung down has
                // always been a port re-enumeration - which hardware proved cannot fix this fault,
                // twelve times in one minute: every re-enumeration SUCCEEDED (control transfers are
                // non-periodic and were never broken) and the interrupt endpoint was dead again 18 ms
                // later. The device was never the stuck thing. The TT's periodic side was, and
                // nothing in the ladder addressed it.
                //
                // Bounded, so it cannot become the thrash the port reset once was: one Reset_TT per
                // 3 s window. If the fault survives that, the run climbs again and the port reset
                // below still happens - this rung delays the expensive one by ~300 ms, never replaces
                // it.
                let now_tsc = ctx.read_tsc();
                let recently_reset_tt = tt_reset_at != 0
                    && now_tsc.wrapping_sub(tt_reset_at) < ctx.duration_cycles(3000);
                if recently_cleared && !recently_reset_tt {
                    tt_reset_at = now_tsc;
                    state.cleared_at = 0;
                    let _ = hub::reset_tt(&ctx, &m, &d, &hub_t, hub_port, hub_multi_tt);
                    continue;
                }
                if clears_stuck {
                    ctx.log_fmt(format_args!(
                        "dwc2-svc: keyboard endpoint stuck - {} TT clears and a translator reset with no report; re-enumerating hub port {}",
                        state.clears_since_data, hub_port));
                } else {
                    ctx.log_fmt(format_args!(
                        "dwc2-svc: keyboard endpoint stuck - {} consecutive XACTERR{}, no data; re-enumerating hub port {}",
                        stuck_at, if recently_cleared { " after a TT clear that did not help" } else { "" },
                        hub_port));
                }
                state.clears_since_data = 0;
                state.reenum_at = ctx.read_tsc();
                state.cleared_at = 0;
                let mut addr = kt.addr; // re-assign the address it already had
                // A RECOVERY THAT RUNS IN A LOOP IS NOT A RECOVERY.
                //
                // 394 re-enumerations in one session, with nobody typing, flooded the console and
                // never let the device settle - each port reset stranding the transaction that
                // triggered the next cycle. Bound it (§26.6): a few attempts close together means the
                // fault is not the kind this repairs, so stop, say so ONCE, and leave the keyboard
                // dead rather than thrash. Any successful poll clears the streak, so a device that is
                // genuinely recovering is never cut off.
                const RECOVERY_BURST_MAX: u32 = 4;
                if recoveries_in_a_row >= RECOVERY_BURST_MAX {
                    // BACK OFF, do not give up forever.
                    //
                    // The bound is right - four resets in a row without a single healthy poll means
                    // the fault is not the kind a reset repairs, and thrashing helps nobody. But
                    // "input is dead until you replug" is not an acceptable resting state on a
                    // machine whose whole design is that things recover. Hardware showed exactly that
                    // after heavy typing: 20 re-enumerations, then a permanently dead keyboard while
                    // the rest of the system carried on perfectly.
                    //
                    // So the burst stops and a SLOW retry takes over. The condition that provokes
                    // this is transient (a storm of errors under load), so the thing to do is wait
                    // out the storm rather than declare the device gone. Still bounded: one attempt
                    // per backoff window, not a loop.
                    if !recovery_gave_up {
                        recovery_gave_up = true;
                        gave_up_at = ctx.read_tsc();
                        ctx.log_fmt(format_args!(
                            "dwc2-svc: keyboard re-enumerated {} times without settling - backing off for                              {} s. Input is dead meanwhile; storage and networking are unaffected.",
                            recoveries_in_a_row, RECOVERY_BACKOFF_MS / 1000));
                    }
                    if ctx.read_tsc().wrapping_sub(gave_up_at) < ctx.duration_cycles(RECOVERY_BACKOFF_MS) {
                        continue;
                    }
                    // Window elapsed: allow ONE fresh attempt, and say so, so a keyboard that keeps
                    // failing produces a slow visible heartbeat rather than either silence or a flood.
                    ctx.log("dwc2-svc: keyboard backoff elapsed - trying the port once more");
                    recoveries_in_a_row = 0;
                    recovery_gave_up = false;
                }
                recoveries_in_a_row += 1;
                match hub::enumerate_downstream(&ctx, &m, &d, &hub_t, hub_port, &mut addr) {
                    Some((_, _, _, ndt, nsplt)) => match hid::bind(&ctx, &m, &d, &ndt, nsplt) {
                        Some(nk) => {
                            // "re-bound", not "RECOVERED". The bind exercises the control path,
                            // which was never what broke; only a delivered report proves the
                            // interrupt endpoint is alive again, and that is where the recovery is
                            // announced below.
                            ctx.log("dwc2-svc: keyboard re-bound after re-enumeration - awaiting a report to confirm");
                            awaiting_report = true;
                            state = hid::KeyState::new(&ctx); // toggles and repeat start clean
                            kbd = Some((nk, ndt, nsplt));
                        }
                        None => ctx.log("dwc2-svc: keyboard re-enumerated but did NOT re-bind - input stays dead"),
                    },
                    None => ctx.log("dwc2-svc: keyboard port re-enumeration FAILED - input stays dead"),
                }
                continue;
            }
            if hid::poll(&ctx, &m, &d, kt, *ksplt, k, &mut state, hub_multi_tt) {
                // A real report is proof the endpoint is healthy: the streak starts over.
                recoveries_in_a_row = 0;
                recovery_gave_up = false;
                tt_reset_at = 0;
                if awaiting_report {
                    awaiting_report = false;
                    ctx.log("dwc2-svc: keyboard RECOVERED - reports are flowing again");
                }
                reports = reports.wrapping_add(1);
                if reports == 1 {
                    ctx.log("dwc2-svc: *** FIRST KEY REPORT FROM USERSPACE *** - type and it reaches the console");
                }
            }
            }
            let t_rest = ctx.read_tsc();
            seg_kbd = seg_kbd.wrapping_add(t_rest.wrapping_sub(t_kbd));
            if ctx.read_tsc().wrapping_sub(last_beat) > ctx.duration_cycles(HEARTBEAT_MS) {
                last_beat = ctx.read_tsc();
                // Report the busy counts as a RATIO against completed commands. A total says the
                // device is slow; a ratio says which stage is making it slow, which is the thing
                // that can be acted on.
                let (bc, bd, bs, cmds) = disk
                    .as_ref()
                    .map(|(d, _, _)| (d.busy_cbw, d.busy_data, d.busy_csw, d.commands))
                    .unwrap_or((0, 0, 0, 0));
                let ms = ctx.duration_cycles(1).max(1);
                let ns = nic.as_ref().map(|(n, _)| {
                    let s = &n.stats;
                    (s.tx_ok, s.tx_fail, s.rx_bursts, s.rx_bytes, s.rx_frames, s.rx_bad, s.rx_hcint, s.rx_nohalt, s.bmsr, s.rx_fifo, s.int_sts,
                     n.tx_hcint, n.tx_nohalt, n.tx_fail_run, n.tx_nptxsts, n.tx_fifo_free)
                }).unwrap_or((0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0));
                // RE-MEASURE ONCE, QUIET. The boot-time sweep runs while the console is saturated,
                // and a serial write is an un-preemptible syscall of ~9 ms per log line - which is
                // the mean it reported. One repeat on a settled system separates "the timer is slow"
                // from "the boot is loud".
                //
                // It lives HERE, beside the alive report, because that is code known to run: the
                // first attempt put it in the service's OTHER loop, which is only reached when there
                // is no keyboard and no disk - so on real hardware it was unreachable and printed
                // nothing, twice. The same mistake as putting an IRQ enable in a function with no
                // callers, and it cost two boots to notice.
                if !quiet_swept {
                    quiet_swept = true;
                    let per_us = (ctx.tsc_ticks_per_10ms() / 10_000).max(1);
                    for want_us in [125u64, 2000] {
                        let want = per_us.saturating_mul(want_us).max(1);
                        let (mut lo, mut hi, mut sum) = (u64::MAX, 0u64, 0u64);
                        for _ in 0..16 {
                            let t0 = ctx.read_tsc();
                            ctx.sleep(want);
                            let d = ctx.read_tsc().wrapping_sub(t0);
                            lo = lo.min(d); hi = hi.max(d); sum += d;
                        }
                        ctx.log_fmt(format_args!(
                            "dwc2-svc: QUIET sleep {} us -> min {} us, mean {} us, MAX {} us",
                            want_us, lo / per_us, (sum / 16) / per_us, hi / per_us));
                    }
                }
                ctx.log_fmt(format_args!(
                    "dwc2-svc: alive - {} key ({} chars from reports, {} from auto-repeat), {} blocks, {} cmds; busy CBW {} DATA {} CSW {}; {} passes, serve {}ms kbd {}ms sleep {}ms",
                    reports, state.emitted_report, state.emitted_repeat, served, cmds, bc, bd, bs,
                    passes, seg_serve / ms, seg_kbd / ms, seg_sleep / ms));
                // The keyboard's poll outcomes get their own line too, for the same 256-byte reason.
                // This is the line that explains a silent keyboard: the service can be provably
                // healthy (pass count unchanged, still spending time in the poll) while the endpoint
                // answers nothing, and only the breakdown says which of "nothing" it is.
                ctx.log_fmt(format_args!(
                    "dwc2-svc: kbd polls - {} data, {} nak, {} nyet, {} stall, {} xacterr, {} SILENT (no halt), {} other (last {:#010x})",
                    state.n_data, state.n_nak, state.n_nyet, state.n_stall, state.n_xacterr,
                    state.n_silent, state.n_other, state.last_other));
                state.n_data = 0; state.n_nak = 0; state.n_nyet = 0; state.n_stall = 0;
                state.n_xacterr = 0; state.n_silent = 0; state.n_other = 0;

                // The net counters get their OWN line. Appending them to the line above pushed it past
                // the SDK's fixed 256-byte format buffer, so the tail - the value being measured - was
                // silently truncated away. An instrument that does not fit in the log is not an
                // instrument; this cost a boot.
                // The TX failure REASON, alongside the count. A count says a transmit failed; only
                // HCINT says whether the device refused it (NAK), the wire broke (XACTERR), or the
                // channel never halted - three faults with three different fixes, previously
                // indistinguishable from this log.
                ctx.log_fmt(format_args!(
                    // GNPTXSTS field order, because getting it wrong inverted a diagnosis: [15:0]
                    // is FIFO words free, [23:16] is REQUEST QUEUE entries free, [30:24] is the queue
                    // TOP. This printed bits 24+ labelled "qfree" and so reported 24 free entries when
                    // the true figure in [23:16] was ZERO - the register said the queue was exhausted
                    // and the log said it was healthy, which killed a correct hypothesis for a boot.
                    "dwc2-svc: net tx {}/{} fail (last HCINT=0x{:08x} nohalt {}, run {}, dev TX_FIFO_FREE {}, NPTXSTS=0x{:08x} qfree {} fifofree {}), rx {} bursts {} bytes {} frames {} unparsed",
                    ns.0, ns.1, ns.11, ns.12, ns.13, ns.15, ns.14,
                    (ns.14 >> 16) & 0xFF, ns.14 & 0xFFFF,
                    ns.2, ns.3, ns.4, ns.5));
                ctx.log_fmt(format_args!(
                    "dwc2-svc: net IN HCINT=0x{:08x} nohalt {} BMSR=0x{:04x} RX_FIFO=0x{:08x} INT_STS=0x{:08x}",
                    ns.6, ns.7, ns.8, ns.9, ns.10));
            }
            // DO NOT SLEEP WHEN THERE WAS WORK. This is the whole of the throughput problem.
            //
            // The segment timing was unambiguous: five passes in nineteen seconds, of which 18.9 s
            // was SLEEP and 12 ms was serving. A `sleep(10ms)` was taking 3.8 SECONDS, and the
            // average grew across the run - 28 ms, then 33, then 44. The work was never slow; the
            // pass was.
            //
            // A sleep asks the scheduler for a MINIMUM, not a maximum. Under `selfcheck` this service
            // shares a core with everything driving it, so the wait to be rescheduled dwarfs the 10 ms
            // requested - and because `fs` sends one request and waits for the reply, every request
            // costs a whole pass and therefore a whole one of those waits. That is a throughput
            // ceiling of one request per reschedule, and it degrades exactly as the load that causes
            // it grows.
            //
            // So: if this pass served anything, go straight round again. A client is blocked waiting
            // for the next answer, and there is no reason to yield the core before giving it. Sleep
            // only when the pass found nothing to do, which is when the sleep is doing its actual job
            // of not busy-spinning an idle driver.
            // BLOCK ON THE ENDPOINT, do not sleep. An arriving request cannot wake a sleeping task.
            //
            // The previous fix - skip the sleep when the pass did work - was right and insufficient,
            // and the numbers said so: 25 passes, 13 served, 18.7 s of sleep across the 12 IDLE ones.
            // Roughly 1.5 s per `sleep(10ms)`.
            //
            // Those idle passes are the gap while `fs` is thinking between requests, and that is the
            // flaw: a task in `sleep` sleeps out its full duration no matter what arrives, so every
            // gap costs a whole scheduler-stretched sleep before the next request is even LOOKED at.
            // The sleep was not conserving CPU during a quiet moment - it was inserting one.
            //
            // `recv_timeout` blocks on the ENDPOINT: an arriving message wakes it immediately because
            // that wake is event-driven, and the deadline still provides the keyboard's poll cadence
            // when nothing arrives. Same idle behaviour, none of the latency.
            //
            // The message it returns must be SERVED, not dropped - `recv_timeout` consumes. That is
            // the exact bug that made the keyboard report `0 USB IRQ(s)` on a boot where the kernel
            // had delivered the interrupt, and it is one `let _ =` away from happening again.
            if served_this_pass == 0 {
                let t_sleep = ctx.read_tsc();
                if let Some(msg) = ctx.recv_timeout(ctx.duration_cycles(period_ms)) {
                    match disk.as_mut() {
                        Some((dk, dt, sectors)) => {
                            if dispatch(&ctx, &m, &d, dt, dk, nic.as_mut(), &msg, *sectors, &mut capless) {
                                served = served.wrapping_add(1);
                            }
                        }
                        // No disk: ANSWER anyway. Dropping it here is what hung `block-driver` before
                        // its first log line, and `fs` behind it.
                        None => answer_no_disk(&ctx, &mut capless),
                    }
                }
                seg_sleep = seg_sleep.wrapping_add(ctx.read_tsc().wrapping_sub(t_sleep));
            }
        }
    }

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
