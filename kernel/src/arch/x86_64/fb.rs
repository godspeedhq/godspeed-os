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

/// Framebuffer console state. The base pointer is stored as `usize` so the
/// struct is `Send` (a raw pointer would not be), which `SpinLock<T: Send>`
/// requires to be `Sync` as a `static`.
struct Fb {
    base: usize,   // framebuffer base virtual address (Limine HHDM)
    pitch: usize,  // bytes per scanline
    bpp: usize,    // bytes per pixel
    width: usize,  // visible width in pixels
    height: usize, // visible height in pixels
    cols: usize,   // text columns (width / GLYPH_W)
    rows: usize,   // text rows (height / GLYPH_H)
    col: usize,    // cursor column
    row: usize,    // cursor row
    fg: u32,       // foreground pixel value (already in the device's channel layout)
    bg: u32,       // background pixel value
    ready: bool,   // false until fb_init runs; put_byte no-ops until then
}

static FB: SpinLock<Fb> = SpinLock::new(Fb {
    base: 0, pitch: 0, bpp: 0, width: 0, height: 0,
    cols: 0, rows: 0, col: 0, row: 0, fg: 0, bg: 0, ready: false,
});

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
    s.cols = s.width / GLYPH_W;
    s.rows = s.height / GLYPH_H;
    s.col = 0;
    s.row = 0;
    s.fg = make(0x80, 0xFF, 0x80); // soft green on black — classic console look
    s.bg = make(0x00, 0x00, 0x00);
    s.ready = true;
    clear(&s);
}

/// Mirror one output byte to the framebuffer console. Called from
/// `serial_write_byte` / `serial_write_bytes_lockfree` for every output byte.
pub fn put_byte(b: u8) {
    let mut s = FB.lock();
    if !s.ready {
        return;
    }
    match b {
        b'\n' => advance_line(&mut s),
        b'\r' => s.col = 0,
        0x08 | 0x7f => {
            if s.col > 0 {
                s.col -= 1;
            }
        }
        0x20..=0x7e => {
            let (c, r) = (s.col, s.row);
            draw_glyph(&s, b, c, r);
            s.col += 1;
            if s.col >= s.cols {
                advance_line(&mut s);
            }
        }
        _ => {}
    }
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
fn clear(s: &Fb) {
    let base = s.base as *mut u8;
    let total = s.height * s.pitch;
    // SAFETY: [base, base+total) is the framebuffer Limine mapped and sized
    // (height*pitch); it is valid for writes for the system lifetime.
    unsafe { core::ptr::write_bytes(base, 0, total) };
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
    let x0 = col * GLYPH_W;
    let y0 = row * GLYPH_H;
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

/// Scroll the display up by one text row. Copies the framebuffer up by one
/// glyph height and clears the freed bottom row.
fn scroll(s: &Fb) {
    let row_bytes = GLYPH_H * s.pitch;
    let total = s.height * s.pitch;
    if row_bytes >= total {
        return;
    }
    let base = s.base as *mut u8;
    // SAFETY: source [base+row_bytes, base+total) and dest [base, base+total-
    // row_bytes) are both within the mapped framebuffer; copy handles overlap.
    unsafe {
        core::ptr::copy(base.add(row_bytes), base, total - row_bytes);
        // Clear the freed bottom row (bg is black ⇒ byte-zero).
        core::ptr::write_bytes(base.add(total - row_bytes), 0, row_bytes);
    }
}
