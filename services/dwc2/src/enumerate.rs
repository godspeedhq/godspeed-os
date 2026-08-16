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
const DESC_CONFIG: u8 = 0x02;
const DESC_HUB: u8 = 0x29;

/// USB device class 0x09 = hub.
pub const CLASS_HUB: u8 = 0x09;

/// What enumeration found on the root port.
pub struct RootDevice {
    /// The addressed device, so the caller can keep talking to it (the hub port survey does).
    pub target: Target,
    pub vid: u16,
    pub pid: u16,
    pub class: u8,
    pub mps0: u16,
    /// Downstream port count, if this is a hub.
    pub hub_ports: Option<u8>,
    /// The hub's STATUS-CHANGE endpoint: (endpoint number, max packet size).
    ///
    /// A hub does not have to be asked whether anything was plugged in - it TELLS you, on an interrupt
    /// IN endpoint, as a bitmap of the ports that changed (bit N = port N, bit 0 = the hub itself).
    /// That is how Linux learns about hot-plug (`hub_irq`), and it is the difference between waiting on
    /// the truth and interrogating every port on a timer.
    ///
    /// We never parsed it, which is why the userspace driver enumerated once at boot and was blind to
    /// every plug and unplug afterwards.
    pub hub_status_ep: Option<(u8, u16)>,
    /// Does this hub have one transaction translator PER PORT, or a single TT shared by all of them?
    ///
    /// `bDeviceProtocol` in the device descriptor: 1 = single TT, 2 = TT per port. It matters for
    /// exactly one thing, and that thing was silently wrong: USB 2.0 §11.24.2.3 requires the port
    /// field of `Clear_TT_Buffer` (and `Reset_TT`) to name the TT, and on a SINGLE-TT hub there is
    /// only one, addressed as port 1 - not the port the device happens to sit on. Sending the
    /// device's port to a single-TT hub is a well-formed request the hub accepts and acts on for a
    /// TT that does not exist, so the remedy reports success and clears nothing.
    pub hub_multi_tt: bool,
}

/// Find the first interrupt IN endpoint in a configuration descriptor.
///
/// For a hub this is the status-change endpoint - the one that reports which ports changed. The walk
/// is bounded by the buffer and refuses a zero-length descriptor, because a device-supplied length of
/// 0 would leave the cursor fixed and spin forever, and device-supplied data is exactly where that
/// comes from.
fn find_interrupt_in(buf: &[u8], total: usize) -> Option<(u8, u16)> {
    const DESC_ENDPOINT: u8 = 0x05;
    const EP_TYPE_INTERRUPT: u8 = 0x03;
    let mut i = 0usize;
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            break;
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7
            && buf[i + 2] & 0x80 != 0
            && buf[i + 3] & 0x03 == EP_TYPE_INTERRUPT
        {
            let mps = match u16::from_le_bytes([buf[i + 4], buf[i + 5]]) & 0x07FF {
                0 => 1,
                v => v,
            };
            return Some((buf[i + 2] & 0x0F, mps));
        }
        i += len;
    }
    None
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
    // bDeviceProtocol. Read and REPORTED rather than assumed: it decides where a TT remedy is
    // addressed, and a remedy sent to the wrong TT succeeds while fixing nothing - the most
    // expensive kind of wrong, because the log then says the repair worked.
    let protocol = full[6];
    ctx.log_fmt(format_args!(
        "dwc2-svc: root device addressed - VID:PID={:04x}:{:04x} class={:#04x} mps0={}",
        vid, pid, class, mps0));

    // SET_CONFIGURATION, without which the hub answers class requests with NOTHING.
    //
    // A device that has been addressed is in the ADDRESS state, and USB 2.0 chapter 9 only permits
    // class requests in the CONFIGURED state. Skipping this does not produce an error: the hub
    // accepted every port-power and port-status request and returned zeros, so all five ports read as
    // empty on a board with four devices attached.
    //
    // It was VISIBLE only because slice 1b zeroes the IN scratch before every read. Without that, the
    // survey would have reported whatever the previous transfer left in the buffer - a plausible
    // topology assembled from stale bytes, which is far worse than an obviously wrong one. That
    // defence was written for exactly this shape of bug and earned itself here.
    let mut cfg = [0u8; 9];
    if !chan::get_descriptor(ctx, mmio, dma, &t, DESC_CONFIG, 0, &mut cfg, 9) {
        ctx.log("dwc2-svc: config descriptor read FAILED");
        return None;
    }
    // bConfigurationValue is byte 5. Use the value the DEVICE reports rather than assuming 1: it is
    // usually 1, and a device for which it is not would silently stay unconfigured.
    let cfg_val = cfg[5];
    // Read the WHOLE configuration, not just its header, so the hub's status-change endpoint can be
    // found. Only the 9-byte header was ever read here, which is all SET_CONFIGURATION needs - and it
    // is why the endpoint that reports hot-plug has never been looked at.
    let total = u16::from_le_bytes([cfg[2], cfg[3]]) as usize;
    let want = total.min(chan::DATA_LEN);
    let mut full_cfg = [0u8; chan::DATA_LEN];
    let hub_status_ep = if class == CLASS_HUB
        && want >= 9
        && chan::get_descriptor(ctx, mmio, dma, &t, DESC_CONFIG, 0, &mut full_cfg, want)
    {
        find_interrupt_in(&full_cfg, want)
    } else {
        None
    };
    let setup = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control(ctx, mmio, dma, &t, &setup, &mut none, false, 0) {
        ctx.log_fmt(format_args!("dwc2-svc: SET_CONFIGURATION {} FAILED", cfg_val));
        return None;
    }
    ctx.log_fmt(format_args!("dwc2-svc: configured (bConfigurationValue={})", cfg_val));

    // MAKE THE MODE TRUE, do not infer it.
    //
    // `bDeviceProtocol == 2` says the hub CAN do one TT per port. It does not say it IS: USB 2.0
    // §11.14 gives such a hub two interface settings - alternate 0 is SINGLE-TT mode and is the
    // default, alternate 1 is multi-TT - and a hub stays in single-TT mode until the host selects
    // alternate 1. We never sent that, so the hub was running single-TT while this code believed the
    // descriptor and addressed TT remedies to the device's port.
    //
    // In single-TT mode there is exactly one translator and it is addressed as port 1, so every
    // Clear_TT_Buffer went to a TT that does not exist. Hardware counted the result: 650 clears in
    // ninety seconds, the wedge never lifting, and a keyboard that stopped after the first `ls`.
    // Reading a capability as a configuration is how a repair gets aimed at nothing - the same
    // mistake, one layer up, as re-enumerating a device to fix a hub.
    //
    // So SELECT the mode and believe the answer. If the hub accepts alternate 1 it really is
    // per-port and the port number names the TT; if it refuses, it is single-TT and TT 1 is the only
    // one there is. Either way the value below is established rather than assumed.
    let hub_multi_tt = if class == CLASS_HUB && protocol == 2 {
        // bmRequestType 0x01 = host-to-device, standard, INTERFACE. wValue = alternate setting 1,
        // wIndex = interface 0 (a hub has exactly one).
        let set_alt = [0x01, 0x0B, 1, 0, 0, 0, 0, 0];
        let mut none: [u8; 0] = [];
        let ok = chan::control(ctx, mmio, dma, &t, &set_alt, &mut none, false, 0);
        ctx.log_fmt(format_args!(
            "dwc2-svc: hub advertises multi-TT - SET_INTERFACE(alt 1) {}",
            if ok { "accepted: one TT per port" } else { "REFUSED: staying single-TT (TT 1)" }));
        ok
    } else {
        false
    };
    let hub_ports = if class == CLASS_HUB {
        let n = hub_port_count(ctx, mmio, dma, &t);
        if let Some(n) = n {
            ctx.log_fmt(format_args!(
                "dwc2-svc: USB2 hub with {} downstream ports, running {} (bDeviceProtocol {})",
                n,
                if hub_multi_tt { "one TT per port" } else { "a SINGLE shared TT, addressed as TT 1" },
                protocol));
        }
        n
    } else {
        None
    };

    Some(RootDevice { target: t, vid, pid, class, mps0, hub_ports, hub_multi_tt, hub_status_ep })
}
