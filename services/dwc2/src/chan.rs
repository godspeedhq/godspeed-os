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
    if mmio.read32(hcchar_at(ch)) & HCCHAR_CHENA != 0 {
        mmio.write32(hcchar_at(ch), (mmio.read32(hcchar_at(ch)) & !HCCHAR_CHENA) | HCCHAR_CHDIS);
        // Bounded: this is a register handshake, not a device transaction, so a small spin is right.
        let mut t = 0u32;
        while mmio.read32(hcchar_at(ch)) & HCCHAR_CHENA != 0 {
            t += 1;
            if t > 100_000 { break; }
        }
    }

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

/// Wait for a channel to halt, bounded by the CLOCK. Returns the latched HCINT, or `None` on timeout.
pub fn wait_halt(ctx: &ServiceContext, mmio: &Mmio, ch: u32, ms: u64) -> Option<u32> {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(ms));
    loop {
        let hcint = mmio.read32(hcint_at(ch));
        if hcint & HCINT_CHHLTD != 0 {
            return Some(hcint);
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            // Leave the channel clean for the next user rather than abandoning it enabled - the
            // failure this driver's channel-per-stream split exists to prevent.
            mmio.write32(hcchar_at(ch), (mmio.read32(hcchar_at(ch)) & !HCCHAR_CHENA) | HCCHAR_CHDIS);
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

/// A full USB control transfer: SETUP, optional DATA, STATUS.
///
/// `data` is filled on an IN transfer and sent on an OUT one. Returns false if any stage failed, with
/// the failing stage already reported.
pub fn control(
    ctx: &ServiceContext, mmio: &Mmio, dma: &Dma, t: &Target,
    setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize,
) -> bool {
    let ch = CH_BULK;
    let setup_phys = dma.phys_at(SETUP_OFF) as u32;
    let data_phys = dma.phys_at(DATA_OFF) as u32;

    for (i, b) in setup.iter().enumerate() {
        dma.write8(SETUP_OFF + i, *b);
    }
    if !stage(ctx, mmio, t, ch, false, PID_SETUP, setup_phys, 8, "SETUP") {
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
            if !stage(ctx, mmio, t, ch, true, PID_DATA1, data_phys, want as u32, "DATA-IN") {
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
            if !stage(ctx, mmio, t, ch, false, PID_DATA1, data_phys, n as u32, "DATA-OUT") {
                return false;
            }
        }
    }

    // STATUS: opposite direction, zero length, DATA1. The setup buffer doubles as a dummy DMA target.
    let ok = if data_in {
        stage(ctx, mmio, t, ch, false, PID_DATA1, setup_phys, 0, "STATUS")
    } else {
        stage(ctx, mmio, t, ch, true, PID_DATA1, data_phys, 0, "STATUS")
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
