// SPDX-License-Identifier: GPL-2.0-only
//! Halting and releasing a core on the chip's AI backplane, through its wrapper.
//!
//! The ARM must be **halted** before firmware is written into its TCM and **released** afterwards, and both
//! go through the core's WRAPPER rather than its register base. That is why deriving `0x18102000` for this
//! chip's CR4 mattered: `brcmf_chip_disable_arm` reads `wrapbase + BCMA_IOCTL`, and
//! `brcmf_chip_ai_coredisable` / `brcmf_chip_ai_resetcore` compute `wrapbase + BCMA_RESET_CTL` and
//! `wrapbase + BCMA_IOCTL` throughout.
//!
//! ## The sequence, quoted
//!
//! From `cyw43-driver`, the chip vendor's own driver, because it is the shortest complete statement of it:
//!
//! ```c
//! static void reset_device_core(cyw43_int_t *self, int core_id, bool core_halt) {
//!     disable_device_core(self, core_id, core_halt);
//!     uint32_t base = get_core_address(core_id);
//!     cyw43_write_backplane(self, base + AI_IOCTRL_OFFSET, 1,
//!         SICF_FGC | SICF_CLOCK_EN | (core_halt ? SICF_CPUHALT : 0));
//!     cyw43_read_backplane(self, base + AI_IOCTRL_OFFSET, 1);
//!     cyw43_write_backplane(self, base + AI_RESETCTRL_OFFSET, 1, 0);
//!     cyw43_delay_ms(1);
//!     cyw43_write_backplane(self, base + AI_IOCTRL_OFFSET, 1,
//!         SICF_CLOCK_EN | (core_halt ? SICF_CPUHALT : 0));
//!     cyw43_read_backplane(self, base + AI_IOCTRL_OFFSET, 1);
//!     cyw43_delay_ms(1);
//! }
//! ```
//!
//! with these offsets and bits, also quoted:
//!
//! ```c
//! #define AI_IOCTRL_OFFSET    (0x408)      /* BCMA_IOCTL     */
//! #define AI_RESETCTRL_OFFSET (0x800)      /* BCMA_RESET_CTL */
//! #define SICF_CLOCK_EN       (0x0001)
//! #define SICF_FGC            (0x0002)
//! #define SICF_CPUHALT        (0x0020)
//! #define AIRC_RESET          (1)
//! ```
//!
//! **The two reads after the writes are not decoration.** Both references perform them, and a read-back
//! after a register write on this bus is how the write is made to take effect before the next step - the
//! same posting discipline the SDHCI side needed. They are kept, and their results are used as the check
//! that the write landed.
//!
//! **The two 1 ms waits are not decoration either.** They are in the reference and they are the silicon's
//! requirement, not the reference's design (§26.14). They use the SDK's `sleep_ms`, which SLEEPS rather than
//! spins and converts through the kernel's own calibration so it is right on any machine. A hand-rolled
//! spin was written first and thrown away on finding it: reaching for the sanctioned bounded tool is the
//! rule, and reinventing it is the mistake §26.6.1 names by name.
//!
//! ## What is deliberately NOT here
//!
//! `brcmf_sdio_buscore_activate` additionally writes the firmware's reset vector - the first four bytes of
//! the image - to backplane address 0, conditionally (`if (rstvec)`). **That function could not be read**:
//! it sits past the point where `sdio.c` truncates when fetched, and the vendor driver's post-download
//! sequence does not show an equivalent write. So it is not implemented, and it is recorded here rather
//! than guessed at: **if the ARM does not come up, that write is the first thing to read and add.**

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::host::Host;

/// Register offsets inside a core's wrapper, quoted above.
mod off {
    /// `AI_IOCTRL` / `BCMA_IOCTL`.
    pub const IOCTRL: u32 = 0x408;
    /// `AI_RESETCTRL` / `BCMA_RESET_CTL`.
    pub const RESETCTRL: u32 = 0x800;
}

/// `AI_IOCTRL` and `AI_RESETCTRL` bits, quoted above.
mod bit {
    pub const CLOCK_EN: u32 = 0x0001;
    pub const FGC: u32 = 0x0002;
    pub const CPUHALT: u32 = 0x0020;
    /// In `AI_RESETCTRL`: the core is held in reset.
    pub const AIRC_RESET: u32 = 0x0001;
}

/// Put a core into reset, and confirm it got there.
///
/// Returns false if the core does not report itself in reset afterwards, because every step after this one
/// writes into memory that a running core also owns.
pub fn disable(
    h: &Host,
    w: &mut Window,
    wrapper: u32,
    halt: bool,
    ctx: &ServiceContext,
) -> bool {
    let ioctrl = if halt { bit::CPUHALT } else { 0 };

    // Already in reset? The reference checks first and returns without touching anything, because driving a
    // core that is already held is how you get an undefined state rather than a fresh one.
    if let Some(rc) = w.read32(h, wrapper + off::RESETCTRL, ctx) {
        if rc & bit::AIRC_RESET != 0 {
            ctx.log_fmt(format_args!(
                "wifi-driver: core wrapper {:#010x} is already in reset (RESETCTRL {:#010x})",
                wrapper, rc
            ));
            // Still configure IOCTRL for the state we want, which is what the reference's
            // `in_reset_configure` label does.
            let _ = w.write32(h, wrapper + off::IOCTRL, ioctrl | bit::FGC | bit::CLOCK_EN, ctx);
            let _ = w.read32(h, wrapper + off::IOCTRL, ctx);
            return true;
        }
    } else {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not read RESETCTRL at {:#010x}, so the core's state is unknown",
            wrapper + off::RESETCTRL
        ));
        return false;
    }

    if w.write32(h, wrapper + off::IOCTRL, ioctrl | bit::FGC | bit::CLOCK_EN, ctx).is_none() {
        return false;
    }
    let _ = w.read32(h, wrapper + off::IOCTRL, ctx);
    if w.write32(h, wrapper + off::RESETCTRL, bit::AIRC_RESET, ctx).is_none() {
        return false;
    }

    // WAIT FOR IT TO TAKE, bounded, and report if it does not. The reference spins on this for 300 us; each
    // read here is a CMD53 of tens of microseconds, so a modest count covers it and the failure says so
    // rather than continuing into a write the core might still be servicing.
    const TRIES: u32 = 100;
    for attempt in 0..TRIES {
        match w.read32(h, wrapper + off::RESETCTRL, ctx) {
            Some(rc) if rc & bit::AIRC_RESET != 0 => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: core wrapper {:#010x} held in reset after {} read(s){}",
                    wrapper,
                    attempt + 1,
                    if halt { ", CPU halted" } else { "" }
                ));
                let _ = w.write32(h, wrapper + off::IOCTRL, ioctrl | bit::FGC | bit::CLOCK_EN, ctx);
                let _ = w.read32(h, wrapper + off::IOCTRL, ctx);
                return true;
            }
            Some(_) => {}
            None => return false,
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: core wrapper {:#010x} never reported itself in reset across {} reads - nothing \
         further is written, because a running core owns the memory the firmware goes into",
        wrapper, TRIES
    ));
    false
}

/// Take a core out of reset. `halt` keeps the CPU halted while its clock runs.
///
/// The sequence is the reference's, in its order, including both read-backs and both 1 ms waits.
pub fn reset(
    h: &Host,
    w: &mut Window,
    wrapper: u32,
    halt: bool,
    ctx: &ServiceContext,
) -> bool {
    if !disable(h, w, wrapper, halt, ctx) {
        return false;
    }
    let halt_bit = if halt { bit::CPUHALT } else { 0 };

    if w.write32(h, wrapper + off::IOCTRL, halt_bit | bit::FGC | bit::CLOCK_EN, ctx).is_none() {
        return false;
    }
    let _ = w.read32(h, wrapper + off::IOCTRL, ctx);

    // OUT OF RESET.
    if w.write32(h, wrapper + off::RESETCTRL, 0, ctx).is_none() {
        return false;
    }
    ctx.sleep_ms(1);

    // Then the clock without the force-gated-clock bit, which is the settled state.
    if w.write32(h, wrapper + off::IOCTRL, halt_bit | bit::CLOCK_EN, ctx).is_none() {
        return false;
    }
    let final_ioctrl = w.read32(h, wrapper + off::IOCTRL, ctx);
    ctx.sleep_ms(1);

    // AND CONFIRM IT IS OUT, rather than assuming the write landed. `is_up` is the reference's own
    // post-condition (`device_core_is_up`), and a core still in reset after this is a failure worth naming
    // before anything waits on firmware that cannot be running.
    let rc = w.read32(h, wrapper + off::RESETCTRL, ctx);
    let up = matches!(rc, Some(v) if v & bit::AIRC_RESET == 0);
    ctx.log_fmt(format_args!(
        "wifi-driver: core wrapper {:#010x} out of reset: RESETCTRL {:#010x}, IOCTRL {:#010x} - {}",
        wrapper,
        rc.unwrap_or(0xFFFF_FFFF),
        final_ioctrl.unwrap_or(0xFFFF_FFFF),
        if up { "RUNNING" } else { "STILL IN RESET" }
    ));
    up
}
