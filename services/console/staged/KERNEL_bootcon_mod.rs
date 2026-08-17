// SPDX-License-Identifier: GPL-2.0-only
//! The boot and panic console floor - what the kernel may print when there is no one to ask.
//!
//! This is **not** a terminal. It is the framebuffer half of the §11.4 log floor: the kernel's own
//! output path on a machine whose only output device is a display. It draws printable ASCII, honours
//! `\n` / `\r` / `\t` / `\b`, and *discards* escape sequences rather than interpreting them. There is no
//! character grid, no cursor, no reverse video, no UTF-8, no scrollback - a scroll clears the screen and
//! starts at the top, because the floor's job is to make the current message visible, and serial holds
//! the full history either way.
//!
//! ## Why the kernel has this at all
//!
//! Everything a terminal does - the ANSI/CSI state machine, the shadow grid, the cursor, scrolling,
//! reverse video, UTF-8 box drawing - lives in the **`console` service**, which drives this same
//! framebuffer through an MMIO grant. That is the whole point: rendering a shell prompt is policy, and
//! policy belongs in a service (§26.10).
//!
//! What is left here earns its place by **impossibility**, which is the only thing that does (§4.4):
//!
//! - **A panic cannot ask a service to report it.** The panic path halts every core; the console
//!   service is one of the things it halts. On a Pi wired to a TV with no serial cable, a kernel with
//!   no blit of its own dies with a frozen screen and no reason on it - a silent failure, which
//!   invariant 12 forbids outright.
//! - **Boot output precedes every service**, including the one that would render it. A boot that fails
//!   before the supervisor exists has nothing else to print through.
//!
//! Both are the ring buffer's argument (§11.4) applied to a machine that has no serial port, so this
//! module claims the same `kernel-log-floor` role the ring buffer does, and nothing wider.
//!
//! ## Ownership is explicit, and one-way until a panic
//!
//! The kernel owns the framebuffer from `init` until the `console` service says it has taken the screen
//! ([`release`]), after which every write here is a no-op - two writers to one framebuffer would fight
//! over the same pixels, and the service's shadow grid would be silently wrong about what is on screen.
//! [`reclaim_for_panic`] takes it back unconditionally, because at that point the service is not
//! running any more and correctness of its grid has stopped mattering.
//!
//! ## The arch contract
//!
//! - [`FbParams`] describes the framebuffer. The arch hands over the mapping as a `&'static mut [u8]`,
//!   which is what keeps every pixel write below bounds-checked and `unsafe`-free (§18.1): the single
//!   `unsafe` that turns a mapped address into a slice lives in the arch backend, which is the only
//!   place that knows the mapping is valid and permanent.
//! - `bg` is always black, so [`clear`] is a byte-zero fill on any channel layout.
//! - The framebuffer must be mapped **non-cacheable**, because the `console` service maps the same
//!   physical pages non-cacheable into its own address space and ARM leaves mismatched memory
//!   attributes for one physical page UNPREDICTABLE. That is also why there is no `fb_commit` any more:
//!   with no cacheable mapping there is nothing to clean to the Point of Coherency, and the arch's
//!   store ordering is handled by [`barrier`] at the end of a batch.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::smp::spinlock::SpinLock;

use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

/// Noto weight + raster height. `get_raster_width` / `RasterHeight::val` are `const fn`, so the per-cell
/// pixel box is known at compile time. Size20 is about 9x20 px.
const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size20;
const CELL_W: usize = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
const CELL_H: usize = RASTER_HEIGHT.val();

/// Safe-area inset per edge, as a percentage of each dimension. TVs overscan (crop off every edge),
/// which clips the outermost characters at `0`. Kept identical to the `console` service's inset so a
/// boot message and the prompt that replaces it sit on the same margin rather than visibly jumping.
const SAFE_PCT: usize = 5;

/// Console foreground, as raw 0-255 channel components. Soft green on black.
const FG_RGB: (u32, u32, u32) = (0x80, 0xFF, 0x80);

/// What the architecture must tell the floor about its framebuffer. Plain linear-framebuffer geometry;
/// nothing here is arch-specific beyond the values themselves.
pub struct FbParams {
    /// The framebuffer itself, valid for the system lifetime. Must be at least `pitch * height` bytes.
    pub mem: &'static mut [u8],
    /// Physical base of that mapping - what the `console` service's grant is built from.
    pub phys: u64,
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

/// The framebuffer as handed to the `console` service at spawn: where it is, how big, and how to read a
/// pixel. Everything the service needs to render and **nothing about text** - no rows, no columns, no
/// cell size. Character geometry is the terminal's own business, and the terminal is the service, so the
/// kernel never computes or publishes it (Commandment III: one source of truth per fact).
#[derive(Clone, Copy)]
pub struct FbGrant {
    pub phys: u64,
    pub len: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
    /// `r_shift | g_shift << 8 | b_shift << 16`, packed so the grant stays one small record.
    pub shifts: u32,
}

/// Boot-floor state. Everything in it is `Send`, which `SpinLock<T: Send>` requires to be `Sync` as a
/// `static` - including the framebuffer, held as a slice rather than a raw pointer (a raw pointer would
/// not be `Send`, and a slice is bounds-checked besides).
struct Boot {
    mem: Option<&'static mut [u8]>,
    phys: u64,
    pitch: usize,
    bpp: usize,
    width: usize,
    height: usize,
    scale: usize, // integer font upscale, so a 4K panel is not a wall of unreadable text
    org_x: usize, // left edge of the text area (safe-area inset)
    org_y: usize, // top edge of the text area
    cols: usize,  // columns that fit - PRIVATE to this blit, never published (see FbGrant)
    rows: usize,  // rows that fit - likewise
    col: usize,
    row: usize,
    fg: u32,
    /// 0 = normal, 1 = saw ESC, 2 = inside CSI. The floor does not *interpret* escapes; it tracks just
    /// enough state to swallow them, so a stray `ESC[2J` from a service does not spray `[2J` across a
    /// boot screen. Discarding is the honest floor behaviour: the terminal that understands these lives
    /// in the console service.
    esc: u8,
    /// `blend_lut[intensity]` = the glyph-pixel colour for that antialiasing intensity, composed in the
    /// device's channel layout, so an antialiased edge costs one table read per pixel.
    blend_lut: [u32; 256],
}

static FB: SpinLock<Boot> = SpinLock::new(Boot {
    mem: None,
    phys: 0,
    pitch: 0,
    bpp: 0,
    width: 0,
    height: 0,
    scale: 1,
    org_x: 0,
    org_y: 0,
    cols: 0,
    rows: 0,
    col: 0,
    row: 0,
    fg: 0,
    esc: 0,
    blend_lut: [0; 256],
});

/// Set once `init` has published the floor. **Deliberately a plain flag rather than a field behind
/// `FB`**, because every entry point below tests it BEFORE taking the lock.
///
/// On ARMv7 the serial path mirrors every byte here, including boot messages emitted *before the MMU is
/// on*, and `SpinLock::lock` is a `compare_exchange` - LDREX/STREX, which a real Cortex-A7 leaves
/// UNPREDICTABLE without the MMU (the exclusive monitor needs memory attributes). QEMU emulates it
/// permissively, so this fails only on silicon: the Pi hangs on the firmware's rainbow splash before a
/// single character of serial output. A plain load is one `LDRB` and is safe with the MMU off.
static READY: AtomicBool = AtomicBool::new(false);

/// Cleared once the `console` service owns the screen. See the module header: two writers to one
/// framebuffer is not a race the service can defend against, so the kernel simply stops.
static OWNED_BY_KERNEL: AtomicBool = AtomicBool::new(true);

/// True once `init` has run AND the kernel still owns the screen. An arch that mirrors serial output to
/// the display checks this so it can call `put_bytes` unconditionally.
pub fn ready() -> bool {
    READY.load(Ordering::Acquire) && OWNED_BY_KERNEL.load(Ordering::Acquire)
}

/// Initialise the floor from the architecture's framebuffer description. Called once during boot, right
/// after serial init and before the first `kprintln`, so boot output reaches the display.
pub fn init(p: FbParams) {
    let (rs, gs, bs) = (p.r_shift, p.g_shift, p.b_shift);

    let mut s = FB.lock();
    s.phys = p.phys;
    s.pitch = p.pitch;
    s.bpp = p.bpp;
    s.width = p.width;
    s.height = p.height;
    s.mem = Some(p.mem);
    // A plain `height / 600` puts the first upscale step at 1200 px, which is the honest boundary: below
    // it a 20 px cell is perfectly legible and should stay pixel-exact, and upscaling coarsens an
    // antialiased raster into visible chunks. The T630 (768) stays 1x, a 1824x984 TV stays 1x, and the
    // Wyse 5070's native 3840x2160 gets 3x rather than a wall of tiny text.
    s.scale = (s.height / 600).clamp(1, 3);
    s.org_x = s.width * SAFE_PCT / 100;
    s.org_y = s.height * SAFE_PCT / 100;
    s.cols = (s.width - 2 * s.org_x) / (CELL_W * s.scale);
    s.rows = (s.height - 2 * s.org_y) / (CELL_H * s.scale);
    s.col = 0;
    s.row = 0;
    s.esc = 0;
    s.fg = (FG_RGB.0 << rs) | (FG_RGB.1 << gs) | (FG_RGB.2 << bs);
    for i in 0..256u32 {
        let (r, g, b) = (FG_RGB.0 * i / 255, FG_RGB.1 * i / 255, FG_RGB.2 * i / 255);
        s.blend_lut[i as usize] = (r << rs) | (g << gs) | (b << bs);
    }
    clear(&mut s);
    let (w, h, bpp, scale) = (s.width, s.height, s.bpp, s.scale);
    // Drop the lock first: `kprintln` renders through this same floor, so logging while holding it would
    // re-enter a non-reentrant spinlock.
    drop(s);
    // Publish LAST - `ready` is what every entry point tests before it touches the lock, so the floor
    // must be fully built and cleared before any of them may proceed.
    READY.store(true, Ordering::Release);
    crate::kprintln!("bootcon: {}x{} {}bpp, font-scale {}x", w, h, bpp * 8, scale);
}

/// The framebuffer as a grant for the `console` service, or `None` if this machine has no framebuffer.
pub fn grant() -> Option<FbGrant> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }
    let s = FB.lock();
    let (rs, gs, bs) = shifts_of(&s);
    Some(FbGrant {
        phys: s.phys,
        len: (s.pitch * s.height) as u64,
        pitch: s.pitch as u32,
        width: s.width as u32,
        height: s.height as u32,
        bpp: s.bpp as u32,
        shifts: rs | (gs << 8) | (bs << 16),
    })
}

/// Recover the channel shifts from the blend LUT rather than storing them twice: `blend_lut[255]` is the
/// full-intensity foreground, and each channel's component of `FG_RGB` is distinct and non-zero, so the
/// position of each is the shift the arch reported. One stored fact, read two ways (Commandment III).
fn shifts_of(s: &Boot) -> (u32, u32, u32) {
    let full = s.blend_lut[255];
    let find = |component: u32| -> u32 {
        for sh in (0..32u32).step_by(8) {
            if (full >> sh) & 0xFF == component {
                return sh;
            }
        }
        0
    };
    (find(FG_RGB.0), find(FG_RGB.1), find(FG_RGB.2))
}

/// Hand the screen to the `console` service. Every subsequent write here is a no-op until a panic.
///
/// The service calls this once it has mapped the framebuffer and cleared it, so there is no window in
/// which neither party is drawing: the kernel stops only after the service is able to start.
pub fn release() {
    OWNED_BY_KERNEL.store(false, Ordering::Release);
}

/// Take the screen back for a panic, and clear it so the reason is not printed over a half-drawn shell.
///
/// Unconditional by design: the panic path has already halted (or is about to halt) every other core,
/// so the console service is not going to draw again and its shadow grid no longer describes anything
/// that matters. A panic that could not print because a service held the screen would be the silent
/// failure invariant 12 exists to prevent.
pub fn reclaim_for_panic() {
    if !READY.load(Ordering::Acquire) {
        return;
    }
    OWNED_BY_KERNEL.store(true, Ordering::Release);
    let mut s = FB.lock();
    clear(&mut s);
}

/// Write a byte sequence to the floor under a single lock.
pub fn put_bytes(bytes: &[u8]) {
    if !ready() {
        return;
    }
    let mut s = FB.lock();
    for &b in bytes {
        put(&mut s, b);
    }
    barrier();
}

/// Clear the screen and home the cursor.
pub fn clear_and_home() {
    if !ready() {
        return;
    }
    let mut s = FB.lock();
    clear(&mut s);
    barrier();
}

/// Order the framebuffer stores before the console lock is released.
///
/// The mapping is non-cacheable on every arch that has one (see the module header), so there is nothing
/// to clean - but a non-cacheable store may still sit in a write buffer, and the lock's atomic release
/// orders normal memory, not that buffer. One barrier per batch, not per glyph.
#[inline]
fn barrier() {
    crate::arch::imp::fb_barrier();
}

/// Clear the whole framebuffer and home the cursor. `bg` is black - all channels zero, so all bytes zero
/// - which makes a flat byte-zero fill correct regardless of the device's channel layout.
fn clear(s: &mut Boot) {
    if let Some(mem) = s.mem.as_deref_mut() {
        mem.fill(0);
    }
    s.col = 0;
    s.row = 0;
}

/// Process one output byte.
fn put(s: &mut Boot, b: u8) {
    // Swallow escape sequences instead of printing their bytes. The floor renders text; a service that
    // wants a cursor moved is talking to the console service, not to this.
    match s.esc {
        1 => {
            s.esc = if b == b'[' { 2 } else { 0 };
            return;
        }
        2 => {
            // A CSI sequence ends at its final byte, 0x40..=0x7E.
            if (0x40..=0x7E).contains(&b) {
                s.esc = 0;
            }
            return;
        }
        _ => {}
    }
    match b {
        0x1B => s.esc = 1,
        b'\n' => newline(s),
        b'\r' => s.col = 0,
        0x08 => s.col = s.col.saturating_sub(1),
        b'\t' => {
            let next = (s.col / 8 + 1) * 8;
            s.col = next.min(s.cols.saturating_sub(1));
        }
        // Anything outside printable ASCII becomes '?': visible, never silently dropped (§3.12). That
        // includes every UTF-8 byte - decoding is the terminal's job, and the terminal is the service.
        _ => {
            let ch = if (0x20..0x7F).contains(&b) { b } else { b'?' };
            if s.col >= s.cols {
                newline(s);
            }
            let (col, row) = (s.col, s.row);
            draw_glyph(s, ch, col, row);
            s.col += 1;
        }
    }
}

/// Advance to the next line. Reaching the bottom **clears and starts again at the top** rather than
/// scrolling: a scroll means either reading the framebuffer back (slow on a non-cacheable mapping) or
/// keeping a shadow grid to repaint from (a terminal, which this is not). Serial holds the full history,
/// so the floor optimises for the message currently being printed being legible.
fn newline(s: &mut Boot) {
    s.col = 0;
    s.row += 1;
    if s.row >= s.rows {
        clear(s);
    }
}

/// Paint `count` consecutive pixels one colour, starting at byte offset `off`. The bounds check happens
/// once, when the row is sliced; the write loop then runs over a contiguous run. An out-of-range run
/// paints nothing - it cannot reach past the framebuffer.
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

/// Fill a solid `w x h` pixel rectangle at (x, y), one contiguous run per row.
fn fill_rect(s: &mut Boot, x: usize, y: usize, w: usize, h: usize, color: u32) {
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

/// Render one glyph at text cell (col, row). Every cell pixel is written - intensity 0 paints the
/// background - so a cell is fully repainted with no stale pixels beneath it.
fn draw_glyph(s: &mut Boot, ch: u8, col: usize, row: usize) {
    let sc = s.scale;
    let x0 = s.org_x + col * CELL_W * sc;
    let y0 = s.org_y + row * CELL_H * sc;
    if ch == b' ' {
        fill_rect(s, x0, y0, CELL_W * sc, CELL_H * sc, 0);
        return;
    }
    let Some(rc) = get_raster(ch as char, FONT_WEIGHT, RASTER_HEIGHT) else {
        fill_rect(s, x0, y0, CELL_W * sc, CELL_H * sc, 0);
        return;
    };
    let raster = rc.raster();
    let (cw, chh) = (CELL_W * sc, CELL_H * sc);
    // The cell lies fully inside the framebuffer (cols/rows are sized to fit) - but guard, so the
    // contiguous-run path below stays on whole rows, and fall back to the general blit otherwise.
    if s.bpp != 4 || x0 + cw > s.width || y0 + chh > s.height {
        for (gy, rowpix) in raster.iter().enumerate() {
            for (gx, &intensity) in rowpix.iter().enumerate() {
                let color = s.blend_lut[intensity as usize];
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

/// Paint one output row of a glyph: each raster intensity looked up once and replicated `sc` times
/// horizontally, written as a single contiguous run. 32bpp only - `draw_glyph` routes other depths to
/// the `fill_rect` path.
fn put_glyph_row(s: &mut Boot, off: usize, rowpix: &[u8], sc: usize) {
    let count = rowpix.len() * sc;
    let lut = &s.blend_lut;
    let Some(mem) = s.mem.as_deref_mut() else {
        return;
    };
    let Some(run) = off.checked_add(count * 4).and_then(|e| mem.get_mut(off..e)) else {
        return;
    };
    let mut px = run.chunks_exact_mut(4);
    for &intensity in rowpix {
        let b = lut[intensity as usize].to_ne_bytes();
        for _ in 0..sc {
            match px.next() {
                Some(p) => p.copy_from_slice(&b),
                None => return,
            }
        }
    }
}
