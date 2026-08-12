//! USB ethernet (SMSC LAN9514) - find its bulk endpoints and configure it.
//!
//! Slice 4, first part (`docs/arm32-usb-userspace.md`). Same shape as the keyboard and the disk: the
//! descriptor walk is control transfers, which are proven, so "did we find it" fails separately from
//! "can we talk to it".
//!
//! The NIC is `0424:ec00` on port 1 - HIGH speed and directly attached, so no splits. It reports
//! device class `0xff` (vendor-specific), which is why the endpoint walk does NOT filter by class:
//! there is no standard class to match, only bulk endpoints to find.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

const DESC_CONFIG: u8 = 0x02;
const DESC_ENDPOINT: u8 = 0x05;
const EP_TYPE_BULK: u8 = 0x02;

/// A bound USB ethernet device.
pub struct Nic {
    pub ep_in: u8,
    pub ep_out: u8,
    pub mps: u16,
    /// Endpoint data toggles, per DIRECTION, for the device's lifetime - the level USB defines them
    /// at, learned three times over on the disk path.
    pub pid_in: u32,
    pub pid_out: u32,
}

/// Find the bulk IN/OUT pair. No class filter: this device is vendor-specific.
fn find_bulk(buf: &[u8], total: usize) -> Option<(u8, u8, u16)> {
    let mut i = 0usize;
    let (mut ep_in, mut ep_out, mut mps) = (0u8, 0u8, 64u16);
    while i + 2 <= total {
        let len = buf[i] as usize;
        if len < 2 || i + len > total {
            break; // malformed descriptor ends the walk rather than freezing the cursor
        }
        if buf[i + 1] == DESC_ENDPOINT && len >= 7 && buf[i + 3] & 0x03 == EP_TYPE_BULK {
            let raw = u16::from_le_bytes([buf[i + 4], buf[i + 5]]);
            mps = match raw & 0x07FF {
                0 => 64,
                v => v,
            };
            if buf[i + 2] & 0x80 != 0 {
                ep_in = buf[i + 2] & 0x0F;
            } else {
                ep_out = buf[i + 2] & 0x0F;
            }
        }
        i += len;
    }
    if ep_in != 0 && ep_out != 0 {
        Some((ep_in, ep_out, mps))
    } else {
        None
    }
}

/// Configure the NIC and return its bulk endpoints.
pub fn bind(ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target) -> Option<Nic> {
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control(ctx, mmio, dma, t, &get9, &mut head, true, 9) {
        return None;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let cfg_val = head[5];

    let want = total.min(chan::DATA_LEN);
    let mut full = [0u8; chan::DATA_LEN];
    let getall = [
        0x80, 0x06, 0, DESC_CONFIG, 0, 0,
        (want & 0xFF) as u8, ((want >> 8) & 0xFF) as u8,
    ];
    if !chan::control(ctx, mmio, dma, t, &getall, &mut full, true, want) {
        return None;
    }

    let (ep_in, ep_out, mps) = find_bulk(&full, want)?;

    // RETRY SET_CONFIGURATION, with a settle between attempts.
    //
    // This is not defensive coding - it is a documented property of this device, and the kernel
    // driver's comment records it from hardware: the LAN9514 ACCEPTS the request (the SETUP is ACKed)
    // and then XactErrs the zero-length status stage for tens of milliseconds while it brings up its
    // internal ethernet state. SET_ADDRESS, the same shape of no-data control-OUT, succeeded moments
    // earlier - so the device CAN do the transfer, it just needs to settle after accepting the config.
    //
    // It also NAKs that status stage for longer than an ordinary control budget allows, which is the
    // other half of the same story. This driver's control path treats a NAK as a failed stage, so the
    // retry here is what covers both: eight attempts, each a fresh whole request, exactly as usbcore
    // retries control transfers. HW-blind and tuned by observation in the kernel driver; carried
    // across rather than re-derived, because it passes in QEMU either way.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    let mut set_ok = false;
    for attempt in 0..8u32 {
        if chan::control(ctx, mmio, dma, t, &setcfg, &mut none, false, 0) {
            set_ok = true;
            if attempt > 0 {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: NIC SET_CONFIGURATION took {} attempts (the LAN9514 settles slowly)",
                    attempt + 1));
            }
            break;
        }
        ctx.sleep(ctx.duration_cycles(50));
    }
    if !set_ok {
        ctx.log("dwc2-svc: NIC SET_CONFIGURATION FAILED after 8 attempts");
        return None;
    }

    ctx.log_fmt(format_args!(
        "dwc2-svc: USB ETHERNET bound - bulk IN {} OUT {} mps {}", ep_in, ep_out, mps));
    Some(Nic { ep_in, ep_out, mps, pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0 })
}
