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
    // ANSI escape-sequence state for the console stream: 0 = normal, 1 = saw ESC, 2 = inside CSI.
    // The shell's line editor emits CSI sequences - notably `ESC[K` (erase to end of line) after each
    // echoed character. A serial terminal interprets those; fbcon used to drop only the ESC byte and then
    // draw "[K" as literal glyphs, which is the garble seen on the TV when typing (`h[Ke[Kl[Kp[K`).
    ansi: u8,
    csi_param: u32,
    csi_private: bool,   // the CSI carried a `?` (a private sequence, e.g. `ESC[?25l` hide cursor)
    // The underline cursor: where it was last painted, so it can be lifted before the write position
    // moves. Full-screen apps (edit, observe) turn it off with `ESC[?25l` and back on with `ESC[?25h`.
    cursor_visible: bool,
    cur_col: usize,
    cur_row: usize,
    cursor_on: bool,     // is the underline currently painted at (cur_col, cur_row)?
}
static mut FBCON: Fbcon = Fbcon {
    base: 0, pitch: 0, width: 0, height: 0, org_x: 0, org_y: 0, cols: 0, rows: 0, col: 0, row: 0,
    ansi: 0, csi_param: 0, csi_private: false,
    cursor_visible: true, cur_col: 0, cur_row: 0, cursor_on: false,
};
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
        c.ansi = 0;
        c.csi_param = 0;
        c.csi_private = false;
        c.cursor_visible = true;
        c.cur_col = 0;
        c.cur_row = 0;
        c.cursor_on = false;
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

/// Height of the underline cursor, in pixels at the bottom of the cell.
const CURSOR_H: usize = 2;

/// Paint or lift the underline cursor at `(cur_col, cur_row)`. `on = true` paints it in the foreground
/// colour; `on = false` lifts it by repainting that strip in the background colour.
///
/// Unlike x86's console this keeps no shadow grid of the screen's characters, so lifting the cursor
/// repaints the strip rather than redrawing the glyph underneath. That is exact for the normal case -
/// during typing the cursor always sits one cell PAST the last character, on a blank cell - and at worst
/// clips the bottom 2 px of a descender (g, y, p) when the cursor is moved back over text with Left/Home.
/// The shell redraws the line on edits, so even that is transient. Keeping a full grid to avoid a 2 px
/// artifact is not worth the memory or the code here.
fn paint_cursor(c: &Fbcon, on: bool) {
    if c.cur_row >= c.rows || c.cur_col >= c.cols { return; }
    let x0 = c.org_x + c.cur_col * CELL_W;
    let y0 = c.org_y + c.cur_row * CELL_H + CELL_H - CURSOR_H;
    let colour = if on { blend(255) } else { BG };
    for gy in 0..CURSOR_H {
        for gx in 0..CELL_W {
            put_pixel(c, x0 + gx, y0 + gy, colour);
        }
    }
    clean_rect(c, x0, y0, CELL_W, CURSOR_H);
}

/// Lift the cursor if it is currently painted (called before anything moves the write position).
fn cursor_lift(c: &mut Fbcon) {
    if c.cursor_on {
        paint_cursor(c, false);
        c.cursor_on = false;
    }
}

/// Paint the cursor at the current write position, if the cursor is enabled.
fn cursor_show(c: &mut Fbcon) {
    if !c.cursor_visible { return; }
    c.cur_col = c.col;
    c.cur_row = c.row;
    paint_cursor(c, true);
    c.cursor_on = true;
}

/// `ESC[K` (Erase in Line): blank the current row from the cursor to the end of the text area. The shell's
/// line editor emits this after each echoed character to wipe any leftover tail.
fn erase_to_eol(c: &Fbcon) {
    if c.row >= c.rows || c.col >= c.cols { return; }
    let x0 = c.org_x + c.col * CELL_W;
    let y0 = c.org_y + c.row * CELL_H;
    let w = (c.cols - c.col) * CELL_W;
    for gy in 0..CELL_H {
        for gx in 0..w {
            put_pixel(c, x0 + gx, y0 + gy, BG);
        }
    }
    clean_rect(c, x0, y0, w, CELL_H);
}

/// Render one byte into the console state. Consumes ANSI CSI escape sequences (so they are acted on,
/// not drawn as literal text); handles CR/LF/backspace; printable ASCII draws a glyph and advances.
/// When the screen fills, scroll up one row (no blanking).
///
/// The CURSOR is not touched here - the callers below lift it before a run and repaint it after, so a
/// bulk write moves it once instead of once per byte.
fn put_byte_inner(c: &mut Fbcon, b: u8) {
    // --- ANSI escape handling (before any glyph is drawn) ---
    if c.ansi == 1 {                                   // previous byte was ESC
        c.ansi = if b == b'[' { 2 } else { 0 };        // only CSI (`ESC[`) is understood
        c.csi_param = 0;
        c.csi_private = false;
        return;
    }
    if c.ansi == 2 {                                   // inside a CSI sequence
        if (0x30..=0x3F).contains(&b) {                // parameter bytes: digits, ';', '?'
            if b == b'?' { c.csi_private = true; }     // `ESC[?...` - a private sequence
            if b.is_ascii_digit() {
                c.csi_param = c.csi_param.saturating_mul(10) + (b - b'0') as u32;
            }
            return;
        }
        if (0x20..=0x2F).contains(&b) { return; }      // intermediate bytes
        // 0x40..=0x7E: the final byte ends the sequence.
        c.ansi = 0;
        let p = c.csi_param;
        let private = c.csi_private;
        c.csi_param = 0;
        c.csi_private = false;
        match b {
            b'K' => erase_to_eol(c),                   // EL - what the line editor uses
            b'J' if p == 2 => { clear(c); c.col = 0; c.row = 0; } // ED(2) - clear screen
            // `ESC[?25l` / `ESC[?25h` - hide / show the cursor. Full-screen apps (edit, observe) hide it
            // for the session so their bulk redraws do not smear an underline across the screen.
            b'l' if private && p == 25 => c.cursor_visible = false,
            b'h' if private && p == 25 => c.cursor_visible = true,
            _ => {}                                    // SGR colours, cursor moves, ... : ignored
        }
        return;
    }
    if b == 0x1B { c.ansi = 1; c.csi_param = 0; c.csi_private = false; return; } // ESC starts a sequence
    match b {
        b'\n' => { c.col = 0; c.row += 1; }
        b'\r' => { c.col = 0; }
        // Backspace is a NON-DESTRUCTIVE cursor-left move, exactly as on a terminal. Drawing a space
        // here (as this did) erases the character the cursor lands on - so moving LEFT along a line
        // deleted the text instead of just moving through it, and the line editor's redraw, which
        // repositions with a run of backspaces after reprinting the tail, wiped what it had just drawn.
        // A caller that wants to erase sends the classic backspace-space-backspace itself.
        //
        // At column 0 it steps back to the END OF THE PREVIOUS ROW, because a line longer than the
        // screen has WRAPPED onto that row and is still one logical line. Stopping at column 0 (as
        // this did) made editing break at exactly the screen width - the line editor repositions with
        // a RUN of backspaces, and once that run hit the wrap point it could go no further, so every
        // redraw after it landed in the wrong place. That is the "typing boundary".
        0x08 => {
            if c.col > 0 {
                c.col -= 1;
            } else if c.row > 0 {
                c.row -= 1;
                c.col = c.cols.saturating_sub(1);
            }
        }
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

/// Render one byte, then leave the underline cursor at the new write position.
pub fn put_byte(b: u8) {
    if !READY.load(Ordering::Relaxed) { return; }
    // SAFETY: FBCON is published; concurrent callers are serialized by pl011_write's SERIAL_BUSY guard
    // (the only caller), so there is a single writer at a time.
    unsafe {
        let c = &mut *core::ptr::addr_of_mut!(FBCON);
        cursor_lift(c);
        put_byte_inner(c, b);
        cursor_show(c);
    }
}

/// Render a run of bytes, moving the underline cursor ONCE for the whole run rather than once per byte.
/// Lifting and repainting per byte would double the pixel traffic of bulk output (a `help` listing, a
/// scroll) for no visible benefit - the intermediate positions are never seen. Lifting BEFORE the run
/// also keeps the underline from being scrolled up the screen as a stray mark.
pub fn put_bytes(s: &[u8]) {
    if !READY.load(Ordering::Relaxed) { return; }
    // SAFETY: as `put_byte` - single writer, serialized by the SERIAL_BUSY guard in pl011_write.
    unsafe {
        let c = &mut *core::ptr::addr_of_mut!(FBCON);
        cursor_lift(c);
        for &b in s { put_byte_inner(c, b); }
        cursor_show(c);
    }
}
