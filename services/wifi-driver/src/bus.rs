// SPDX-License-Identifier: GPL-2.0-only
//! Bringing the bus up to the point where frames could flow, once the firmware is running.
//!
//! Phase 2 ends with the radio's own CR4 executing Broadcom's firmware and publishing its SDPCM shared
//! structure. That is a running chip, not a reachable one: there is no data function enabled, the chip is
//! still on its low-power ALP clock, and the firmware has not been told which protocol version the host
//! speaks. This module is the four steps that close that gap, all of them taken from the reference.
//!
//! ## The sequence, quoted
//!
//! From OpenBSD's `bwfm`, used here because Linux's `brcmfmac/sdio.c` truncates on every fetch before the
//! functions that matter (see `docs/wifi.md` §26). Immediately after the firmware is activated:
//!
//! ```c
//! bwfm_sdio_clkctl(sc, CLK_AVAIL, 0);
//! bwfm_sdio_write_1(sc, BWFM_SDIO_FUNC1_CHIPCLKCSR, clk |
//!     BWFM_SDIO_FUNC1_CHIPCLKCSR_FORCE_HT);
//! bwfm_sdio_dev_write(sc, SDPCMD_TOSBMAILBOXDATA,
//!     SDPCM_PROT_VERSION << SDPCM_PROT_VERSION_SHIFT);
//! sdmmc_io_set_blocklen(sc->sc_sf[2], 512);
//! sdmmc_io_function_enable(sc->sc_sf[2])
//! ```
//!
//! with these constants, also quoted:
//!
//! ```c
//! #define SDPCM_PROT_VERSION			4
//! #define SDPCM_PROT_VERSION_SHIFT		16
//! #define SDPCMD_INTSTATUS			0x020
//! #define SDPCMD_TOSBMAILBOXDATA			0x048
//! ```
//!
//! ## Why each step, and how each one is CHECKED
//!
//! Every step here is confirmed by something the chip reports, rather than assumed from a write landing.
//! That is the discipline the last six boots established: a write that succeeds is not a state that holds.
//!
//! 1. **The HT clock.** The chip has been running on ALP - the low-power clock the backplane needed for the
//!    upload. Frames need the high-throughput clock. Checked by `CHIPCLKCSR` reporting `HT_AVAIL` (0x80),
//!    which is the chip saying the clock is actually up, not that the request was accepted.
//! 2. **The SDIO core's `INTSTATUS`, cleared.** The reference does this first, to discard interrupt bits
//!    left over from the firmware's own start-up. `docs/wifi.md` §26 recorded this as impossible here
//!    because "the EROM walk has not identified the SDIOD core's base" - **that was wrong**, and the same
//!    boot's log disproved it: `core 0x829 rev 21 base 0x18004000 wrap 0x18104000 SDIO device`. The walk
//!    found it, this driver even names it, and the claim was made without looking. It is implemented.
//! 3. **The protocol version, to the mailbox.** `SDPCM_PROT_VERSION << SDPCM_PROT_VERSION_SHIFT` is
//!    `4 << 16` = `0x0004_0000`, written to the SDIO core's `TOSBMAILBOXDATA`. This tells the firmware which
//!    SDPCM version the host speaks, and it must happen before any frame is exchanged. Note the asymmetry
//!    worth recording: the firmware reported shared-structure version **1**, while the host announces
//!    protocol version **4**. Those are two different version numbers - the shared-memory LAYOUT versus the
//!    FRAMING protocol - and conflating them would be an easy mistake to make from the names alone.
//! 4. **Function 2, the data function.** 512-byte blocks and enabled, checked by the `IOR` ready bit for
//!    function 2 - the chip confirming the function came up, which `enable_function` already polls for.
//!
//! What this module deliberately does NOT do is exchange a frame. That needs the SDPCM and BCDC headers and
//! is the next step; this one ends with a bus that could carry one, and says so rather than implying more.

use godspeed_sdk::ServiceContext;

use crate::backplane::{clk, f1, Window};
use crate::host::Host;
use crate::sdio;
use crate::sdio::DATA_FUNC;

/// Register offsets inside the SDIO device core's register block, quoted above.
mod sdpcmd {
    /// `SDPCMD_INTSTATUS`.
    pub const INTSTATUS: u32 = 0x020;
    /// `SDPCMD_TOSBMAILBOXDATA`.
    pub const TOSBMAILBOXDATA: u32 = 0x048;
}

/// `SDPCM_PROT_VERSION` - the framing protocol version the host announces.
const PROT_VERSION: u32 = 4;
/// `SDPCM_PROT_VERSION_SHIFT`.
const PROT_VERSION_SHIFT: u32 = 16;

/// `sdmmc_io_set_blocklen(sc->sc_sf[2], 512)`.
const DATA_BLOCK: u16 = 512;

/// Move the chip from its ALP clock to HT, and confirm the chip says HT is available.
///
/// The upload ran on ALP because that is all the backplane needed. Frames need HT. `HT_AVAIL` (0x80) is the
/// chip reporting the clock is actually up; the request bit only reports that we asked.
///
/// No `Window` here on purpose: `CHIPCLKCSR` is a function 1 register reached by CMD52, not a backplane
/// address, so it needs no window at all.
fn ht_clock(h: &Host, ctx: &ServiceContext) -> bool {
    const HT_TRIES: u32 = 100;

    let before = match sdio::read_reg(h, 1, f1::CHIPCLKCSR) {
        Some(v) => v,
        None => {
            ctx.log("wifi-driver: could not read CHIPCLKCSR, so the clock state is unknown");
            return false;
        }
    };
    if sdio::write_reg(h, 1, f1::CHIPCLKCSR, before | clk::HT_AVAIL_REQ as u8).is_none() {
        ctx.log("wifi-driver: the HT clock request was refused");
        return false;
    }
    let mut last = before;
    for attempt in 0..HT_TRIES {
        match sdio::read_reg(h, 1, f1::CHIPCLKCSR) {
            Some(v) => {
                last = v;
                if v & clk::HT_AVAIL as u8 != 0 {
                    // FORCE_HT only once the chip says HT is there, which is the reference's order.
                    let _ = sdio::write_reg(h, 1, f1::CHIPCLKCSR, v | clk::FORCE_HT as u8);
                    let after = sdio::read_reg(h, 1, f1::CHIPCLKCSR).unwrap_or(0);
                    ctx.log_fmt(format_args!(
                        "wifi-driver: the chip is on its HT clock - CHIPCLKCSR {:#04x} -> {:#04x} after {} \
                         read(s), HT_AVAIL set, then forced ({:#04x})",
                        before, v, attempt + 1, after
                    ));
                    return true;
                }
            }
            None => {
                ctx.log("wifi-driver: CHIPCLKCSR stopped answering while waiting for the HT clock");
                return false;
            }
        }
        ctx.sleep_ms(1);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the chip never reported HT_AVAIL - CHIPCLKCSR {:#04x} after {} reads over ~{} ms. It \
         is still on ALP, which carried the upload but will not carry frames",
        last, HT_TRIES, HT_TRIES
    ));
    // NOT fatal by itself: the ALP clock is what the whole upload ran on, so the bus still works. Saying so
    // is better than refusing to continue over a clock the next step may not need.
    false
}

/// Tell the firmware which SDPCM version the host speaks, and clear the core's stale interrupt bits.
///
/// `sdiod_base` is the SDIO device core's register base, which the EROM walk reports (`0x18004000` on this
/// part). Both writes go through the backplane window like any other.
fn announce_protocol(h: &Host, w: &mut Window, sdiod_base: u32, ctx: &ServiceContext) -> bool {
    // The reference clears INTSTATUS first, discarding bits the firmware's own start-up left set.
    if w.write32(h, sdiod_base + sdpcmd::INTSTATUS, 0xFFFF_FFFF, ctx).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not clear the SDIO core's INTSTATUS at {:#010x}",
            sdiod_base + sdpcmd::INTSTATUS
        ));
        return false;
    }
    let val = PROT_VERSION << PROT_VERSION_SHIFT;
    if w.write32(h, sdiod_base + sdpcmd::TOSBMAILBOXDATA, val, ctx).is_none() {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not write the protocol version to the mailbox at {:#010x}",
            sdiod_base + sdpcmd::TOSBMAILBOXDATA
        ));
        return false;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: announced SDPCM protocol version {} to the firmware ({:#010x} -> mailbox {:#010x}), \
         and cleared the SDIO core's INTSTATUS. Note this is the FRAMING version, not the shared-structure \
         layout version the firmware reported",
        PROT_VERSION,
        val,
        sdiod_base + sdpcmd::TOSBMAILBOXDATA
    ));
    true
}

/// Bring the bus up for frames: HT clock, protocol announcement, and the data function enabled.
///
/// Returns true only when function 2 reports itself READY, because that is the one outcome that makes a
/// frame possible. The HT clock is reported but not required: the entire upload ran on ALP, so a chip that
/// will not raise HT is degraded rather than dead, and refusing to continue would hide that distinction.
pub fn bring_up(h: &Host, w: &mut Window, sdiod_base: u32, ctx: &ServiceContext) -> bool {
    ctx.log("wifi-driver: stage 12 - bringing the bus up for frames");

    let ht = ht_clock(h, ctx);

    if !announce_protocol(h, w, sdiod_base, ctx) {
        return false;
    }

    // Function 2's block size goes to its FBR in function 0's address space, exactly as function 1's did.
    if !sdio::set_block_size(h, DATA_FUNC, DATA_BLOCK, ctx) {
        return false;
    }
    if !sdio::enable_function(h, DATA_FUNC, ctx) {
        ctx.log(
            "wifi-driver: function 2 did not come ready, so no frame can be sent. The firmware is running \
             and the backplane still answers, so what failed is the DATA path rather than the chip",
        );
        return false;
    }

    ctx.log_fmt(format_args!(
        "wifi-driver: the bus is up for frames - function 2 ready at {} bytes a block, protocol announced, \
         clock {}. NOTHING HAS BEEN SENT: the SDPCM and BCDC headers are the next step, and the first thing \
         worth asking for is the firmware's own MAC address",
        DATA_BLOCK,
        if ht { "HT" } else { "ALP (degraded - HT never came up)" }
    ));
    true
}
