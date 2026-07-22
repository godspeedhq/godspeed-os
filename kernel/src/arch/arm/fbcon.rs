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
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
}
static mut FBCON: Fbcon =
    Fbcon { base: 0, pitch: 0, width: 0, height: 0, cols: 0, rows: 0, col: 0, row: 0 };
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
        c.cols = c.width / CELL_W;
        c.rows = c.height / CELL_H;
        c.col = 0;
        c.row = 0;
        clear(c);
    }
    READY.store(true, Ordering::Release);
}

#[inline]
fn put_pixel(c: &Fbcon, x: usize, y: usize, color: u32) {
    if x >= c.width || y >= c.height { return; }
    // SAFETY: (x,y) in bounds; the framebuffer is device-mapped RAM, one aligned u32 store per pixel.
    unsafe { ((c.base + y * c.pitch + x * 4) as *mut u32).write_volatile(color); }
}

fn clear(c: &Fbcon) {
    for y in 0..c.height {
        for x in 0..c.width {
            put_pixel(c, x, y, BG);
        }
    }
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
    let x0 = col * CELL_W;
    let y0 = row * CELL_H;
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
}

/// Scroll the framebuffer up by one text row: move the pixel rows below the top cell-row up over it,
/// then clear the freed bottom band to the background. Reads + writes the framebuffer, which is Normal
/// non-cacheable (mapped so by `map_framebuffer`), so a byte memmove is valid (no Device alignment
/// rules) and reaches the display. This replaces the old clear-and-home so the screen never blanks -
/// the flicker seen on the TV under continuous output.
fn scroll(c: &Fbcon) {
    let shift = CELL_H * c.pitch;   // bytes in one text row
    let total = c.height * c.pitch; // bytes in the whole framebuffer
    // SAFETY: [base, base+total) is the mapped framebuffer; the copy stays inside it (shift < total),
    // and Normal-NC memory permits an unaligned byte memmove.
    unsafe {
        let base = c.base as *mut u8;
        core::ptr::copy(base.add(shift), base, total - shift);
        // Clear the freed bottom band to BG (aligned u32 stores).
        let bottom = (c.base + (total - shift)) as *mut u32;
        for i in 0..(shift / 4) {
            bottom.add(i).write_volatile(BG);
        }
    }
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
