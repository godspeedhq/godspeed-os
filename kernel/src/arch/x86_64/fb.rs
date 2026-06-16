// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Framebuffer text console (fbcon) — Phase 1: boot output mirrored to the
//! display (§11.4). Output-only.
//!
//! Renders a public-domain 8x8 bitmap font (`font8x8`) at 2x scale (16x16 px
//! per glyph) into Limine's linear framebuffer. Every byte written to the
//! serial console is also handed to `put_byte` here, so the monitor shows
//! exactly what the serial console shows — boot logs, `supervisor: ready`,
//! ping/pong, the lot.
//!
//! Lives in the arch layer (§18.1) because it writes framebuffer memory
//! directly. The framebuffer is mapped by Limine in the higher half (PML4
//! entries 256–511), which `PageTable::new` copies into every task address
//! space, so the pointer stays valid for the system lifetime — no explicit
//! mapping is required.

use crate::smp::spinlock::SpinLock;
use core::sync::atomic::{AtomicU32, Ordering};
use limine::framebuffer::Framebuffer;

/// Font glyph lookup. `font8x8` legacy basic font: 8 rows per glyph, bit
/// `(1 << x)` of a row is the pixel at column `x` (LSB = leftmost).
#[inline]
fn glyph(ch: u8) -> [u8; 8] {
    let idx = ch as usize;
    if idx < 128 {
        font8x8::legacy::BASIC_LEGACY[idx]
    } else {
        [0u8; 8]
    }
}

/// Integer upscale factor: an 8x8 font cell becomes 16x16 px — readable on a TV.
const SCALE: usize = 2;
const GLYPH_W: usize = 8 * SCALE;
const GLYPH_H: usize = 8 * SCALE;

/// Char-grid shadow bounds. Sized for up to ~4K UHD with the 10% safe-area inset
/// (3072/16 = 192 cols, 1728/16 = 108 rows); larger displays clamp the text area to
/// these bounds. The shadow holds each cell's printable content so `scroll` can
/// redraw from RAM instead of reading the framebuffer back — uncached/WC VRAM reads
/// run ~100x slower than writes, the fbcon scroll trap that made a respawn look 40x
/// a cold spawn (see the iso-c7/iso-xlife investigation).
const MAX_COLS: usize = 256;
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
    cursor_visible: bool,// draw the underline cursor (off for full-screen apps)
    cur_col: usize,      // column where the cursor underline was last drawn
    cur_row: usize,      // row where the cursor underline was last drawn
    blink_shown: bool,   // is the blinking underline currently drawn (vs blanked)?
    // Char-grid shadow: the printable content of each text cell (the transient
    // cursor overlay is excluded — it is always erased before a scroll). `scroll`
    // shifts this in RAM and redraws the screen from it, so it never reads the
    // (uncached) framebuffer back.
    grid: [[u8; MAX_COLS]; MAX_ROWS],
}

static FB: SpinLock<Fb> = SpinLock::new(Fb {
    base: 0, pitch: 0, bpp: 0, width: 0, height: 0,
    org_x: 0, org_y: 0, cols: 0, rows: 0, col: 0, row: 0, fg: 0, bg: 0, ready: false,
    esc: 0, csi_priv: false, csi_params: [0; 3], csi_nparam: 0, cursor_visible: true,
    cur_col: 0, cur_row: 0, blink_shown: true,
    grid: [[b' '; MAX_COLS]; MAX_ROWS],
});

/// Safe-area inset per edge, as a percentage of each dimension. TVs overscan
/// (crop) ~3–5% off every edge; insetting the text by 5% keeps it all visible
/// without depending on the TV's "Just Scan" / "Screen Fit" / "Full pixel"
/// setting. Harmless on a monitor (no overscan) — just a small border.
const SAFE_PCT: usize = 10;

/// Initialise the console from Limine's framebuffer descriptor. Called once in
/// `_start`, right after `serial_init`, before the first `kprintln`.
pub fn fb_init(fb: &Framebuffer) {
    // Compose pixel values in the framebuffer's own channel layout via the
    // reported mask shifts, so we render correct colours on RGB or BGR devices.
    let make = |r: u32, g: u32, b: u32| -> u32 {
        (r << fb.red_mask_shift) | (g << fb.green_mask_shift) | (b << fb.blue_mask_shift)
    };

    let mut s = FB.lock();
    s.base = fb.address() as usize;
    s.pitch = fb.pitch as usize;
    s.bpp = (fb.bpp as usize) / 8;
    s.width = fb.width as usize;
    s.height = fb.height as usize;
    // Inset the text area by SAFE_PCT on each edge so TV overscan can't clip it.
    s.org_x = s.width * SAFE_PCT / 100;
    s.org_y = s.height * SAFE_PCT / 100;
    s.cols = (s.width - 2 * s.org_x) / GLYPH_W;
    s.rows = (s.height - 2 * s.org_y) / GLYPH_H;
    // Clamp the text area to the char-grid shadow bounds (only matters above ~4K).
    s.cols = s.cols.min(MAX_COLS);
    s.rows = s.rows.min(MAX_ROWS);
    s.col = 0;
    s.row = 0;
    s.fg = make(0x80, 0xFF, 0x80); // soft green on black — classic console look
    s.bg = make(0x00, 0x00, 0x00);
    s.esc = 0;
    s.csi_nparam = 0;
    s.cursor_visible = true;
    s.ready = true;
    clear(&mut s);
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
/// with respect to another core's console output — no byte from another core can
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
/// line's first glyph drawn on another core — erasing it ("gsh>" → " s>").
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

    // --- Normal byte ---
    // Erase the underline cursor at the current cell before changing position or
    // drawing, so it never leaves a trail. Redraw it at the new position after.
    if s.cursor_visible {
        erase_cursor(s);
    }
    // Carriage return moves to column 0, which usually still holds drawn text
    // (e.g. the prompt). Stamping the cursor there and erasing it on the next byte
    // would blank that text, so a `\r` does not redraw the cursor — the next glyph
    // write or the newline's fresh line will place it.
    let mut redraw_cursor = true;
    match b {
        b'\n' => advance_line(s),
        b'\r' => {
            s.col = 0;
            redraw_cursor = false;
        }
        0x08 | 0x7f => {
            if s.col > 0 {
                s.col -= 1;
            }
        }
        0x20..=0x7e => {
            // The glyph is drawn at the (now blank) cursor cell.
            let (c, r) = (s.col, s.row);
            draw_glyph(s, b, c, r);
            grid_set(s, c, r, b);
            s.col += 1;
            if s.col >= s.cols {
                advance_line(s);
            }
        }
        _ => {}
    }
    // The cursor follows the write position: a steady underline at the cell where
    // the next character will land. Framebuffer only — a serial terminal draws its
    // own cursor. Full-screen apps hide it via ESC[?25l.
    if s.cursor_visible && redraw_cursor {
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
            // Malformed — abort the sequence.
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
        _ => {} // unsupported command — ignore
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

/// Draw the text cursor (a steady underline) at the current write position, and
/// remember where it landed so `erase_cursor` can blank exactly that cell later —
/// even after the write position has since moved (e.g. a carriage return).
fn draw_cursor(s: &mut Fb) {
    draw_glyph(s, b'_', s.col, s.row);
    s.cur_col = s.col;
    s.cur_row = s.row;
    // A fresh draw (after any output/keystroke) starts the blink phase "shown", so the
    // cursor is solid right after activity and only blinks once the prompt goes idle.
    s.blink_shown = true;
}

/// Blink interval, in 10 ms timer ticks: toggle every ~500 ms (a 1 s on/off cycle).
const BLINK_PERIOD_TICKS: u32 = 50;
static BLINK_TICKS: AtomicU32 = AtomicU32::new(0);

/// Called every timer tick on core 0 (next to `process_pending`/`uart_rx_poll`). Toggles the
/// cursor underline every `BLINK_PERIOD_TICKS` so a ready prompt visibly blinks. Cheap: it
/// redraws a single cell, and only when the period elapses.
pub fn cursor_blink_tick() {
    if BLINK_TICKS.fetch_add(1, Ordering::Relaxed) % BLINK_PERIOD_TICKS == 0 {
        toggle_cursor_blink();
    }
}

/// Flip the cursor cell between the underline and the character underneath it (normally blank,
/// since the cursor sits at the next write position). `try_lock` — never block the timer ISR:
/// if a console write holds the FB lock (it can be mid-write on this very core), skip this frame
/// and blink on the next tick. A hidden/locked frame is harmless — the next write redraws solid.
fn toggle_cursor_blink() {
    let mut s = match FB.try_lock() { Some(g) => g, None => return };
    if !s.ready || !s.cursor_visible { return; }
    let (c, r) = (s.cur_col, s.cur_row);
    if s.blink_shown {
        let under = s.grid[r][c];        // restore the cell content (a space at the prompt)
        draw_glyph(&s, under, c, r);
        s.blink_shown = false;
    } else {
        draw_glyph(&s, b'_', c, r);
        s.blink_shown = true;
    }
}

/// Erase the cursor at the cell where it was last drawn (blank it). Using the
/// remembered position — not the current write position — is what stops a
/// carriage return from blanking real text: after `\r` moves the column to 0 over
/// existing characters, the cursor is still erased at its old cell, leaving the
/// text (the `g` of `gsh>`) intact.
fn erase_cursor(s: &Fb) {
    draw_glyph(s, b' ', s.cur_col, s.cur_row);
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
        let mut i = 0;
        while i < s.bpp {
            *base.add(off + i) = (color >> (i * 8)) as u8;
            i += 1;
        }
    }
}

/// Render one glyph at text cell (col, row), scaled by `SCALE`.
fn draw_glyph(s: &Fb, ch: u8, col: usize, row: usize) {
    let bits = glyph(ch);
    let x0 = s.org_x + col * GLYPH_W;
    let y0 = s.org_y + row * GLYPH_H;
    for gy in 0..8 {
        let rowbits = bits[gy];
        for gx in 0..8 {
            let on = (rowbits >> gx) & 1 != 0; // LSB = leftmost
            let color = if on { s.fg } else { s.bg };
            for sy in 0..SCALE {
                for sx in 0..SCALE {
                    put_pixel(s, x0 + gx * SCALE + sx, y0 + gy * SCALE + sy, color);
                }
            }
        }
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
/// The old implementation `core::ptr::copy`'d the framebuffer up in place — which
/// *reads the framebuffer back*. The framebuffer is uncached / write-combining, so
/// those reads run at tens of MB/s; an 8 MB read-back cost ~130 ms per scrolled line
/// on the T630, which dominated every kill/respawn-heavy workload (the iso-c7 /
/// iso-xlife dig). Instead, shift the char-grid shadow up in normal RAM (fast) and
/// repaint the text area from it — **write-only** to the framebuffer (WC writes are
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
    // Repaint every cell from the shadow — no framebuffer read-back.
    for r in 0..rows {
        for c in 0..cols {
            let ch = s.grid[r][c];
            draw_glyph(s, ch, c, r);
        }
    }
}
