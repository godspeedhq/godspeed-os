// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7 (Pi 2 / BCM2836) backend for the shared framebuffer console (`crate::fbcon`).
//!
//! The terminal itself - escape parsing, the character grid, glyph rendering, scrolling - is neutral
//! code. This file is only what is genuinely architecture-specific here: the pixel format the GPU
//! mailbox hands back, publishing writes out of the **cacheable** framebuffer to the GPU, and the
//! single-writer policy the console path relies on.

use core::sync::atomic::{AtomicBool, Ordering};

/// Reading this framebuffer back is **cheap**: `mmu::section_fb` maps it cacheable (the GPU scans RAM
/// through its own bus, and this console cleans to the Point of Coherency after every batch anyway), so
/// a strided memmove of the text area beats repainting every cell from the shadow grid on a slower
/// core. `fbcon::scroll` reads this to pick that path.
pub const FB_READBACK_CHEAP: bool = true;

/// The Pi's mailbox framebuffer is 32bpp with RED in the low byte (see `video::rgb`).
const BPP: usize = 4;
const R_SHIFT: u32 = 0;
const G_SHIFT: u32 = 8;
const B_SHIFT: u32 = 16;

/// Publish a written rectangle to the Point of Coherency so the GPU sees it.
///
/// The framebuffer is cacheable, so every glyph, scroll and clear lands in the D-cache; without this
/// the display shows stale pixels. Cleans row by row because a rectangle is strided in memory (each
/// scanline is `pitch` apart), and cleans only the rectangle's own bytes - the padding between
/// `width * bpp` and `pitch` is never scanned out, so it needs no maintenance.
pub fn fb_commit(base: usize, pitch: usize, bpp: usize, x: usize, y: usize, w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    let bytes = (w * bpp) as u32;
    for row in y..(y + h) {
        super::page_tables::clean_dcache((base + row * pitch + x * bpp) as u32, bytes);
    }
}

/// True once `init` has set up the console; the serial path mirrors to the framebuffer only after this.
///
/// **Lock-free by requirement, not by preference.** `pl011_write` mirrors every serial byte through
/// here, including boot messages emitted *before `mmu::enable()`*, and with the MMU off a real
/// Cortex-A7 leaves LDREX/STREX UNPREDICTABLE (the exclusive monitor needs memory attributes). So this
/// must stay a plain atomic load, and nothing on the pre-`init` path may take the console's spinlock -
/// which is why `crate::fbcon::ready` is a bare `AtomicBool`, not a field behind the lock.
pub fn ready() -> bool {
    crate::fbcon::ready()
}

/// Bring up the console over an already-mapped framebuffer. Called from the boot path right after
/// `video::map`, so everything logged from there on also shows on the TV.
///
/// This is where the framebuffer becomes a **slice**, which is what keeps the neutral console
/// (`crate::fbcon`) free of `unsafe` - only the arch knows that the mapping is valid and permanent.
pub fn init(base: u32, pitch: u32, width: u32, height: u32) {
    let len = (pitch as usize) * (height as usize);
    // SAFETY: the GPU mailbox reported this framebuffer's base and geometry (`video::request`) and
    // `video::map` mapped [base, base + pitch*height) cacheable before this runs, so the region is
    // valid for writes for exactly that length and stays mapped for the system lifetime - hence
    // 'static. This runs once on the boot core before the tick or any AP starts, and nothing else
    // touches the framebuffer after `video::map`, so this is the only live reference and the
    // exclusivity of `&mut` holds.
    let mem: &'static mut [u8] =
        unsafe { core::slice::from_raw_parts_mut(base as usize as *mut u8, len) };
    crate::fbcon::init(crate::fbcon::FbParams {
        mem,
        pitch: pitch as usize,
        bpp: BPP,
        width: width as usize,
        height: height as usize,
        r_shift: R_SHIFT,
        g_shift: G_SHIFT,
        b_shift: B_SHIFT,
    });
}

/// Clear the screen and home the cursor - the end-of-boot dismissal (`console_boot_complete`).
pub fn clear_and_home() {
    crate::fbcon::clear_and_home();
}

/// Set while a caller is inside the neutral console, so a second writer skips the mirror instead of
/// waiting for it.
static RENDERING: AtomicBool = AtomicBool::new(false);

/// Mirror a run of serial bytes to the framebuffer, **at most one writer at a time**.
///
/// Serial is the source of truth and has already taken these bytes; the framebuffer is a mirror, so
/// dropping a contended write costs nothing visible but avoids two hazards:
///
/// - **Re-entrancy.** The tick ISR polls DWC2 and can log (a hot-plug notice, say) while boot code is
///   part-way through rendering. The neutral console is behind a spinlock, and a spinlock is not
///   reentrant, so a same-core re-entry would spin until the lock watchdog panicked. This flag makes
///   the ISR's mirror a no-op instead - the bytes still reach serial.
/// - **Cross-core contention.** `pl011_write` already declines to mirror when it could not claim
///   `SERIAL_BUSY`; this covers the pre-SMP window before that gate is armed.
///
/// The dominant console writer (the shell) is never the one that loses a write, because it holds
/// `SERIAL_BUSY` for its own output.
pub fn mirror(s: &[u8]) {
    // The `ready` test MUST come first and MUST stay lock-free: it is what keeps every pre-MMU boot
    // message out of the exclusive-access code below. `swap` is LDREX/STREX, as is the console's
    // spinlock, and neither is safe before `mmu::enable()` - see `ready`.
    if !ready() {
        return;
    }
    if RENDERING.swap(true, Ordering::Acquire) {
        return; // another writer (or a preempted self) owns the console; serial already has the bytes
    }
    crate::fbcon::put_bytes(s);
    RENDERING.store(false, Ordering::Release);
}
