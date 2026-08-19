// SPDX-License-Identifier: GPL-2.0-only
//! The terminal: an ANSI state machine over a character grid.
//!
//! This is the whole terminal model - the escape parser, the UTF-8 decoder, the shadow grid, the cursor,
//! scrolling, erase operations, reverse video - driving a linear framebuffer the kernel granted this
//! service at spawn. **It used to be `kernel/src/fbcon`** and moved out wholesale (`docs/console-service.md`
//! §9): rendering a shell prompt is policy, and policy belongs in a service (§26.10, §4.4).
//!
//! What stayed in the kernel is a separate, cruder boot/panic blit (`kernel/src/bootcon`) that draws
//! plain text and discards escapes. It is not a small terminal - it is the §11.4 log floor for a machine
//! whose only output is a display, and it exists because **a panic cannot ask a service to report it**.
//!
//! ## Ownership
//!
//! The kernel stops writing the framebuffer the moment it GRANTS it to this service, at spawn - there
//! is no call back the other way (an earlier draft of this comment named a `release_kernel_console()`
//! that was never built). Two writers to one framebuffer would leave the shadow grid below silently
//! wrong about what is on screen, so ownership is a state rather than a convention. The kernel takes it
//! back if this service dies, and unconditionally on a panic.
//!
//! ## Framebuffer semantics
//!
//! The mapping is **Normal non-cacheable**, because the kernel maps the same physical pages and ARM
//! leaves mismatched memory attributes UNPREDICTABLE - and a service cannot do cache maintenance at all
//! (`unsafe` is forbidden here, §18.2). Two consequences shape the code:
//!
//! - There is nothing to clean and no rectangle to publish, so the old `fb_commit` / dirty-rectangle
//!   machinery is gone. `render` orders its stores once per batch.
//! - **Reading the framebuffer back is never cheap**, so [`scroll`] always repaints from the shadow grid.
//!   The old `FB_READBACK_CHEAP` arch switch and its `scroll_by_copy` partner are deleted.
//!
//! ## State is owned, not global
//!
//! [`Term`] is a plain owned value held by `service_main`. The kernel version was a `SpinLock<Fb>`
//! static because several cores could log at once; a service is one task, so the lock is not merely
//! unnecessary, it would be an unowned global (invariant 9).

use godspeed_sdk::Framebuffer;

use crate::render;
use crate::render::{CELL_H, CELL_W};

/// Char-grid shadow bounds. **Sized from the font-scale rule, not from a raw pixel count.**
///
/// The old bounds (448 x 128) assumed 4K edge-to-edge at a 1x cell. That case cannot occur:
/// `render::cell_scale` upscales at 1200 px and above, so a panel wide enough for 427 columns is always
/// rendering at 2x or 3x and gets far fewer. Taking the scale into account, the widest real grid is at
/// **1920x1080** - just under the 1200 px step, so still 1x - which after the 5% safe-area inset gives
/// 1728/9 = 192 columns and 972/20 = 48 rows. Every denser panel scales up and lands lower (3840x2160 at
/// 3x is 128 x 32; 2560x1440 at 2x is 128 x 32), and every smaller one is smaller.
///
/// This matters here in a way it did not in the kernel: the grid and its attribute plane are now a
/// **stack-resident** part of the terminal state, and the old bounds were 64 KiB of a 256 KiB service
/// stack (§26.6.1 - the bound must be one you can read off the source and afford). 208 x 64 keeps a
/// margin over the worst real case at about 15 KiB. A larger display than this clamps its text area, as
/// it always did; nothing overruns.
pub(crate) const MAX_COLS: usize = 208;
pub(crate) const MAX_ROWS: usize = 64;

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

/// The terminal. One owned value, held by `service_main` for the life of the service.
///
/// Named `Term` at the type level and still `Fb` inside this module's helpers, because every function
/// below took `&mut Fb` in the kernel and renaming them all would obscure a move that is otherwise
/// line-for-line the code that ran in ring 0.
pub(crate) struct Fb {
    // The kernel's framebuffer grant, or None before `init`. An OWNED handle rather than a
    // borrowed-forever slice field: that spelling is an unowned mutable global in everything but name,
    // and this way there is exactly one route to the pixels and it is a `&mut self` borrow. The
    // single `unsafe` that produces the slice lives in the SDK's audited MMIO layer (§18.1).
    /// A scroll has shifted the shadow grid and the screen has NOT been repainted for it yet.
    ///
    /// Scrolling repaints the whole screen from the shadow - about 8,000 glyph cells, several megabytes
    /// of writes - and the terminal used to do that once per message. Under a flood that is the entire
    /// cost: sixteen queued lines meant sixteen full repaints where one would have shown the same
    /// result, and the service sat at 100% CPU with its queue jammed at 16/16 while every other service
    /// idled. Interactive echo was stuck behind that queue, so typing produced no cursor and no
    /// feedback, which reads as a dead keyboard and is not one.
    ///
    /// Deferring the paint makes a batch of lines cost ONE repaint. Nothing is dropped and nothing is
    /// approximated - the shadow is updated for every byte exactly as before, and the screen is painted
    /// from it once the batch is in. The flag is what remembers that the two have diverged.
    pub(crate) repaint_pending: bool,
    pub(crate) mem: Option<Framebuffer>,
    pub(crate) pitch: usize,  // bytes per scanline
    pub(crate) bpp: usize,    // bytes per pixel
    pub(crate) width: usize,  // visible width in pixels
    pub(crate) height: usize, // visible height in pixels
    pub(crate) org_x: usize,  // left edge of the text area (safe-area inset for TV overscan)
    pub(crate) org_y: usize,  // top edge of the text area
    pub(crate) cols: usize,   // text columns within the safe area
    pub(crate) rows: usize,   // text rows within the safe area
    pub(crate) col: usize,    // cursor column
    pub(crate) row: usize,    // cursor row
    pub(crate) fg: u32,       // foreground pixel value (already in the device's channel layout)
    pub(crate) bg: u32,       // background pixel value (always black - see FbParams)

    // --- ANSI escape parser ---
    // The shell and the full-screen apps drive the terminal with a small ANSI subset (clear, cursor
    // position and movement, erase line, reverse video, hide/show cursor). The same escapes work on a
    // serial terminal for free. State persists across put_byte calls because a sequence spans bytes.
    pub(crate) esc: u8,              // 0 = normal, 1 = saw ESC, 2 = inside CSI (after '[')
    pub(crate) csi_priv: bool,       // saw '?' immediately after '[' (private-mode sequence)
    pub(crate) csi_params: [u16; 4], // numeric parameters (e.g. row;col)
    pub(crate) csi_nparam: usize,    // count of parameters accumulated
    pub(crate) reverse: bool,        // SGR reverse video (ESC[7m), reset by ESC[0m

    // --- UTF-8 decode ---
    // Accumulate a multi-byte sequence into a codepoint so the box-drawing UI renders as frames rather
    // than mojibake. `utf8_remaining` is how many continuation bytes are still expected (0 = idle).
    pub(crate) utf8_cp: u32,
    pub(crate) utf8_remaining: u8,

    // --- Cursor ---
    pub(crate) cursor_visible: bool, // draw the underline cursor (off for full-screen apps)
    pub(crate) cur_col: usize,       // column where the cursor underline was last drawn
    pub(crate) cur_row: usize,       // row where the cursor underline was last drawn

    // --- Shadow grid ---
    // The printable content of each text cell (the transient cursor overlay is excluded - it is always
    // erased before a scroll), plus one reverse-video bit per cell. `scroll` shifts these in RAM and
    // repaints from them, so it never reads the framebuffer back; `erase_cursor` restores the real glyph
    // under the cursor instead of blanking it. Storing the attribute alongside the character is what
    // keeps a reverse-video row correct after a repaint - without it a redraw would silently lose the
    // highlight. One bit per cell, so the whole attribute plane is ATTR_STRIDE * MAX_ROWS bytes.
    pub(crate) grid: [[u8; MAX_COLS]; MAX_ROWS],
    pub(crate) attr: [[u8; ATTR_STRIDE]; MAX_ROWS],

    // Precomputed foreground-blend LUT: blend_lut[intensity] = the glyph-pixel colour for that
    // antialiasing intensity, composed in the device layout. Lets an antialiased glyph edge blit as a
    // table read instead of a per-pixel multiply/divide.
    pub(crate) blend_lut: [u32; 256],
}

/// The terminal, as the rest of the service sees it.
pub struct Term {
    s: Fb,
}

impl Term {
    /// Build the terminal over the granted framebuffer and clear it.
    ///
    /// Constructed **in place** through `&mut self` rather than returned by value: `Fb` carries the
    /// shadow grid and its attribute plane, about 15 KiB, and returning it by value would put a second
    /// copy on a 256 KiB service stack during the move (§26.6.1 - the same by-value trap that cost five
    /// `fs` stack overflows). `service_main` therefore zero-initialises one and hands out a reference.
    pub fn new(fb: Framebuffer) -> Self {
        let mut t = Term {
            s: Fb {
                repaint_pending: false,
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
                blend_lut: [0; 256],
            },
        };
        init(&mut t.s, fb);
        t
    }

    /// Terminal geometry as `(rows, cols)`. **The single source of truth** - the kernel no longer
    /// answers this (`InspectKernel` query 9 is deleted), because the safe-area inset, the cell size and
    /// the font-scale rule that produce it all live here.
    pub fn dims(&self) -> (u16, u16) {
        (self.s.rows as u16, self.s.cols as u16)
    }

    /// Write a byte sequence to the terminal.
    pub fn put_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            process_byte(&mut self.s, b);
        }
    }

    /// Show everything written since the last flush.
    ///
    /// Separate from `put_bytes` so a caller that has several messages in hand can apply them all and
    /// pay for ONE repaint. Calling it once per message is still correct - it is just the expensive way,
    /// and it is what jammed the console under load.
    pub fn flush(&mut self) {
        if self.s.repaint_pending {
            // Cleared FIRST: the repaint draws cells, and the cell painter skips drawing while a
            // repaint is outstanding (there is no point painting what is about to be overpainted).
            self.s.repaint_pending = false;
            repaint_all(&mut self.s);
        }
        render::present();
    }

}

/// Initialise the terminal from the kernel's framebuffer grant and clear the screen.
fn init(s: &mut Fb, fb: Framebuffer) {
    // Compose pixel values in the framebuffer's own channel layout via the reported mask shifts, so we
    // render correct colours on an RGB or a BGR device.
    let (rs, gs, bs) = (fb.r_shift, fb.g_shift, fb.b_shift);

    s.pitch = fb.pitch;
    s.bpp = fb.bpp;
    s.width = fb.width;
    s.height = fb.height;
    s.mem = Some(fb);
    // Inset the text area by SAFE_PCT on each edge.
    s.org_x = s.width * SAFE_PCT / 100;
    s.org_y = s.height * SAFE_PCT / 100;
    let sc = render::cell_scale(s);
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
    clear(s);
    render::present();
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
    // The shadow is always updated; the PIXELS are skipped when a full repaint is already owed, because
    // that repaint paints this cell from the shadow moments later. This is what makes a batch cheap:
    // the lines that scroll off cost nothing to draw twice.
    if !s.repaint_pending {
        render::draw_glyph(s, cell, c, r);
    }
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

/// Blank a rectangular block of cells as ONE solid fill, and mark them blank in the shadow grid.
///
/// Erasing used to draw a space *glyph* per cell - a font raster lookup and a full cell of blended
/// pixel writes, per blank cell. That is affordable on a fast CPU with an 80x24 grid and is not on a
/// 900 MHz Cortex-A7 with a 182x44 one: `ESC[K` fires once per row, so a full-screen repaint became
/// thousands of glyph blits and `edit` visibly crawled on the Pi's TV. A blank cell is by definition
/// just the background colour (the foreground, under reverse video), so the whole block is one
/// `fill_rect` writing long contiguous runs - same pixels, a fraction of the work.
fn blank_block(s: &mut Fb, col: usize, row: usize, cols: usize, rows: usize) {
    if cols == 0 || rows == 0 {
        return;
    }
    let sc = render::cell_scale(s);
    let (cw, ch) = (CELL_W * sc, CELL_H * sc);
    let x0 = s.org_x + col * cw;
    let y0 = s.org_y + row * ch;
    let colour = render::paper(s);
    render::fill_rect(s, x0, y0, cols * cw, rows * ch, colour);
    for r in row..(row + rows).min(s.rows) {
        for c in col..(col + cols).min(s.cols) {
            grid_set(s, c, r, b' ');
        }
    }
}

/// Blank cells from the cursor column to the end of the current row.
fn erase_line_to_eol(s: &mut Fb) {
    let (row, col, cols) = (s.row, s.col, s.cols);
    blank_block(s, col, row, cols.saturating_sub(col), 1);
}

/// Blank every cell on the current row.
fn erase_line_full(s: &mut Fb) {
    let (row, cols) = (s.row, s.cols);
    blank_block(s, 0, row, cols, 1);
}

/// Blank from the cursor to the end of the screen (rest of this row, then every row below it).
fn erase_to_end_of_screen(s: &mut Fb) {
    erase_line_to_eol(s);
    let (rows, cols, start) = (s.rows, s.cols, s.row + 1);
    blank_block(s, 0, start, cols, rows.saturating_sub(start));
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

/// Scroll the display up by one text row, by shifting the shadow grid in normal RAM and repainting the
/// text area from it - **write-only to the framebuffer**.
///
/// There used to be a second strategy (`scroll_by_copy`, a strided memmove of the pixels) selected by
/// `arch::imp::FB_READBACK_CHEAP`, taken on ARM because the kernel mapped the framebuffer cacheable. That
/// choice is gone with the mapping: this service maps the framebuffer **non-cacheable** (it cannot do
/// cache maintenance, §18.2, and the kernel maps the same physical pages - mismatched attributes are
/// UNPREDICTABLE on ARM). Reading a non-cacheable framebuffer back is never cheap on any arch, so the
/// repaint below is now the only correct strategy rather than the x86 one of two.
///
/// The x86 measurement that motivated it still stands as the reason: an 8 MB read-back cost about 130 ms
/// per scrolled line on the T630 and dominated every kill/respawn-heavy workload.
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

    // Repaint from the shadow - no framebuffer read-back - but only as far as each row HAS content,
    // and blank the rest of that row in one fill.
    //
    // The naive version repainted every cell: 182 x 44 = 8,008 glyph draws, each writing a 9x20 cell,
    // which is 5.7 MB of non-cacheable stores for one scrolled line. Console lines are mostly trailing
    // blanks - a prompt or a selfcheck line uses perhaps 40 of 182 columns - and a run of blanks is by
    // definition one flat rectangle, so painting them one space-glyph at a time is the same pixels at a
    // fraction of the throughput. That difference is what "the scroll was a bit slow" is made of.
    //
    // The tail is only bulk-filled where it is genuinely uniform: a blank cell under REVERSE VIDEO is
    // painted in the foreground colour, so a row whose tail is highlighted is not one rectangle. Those
    // cells fall back to the per-cell path rather than being flattened into the wrong colour, which is
    // the sort of shortcut that would show up as a highlight silently vanishing after a scroll.
    // Deferred, not skipped: `flush` paints this before anything is shown. See `Fb::repaint_pending`.
    s.repaint_pending = true;
    let _ = cols;
}

/// Paint the whole screen from the shadow grid. The deferred half of `scroll`.
fn repaint_all(s: &mut Fb) {
    let (rows, cols) = (s.rows, s.cols);
    for r in 0..rows {
        let painted = paint_row_content(s, r, cols);
        blank_row_tail(s, r, painted, cols);
    }
}

/// Repaint row `r` up to the end of its content, and return the column where the uniform blank tail
/// begins. A cell counts as tail only if it is a space AND carries no attribute.
fn paint_row_content(s: &mut Fb, r: usize, cols: usize) -> usize {
    let mut tail = cols;
    while tail > 0 {
        let c = tail - 1;
        let blank = (if r < MAX_ROWS && c < MAX_COLS { s.grid[r][c] } else { b' ' }) == b' ';
        if !blank || attr_get(s, c, r) {
            break;
        }
        tail -= 1;
    }
    for c in 0..tail {
        redraw_cell(s, c, r);
    }
    tail
}

/// Blank columns `from..cols` of row `r` as a single rectangle. The shadow grid already says these are
/// blank (the caller established it), so this paints pixels only and does not touch the grid.
fn blank_row_tail(s: &mut Fb, r: usize, from: usize, cols: usize) {
    if from >= cols {
        return;
    }
    let sc = render::cell_scale(s);
    let (cw, ch) = (CELL_W * sc, CELL_H * sc);
    let x0 = s.org_x + from * cw;
    let y0 = s.org_y + r * ch;
    let bg = s.bg;
    render::fill_rect(s, x0, y0, (cols - from) * cw, ch, bg);
}
