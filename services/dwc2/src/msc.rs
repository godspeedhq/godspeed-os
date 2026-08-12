//! USB mass storage: find the Bulk-Only interface and its two bulk endpoints.
//!
//! Slice 3, first part (`docs/arm32-usb-userspace.md`). Same shape as the keyboard binding, and for
//! the same reason: the descriptor walk is control transfers, which are proven, and separating "did
//! we find the device" from "can we talk to it" means a failure names itself.
//!
//! The stick on this board is HIGH SPEED and directly attached (port 2, `0781:5567`), so nothing here
//! needs a split. That is worth stating rather than assuming - if a full-speed stick ever appears on
//! a hub port, `bind` takes the split descriptor exactly as `hid::bind` does and the rest follows.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::chan::{self, Target};

/// Mass Storage class, SCSI transparent command set, Bulk-Only Transport.
const CLASS_MASS_STORAGE: u8 = 0x08;
const PROTOCOL_BULK_ONLY: u8 = 0x50;

/// Descriptor types.
const DESC_CONFIG: u8 = 0x02;
const DESC_INTERFACE: u8 = 0x04;
const DESC_ENDPOINT: u8 = 0x05;

/// Endpoint transfer type: the low two bits of bmAttributes, 2 = bulk.
const EP_TYPE_BULK: u8 = 0x02;

/// A bound Bulk-Only mass-storage device.
pub struct Disk {
    /// Bulk IN endpoint number (direction bit stripped).
    pub ep_in: u8,
    /// Bulk OUT endpoint number.
    pub ep_out: u8,
    /// Bulk max packet size. 512 for a high-speed device.
    pub mps: u16,
    /// EP0's max packet size, kept because a later control transfer to this device - clearing a
    /// stalled endpoint, resetting the interface - must be framed with EP0's size and NOT the bulk
    /// endpoint's. The kernel driver captures this for exactly that reason, before the bulk size
    /// replaces it in its target state.
    pub ep0_mps: u16,
}

/// Walk a configuration descriptor for a Bulk-Only mass-storage interface and its endpoints.
///
/// Bounded by the buffer, and a zero-length descriptor ends the walk rather than freezing the cursor
/// - device-supplied lengths drive this loop, so the classic parser hang is one malformed byte away.
fn find_bulk_only(buf: &[u8], total: usize) -> Option<(u8, u8, u16)> {
    let mut i = 0usize;
    let mut in_ms = false;
    let (mut ep_in, mut ep_out, mut mps) = (0u8, 0u8, 64u16);
    while i + 2 <= total {
        let len = buf[i] as usize;
        let ty = buf[i + 1];
        if len < 2 || i + len > total {
            break;
        }
        if ty == DESC_INTERFACE && len >= 9 {
            // Recomputed per interface, not latched: a composite device's later non-storage
            // interface must not have its endpoints claimed by an earlier match.
            in_ms = buf[i + 5] == CLASS_MASS_STORAGE && buf[i + 7] == PROTOCOL_BULK_ONLY;
        } else if ty == DESC_ENDPOINT && len >= 7 && in_ms && buf[i + 3] & 0x03 == EP_TYPE_BULK {
            let addr = buf[i + 2];
            // wMaxPacketSize: bits [10:0] are the size, [12:11] a high-speed multiplier that does not
            // apply to bulk. Masking matters - taking the raw word would give a nonsense packet size
            // on a device that sets those bits, and a zero size would divide by zero in the packet
            // count, so it falls back to the 64 every device supports.
            let raw = u16::from_le_bytes([buf[i + 4], buf[i + 5]]);
            mps = match raw & 0x07FF {
                0 => 64,
                v => v,
            };
            if addr & 0x80 != 0 {
                ep_in = addr & 0x0F;
            } else {
                ep_out = addr & 0x0F;
            }
        }
        i += len;
    }
    // BOTH endpoints are required. A Bulk-Only device with one of them is not usable, and returning a
    // half-bound disk would fail later at a transfer, far from the cause.
    if ep_in != 0 && ep_out != 0 {
        Some((ep_in, ep_out, mps))
    } else {
        None
    }
}

/// Configure a device and bind it if it is Bulk-Only mass storage.
///
/// Returns `None` for anything else, which is an ordinary outcome and not reported.
pub fn bind(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, splt: u32,
) -> Option<Disk> {
    let mut head = [0u8; 9];
    let get9 = [0x80, 0x06, 0, DESC_CONFIG, 0, 0, 9, 0];
    if !chan::control_split(ctx, mmio, dma, t, &get9, &mut head, true, 9, splt) {
        return None;
    }
    let total = u16::from_le_bytes([head[2], head[3]]) as usize;
    let cfg_val = head[5];

    // Clamp the REQUEST, not the copy: the controller DMAs the programmed length, so a device
    // reporting an oversized wTotalLength must not be allowed to write past the scratch buffer.
    let want = total.min(chan::DATA_LEN);
    let mut full = [0u8; chan::DATA_LEN];
    let getall = [
        0x80, 0x06, 0, DESC_CONFIG, 0, 0,
        (want & 0xFF) as u8, ((want >> 8) & 0xFF) as u8,
    ];
    if !chan::control_split(ctx, mmio, dma, t, &getall, &mut full, true, want, splt) {
        return None;
    }

    let (ep_in, ep_out, mps) = find_bulk_only(&full, want)?;

    // SET_CONFIGURATION: the same requirement the hub and the keyboard had. An addressed but
    // unconfigured device has no usable endpoints, and its bulk transfers simply do not work.
    let setcfg = [0x00, 0x09, cfg_val, 0, 0, 0, 0, 0];
    let mut none: [u8; 0] = [];
    if !chan::control_split(ctx, mmio, dma, t, &setcfg, &mut none, false, 0, splt) {
        ctx.log_fmt(format_args!("dwc2-svc: disk SET_CONFIGURATION {} FAILED", cfg_val));
        return None;
    }

    ctx.log_fmt(format_args!(
        "dwc2-svc: MASS STORAGE bound - bulk IN {} OUT {} mps {} (ep0 mps {})",
        ep_in, ep_out, mps, t.mps));
    Some(Disk { ep_in, ep_out, mps, ep0_mps: t.mps })
}
