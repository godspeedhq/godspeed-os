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

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::{next_event, EvMail, TRB_NORMAL, TRB_SIZE, TRB_TRANSFER_EVENT};

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

/// How long to wait for one bulk transfer to complete - a REAL DURATION, in milliseconds.
///
/// This constant was first written as `XFER_TRIES = 300_000_000`, a raw spin count handed to
/// `next_event`, with a comment claiming it was "a real duration and generous". It was neither. A
/// count is not a duration: the same number is seconds on one machine and minutes on another,
/// because what it actually measures is how fast the polling loop happens to run - and on AArch64
/// each poll is an UNCACHED DMA read, so it runs far slower than the x86 figure it was eyeballed
/// against. Worse, `await_on_slot` looped up to 64 times around it, so the true worst case was ~19
/// billion reads: a hang wearing a timeout's clothing.
///
/// This port has now been taught that lesson three times (chaos `PACE_YIELDS`, the storage
/// `BUSY_RETRIES`, and this). The tell is always the same: **a bound whose real value depends on how
/// fast the loop happens to run.** Bound by the clock and report seconds.
///
/// 30 s is deliberately generous, for the reason the predecessor's 2 s budget was not: a stick doing
/// an internal block remap legitimately stops answering for seconds, and aborting a command it is
/// still working on is what CAUSED the wedges that recovery was then built to clean up.
const XFER_TIMEOUT_MS: u64 = 30_000;

/// Poll iterations per `next_event` call. Small on purpose: it is only the granularity at which the
/// CLOCK is re-checked, never the bound itself.
const POLL_GRANULARITY: u32 = 4096;

/// Unrelated transfer events tolerated while waiting for ours. Bounds an event storm (a keyboard
/// held down) without bounding the WAIT, which is the clock's job.
const MAX_UNRELATED_EVENTS: u32 = 4096;

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
        Ring {
            base,
            cur: 0,
            pcs: 1,
        }
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
            dma.write32(
                self.base + self.cur + 12,
                (crate::TRB_LINK << 10) | (1 << 1) | self.pcs,
            );
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
/// Set once "no disk is bound" has been reported, so the refusal is not logged per request.
static NO_DISK_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// As `NO_DISK_LOGGED`, for a bound disk whose reads have started failing.
static READ_FAIL_LOGGED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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
    /// The parent hub's slot, and the hub port this disk occupies (0 = on a root port).
    ///
    /// The hot-plug scan walks a hub's downstream ports looking for devices it does not already
    /// know about, and it knew only about bound HIDs. A disk behind a hub therefore read as a NEW
    /// device on every pass, re-enumerating the controller forever - 12 times in one boot on the
    /// Pi 4, tearing down the keyboard it had just bound each time. These two fields are what let
    /// that scan account for it.
    pub hub_slot: u32,
    pub hub_port: u32,
    /// The parent hub's DMA slice, its downstream port count, and the byte offset just past the
    /// enumeration TRBs on its EP0 ring.
    ///
    /// Enough to POLL that hub's downstream ports. Without it, a machine whose only bound device is
    /// this disk scans no hub at all - so a keyboard plugged in behind the same hub is never seen,
    /// because a device behind a hub changes no root PORTSC when it arrives.
    pub hub_dev: u32,
    pub hub_nports: u32,
    pub hub_off: usize,
}

impl Disk {
    pub fn new(slot: u32, out_ep: u8, in_ep: u8, port: u32) -> Self {
        // A disk exists again - let the next absence be reported.
        NO_DISK_LOGGED.store(false, core::sync::atomic::Ordering::Relaxed);
        READ_FAIL_LOGGED.store(false, core::sync::atomic::Ordering::Relaxed);
        Disk {
            slot,
            out_dci: (out_ep & 0x0F) as u32 * 2,
            in_dci: (in_ep & 0x0F) as u32 * 2 + 1,
            out_ring: Ring::new(OUT_RING),
            in_ring: Ring::new(IN_RING),
            tag: 1,
            sectors: 0,
            port,
            hub_slot: 0,
            hub_port: 0,
            hub_dev: 0,
            hub_nports: 0,
            hub_off: 0,
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
/// transfer's status. Returns the completion code, or `None` if the device did not answer inside
/// `XFER_TIMEOUT_MS`.
///
/// The bound is the CLOCK. Two things are counted as well, and neither is the timeout: the poll
/// granularity (how often the clock is re-read) and the number of unrelated events tolerated (an
/// event storm must not livelock this loop, §26.6). Mixing those up is what produced a 19-billion-
/// iteration "timeout" in the first draft.
#[allow(clippy::too_many_arguments)]
fn await_on_slot(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    ir0: usize,
    slot: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> Option<u32> {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(XFER_TIMEOUT_MS));
    let mut unrelated = 0u32;
    // Our answer may already be in hand: another consumer of the shared event ring can have
    // dequeued it and filed it for us. Check the mailbox before touching the ring, or we would wait
    // out a deadline for a completion that already arrived.
    if let Some(cc) = eaten.take(slot) {
        return Some(cc);
    }
    loop {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, POLL_GRANULARITY) {
            Some((TRB_TRANSFER_EVENT, cc, sid)) if sid == slot => return Some(cc),
            // Another consumer's transfer completed. FILE IT - do not just note that it happened.
            //
            // This arm used to record a re-arm bit and DISCARD the completion, which was fine for a
            // keystroke (the caller only needs to know the endpoint's TRB is spent) and destructive
            // for anything with a waiter. A hub port-status probe posts its TD, gives up after its
            // budget, and its answer retires during the NEXT pass's serve segment - right here,
            // where it was silently dropped. The probe therefore never got an answer, and a probe
            // with no answer correctly concludes nothing, so USB hot-plug did nothing at all.
            //
            // Measured: 328 probes posted, 151 answered, and ZERO late answers seen by the drain -
            // because they never reached the drain. They were eaten here.
            // `docs/xhci-completion-correlation.md`.
            Some((TRB_TRANSFER_EVENT, cc, sid)) => {
                eaten.put(sid, cc);
                unrelated += 1;
                if unrelated >= MAX_UNRELATED_EVENTS {
                    ctx.log("xhci: gave up waiting for a disk transfer - too many unrelated events");
                    return None;
                }
            }
            Some(_) => {} // port change or command completion; not ours
            None => {}    // this poll window saw nothing; the clock below decides whether to stop
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            ctx.log_fmt(format_args!(
                "xhci: disk transfer got no completion in {} s - the device stopped answering",
                XFER_TIMEOUT_MS / 1000
            ));
            return None;
        }
    }
}

/// One Bulk-Only Transport command: CBW out, optional data, CSW in.
///
/// Returns the CSW status byte (0 = good), or `None` if a transfer failed or the CSW answered a
/// different command. `data_len` bytes are exchanged through `DATA_BUF`; the caller fills it before
/// an OUT and reads it after an IN.
#[allow(clippy::too_many_arguments)]
pub fn bot(
    ctx: &ServiceContext,
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
    eaten: &mut EvMail,
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
    let cc = await_on_slot(ctx, dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
    if cc != 1 && cc != 13 {
        return None;
    }

    if data_len > 0 {
        let dci = if data_in { d.in_dci } else { d.out_dci };
        let ring = if data_in {
            &mut d.in_ring
        } else {
            &mut d.out_ring
        };
        ring.push(dma, dma.phys_at(DATA_BUF), data_len);
        mmio.write32(dboff + d.slot as usize * 4, dci);
        let cc = await_on_slot(ctx, dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
        // 13 is Short Packet: the device sent less than asked, which for a data stage is normal.
        if cc != 1 && cc != 13 {
            return None;
        }
    }

    d.in_ring.push(dma, dma.phys_at(CSW_OFF), 13);
    mmio.write32(dboff + d.slot as usize * 4, d.in_dci);
    let cc = await_on_slot(ctx, dma, mmio, ir0, d.slot, ev_idx, ev_cycle, eaten)?;
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> bool {
    let cmd = [0u8; 6];
    bot(
        ctx,
        dma, mmio, dboff, ir0, d, &cmd, 0, true, ev_idx, ev_cycle, eaten,
    ) == Some(0)
}

/// SCSI READ CAPACITY(10) - returns the sector COUNT.
///
/// The command reports the LAST ADDRESSABLE BLOCK, not the count, so the `+ 1` is load-bearing:
/// without it a filesystem believes in one fewer sector than exists, and with a `- 1` mistake it
/// believes in one the device does not have.
#[allow(clippy::too_many_arguments)]
pub fn read_capacity(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> Option<u64> {
    let cmd = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    if bot(
        ctx,
        dma, mmio, dboff, ir0, d, &cmd, 8, true, ev_idx, ev_cycle, eaten,
    ) != Some(0)
    {
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    lba: u64,
    count: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> bool {
    // `count * SECTOR` is checked as a DIVISION, not a multiplication: `count` is attacker- or
    // caller-supplied and `count * SECTOR` overflows u32 above 8388608, wrapping to a SMALL value
    // that sails through a naive bound. The command would then name one length and the data stage
    // transfer another - a protocol desync, not a rejected request.
    if count == 0 || count > MAX_XFER / SECTOR || lba > u32::MAX as u64 {
        return false;
    }
    let l = lba as u32;
    let cmd = [
        0x28,
        0,
        (l >> 24) as u8,
        (l >> 16) as u8,
        (l >> 8) as u8,
        l as u8,
        0,
        (count >> 8) as u8,
        count as u8,
        0,
    ];
    bot(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        d,
        &cmd,
        count * SECTOR,
        true,
        ev_idx,
        ev_cycle,
        eaten,
    ) == Some(0)
}

/// SCSI WRITE(10) from the data buffer, which the caller fills first via `data_write`.
#[allow(clippy::too_many_arguments)]
pub fn write10(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    lba: u64,
    count: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> bool {
    // `count * SECTOR` is checked as a DIVISION, not a multiplication: `count` is attacker- or
    // caller-supplied and `count * SECTOR` overflows u32 above 8388608, wrapping to a SMALL value
    // that sails through a naive bound. The command would then name one length and the data stage
    // transfer another - a protocol desync, not a rejected request.
    if count == 0 || count > MAX_XFER / SECTOR || lba > u32::MAX as u64 {
        return false;
    }
    let l = lba as u32;
    let cmd = [
        0x2A,
        0,
        (l >> 24) as u8,
        (l >> 16) as u8,
        (l >> 8) as u8,
        l as u8,
        0,
        (count >> 8) as u8,
        count as u8,
        0,
    ];
    bot(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        d,
        &cmd,
        count * SECTOR,
        false,
        ev_idx,
        ev_cycle,
        eaten,
    ) == Some(0)
}

/// SCSI SYNCHRONIZE CACHE(10) - the durability barrier a journal's ordering depends on.
///
/// Its outcome is RETURNED, not swallowed. A device that refuses this (the Pi 2's stick does) leaves
/// the filesystem crash-recoverable only in the weaker, backend-conditional sense the constitution
/// records in §6.1, and the caller must be able to say so rather than imply a guarantee it cannot
/// deliver.
#[allow(clippy::too_many_arguments)]
pub fn sync_cache(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    d: &mut Disk,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> bool {
    let cmd = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    bot(
        ctx,
        dma, mmio, dboff, ir0, d, &cmd, 0, true, ev_idx, ev_cycle, eaten,
    ) == Some(0)
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
        Some(MscInfo {
            cfg_value,
            iface,
            in_ep,
            out_ep,
            mps,
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Block-IPC server
// ---------------------------------------------------------------------------------------------
//
// This is what makes the userspace disk REACHABLE. Reading sectors is useless if nothing can ask
// for one, and until now the only way to reach a USB disk was the four `usb_disk_*` syscalls -
// which exist solely to expose the IN-KERNEL stack. Serving the block protocol here is what lets
// those syscalls, and the stack behind them, be deleted.
//
// The wire protocol is EXACTLY the one `block-driver` already speaks to `fs`, deliberately: `fs`
// does not learn that its disk moved out of the kernel, and `block-driver` translates nothing. The
// chain becomes fs -> block-driver -> xhci -> device, with the same bytes at every hop.

/// Block-IPC opcodes. These are not ours to choose - they must match `block-driver`'s constants,
/// because the whole point is that this speaks the protocol that already exists.
pub const OP_READ_BLOCK: u8 = 1;
pub const OP_WRITE_BLOCK: u8 = 2;
pub const OP_CAPACITY: u8 = 3;
pub const OP_WRITE_ZEROS: u8 = 4;
pub const OP_FLUSH: u8 = 5;
pub const STATUS_OK: u8 = 0;
pub const STATUS_ERR: u8 = 1;

/// Build the reply for one block-IPC request, into `out`, returning how many bytes of it are valid.
///
/// Returns `STATUS_ERR` rather than nothing whenever the request cannot be honoured - a malformed
/// request, no disk attached, or a device that failed. A caller blocked awaiting a reply must always
/// get one; silently dropping the request would hang `fs` on a disk that merely returned an error
/// (§26.7 - a failure has to be visible, and an answer that never comes is the least visible kind).
#[allow(clippy::too_many_arguments)]
pub fn serve_block(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    disk: &mut Option<Disk>,
    req: &[u8],
    out: &mut [u8; 520],
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut EvMail,
) -> usize {
    out[0] = STATUS_ERR;
    if req.is_empty() {
        return 1;
    }
    let Some(d) = disk.as_mut() else {
        // A defined error, not a hang - but SAY SO. "refused, status -1" on the client side cannot
        // tell "no disk is bound here" from "the transfer failed", and those are different bugs
        // needing different fixes.
        // ONCE, not per request.
        //
        // With the stick out and `fs` still mounted, `fs` retries block reads indefinitely - and this
        // line fired on every one. Serial output costs about a millisecond a line, so the driver
        // ended up spending its pass writing logs instead of polling the keyboard, and typing
        // degraded the longer the machine sat with no disk. The diagnostic became the load it was
        // added to diagnose.
        //
        // The fact is worth saying once per drop; saying it ten times a second adds nothing and costs
        // the thing the user actually notices. Reset when a disk is bound again (see `Disk::new`).
        if !NO_DISK_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            ctx.log("xhci: block request but NO disk is bound - refusing (silenced until one is)");
        }
        return 1;
    };

    match req[0] {
        OP_CAPACITY => {
            // Ask the DEVICE whether it is still there before quoting its size.
            //
            // `d.sectors` is READ CAPACITY as it read at BIND time. Serving it unconditionally made
            // this the last stale copy in the chain: with the stick pulled, the log showed
            // block-driver correctly reporting "no USB disk attached" on every read while, in the
            // same second, fs reported "capacity 31266816 sectors" - because the capacity path never
            // consulted the device at all. `drives` then printed a row reading "raw ... not
            // formatted", which describes a blank disk that is PRESENT rather than one that is gone.
            //
            // The same Commandment III error was fixed in `block-driver` (it held its own boot-time
            // snapshot) and this was simply one layer further down, still holding one. A derived view
            // is allowed only while something reconciles it; nothing reconciled this.
            //
            // TEST UNIT READY is the cheapest question that has a truthful answer, and this is not a
            // hot path - a capacity is asked at mount and by `drives`. A device that does not answer
            // it is reported as an error, and the caller in `main.rs` drops the disk and re-scans,
            // exactly as a failed read already does.
            // Answered from WHETHER A DISK IS BOUND - no device round trip.
            //
            // Two earlier versions of this were wrong in opposite directions. The first quoted
            // `d.sectors` unconditionally, so an unplugged stick still reported 15267 MiB. The second
            // asked the device with TEST UNIT READY, which made this a slow operation inside a
            // request whose caller has a reply DEADLINE: fs re-mounted GSFS successfully and then
            // dropped the mount 75 ms later because the capacity reply had not come back, and
            // `sectors_now` reports a missing reply as 0 - the same "no answer is not the answer
            // zero" confusion this port keeps making.
            //
            // Reaching `d` at all already means a disk is bound, and that binding IS the reconciled
            // truth now: removal is detected by the port probes and drops it (`disk = None`), which
            // is what puts "device REMOVED" on screen. With no disk bound this function returned
            // STATUS_ERR long before this match, so absence needs no probe here either.
            //
            // So the honest answer is the fast one, and the window where it can be stale is the
            // window before removal is noticed - which is bounded by the probe interval and visible.
            out[0] = STATUS_OK;
            out[1..9].copy_from_slice(&d.sectors.to_le_bytes());
            9
        }
        OP_FLUSH => {
            // The one command a stick genuinely needs: it acknowledges a WRITE(10) into its own
            // buffer, so without SYNCHRONIZE CACHE a reset loses the tail of everything just
            // written. Its outcome is reported, never assumed (§6.1, the backend-conditional
            // durability amendment).
            out[0] = if sync_cache(ctx, dma, mmio, dboff, ir0, d, ev_idx, ev_cycle, eaten) {
                STATUS_OK
            } else {
                STATUS_ERR
            };
            1
        }
        OP_READ_BLOCK => {
            if req.len() < 9 {
                return 1;
            }
            let lba = u64::from_le_bytes([
                req[1], req[2], req[3], req[4], req[5], req[6], req[7], req[8],
            ]);
            let ok = read10(ctx, dma, mmio, dboff, ir0, d, lba, 1, ev_idx, ev_cycle, eaten);
            if !ok {
                // Also once. A disk that has stopped answering fails EVERY read `fs` retries, and
                // one line per failure is the same self-inflicted load as the refusal above.
                if !READ_FAIL_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    ctx.log_fmt(format_args!(
                        "xhci: READ(10) of lba {} FAILED on a bound disk (further failures silenced)", lba));
                }
            }
            if ok {
                out[0] = STATUS_OK;
                for i in 0..SECTOR as usize {
                    out[1 + i] = data_read8(dma, i);
                }
                1 + SECTOR as usize
            } else {
                1
            }
        }
        OP_WRITE_BLOCK => {
            if req.len() < 9 + SECTOR as usize {
                return 1;
            }
            for i in 0..SECTOR as usize {
                data_write8(dma, i, req[9 + i]);
            }
            out[0] = if write10(ctx, dma, mmio, dboff, ir0, d, lba_of(req), 1, ev_idx, ev_cycle, eaten) {
                STATUS_OK
            } else {
                STATUS_ERR
            };
            1
        }
        OP_WRITE_ZEROS => {
            if req.len() < 17 {
                return 1;
            }
            let lba = lba_of(req);
            let count = u64::from_le_bytes([
                req[9], req[10], req[11], req[12], req[13], req[14], req[15], req[16],
            ]);
            for i in 0..SECTOR as usize {
                data_write8(dma, i, 0);
            }
            // The count comes off the wire, so it gets a CEILING of its own. "Bounded by the
            // request" is not a bound - it delegates the limit to the caller, and each iteration
            // here is a command that may itself wait out the full transfer timeout. A `u64::MAX`
            // would be indistinguishable from a hang. The disk's own size is the honest ceiling: no
            // legitimate zeroing extends past the last sector.
            if count > d.sectors.saturating_sub(lba) {
                return 1;
            }
            let mut ok = true;
            for i in 0..count {
                if !write10(ctx, dma, mmio, dboff, ir0, d, lba + i, 1, ev_idx, ev_cycle, eaten) {
                    ok = false;
                    break;
                }
            }
            out[0] = if ok { STATUS_OK } else { STATUS_ERR };
            1
        }
        _ => 1,
    }
}

fn lba_of(req: &[u8]) -> u64 {
    u64::from_le_bytes([
        req[1], req[2], req[3], req[4], req[5], req[6], req[7], req[8],
    ])
}
