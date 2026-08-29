// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7 (Pi 2 / BCM2836) backend for the kernel's boot/panic console floor (`crate::bootcon`).
//!
//! The terminal lives in the `console` SERVICE (`docs/console-service.md` §9). This file is only what is
//! genuinely architecture-specific here: the pixel format the GPU mailbox hands back, ordering writes out
//! to the GPU, and the single-writer policy the console path relies on.

use core::sync::atomic::{AtomicBool, Ordering};

/// The Pi's mailbox framebuffer is 32bpp with RED in the low byte (see `video::rgb`).
const BPP: usize = 4;
const R_SHIFT: u32 = 0;
const G_SHIFT: u32 = 8;
const B_SHIFT: u32 = 16;

/// Publish a written rectangle so the GPU sees it.
///
/// There is nothing to CLEAN: `mmu::section_fb` maps the framebuffer **Normal non-cacheable**, because
/// the `console` service maps the same physical pages into its own address space and ARM leaves
/// mismatched memory attributes for one physical page UNPREDICTABLE - and a service cannot do cache
/// maintenance at all (§18.2 forbids `unsafe`; `DCCMVAC` is PL1-only). What is still owed is a drain: a
/// non-cacheable store may sit in the write buffer, and the GPU scans RAM through its own bus. `DSB`
/// completes them.
///
/// The rectangle is therefore IGNORED here, and the signature keeps it only because an arch that maps
/// its framebuffer cacheable (the Pi 4) still needs it. This replaced a per-rectangle `clean_dcache`
/// walk, which the cacheable mapping needed and this one does not.
#[inline]
pub fn fb_commit(
    _base: usize, _pitch: usize, _bpp: usize,
    _x: usize, _y: usize, _w: usize, _h: usize,
) {
    // SAFETY: `dsb` is a barrier with no memory operand and no side effect beyond completing
    // outstanding accesses. Valid at PL1.
    unsafe { core::arch::asm!("dsb", options(nostack, preserves_flags)) };
}

/// True once `init` has set up the console; the serial path mirrors to the framebuffer only after this.
///
/// **This is an ARM-LOCAL flag, deliberately, and must not be replaced by a call into `crate::bootcon`.**
/// `pl011_write` mirrors every serial byte through here, including boot messages emitted *before*
/// `mmu::enable()`, and with the MMU off a real Cortex-A7 leaves LDREX/STREX UNPREDICTABLE (the
/// exclusive monitor needs memory attributes). So the pre-init serial path must touch nothing but a
/// plain load.
///
/// It USED to ask the neutral console's `ready()`, which at the time read a field behind its spinlock -
/// a compare_exchange, hence LDREX/STREX - and the Pi stopped booting entirely: it hung on the
/// firmware's rainbow splash before a single character of serial output, while QEMU (permissive about
/// exclusives) ran the same binary fine. Making the neutral flag lock-free fixed it, but left ARM's boot
/// depending on an implementation detail of another module, enforced only by comments. Owning the flag
/// here makes that dependency structural instead: nothing `crate::bootcon` does internally can reach this
/// path any more.
static READY: AtomicBool = AtomicBool::new(false);

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

/// Bring up the console over an already-mapped framebuffer. Called from the boot path right after
/// `video::map`, so everything logged from there on also shows on the TV.
///
/// This is where the framebuffer becomes a **slice**, which is what keeps the neutral console
/// (`crate::bootcon`) free of `unsafe` - only the arch knows that the mapping is valid and permanent.
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
    let phys = base as u64;
    // Publish the ARM-local ready flag only AFTER the neutral console is fully built (see `ready`).
    crate::bootcon::init(crate::bootcon::FbParams {
        mem,
        // The Pi identity-maps its framebuffer, so the mailbox base IS the physical base - which is what
        // the `console` service's grant is built from.
        phys,
        pitch: pitch as usize,
        bpp: BPP,
        width: width as usize,
        height: height as usize,
        r_shift: R_SHIFT,
        g_shift: G_SHIFT,
        b_shift: B_SHIFT,
    });
    READY.store(true, Ordering::Release);
}

/// Clear the screen and home the cursor - the end-of-boot dismissal (`console_boot_complete`).
pub fn clear_and_home() {
    crate::bootcon::clear_and_home();
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
    crate::bootcon::put_bytes(s);
    RENDERING.store(false, Ordering::Release);
}
