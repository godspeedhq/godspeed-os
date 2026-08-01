// SPDX-License-Identifier: GPL-2.0-only
//! x86-64 backend for the shared framebuffer console (`crate::fbcon`).
//!
//! The terminal itself - escape parsing, the character grid, glyph rendering, scrolling - is neutral
//! code. This file is only what is genuinely architecture-specific: reading the framebuffer's geometry
//! out of the Limine boot protocol, and publishing writes to a **write-combining** mapping.
//!
//! The framebuffer Limine maps lives in the higher half (PML4 entries 256-511), which `PageTable::new`
//! copies into every task address space, so the pointer stays valid for the system lifetime and no
//! explicit mapping is required.

use limine::framebuffer::Framebuffer;

/// Reading this framebuffer back is **expensive**: Limine's HHDM maps it write-combining, where reads
/// run at tens of MB/s while writes are roughly 100x faster. So `fbcon::scroll` repaints from the shadow
/// grid rather than shifting pixels in place - an 8 MB read-back cost about 130 ms per scrolled line on
/// the T630 and dominated every kill/respawn-heavy workload.
pub const FB_READBACK_CHEAP: bool = false;

/// Publish a written rectangle. On x86 the framebuffer is coherent with the scanout engine, so there is
/// nothing to clean and the rectangle is ignored - but the write-combining store buffer still has to be
/// drained before the console lock is released.
///
/// The lock's atomic release orders normal memory but NOT the WC store buffer. Without this fence, a
/// scroll on one core can flush *after* the next line's first glyph drawn on another core, erasing it
/// (`gsh>` became ` s>`).
#[inline]
pub fn fb_commit(
    _base: usize,
    _pitch: usize,
    _bpp: usize,
    _x: usize,
    _y: usize,
    _w: usize,
    _h: usize,
) {
    // SAFETY: SFENCE is valid at any privilege level; it only orders stores.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}

/// Bring up the console from Limine's framebuffer descriptor. Called once in `_start`, right after
/// `serial_init` and before the first `kprintln`, so all boot output mirrors to the display.
///
/// This is where the framebuffer becomes a **slice**, which is what keeps the neutral console
/// (`crate::fbcon`) free of `unsafe` - only the arch knows that the mapping is valid and permanent.
pub fn fb_init(fb: &Framebuffer) {
    let len = (fb.pitch as usize) * (fb.height as usize);
    // SAFETY: Limine mapped [address, address + pitch*height) as this framebuffer and reports its own
    // geometry, so the region is valid for writes for exactly that length. It lives in the higher half
    // (PML4 entries 256-511), which `PageTable::new` copies into every task address space, so it stays
    // mapped for the system lifetime - hence 'static. `fb_init` runs once, on the BSP, before any other
    // core is started, and nothing else in the kernel takes a reference to the framebuffer, so this is
    // the only live reference and the exclusivity of `&mut` holds.
    let mem: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(fb.address() as *mut u8, len) };
    crate::fbcon::init(crate::fbcon::FbParams {
        mem,
        pitch: fb.pitch as usize,
        bpp: (fb.bpp as usize) / 8,
        width: fb.width as usize,
        height: fb.height as usize,
        r_shift: fb.red_mask_shift as u32,
        g_shift: fb.green_mask_shift as u32,
        b_shift: fb.blue_mask_shift as u32,
    });
}
