// SPDX-License-Identifier: GPL-2.0-only
//! VideoCore framebuffer for the Pi 2 (BCM2836) - the ARM has no Limine to hand it a framebuffer the
//! way x86 does, so it asks the GPU for one through the **mailbox property interface** and renders the
//! console into it. This module owns only the acquisition (the arch-specific half); the glyph renderer
//! is shared with x86 (the arch-neutral half).
//!
//! Bring-up order (like the rest of the port): prove the pipeline with a solid-colour fill first, then
//! layer text on top. A colour on the TV means the mailbox, the returned base/pitch, the device mapping,
//! and the display path are all correct end to end.

use super::pl011_write;
use super::exceptions::write_hex32;

/// VideoCore mailbox, at peripheral base + 0xB880. The property channel (8) is how the ARM asks the GPU
/// to allocate a framebuffer and reports back the base, pitch, and dimensions.
const MBOX_BASE: usize = super::PERIPHERAL_BASE + 0xB880;
const MBOX_READ:   *const u32 = MBOX_BASE as *const u32;
const MBOX_STATUS: *const u32 = (MBOX_BASE + 0x18) as *const u32;
const MBOX_WRITE:  *mut u32   = (MBOX_BASE + 0x20) as *mut u32;
const MBOX_FULL:  u32 = 0x8000_0000; // status: outgoing mailbox full
const MBOX_EMPTY: u32 = 0x4000_0000; // status: incoming mailbox empty
const CHANNEL_PROP: u32 = 8;         // ARM->VC property tags
const RESP_SUCCESS: u32 = 0x8000_0000;
// Bounds for the mailbox handshake so an absent/wedged GPU degrades to "no framebuffer" instead of
// hanging the boot. Generous - a live GPU drains/responds in microseconds; these are only ever reached
// when the peripheral is dead.
const MBOX_SPIN_CAP:  u32 = 20_000_000; // per FULL/EMPTY status wait
const MBOX_MATCH_CAP: u32 = 64;          // responses read before giving up on a channel match

/// The property buffer: 16-byte aligned (the low 4 bits of its address carry the channel number).
#[repr(C, align(16))]
struct MboxBuf { data: [u32; 36] }
static mut MBOX: MboxBuf = MboxBuf { data: [0; 36] };

#[derive(Clone, Copy)]
pub struct FbInfo {
    pub base:   u32, // ARM physical base of the framebuffer (device-mapped after `init`)
    pub pitch:  u32, // bytes per scanline
    pub width:  u32,
    pub height: u32,
}

/// Send the property buffer to the GPU and wait for its reply. Returns whether the GPU reported success.
///
/// **Called with the MMU and caches OFF** (from the boot path before `mmu::enable`), which is what makes
/// it coherent with the GPU on real silicon: the GPU reaches RAM through its own bus, and with the ARM
/// caches off there is no L1/L2 copy to go stale - our request and the GPU's reply both live in RAM. (An
/// earlier caches-on version FAILED on hardware because the reply came back through the A7's L2, which a
/// set/way L1 clean does not reach, while QEMU's cacheless model hid it.) The address handed to the
/// mailbox is still the uncached VideoCore bus alias (`| 0xC0000000`).
fn mbox_call(channel: u32) -> bool {
    // SAFETY: MBOX is a 16-byte-aligned static touched only on this single-threaded, caches-off boot
    // path; the mailbox registers are physical MMIO (Strongly-Ordered with the MMU off).
    unsafe {
        let phys = core::ptr::addr_of!(MBOX) as u32;
        let bus  = (phys | 0xC000_0000) & !0xF; // uncached GPU alias, 16-byte aligned
        core::arch::asm!("dsb", options(nostack)); // request is in RAM (caches off) before we signal
        // Every mailbox wait is bounded: an absent/wedged VideoCore that never drains (FULL stuck),
        // never fills (EMPTY stuck), or never posts our matching response must NOT hang the boot before
        // the scheduler (invariant 12 / 26.6 - the same discipline dwc2.rs applies). On timeout report
        // loudly and return false; every caller treats false as "no framebuffer" and falls back to serial.
        let mut spins = 0u32;
        while MBOX_STATUS.read_volatile() & MBOX_FULL != 0 {
            spins += 1;
            if spins > MBOX_SPIN_CAP { pl011_write(b"video: WARN mailbox FULL stuck - no GPU\r\n"); return false; }
        }
        MBOX_WRITE.write_volatile(bus | (channel & 0xF));
        let mut waits = 0u32;
        loop {
            let mut spins = 0u32;
            while MBOX_STATUS.read_volatile() & MBOX_EMPTY != 0 {
                spins += 1;
                if spins > MBOX_SPIN_CAP { pl011_write(b"video: WARN mailbox EMPTY stuck - no GPU\r\n"); return false; }
            }
            let r = MBOX_READ.read_volatile();
            if (r & 0xF) == channel && (r & !0xF) == bus {
                break;
            }
            waits += 1;
            if waits > MBOX_MATCH_CAP { pl011_write(b"video: WARN mailbox response never matched - no GPU\r\n"); return false; }
        }
        core::arch::asm!("dsb", options(nostack));
        MBOX.data[1] == RESP_SUCCESS
    }
}

/// Ask the VideoCore to power ON the USB HCD (`SET_POWER_STATE`, device 3, `ON | WAIT`) - Circle does this
/// before DWC2 init. Register access to the DWC2 is on the always-on APB peripheral bus, but its AXI DMA
/// **master** lives in a separate power/clock domain the firmware may leave off - which is exactly the
/// symptom on the Pi 2 (GSNPSID reads, PHY detects a connect, SOFs run, yet the DMA master never
/// dispatches, AHBIdle stuck 1). Runs with the MMU + caches OFF like the rest of this file, so it must be
/// called from the early boot path before `mmu::enable`. Returns the GPU's success flag.
pub fn set_usb_power_on() -> bool {
    // SAFETY: single-threaded, caches-off boot; MBOX is filled then read here only.
    unsafe {
        let m = &mut *core::ptr::addr_of_mut!(MBOX);
        m.data[0] = 8 * 4;       // total buffer size in bytes
        m.data[1] = 0;           // request code
        m.data[2] = 0x0002_8001; // tag: SET_POWER_STATE
        m.data[3] = 8;           // value buffer size (2 u32s: device id + state)
        m.data[4] = 0;           // tag request code
        m.data[5] = 3;           // device id: USB HCD
        m.data[6] = 0b11;        // state: bit0 ON | bit1 WAIT (block until powered + stable)
        m.data[7] = 0;           // end tag
    }
    mbox_call(8)
}

/// Ask the GPU for the display's native (physical) resolution, so the framebuffer can be requested at
/// exactly that size and fill the screen - no pillarbox bars. `None` (fall back to a default) if the
/// query fails or returns nothing. Runs with the MMU + caches OFF, like `request`.
pub fn query_display_size() -> Option<(u32, u32)> {
    // SAFETY: single-threaded, caches-off boot; MBOX filled then read here only.
    unsafe {
        let b = &mut (*core::ptr::addr_of_mut!(MBOX)).data;
        *b = [0; 36];
        b[0] = 8 * 4; b[1] = 0;
        b[2] = 0x0004_0003; b[3] = 8; b[4] = 0; b[5] = 0; b[6] = 0; // get physical (display) W/H
        b[7] = 0; // end tag
    }
    if !mbox_call(CHANNEL_PROP) {
        return None;
    }
    let (w, h) = unsafe { let b = &(*core::ptr::addr_of!(MBOX)).data; (b[5], b[6]) };
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }
    Some((w, h))
}

/// Ask the GPU for a 32-bpp framebuffer at `width` x `height` and return its descriptor. `None`
/// (logged) if the mailbox call fails or returns nothing. **Must run with the MMU + caches OFF** (before
/// `mmu::enable`) so the mailbox exchange is coherent with the GPU; the framebuffer is mapped and drawn
/// later via `map_and_fill` once translation is on.
pub fn request(width: u32, height: u32) -> Option<FbInfo> {
    // SAFETY: single-threaded boot; MBOX is filled then read here only.
    unsafe {
        let b = &mut (*core::ptr::addr_of_mut!(MBOX)).data;
        *b = [0; 36];
        b[0] = 35 * 4;  b[1] = 0;                                   // total size, request code
        b[2]=0x0004_8003; b[3]=8; b[4]=8; b[5]=width; b[6]=height;  // set physical W/H
        b[7]=0x0004_8004; b[8]=8; b[9]=8; b[10]=width; b[11]=height;// set virtual W/H
        b[12]=0x0004_8009; b[13]=8; b[14]=8; b[15]=0; b[16]=0;      // set virtual offset 0,0
        b[17]=0x0004_8005; b[18]=4; b[19]=4; b[20]=32;              // set depth 32 bpp
        b[21]=0x0004_8006; b[22]=4; b[23]=4; b[24]=1;              // set pixel order RGB
        b[25]=0x0004_0001; b[26]=8; b[27]=8; b[28]=4096; b[29]=0;   // allocate FB (align) -> base,size
        b[30]=0x0004_0008; b[31]=4; b[32]=4; b[33]=0;              // get pitch -> pitch
        b[34]=0;                                                    // end tag
    }
    if !mbox_call(CHANNEL_PROP) {
        pl011_write(b"arm32: framebuffer mailbox request FAILED\r\n");
        return None;
    }
    // SAFETY: response fields written by the GPU, now coherent after the invalidate in mbox_call.
    let (bus_base, pitch, w, h) = unsafe {
        let b = &(*core::ptr::addr_of!(MBOX)).data;
        (b[28], b[33], b[5], b[6])
    };
    let base = bus_base & 0x3FFF_FFFF; // GPU bus address -> ARM physical
    if base == 0 || pitch == 0 {
        pl011_write(b"arm32: framebuffer allocation returned null (base/pitch 0)\r\n");
        return None;
    }
    // Range-check the GPU-returned geometry before it drives the fill loop (`(pitch/4)*height` stores)
    // and the mapping length. The GPU is trusted, but an absurd pitch/width/height must not be accepted
    // blindly (defence in depth; matches the <=4096 cap query_display_size already applies). 8 KiB per
    // dimension and a 64 KiB pitch bound any real Pi display with headroom.
    if w == 0 || h == 0 || w > 8192 || h > 8192 || pitch > 0x1_0000 {
        pl011_write(b"arm32: framebuffer geometry out of range - falling back to serial\r\n");
        return None;
    }
    pl011_write(b"arm32: framebuffer at ");
    write_hex32(base);
    pl011_write(b" pitch ");
    write_hex32(pitch);
    pl011_write(b" (");
    write_hex32(w);
    pl011_write(b"x");
    write_hex32(h);
    pl011_write(b")\r\n");
    Some(FbInfo { base, pitch, width: w, height: h })
}

/// Pack an (r, g, b) triple into a framebuffer pixel. We request `pixel_order = RGB`, which lands the
/// RED channel in the LOW byte (verified via a QEMU screendump), so a pixel is `r | g<<8 | b<<16` -
/// NOT the x86 `0x00RRGGBB` order. Use this helper everywhere so colours read correctly on the display.
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Map the framebuffer cacheable (so console rendering is fast; `fbcon` cleans each write to the GPU).
/// Called AFTER `mmu::enable`, once translation is on - the counterpart to the MMU-off `request`. The
/// text console (`fbcon`) draws into it after this.
pub fn map(fb: &FbInfo) {
    super::mmu::map_framebuffer(fb.base, fb.pitch.saturating_mul(fb.height));
}

/// Map + fill with a solid colour. Kept for the Phase-1 pipeline proof; the console path uses `map`.
pub fn map_and_fill(fb: &FbInfo, color: u32) {
    map(fb);
    fill(fb, color);
}

/// Fill the whole framebuffer with a solid colour. Phase-1 proof the pipeline works.
pub fn fill(fb: &FbInfo, color: u32) {
    let words = (fb.pitch / 4) * fb.height;
    let p = fb.base as *mut u32;
    // SAFETY: [base, base + pitch*height) is the GPU-allocated framebuffer, Device-mapped by `init`.
    unsafe {
        for i in 0..words {
            p.add(i as usize).write_volatile(color);
        }
    }
}
