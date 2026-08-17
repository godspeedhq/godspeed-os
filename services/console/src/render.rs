// SPDX-License-Identifier: GPL-2.0-only
//! Pixel and glyph rendering for the framebuffer console.
//!
//! Everything here writes into a **linear framebuffer** the architecture hands over as a plain
//! `&'static mut [u8]` (`FbParams::mem`), described by `(pitch, bpp, channel shifts)` - which is
//! arch-neutral data, so this file is too. The architecture does not participate in the drawing; it
//! supplies the slice; ordering the writes is [`present`], once per batch.
//!
//! **No `unsafe` lives here, and none may.** §18.2 forbids `unsafe` in a service outright, so the single
//! `unsafe` needed to turn the kernel's granted framebuffer VA into a slice sits in the SDK's audited
//! MMIO layer (§18.1), which is the only place that knows the kernel mapped it and for how long.
//! Everything below is bounds-checked slice work: a wrong offset paints nothing rather than corrupting
//! memory, and the checks are hoisted to one per row so the hot paths keep writing contiguous runs.
//!
//! Two glyph sources:
//! - **Antialiased Noto Sans Mono** (`noto-sans-mono-bitmap`, Regular at a 20 px raster) for ASCII.
//!   Each glyph pixel is a 0-255 intensity blended toward the background, so text is smooth rather
//!   than the blocky look of a 1-bpp bitmap font.
//! - **Procedural strokes** for the light box-drawing block (`tree`'s `├──`, table frames). The
//!   font's Basic-Latin range carries no U+2500 block, and strokes drawn out from the cell centre
//!   connect cell-to-cell exactly at any cell size, which a font glyph cannot guarantee.

use crate::term::Fb;
use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

/// Noto weight + raster height for the console. `get_raster_width`/`RasterHeight::val` are `const fn`,
/// so the per-cell pixel box (`CELL_W` x `CELL_H`) is known at compile time and the char-grid geometry
/// is computed from it. Size20 is about 9x20 px - smooth on a TV at 1:1, and upscaled by `cell_scale`
/// on a denser panel.
pub(crate) const FONT_WEIGHT: FontWeight = FontWeight::Regular;
pub(crate) const RASTER_HEIGHT: RasterHeight = RasterHeight::Size20;
pub(crate) const CELL_W: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
pub(crate) const CELL_H: usize = RASTER_HEIGHT.val();

/// First reserved cell byte for the **box-drawing** glyphs. The grid stores these high bytes;
/// `draw_box_glyph` renders them with procedural strokes.
pub(crate) const BOX_FIRST: u8 = 0xB3;

/// Integer font-scale factor for the current framebuffer. The glyph raster is a fixed CELL_W x CELL_H
/// pixels; on a genuinely dense panel (the Dell Wyse 5070's native 3840x2160) that renders as a wall of
/// tiny text, so each glyph pixel is upscaled to a `scale x scale` block.
///
/// **Upscaling is a last resort, not a mid-range default, because it coarsens the font.** The Noto
/// raster is antialiased - each pixel is a 0-255 intensity - and replicating a pixel into an `sc x sc`
/// block magnifies that gradient into visible chunks. At 1x the text is smooth; at 2x it reads as
/// grainy. So scale only where 1x would genuinely be unreadable.
///
/// A plain `height / 600` puts the first step at 1200 px, which is the honest boundary: below it a
/// 20 px cell is perfectly legible and should stay pixel-exact. The previous `(height + 300) / 600`
/// biased the step down to 900 px, which caught a 1824x984 TV (the Pi 2's HDMI mode) and made it 2x -
/// bigger and grainier than the same panel had always rendered. Both verified x86 machines are
/// unaffected either way: the T630 (768) stays 1x and the Wyse (2160) stays 3x.
#[inline]
pub(crate) fn cell_scale(s: &Fb) -> usize {
    (s.height / 600).clamp(1, 3)
}

/// Map a decoded Unicode codepoint to the internal **cell byte** the grid stores. ASCII passes through
/// (rendered via Noto); the light box-drawing block (`U+2500..U+253C`) maps to the reserved high bytes
/// (rendered procedurally); anything else becomes `?` - visible, never silently dropped (§3.12).
pub(crate) fn cell_for_codepoint(cp: u32) -> u8 {
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

/// Which of the four arms a box-drawing cell byte has: `(up, down, left, right)`. The procedural
/// renderer draws a stroke for each present arm out from the cell centre, so neighbouring cells join
/// seamlessly.
pub(crate) fn box_arms(ch: u8) -> (bool, bool, bool, bool) {
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

/// Order the framebuffer stores written since the last call, once per batch.
///
/// The mapping is Normal non-cacheable (see `term`), so there is nothing to clean to the Point of
/// Coherency and no rectangle to publish - the old `fb_commit` took both. What remains is ordering: a
/// non-cacheable store may sit in a write buffer, and the display scans RAM. A `SeqCst` fence orders our
/// own stores, and the syscall this service always makes next (it returns to a blocking drain) carries
/// the write buffer out to memory, so a finished frame is never left pending.
#[inline]
pub(crate) fn present() {
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// The glyph-pixel colour for an antialiasing `intensity` (0-255): 0 gives the (black) background, 255
/// gives full foreground, and in between gives the smooth edges. Reads straight from `blend_lut`, which
/// `term::init` precomputes once (foreground scaled per channel, composed in the device's layout), so
/// an antialiased edge costs one table read rather than three multiplies and three divides per pixel.
///
/// Under SGR reverse video (`ESC[7m`) the ramp is inverted, so the cell fills with foreground and the
/// glyph is punched out of it - which is what the full-screen apps use to mark a selected row.
#[inline]
pub(crate) fn blend(s: &Fb, intensity: u8) -> u32 {
    let i = if s.reverse { 255 - intensity } else { intensity };
    s.blend_lut[i as usize]
}

/// Solid stroke colour (box glyphs, the cursor underline), honouring reverse video.
#[inline]
pub(crate) fn ink(s: &Fb) -> u32 {
    if s.reverse {
        s.bg
    } else {
        s.fg
    }
}

/// Solid cell-background colour, honouring reverse video.
#[inline]
pub(crate) fn paper(s: &Fb) -> u32 {
    if s.reverse {
        s.fg
    } else {
        s.bg
    }
}

/// Paint `count` consecutive pixels one colour, starting at byte offset `off`.
///
/// The bounds check happens once, when the row is sliced; the write loop then runs unchecked over a
/// contiguous run, which is what lets write-combining coalesce the stores into bursts. An out-of-range
/// run paints nothing - it cannot reach past the framebuffer.
fn put_run(mem: &mut [u8], off: usize, count: usize, bpp: usize, color: u32) {
    let Some(run) = off
        .checked_add(count * bpp)
        .and_then(|end| mem.get_mut(off..end))
    else {
        return;
    };
    if bpp == 4 {
        let b = color.to_ne_bytes();
        for px in run.chunks_exact_mut(4) {
            px.copy_from_slice(&b);
        }
    } else {
        for px in run.chunks_exact_mut(bpp) {
            for (i, c) in px.iter_mut().enumerate() {
                *c = (color >> (i * 8)) as u8;
            }
        }
    }
}

/// Write a single pixel at (x, y) in the device's pixel layout.
#[inline]
pub(crate) fn put_pixel(s: &mut Fb, x: usize, y: usize, color: u32) {
    if x >= s.width || y >= s.height {
        return;
    }
    let (off, bpp) = (y * s.pitch + x * s.bpp, s.bpp);
    let Some(mem) = s.mem.as_deref_mut() else { return };
    put_run(mem, off, 1, bpp, color);
}

/// Fill a solid `w x h` pixel rectangle at (x, y), one contiguous run per row.
pub(crate) fn fill_rect(s: &mut Fb, x: usize, y: usize, w: usize, h: usize, color: u32) {
    if x >= s.width || y >= s.height {
        return;
    }
    let xw = (x + w).min(s.width);
    let yh = (y + h).min(s.height);
    let (pitch, bpp) = (s.pitch, s.bpp);
    let count = xw - x;
    if let Some(mem) = s.mem.as_deref_mut() {
        for yy in y..yh {
            put_run(mem, yy * pitch + x * bpp, count, bpp, color);
        }
    }
}

/// Clear the whole framebuffer to the background colour. `bg` is black (all channels zero, so all bytes
/// zero), which makes a flat byte-zero fill correct regardless of the device's channel layout.
pub(crate) fn clear_all(s: &mut Fb) {
    if let Some(mem) = s.mem.as_deref_mut() {
        mem.fill(0);
    }
    for r in 0..crate::term::MAX_ROWS {
        for c in 0..crate::term::MAX_COLS {
            s.grid[r][c] = b' ';
        }
    }
}

/// Paint one output row of a glyph: each raster intensity blended once and replicated `sc` times
/// horizontally, written as a single contiguous run. 32bpp only - `draw_glyph` routes other depths to
/// the `fill_rect` path.
fn put_glyph_row(s: &mut Fb, off: usize, rowpix: &[u8], sc: usize) {
    let count = rowpix.len() * sc;
    let (lut, rev) = (&s.blend_lut, s.reverse);
    let Some(mem) = s.mem.as_deref_mut() else { return };
    let Some(run) = off.checked_add(count * 4).and_then(|e| mem.get_mut(off..e)) else {
        return;
    };
    let mut px = run.chunks_exact_mut(4);
    for &intensity in rowpix {
        let i = if rev { 255 - intensity } else { intensity };
        let b = lut[i as usize].to_ne_bytes();
        for _ in 0..sc {
            match px.next() {
                Some(p) => p.copy_from_slice(&b),
                None => return,
            }
        }
    }
}

/// Render one glyph at text cell (col, row). ASCII renders via the antialiased Noto raster (every cell
/// pixel is written - intensity 0 paints the background, so the cell is fully repainted with no stale
/// pixels). The reserved high bytes render as procedural box-drawing strokes.
pub(crate) fn draw_glyph(s: &mut Fb, ch: u8, col: usize, row: usize) {
    let sc = cell_scale(s);
    let x0 = s.org_x + col * CELL_W * sc;
    let y0 = s.org_y + row * CELL_H * sc;
    if ch >= BOX_FIRST && box_arms(ch) != (false, false, false, false) {
        draw_box_glyph(s, ch, x0, y0);
        return;
    }
    // A blank cell is just the background: skip the raster lookup and the per-pixel blend and paint it
    // as one solid rectangle. Spaces dominate real output - padded status bars, indentation, and every
    // erase - so this is the difference between a full-screen repaint being a rectangle fill and being
    // thousands of glyph blits. It matters most where the CPU is slowest and the grid largest (a Pi 2
    // driving a 182x44 TV, where `edit` was visibly crawling before this).
    if ch == b' ' {
        clear_cell(s, x0, y0);
        return;
    }
    // Noto raster: `raster()` is `height` rows of `width` intensity bytes; the crate guarantees those
    // dims equal CELL_H x CELL_W for this weight/size, so iterating the raster covers the whole cell.
    // An unknown char falls back to a blank cell. Each raster pixel is upscaled to an `sc x sc` block
    // so the fixed-size glyph stays readable on a dense panel; sc == 1 is the original 1:1 path.
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
    // contiguous run path below stays on whole rows (fall back to the general blit otherwise).
    if s.bpp != 4 || x0 + cw > s.width || y0 + chh > s.height {
        for (gy, rowpix) in raster.iter().enumerate() {
            for (gx, &intensity) in rowpix.iter().enumerate() {
                let color = blend(s, intensity);
                fill_rect(s, x0 + gx * sc, y0 + gy * sc, sc, sc, color);
            }
        }
        return;
    }
    let pitch = s.pitch;
    for (gy, rowpix) in raster.iter().enumerate() {
        for sy in 0..sc {
            let off = (y0 + gy * sc + sy) * pitch + x0 * 4;
            put_glyph_row(s, off, rowpix, sc);
        }
    }
}

/// Paint a whole cell to the background colour (used when a char has no raster).
pub(crate) fn clear_cell(s: &mut Fb, x0: usize, y0: usize) {
    let sc = cell_scale(s);
    let color = paper(s);
    fill_rect(s, x0, y0, CELL_W * sc, CELL_H * sc, color);
}

/// Draw a procedural box-drawing glyph at pixel origin (x0, y0). Each present arm is a stroke from the
/// cell edge to the centre; arms overlap at the centre so adjacent cells connect into continuous lines.
pub(crate) fn draw_box_glyph(s: &mut Fb, ch: u8, x0: usize, y0: usize) {
    clear_cell(s, x0, y0);
    let sc = cell_scale(s);
    let (cellw, cellh) = (CELL_W * sc, CELL_H * sc);
    let (up, down, left, right) = box_arms(ch);
    let th = (cellw / 5).max(2); // stroke thickness in px
    let vx = cellw.saturating_sub(th) / 2; // left edge of the vertical stroke band
    let hy = cellh.saturating_sub(th) / 2; // top edge of the horizontal stroke band
    let colour = ink(s);
    // Vertical arms span the stroke columns [vx, vx+th); they reach to the centre band's far edge so
    // the cross is solid. Horizontal arms span the stroke rows [hy, hy+th). `clear_cell` above
    // already painted the whole cell and every stroke lies inside it.
    if up {
        stroke(s, x0, y0, vx, vx + th, 0, hy + th, cellw, cellh, colour);
    }
    if down {
        stroke(s, x0, y0, vx, vx + th, hy, cellh, cellw, cellh, colour);
    }
    if left {
        stroke(s, x0, y0, 0, vx + th, hy, hy + th, cellw, cellh, colour);
    }
    if right {
        stroke(s, x0, y0, vx, cellw, hy, hy + th, cellw, cellh, colour);
    }
}

/// One solid stroke band of a box glyph, in cell-relative coordinates clamped to the cell box.
#[allow(clippy::too_many_arguments)]
fn stroke(
    s: &mut Fb,
    x0: usize,
    y0: usize,
    xs: usize,
    xe: usize,
    ys: usize,
    ye: usize,
    cellw: usize,
    cellh: usize,
    colour: u32,
) {
    let (xe, ye) = (xe.min(cellw), ye.min(cellh));
    for y in ys..ye {
        for x in xs..xe {
            put_pixel(s, x0 + x, y0 + y, colour);
        }
    }
}
