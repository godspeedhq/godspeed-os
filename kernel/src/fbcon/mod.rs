// SPDX-License-Identifier: GPL-2.0-only
//! Framebuffer text console - the terminal every architecture shares.
//!
//! Boot output and the interactive console are mirrored to the display (§11.4), so the monitor shows
//! exactly what the serial console shows: boot logs, `supervisor: ready`, ping/pong, the `gsh>` prompt.
//! Serial remains the source of truth; this mirrors it.
//!
//! **Why this is neutral code.** A linear framebuffer is `(base, pitch, bpp, channel shifts)` and a
//! terminal is an ANSI state machine over a character grid. Neither is architecture-specific. What *is*
//! architecture-specific is only how those parameters are learned (a Limine descriptor on x86, a GPU
//! mailbox call on the Pi) and how a write is **published** to the scanout engine. So the arch hands
//! over an `FbParams` at init and implements one primitive, `arch::imp::fb_commit`; everything else -
//! the escape parser, the UTF-8 decoder, the shadow grid, the cursor, scrolling, glyph rendering - lives
//! here and is written once.
//!
//! This replaced two independent implementations (`arch/x86_64/fb.rs` and `arch/arm/fbcon.rs`) that had
//! each fixed terminal bugs the other still had: x86 silently ignored the reverse-video the shell emits
//! to highlight a row, and ARM lacked the shadow grid that lets the cursor underline a glyph without
//! destroying it. One terminal, so a fix lands once.
//!
//! ## The arch contract (see §26.10 and the SEC-27 rule that a primitive owes a semantic)
//!
//! - [`FbParams`] describes the framebuffer. `bg` is always black, which lets `clear` be a byte-zero
//!   fill on any channel layout.
//! - `arch::imp::fb_commit(base, pitch, bpp, x, y, w, h)` publishes a written rectangle. It is called
//!   **once per locked batch**, with the bounding box of everything drawn in that batch. An arch that
//!   maps the framebuffer *cacheable* (ARM: the GPU scans RAM, not the CPU cache) cleans that region to
//!   the Point of Coherency; an arch that maps it *write-combining* (x86) ignores the region and issues
//!   a store fence instead.
//! - `arch::imp::FB_READBACK_CHEAP` says whether reading the framebuffer back is as cheap as writing
//!   it, which selects the scroll strategy - see [`scroll`].

use crate::smp::spinlock::SpinLock;

mod render;

use render::{CELL_H, CELL_W};

/// Char-grid shadow bounds. Sized for up to about 4K UHD edge-to-edge at the Noto cell
/// (3840/9 = 427 cols, 2160/20 = 108 rows); larger displays clamp the text area to these bounds.
pub(crate) const MAX_COLS: usize = 448;
pub(crate) const MAX_ROWS: usize = 128;

/// Attribute-bitmap row stride: one bit per column, rounded up to whole bytes.
const ATTR_STRIDE: usize = MAX_COLS.div_ceil(8);

/// Safe-area inset per edge, as a percentage of each dimension. TVs overscan (crop off every edge),
/// which clips the outermost characters at `0`. This insets the text so it stays visible without
/// depending on the TV's "Just Scan" / "1:1" picture mode (which most sets bury or do not offer). `5` is
/// the standard title-safe margin (10% total per axis): clipping LOSES text (functionally bad), a border
/// is merely cosmetic (harmless), so we bias toward the larger inset. `2` overscanned little on the
/// Wyse's TV but CLIPPED text on the T630's display, so `5` is the safe default that clears typical
/// consumer overscan on both. On a low-overscan panel this just leaves a thin border - never lose a
/// character to save one.
const SAFE_PCT: usize = 5;

/// Console foreground, as raw 0-255 channel components. Soft green on black - the classic console look.
const FG_RGB: (u32, u32, u32) = (0x80, 0xFF, 0x80);

/// Height of the underline cursor, in raster pixels before `cell_scale`.
const CURSOR_TH: usize = 2;

/// What the architecture must tell the console about its framebuffer. Everything here is plain linear
/// framebuffer geometry; nothing in it is arch-specific beyond the values themselves.
pub struct FbParams {
    /// The framebuffer itself. The arch maps it and hands it over as a slice valid for the system
    /// lifetime, which is what keeps every pixel write in this module bounds-checked and `unsafe`-free
    /// (§18.1 - see the `render` module header). Must be at least `pitch * height` bytes.
    pub mem: &'static mut [u8],
    /// Bytes per scanline.
    pub pitch: usize,
    /// Bytes per pixel (4 on every framebuffer we have met; the slow path handles others).
    pub bpp: usize,
    /// Visible width in pixels.
    pub width: usize,
    /// Visible height in pixels.
    pub height: usize,
    /// Bit position of the red channel within a pixel.
    pub r_shift: u32,
    /// Bit position of the green channel within a pixel.
    pub g_shift: u32,
    /// Bit position of the blue channel within a pixel.
    pub b_shift: u32,
}

/// Framebuffer console state. Everything in it is `Send`, which `SpinLock<T: Send>` requires to be
/// `Sync` as a `static` - including the framebuffer, which is held as a slice rather than a raw
/// pointer (a raw pointer would not be `Send`, and a slice is bounds-checked besides).
pub(crate) struct Fb {
    // The mapped framebuffer, or None before `init`. Held as a slice so every write below is
    // bounds-checked; the one `unsafe` that produces it lives in the arch backend.
    mem: Option<&'static mut [u8]>,
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
    bg: u32,       // background pixel value (always black - see FbParams)
    ready: bool,   // false until init runs; put_byte no-ops until then

    // --- ANSI escape parser ---
    // The shell and the full-screen apps drive the terminal with a small ANSI subset (clear, cursor
    // position and movement, erase line, reverse video, hide/show cursor). The same escapes work on a
    // serial terminal for free. State persists across put_byte calls because a sequence spans bytes.
    esc: u8,              // 0 = normal, 1 = saw ESC, 2 = inside CSI (after '[')
    csi_priv: bool,       // saw '?' immediately after '[' (private-mode sequence)
    csi_params: [u16; 4], // numeric parameters (e.g. row;col)
    csi_nparam: usize,    // count of parameters accumulated
    reverse: bool,        // SGR reverse video (ESC[7m), reset by ESC[0m

    // --- UTF-8 decode ---
    // Accumulate a multi-byte sequence into a codepoint so the box-drawing UI renders as frames rather
    // than mojibake. `utf8_remaining` is how many continuation bytes are still expected (0 = idle).
    utf8_cp: u32,
    utf8_remaining: u8,

    // --- Cursor ---
    cursor_visible: bool, // draw the underline cursor (off for full-screen apps)
    cur_col: usize,       // column where the cursor underline was last drawn
    cur_row: usize,       // row where the cursor underline was last drawn

    // --- Shadow grid ---
    // The printable content of each text cell (the transient cursor overlay is excluded - it is always
    // erased before a scroll), plus one reverse-video bit per cell. `scroll` shifts these in RAM and
    // repaints from them, so it never reads the framebuffer back; `erase_cursor` restores the real glyph
    // under the cursor instead of blanking it. Storing the attribute alongside the character is what
    // keeps a reverse-video row correct after a repaint - without it a redraw would silently lose the
    // highlight. One bit per cell, so the whole attribute plane is ATTR_STRIDE * MAX_ROWS bytes.
    grid: [[u8; MAX_COLS]; MAX_ROWS],
    attr: [[u8; ATTR_STRIDE]; MAX_ROWS],

    // --- Publication ---
    // Bounding box of everything drawn since the last commit, in device pixels (see render::dirty).
    dirty_x: usize,
    dirty_y: usize,
    dirty_w: usize,
    dirty_h: usize,

    // Precomputed foreground-blend LUT: blend_lut[intensity] = the glyph-pixel colour for that
    // antialiasing intensity, composed in the device layout. Lets an antialiased glyph edge blit as a
    // table read instead of a per-pixel multiply/divide.
    blend_lut: [u32; 256],
}

static FB: SpinLock<Fb> = SpinLock::new(Fb {
    mem: None,
    pitch: 0,
    bpp: 0,
    width: 0,
    height: 0,
    org_x: 0,
    org_y: 0,
    cols: 0,
    rows: 0,
    col: 0,
    row: 0,
    fg: 0,
    bg: 0,
    ready: false,
    esc: 0,
    csi_priv: false,
    csi_params: [0; 4],
    csi_nparam: 0,
    reverse: false,
    utf8_cp: 0,
    utf8_remaining: 0,
    cursor_visible: true,
    cur_col: 0,
    cur_row: 0,
    grid: [[b' '; MAX_COLS]; MAX_ROWS],
    attr: [[0; ATTR_STRIDE]; MAX_ROWS],
    dirty_x: 0,
    dirty_y: 0,
    dirty_w: 0,
    dirty_h: 0,
    blend_lut: [0; 256],
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialise the console from the architecture's framebuffer description. Called once during boot,
/// right after serial init, before the first `kprintln` - so all boot output mirrors to the display.
pub fn init(p: FbParams) {
    // Compose pixel values in the framebuffer's own channel layout via the reported mask shifts, so we
    // render correct colours on an RGB or a BGR device.
    let (rs, gs, bs) = (p.r_shift, p.g_shift, p.b_shift);

    let mut s = FB.lock();
    s.pitch = p.pitch;
    s.bpp = p.bpp;
    s.width = p.width;
    s.height = p.height;
    s.mem = Some(p.mem);
    // Inset the text area by SAFE_PCT on each edge.
    s.org_x = s.width * SAFE_PCT / 100;
    s.org_y = s.height * SAFE_PCT / 100;
    let sc = render::cell_scale(&s);
    s.cols = ((s.width - 2 * s.org_x) / (CELL_W * sc)).min(MAX_COLS);
    s.rows = ((s.height - 2 * s.org_y) / (CELL_H * sc)).min(MAX_ROWS);
    s.col = 0;
    s.row = 0;
    s.fg = (FG_RGB.0 << rs) | (FG_RGB.1 << gs) | (FG_RGB.2 << bs);
    s.bg = 0; // black in any channel layout - clear() relies on this
    // Precompute the blend LUT: foreground scaled by each 0-255 antialiasing intensity, composed in the
    // device channel layout. Background is black, so blend_lut[0] == bg and blend_lut[255] == fg.
    for i in 0..256u32 {
        let (r, g, b) = (FG_RGB.0 * i / 255, FG_RGB.1 * i / 255, FG_RGB.2 * i / 255);
        s.blend_lut[i as usize] = (r << rs) | (g << gs) | (b << bs);
    }
    s.esc = 0;
    s.csi_nparam = 0;
    s.reverse = false;
    s.cursor_visible = true;
    s.ready = true;
    clear(&mut s);
    commit(&mut s);
    // Report the panel geometry + the chosen font scale. Drop the lock first: `kprintln` renders to this
    // same console, so logging while holding it would re-enter the lock.
    let (w, h, bpp, cols, rows) = (s.width, s.height, s.bpp, s.cols, s.rows);
    drop(s);
    crate::kprintln!(
        "fb: {}x{} {}bpp, font-scale {}x -> {} cols x {} rows",
        w,
        h,
        bpp * 8,
        sc,
        cols,
        rows
    );
}

/// True once `init` has run. An arch that mirrors serial output to the display checks this so it can
/// call `put_bytes` unconditionally and have it no-op before the console exists.
pub fn ready() -> bool {
    FB.lock().ready
}

/// Framebuffer text geometry packed as `(rows << 16) | cols`, or 0 if the console has not been
/// initialised. Exposed to userspace via `InspectKernel` query 9 so the console service can lay out its
/// terminal.
pub fn dims_packed() -> u32 {
    let s = FB.lock();
    if !s.ready {
        return 0;
    }
    (((s.rows as u32) & 0xFFFF) << 16) | ((s.cols as u32) & 0xFFFF)
}

/// Clear the framebuffer and move the cursor to the top-left. Used when the shell ends boot-log
/// mirroring (`console_boot_complete`) to hand over a clean console.
pub fn clear_and_home() {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    clear(&mut s);
    s.col = 0;
    s.row = 0;
    s.cur_col = 0;
    s.cur_row = 0;
    s.esc = 0;
    s.reverse = false;
    if s.cursor_visible {
        draw_cursor(&mut s);
    }
    commit(&mut s);
}

/// Write one output byte to the console.
pub fn put_byte(b: u8) {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    process_byte(&mut s, b);
    commit(&mut s);
}

/// Write a whole byte sequence under a SINGLE lock, then publish once. Used by the console path so a
/// multi-byte write (the shell's `gsh> ` prompt, say) is atomic with respect to another core's console
/// output - no byte from another core can interleave mid-string.
pub fn put_bytes(bytes: &[u8]) {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    for &b in bytes {
        process_byte(&mut s, b);
    }
    commit(&mut s);
}

// ---------------------------------------------------------------------------
// Publication
// ---------------------------------------------------------------------------

/// Publish everything drawn since the last commit and reset the dirty rectangle.
///
/// Called once per locked batch, so the arch primitive sees one bounding box rather than one call per
/// glyph. What it does is arch-defined: clean a cacheable region to the Point of Coherency (ARM), or
/// fence write-combining stores (x86). See the module header for the contract.
fn commit(s: &mut Fb) {
    let base = match s.mem.as_deref() {
        Some(m) => m.as_ptr() as usize,
        None => return,
    };
    if s.dirty_w == 0 || s.dirty_h == 0 {
        // Nothing was drawn. Still fence: an arch may have pending stores from an earlier batch that a
        // caller is entitled to see ordered here, and a fence with nothing to order is free.
        crate::arch::imp::fb_commit(base, s.pitch, s.bpp, 0, 0, 0, 0);
        return;
    }
    crate::arch::imp::fb_commit(
        base, s.pitch, s.bpp, s.dirty_x, s.dirty_y, s.dirty_w, s.dirty_h,
    );
    s.dirty_x = 0;
    s.dirty_y = 0;
    s.dirty_w = 0;
    s.dirty_h = 0;
}

// ---------------------------------------------------------------------------
// Shadow grid
// ---------------------------------------------------------------------------

/// Record a cell's printable character and reverse-video attribute in the shadow grid. Bounds-guarded;
/// cols/rows are clamped to the grid in `init`, so in practice every cell is in range.
#[inline]
fn grid_set(s: &mut Fb, c: usize, r: usize, ch: u8) {
    if r < MAX_ROWS && c < MAX_COLS {
        s.grid[r][c] = ch;
        let rev = s.reverse;
        attr_set(s, c, r, rev);
    }
}

#[inline]
fn attr_set(s: &mut Fb, c: usize, r: usize, on: bool) {
    if r >= MAX_ROWS || c >= MAX_COLS {
        return;
    }
    let mask = 1u8 << (c % 8);
    if on {
        s.attr[r][c / 8] |= mask;
    } else {
        s.attr[r][c / 8] &= !mask;
    }
}

#[inline]
fn attr_get(s: &Fb, c: usize, r: usize) -> bool {
    if r >= MAX_ROWS || c >= MAX_COLS {
        return false;
    }
    s.attr[r][c / 8] & (1u8 << (c % 8)) != 0
}

/// Repaint a cell from the shadow grid, honouring the attribute it was written with rather than the
/// terminal's current one. Without that, a repaint (a cursor lift, a scroll) over a reverse-video row
/// would silently drop the highlight.
fn redraw_cell(s: &mut Fb, c: usize, r: usize) {
    let ch = if r < MAX_ROWS && c < MAX_COLS {
        s.grid[r][c]
    } else {
        b' '
    };
    let saved = s.reverse;
    s.reverse = attr_get(s, c, r);
    render::draw_glyph(s, ch, c, r);
    s.reverse = saved;
}

/// Clear the whole screen and blank the shadow grid.
fn clear(s: &mut Fb) {
    render::clear_all(s);
    for r in 0..MAX_ROWS {
        for c in 0..ATTR_STRIDE {
            s.attr[r][c] = 0;
        }
    }
}

// ---------------------------------------------------------------------------
// Byte processing
// ---------------------------------------------------------------------------

/// Process one output byte against the (locked) console state.
fn process_byte(s: &mut Fb, b: u8) {
    // --- Escape-sequence state machine ---
    match s.esc {
        1 => {
            // Saw ESC; expect '[' to start a CSI sequence. Anything else: drop.
            if b == b'[' {
                s.esc = 2;
                s.csi_priv = false;
                s.csi_params = [0; 4];
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

    // --- UTF-8 decode (so the console renders the box-drawing UI, not garbled bytes) ---
    if s.utf8_remaining > 0 {
        // Mid-sequence: fold a continuation byte into the codepoint; render when complete.
        if b & 0xC0 == 0x80 {
            s.utf8_cp = (s.utf8_cp << 6) | (b & 0x3F) as u32;
            s.utf8_remaining -= 1;
            if s.utf8_remaining == 0 {
                let cell = render::cell_for_codepoint(s.utf8_cp);
                put_printable_cell(s, cell);
            }
            return;
        }
        s.utf8_remaining = 0; // malformed - abandon the sequence and reprocess this byte
    }
    if b >= 0x80 {
        // Lead byte: begin a 2/3/4-byte sequence; a stray continuation or invalid lead is a `?`.
        if b & 0xE0 == 0xC0 {
            s.utf8_cp = (b & 0x1F) as u32;
            s.utf8_remaining = 1;
            return;
        }
        if b & 0xF0 == 0xE0 {
            s.utf8_cp = (b & 0x0F) as u32;
            s.utf8_remaining = 2;
            return;
        }
        if b & 0xF8 == 0xF0 {
            s.utf8_cp = (b & 0x07) as u32;
            s.utf8_remaining = 3;
            return;
        }
        put_printable_cell(s, b'?');
        return;
    }

    // --- Control / printable ASCII byte ---
    // A `\r` moves to column 0 over already-drawn text (the prompt, say); stamping the cursor there and
    // erasing it next byte would blank that text, so `\r` does not redraw the cursor.
    match b {
        b'\n' => {
            cursor_off(s);
            advance_line(s);
            cursor_on(s);
        }
        b'\r' => {
            cursor_off(s);
            s.col = 0;
        }
        // Backspace is a NON-DESTRUCTIVE cursor-left move, exactly as on a terminal: a caller that wants
        // to erase sends the classic backspace-space-backspace itself.
        //
        // At column 0 it steps back to the END OF THE PREVIOUS ROW, because a line longer than the
        // screen has WRAPPED onto that row and is still one logical line. Stopping at column 0 made
        // editing break at exactly the screen width - the shell's line editor repositions with a RUN of
        // backspaces, and once that run hit the wrap point it could go no further, so every redraw after
        // it landed in the wrong place. That is the "typing boundary" bug, found on ARM and fixed here
        // for every arch.
        0x08 | 0x7f => {
            cursor_off(s);
            if s.col > 0 {
                s.col -= 1;
            } else if s.row > 0 {
                s.row -= 1;
                s.col = s.cols.saturating_sub(1);
            }
            cursor_on(s);
        }
        0x20..=0x7e => put_printable_cell(s, b),
        _ => {} // other control byte: ignore (serial keeps the full stream)
    }
}

/// Draw a printable cell byte at the write position and advance. Self-contained: erases the cursor
/// first and redraws it after, so it renders an ASCII byte and a UTF-8-decoded glyph (which may be a
/// box-drawing cell byte above 0x7e) the same way.
fn put_printable_cell(s: &mut Fb, cell: u8) {
    cursor_off(s);
    let (c, r) = (s.col, s.row);
    render::draw_glyph(s, cell, c, r);
    grid_set(s, c, r, cell);
    s.col += 1;
    if s.col >= s.cols {
        advance_line(s);
    }
    cursor_on(s);
}

// ---------------------------------------------------------------------------
// CSI (ESC[...) handling
// ---------------------------------------------------------------------------

/// Handle one byte inside a CSI sequence. Accumulates numeric parameters until a final byte
/// (0x40..=0x7e) dispatches the command.
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
        b'?' => s.csi_priv = true,
        0x20..=0x2f => {} // intermediate bytes: ignored
        0x40..=0x7e => {
            execute_csi(s, b);
            s.esc = 0;
        }
        _ => s.esc = 0, // malformed - abort the sequence
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
    cursor_off(s);
    // A CSI parameter defaults to 1 when omitted, except for ED/EL where it defaults to 0.
    let n1 = csi_param(s, 0, 1).max(1) as usize;
    match final_byte {
        // CUP / HVP - absolute cursor position: ESC[r;cH or ESC[r;cf (1-based; default 1,1 = home).
        // A full-screen app paints by seeking to a row/column and writing, so without this its output
        // simply streams at the current position and scrolls the shell instead of overlaying it.
        b'H' | b'f' => {
            let r = csi_param(s, 0, 1).max(1) as usize - 1;
            let c = csi_param(s, 1, 1).max(1) as usize - 1;
            s.row = r.min(s.rows.saturating_sub(1));
            s.col = c.min(s.cols.saturating_sub(1));
        }
        // Relative cursor movement: CUU / CUD / CUF / CUB.
        b'A' => s.row = s.row.saturating_sub(n1),
        b'B' => s.row = (s.row + n1).min(s.rows.saturating_sub(1)),
        b'C' => s.col = (s.col + n1).min(s.cols.saturating_sub(1)),
        b'D' => s.col = s.col.saturating_sub(n1),
        // ED - erase in display: 2 = whole screen + home; 0 (default) = cursor to end. The second form
        // is what a full-screen repaint uses to wipe the tail of a shorter previous frame.
        b'J' => match csi_param(s, 0, 0) {
            2 => {
                clear(s);
                s.row = 0;
                s.col = 0;
            }
            _ => erase_to_end_of_screen(s),
        },
        // EL - erase in line: 2 = whole line; 0 (default) = cursor to end of line, which the line editor
        // emits after each echoed character to wipe any leftover tail.
        b'K' => match csi_param(s, 0, 0) {
            2 => erase_line_full(s),
            _ => erase_line_to_eol(s),
        },
        // SGR - only the two the shell and full-screen apps actually emit: reverse video for a
        // highlighted row, and reset. A bare `ESC[m` parses as parameter 0, so it resets.
        b'm' => match csi_param(s, 0, 0) {
            7 => s.reverse = true,
            0 => s.reverse = false,
            _ => {} // colours and the rest: ignored
        },
        // Private mode set/reset: ESC[?25h shows the cursor, ESC[?25l hides it. Full-screen apps hide it
        // for the session so their bulk redraws do not smear an underline across the screen.
        b'h' if s.csi_priv && csi_param(s, 0, 0) == 25 => s.cursor_visible = true,
        b'l' if s.csi_priv && csi_param(s, 0, 0) == 25 => s.cursor_visible = false,
        _ => {} // unsupported command - ignore
    }
    cursor_on(s);
}

// ---------------------------------------------------------------------------
// Erase operations
// ---------------------------------------------------------------------------

/// Blank cells from the cursor column to the end of the current row.
fn erase_line_to_eol(s: &mut Fb) {
    let (row, col, cols) = (s.row, s.col, s.cols);
    for c in col..cols {
        render::draw_glyph(s, b' ', c, row);
        grid_set(s, c, row, b' ');
    }
}

/// Blank every cell on the current row.
fn erase_line_full(s: &mut Fb) {
    let (row, cols) = (s.row, s.cols);
    for c in 0..cols {
        render::draw_glyph(s, b' ', c, row);
        grid_set(s, c, row, b' ');
    }
}

/// Blank from the cursor to the end of the screen (rest of this row, then every row below it).
fn erase_to_end_of_screen(s: &mut Fb) {
    erase_line_to_eol(s);
    let (rows, cols, start) = (s.rows, s.cols, s.row + 1);
    for r in start..rows {
        for c in 0..cols {
            render::draw_glyph(s, b' ', c, r);
            grid_set(s, c, r, b' ');
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

/// Erase the underline cursor if it is visible (so a move or draw leaves no trail).
#[inline]
fn cursor_off(s: &mut Fb) {
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

/// Draw the text cursor as a true underline at the current write position: paint the cell's real glyph
/// first, then overlay a thin underline beneath it - so a character the cursor sits on stays visible
/// (underlined) instead of being hidden. Remember where it landed so `erase_cursor` can restore exactly
/// that cell later, even after the write position has moved (a carriage return, say).
fn draw_cursor(s: &mut Fb) {
    let (c, r) = (s.col, s.row);
    redraw_cell(s, c, r);
    let sc = render::cell_scale(s);
    let (cellw, cellh) = (CELL_W * sc, CELL_H * sc);
    let x0 = s.org_x + c * cellw;
    let y0 = s.org_y + r * cellh;
    let th = (CURSOR_TH * sc).min(cellh);
    // Contrast against the cell the cursor sits on, not against the terminal's current attribute: on a
    // reverse-video cell the background IS the foreground colour, so a foreground underline there would
    // be invisible. Read the cell's own stored attribute (the terminal may have reset SGR since).
    let colour = if attr_get(s, c, r) { s.bg } else { s.fg };
    render::fill_rect(s, x0, y0 + cellh - th, cellw, th, colour);
    s.cur_col = c;
    s.cur_row = r;
}

/// Erase the cursor at the cell where it was last drawn by restoring that cell's real content from the
/// shadow grid - NOT by blanking it. The cursor underline is drawn over whatever glyph occupies the cell
/// (the grid is not touched), so restoring the grid glyph removes the underline without destroying text.
/// Blanking instead would erase any character the cursor sits on, which is exactly what made moving the
/// cursor back over typed text (Left arrow, Home) delete it. Using the *remembered* position, not the
/// current write position, keeps a carriage return from touching real text elsewhere.
fn erase_cursor(s: &mut Fb) {
    let (c, r) = (s.cur_col, s.cur_row);
    redraw_cell(s, c, r);
}

// ---------------------------------------------------------------------------
// Line advance and scrolling
// ---------------------------------------------------------------------------

/// Move the cursor to the start of the next row, scrolling if at the bottom.
fn advance_line(s: &mut Fb) {
    s.col = 0;
    if s.row + 1 >= s.rows {
        scroll(s); // bottom row freed and cleared; cursor stays on the last row
    } else {
        s.row += 1;
    }
}

/// Scroll the display up by one text row.
///
/// Two strategies, chosen by `arch::imp::FB_READBACK_CHEAP`:
///
/// - **Read-back is expensive** (x86: the framebuffer is write-combining, so reads run at tens of MB/s
///   while writes are about 100x faster). Shifting the pixels in place would *read the framebuffer
///   back*; an 8 MB read-back cost about 130 ms per scrolled line on the T630 and dominated every
///   kill/respawn-heavy workload. So shift the shadow grid in normal RAM and repaint the text area from
///   it - write-only to the framebuffer.
/// - **Read-back is cheap** (ARM: the framebuffer is mapped cacheable, because the GPU scans RAM and
///   the console cleans to the Point of Coherency anyway). A strided memmove of the text area beats
///   repainting every cell on a slower core, so take that path.
///
/// The shadow grid is shifted either way - it is what `erase_cursor` and a later repaint read.
fn scroll(s: &mut Fb) {
    let (rows, cols) = (s.rows, s.cols);
    if rows == 0 {
        return;
    }
    // Shift the shadow up one row in RAM; blank the freed bottom row.
    for r in 0..rows - 1 {
        for c in 0..cols {
            s.grid[r][c] = s.grid[r + 1][c];
            let a = attr_get(s, c, r + 1);
            attr_set(s, c, r, a);
        }
    }
    for c in 0..cols {
        s.grid[rows - 1][c] = b' ';
        attr_set(s, c, rows - 1, false);
    }

    if crate::arch::imp::FB_READBACK_CHEAP {
        scroll_by_copy(s);
        return;
    }
    // Repaint every cell from the shadow - no framebuffer read-back.
    for r in 0..rows {
        for c in 0..cols {
            redraw_cell(s, c, r);
        }
    }
}

/// Scroll by moving pixels: shift the text area up one cell row, then clear the freed bottom band.
/// Only taken where reading the framebuffer is as cheap as writing it (see [`scroll`]). The margins
/// outside the text area are untouched, so the copy is strided row by row.
fn scroll_by_copy(s: &mut Fb) {
    let sc = render::cell_scale(s);
    let cellh = CELL_H * sc;
    let text_w = s.cols * CELL_W * sc;
    let text_h = s.rows * cellh;
    let (org_x, org_y, pitch, bpp) = (s.org_x, s.org_y, s.pitch, s.bpp);
    let row_bytes = text_w * bpp;
    // `copy_within` is the safe memmove: it bounds-checks the source range and destination and handles
    // the overlap. Row by row, because the text area is strided - the margins outside it stay put.
    if let Some(mem) = s.mem.as_deref_mut() {
        for gy in 0..(text_h - cellh) {
            let src = (org_y + gy + cellh) * pitch + org_x * bpp;
            let dst = (org_y + gy) * pitch + org_x * bpp;
            if src + row_bytes <= mem.len() && dst + row_bytes <= mem.len() {
                mem.copy_within(src..src + row_bytes, dst);
            }
        }
    }
    // Clear the freed bottom text row.
    let bg = s.bg;
    render::fill_rect(s, org_x, org_y + text_h - cellh, text_w, cellh, bg);
    render::dirty(s, org_x, org_y, text_w, text_h);
}
