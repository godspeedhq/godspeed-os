//! Host channels and control transfers.
//!
//! Slice 1b of the port (`docs/arm32-usb-userspace.md`): program a host channel, run a DMA transfer,
//! and compose the three of them into a USB control transfer. Everything here goes through the SDK's
//! safe `Mmio`/`Dma` wrappers, so the service still carries no `unsafe` (§18.2).
//!
//! **No cache maintenance, and that is a property of the grant rather than an omission.** The kernel
//! driver brackets every transfer with `flush_dcache` (DCCIMVAC) because it DMAs to and from CACHED
//! kernel buffers on a non-coherent Cortex-A7. This service's DMA arena is mapped **Device/uncached**
//! (`DMA_ARENA_UNCACHED = true` on arm32 sets `PCD`, which the ARM encoder maps to TEX=0b000, C=0,
//! B=1), so CPU writes reach memory directly and the device's writes are visible without an
//! invalidate. Ring-0 cache ops are not available to a service, and this is why they are not needed.

use godspeed_sdk::{Dma, Mmio, ServiceContext};

use crate::regs::*;

// --- Per-channel register bases --------------------------------------------------------------
//
// The DWC2 lays its host channels out at 0x500 + ch*0x20, and this board reports 8 of them.
#[inline] pub fn hcchar_at(ch: u32) -> usize { 0x500 + (ch as usize) * 0x20 }
#[inline] pub fn hcsplt_at(ch: u32) -> usize { 0x504 + (ch as usize) * 0x20 }
#[inline] pub fn hcint_at(ch: u32) -> usize { 0x508 + (ch as usize) * 0x20 }
#[inline] pub fn hcintmsk_at(ch: u32) -> usize { 0x50C + (ch as usize) * 0x20 }
#[inline] pub fn hctsiz_at(ch: u32) -> usize { 0x510 + (ch as usize) * 0x20 }
#[inline] pub fn hcdma_at(ch: u32) -> usize { 0x514 + (ch as usize) * 0x20 }

/// Control + enumeration + bulk. One channel PER STREAM, not a pool.
///
/// The kernel driver learned this the hard way: every transfer used to go through channel 0, and an
/// interrupt-split abandoned mid-flight left its state on the channel the next user inherited,
/// corrupting a block transfer that never ran concurrently with it. Linux keeps a free list because
/// it juggles arbitrary URBs; there are three statically-known streams here, so a pool would be
/// speculative generality (§26.2) and one channel per stream buys the isolation it exists to provide.
pub const CH_BULK: u32 = 0;

/// The keyboard's periodic poll. A SEPARATE channel from control/bulk on purpose - it is the one
/// transfer deliberately abandoned when its budget expires, so it must not share channel state with
/// anything. This is the channel-per-stream rule the kernel driver learned by corrupting block
/// transfers with an abandoned interrupt split.
pub const CH_KBD: u32 = 1;

/// Channel-enable / disable bits in HCCHAR.
const HCCHAR_CHENA: u32 = 1 << 31;
const HCCHAR_CHDIS: u32 = 1 << 30;

/// USB PIDs as HCTSIZ encodes them.
pub const PID_DATA0: u32 = 0;
pub const PID_DATA1: u32 = 2;
pub const PID_SETUP: u32 = 3;

/// Where in the DMA arena each control-transfer buffer lives.
///
/// Fixed offsets rather than an allocator: the arena is 64 KiB, this slice needs two small buffers,
/// and a bump allocator would be machinery for a problem that does not exist yet (§26.2). Slice 3
/// will need real sectors and can carve the rest then.
pub const SETUP_OFF: usize = 0x0000; // 8 bytes
pub const DATA_OFF: usize = 0x0040;  // scratch for descriptors
pub const DATA_LEN: usize = 256;

/// The device this driver is currently talking to. Control transfers address whoever is selected.
#[derive(Clone, Copy)]
pub struct Target {
    pub addr: u8,
    pub mps: u16,
    pub low_speed: bool,
}

/// Bring a channel back to a clean state: mask its interrupts and clear every latched status bit.
///
/// An abandoned transfer leaves both set, and the next user of that channel would otherwise read a
/// previous transfer's completion as its own.
pub fn release(mmio: &Mmio, ch: u32) {
    mmio.write32(hcintmsk_at(ch), 0);
    mmio.write32(hcint_at(ch), 0xFFFF_FFFF);
}

/// Program a host channel and enable it.
#[allow(clippy::too_many_arguments)]
pub fn program(
    mmio: &Mmio, t: &Target, ch: u32, dir_in: bool, pid: u32,
    len: u32, buf_phys: u32, ep: u32, ep_type: u32, hcsplt: u32,
) {
    let mps = t.mps as u32;
    let pkts = if len == 0 { 1 } else { (len + mps - 1) / mps };

    // Channel-reuse hygiene: if a prior transaction left the channel ENABLED - a timeout that never
    // truly halted, or a split phase re-arm - disable it cleanly before reprogramming. Never reuse a
    // half-live channel.
    halt(mmio, ch);

    mmio.write32(hcint_at(ch), 0xFFFF_FFFF);
    mmio.write32(hctsiz_at(ch), (len & 0x7_FFFF) | ((pkts & 0x3ff) << 19) | (pid << 29));
    // The HCDMA address is a BUS address as the DWC2 master sees memory, not a CPU physical address.
    // On real BCM2836 silicon that means the VideoCore alias; in QEMU it is identity. Getting this
    // wrong does not fault - it transfers nothing, or STALLs the DATA stage.
    mmio.write32(hcdma_at(ch), buf_phys | DMA_BUS_ALIAS);
    // HCSPLT LAST before HCCHAR - the Linux order is HCTSIZ -> HCSPLT -> HCCHAR.
    mmio.write32(hcsplt_at(ch), hcsplt);

    // Odd-frame scheduling applies to PERIODIC transfers AND split transactions (a split's
    // SSPLIT/CSPLIT are microframe-scheduled by the hub's TT). Target the NEXT microframe: OddFrm set
    // when the current one is even. A direct non-periodic transfer keeps OddFrm = 0 - setting it
    // there makes the v2.80a core defer the token and strand the bytes, diagnosed on hardware.
    let oddfrm = if (ep_type == 3 || hcsplt != 0) && (mmio.read32(HFNUM) & 1) == 0 {
        HCCHAR_ODDFRM
    } else {
        0
    };
    let chan = (mps & 0x7FF)
        | ((ep & 0xF) << 11)
        | ((dir_in as u32) << 15)
        | ((t.low_speed as u32) << 17)
        | ((ep_type & 0x3) << 18)
        | (1 << 20)                       // multi-count = 1 (Linux ec_mc for control/bulk)
        | ((t.addr as u32 & 0x7F) << 22)
        | oddfrm
        | HCCHAR_CHENA;
    mmio.write32(hcchar_at(ch), chan);
}

/// The data PID the controller has advanced to, read back from HCTSIZ [30:29].
///
/// The hardware owns this toggle. Tracking it in software works only until the two disagree, and
/// when they do the failure is silent: the device retransmits its previous packet rather than
/// erroring, so a keystroke is delivered twice.
pub fn pid_from_hctsiz(mmio: &Mmio, ch: u32) -> u32 {
    (mmio.read32(hctsiz_at(ch)) >> 29) & 0x3
}

/// Wait for a channel to halt, bounded by the CLOCK. Returns the latched HCINT, or `None` on timeout.
/// Abort a running channel, the way the core requires.
///
/// **Both ChEna AND ChDis must be set.** This read as "clear ChEna, set ChDis" in two places, which
/// looks like the obvious way to stop a channel and is the documented-wrong one: the DWC2 programming
/// guide, and Linux's `dwc2_hc_halt`, both write ChEna|ChDis together, because the core needs ChEna
/// set to PROCESS the halt - that is how it dequeues the channel's outstanding request from the
/// non-periodic request queue.
///
/// Clearing ChEna instead stops the software's view and leaves the core's. The request is never
/// retired, and that queue is only a handful of entries deep, so every abandoned transfer
/// permanently consumes one. Hardware showed the end state exactly: transmit worked 584 times, then
/// degraded, then stopped forever with HCINT reading 0x00000000 and the channel never halting - not
/// a device refusing frames (that is a NAK, and a NAK sets a bit) but transactions that were never
/// scheduled at all, because there was no room left to schedule them. Flushing the FIFO did not
/// touch it, which is what ruled the FIFO out.
///
/// Bounded, and it waits for the halt to actually land: an abort that returns before the core has
/// finished is the same abandoned-channel bug in a smaller window.
pub fn halt(mmio: &Mmio, ch: u32) {
    let hcchar = mmio.read32(hcchar_at(ch));
    if hcchar & HCCHAR_CHENA == 0 {
        return;                       // already idle - nothing queued to retire
    }
    mmio.write32(hcchar_at(ch), hcchar | HCCHAR_CHENA | HCCHAR_CHDIS);
    // Spin for the core to retire it. This is a register handshake with the controller, not a wait on
    // a device, so a bounded spin is the right shape - and if it ever expires the channel is left
    // exactly as an unbounded wait would leave it, minus the hang.
    let mut t = 0u32;
    while mmio.read32(hcchar_at(ch)) & HCCHAR_CHENA != 0 {
        t += 1;
        if t > 100_000 {
            break;
        }
    }
}

pub fn wait_halt(ctx: &ServiceContext, mmio: &Mmio, ch: u32, ms: u64) -> Option<u32> {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(ms));
    loop {
        let hcint = mmio.read32(hcint_at(ch));
        if hcint & HCINT_CHHLTD != 0 {
            return Some(hcint);
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            // Leave the channel clean for the next user rather than abandoning it enabled - the
            // failure this driver's channel-per-stream split exists to prevent. `halt` is what makes
            // that true: it retires the core's outstanding request, which the old open-coded disable
            // did not.
            halt(mmio, ch);
            return None;
        }
    }
}

/// One stage of a control transfer: program, wait for the halt, decide success from HCINT.
#[allow(clippy::too_many_arguments)]
fn stage(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target,
    ch: u32, dir_in: bool, pid: u32, buf_phys: u32, len: u32, what: &str,
) -> bool {
    program(mmio, t, ch, dir_in, pid, len, buf_phys, 0, 0, 0);
    match wait_halt(ctx, mmio, ch, 100) {
        None => {
            ctx.log_fmt(format_args!("dwc2-svc: {} stage timed out (channel never halted)", what));
            false
        }
        Some(hcint) => {
            // XFERCOMPL is the only success. A halt with anything else latched is a real failure and
            // is named, because "the transfer did not work" and "the device STALLed" want different
            // responses and a single false would merge them.
            if hcint & HCINT_XFERCOMPL != 0 {
                true
            } else {
                ctx.log_fmt(format_args!(
                    "dwc2-svc: {} stage FAILED (HCINT={:#010x}{}{}{})", what, hcint,
                    if hcint & HCINT_STALL != 0 { " STALL" } else { "" },
                    if hcint & HCINT_XACTERR != 0 { " XACTERR" } else { "" },
                    if hcint & HCINT_NAK != 0 { " NAK" } else { "" }));
                false
            }
        }
    }
}

/// Build the HCSPLT descriptor for a device behind a hub.
///
/// `PrtAddr` = the hub's downstream port, `HubAddr` = the hub's device address, `XactPos = ALL`
/// (0b11) because a control/bulk split moves a whole packet rather than a begin/mid/end chunk, and
/// `SplEna` to arm it. Zero means "direct device" and takes the non-split path.
pub fn hcsplt(hub_addr: u8, hub_port: u8) -> u32 {
    if hub_port == 0 {
        return 0;
    }
    (hub_port as u32 & 0x7F) | ((hub_addr as u32 & 0x7F) << 7) | (0b11 << 14) | (1 << 31)
}

/// Wait until the controller reports the given microframe. Bounded: one full frame is 8 microframes
/// at 125 us, so a whole sweep cannot take more than a millisecond even if the target never appears.
// The microframe/halt cycle counters that lived here are REMOVED, along with the three
// `pub static ...: Atomic*` they used. They answered the question they were added for - the
// channel-halt wait is ~97% of the keyboard's CPU and the microframe wait ~3%, which is why
// optimising the microframe wait moved nothing - and the answer is in the commit history where it
// cannot drift.
//
// They also should never have been statics. Nothing here is concurrent (a service is one task on one
// core), so the atomic was an `unsafe`-free spelling of `static mut`: it passes `unsafe_check.py`
// while leaving invariant 9 exactly as violated. The owner was visible at the read site - `main.rs`
// resets the sibling counters on the same log flush and those live on `KeyState`. Threading a
// keyboard-owned struct through `wait_halt` is wrong too, because every transfer type shares it, so
// a measurement worth keeping would need its own home rather than a parameter on a shared path.

fn wait_for_uframe(ctx: &ServiceContext, mmio: &Mmio, target: u32) {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(2));
    loop {
        let cur = mmio.read32(HFNUM) & 7;
        if cur == target {
            return;
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            return;
        }
        // SLEEP THE BULK OF THE WAIT, SPIN ONLY THE LAST MICROFRAME.
        //
        // This loop was a pure spin, and it is where the keyboard's CPU went: roughly 375 us of every
        // 450 us poll, about 4.6% of a core, burned waiting for a microframe boundary. It spun because
        // the kernel had no clock that could name 125 us - the finest thing available was the 10 ms
        // scheduler tick.
        //
        // It has one now, and it is exact: a 125 us sleep returns in 146 us on average and 155 us at
        // worst, measured on this board with the console TX ring in place. About 30 us of jitter,
        // against a 125 us microframe.
        //
        // 30 us of jitter is small but it is not nothing, and landing in the WRONG microframe is the
        // failure this whole path exists to avoid - a missed complete-split strands a transaction and
        // wedges the transaction translator, which is exactly how the keyboard dies. So sleep only
        // while there is more than one microframe to go, and spin the last one, where precision
        // matters and the cost is bounded to a single 125 us window.
        let togo = (target.wrapping_sub(cur)) & 7;
        if togo >= 2 {
            // Wake one microframe SHORT of the target and spin in from there. `duration_cycles` takes
            // whole milliseconds, so the sub-millisecond figure is derived from the board's own
            // calibration instead - a cycle count is not a portable duration.
            let per_us = (ctx.tsc_ticks_per_10ms() / 10_000).max(1);
            let sleep_us = (togo as u64 - 1) * 125;
            ctx.sleep(per_us.saturating_mul(sleep_us).max(1));
        }
    }
}

/// One SPLIT transaction stage: start-split, then poll the complete-split.
///
/// **This is the non-periodic (control/bulk) path, and it is deliberately the one this slice uses.**
/// It brute-forces a landing microframe by SWEEPING 0..7 across retries rather than scheduling one
/// precisely, because a non-periodic split has no fixed schedule. That makes it tolerant of being
/// preempted: a badly-timed attempt simply retries at a different microframe. The PERIODIC path
/// (`split_txn_periodic` in the kernel driver) does need microframe-accurate scheduling and is the
/// genuinely risky part of moving this driver out of ring 0 - it is needed only for the keyboard's
/// interrupt endpoint, so it belongs to Slice 2, faced on its own.
#[allow(clippy::too_many_arguments)]
fn stage_split_one(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target,
    ch: u32, dir_in: bool, pid: u32, buf_phys: u32, len: u32, splt: u32, what: &str,
) -> bool {
    const SS_TRIES: u32 = 24;
    for attempt in 0..SS_TRIES {
        wait_for_uframe(ctx, mmio, attempt % 8);

        // STATE 1 - the Start-Split (CompleteSplit = 0). The hub's transaction translator legitimately
        // NAKs or transaction-errors while busy, and USB 2.0 11.17.5 says the host re-issues the whole
        // start-split rather than treating it as a failure.
        program(mmio, t, ch, dir_in, pid, len, buf_phys, 0, 0, splt);
        let ss = match wait_halt(ctx, mmio, ch, 50) {
            Some(v) => v,
            None => continue,
        };
        if ss & HCINT_STALL != 0 {
            ctx.log_fmt(format_args!(
                "dwc2-svc: {} start-split STALLed - HCINT={:#010x} HCSPLT={:#010x} HCCHAR={:#010x}",
                what, ss, mmio.read32(hcsplt_at(ch)), mmio.read32(hcchar_at(ch))));
            return false;
        }
        if ss & HCINT_XFERCOMPL != 0 {
            return true; // rare, but a split can complete outright
        }
        if ss & HCINT_ACK == 0 {
            continue; // no ACK: the TT did not take it - retry at the next microframe
        }

        // STATE 2 - poll the Complete-Split for the low/full-speed device's answer.
        let mut nyet = 0u32;
        loop {
            program(mmio, t, ch, dir_in, pid, len, buf_phys, 0, 0, splt | (1 << 16));
            let cs = match wait_halt(ctx, mmio, ch, 50) {
                Some(v) => v,
                None => break,
            };
            if cs & HCINT_XFERCOMPL != 0 {
                return true;
            }
            if cs & HCINT_STALL != 0 {
                // DUMP THE REGISTERS, do not guess again.
                //
                // A compliant device may not STALL a SETUP (USB 2.0 8.5.3), so this is a
                // contradiction and one hypothesis for it - a missing TRSTRCY recovery delay - has
                // already been tried and refuted on hardware. The rule this port has followed
                // throughout applies: when a hypothesis about hardware state is wrong, do not form a
                // second one, find where the hardware RECORDS the answer and read it.
                //
                // HCSPLT is the field most likely to be wrong (hub address placement, XactPos) and it
                // is right here in a register. HCCHAR carries the device address, endpoint and speed
                // the controller actually used, which is the other half of "who did we just talk to".
                ctx.log_fmt(format_args!(
                    "dwc2-svc: {} complete-split STALLed - HCINT={:#010x} HCSPLT={:#010x} HCCHAR={:#010x} HFNUM={:#06x}",
                    what, cs, mmio.read32(hcsplt_at(ch)), mmio.read32(hcchar_at(ch)),
                    mmio.read32(HFNUM) & 0xFFFF));
                return false;
            }
            if cs & HCINT_NYET != 0 {
                // The TT has not finished with the device yet. Keep polling, BOUNDED - then fall out
                // to a fresh start-split rather than spinning here forever.
                nyet += 1;
                if nyet > 500 {
                    break;
                }
                ctx.sleep(ctx.duration_cycles(1));
                continue;
            }
            break; // NAK or XactErr: re-issue the start-split
        }
    }
    ctx.log_fmt(format_args!("dwc2-svc: {} split gave up after {} start-splits", what, SS_TRIES));
    false
}

/// Wait until the controller reaches a given microframe, bounded by REAL TIME.
///
/// 1.5 ms, because reaching any target microframe takes at most one ~1 ms frame. Time and not a spin
/// count: the kernel driver's comment records that a spin-count bound here "was the cause of the
/// scheduler-starving hang", since spin latency depends on MMIO speed rather than on the clock the
/// microframes actually advance on.
fn wait_uframe(ctx: &ServiceContext, mmio: &Mmio, target: u32) {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(2));
    while (mmio.read32(HFNUM) & 0x7) != (target & 0x7) {
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            return;
        }
    }
}

/// A PERIODIC (interrupt) IN split, frame-scheduled. Returns the latched HCINT.
///
/// Unlike a non-periodic split - which sweeps microframes across retries and tolerates bad timing -
/// a periodic split must be SCHEDULED, and the schedule is the thing that makes it work:
///
///   1. START-SPLIT in microframe (current+1)&7, SKIPPING microframe 6: too little of the frame is
///      left after it for the complete-split at +2. ODDFRM must match that microframe's parity, which
///      `program` derives from HFNUM - correct only because the channel is enabled AFTER `wait_uframe`
///      has reached the scheduled microframe.
///   2. COMPLETE-SPLIT at +2, retrying NYET in the following microframes (3 tries).
///
/// **ONE attempt per call, and that is what makes this safe in a preemptible task.** Any failure -
/// including being descheduled at the wrong moment - simply reschedules a fresh start-split on the
/// next poll. Combined with every wait being time-bounded, the whole poll is a few milliseconds and
/// cannot wedge the service. The risk this port carried from the start turns out to be answered by
/// the algorithm's own structure rather than by holding the CPU.
#[allow(clippy::too_many_arguments)]
pub fn periodic_split_in(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target,
    ch: u32, pid: u32, buf_phys: u32, len: u32, ep: u32, splt: u32,
) -> u32 {
    let mut ssf = (mmio.read32(HFNUM).wrapping_add(1)) & 0x7;
    if ssf == 6 {
        ssf = 7; // skip microframe 6
    }
    wait_uframe(ctx, mmio, ssf);
    program(mmio, t, ch, true, pid, len, buf_phys, ep, 3, splt); // ep_type 3 = interrupt
    let ss = match wait_halt(ctx, mmio, ch, 5) {
        Some(v) => v,
        None => return 0,
    };
    if ss & (HCINT_STALL | HCINT_XFERCOMPL) != 0 {
        return ss;
    }
    if ss & HCINT_ACK == 0 {
        return ss; // the TT refused the start-split - try again next poll
    }

    let mut csf = (ssf + 2) & 0x7;
    let mut last = ss;
    // SIX complete-split attempts, not three.
    //
    // Giving up here does not merely lose one keystroke: the transaction translator is left holding
    // a transaction nobody collected, and that is what wedges it - after which the keyboard needs a
    // TT clear and, in practice on this board, a port re-enumeration, which the operator experiences
    // as a a stutter of over a second. Each extra attempt is one microframe of waiting, so the whole
    // retry budget costs well under a millisecond and buys a large reduction in stranding.
    //
    // Not unbounded: a translator that never finishes must still be given up on, or this poll would
    // hold the core. Six is chosen to cover the NYET runs actually seen in the logs while staying far
    // inside the ~1 ms the whole poll is allowed.
    for _ in 0..6 {
        wait_uframe(ctx, mmio, csf);
        program(mmio, t, ch, true, pid, len, buf_phys, ep, 3, splt | (1 << 16));
        let cs = match wait_halt(ctx, mmio, ch, 5) {
            Some(v) => v,
            None => return last,
        };
        last = cs;
        if cs & (HCINT_XFERCOMPL | HCINT_STALL | HCINT_NAK) != 0 {
            // NAK is NOT an error here: it is the keyboard positively answering "no key activity this
            // period", which is what an idle keyboard says most of the time.
            return cs;
        }
        if cs & HCINT_NYET != 0 {
            csf = (csf + 1) & 0x7; // the TT is not done - retry the complete-split a microframe later
            continue;
        }
        return cs;
    }
    last
}

/// A split transfer of any length: ONE low/full-speed packet per split transaction.
///
/// **The DWC2 does not auto-continue a multi-packet split in buffer-DMA mode.** It halts with
/// XferCompl after the FIRST packet, so software must sequence each mps-sized packet itself,
/// advancing the buffer and toggling the data PID. The kernel driver says so in a comment that
/// describes this exact failure, already diagnosed on this board:
///
///   "HW-proven (Pi 2 / LAN9514): an 18-byte device descriptor read whole came back as 8 correct
///    bytes + 10 stale, because only packet 1 was ever retrieved."
///
/// Which is precisely what happened here - `VID:PID=0000:0000` from an 18-byte read at MPS 8, with
/// the first packet correct. It read as ZEROS rather than stale bytes only because the IN scratch is
/// zeroed before every transfer, and VID lives at bytes 8..11, in the part that was never fetched.
///
/// A single-packet transfer is one iteration; a zero-length STATUS stage is one iteration with a
/// chunk of 0, which is why the loop tests `off >= len` AFTER advancing rather than before.
#[allow(clippy::too_many_arguments)]
fn stage_split(
    ctx: &ServiceContext, mmio: &Mmio, t: &Target,
    ch: u32, dir_in: bool, pid: u32, buf_phys: u32, len: u32, splt: u32, what: &str,
) -> bool {
    let mps = (t.mps as u32).max(1);
    let mut off = 0u32;
    let mut cur_pid = pid;
    loop {
        let chunk = (len - off).min(mps);
        if !stage_split_one(ctx, mmio, t, ch, dir_in, cur_pid, buf_phys + off, chunk, splt, what) {
            return false;
        }
        off += chunk;
        // Done when the whole length is moved, or when a SHORT packet ended the transfer early - a
        // device answering with less than it was asked for is a complete answer, not a truncated one.
        if off >= len || chunk < mps {
            return true;
        }
        // Control/bulk data toggle: every packet after the first flips DATA1 <-> DATA0.
        cur_pid = if cur_pid == PID_DATA1 { PID_DATA0 } else { PID_DATA1 };
    }
}

/// A full USB control transfer: SETUP, optional DATA, STATUS.
///
/// `data` is filled on an IN transfer and sent on an OUT one. Returns false if any stage failed, with
/// the failing stage already reported.
pub fn control(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target,
    setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize,
) -> bool {
    control_split(ctx, mmio, dma, t, setup, data, data_in, dlen, 0)
}

/// A control transfer, optionally through a hub's transaction translator.
///
/// `splt` of 0 is a direct device and behaves exactly as before - the direct path is untouched, so a
/// regression here cannot break what slices 1a-1c-ii proved.
#[allow(clippy::too_many_arguments)]
pub fn control_split(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target,
    setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize, splt: u32,
) -> bool {
    let ch = CH_BULK;
    let setup_phys = dma.phys_at(SETUP_OFF) as u32;
    let data_phys = dma.phys_at(DATA_OFF) as u32;

    for (i, b) in setup.iter().enumerate() {
        dma.write8(SETUP_OFF + i, *b);
    }
    let run = |ctx: &ServiceContext, dir_in, pid, phys, len, what| {
        if splt == 0 {
            stage(ctx, mmio, t, ch, dir_in, pid, phys, len, what)
        } else {
            stage_split(ctx, mmio, t, ch, dir_in, pid, phys, len, splt, what)
        }
    };
    if !run(ctx, false, PID_SETUP, setup_phys, 8, "SETUP") {
        return false;
    }

    if dlen > 0 {
        if data_in {
            // Never let the device DMA past the scratch buffer - clamp the PROGRAMMED length, not
            // just the copy-out.
            let want = dlen.min(DATA_LEN);
            // ZERO the scratch first. A control IN completes successfully on a SHORT packet, and this
            // buffer is shared by every control transfer - so a device returning fewer bytes than
            // asked would leave the PREVIOUS transfer's tail in place and the caller would read it as
            // this reply's data. Harmless while a reply is only logged; not harmless once one DECIDES
            // something, and a hub port-status short read decides connect or disconnect.
            for i in 0..want {
                dma.write8(DATA_OFF + i, 0);
            }
            if !run(ctx, true, PID_DATA1, data_phys, want as u32, "DATA-IN") {
                return false;
            }
            let n = want.min(data.len());
            for i in 0..n {
                data[i] = dma.read8(DATA_OFF + i);
            }
        } else {
            // Send only what fits in BOTH the scratch buffer and the source slice.
            let n = dlen.min(DATA_LEN).min(data.len());
            for i in 0..n {
                dma.write8(DATA_OFF + i, data[i]);
            }
            if !run(ctx, false, PID_DATA1, data_phys, n as u32, "DATA-OUT") {
                return false;
            }
        }
    }

    // STATUS: opposite direction, zero length, DATA1. The setup buffer doubles as a dummy DMA target.
    let ok = if data_in {
        run(ctx, false, PID_DATA1, setup_phys, 0, "STATUS")
    } else {
        run(ctx, true, PID_DATA1, data_phys, 0, "STATUS")
    };
    if ok {
        release(mmio, ch);
    }
    ok
}

/// GET_DESCRIPTOR, the first thing worth asking a device.
pub fn get_descriptor(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target,
    dtype: u8, dindex: u8, buf: &mut [u8], len: usize,
) -> bool {
    let setup = [
        0x80,            // bmRequestType: device-to-host, standard, device
        0x06,            // bRequest: GET_DESCRIPTOR
        dindex,
        dtype,
        0, 0,            // wIndex
        (len & 0xFF) as u8,
        ((len >> 8) & 0xFF) as u8,
    ];
    control(ctx, mmio, dma, t, &setup, buf, true, len)
}
