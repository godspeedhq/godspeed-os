//! Hub downstream ports: power them, read their status, reset one that has a device.
//!
//! Slice 1c-ii, first part (`docs/arm32-usb-userspace.md`).
//!
//! **Still no split transactions here, and that is the point.** Every request in this file is a class
//! request addressed to the HUB, which sits directly on the root port at high speed - so they are all
//! direct transfers on a path already proven by 1b. Splits become necessary only when addressing a
//! device BEHIND the hub, which is the next and final piece of Slice 1. Getting the topology first
//! means that when splits are attempted, the port they are attempted through is already known to
//! report the right thing.

use godspeed_sdk::{Dma, Mmio, ServiceContext};


use crate::chan::{self, Target};

/// The first USB address handed to a downstream device.
///
/// The hub took 1 at its own enumeration, so downstream devices start at 2. A counter rather than a
/// per-port formula because an address is a property of the BUS, not of where a device happens to sit.
///
/// C6-1: this used to be `static NEXT_ADDR: AtomicU8`, which is unowned global mutable state
/// (Invariant 9) wearing a thread-safe type. Nothing about it was shared or concurrent - one caller,
/// on one core - it was simply easier to reach for a static than to thread the counter through. The
/// owner is the enumeration loop, so the counter now lives there and is passed in.
pub const FIRST_DOWNSTREAM_ADDR: u8 = 2;

// --- Hub class requests (USB 2.0 chapter 11) ---------------------------------------------------
//
// bmRequestType 0x23 = host-to-device, CLASS, OTHER (a port, not the hub itself). The `OTHER`
// recipient is what makes wIndex a port number; sending these with the DEVICE recipient addresses
// the hub as a whole and does something quite different.
const REQ_TYPE_PORT_OUT: u8 = 0x23;
const REQ_TYPE_PORT_IN: u8 = 0xA3;
const REQ_SET_FEATURE: u8 = 0x03;
const REQ_CLEAR_FEATURE: u8 = 0x01;
const REQ_GET_STATUS: u8 = 0x00;
/// USB 2.0 §11.24.2.3 - flush one TT buffer. The only way out of a permanent NYET.
const REQ_CLEAR_TT_BUFFER: u8 = 0x08;
/// USB 2.0 §11.24.2.9. Returns the WHOLE transaction translator to its power-on state, as opposed to
/// `Clear_TT_Buffer`, which flushes the buffer belonging to one endpoint.
///
/// This rung was missing entirely, and its absence is why a wedged keyboard never came back. A TT
/// keeps its periodic and non-periodic traffic in separate buffers, so a hub whose PERIODIC pipeline
/// is stuck still services control transfers perfectly - which is exactly what hardware showed:
/// re-enumeration succeeded twelve times in a row (all control), and the interrupt endpoint was dead
/// again 18 ms later every time. Re-enumerating the DEVICE cannot repair a translator that lives in
/// the HUB, so the driver's most expensive remedy was aimed at the wrong box.
const REQ_RESET_TT: u8 = 0x09;

/// The `wIndex` that names a TT.
///
/// On a multi-TT hub each port has its own translator, so the port number names it. On a single-TT
/// hub there is exactly one, and USB 2.0 requires it to be addressed as port 1 - passing the device's
/// real port asks the hub to operate on a translator it does not have. The hub answers that request
/// successfully, which is the trap: the remedy is reported as accepted and does nothing at all.
fn tt_windex(port: u8, multi_tt: bool) -> u8 {
    if multi_tt { port } else { 1 }
}

// Port features.
const FEAT_PORT_RESET: u16 = 4;
const FEAT_PORT_POWER: u16 = 8;
const FEAT_C_PORT_CONNECTION: u16 = 16;
const FEAT_C_PORT_RESET: u16 = 20;

// wPortStatus bits.
pub const PORT_CONNECTED: u16 = 1 << 0;
pub const PORT_ENABLED: u16 = 1 << 1;
pub const PORT_RESET: u16 = 1 << 4;
pub const PORT_LOW_SPEED: u16 = 1 << 9;
pub const PORT_HIGH_SPEED: u16 = 1 << 10;

/// What a downstream port reports.
pub struct PortStatus {
    pub status: u16,
    pub change: u16,
}

impl PortStatus {
    pub fn connected(&self) -> bool { self.status & PORT_CONNECTED != 0 }
    pub fn enabled(&self) -> bool { self.status & PORT_ENABLED != 0 }
    /// USB speed of the attached device, as a word for the log.
    ///
    /// The bits are mutually exclusive and NEITHER being set means full speed - it is not an error
    /// case and must not be reported as one, because full speed is exactly what a device needing
    /// split transactions looks like.
    pub fn speed(&self) -> &'static str {
        if self.status & PORT_LOW_SPEED != 0 {
            "low"
        } else if self.status & PORT_HIGH_SPEED != 0 {
            "high"
        } else {
            "full"
        }
    }
    /// Does reaching this device require split transactions?
    ///
    /// A full or low-speed device behind a high-speed hub must be reached through the hub's
    /// transaction translator. This is the predicate that decides whether the hard part of the port
    /// is needed for a given device, so it is worth naming rather than open-coding.
    pub fn needs_split(&self) -> bool {
        self.status & PORT_HIGH_SPEED == 0
    }
}

fn port_feature(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target,
    set: bool, feature: u16, port: u8,
) -> bool {
    let setup = [
        REQ_TYPE_PORT_OUT,
        if set { REQ_SET_FEATURE } else { REQ_CLEAR_FEATURE },
        (feature & 0xFF) as u8,
        ((feature >> 8) & 0xFF) as u8,
        port,
        0,
        0,
        0,
    ];
    let mut none: [u8; 0] = [];
    chan::control(ctx, mmio, dma, t, &setup, &mut none, false, 0)
}

/// Flush a wedged transaction-translator buffer (USB 2.0 §11.24.2.3, `Clear_TT_Buffer`).
///
/// **This is the recovery for a keyboard that goes silent and stays silent.** A low-speed device
/// behind a high-speed hub is reached through the hub's TT, which buffers the low-speed transaction
/// and hands the result back when the host asks with a complete-split. If the host never successfully
/// collects it, that buffer stays occupied and the TT answers NYET to every complete-split from then
/// on - forever, because nothing in the transfer path clears it.
///
/// Measured on this board, in the window where the keyboard was dead: **1497 polls, 1497 NYET**, zero
/// data, zero NAK, zero errors. Not a scheduling problem and not a lost report - one stuck buffer,
/// and the driver had no way to say so because it had no way to clear it. The spec's answer is this
/// request; Linux issues it from `usb_hub_clear_tt_buffer` for exactly this case.
///
/// `wValue` packs the pipe the TT should forget: endpoint number, device address, endpoint type and
/// direction. `wIndex` is the hub port the TT serves. The request goes to the HUB (direct, high
/// speed), never to the stuck device - which is the point, since the device is unreachable.
pub fn clear_tt_buffer(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target,
    dev_addr: u8, ep: u8, ep_type: u8, dir_in: bool, port: u8, multi_tt: bool,
) -> bool {
    let wvalue: u16 = ((ep as u16) & 0xF)
        | (((dev_addr as u16) & 0x7F) << 4)
        | (((ep_type as u16) & 0x3) << 11)
        | if dir_in { 1 << 15 } else { 0 };
    let setup = [
        REQ_TYPE_PORT_OUT,          // 0x23: host-to-device, class, OTHER (a port)
        REQ_CLEAR_TT_BUFFER,
        (wvalue & 0xFF) as u8,
        (wvalue >> 8) as u8,
        tt_windex(port, multi_tt),
        0,
        0,
        0,
    ];
    let mut none: [u8; 0] = [];
    let ok = chan::control(ctx, mmio, dma, hub, &setup, &mut none, false, 0);
    // Only a REFUSAL is worth a line every time. An accepted clear is the normal, healthy outcome
    // of an eager remedy that may fire often; its caller already reports a running count.
    if !ok {
        ctx.log_fmt(format_args!(
            "dwc2-svc: Clear_TT_Buffer for addr {} ep {} on hub port {} REFUSED - the TT stays wedged",
            dev_addr, ep, port));
    }
    ok
}

/// Reset the hub's transaction translator (USB 2.0 §11.24.2.9).
///
/// The rung between "flush one endpoint's buffer" and "re-enumerate the device", and the one that
/// was missing. A TT services periodic and non-periodic traffic from separate buffers, so when its
/// periodic side wedges the device stays perfectly reachable by control transfers and completely
/// unreachable by interrupt ones. Every remedy the driver had either flushed a single endpoint's
/// entry or reset the DEVICE; neither touches a translator in that state.
///
/// It is heavier than a buffer clear - it disturbs every device behind this TT, briefly - so it sits
/// where it belongs in the ladder: after a clear has been tried and demonstrably not helped, and
/// before the port reset that costs 0.31 s and cannot fix this fault at all.
pub fn reset_tt(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target, port: u8, multi_tt: bool,
) -> bool {
    let setup = [REQ_TYPE_PORT_OUT, REQ_RESET_TT, 0, 0, tt_windex(port, multi_tt), 0, 0, 0];
    let mut none: [u8; 0] = [];
    let ok = chan::control(ctx, mmio, dma, hub, &setup, &mut none, false, 0);
    ctx.log_fmt(format_args!(
        "dwc2-svc: Reset_TT on hub TT {} - {}",
        tt_windex(port, multi_tt),
        if ok { "accepted" } else { "REFUSED" }));
    ok
}

/// Read one downstream port's status. `None` if the request failed.
/// Ask the hub which ports have changed - without asking about the ports.
///
/// The hub reports changes on an interrupt IN endpoint as a bitmap: bit N set means port N changed,
/// and bit 0 means the hub itself. This is the truth being waited on (Commandment VIII); the
/// alternative - reading every port's status on a timer - is a poll that costs the same whether
/// anything happened or not, and reports it late by up to one interval.
///
/// The endpoint NAKs when nothing has changed, which is the ordinary answer and costs one transfer.
/// `Some(0)` therefore means "asked, nothing to report"; `None` means the transfer did not complete
/// and the caller should simply ask again next pass rather than infer anything.
///
/// The hub is directly attached at high speed, so this needs no split transaction - it is the plain
/// interrupt IN that the keyboard cannot have.
pub fn status_change(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target, ep: u8, mps: u16, pid: &mut u32,
) -> Option<u32> {
    const STATUS_OFF: usize = 0x0300;          // clear of the control scratch and the HID report
    let len = (mps as u32).min(4);
    for i in 0..len as usize {
        dma.write8(STATUS_OFF + i, 0);
    }
    let hcint = chan::interrupt_in(
        ctx, mmio, hub, chan::CH_HUB, *pid, dma.phys_at(STATUS_OFF) as u32, len, ep as u32);
    if hcint & crate::regs::HCINT_XFERCOMPL == 0 {
        // NAK is the ordinary "nothing changed"; anything else is a transfer that did not land and is
        // not evidence of anything either way.
        return if hcint & crate::regs::HCINT_NAK != 0 { Some(0) } else { None };
    }
    *pid = chan::pid_from_hctsiz(mmio, chan::CH_HUB);
    let mut bits = 0u32;
    for i in 0..len as usize {
        bits |= (dma.read8(STATUS_OFF + i) as u32) << (i * 8);
    }
    Some(bits)
}

/// Clear a port's CONNECTION-CHANGE flag, so the hub stops reporting the same event forever.
pub fn clear_connect_change(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target, port: u8,
) {
    let _ = port_feature(ctx, mmio, dma, hub, false, FEAT_C_PORT_CONNECTION, port);
}

pub fn port_status(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, port: u8,
) -> Option<PortStatus> {
    let setup = [REQ_TYPE_PORT_IN, REQ_GET_STATUS, 0, 0, port, 0, 4, 0];
    let mut buf = [0u8; 4];
    if !chan::control(ctx, mmio, dma, t, &setup, &mut buf, true, 4) {
        return None;
    }
    Some(PortStatus {
        status: u16::from_le_bytes([buf[0], buf[1]]),
        change: u16::from_le_bytes([buf[2], buf[3]]),
    })
}

/// Power every downstream port and let them settle.
///
/// A port with no power reports nothing connected, which is indistinguishable from an empty socket -
/// so powering first is what makes the survey below mean anything.
pub fn power_all(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, ports: u8) {
    for p in 1..=ports {
        if !port_feature(ctx, mmio, dma, t, true, FEAT_PORT_POWER, p) {
            ctx.log_fmt(format_args!("dwc2-svc: hub port {} would not power on", p));
        }
    }
    // bPwrOn2PwrGood is in the hub descriptor in 2 ms units; 100 ms is comfortably past every hub's
    // value and this runs once. A device is not detectable before its port's power is good, so
    // surveying early reports an empty hub on a populated one.
    ctx.sleep(ctx.duration_cycles(100));
}

/// Reset a downstream port so its device is addressable, and report the speed the hub then sees.
///
/// The speed is only valid AFTER the reset completes - before it, the hub has not yet detected which
/// bus speed the device chirped for.
pub fn reset_port(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, port: u8,
) -> Option<PortStatus> {
    if !port_feature(ctx, mmio, dma, t, true, FEAT_PORT_RESET, port) {
        ctx.log_fmt(format_args!("dwc2-svc: hub port {} reset request FAILED", port));
        return None;
    }
    // USB 2.0 requires at least 10 ms of reset; the hub drives it and clears PORT_RESET when done.
    // Poll for that rather than assuming a duration - the hub is the authority on when it finished.
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(200));
    loop {
        let st = port_status(ctx, mmio, dma, t, port)?;
        if st.status & PORT_RESET == 0 && st.enabled() {
            // Acknowledge the change bits, or the hub keeps reporting this reset as news and a later
            // survey reads a stale event as a fresh plug.
            let _ = port_feature(ctx, mmio, dma, t, false, FEAT_C_PORT_RESET, port);
            let _ = port_feature(ctx, mmio, dma, t, false, FEAT_C_PORT_CONNECTION, port);
            return Some(st);
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            ctx.log_fmt(format_args!(
                "dwc2-svc: hub port {} did not finish reset within 200 ms (status={:#06x})",
                port, st.status));
            return None;
        }
    }
}

/// Survey every downstream port and report what is attached.
///
/// Reports rather than binds: this slice establishes the topology, and Slice 2 onward is what
/// actually drives a device. Separating them means a wrong topology is visible as a wrong topology
/// instead of as a device that will not work.
pub fn survey(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, ports: u8,
) -> ([u16; 8], usize) {
    power_all(ctx, mmio, dma, t, ports);
    let mut found = 0u8;
    let mut split_needed = 0u8;
    // Keep what we read. Every status here cost a control transfer, and re-reading them to decide
    // enumeration order made the survey's work a throwaway - about half a second of the window a
    // storm leaves before it kills this service again.
    let mut st_by_port = [0u16; 8];
    let n_status = (ports as usize).min(8);
    for p in 1..=ports.min(8) {
        match port_status(ctx, mmio, dma, t, p) {
            None => ctx.log_fmt(format_args!("dwc2-svc: hub port {} status read FAILED", p)),
            Some(st) if !st.connected() => {
                ctx.log_fmt(format_args!("dwc2-svc: hub port {} empty", p));
            }
            Some(st) => {
                found += 1;
                st_by_port[p as usize - 1] = st.status;
                if st.needs_split() {
                    split_needed += 1;
                }
                // NO SPEED HERE. The bits are not valid until the port has been RESET - an idle
                // port has not seen the device chirp - and reporting them anyway is what produced a
                // confident, wrong "4 need split transactions" that cost three boots. What this
                // survey legitimately knows is that something is attached.
                ctx.log_fmt(format_args!(
                    "dwc2-svc: hub port {} CONNECTED (status={:#06x}; speed unknown until reset)",
                    p, st.status));
            }
        }
    }
    let _ = split_needed;
    ctx.log_fmt(format_args!(
        "dwc2-svc: hub survey complete - {} device(s) attached (each one's speed, and whether it          needs a split, is known only after its port reset)", found));
    (st_by_port, n_status)
}

/// The order to enumerate downstream ports in: LOW-SPEED FIRST, then the rest in port order.
///
/// **Why this is not cosmetic.** A full bring-up of this hub takes ~2.75 s (four devices, each reset,
/// addressed and configured), and the keyboard is on the last port. During a kill storm `dwc2` is
/// restarted faster than that, so it dies partway through EVERY time - and "partway" always means the
/// tail, which is always the keyboard. Measured over 12 restarts: every restart that reached port 4
/// bound the keyboard, and the nine that did not never got there. The console was not failing to bind,
/// it was never being reached.
///
/// Enumerating the input device first inverts what a partial bring-up costs. Storage and network
/// tolerate arriving late - `fs` and `net-stack` reacquire and retry by design - whereas a missing
/// keyboard is the operator losing their hands, with no retry path because there is nobody to type it.
///
/// **On using the speed bit before a port reset.** The survey above deliberately refuses to report
/// speed, and that refusal is right for `PORT_HIGH_SPEED` (bit 10), which is only meaningful after the
/// reset chirp - trusting it early is what once produced a confident, wrong "4 need split
/// transactions". `PORT_LOW_SPEED` (bit 9) is different in kind: it comes from which data line the
/// device pulls up at attach, so it is valid as soon as the port reports connected.
///
/// And the consequence of being wrong here is bounded to nothing: this decides only the ORDER of
/// visits. Every connected port is still enumerated exactly once, by the same code, whatever the bit
/// said. A speed bit that lied would cost a slightly different order, never a device.
pub fn visit_order(status: &[u16; 8], n_status: usize) -> ([u8; 8], usize) {
    let mut order = [0u8; 8];
    let mut n = 0usize;
    // Two passes over the statuses rather than a sort: at this size it is the same work, and
    // "low speed first, otherwise port order" is the whole policy stated in the shape of the code.
    // Pure - it performs no I/O, so ordering costs nothing on a path a storm is racing.
    for low_first in [true, false] {
        for p in 1..=n_status.min(8) {
            if n >= order.len() {
                break;
            }
            let st = status[p - 1];
            // A port that reported nothing connected has status 0, so it is not low speed and lands
            // in the second pass; the enumeration below reports its own outcome for it either way.
            let is_low = st & PORT_CONNECTED != 0 && st & PORT_LOW_SPEED != 0;
            if is_low == low_first {
                order[n] = p as u8;
                n += 1;
            }
        }
    }
    (order, n)
}

/// Reset a port, then address and identify the device behind it - THROUGH a split transaction.
///
/// This is the first transfer in the port that reaches past the hub, and every device on this board
/// needs it (the survey says all four are full or low speed). A downstream device is addressed at 0
/// with MPS 8 exactly as a root device is; what differs is that every stage rides the hub's
/// transaction translator.
pub fn enumerate_downstream(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target, port: u8, next_addr: &mut u8,
) -> Option<(u16, u16, u8, Target, u32)> {
    // THE SPEED IS ONLY VALID AFTER THE RESET, and it decides whether a split is needed at all.
    //
    // `reset_port` returns the POST-reset status, and that is the one to believe. Before a reset the
    // hub reports an idle port whose speed bits mean nothing - the device has not chirped yet - and
    // reading them there is what produced the survey's confident and wrong "4 devices, 4 need split
    // transactions". Three of them are HIGH speed:
    //
    //   survey (pre-reset)   0x0101 -> looked full speed
    //   after reset          0x0503 -> HIGH speed
    //
    // A high-speed device behind a high-speed hub is addressed DIRECTLY: there is no transaction
    // translator in the path, and sending it a split is exactly what a hub STALLs. That was the
    // `SETUP complete-split STALLed` on ports 1-3, and it was never a fault in the split code - port
    // 4, the one genuinely low-speed device, enumerated through a split on the same boot.
    let st = reset_port(ctx, mmio, dma, hub, port)?;
    // TRSTRCY: the device gets 10 ms of RECOVERY after its reset before it is required to respond.
    //
    // USB 2.0 7.1.7.5. Omitting it produced `SETUP complete-split STALLed` on the very first
    // transfer, and that error was the clue rather than the problem: a compliant device may NOT STALL
    // a SETUP packet - it must always ACK one - so a STALL there says the device was not yet
    // answering, not that it rejected the request. The split machinery had already done its job by
    // then, since the complete-split is only ever issued after the transaction translator ACKed the
    // start-split.
    ctx.sleep(ctx.duration_cycles(15));
    // Split ONLY when the device actually needs one.
    let splt = if st.needs_split() { chan::hcsplt(hub.addr, port) } else { 0 };

    // The device answers at address 0 until it is given one. `low_speed` matters to the controller's
    // channel programming, and the hub is the authority on it - the port status just told us.
    let mut t = Target { addr: 0, mps: 8, low_speed: st.status & PORT_LOW_SPEED != 0 };
    let mut first = [0u8; 18];
    let setup = [0x80, 0x06, 0, 0x01, 0, 0, 8, 0];
    if !chan::control_split(ctx, mmio, dma, &t, &setup, &mut first, true, 8, splt) {
        // WAS THAT THE HUB OR THE DEVICE? Re-read the port, from the HUB, over a DIRECT transfer.
        //
        // The two answers want opposite fixes and nothing in the failure itself separates them. This
        // read does, three ways at once:
        //
        //   - it ANSWERS at all -> the hub is alive and its own control endpoint is fine, so whatever
        //     STALLed was produced inside the split path rather than by a sick hub;
        //   - the port is still ENABLED -> the hub did not react to the transaction by disabling it,
        //     which a hub does when it decides a device is misbehaving;
        //   - the speed still reads the same -> the port did not silently drop and re-detect.
        //
        // A hub that has stopped answering entirely is a completely different fault from a device
        // that STALLed, and a failure that cannot tell them apart is worth one extra transfer.
        match port_status(ctx, mmio, dma, hub, port) {
            Some(after) => ctx.log_fmt(format_args!(
                "dwc2-svc: port {} FAILED (split) - hub still answers, port now connected={} enabled={} speed={} (status={:#06x})",
                port, after.connected(), after.enabled(), after.speed(), after.status)),
            None => ctx.log_fmt(format_args!(
                "dwc2-svc: port {} FAILED (split) - and the HUB has stopped answering too (its own control endpoint is broken, not the device's)",
                port)),
        }
        return None;
    }
    // GIVE IT AN ADDRESS, then read the WHOLE descriptor.
    //
    // Two bugs the last boot exposed, both here:
    //
    // 1. VID:PID read as 0000:0000 because only EIGHT bytes were requested and VID lives at bytes
    //    8..11. The class byte (offset 4) came through correctly - `class=0xff` on port 1 is the real
    //    vendor-specific class of the SMSC ethernet - which is what proved the transfer itself was
    //    fine. The zeros were the scratch-zeroing doing its job again rather than handing back stale
    //    bytes that would have looked like a plausible device.
    //
    // 2. Ports 2 and 3 failed AFTER port 1 succeeded, and that ordering is the clue: nothing was ever
    //    given an address, so port 1's device stayed at address 0 and port 2's device answered at
    //    address 0 as well. Two devices replying to the same token garbles both. Enumeration assigns
    //    a unique address precisely so only one device is ever listening at 0.
    let mps0 = first[7] as u16;
    if mps0 == 0 {
        ctx.log_fmt(format_args!("dwc2-svc: port {} reports EP0 max packet size 0", port));
        return None;
    }
    t.mps = mps0;
    let addr = *next_addr;
    *next_addr += 1;
    let sa = [0x00, 0x05, addr, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control_split(ctx, mmio, dma, &t, &sa, &mut none, false, 0, splt) {
        ctx.log_fmt(format_args!("dwc2-svc: port {} SET_ADDRESS {} FAILED", port, addr));
        return None;
    }
    ctx.sleep(ctx.duration_cycles(5)); // USB 2.0 9.2.6.3: 2 ms to commit the new address
    t.addr = addr;

    let mut full = [0u8; 18];
    let getall = [0x80, 0x06, 0, 0x01, 0, 0, 18, 0];
    if !chan::control_split(ctx, mmio, dma, &t, &getall, &mut full, true, 18, splt) {
        ctx.log_fmt(format_args!("dwc2-svc: port {} full descriptor read FAILED at address {}", port, addr));
        return None;
    }

    let vid = u16::from_le_bytes([full[8], full[9]]);
    let pid = u16::from_le_bytes([full[10], full[11]]);
    let class = full[4];
    ctx.log_fmt(format_args!(
        "dwc2-svc: port {} DEVICE {} - VID:PID={:04x}:{:04x} class={:#04x} speed={} addr={}",
        port, if splt != 0 { "via split" } else { "direct" }, vid, pid, class, st.speed(), addr));
    Some((vid, pid, class, t, splt))
}
