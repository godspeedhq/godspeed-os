// SPDX-License-Identifier: GPL-2.0-only
//! Framebuffer text console (fbcon) - Phase 1: boot output mirrored to the
//! display (§11.4). Output-only.
//!
//! Renders **antialiased Noto Sans Mono** glyphs (`noto-sans-mono-bitmap`, the
//! Regular weight at a 20 px raster) into Limine's linear framebuffer. Each glyph
//! pixel is a 0-255 intensity, blended against the soft-green foreground, so text
//! is smooth rather than the blocky look of a 1-bpp bitmap font. Every byte written
//! to the serial console is also handed to `put_byte` here, so the monitor shows
//! exactly what the serial console shows - boot logs, `supervisor: ready`,
//! ping/pong, the lot.
//!
//! Box-drawing glyphs (`tree`'s `├──`, `│`, …) are drawn **procedurally** - the
//! font's Basic-Latin range carries no U+2500 block, and procedural strokes connect
//! cell-to-cell exactly.
//!
//! Lives in the arch layer (§18.1) because it writes framebuffer memory
//! directly. The framebuffer is mapped by Limine in the higher half (PML4
//! entries 256-511), which `PageTable::new` copies into every task address
//! space, so the pointer stays valid for the system lifetime - no explicit
//! mapping is required.

use crate::smp::spinlock::SpinLock;
use limine::framebuffer::Framebuffer;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

/// Noto weight + raster height for the console. `get_raster_width`/`RasterHeight::val`
/// are `const fn`, so the per-cell pixel box (`CELL_W` × `CELL_H`) is known at compile
/// time and the char-grid geometry below is computed from it. Size20 ≈ 9×20 px - a touch
/// taller than the old 16×16 bitmap cell and far smoother on a TV.
const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size20;
const CELL_W: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
const CELL_H: usize = RASTER_HEIGHT.val();

/// Integer font-scale factor for the current framebuffer. The glyph raster is a fixed CELL_W x CELL_H
/// pixels; on a dense panel (the Dell Wyse 5070's native mode) that renders as a wall of tiny text, so
/// each glyph pixel is upscaled to a `scale x scale` block. Chosen to target ~30 text rows: 1x on a
/// T630-class panel, ~2x around 1080p, 3x on a very high resolution. Larger cells also mean fewer
/// rows/cols, so a `scroll` (which repaints every cell) touches fewer cells and boot renders faster.
#[inline]
fn cell_scale(s: &Fb) -> usize {
    ((s.height + 300) / 600).clamp(1, 3)
}

/// Reserved cell bytes for the **box-drawing** glyphs (`tree`). The grid stores these
/// high bytes; `draw_box_glyph` renders them with procedural strokes. The UTF-8 decoder
/// maps `U+2500..U+253C` to them via `cell_for_codepoint`.
const BOX_FIRST: u8 = 0xB3;

/// Map a decoded Unicode codepoint to the internal **cell byte** the grid stores. ASCII
/// passes through (rendered via Noto); the light box-drawing block (`U+2500..U+253C`)
/// maps to the reserved high bytes (rendered procedurally); anything else becomes `?` -
/// visible, never silently dropped (§3.12).
fn cell_for_codepoint(cp: u32) -> u8 {
    if cp < 0x80 {
        return cp as u8;
    }
    match cp {
        0x2500 => 0xC4, // ─
        0x2502 => 0xB3, // │
        0x250C => 0xDA, // ┌
        0x2510 => 0xBF, // ┐
        0x2514 => 0xC0, // └
        0x2518 => 0xD9, // ┘
        0x251C => 0xC3, // ├
        0x2524 => 0xB4, // ┤
        0x252C => 0xC2, // ┬
        0x2534 => 0xC1, // ┴
        0x253C => 0xC5, // ┼
        _ => b'?',
    }
}

/// Which of the four arms a box-drawing cell byte has: `(up, down, left, right)`.
/// The procedural renderer draws a stroke for each present arm out from the cell
/// centre, so neighbouring cells join seamlessly.
fn box_arms(ch: u8) -> (bool, bool, bool, bool) {
    match ch {
        0xC4 => (false, false, true, true),  // ─
        0xB3 => (true, true, false, false),  // │
        0xDA => (false, true, false, true),  // ┌
        0xBF => (false, true, true, false),  // ┐
        0xC0 => (true, false, false, true),  // └
        0xD9 => (true, false, true, false),  // ┘
        0xC3 => (true, true, false, true),   // ├
        0xB4 => (true, true, true, false),   // ┤
        0xC2 => (false, true, true, true),   // ┬
        0xC1 => (true, false, true, true),   // ┴
        0xC5 => (true, true, true, true),    // ┼
        _ => (false, false, false, false),
    }
}

/// Char-grid shadow bounds. Sized for up to ~4K UHD edge-to-edge at the Noto cell
/// (3840/9 ≈ 427 cols, 2160/20 = 108 rows); larger displays clamp the text area to
/// these bounds. The shadow holds each cell's printable content so `scroll` can
/// redraw from RAM instead of reading the framebuffer back - uncached/WC VRAM reads
/// run ~100x slower than writes, the fbcon scroll trap that made a respawn look 40x
/// a cold spawn (see the iso-c7/iso-xlife investigation).
const MAX_COLS: usize = 448;
const MAX_ROWS: usize = 128;

/// Framebuffer console state. The base pointer is stored as `usize` so the
/// struct is `Send` (a raw pointer would not be), which `SpinLock<T: Send>`
/// requires to be `Sync` as a `static`.
struct Fb {
    base: usize,   // framebuffer base virtual address (Limine HHDM)
    pitch: usize,  // bytes per scanline
    bpp: usize,    // bytes per pixel
    width: usize,  // visible width in pixels
    height: usize, // visible height in pixels
    org_x: usize,  // left edge of the text area (safe-area inset for TV overscan)
    org_y: usize,  // top edge of the text area
    cols: usize,   // text columns within the safe area
    rows: usize,   // text rows within the safe area
    col: usize,    // cursor column
    row: usize,    // cursor row
    fg: u32,       // foreground pixel value (already in the device's channel layout)
    bg: u32,       // background pixel value
    // Foreground as raw 0-255 channel components plus the device's channel shifts,
    // so a glyph pixel's 0-255 antialiasing intensity can be blended toward the
    // background per channel (`blend`) and composed into the device pixel layout.
    fg_r: u32,
    fg_g: u32,
    fg_b: u32,
    r_shift: u32,
    g_shift: u32,
    b_shift: u32,
    ready: bool,   // false until fb_init runs; put_byte no-ops until then
    // --- Minimal ANSI escape parser (Stage 2a) ---
    // The console service drives the terminal by emitting a small ANSI subset
    // (clear, cursor position, erase line, hide/show cursor). The same escapes
    // work on a serial terminal for free. State persists across put_byte calls
    // because an escape sequence spans several bytes.
    esc: u8,             // 0 = normal, 1 = saw ESC, 2 = inside CSI (after '[')
    csi_priv: bool,      // saw '?' immediately after '[' (private-mode sequence)
    csi_params: [u16; 3],// numeric parameters (e.g. row;col)
    csi_nparam: usize,   // count of parameters accumulated
    // UTF-8 decode: accumulate a multi-byte sequence into a codepoint. `utf8_remaining` is
    // how many continuation bytes are still expected (0 = not mid-sequence).
    utf8_cp: u32,
    utf8_remaining: u8,
    cursor_visible: bool,// draw the underline cursor (off for full-screen apps)
    cur_col: usize,      // column where the cursor underline was last drawn
    cur_row: usize,      // row where the cursor underline was last drawn
    // Char-grid shadow: the printable content of each text cell (the transient
    // cursor overlay is excluded - it is always erased before a scroll). `scroll`
    // shifts this in RAM and redraws the screen from it, so it never reads the
    // (uncached) framebuffer back.
    grid: [[u8; MAX_COLS]; MAX_ROWS],
    // Precomputed foreground-blend LUT: blend_lut[intensity] = the glyph-pixel colour for that
    // antialiasing intensity, composed in the device layout. Lets an antialiased glyph edge blit as a
    // table read instead of a per-pixel multiply/divide - the last bit of scroll smoothness at 4K.
    blend_lut: [u32; 256],
}

static FB: SpinLock<Fb> = SpinLock::new(Fb {
    base: 0, pitch: 0, bpp: 0, width: 0, height: 0,
    org_x: 0, org_y: 0, cols: 0, rows: 0, col: 0, row: 0, fg: 0, bg: 0,
    fg_r: 0, fg_g: 0, fg_b: 0, r_shift: 0, g_shift: 0, b_shift: 0, ready: false,
    esc: 0, csi_priv: false, csi_params: [0; 3], csi_nparam: 0, utf8_cp: 0, utf8_remaining: 0,
    cursor_visible: true, cur_col: 0, cur_row: 0,
    grid: [[b' '; MAX_COLS]; MAX_ROWS],
    blend_lut: [0; 256],
});

/// Safe-area inset per edge, as a percentage of each dimension. TVs overscan (crop ~3-5%
/// off every edge), which clips the outermost characters at `0`. `5` insets the text by 5%
/// per edge so it all stays visible without depending on the TV's "Just Scan" / "1:1"
/// picture mode (which most sets bury or don't offer). Set this to `0` only on a display
/// known not to overscan, or when the TV is in a 1:1 pixel-mapping mode, for true
/// edge-to-edge. Harmless on a monitor - just a small border.
const SAFE_PCT: usize = 5;

/// Initialise the console from Limine's framebuffer descriptor. Called once in
/// `_start`, right after `serial_init`, before the first `kprintln`.
pub fn fb_init(fb: &Framebuffer) {
    // Compose pixel values in the framebuffer's own channel layout via the
    // reported mask shifts, so we render correct colours on RGB or BGR devices.
    let (rs, gs, bs) = (
        fb.red_mask_shift as u32,
        fb.green_mask_shift as u32,
        fb.blue_mask_shift as u32,
    );
    let make = |r: u32, g: u32, b: u32| -> u32 { (r << rs) | (g << gs) | (b << bs) };

    let mut s = FB.lock();
    s.base = fb.address() as usize;
    s.pitch = fb.pitch as usize;
    s.bpp = (fb.bpp as usize) / 8;
    s.width = fb.width as usize;
    s.height = fb.height as usize;
    // Inset the text area by SAFE_PCT on each edge (0 = edge-to-edge full screen).
    s.org_x = s.width * SAFE_PCT / 100;
    s.org_y = s.height * SAFE_PCT / 100;
    let sc = ((s.height + 300) / 600).clamp(1, 3); // integer font scale (see cell_scale)
    s.cols = (s.width - 2 * s.org_x) / (CELL_W * sc);
    s.rows = (s.height - 2 * s.org_y) / (CELL_H * sc);
    // Clamp the text area to the char-grid shadow bounds (only matters above ~4K).
    s.cols = s.cols.min(MAX_COLS);
    s.rows = s.rows.min(MAX_ROWS);
    s.col = 0;
    s.row = 0;
    // soft green on black - classic console look. Keep the raw components + channel
    // shifts so antialiased glyph intensities can be blended per channel (`blend`).
    s.fg_r = 0x80; s.fg_g = 0xFF; s.fg_b = 0x80;
    s.r_shift = rs; s.g_shift = gs; s.b_shift = bs;
    s.fg = make(s.fg_r, s.fg_g, s.fg_b);
    s.bg = make(0x00, 0x00, 0x00);
    // Precompute the blend LUT (see `blend`): foreground scaled by each 0-255 antialiasing intensity,
    // composed in the device channel layout. Background is black, so blend_lut[0] == bg (0) and
    // blend_lut[255] == fg. Turns every glyph-edge pixel into a table read instead of a multiply/divide.
    for i in 0..256u32 {
        let (r, g, b) = (s.fg_r * i / 255, s.fg_g * i / 255, s.fg_b * i / 255);
        s.blend_lut[i as usize] = (r << rs) | (g << gs) | (b << bs);
    }
    s.esc = 0;
    s.csi_nparam = 0;
    s.cursor_visible = true;
    s.ready = true;
    clear(&mut s);
    // Report the panel geometry + the chosen font scale (drop the FB lock first: kprintln renders to
    // this same console, so logging while holding it would re-enter the lock). Confirms the resolution
    // that drives the scale - the Wyse boots a much denser mode than the T630.
    let (w, h, bpp, cols, rows) = (s.width, s.height, s.bpp, s.cols, s.rows);
    drop(s);
    crate::kprintln!(
        "fb: {}x{} {}bpp, font-scale {}x -> {} cols x {} rows",
        w, h, bpp * 8, sc, cols, rows
    );
}

/// Framebuffer text geometry packed as `(rows << 16) | cols`, or 0 if the
/// console has not been initialised. Exposed to userspace via `InspectKernel`
/// query 9 so the console service can lay out its terminal.
pub fn dims_packed() -> u32 {
    let s = FB.lock();
    if !s.ready { return 0; }
    (((s.rows as u32) & 0xFFFF) << 16) | ((s.cols as u32) & 0xFFFF)
}

/// Clear the framebuffer and move the cursor to the top-left. Used when the shell
/// ends boot-log mirroring (`console_boot_complete`) to hand over a clean console.
pub fn clear_and_home() {
    let mut s = FB.lock();
    if !s.ready { return; }
    clear(&mut s);
    s.col = 0;
    s.row = 0;
    s.esc = 0;
    if s.cursor_visible {
        draw_cursor(&mut s);
    }
}

/// Write one output byte to the framebuffer console. Called from
/// `console_write_byte` / `console_write_bytes` (Stage 1: only the interactive
/// console path reaches the framebuffer; logs go to serial only).
///
/// Recognises a minimal ANSI escape subset (Stage 2a) so the console service can
/// drive a terminal: `ESC[2J` clear, `ESC[H`/`ESC[r;cH` cursor position,
/// `ESC[K`/`ESC[2K` erase line, `ESC[J` erase to end of screen, `ESC[?25l/h`
/// hide/show cursor. Unrecognised escapes are consumed and dropped.
pub fn put_byte(b: u8) {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    process_byte(&mut s, b);
    wc_flush();
}

/// Write a whole byte sequence under a SINGLE lock, then flush once. Used by the
/// console path so a multi-byte write (e.g. the shell's `gsh> ` prompt) is atomic
/// with respect to another core's console output - no byte from another core can
/// interleave mid-string.
pub fn put_bytes(bytes: &[u8]) {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    for &b in bytes {
        process_byte(&mut s, b);
    }
    wc_flush();
}

/// Flush write-combining framebuffer stores so they are globally visible before
/// the FB lock is released. The framebuffer is mapped write-combining (Limine's
/// HHDM default); the lock's atomic release orders normal memory but NOT the WC
/// store buffer. Without this, a scroll on one core can flush *after* the next
/// line's first glyph drawn on another core - erasing it ("gsh>" → " s>").
#[inline]
fn wc_flush() {
    // SAFETY: SFENCE is always valid in any privilege level; it only orders stores.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)); }
}

/// Process one output byte against the (locked) console state. Caller holds the
/// FB lock and is responsible for `wc_flush()` before releasing it.
fn process_byte(s: &mut Fb, b: u8) {
    // --- Escape-sequence state machine ---
    match s.esc {
        1 => {
            // Saw ESC; expect '[' to start a CSI sequence. Anything else: drop.
            if b == b'[' {
                s.esc = 2;
                s.csi_priv = false;
                s.csi_params = [0; 3];
                s.csi_nparam = 0;
            } else {
                s.esc = 0;
            }
            return;
        }
        2 => {
            handle_csi(s, b);
            return;
        }
        _ => {}
    }
    if b == 0x1B {
        s.esc = 1;
        return;
    }

    // --- UTF-8 decode (so the console renders ├──, │, etc., not garbled bytes) ---
    if s.utf8_remaining > 0 {
        // Mid-sequence: fold a continuation byte into the codepoint; render when complete.
        if b & 0xC0 == 0x80 {
            s.utf8_cp = (s.utf8_cp << 6) | (b & 0x3F) as u32;
            s.utf8_remaining -= 1;
            if s.utf8_remaining == 0 {
                put_printable_cell(s, cell_for_codepoint(s.utf8_cp));
            }
            return;
        }
        s.utf8_remaining = 0; // malformed - abandon the sequence and reprocess this byte
    }
    if b >= 0x80 {
        // Lead byte: begin a 2/3/4-byte sequence; a stray continuation/invalid lead is a `?`.
        if b & 0xE0 == 0xC0 { s.utf8_cp = (b & 0x1F) as u32; s.utf8_remaining = 1; return; }
        if b & 0xF0 == 0xE0 { s.utf8_cp = (b & 0x0F) as u32; s.utf8_remaining = 2; return; }
        if b & 0xF8 == 0xF0 { s.utf8_cp = (b & 0x07) as u32; s.utf8_remaining = 3; return; }
        put_printable_cell(s, b'?');
        return;
    }

    // --- Control / printable ASCII byte ---
    // A `\r` moves to column 0 over already-drawn text (e.g. the prompt); stamping the cursor
    // there and erasing it next byte would blank that text, so `\r` doesn't redraw the cursor.
    match b {
        b'\n' => { cursor_off(s); advance_line(s); cursor_on(s); }
        b'\r' => { cursor_off(s); s.col = 0; }
        0x08 | 0x7f => { cursor_off(s); if s.col > 0 { s.col -= 1; } cursor_on(s); }
        0x20..=0x7e => put_printable_cell(s, b),
        _ => {}
    }
}

/// Draw a printable cell byte at the write position and advance. Self-contained: erases the
/// cursor first and redraws it after, so it renders an ASCII byte and a UTF-8-decoded glyph
/// (which may be a box-drawing cell byte > 0x7e) the same way.
fn put_printable_cell(s: &mut Fb, cell: u8) {
    cursor_off(s);
    let (c, r) = (s.col, s.row);
    draw_glyph(s, cell, c, r);
    grid_set(s, c, r, cell);
    s.col += 1;
    if s.col >= s.cols {
        advance_line(s);
    }
    cursor_on(s);
}

/// Erase the underline cursor if it is visible (so a move/draw leaves no trail).
#[inline]
fn cursor_off(s: &Fb) {
    if s.cursor_visible {
        erase_cursor(s);
    }
}

/// Redraw the underline cursor at the write position if it is visible.
#[inline]
fn cursor_on(s: &mut Fb) {
    if s.cursor_visible {
        draw_cursor(s);
    }
}

/// Handle one byte inside a CSI (`ESC[`) sequence. Accumulates numeric
/// parameters until a final byte (0x40..=0x7e) dispatches the command.
fn handle_csi(s: &mut Fb, b: u8) {
    match b {
        b'0'..=b'9' => {
            if s.csi_nparam == 0 {
                s.csi_nparam = 1;
            }
            let i = s.csi_nparam - 1;
            if i < s.csi_params.len() {
                s.csi_params[i] = s.csi_params[i]
                    .saturating_mul(10)
                    .saturating_add((b - b'0') as u16);
            }
        }
        b';' => {
            if s.csi_nparam == 0 {
                s.csi_nparam = 1; // empty leading parameter defaults to 0
            }
            if s.csi_nparam < s.csi_params.len() {
                s.csi_nparam += 1;
            }
        }
        b'?' => {
            s.csi_priv = true;
        }
        0x40..=0x7e => {
            execute_csi(s, b);
            s.esc = 0;
        }
        _ => {
            // Malformed - abort the sequence.
            s.esc = 0;
        }
    }
}

/// `csi_params[i]`, or `default` if fewer than `i+1` parameters were given.
fn csi_param(s: &Fb, i: usize, default: u16) -> u16 {
    if i < s.csi_nparam {
        s.csi_params[i]
    } else {
        default
    }
}

/// Dispatch a complete CSI command given its final byte.
fn execute_csi(s: &mut Fb, final_byte: u8) {
    // Erase the underline cursor before any geometry change so it leaves no trail.
    if s.cursor_visible {
        erase_cursor(s);
    }
    match final_byte {
        // Cursor position: ESC[r;cH or ESC[r;cf (1-based; defaults to 1,1 = home).
        b'H' | b'f' => {
            let r = csi_param(s, 0, 1).max(1) as usize - 1;
            let c = csi_param(s, 1, 1).max(1) as usize - 1;
            s.row = r.min(s.rows.saturating_sub(1));
            s.col = c.min(s.cols.saturating_sub(1));
        }
        // Erase in display: 2 = whole screen + home; 0 (default) = cursor to end.
        b'J' => match csi_param(s, 0, 0) {
            2 => {
                clear(s);
                s.row = 0;
                s.col = 0;
            }
            _ => erase_to_end_of_screen(s),
        },
        // Erase in line: 2 = whole line; 0 (default) = cursor to end of line.
        b'K' => match csi_param(s, 0, 0) {
            2 => erase_line_full(s),
            _ => erase_line_to_eol(s),
        },
        // Private mode set/reset: ESC[?25h shows the cursor, ESC[?25l hides it.
        b'h' if s.csi_priv && csi_param(s, 0, 0) == 25 => s.cursor_visible = true,
        b'l' if s.csi_priv && csi_param(s, 0, 0) == 25 => s.cursor_visible = false,
        _ => {} // unsupported command - ignore
    }
    if s.cursor_visible {
        draw_cursor(s);
    }
}

/// Blank cells from the cursor column to the end of the current row.
fn erase_line_to_eol(s: &mut Fb) {
    let (row, col, cols) = (s.row, s.col, s.cols);
    for c in col..cols {
        draw_glyph(s, b' ', c, row);
        grid_set(s, c, row, b' ');
    }
}

/// Blank every cell on the current row.
fn erase_line_full(s: &mut Fb) {
    let (row, cols) = (s.row, s.cols);
    for c in 0..cols {
        draw_glyph(s, b' ', c, row);
        grid_set(s, c, row, b' ');
    }
}

/// Blank from the cursor to the end of the screen (rest of this row, then every
/// row below it).
fn erase_to_end_of_screen(s: &mut Fb) {
    erase_line_to_eol(s);
    let (rows, cols, start) = (s.rows, s.cols, s.row + 1);
    for r in start..rows {
        for c in 0..cols {
            draw_glyph(s, b' ', c, r);
            grid_set(s, c, r, b' ');
        }
    }
}

/// Draw the text cursor as a true underline at the current write position: paint the
/// cell's real glyph first, then overlay a thin underline beneath it - so a character the
/// cursor sits on stays visible (underlined), instead of being hidden by a `_` glyph that
/// replaced it. Remember where it landed so `erase_cursor` can restore exactly that cell
/// later, even after the write position has moved (e.g. a carriage return).
fn draw_cursor(s: &mut Fb) {
    let (c, r) = (s.col, s.row);
    let ch = if r < MAX_ROWS && c < MAX_COLS { s.grid[r][c] } else { b' ' };
    draw_glyph(s, ch, c, r);
    // Underline: the bottom ~2 px of the cell, in the foreground colour.
    let sc = cell_scale(s);
    let (cellw, cellh) = (CELL_W * sc, CELL_H * sc);
    let x0 = s.org_x + c * cellw;
    let y0 = s.org_y + r * cellh;
    let th = (2 * sc).min(cellh);
    fill_rect(s, x0, y0 + cellh - th, cellw, th, s.fg);
    s.cur_col = c;
    s.cur_row = r;
}

/// Erase the cursor at the cell where it was last drawn by restoring that cell's real
/// content from the shadow grid - NOT by blanking it. The cursor underline is drawn over
/// whatever glyph occupies the cell (the grid is not touched), so restoring the grid glyph
/// removes the underline without destroying text. Blanking instead would erase any
/// character the cursor sits on, which is exactly what made moving the cursor back over
/// typed text (Left arrow, Home) delete it. Using the *remembered* position (not the
/// current write position) keeps a carriage return from touching real text elsewhere.
fn erase_cursor(s: &Fb) {
    let ch = if s.cur_row < MAX_ROWS && s.cur_col < MAX_COLS {
        s.grid[s.cur_row][s.cur_col]
    } else {
        b' '
    };
    draw_glyph(s, ch, s.cur_col, s.cur_row);
}

/// Move the cursor to the start of the next row, scrolling if at the bottom.
fn advance_line(s: &mut Fb) {
    s.col = 0;
    if s.row + 1 >= s.rows {
        scroll(s); // bottom row freed and cleared; cursor stays on the last row
    } else {
        s.row += 1;
    }
}

/// Clear the whole framebuffer to the background colour. `bg` is black (all
/// channels zero ⇒ all bytes zero), so a flat byte-zero fill is correct.
fn clear(s: &mut Fb) {
    let base = s.base as *mut u8;
    let total = s.height * s.pitch;
    // SAFETY: [base, base+total) is the framebuffer Limine mapped and sized
    // (height*pitch); it is valid for writes for the system lifetime.
    unsafe { core::ptr::write_bytes(base, 0, total) };
    // Shadow: every cell is now blank.
    for r in 0..MAX_ROWS {
        for c in 0..MAX_COLS {
            s.grid[r][c] = b' ';
        }
    }
}

/// Write a single pixel at (x, y) in the device's pixel layout.
#[inline]
fn put_pixel(s: &Fb, x: usize, y: usize, color: u32) {
    if x >= s.width || y >= s.height {
        return;
    }
    let off = y * s.pitch + x * s.bpp;
    let base = s.base as *mut u8;
    // SAFETY: off < height*pitch (x,y bounds-checked; bpp ≤ pitch/width); the
    // framebuffer is mapped for the system lifetime.
    unsafe {
        // 32bpp is the near-universal case: one aligned 32-bit store (off is 4-aligned - pitch and
        // x*bpp are multiples of 4 for a 32bpp framebuffer) instead of a per-byte loop.
        if s.bpp == 4 {
            *(base.add(off) as *mut u32) = color;
        } else {
            let mut i = 0;
            while i < s.bpp {
                *base.add(off + i) = (color >> (i * 8)) as u8;
                i += 1;
            }
        }
    }
}

/// Fill a solid `w x h` pixel rectangle at (x, y). Writes each row as a contiguous run - one aligned
/// 32-bit store per pixel on the 32bpp path, not a per-byte loop - so glyph upscale-blocks and cell
/// clears blit fast (write-combining coalesces the sequential stores into bursts). The boot scroll
/// repaints the whole text area, and on the Wyse's 4K panel a per-pixel byte loop there is exactly
/// what made it crawl.
#[inline]
fn fill_rect(s: &Fb, x: usize, y: usize, w: usize, h: usize, color: u32) {
    if x >= s.width || y >= s.height {
        return;
    }
    let xw = (x + w).min(s.width);
    let yh = (y + h).min(s.height);
    let base = s.base as *mut u8;
    for yy in y..yh {
        let row = yy * s.pitch + x * s.bpp;
        // SAFETY: [row, row + (xw-x)*bpp) is within the mapped framebuffer (x,y,xw,yh clamped to
        // width/height; bpp*width <= pitch); row is 4-aligned on the 32bpp path. Mapped for life.
        unsafe {
            if s.bpp == 4 {
                let mut p = base.add(row) as *mut u32;
                for _ in x..xw {
                    *p = color;
                    p = p.add(1);
                }
            } else {
                let mut off = row;
                for _ in x..xw {
                    let mut i = 0;
                    while i < s.bpp {
                        *base.add(off + i) = (color >> (i * 8)) as u8;
                        i += 1;
                    }
                    off += s.bpp;
                }
            }
        }
    }
}

/// The glyph-pixel colour for an antialiasing `intensity` (0-255): 0 gives the (black)
/// background, 255 gives full foreground, and in between gives the smooth edges. Reads
/// the value straight from `blend_lut`, which `fb_init` precomputes once (foreground
/// scaled per channel, composed into the device's channel layout).
#[inline]
fn blend(s: &Fb, intensity: u8) -> u32 {
    // Table lookup (fb_init precomputed blend_lut[intensity] = fg scaled by intensity, composed in the
    // device layout), so an antialiased glyph edge is one read instead of three multiplies + three
    // divides per pixel - the last bit of scroll smoothness on a dense 4K panel.
    s.blend_lut[intensity as usize]
}

/// Render one glyph at text cell (col, row). ASCII (`< 0x80`) renders via the
/// antialiased Noto raster (every cell pixel is written - intensity 0 paints the
/// background, so the cell is fully repainted with no stale pixels). The reserved
/// high bytes render as procedural box-drawing strokes.
fn draw_glyph(s: &Fb, ch: u8, col: usize, row: usize) {
    let sc = cell_scale(s);
    let x0 = s.org_x + col * CELL_W * sc;
    let y0 = s.org_y + row * CELL_H * sc;
    if ch >= BOX_FIRST && box_arms(ch) != (false, false, false, false) {
        draw_box_glyph(s, ch, x0, y0);
        return;
    }
    // Noto raster: `raster()` is `height` rows of `width` intensity bytes; the crate
    // guarantees those dims equal CELL_H x CELL_W for this weight/size, so iterating the
    // raster covers the whole cell. An unknown char falls back to a blank cell. Each raster
    // pixel is upscaled to an `sc x sc` device-pixel block so the fixed-size glyph stays
    // readable on a dense panel (cell_scale); sc == 1 is the original 1:1 path.
    let rc = match get_raster(ch as char, FONT_WEIGHT, RASTER_HEIGHT) {
        Some(rc) => rc,
        None => {
            clear_cell(s, x0, y0);
            return;
        }
    };
    let raster = rc.raster();
    let (cw, chh) = (CELL_W * sc, CELL_H * sc);
    // The cell lies fully inside the framebuffer (cols/rows are sized to fit) - but guard, so the
    // unchecked contiguous run below can never write past the mapping (fall back to the safe blit).
    if s.bpp != 4 || x0 + cw > s.width || y0 + chh > s.height {
        for (gy, rowpix) in raster.iter().enumerate() {
            for (gx, &intensity) in rowpix.iter().enumerate() {
                fill_rect(s, x0 + gx * sc, y0 + gy * sc, sc, sc, blend(s, intensity));
            }
        }
        return;
    }
    // Fast 32bpp path: write each output row as ONE contiguous run of `cw` aligned u32 stores (best
    // write-combining, no per-pixel fill_rect call). Each raster pixel's colour is blended once and
    // replicated `sc` times horizontally; the raster row is replicated `sc` times vertically.
    let base = s.base as *mut u8;
    for (gy, rowpix) in raster.iter().enumerate() {
        for sy in 0..sc {
            let yy = y0 + gy * sc + sy;
            // SAFETY: the whole [x0, x0+cw) x [y0, y0+chh) cell is in-bounds (checked above); this
            // writes exactly `cw` pixels of row `yy`, and yy < height, x0+cw <= width. bpp == 4 so
            // `x0*4` and `pitch` are multiples of 4 - the u32 stores are aligned. Mapped for life.
            unsafe {
                let mut p = base.add(yy * s.pitch + x0 * 4) as *mut u32;
                for &intensity in rowpix.iter() {
                    let color = blend(s, intensity);
                    for _ in 0..sc {
                        *p = color;
                        p = p.add(1);
                    }
                }
            }
        }
    }
}

/// Paint a whole cell to the background colour (used when a char has no raster).
fn clear_cell(s: &Fb, x0: usize, y0: usize) {
    let sc = cell_scale(s);
    fill_rect(s, x0, y0, CELL_W * sc, CELL_H * sc, s.bg);
}

/// Draw a procedural box-drawing glyph at pixel origin (x0, y0). Each present arm is a
/// stroke from the cell edge to the centre; arms overlap at the centre so adjacent cells
/// connect into continuous lines. Stroke thickness is ~2 px, centred on the cell axes.
fn draw_box_glyph(s: &Fb, ch: u8, x0: usize, y0: usize) {
    clear_cell(s, x0, y0);
    let sc = cell_scale(s);
    let (cellw, cellh) = (CELL_W * sc, CELL_H * sc);
    let (up, down, left, right) = box_arms(ch);
    let th = (cellw / 5).max(2); // stroke thickness in px (~2*sc)
    let vx = cellw.saturating_sub(th) / 2; // left edge of the vertical stroke band
    let hy = cellh.saturating_sub(th) / 2; // top edge of the horizontal stroke band
    let fill = |xs: usize, xe: usize, ys: usize, ye: usize| {
        for y in ys..ye.min(cellh) {
            for x in xs..xe.min(cellw) {
                put_pixel(s, x0 + x, y0 + y, s.fg);
            }
        }
    };
    // Vertical arms span the stroke columns [vx, vx+th); they reach to the centre band's
    // far edge so the cross is solid.
    if up {
        fill(vx, vx + th, 0, hy + th);
    }
    if down {
        fill(vx, vx + th, hy, cellh);
    }
    // Horizontal arms span the stroke rows [hy, hy+th).
    if left {
        fill(0, vx + th, hy, hy + th);
    }
    if right {
        fill(vx, cellw, hy, hy + th);
    }
}

/// Record a cell's printable character in the shadow grid. Bounds-guarded; cols/rows
/// are clamped to the grid in `fb_init`, so in practice every cell is in range.
#[inline]
fn grid_set(s: &mut Fb, c: usize, r: usize, ch: u8) {
    if r < MAX_ROWS && c < MAX_COLS {
        s.grid[r][c] = ch;
    }
}

/// Scroll the display up by one text row.
///
/// The old implementation `core::ptr::copy`'d the framebuffer up in place - which
/// *reads the framebuffer back*. The framebuffer is uncached / write-combining, so
/// those reads run at tens of MB/s; an 8 MB read-back cost ~130 ms per scrolled line
/// on the T630, which dominated every kill/respawn-heavy workload (the iso-c7 /
/// iso-xlife dig). Instead, shift the char-grid shadow up in normal RAM (fast) and
/// repaint the text area from it - **write-only** to the framebuffer (WC writes are
/// ~100x faster than reads).
fn scroll(s: &mut Fb) {
    let rows = s.rows;
    let cols = s.cols;
    if rows == 0 {
        return;
    }
    // Shift the shadow up one row in RAM; blank the freed bottom row.
    for r in 0..rows - 1 {
        for c in 0..cols {
            s.grid[r][c] = s.grid[r + 1][c];
        }
    }
    for c in 0..cols {
        s.grid[rows - 1][c] = b' ';
    }
    // Repaint every cell from the shadow - no framebuffer read-back.
    for r in 0..rows {
        for c in 0..cols {
            let ch = s.grid[r][c];
            draw_glyph(s, ch, c, r);
        }
    }
}
