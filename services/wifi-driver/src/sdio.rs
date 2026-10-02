// SPDX-License-Identifier: GPL-2.0-only
//! The Broadcom side of SDIO bring-up. The protocol itself - CMD52, CMD53, identification, the CIS - is
//! the shared one (`godspeed_wifi::sdio`, re-exported below so every module keeps writing `sdio::...`).
//! What stays here is what is true of THIS chip: which function carries frames and at what block size,
//! and the decision a respawn makes about a card an earlier instance left behind - which turns on the
//! Broadcom firmware asserting function 2 ready.

use godspeed_sdk::ServiceContext;

pub use godspeed_wifi::sdio::*;

/// The data function. Function 1 is the backplane window; function 2 carries frames.
///
/// Declared HERE because it is a fact about the SDIO card, not about the bus bring-up or the
/// control protocol that both need it. It was declared in each of those and the duplicate-constant
/// gate refused it, correctly: one fact, one place.
pub const DATA_FUNC: u8 = 2;

/// Function 2's block size. Set on the card in `bus::bring_up`, and the threshold above which a
/// CMD53 must be asked for in BLOCK mode - byte mode's count field is nine bits, so it cannot
/// express more than this.
///
/// Declared HERE with `DATA_FUNC` because it is one fact about the card that three modules need.
/// It was `BLOCK` in `ctrl.rs` at 512 and `BLOCK` in `upload.rs` at 64 - function 2's and function
/// 1's - which the duplicate-constant gate refused as two facts wearing one name. It was right.
pub const DATA_BLOCK: u16 = 512;

/// Identify the card on the bus: CMD0, CMD5 twice, CMD3, CMD7.
///
/// Every step names itself on failure. That is not verbosity - on this board each one fails for a
/// different reason and they need different fixes. CMD5 timing out means the radio is not reachable at
/// all (its power domain, or the GPIO34-39 mux, both of which the kernel reports at boot). CMD5
/// answering but never becoming ready means the voltage window was refused. CMD3 failing after a ready
/// CMD5 means the card is listening and the bus is marginal.
pub fn identify(h: &dyn SdioHost, ctx: &ServiceContext) -> Option<Card> {
    // A CARD AN EARLIER INSTANCE LEFT RUNNING IS ADOPTED, NOT RESET. Six host-side resets were tried
    // on such a chip (`docs/wifi.md` 45, `backlog/69`) and each ended the same way: a firmware started
    // on a chip the host reset traps in the chip's ROM before it has a stack. Only a power cycle is a
    // power-on for this part, and only the kernel can drive that. But a kill of this SERVICE does
    // nothing to the CHIP - the firmware the dead instance loaded is still running, associated, alive -
    // and brcmfmac's resume-with-power-kept path re-attaches to exactly such a firmware without
    // touching the card (`mmc_sdio_resume` reinitialises nothing for a powered, non-removable card).
    //
    // A CMD52 read of the CCCR answers only on an initialised card: a fresh one is not selected and
    // stays silent, so silence is the boot's path. An initialised card with function 2 ENABLED and
    // READY was brought up by an earlier instance and its firmware asserts the ready bit, so that
    // firmware is alive: it is adopted. An initialised card WITHOUT function 2 belongs to an instance
    // that died before its firmware ran (mid-upload, ARM halted); its I/O side is reset (CCCR RES,
    // Linux's `sdio_reset`) and it is identified from CMD0 like a fresh card, because the state it is
    // in is the one the boot's own upload starts from.
    // REVISION ZERO IS NOT A CARD. The SDIO specification gives every card a nonzero CCCR format version
    // in this register (the CYW43455 says 0x32), and a host that answers a CMD52 read with zeros when
    // nothing is on the bus - QEMU's does, boot 2026-10-01 01:50 - would otherwise read as an initialised
    // card with nothing up, and be power-cycled for it. The fresh path below is what handles "nothing".
    if let Some(rev) = read_reg(h, 0, cccr::REVISION).filter(|r| *r != 0) {
        let ioe = read_reg(h, 0, cccr::IO_ENABLE).unwrap_or(0);
        let ior = read_reg(h, 0, cccr::IO_READY).unwrap_or(0);
        if ioe & 0x04 != 0 && ior & 0x04 != 0 {
            ctx.log_fmt(format_args!(
                "wifi-driver: an earlier instance left the card initialised (CCCR rev {:#04x}, IOE {:#04x}, IOR {:#04x}) with function 2 up - its firmware is running. ADOPTING it: no reset, no re-enumeration, no upload; the firmware is asked to answer further on",
                rev, ioe, ior));
            return Some(Card { rca: 0, funcs: 3, memory: false, ocr: 0, warm: true });
        }
        ctx.log_fmt(format_args!(
            "wifi-driver: an earlier instance left the card initialised (CCCR rev {:#04x}, IOE {:#04x}, IOR {:#04x}) but function 2 is not up - it died before its firmware ran",
            rev, ioe, ior));
        // POWER, where the machine has it. A chip the host can only reset is a chip whose ROM will not
        // boot a new firmware (docs/wifi.md 45); a chip whose power was cut is a chip as after power-on,
        // and the boot's own path handles that. The kernel minted this service `DEVICE_POWER` with its
        // window where the board can do this (docs/wifi.md 47); where it cannot, the SDK call returns
        // false and the CCCR RES path below is what remains.
        if crate::power_cycle_device(ctx, h) {
            ctx.log("wifi-driver: the radio was power-cycled - identifying it as a card just powered up");
            // The host was PARKED across the cut; CMD0 below needs a clock, and `identify_once` sets none.
            if !h.reset(ctx) {
                ctx.log("wifi-driver: the host did not come back from its park after the power cycle");
                return None;
            }
        } else if write_reg(h, 0, CCCR_IO_ABORT, CCCR_IO_ABORT_RES).is_none() {
            ctx.log("wifi-driver: no power control here, and the CCCR RES write was refused by a card that answers CMD52 - identifying anyway");
        } else {
            ctx.log("wifi-driver: no power control here - the I/O side is reset (CCCR RES) and identification starts from CMD0; a firmware that ran before will not boot again this way (docs/wifi.md 45)");
        }
    }
    identify_once(h, ctx)
}

