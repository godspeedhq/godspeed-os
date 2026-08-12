//! Enumeration: address the device on the root port, read what it is, and if it is a hub, ask how
//! many downstream ports it has.
//!
//! Slice 1c, first half (`docs/arm32-usb-userspace.md`). Deliberately stops at the hub.
//!
//! **Why the hub is a clean stopping point.** The hub sits DIRECTLY on the root port at high speed,
//! so every transfer here is a direct one. Split transactions are only needed to reach a device
//! BEHIND the hub, where a full/low-speed device hangs off a high-speed bus and the hub's transaction
//! translator has to be microframe-scheduled. That is the part flagged from the start as the risk of
//! this port - `split_txn_periodic` moves from ring 0 with interrupts masked into a PREEMPTIBLE task,
//! and a preemption in the wrong microframe does not fail cleanly, it transfers nothing. Enumerating
//! the hub first means that risk is faced on its own, against a device tree already known to be
//! reachable, rather than tangled with "does addressing even work".

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

/// Standard descriptor types.
const DESC_DEVICE: u8 = 0x01;
const DESC_HUB: u8 = 0x29;

/// USB device class 0x09 = hub.
pub const CLASS_HUB: u8 = 0x09;

/// What enumeration found on the root port.
pub struct RootDevice {
    pub vid: u16,
    pub pid: u16,
    pub class: u8,
    pub mps0: u16,
    /// Downstream port count, if this is a hub.
    pub hub_ports: Option<u8>,
}

/// SET_ADDRESS. No data stage.
///
/// The device accepts this at address 0 and answers everything afterwards at the new one, so the
/// caller's `Target` must be updated in step. A mismatch here does not report an error - the device
/// simply stops answering, which reads as broken hardware.
fn set_address(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, addr: u8) -> bool {
    let setup = [0x00, 0x05, addr, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control(ctx, mmio, dma, t, &setup, &mut none, false, 0) {
        ctx.log_fmt(format_args!("dwc2-svc: SET_ADDRESS {} FAILED", addr));
        return false;
    }
    // USB 2.0 9.2.6.3: the device has 2 ms to commit the new address. Everything after this talks to
    // a different address, so going early loses the device with no error anywhere.
    ctx.sleep(ctx.duration_cycles(5));
    true
}

/// Read the hub descriptor (class-specific, type 0x29) and return its downstream port count.
fn hub_port_count(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> Option<u8> {
    // bmRequestType 0xA0: device-to-host, CLASS, device recipient. Not the 0x80 a standard
    // descriptor uses - a hub descriptor is class-specific and the wrong request type STALLs.
    let setup = [0xA0, 0x06, 0, DESC_HUB, 0, 0, 9, 0];
    let mut buf = [0u8; 9];
    if !chan::control(ctx, mmio, dma, t, &setup, &mut buf, true, 9) {
        ctx.log("dwc2-svc: hub descriptor read FAILED");
        return None;
    }
    // bDescLength, bDescriptorType, bNbrPorts. Check the type rather than trusting position: a short
    // or misparsed reply that happened to be zeroed would otherwise report a plausible port count.
    if buf[1] != DESC_HUB {
        ctx.log_fmt(format_args!(
            "dwc2-svc: hub descriptor has wrong type {:#04x} (expected {:#04x})", buf[1], DESC_HUB));
        return None;
    }
    Some(buf[2])
}

/// Address and identify whatever is on the root port.
///
/// Returns `None` if any step failed - each one has already reported which.
pub fn root_device(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma) -> Option<RootDevice> {
    // A just-reset device answers at address 0 with an EP0 max-packet-size of 8. Eight bytes is the
    // minimum every device must accept, which is why the first read asks for exactly that: the real
    // MPS0 is byte 7 of the reply, and asking for more before knowing it is how enumeration fails on
    // devices with a small EP0.
    let mut t = Target { addr: 0, mps: 8, low_speed: false };
    let mut first = [0u8; 18];
    if !chan::get_descriptor(ctx, mmio, dma, &t, DESC_DEVICE, 0, &mut first, 8) {
        ctx.log("dwc2-svc: first descriptor read FAILED - nothing is answering on the root port");
        return None;
    }
    let mps0 = first[7] as u16;
    if mps0 == 0 {
        ctx.log("dwc2-svc: device reports EP0 max packet size 0 - refusing to enumerate it");
        return None;
    }

    // Now the device can be addressed and re-read in full, at its real packet size.
    t.mps = mps0;
    if !set_address(ctx, mmio, dma, &t, 1) {
        return None;
    }
    t.addr = 1;

    let mut full = [0u8; 18];
    if !chan::get_descriptor(ctx, mmio, dma, &t, DESC_DEVICE, 0, &mut full, 18) {
        ctx.log("dwc2-svc: full descriptor read FAILED after SET_ADDRESS");
        return None;
    }

    let vid = u16::from_le_bytes([full[8], full[9]]);
    let pid = u16::from_le_bytes([full[10], full[11]]);
    let class = full[4];
    ctx.log_fmt(format_args!(
        "dwc2-svc: root device addressed - VID:PID={:04x}:{:04x} class={:#04x} mps0={}",
        vid, pid, class, mps0));

    let hub_ports = if class == CLASS_HUB {
        let n = hub_port_count(ctx, mmio, dma, &t);
        if let Some(n) = n {
            ctx.log_fmt(format_args!("dwc2-svc: USB2 hub with {} downstream ports", n));
        }
        n
    } else {
        None
    };

    Some(RootDevice { vid, pid, class, mps0, hub_ports })
}
