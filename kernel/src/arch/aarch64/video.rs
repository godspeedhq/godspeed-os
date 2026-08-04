// SPDX-License-Identifier: GPL-2.0-only
//! Asking the VideoCore for a framebuffer, and handing it to the shared console.
//!
//! The Pi has no BIOS or bootloader-supplied framebuffer descriptor the way x86 does with Limine. The
//! GPU owns the display, and the ARM asks for a framebuffer through the same property mailbox that
//! reported the board revision and the RAM size at milestone 7.
//!
//! ## Why this runs before the MMU, like the memory map
//!
//! The GPU reads the request out of RAM directly. Asking while the caches are off removes the
//! coherency question rather than answering it - the same reasoning as [`super::mailbox`], and the same
//! reason both queries happen in the same early window.
//!
//! ## What the arch owes the shared console, and what it does not
//!
//! `crate::fbcon` is arch-neutral: the ANSI parser, the UTF-8 decoder, the character grid, glyph
//! rendering and scrolling are shared with x86 and the 32-bit ARM port. This file owes exactly three
//! things - a framebuffer as a `&'static mut [u8]`, [`FB_READBACK_CHEAP`], and [`fb_commit`] - and gets
//! a full terminal for them.
//!
//! Handing it a **slice** rather than a base address is what keeps every pixel write in the neutral
//! console bounds-checked and `unsafe`-free (§18.1). The one `unsafe` is here, where the mapping's
//! validity is actually known.
//!
//! ## Bus addresses, again
//!
//! The mailbox returns a **GPU bus address**, not an ARM physical one. Masking off the alias bits is
//! what turns it back into something the ARM can map. Skipping that step yields a plausible-looking
//! address that points nowhere, and a display that stays dark with no error - the GPU did its job.

use core::sync::atomic::{AtomicBool, Ordering};

use super::{put_dec, put_hex, put_str};

/// Set once the shared console has a framebuffer. Read before every mirror.
static CONSOLE_UP: AtomicBool = AtomicBool::new(false);

/// Held by whoever is currently rendering. See [`mirror`].
static RENDERING: AtomicBool = AtomicBool::new(false);

/// Whether the KERNEL LOG still mirrors to the display. True during boot, so the machine shows what it
/// is doing on a screen with no serial cable attached; false once the shell takes the console, so a
/// late service log does not land in the middle of the prompt. Serial keeps the full stream either way.
static BOOT_LOG_TO_FB: AtomicBool = AtomicBool::new(true);

/// Latches the one-way boot-log dismissal, so a second call is a no-op rather than a second clear.
static BOOT_DISMISSED: AtomicBool = AtomicBool::new(false);

/// Mirror a KERNEL LOG line, which stops at [`boot_complete`]. Console output uses [`mirror`].
pub fn mirror_log(s: &[u8]) {
    if BOOT_LOG_TO_FB.load(Ordering::Relaxed) {
        mirror(s);
    }
}

/// The shell has the console: clear the boot screen and stop mirroring the kernel log to it.
///
/// Without this the prompt shares a line with whatever service logged last, and the user sees a cursor
/// sitting after some log text with no `gsh>` in front of it. Serial is untouched - a captured log still
/// gets everything.
pub fn boot_complete() {
    if BOOT_DISMISSED.swap(true, Ordering::AcqRel) {
        return;
    }
    BOOT_LOG_TO_FB.store(false, Ordering::Release);
    if CONSOLE_UP.load(Ordering::Relaxed) {
        crate::fbcon::clear_and_home();
    }
}

/// Mirror a run of serial bytes to the display, **at most one writer at a time**.
///
/// Serial is the source of truth and has already taken these bytes; the framebuffer is a mirror, so
/// dropping a contended write costs nothing visible but avoids a real hazard. The neutral console is
/// behind a spinlock, and a spinlock is not reentrant: the timer tick can log while boot code is
/// part-way through rendering, and a same-core re-entry would spin until the lock watchdog panicked.
/// This flag makes the interrupted writer's mirror a no-op instead - the bytes still reach serial.
///
/// The `CONSOLE_UP` test comes first and stays lock-free, which is what keeps every pre-MMU boot
/// message out of the code below: those run with the caches off, where the console's exclusive
/// accesses have no business executing.
pub fn mirror(s: &[u8]) {
    if !CONSOLE_UP.load(Ordering::Relaxed) {
        return;
    }
    if RENDERING.swap(true, Ordering::Acquire) {
        return; // another writer (or a preempted self) owns the console; serial already has the bytes
    }
    crate::fbcon::put_bytes(s);
    RENDERING.store(false, Ordering::Release);
}

/// Framebuffer geometry as the GPU reported it.
#[derive(Clone, Copy)]
pub struct FbInfo {
    /// ARM physical base (bus alias already masked off).
    pub base: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
}

/// Reading this framebuffer back is **cheap**: it lives in ordinary RAM the GPU scans out of, and the
/// kernel's direct map covers it as Normal cacheable memory. The neutral console uses this to pick its
/// scroll strategy - copying within the framebuffer rather than re-rendering.
pub const FB_READBACK_CHEAP: bool = true;

/// Publish a written rectangle to the GPU.
///
/// The framebuffer is **cacheable**, so glyphs, scrolls and clears land in the D-cache and the GPU -
/// which reads RAM directly, not through the CPU's caches - would otherwise scan out stale pixels. This
/// cleans the affected lines to the point of coherency.
///
/// Only the rectangle that changed is cleaned, not the whole framebuffer: a full-screen clean on every
/// character would cost more than the drawing.
pub fn fb_commit(base: usize, pitch: usize, bpp: usize, x: usize, y: usize, w: usize, h: usize) {
    if w == 0 || h == 0 {
        return;
    }
    let line_bytes = w * bpp;
    // SAFETY: `dc cvac` is cache maintenance by virtual address over a range inside the framebuffer
    // mapping the caller is writing; it alters no architectural state beyond the caches. The `dsb`
    // orders the cleans against the GPU's next scan-out.
    unsafe {
        let ctr: u64;
        core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack));
        let dline = 4u64 << ((ctr >> 16) & 0xF); // CTR_EL0.DminLine, in words
        for row in 0..h {
            let start = (base + (y + row) * pitch + x * bpp) as u64;
            let end = start + line_bytes as u64;
            let mut a = start & !(dline - 1);
            while a < end {
                core::arch::asm!("dc cvac, {}", in(reg) a, options(nostack));
                a += dline;
            }
        }
        core::arch::asm!("dsb ish", options(nostack));
    }
}

// --- Property tags ---------------------------------------------------------------------------
const TAG_SET_PHYS_WH: u32 = 0x0004_8003;
const TAG_SET_VIRT_WH: u32 = 0x0004_8004;
const TAG_SET_VIRT_OFF: u32 = 0x0004_8009;
const TAG_SET_DEPTH: u32 = 0x0004_8005;
const TAG_SET_PIXEL_ORDER: u32 = 0x0004_8006;
const TAG_ALLOC_FB: u32 = 0x0004_0001;
const TAG_GET_PITCH: u32 = 0x0004_0008;
const TAG_GET_PHYS_WH: u32 = 0x0004_0003;

/// Ask the GPU what the attached display's resolution is, so the console matches the screen instead of
/// imposing a size on it. Falls back to a safe default if the display does not answer.
fn query_display_size() -> (u32, u32) {
    const FALLBACK: (u32, u32) = (1024, 768);
    let mut req = [0u32; 8];
    req[0] = 7 * 4;
    req[1] = 0;
    req[2] = TAG_GET_PHYS_WH;
    req[3] = 8;
    req[4] = 0;
    req[5] = 0;
    req[6] = 0;
    req[7] = 0; // end tag
    match super::mailbox::property_call(&mut req) {
        // A display can report an absurd size (or none at all, unplugged); cap it rather than let it
        // drive a fill loop and a mapping length.
        Some(_) if req[5] > 0 && req[6] > 0 && req[5] <= 4096 && req[6] <= 4096 => (req[5], req[6]),
        _ => FALLBACK,
    }
}

/// Allocate a framebuffer. Call once, before the MMU is enabled.
pub fn init() -> Option<FbInfo> {
    let (width, height) = query_display_size();

    let mut req = [0u32; 36];
    req[0] = 35 * 4;
    req[1] = 0;
    req[2] = TAG_SET_PHYS_WH;      req[3] = 8; req[4] = 8;  req[5] = width;  req[6] = height;
    req[7] = TAG_SET_VIRT_WH;      req[8] = 8; req[9] = 8;  req[10] = width; req[11] = height;
    req[12] = TAG_SET_VIRT_OFF;    req[13] = 8; req[14] = 8; req[15] = 0;    req[16] = 0;
    req[17] = TAG_SET_DEPTH;       req[18] = 4; req[19] = 4; req[20] = 32;
    req[21] = TAG_SET_PIXEL_ORDER; req[22] = 4; req[23] = 4; req[24] = 1;    // 1 = RGB
    req[25] = TAG_ALLOC_FB;        req[26] = 8; req[27] = 8; req[28] = 4096; req[29] = 0;
    req[30] = TAG_GET_PITCH;       req[31] = 4; req[32] = 4; req[33] = 0;
    req[34] = 0; // end tag

    if super::mailbox::property_call(&mut req).is_none() {
        put_str(b"aarch64: framebuffer mailbox request FAILED - staying on serial\r\n");
        return None;
    }

    put_str(b"aarch64: fb mailbox reply: alloc-base ");
    put_hex(req[28] as u64);
    put_str(b" size ");
    put_hex(req[29] as u64);
    put_str(b" pitch ");
    put_dec(req[33] as u64);
    put_str(b"
");
    let bus_base = req[28];
    let pitch = req[33];
    let (w, h) = (req[5], req[6]);
    let base = (bus_base & 0x3FFF_FFFF) as u64; // GPU bus address -> ARM physical

    if base == 0 || pitch == 0 {
        put_str(b"aarch64: framebuffer allocation returned null - staying on serial\r\n");
        return None;
    }
    // Range-check GPU-reported geometry before it drives a fill loop and a slice length. The GPU is
    // trusted, but an absurd value must not be taken on faith (defence in depth).
    if w == 0 || h == 0 || w > 8192 || h > 8192 || pitch > 0x1_0000 {
        put_str(b"aarch64: framebuffer geometry out of range - staying on serial\r\n");
        return None;
    }

    put_str(b"aarch64: framebuffer at ");
    put_hex(base);
    put_str(b" pitch ");
    put_dec(pitch as u64);
    put_str(b" (");
    put_dec(w as u64);
    put_str(b"x");
    put_dec(h as u64);
    put_str(b")\r\n");

    Some(FbInfo { base, pitch, width: w, height: h })
}

/// Hand the framebuffer to the shared console. Call AFTER the kernel has moved to the high half - the
/// slice must name the framebuffer through the direct map, not through a physical address.
pub fn start_console(fb: FbInfo) {
    let len = (fb.pitch as usize) * (fb.height as usize);
    // SAFETY: the GPU allocated this framebuffer and owns nothing else in it; it sits in the low 4 GiB
    // that the kernel's high half maps as Normal memory, so `KERNEL_VA_BASE + base` addresses it for the
    // system's lifetime. `len` is `pitch * height` from the GPU's own reply, bounds-checked above. This
    // is the one `unsafe` the display costs: from here the neutral console sees a slice and every pixel
    // write it makes is bounds-checked.
    let mem: &'static mut [u8] = unsafe {
        core::slice::from_raw_parts_mut((super::mmu::KERNEL_VA_BASE + fb.base) as *mut u8, len)
    };

    // Prove the mapping before trusting it. A framebuffer mapped somewhere other than where the GPU
    // scans out looks EXACTLY like a working console: the log says the console is up, and the screen
    // stays dark, with nothing on serial to say which of the two is wrong. One line of evidence is
    // cheaper than the guess it replaces (§26.7 - a failure with no symptom is a silent one). The
    // console clears the screen a few lines below, so the pattern is not left on the display.
    mem[0] = 0xAB;
    mem[1] = 0xCD;
    let probe_ok = mem[0] == 0xAB && mem[1] == 0xCD;
    put_str(b"aarch64: fb slice at ");
    put_hex(super::mmu::KERNEL_VA_BASE + fb.base);
    put_str(b" len ");
    put_dec(len as u64);
    put_str(if probe_ok { b" readback OK\r\n" } else { b" READBACK FAILED\r\n" });

    crate::fbcon::init(crate::fbcon::FbParams {
        mem,
        pitch: fb.pitch as usize,
        bpp: 4,
        width: fb.width as usize,
        height: fb.height as usize,
        // The Pi's mailbox framebuffer is 32bpp with RED in the low byte when pixel order 1 (RGB) is
        // requested above.
        r_shift: 0,
        g_shift: 8,
        b_shift: 16,
    });

    crate::fbcon::clear_and_home();
    // Armed LAST: until this is true every `mirror` is a no-op, so nothing can render through a console
    // that is not yet initialised.
    CONSOLE_UP.store(true, Ordering::Release);

    put_str(b"aarch64: framebuffer console up - serial output now mirrors to the display\r\n");
}
