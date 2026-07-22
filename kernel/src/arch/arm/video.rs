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
fn mbox_call(channel: u32) -> bool {
    // SAFETY: MBOX is a 16-byte-aligned static touched only on this single-threaded boot path; the
    // mailbox registers are Device-mapped MMIO. Cache maintenance brackets the exchange so the GPU sees
    // our request and we see its reply (the buffer is Normal cacheable RAM, the GPU a second observer).
    unsafe {
        let addr = core::ptr::addr_of!(MBOX) as u32;
        super::page_tables::clean_invalidate_dcache_all(); // publish the request to RAM
        core::arch::asm!("dsb", options(nostack));
        while MBOX_STATUS.read_volatile() & MBOX_FULL != 0 {}
        MBOX_WRITE.write_volatile((addr & !0xF) | (channel & 0xF));
        loop {
            while MBOX_STATUS.read_volatile() & MBOX_EMPTY != 0 {}
            let r = MBOX_READ.read_volatile();
            if (r & 0xF) == channel && (r & !0xF) == (addr & !0xF) {
                break;
            }
        }
        super::page_tables::clean_invalidate_dcache_all(); // re-read the GPU's reply from RAM
        core::arch::asm!("dsb", "isb", options(nostack));
        MBOX.data[1] == RESP_SUCCESS
    }
}

/// Ask the GPU for a 32-bpp framebuffer at `width` x `height`, map it Device so ARM writes reach the
/// display, and return its descriptor. `None` (logged) if the mailbox call fails or returns nothing.
pub fn init(width: u32, height: u32) -> Option<FbInfo> {
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
    super::mmu::map_framebuffer(base, pitch * h);
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
