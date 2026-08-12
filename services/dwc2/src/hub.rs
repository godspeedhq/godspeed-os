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

/// Read one downstream port's status. `None` if the request failed.
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
pub fn survey(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, ports: u8) {
    power_all(ctx, mmio, dma, t, ports);
    let mut found = 0u8;
    let mut split_needed = 0u8;
    for p in 1..=ports {
        match port_status(ctx, mmio, dma, t, p) {
            None => ctx.log_fmt(format_args!("dwc2-svc: hub port {} status read FAILED", p)),
            Some(st) if !st.connected() => {
                ctx.log_fmt(format_args!("dwc2-svc: hub port {} empty", p));
            }
            Some(st) => {
                found += 1;
                if st.needs_split() {
                    split_needed += 1;
                }
                ctx.log_fmt(format_args!(
                    "dwc2-svc: hub port {} CONNECTED speed={} enabled={} (status={:#06x})",
                    p, st.speed(), st.enabled(), st.status));
            }
        }
    }
    ctx.log_fmt(format_args!(
        "dwc2-svc: hub survey complete - {} device(s) attached, {} need split transactions",
        found, split_needed));
}

/// Reset a port, then address and identify the device behind it - THROUGH a split transaction.
///
/// This is the first transfer in the port that reaches past the hub, and every device on this board
/// needs it (the survey says all four are full or low speed). A downstream device is addressed at 0
/// with MPS 8 exactly as a root device is; what differs is that every stage rides the hub's
/// transaction translator.
pub fn enumerate_downstream(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, hub: &Target, port: u8,
) -> Option<(u16, u16, u8)> {
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
    let splt = chan::hcsplt(hub.addr, port);

    // The device answers at address 0 until it is given one. `low_speed` matters to the controller's
    // channel programming, and the hub is the authority on it - the port status just told us.
    let t = Target { addr: 0, mps: 8, low_speed: st.status & PORT_LOW_SPEED != 0 };
    let mut first = [0u8; 18];
    let setup = [0x80, 0x06, 0, 0x01, 0, 0, 8, 0];
    if !chan::control_split(ctx, mmio, dma, &t, &setup, &mut first, true, 8, splt) {
        ctx.log_fmt(format_args!("dwc2-svc: port {} first descriptor read FAILED (split)", port));
        return None;
    }
    let vid = u16::from_le_bytes([first[8], first[9]]);
    let pid = u16::from_le_bytes([first[10], first[11]]);
    let class = first[4];
    ctx.log_fmt(format_args!(
        "dwc2-svc: port {} DEVICE via split - VID:PID={:04x}:{:04x} class={:#04x} speed={}",
        port, vid, pid, class, st.speed()));
    Some((vid, pid, class))
}
