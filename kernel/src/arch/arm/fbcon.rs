// SPDX-License-Identifier: GPL-2.0-only
//! Minimal framebuffer text console for the Pi 2 - renders the serial stream onto the HDMI display so
//! the boot log and the `gsh>` prompt appear on the TV, not just the UART.
//!
//! This is deliberately a self-contained first cut (reusing the same `noto-sans-mono-bitmap` font as the
//! x86 console): draw a glyph, advance, wrap, and clear-and-home when the screen fills. It has no scroll
//! or antialiasing-fast-paths yet - those, and sharing the x86 renderer through a neutral module, are
//! the follow-up. It mirrors serial rather than replacing it, so serial stays the source of truth.

use core::sync::atomic::{AtomicBool, Ordering};
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size20;
const CELL_W: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
const CELL_H: usize = RASTER_HEIGHT.val();

/// Black background + green foreground, matching the x86 console look. Colours are packed in the
/// framebuffer's channel order (RED in the low byte - see `video::rgb`), so they read correctly.
const BG: u32 = super::video::rgb(0x00, 0x00, 0x00); // black
const FG: (u32, u32, u32) = (0x33, 0xFF, 0x66);      // terminal green

struct Fbcon {
    base: usize,
    pitch: usize,
    width: usize,
    height: usize,
    org_x: usize, // left inset (overscan safe area)
    org_y: usize, // top inset
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}
static mut FBCON: Fbcon =
    Fbcon { base: 0, pitch: 0, width: 0, height: 0, org_x: 0, org_y: 0, cols: 0, rows: 0, col: 0, row: 0 };
static READY: AtomicBool = AtomicBool::new(false);

/// True once `init` has set up the console; `pl011_write` mirrors to the framebuffer only after this.
pub fn ready() -> bool { READY.load(Ordering::Relaxed) }

/// Set up the console over an already-mapped framebuffer, clear it to the background, and go live.
pub fn init(base: u32, pitch: u32, width: u32, height: u32) {
    // SAFETY: single-threaded boot; FBCON is set once here before READY is published.
    unsafe {
        let c = &mut *core::ptr::addr_of_mut!(FBCON);
        c.base = base as usize;
        c.pitch = pitch as usize;
        c.width = width as usize;
        c.height = height as usize;
        // Inset a ~5% margin so text lands inside the TV's overscan-cropped visible area (an HDMI TV
        // typically drops the outer edge). The text grid is sized to the safe area, not the full panel.
        c.org_x = c.width / 20;
        c.org_y = c.height / 20;
        c.cols = (c.width - 2 * c.org_x) / CELL_W;
        c.rows = (c.height - 2 * c.org_y) / CELL_H;
        c.col = 0;
        c.row = 0;
        clear(c);
    }
    READY.store(true, Ordering::Release);
}

#[inline]
fn put_pixel(c: &Fbcon, x: usize, y: usize, color: u32) {
    if x >= c.width || y >= c.height { return; }
    // SAFETY: (x,y) in bounds; the framebuffer is cacheable RAM, one aligned u32 store per pixel. The
    // write lands in the cache; `clean_rect` publishes it to the GPU afterwards.
    unsafe { ((c.base + y * c.pitch + x * 4) as *mut u32).write_volatile(color); }
}

/// Publish a written pixel rectangle to the Point of Coherency so the GPU (which scans RAM, not the CPU
/// cache) sees it. The framebuffer is cacheable (see `section_fb`), so every glyph/scroll/clear writes to
/// the cache and MUST be cleaned by MVA (DCCMVAC) here or the display shows stale pixels. Cleans row by
/// row because a rectangle is strided in memory (each scanline is `pitch` apart).
fn clean_rect(c: &Fbcon, x0: usize, y0: usize, w: usize, h: usize) {
    let y_end = (y0 + h).min(c.height);
    let bytes = (w.min(c.width.saturating_sub(x0)) * 4) as u32;
    let mut y = y0;
    while y < y_end {
        super::page_tables::clean_dcache((c.base + y * c.pitch + x0 * 4) as u32, bytes);
        y += 1;
    }
}

fn clear(c: &Fbcon) {
    for y in 0..c.height {
        for x in 0..c.width {
            put_pixel(c, x, y, BG);
        }
    }
    // Publish the whole framebuffer to the GPU (one contiguous clean of the full buffer).
    super::page_tables::clean_dcache(c.base as u32, (c.pitch * c.height) as u32);
}

/// Blend the foreground over the background by an antialiasing intensity (0 = bg, 255 = fg).
#[inline]
fn blend(intensity: u8) -> u32 {
    let i = intensity as u32;
    let j = 255 - i;
    let br = BG & 0xFF; let bg = (BG >> 8) & 0xFF; let bb = (BG >> 16) & 0xFF;
    let r = (FG.0 * i + br * j) / 255;
    let g = (FG.1 * i + bg * j) / 255;
    let b = (FG.2 * i + bb * j) / 255;
    r | (g << 8) | (b << 16)
}

fn draw_glyph(c: &Fbcon, ch: u8, col: usize, row: usize) {
    let x0 = c.org_x + col * CELL_W;
    let y0 = c.org_y + row * CELL_H;
    match get_raster(ch as char, FONT_WEIGHT, RASTER_HEIGHT) {
        Some(rc) => {
            for (gy, rowpix) in rc.raster().iter().enumerate() {
                for (gx, &intensity) in rowpix.iter().enumerate() {
                    put_pixel(c, x0 + gx, y0 + gy, blend(intensity));
                }
            }
        }
        None => {
            // Unknown glyph: leave a blank (repaint the cell background, no stale pixels).
            for gy in 0..CELL_H {
                for gx in 0..CELL_W {
                    put_pixel(c, x0 + gx, y0 + gy, BG);
                }
            }
        }
    }
    // Publish this glyph cell to the GPU.
    clean_rect(c, x0, y0, CELL_W, CELL_H);
}

/// Scroll the framebuffer up by one text row: move the pixel rows below the top cell-row up over it,
/// then clear the freed bottom band to the background. The framebuffer is cacheable (`section_fb`), so
/// the memmove reads + writes the cache - fast and smooth, not the uncached crawl a non-cacheable buffer
/// forced. The shifted region is then published to the GPU by one `clean_rect` over the text area. This
/// replaces the old clear-and-home so the screen never blanks - the flicker seen on the TV under output.
fn scroll(c: &Fbcon) {
    let row_bytes = c.cols * CELL_W * 4; // one text row, inset width
    // SAFETY: every source/destination row is inside the mapped framebuffer's text region (cacheable
    // RAM), so the byte memmove is valid; margins stay fixed (only the inset region moves).
    unsafe {
        // Shift the text region up by one cell row, row by row (strided - margins untouched).
        for gy in 0..((c.rows - 1) * CELL_H) {
            let dst = (c.base + (c.org_y + gy) * c.pitch + c.org_x * 4) as *mut u8;
            let src = (c.base + (c.org_y + gy + CELL_H) * c.pitch + c.org_x * 4) as *const u8;
            core::ptr::copy(src, dst, row_bytes);
        }
        // Clear the freed bottom text row to BG.
        for gy in ((c.rows - 1) * CELL_H)..(c.rows * CELL_H) {
            let row = (c.base + (c.org_y + gy) * c.pitch + c.org_x * 4) as *mut u32;
            for i in 0..(c.cols * CELL_W) {
                row.add(i).write_volatile(BG);
            }
        }
    }
    // Publish the whole shifted inset text region to the GPU in one pass.
    clean_rect(c, c.org_x, c.org_y, c.cols * CELL_W, c.rows * CELL_H);
}

/// Render one byte of the serial stream. Handles CR/LF/backspace; printable ASCII draws a glyph and
/// advances. When the screen fills, scroll up one row (no blanking).
pub fn put_byte(b: u8) {
    if !READY.load(Ordering::Relaxed) { return; }
    // SAFETY: FBCON is published; concurrent callers are serialized by pl011_write's SERIAL_BUSY guard
    // (the only caller), so there is a single writer at a time.
    unsafe {
        let c = &mut *core::ptr::addr_of_mut!(FBCON);
        match b {
            b'\n' => { c.col = 0; c.row += 1; }
            b'\r' => { c.col = 0; }
            0x08 => { if c.col > 0 { c.col -= 1; draw_glyph(c, b' ', c.col, c.row); } }
            0x20..=0x7E => { draw_glyph(c, b, c.col, c.row); c.col += 1; }
            _ => {} // control byte: ignore (serial keeps the full stream)
        }
        if c.col >= c.cols { c.col = 0; c.row += 1; }
        if c.row >= c.rows {
            scroll(c);
            c.row = c.rows - 1;
            c.col = 0;
        }
    }
}

pub fn put_bytes(s: &[u8]) {
    for &b in s {
        put_byte(b);
    }
}
