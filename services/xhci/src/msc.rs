// SPDX-License-Identifier: GPL-2.0-only
//! USB mass storage (Bulk-Only Transport + SCSI transparent) for the `xhci` service.
//!
//! This is the piece that was missing from the userspace USB driver, and its absence is the whole
//! reason the kernel still contains a USB stack: `services/xhci` drove keyboards and mice, so
//! building with the driver in userspace cost the disk, and a build that costs the disk does not
//! become the default. Bulk transfers are the last capability the in-kernel stack had that this one
//! did not.
//!
//! ## What BOT is
//!
//! Three bulk transfers, in order, and nothing else:
//!
//! 1. **CBW** (Command Block Wrapper, 31 bytes) out on the bulk-OUT endpoint - signature `USBC`, a
//!    tag we choose, the expected data length, the direction, and the SCSI command itself.
//! 2. **Data**, optional, on whichever bulk endpoint the direction named.
//! 3. **CSW** (Command Status Wrapper, 13 bytes) in on the bulk-IN endpoint - the same tag back, and
//!    a status byte.
//!
//! Deliberately literal. There is no queueing, no overlap and no pipelining: one command is in
//! flight at a time and each of the three transfers is awaited before the next is posted. That is
//! what makes the endpoint rings provably un-fillable (see `Ring::push`) and it is what makes a
//! failure attributable to one transfer rather than to "something in the pipe".
//!
//! ## The tag check is not a formality
//!
//! A CSW whose tag does not match the CBW we just sent is the answer to an EARLIER command, which is
//! exactly what a device hands you after a stall. Accepting it would attribute one command's status
//! to another - a read reported good because the *previous* write succeeded. It is treated as a
//! failure of this command, which is the honest reading (§26.7).
//!
//! ## Reference
//!
//! Reimplemented from our own hardware-proven `kernel/src/arch/aarch64/xhci.rs`, which carries two
//! findings worth restating because they cost days:
//!
//! - **The recovery was causing the wedges.** A timeout budget of 2 s aborted commands a healthy
//!   stick was still working on (a block remap takes seconds), and the abort desynchronised the
//!   endpoint that recovery then repaired. The budget here is a real duration and it is generous.
//! - **One shared DMA page with three owners aliases.** An armed interrupt endpoint DMAs into its
//!   buffer on any keypress, regardless of which driver code thinks it holds the device. The disk's
//!   CBW/CSW page here is the disk's alone - see `DISK_BASE`.

use godspeed_sdk::{Dma, Mmio};

use crate::{next_event, TRB_NORMAL, TRB_SIZE, TRB_TRANSFER_EVENT};

/// The disk's own DMA region, past the scratchpad tail (`SCRATCHPAD_BUF_BASE + MAX_SCRATCHPAD`).
///
/// Four pages, fixed, never allocated from anything (§26.6 - the bound is readable off this file).
/// It is the disk's ALONE: the aliasing bug that cost the Pi 4 port days was one page shared by a
/// keyboard report, a hub's port status and the disk's CBW, where an armed interrupt endpoint
/// overwrote a command mid-flight on every keypress. Separate regions make that unrepresentable
/// rather than merely avoided.
pub const DISK_BASE: usize = 0x12_0000;
/// Bulk-OUT transfer ring (one page).
const OUT_RING: usize = DISK_BASE;
/// Bulk-IN transfer ring (one page).
const IN_RING: usize = DISK_BASE + 0x1000;
/// CBW at +0, CSW at +64. Kept 64 bytes apart so a device that writes a longer-than-expected CSW
/// cannot land on the next command's CBW signature.
const CMD_PAGE: usize = DISK_BASE + 0x2000;
const CSW_OFF: usize = CMD_PAGE + 64;
/// Data buffer (one page). Caps a single transfer at 8 sectors of 512 B.
const DATA_BUF: usize = DISK_BASE + 0x3000;
/// Total size of the region, for the arena bound check at startup.
pub const DISK_REGION_BYTES: usize = 0x4000;

/// Largest data payload one BOT command may carry - the data page.
pub const MAX_XFER: u32 = 0x1000;
/// The only sector size this driver speaks. A device reporting anything else is refused rather than
/// driven with a wrong stride, which would read plausible bytes from the wrong offsets.
pub const SECTOR: u32 = 512;

/// How long to wait for one bulk transfer to complete, in `next_event` polling attempts.
///
/// This is the constant that taught the port "a count is not a duration" TWICE. It is generous on
/// purpose: a USB stick doing an internal block remap legitimately stops answering for seconds, and
/// the previous 2 s budget aborted live commands and then blamed the device for the desynchronised
/// endpoint that followed. 62 wedges in one run were all ours. A device that is merely slow must be
/// waited out; only a device that never answers at all is broken.
const XFER_TRIES: u32 = 300_000_000;

/// A bulk endpoint's transfer ring: a one-page producer cursor with a Link TRB at the tail.
///
/// The ring can never fill, and that is a proof rather than a hope: BOT is strictly synchronous, so
/// at most ONE TRB is outstanding on either ring at any moment (every push is followed by an await
/// before the next). A fullness check here could never fire, and a check that cannot fire is not
/// evidence of anything - so the invariant is stated instead of tested, and the only bookkeeping is
/// the wrap.
struct Ring {
    base: usize,
    cur: usize,
    /// Producer Cycle State: flipped on every wrap so the controller can tell fresh TRBs from the
    /// stale ones left behind by the previous lap.
    pcs: u32,
}

impl Ring {
    const BYTES: usize = 0x1000;

    fn new(base: usize) -> Self {
        Ring { base, cur: 0, pcs: 1 }
    }

    /// Post one Normal TRB pointing at `buf_phys` for `len` bytes, with Interrupt On Completion.
    fn push(&mut self, dma: &Dma, buf_phys: u64, len: u32) {
        // Wrap if this TRB plus the Link that would follow it do not both fit. The Link is written
        // AT the cursor (where the controller's dequeue pointer sits), so there is no stale gap in
        // front of it for the controller to stop on.
        if self.cur + 2 * TRB_SIZE > Self::BYTES {
            let bp = dma.phys_at(self.base);
            dma.write32(self.base + self.cur, bp as u32);
            dma.write32(self.base + self.cur + 4, (bp >> 32) as u32);
            dma.write32(self.base + self.cur + 8, 0);
            // Toggle Cycle (bit 1) tells the controller to flip ITS cycle when it follows the link.
            dma.write32(self.base + self.cur + 12, (crate::TRB_LINK << 10) | (1 << 1) | self.pcs);
            self.cur = 0;
            self.pcs ^= 1;
        }
        let t = self.base + self.cur;
        dma.write32(t, buf_phys as u32);
        dma.write32(t + 4, (buf_phys >> 32) as u32);
        // TD Transfer Size in 21:0. Interrupter target 0.
        dma.write32(t + 8, len & 0x1F_FFFF);
        // Cycle | IOC (bit 5) | type. IOC is what makes the transfer produce the event we await.
        dma.write32(t + 12, self.pcs | (1 << 5) | (TRB_NORMAL << 10));
        self.cur += TRB_SIZE;
    }
}

/// A bound mass-storage device: the coordinates needed to talk to it and the geometry it reported.
pub struct Disk {
    pub slot: u32,
    /// Device Context Index of the bulk-OUT endpoint (`num * 2`).
    out_dci: u32,
    /// Device Context Index of the bulk-IN endpoint (`num * 2 + 1`).
    in_dci: u32,
    out_ring: Ring,
    in_ring: Ring,
    /// CBW tag, incremented per command. Only its match in the CSW is meaningful.
    tag: u32,
    /// Total addressable sectors, from READ CAPACITY(10).
    pub sectors: u64,
    /// The root port the device sits on, so a disconnect can be noticed.
    pub port: u32,
}

impl Disk {
    pub fn new(slot: u32, out_ep: u8, in_ep: u8, port: u32) -> Self {
        Disk {
            slot,
            out_dci: (out_ep & 0x0F) as u32 * 2,
            in_dci: (in_ep & 0x0F) as u32 * 2 + 1,
            out_ring: Ring::new(OUT_RING),
            in_ring: Ring::new(IN_RING),
            tag: 1,
            sectors: 0,
            port,
        }
    }

    /// Physical address of the bulk-OUT ring, for the Configure Endpoint input context.
    pub fn out_ring_phys(&self, dma: &Dma) -> u64 {
        dma.phys_at(OUT_RING)
    }
    pub fn in_ring_phys(&self, dma: &Dma) -> u64 {
        dma.phys_at(IN_RING)
    }
    pub fn out_dci(&self) -> u32 {
        self.out_dci
    }
    pub fn in_dci(&self) -> u32 {
        self.in_dci
    }
}

/// Await the completion of a transfer on `slot`, ignoring events belonging to anything else.
///
/// A keyboard's interrupt endpoint stays ARMED, so its completions arrive at any time - including in
/// the middle of a disk command. Matching on slot is what keeps a keystroke from being read as this
/// transfer's status. Returns the completion code, or `None` if nothing arrived in the budget.
#[allow(clippy::too_many_arguments)]
fn await_on_slot(
    dma: &Dma,
    mmio: &Mmio,
    ir0: usize,
    slot: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut u32,
) -> Option<u32> {
    // Bounded: at most this many unrelated events before we give up rather than spin forever on a
    // storm (§26.6 - every device loop is bounded).
    for _ in 0..64 {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, XFER_TRIES) {
            Some((TRB_TRANSFER_EVENT, cc, sid)) if sid == slot => return Some(cc),
            // Another device's transfer completed (a keystroke). Record it so the caller can re-arm
            // that endpoint - its in-flight TRB is spent - and keep waiting for ours.
            Some((TRB_TRANSFER_EVENT, _, sid)) => {
                if sid < 32 {
                    *eaten |= 1 << sid;
                }
            }
            Some(_) => {} // port change or command completion; not ours
            None => return None,
        }
    }
    None
}

/// One Bulk-Only Transport command: CBW out, optional data, CSW in.
///
/// Returns the CSW status byte (0 = good), or `None` if a transfer failed or the CSW answered a
/// different command. `data_len` bytes are exchanged through `DATA_BUF`; the caller fills it before
/// an OUT and reads it after an IN.
#[allow(clippy::too_many_arguments)]
pub fn bot(
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    cmd: &[u8],
    data_len: u32,
    data_in: bool,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut u32,
) -> Option<u8> {
    if data_len > MAX_XFER {
        return None; // caller asked for more than the bounded data page holds
    }
    let tag = d.tag;
    d.tag = d.tag.wrapping_add(1);

    // Command Block Wrapper: 31 bytes, little-endian, signature 'USBC'.
    for i in 0..31 {
        dma.write8(CMD_PAGE + i, 0);
    }
    dma.write32(CMD_PAGE, 0x4342_5355); // 'USBC' little-endian
    dma.write32(CMD_PAGE + 4, tag);
    dma.write32(CMD_PAGE + 8, data_len);
    dma.write8(CMD_PAGE + 12, if data_in { 0x80 } else { 0x00 });
    dma.write8(CMD_PAGE + 13, 0); // LUN 0
    dma.write8(CMD_PAGE + 14, cmd.len() as u8);
    for (i, b) in cmd.iter().take(16).enumerate() {
        dma.write8(CMD_PAGE + 15 + i, *b);
    }

    d.out_ring.push(dma, dma.phys_at(CMD_PAGE), 31);
    mmio.write32(dboff + d.slot as usize * 4, d.out_dci);
    let cc = await_on_slot(dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
    if cc != 1 && cc != 13 {
        return None;
    }

    if data_len > 0 {
        let dci = if data_in { d.in_dci } else { d.out_dci };
        let ring = if data_in { &mut d.in_ring } else { &mut d.out_ring };
        ring.push(dma, dma.phys_at(DATA_BUF), data_len);
        mmio.write32(dboff + d.slot as usize * 4, dci);
        let cc = await_on_slot(dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
        // 13 is Short Packet: the device sent less than asked, which for a data stage is normal.
        if cc != 1 && cc != 13 {
            return None;
        }
    }

    d.in_ring.push(dma, dma.phys_at(CSW_OFF), 13);
    mmio.write32(dboff + d.slot as usize * 4, d.in_dci);
    let cc = await_on_slot(dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
    if cc != 1 && cc != 13 {
        return None;
    }
    // A CSW whose tag is not ours answers an EARLIER command - what a device hands you after a
    // stall. Reporting its status as this command's would attribute one result to another.
    if dma.read32(CSW_OFF + 4) != tag {
        return None;
    }
    Some(dma.read8(CSW_OFF + 12))
}

/// SCSI TEST UNIT READY - is the medium present and the device willing?
#[allow(clippy::too_many_arguments)]
pub fn test_unit_ready(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, d: &mut Disk,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut u32,
) -> bool {
    let cmd = [0u8; 6];
    bot(dma, mmio, dboff, ir0, d, &cmd, 0, true, ev_idx, ev_cycle, eaten) == Some(0)
}

/// SCSI READ CAPACITY(10) - returns the sector COUNT.
///
/// The command reports the LAST ADDRESSABLE BLOCK, not the count, so the `+ 1` is load-bearing:
/// without it a filesystem believes in one fewer sector than exists, and with a `- 1` mistake it
/// believes in one the device does not have.
#[allow(clippy::too_many_arguments)]
pub fn read_capacity(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, d: &mut Disk,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut u32,
) -> Option<u64> {
    let cmd = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    if bot(dma, mmio, dboff, ir0, d, &cmd, 8, true, ev_idx, ev_cycle, eaten) != Some(0) {
        return None;
    }
    // Both fields are BIG-endian, unlike everything else in BOT.
    let last_lba = u32::from_be(dma.read32(DATA_BUF)) as u64;
    let blen = u32::from_be(dma.read32(DATA_BUF + 4));
    if blen != SECTOR {
        return None; // reported by the caller, which has the context to say which device
    }
    Some(last_lba + 1)
}

/// SCSI READ(10) into the data buffer. `count` sectors, at most `MAX_XFER / SECTOR`.
#[allow(clippy::too_many_arguments)]
pub fn read10(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, d: &mut Disk, lba: u64, count: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut u32,
) -> bool {
    if count == 0 || count * SECTOR > MAX_XFER || lba > u32::MAX as u64 {
        return false;
    }
    let l = lba as u32;
    let cmd = [
        0x28, 0,
        (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8,
        0, (count >> 8) as u8, count as u8, 0,
    ];
    bot(dma, mmio, dboff, ir0, d, &cmd, count * SECTOR, true, ev_idx, ev_cycle, eaten) == Some(0)
}

/// SCSI WRITE(10) from the data buffer, which the caller fills first via `data_write`.
#[allow(clippy::too_many_arguments)]
pub fn write10(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, d: &mut Disk, lba: u64, count: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut u32,
) -> bool {
    if count == 0 || count * SECTOR > MAX_XFER || lba > u32::MAX as u64 {
        return false;
    }
    let l = lba as u32;
    let cmd = [
        0x2A, 0,
        (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8,
        0, (count >> 8) as u8, count as u8, 0,
    ];
    bot(dma, mmio, dboff, ir0, d, &cmd, count * SECTOR, false, ev_idx, ev_cycle, eaten) == Some(0)
}

/// SCSI SYNCHRONIZE CACHE(10) - the durability barrier a journal's ordering depends on.
///
/// Its outcome is RETURNED, not swallowed. A device that refuses this (the Pi 2's stick does) leaves
/// the filesystem crash-recoverable only in the weaker, backend-conditional sense the constitution
/// records in §6.1, and the caller must be able to say so rather than imply a guarantee it cannot
/// deliver.
#[allow(clippy::too_many_arguments)]
pub fn sync_cache(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, d: &mut Disk,
    ev_idx: &mut usize, ev_cycle: &mut u32, eaten: &mut u32,
) -> bool {
    let cmd = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    bot(dma, mmio, dboff, ir0, d, &cmd, 0, true, ev_idx, ev_cycle, eaten) == Some(0)
}

/// Read one byte out of the data buffer (after a `read10`).
pub fn data_read8(dma: &Dma, off: usize) -> u8 {
    dma.read8(DATA_BUF + off)
}

/// Write one byte into the data buffer (before a `write10`).
pub fn data_write8(dma: &Dma, off: usize, val: u8) {
    dma.write8(DATA_BUF + off, val);
}

/// A mass-storage interface found in a configuration descriptor.
pub struct MscInfo {
    pub cfg_value: u8,
    pub iface: u8,
    pub in_ep: u8,
    pub out_ep: u8,
    pub mps: u16,
}

/// A bulk endpoint must have a non-zero endpoint NUMBER, not merely a non-zero address.
///
/// `dci = num * 2 + in`, so `num == 0` gives `out_dci == 0` - whose add-context flag collides with
/// the slot flag, writing endpoint fields over the slot context - and `in_dci == 1`, which would
/// reconfigure EP0 as a bulk pipe. Both are caught downstream as a Configure Endpoint parameter
/// error, so this is legibility rather than corruption: code parsing a stranger's descriptors should
/// reject them where the reason can still be named.
fn ep_num_ok(addr: u8) -> bool {
    addr & 0x0F != 0
}

/// Walk a configuration descriptor for a **Bulk-Only Transport, SCSI transparent** interface.
///
/// Class 8 / subclass 6 / protocol 0x50 is what essentially every USB stick presents, and the only
/// combination this driver speaks. A UAS-only device or a CBI device is left alone rather than
/// half-driven: storage driven by a protocol it does not implement returns plausible garbage exactly
/// where a filesystem expects blocks.
pub fn parse_msc(dma: &Dma, buf_off: usize, buf_len: usize) -> Option<MscInfo> {
    let total = (dma.read16(buf_off + 2) as usize).min(buf_len);
    let cfg_value = dma.read8(buf_off + 5);

    let mut i = 9usize; // past the configuration descriptor itself
    let mut in_msc = false;
    let mut iface = 0u8;
    let (mut in_ep, mut out_ep, mut mps) = (0u8, 0u8, 0u16);
    while i + 2 <= total {
        let len = dma.read8(buf_off + i) as usize;
        let ty = dma.read8(buf_off + i + 1);
        // A malformed descriptor ENDS the walk rather than spinning it: a zero length would
        // otherwise leave `i` fixed forever.
        if len < 2 || i + len > total {
            break;
        }
        if ty == 0x04 && len >= 9 {
            in_msc = dma.read8(buf_off + i + 5) == 0x08
                && dma.read8(buf_off + i + 6) == 0x06
                && dma.read8(buf_off + i + 7) == 0x50;
            if in_msc {
                iface = dma.read8(buf_off + i + 2);
            } else {
                // Another interface's endpoints must not be adopted by the storage one.
                in_ep = 0;
                out_ep = 0;
            }
        } else if ty == 0x05 && len >= 7 && in_msc {
            let addr = dma.read8(buf_off + i + 2);
            // Attributes bits 1:0 == 2 is Bulk. An interrupt endpoint on the same interface is not a
            // data pipe, and taking it would send commands somewhere nothing reads them.
            if dma.read8(buf_off + i + 3) & 0x03 == 0x02 {
                mps = dma.read16(buf_off + i + 4) & 0x7FF;
                if addr & 0x80 != 0 {
                    in_ep = addr;
                } else {
                    out_ep = addr;
                }
            }
        }
        i += len;
    }
    if in_msc && ep_num_ok(in_ep) && ep_num_ok(out_ep) && mps != 0 {
        Some(MscInfo { cfg_value, iface, in_ep, out_ep, mps })
    } else {
        None
    }
}
