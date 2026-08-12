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
    /// The endpoint data toggles, one per DIRECTION, for the DEVICE'S LIFETIME.
    ///
    /// A toggle belongs to the ENDPOINT, not to a transfer and not to a command. Making it
    /// per-command fixed READ CAPACITY and then broke READ(10): the second command restarted at
    /// DATA0 while the device had already advanced past it, so the data stage was ignored and the
    /// channel never halted. That is the same error as the per-transfer version, one level up, and
    /// this is the level the USB spec actually defines it at.
    ///
    /// Reset only by a clear-halt or a SET_CONFIGURATION, which is why they live with the binding.
    pub pid_in: u32,
    pub pid_out: u32,
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
    Some(Disk { ep_in, ep_out, mps, ep0_mps: t.mps, pid_in: chan::PID_DATA0, pid_out: chan::PID_DATA0 })
}

/// Where a BOT command's buffers live in the DMA arena. Past the keyboard report, which is past the
/// control scratch - the assertions below derive that rather than asserting it in prose, after a
/// buffer overlap cost a boot in slice 2.
pub const CBW_OFF: usize = 0x0300;
pub const CSW_OFF: usize = 0x0340;
pub const DATA_OFF: usize = 0x0400;
pub const DATA_MAX: usize = 4096;
const _: () = assert!(CBW_OFF >= crate::hid::REPORT_OFF + 8);
const _: () = assert!(CSW_OFF >= CBW_OFF + 31);
const _: () = assert!(DATA_OFF >= CSW_OFF + 13);

/// One bulk transfer. Returns bytes moved, or `None` on failure.
///
/// NAK is FLOW CONTROL, not an error - the device saying "busy, ask again" - so it is retried inside
/// the time budget and never counted against the error allowance. That distinction is the whole
/// difference between a driver that survives this board's stick and one that declares it broken: it
/// goes BUSY for tens of seconds under load, and a 45-second stall was observed on this branch.
#[allow(clippy::too_many_arguments)]
fn bulk_xfer(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target, mps: u16,
    dir_in: bool, ep: u8, buf_phys: u32, len: u32, budget_ms: u64, pid: &mut u32,
) -> Option<u32> {
    let bt = Target { addr: t.addr, mps, low_speed: false };
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(budget_ms));
    let mut xact_errs = 0u32;
    loop {
        chan::program(mmio, &bt, chan::CH_BULK, dir_in, *pid, len, buf_phys, ep as u32, 2, 0);
        match chan::wait_halt(ctx, mmio, chan::CH_BULK, 100) {
            Some(hcint) if hcint & crate::regs::HCINT_XFERCOMPL != 0 => {
                // HCTSIZ counts DOWN the bytes still outstanding, so what moved is the difference.
                // Reading it is what makes a SHORT transfer detectable at all.
                let left = mmio.read32(chan::hctsiz_at(chan::CH_BULK)) & 0x7_FFFF;
                // CARRY THE TOGGLE FORWARD, read back from the hardware.
                //
                // A bulk endpoint's DATA0/DATA1 toggle alternates across transfers, and the device
                // IGNORES a packet with the wrong one. Programming DATA0 every time worked for the
                // CBW and the data stage - both happened to land on DATA0 - and then the CSW was sent
                // with a toggle the device had already consumed, so it never answered and the channel
                // never halted. Exactly the same fault as the keyboard's doubled keystroke, in the
                // other direction: there a stale toggle made the device RETRANSMIT, here it makes the
                // device IGNORE.
                *pid = chan::pid_from_hctsiz(mmio, chan::CH_BULK);
                return Some(len.saturating_sub(left));
            }
            Some(hcint) if hcint & crate::regs::HCINT_STALL != 0 => {
                ctx.log_fmt(format_args!("dwc2-svc: bulk ep {} STALLed", ep));
                return None;
            }
            Some(hcint) => {
                // Only a real TRANSACTION error spends the budget. NAK and NYET are the device
                // pacing us and cost nothing, which is why they are not counted here.
                if hcint & crate::regs::HCINT_XACTERR != 0 {
                    xact_errs += 1;
                    if xact_errs >= 3 {
                        return None;
                    }
                }
            }
            None => {
                ctx.log_fmt(format_args!("dwc2-svc: bulk ep {} channel never halted", ep));
                return None;
            }
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            ctx.log_fmt(format_args!(
                "dwc2-svc: bulk ep {} out of time after {} ms ({} xact errors)", ep, budget_ms, xact_errs));
            return None; // the caller decides whether that is "busy" or "broken"
        }
        ctx.sleep(ctx.duration_cycles(1));
    }
}

/// One Bulk-Only Transport command: CBW out, optional data, CSW in.
///
/// Returns the bytes moved by the data stage, or `None`.
pub fn bot(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, disk: &mut Disk,
    cdb: &[u8], data_in: bool, dlen: usize,
) -> Option<usize> {
    const TAG: u32 = 0x1234_5678;
    // Read the immutable fields out first: the transfers below borrow the toggles mutably.
    let (ep_in, ep_out, mps) = (disk.ep_in, disk.ep_out, disk.mps);
    // CBW: 31 bytes, "USBC".
    for i in 0..31 {
        dma.write8(CBW_OFF + i, 0);
    }
    for (i, b) in 0x4342_5355u32.to_le_bytes().iter().enumerate() {
        dma.write8(CBW_OFF + i, *b);
    }
    for (i, b) in TAG.to_le_bytes().iter().enumerate() {
        dma.write8(CBW_OFF + 4 + i, *b);
    }
    for (i, b) in (dlen as u32).to_le_bytes().iter().enumerate() {
        dma.write8(CBW_OFF + 8 + i, *b);
    }
    dma.write8(CBW_OFF + 12, if data_in { 0x80 } else { 0x00 });
    dma.write8(CBW_OFF + 13, 0); // LUN
    dma.write8(CBW_OFF + 14, cdb.len().min(16) as u8);
    for (i, b) in cdb.iter().take(16).enumerate() {
        dma.write8(CBW_OFF + 15 + i, *b);
    }

    // NAME THE STAGE THAT FAILED. `?` here discarded which of the three transfers died, and "READ
    // CAPACITY FAILED" with nothing else is exactly the un-diagnosable failure that has cost this
    // port a boot every time it has appeared. CBW-out failing means the device is not accepting
    // commands at all; a data-stage failure means it accepted the command and would not answer; a
    // CSW failure means it did the work and would not report. Three different faults.
    if bulk_xfer(ctx, mmio, t, mps, false, ep_out, dma.phys_at(CBW_OFF) as u32, 31, 2_000, &mut disk.pid_out).is_none() {
        ctx.log_fmt(format_args!(
            "dwc2-svc: BOT CBW-out FAILED (cdb {:#04x}, ep_out {}, mps {})", cdb[0], disk.ep_out, disk.mps));
        return None;
    }

    let mut moved = 0usize;
    if dlen > 0 {
        let want = dlen.min(DATA_MAX);
        if data_in {
            for i in 0..want {
                dma.write8(DATA_OFF + i, 0);
            }
        }
        moved = match bulk_xfer(
            ctx, mmio, t, mps, data_in,
            if data_in { ep_in } else { ep_out },
            dma.phys_at(DATA_OFF) as u32, want as u32, 5_000,
            if data_in { &mut disk.pid_in } else { &mut disk.pid_out })
        {
            Some(n) => n as usize,
            None => {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: BOT data-stage FAILED (cdb {:#04x}, {} bytes {})",
                    cdb[0], want, if data_in { "IN" } else { "OUT" }));
                return None;
            }
        };
    }

    // CSW: 13 bytes, "USBS".
    for i in 0..13 {
        dma.write8(CSW_OFF + i, 0);
    }
    if bulk_xfer(ctx, mmio, t, mps, true, ep_in, dma.phys_at(CSW_OFF) as u32, 13, 2_000, &mut disk.pid_in).is_none() {
        ctx.log_fmt(format_args!("dwc2-svc: BOT CSW-in FAILED (cdb {:#04x})", cdb[0]));
        return None;
    }

    let mut csw = [0u8; 13];
    for i in 0..13 {
        csw[i] = dma.read8(CSW_OFF + i);
    }
    let sig = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    let residue = u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]);

    // A SHORT DATA PHASE IS A FAILED TRANSFER, even when the device says it passed.
    //
    // This is the invariant the kernel driver is most emphatic about, and it earned that: a device is
    // entitled to return a short data phase with status 0, so 100 bytes of a 512-byte read with
    // residue 412 was accepted as a good block, and `fs` received 412 bytes of stale zeros PRESENTED
    // AS DATA. Silent corruption arriving through the device's verdict rather than through the DMA
    // buffer - the one thing a block driver must never do. So the residue is CHECKED, and what the
    // data stage actually moved is compared against what was asked for.
    let short = residue != 0 || moved != dlen.min(DATA_MAX);
    if sig != 0x5342_5355 || tag != TAG || csw[12] != 0 || short {
        ctx.log_fmt(format_args!(
            "dwc2-svc: BOT failed - sig={:#010x} tag={:#010x} residue={} status={} moved={}/{}",
            sig, tag, residue, csw[12], moved, dlen));
        return None;
    }
    Some(moved)
}

/// READ CAPACITY(10): the device's last LBA and block size.
pub fn read_capacity(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, disk: &mut Disk,
) -> Option<(u64, u32)> {
    let cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    bot(ctx, mmio, dma, t, disk, &cdb, true, 8)?;
    let mut b = [0u8; 8];
    for i in 0..8 {
        b[i] = dma.read8(DATA_OFF + i);
    }
    // Both fields are BIG endian - SCSI, not USB. Reading them little-endian gives a plausible but
    // wildly wrong capacity, which is the sort of error that surfaces as corruption much later.
    let last_lba = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
    let block = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    Some((last_lba + 1, block))
}

/// READ(10): one block into the arena's data buffer.
pub fn read_block(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, disk: &mut Disk, lba: u32, block: u32,
) -> bool {
    let cdb = [
        0x28u8, 0,
        (lba >> 24) as u8, (lba >> 16) as u8, (lba >> 8) as u8, lba as u8,
        0, 0, 1, 0,
    ];
    bot(ctx, mmio, dma, t, disk, &cdb, true, block as usize).is_some()
}


// --- The block IPC protocol (what `block-driver` already speaks on the Pi 4) -------------------
//
// Identical wire format, deliberately: `block-driver`'s `xhciblk` client is reused unchanged, so
// this port adds one hop and changes nothing `fs` sees.
pub const OP_READ_BLOCK: u8 = 1;
pub const OP_WRITE_BLOCK: u8 = 2;
pub const OP_CAPACITY: u8 = 3;
pub const OP_WRITE_ZEROS: u8 = 4;
pub const OP_FLUSH: u8 = 5;
pub const STATUS_OK: u8 = 0;
pub const STATUS_ERR: u8 = 1;

/// WRITE(10): one block from the arena's data buffer.
pub fn write_block(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, disk: &mut Disk, lba: u32, block: u32,
) -> bool {
    let cdb = [
        0x2Au8, 0,
        (lba >> 24) as u8, (lba >> 16) as u8, (lba >> 8) as u8, lba as u8,
        0, 0, 1, 0,
    ];
    bot(ctx, mmio, dma, t, disk, &cdb, false, block as usize).is_some()
}

/// Serve one block request. Returns false if the message was not one.
///
/// The reply cap is the ONLY authority to answer (§8.5), and a request without one cannot be
/// answered at all - so it is dropped LOUDLY rather than silently, which is the fault the userspace
/// audit found in the GENET backend and fixed for the same reason.
pub fn serve(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target, disk: &mut Disk,
    msg: &godspeed_sdk::Message, sectors: u64, capless_logged: &mut bool,
) -> bool {
    let p = msg.payload_bytes();
    if p.is_empty() {
        return false;
    }
    let reply = match ctx.take_pending_cap() {
        Some(c) => c,
        None => {
            if !*capless_logged {
                *capless_logged = true;
                ctx.log("dwc2-svc: block request had no reply cap - dropping (cannot answer without one)");
            }
            return true;
        }
    };

    let mut out = [0u8; 520];
    let n = match p[0] {
        OP_CAPACITY => {
            out[0] = STATUS_OK;
            out[1..9].copy_from_slice(&sectors.to_le_bytes());
            9
        }
        OP_READ_BLOCK if p.len() >= 9 => {
            let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]) as u32;
            if read_block(ctx, mmio, dma, t, disk, lba, 512) {
                out[0] = STATUS_OK;
                for i in 0..512 {
                    out[1 + i] = dma.read8(DATA_OFF + i);
                }
                513
            } else {
                // STATUS_ERR and NOTHING else. A short reply carrying STATUS_OK would have the
                // client copy whatever its message buffer held into a sector `fs` then trusts -
                // which is the corruption the client's own length check exists to catch, and it
                // should never have to.
                out[0] = STATUS_ERR;
                1
            }
        }
        OP_WRITE_BLOCK if p.len() >= 521 => {
            for i in 0..512 {
                dma.write8(DATA_OFF + i, p[9 + i]);
            }
            let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]) as u32;
            out[0] = if write_block(ctx, mmio, dma, t, disk, lba, 512) { STATUS_OK } else { STATUS_ERR };
            1
        }
        OP_FLUSH => {
            // SYNCHRONIZE CACHE(10). This board's stick REFUSES it outright (CLAUDE.md §6.1,
            // amendment 2026-07-25), and that refusal is a FACT ABOUT THE DEVICE rather than an
            // error: reporting it as failure would make `fs` treat every flush as a fault. It is
            // answered honestly - the constitution's crash-recovery guarantee is backend-conditional
            // precisely because of this device.
            let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            out[0] = if bot(ctx, mmio, dma, t, disk, &cdb, false, 0).is_some() { STATUS_OK } else { STATUS_ERR };
            1
        }
        _ => {
            out[0] = STATUS_ERR;
            1
        }
    };

    // The outcome is CHECKED, not discarded. A failed reply means the client waits out its deadline
    // and calls this driver unresponsive while our log shows a clean run (userspace audit A8-3).
    if ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n])).is_err()
        && !*capless_logged
    {
        *capless_logged = true;
        ctx.log("dwc2-svc: block reply send FAILED - the requester will time out");
    }
    ctx.remove_cap(reply);
    true
}
