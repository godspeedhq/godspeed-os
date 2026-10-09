// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK's audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a grep cannot
// - unsafe produced by a macro, or spelled across lines. `deny` rather than `forbid` for exactly one
// reason: the exported `service_main` symbol needs `#[allow(unsafe_code)]`, because a `#[no_mangle]`
// declaration is itself covered by this lint (a colliding symbol is a soundness hole). `forbid` cannot
// be relaxed even there.
#![deny(unsafe_code)]
//! `wifi-driver` - a board's onboard radio, as a userspace service (`docs/wifi.md`).
//!
//! The kernel grants it the radio's SDIO host registers at spawn by device KIND (`WIFI_SDIO`), only where
//! its boot census saw the controller answer, and `DEVICE_POWER` over the radio's enable pin where the board
//! can drive it.
//! Where no window is granted (QEMU, a board with no radio) it says so and serves `no radio`.
//!
//! **Raspberry Pi 4 (aarch64): the Broadcom CYW43455, on the Arasan host (`host.rs`).** It identifies the
//! card, uploads the firmware and CLM blob (`upload.rs`, `firmware.rs`) while holding a lease on the Arm
//! clock from `power` (the chip's firmware traps when uploaded with the cores slow), and then serves the
//! shell's `wifi`: scans, WPA2 joins with the four-way handshake run on the host (`join.rs`; the firmware
//! has no supplicant), pairwise and group rekeys, and the radio's power through `DevicePower` - `radio off
//! hard`, `powercycle`, and the recovery of a firmware that has stopped. It serves `nic-driver` the frame
//! path (`frames.rs`) for when the cable is out. A respawn ADOPTS a firmware the dead instance left
//! running rather than reloading it, and rejoins from `/wifi.keys`. A radio that cannot be brought up is
//! served as `radio down` with its reason (`wire::DOWN_*`) - answered loudly, never left to time out.
//!
//! **VisionFive 2 Lite (riscv64, `wifi_host_dw_mmc`): the AIC8800, phases V0-V6** (`docs/wifi-aic8800.md`).
//! `dwmmc.rs` drives the DesignWare host; the radio is power-cycled and identified, the firmware uploaded
//! and started (`aic.rs`), and the station served through the shared loop (`aic_station.rs`). A bring-up
//! that stops short answers `radio down` with the reason `DOWN_NOT_BUILT`.
//!
//! ## Reference
//!
//! Written from the SDIO specification's identification sequence and the behaviour of Linux's
//! `drivers/mmc/core/sdio.c` and `drivers/mmc/host/sdhci-iproc.c`, read as executable datasheets
//! (§26.14): what the silicon needs written, in what order, and what it does when you get it wrong. No
//! code is taken from either. The register-level lessons about this specific controller come from
//! `services/block-driver/src/sdhci.rs`, which drives the same Arasan block on the Pi 2.

#![no_std]
#![no_main]

mod aicore;
mod armcr4;
mod backplane;
mod bcm;
mod bus;
#[cfg(wifi_host_dw_mmc)]
mod aic;
#[cfg(wifi_host_dw_mmc)]
mod aic_fw;
#[cfg(wifi_host_dw_mmc)]
mod aic_station;
#[cfg(wifi_host_dw_mmc)]
mod aic_wire;
#[cfg(wifi_host_dw_mmc)]
mod dwmmc;
mod ctrl;
mod frames;
mod scan;
mod firmware;
mod erom;
mod host;
mod join;
mod sdio;
mod upload;

use godspeed_sdk::{Message, ServiceContext};
// The chip-independent half, shared with every radio driver (`sdk/wifi`). Imported at the root so the
// modules here keep writing `crate::crypto` and friends.
use godspeed_wifi::eapol;
use godspeed_wifi::sdio::SdioHost;
use godspeed_wifi::station::Station;

/// Once identification is over, this is the clock to run at.
///
/// 25 MHz is SDIO default speed - the mode any card must support without a high-speed negotiation this
/// driver does not perform. Raising it is a later phase's business, and asking for a mode we have not
/// enabled is how a working bus becomes an intermittent one.
const OPERATING_HZ: u32 = 25_000_000;

// (`serve_unavailable` and `serve_unavailable_why`, below, are the serve-with-no-radio loop: every request
// is answered "radio down", loudly and at once, so a shell that asks gets a fact rather than a timeout.
// Answering matters more than what is answered - a service that recv's and never replies leaves its caller
// waiting out a deadline, and one that never recv's sits at 16/16 on its queue forever. `recv` BLOCKS, so
// the loop costs nothing while nobody is calling. Commandment VIII: a dependency that cannot do the thing
// RETURNS with a loud unavailable, never hangs.)

/// How long WL_REG_ON is held low by a power cycle the driver makes for itself: two seconds (2026-10-01).
/// Fifty milliseconds was tried first and was MARGINAL: of two power cycles on hardware one gave a cold
/// chip and one a chip whose SDIO side had reset but whose firmware then trapped at start with the
/// section-45 signature - reset, not power-cycled. The pin is driven by the VideoCore over I2C at its own
/// pace, and the chip's internal supplies take time to drain. 50 ms, 500 ms, 2 s and 75 s all produced warm
/// starts, so the hold-off is not the variable; this only has to exceed the chip's own discharge.
const POWER_OFF_MS: u64 = 2_000;
/// How long after WL_REG_ON goes high before the SDIO side is asked anything.
///
/// 300 ms. Five seconds was tried (2026-10-01) to give the chip the time on that boot gives it, and changed
/// nothing - docs/wifi.md 52 - so the reasoning below is the experiment's, kept as its record.
/// At boot the VideoCore raises WL_ON seconds before this driver's first command, and boot always comes
/// up cold; after a cycle the driver started 300 ms after, and came up warm almost every time. The OFF time
/// was varied from 50 ms to 75 s and never mattered; the ON time never was varied. Linux's four pre-download
/// steps changed nothing, and the chip's registers read identically cold and warm (docs/wifi.md 52), which
/// leaves the chip's own power-on initialisation, still running when the driver halts it, as the suspect.
const POWER_ON_SETTLE_MS: u64 = 300;

/// Cut the radio's power and restore it, through the kernel's `DevicePower` (docs/wifi.md 47). The
/// two waits are the DEVICE'S - WL_REG_ON low long enough for the CYW43455 to lose its state, then the
/// chip's own power-on before its SDIO side answers - and live here and not in the kernel because they
/// are facts about this chip, not about power (26.10). `false` means the kernel has no control over
/// this device's power on this machine - the caller then does what it can without.
pub(crate) fn power_cycle_device(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    power_cycle_device_ms(ctx, h, POWER_OFF_MS)
}

/// The three settings Linux's brcmfmac writes after making the cores passive and before downloading the
/// firmware (`brcmf_sdio_probe_attach`, read 2026-10-01). Each is read back and said; none is fatal.
fn linux_pre_download(h: &dyn SdioHost, w: &mut backplane::Window, cores: Option<&erom::Cores>, ctx: &ServiceContext) {
    // KSO, keep-SDIO-on (`brcmf_sdio_kso_init`): SDIO device core rev >= 12 only; F1 SLEEPCSR bit 0.
    const SLEEPCSR: u32 = 0x1_001F;
    const KSO_EN: u8 = 0x01;
    let sdio_rev = cores.and_then(|c| c.sdiod).map(|c| c.rev).unwrap_or(0);
    if sdio_rev >= 12 {
        match sdio::read_reg(h, 1, SLEEPCSR) {
            Some(v) if v & KSO_EN != 0 => ctx.log_fmt(format_args!("wifi-driver: KSO already set (SLEEPCSR {:#04x})", v)),
            Some(v) => {
                let ok = sdio::write_reg(h, 1, SLEEPCSR, v | KSO_EN).is_some();
                let after = sdio::read_reg(h, 1, SLEEPCSR);
                ctx.log_fmt(format_args!("wifi-driver: KSO set (SLEEPCSR {:#04x} -> {:?}, write {})", v, after, if ok { "ok" } else { "refused" }));
            }
            None => ctx.log("wifi-driver: KSO - SLEEPCSR did not answer"),
        }
    } else {
        ctx.log_fmt(format_args!("wifi-driver: KSO skipped - SDIO core rev {} is below 12", sdio_rev));
    }
    // CCCR_BRCM_CARDCTRL (F0 0xF1) |= WLANRESET: "so an SDIO card reset does a WLAN backplane reset".
    const CARDCTRL: u32 = 0xF1;
    const WLANRESET: u8 = 0x02;
    match sdio::read_reg(h, 0, CARDCTRL) {
        Some(v) => {
            let ok = sdio::write_reg(h, 0, CARDCTRL, v | WLANRESET).is_some();
            ctx.log_fmt(format_args!("wifi-driver: CARDCTRL {:#04x} -> {:?} (WLANRESET, write {})",
                v, sdio::read_reg(h, 0, CARDCTRL), if ok { "ok" } else { "refused" }));
        }
        None => ctx.log("wifi-driver: CARDCTRL did not answer"),
    }
    // PMU pmucontrol |= RES_RELOAD << RES_SHIFT: "so a backplane reset does PMU state reload". No separate
    // PMU core in this chip's table, so pmucontrol is chipcommon's, at 0x18000600.
    const PMUCONTROL: u32 = 0x1800_0600;
    const RES_RELOAD: u32 = 0x2 << 13;
    match w.read32(h, PMUCONTROL, ctx) {
        Some(v) => {
            let ok = w.write32(h, PMUCONTROL, v | RES_RELOAD, ctx).is_some();
            ctx.log_fmt(format_args!("wifi-driver: PMU control {:#010x} -> {:?} (RES_RELOAD, write {})",
                v, w.read32(h, PMUCONTROL, ctx), if ok { "ok" } else { "refused" }));
        }
        None => ctx.log("wifi-driver: PMU control did not answer"),
    }
}

/// How long the Arm clock lease asks for. The bring-up through the upload and the first scan takes about
/// 6 s with the cores fast; the margin is for a slow card, and `power` caps any lease at 30 s anyway.
const CLOCK_LEASE_SECS: u8 = 20;
/// How long to wait for `power` to answer. It answers from memory, so this is a dead-or-alive bound.
const POWER_SECS: i64 = 2;

/// Ask `power` to hold the Arm clock at its maximum while this instance loads the chip (`docs/power.md`).
///
/// WHY: the CYW43455's firmware traps at start when it is uploaded with the cores at their minimum clock -
/// every load after the Pi firmware's first minute did, every load inside it did not (`docs/wifi.md` 55).
/// The mechanism is not known; the dependence is measured, so it is asked for rather than assumed.
///
/// OPTIONAL by design: a `power` that is absent, dead or refusing costs one line and the load goes ahead at
/// whatever the clock is - the same as before this existed. `gs::call::request_within` does the one retry
/// that is safe - reacquire by name and resend when the send itself failed (a `power` respawned since this
/// driver was wired, Commandment IX) - and never resends after a deadline.
///
/// A deadline is `OutcomeUnknown`, which here means a lease MAY be open with no id to release it by. That
/// is said rather than hidden, and it is bounded: the lease expires on its own within 30 s.
fn clock_lease(ctx: &ServiceContext) -> Option<u8> {
    let msg = Message::from_bytes(&[1, CLOCK_LEASE_SECS]);
    let r = match godspeed::call::request_within(ctx, "power", &msg, POWER_SECS) {
        Ok(r) => r,
        Err(godspeed::Error::OutcomeUnknown) => {
            ctx.log("wifi-driver: `power` did not answer the clock lease in time - a lease may be open with no id to release it by, and expires on its own; loading at whatever the Arm clock is");
            return None;
        }
        Err(e) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: no clock lease ({}) - loading at whatever the Arm clock is (docs/power.md)", e.as_str()));
            return None;
        }
    };
    let p = r.payload_bytes();
    match p.first().copied() {
        Some(0) if p.len() >= 6 => {
            let hz = u32::from_le_bytes([p[2], p[3], p[4], p[5]]);
            if hz > 0 {
                ctx.log_fmt(format_args!("wifi-driver: Arm clock held at {} MHz for the load (lease {})", hz / 1_000_000, p[1]));
            } else {
                ctx.log_fmt(format_args!("wifi-driver: Arm clock lease {} joined one already open", p[1]));
            }
            Some(p[1])
        }
        Some(2) => {
            ctx.log("wifi-driver: this machine gives the OS no control over its clock - loading at whatever it is");
            None
        }
        other => {
            ctx.log_fmt(format_args!("wifi-driver: `power` refused the clock lease ({:?}) - loading at whatever the Arm clock is", other));
            None
        }
    }
}

/// Hand the lease back. A failure here is said and otherwise harmless: the lease expires on its own.
fn clock_release(ctx: &ServiceContext, lease: Option<u8>) {
    let Some(id) = lease else { return };
    let msg = Message::from_bytes(&[2, id]);
    if let Err(e) = godspeed::call::request_within(ctx, "power", &msg, POWER_SECS) {
        ctx.log_fmt(format_args!(
            "wifi-driver: could not hand clock lease {} back to `power` ({}) - it expires on its own", id, e.as_str()));
    }
}

/// After a cut: is the chip really unpowered? It waits for the rail to fall, brings the host back long
/// enough for one CMD52, and parks it again. `true` means the chip still answers - the cut did NOT take.
fn chip_still_answers(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    const RAIL_FALL_MS: u64 = 50;
    godspeed::task::sleep_ms(ctx, RAIL_FALL_MS);
    let answers = h.reset(ctx) && sdio::initialised_card_answers(h);
    h.park(ctx);
    answers
}

/// The verdict on a hard off, said in the log and returned for reply byte 3.
fn verify_hard_off(ctx: &ServiceContext, h: &dyn SdioHost) -> u8 {
    if chip_still_answers(ctx, h) {
        ctx.log("wifi-driver: `wifi radio off hard` - the pin was driven low but the chip STILL ANSWERS on its bus: its power did not go off");
        scan::reply::OFF_CONTRADICTED
    } else {
        ctx.log("wifi-driver: `wifi radio off hard` - verified: the chip no longer answers on its bus");
        scan::reply::OFF_VERIFIED
    }
}

/// Restore the chip's power the way a board power-on would: with the host PARKED (`host::Host::park`),
/// nothing driving the SDIO lines at the rising edge of WL_REG_ON, where the chip samples its boot straps.
/// The park is repeated here although the cut already parked it, so the edge is quiet whatever touched the
/// host while the power was off. The host is left parked: its next user brings it back from reset after
/// this delay, which is Linux's order (power first, the init clock after). `false` means the kernel refused.
/// (docs/wifi.md 48: the park did not by itself make the start cold.)
pub(crate) fn power_on_device(ctx: &ServiceContext, h: &dyn SdioHost) -> bool {
    h.park(ctx);
    if !ctx.device_power(true) {
        return false;
    }
    godspeed::task::sleep_ms(ctx, POWER_ON_SETTLE_MS);
    true
}

/// `power_cycle_device` with the hold-off chosen by the caller (`wifi radio powercycle` asks for a fixed
/// 2 s): the shell decides how long, this decides how.
pub(crate) fn power_cycle_device_ms(ctx: &ServiceContext, h: &dyn SdioHost, off_ms: u64) -> bool {
    if !ctx.device_power(false) {
        return false;
    }
    // THE HOST GOES QUIET FOR THE WHOLE OFF WINDOW (docs/wifi.md 48). Every earlier cycle left the card
    // clock toggling into the unpowered chip from here to the power-on. Parked after the cut is accepted,
    // not before: a refused cut then leaves the host and any live session untouched, and the gap between
    // the pin going low and the park is the mailbox's return - well under a millisecond of a long quiet.
    // Tried as the warm-start fix and it was not one (one cold start in three loads, docs/wifi.md 48); kept,
    // because a quiet host across the edge is still the reference's order.
    h.park(ctx);
    godspeed::task::sleep_ms(ctx, off_ms);
    if !power_on_device(ctx, h) {
        ctx.log("wifi-driver: the radio's power was cut and could NOT be restored - the kernel refused the second request");
        return false;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the radio's power was held off for {} ms and given {} ms to come up",
        off_ms, POWER_ON_SETTLE_MS));
    true
}

/// The hold-off a `[OP_RADIO, RADIO_POWERCYCLE, units]` request asks for: `units` (its byte 2) of 100 ms, 0
/// meaning the driver's default. Bounded above so a stray byte cannot hold the radio off for half a minute.
fn requested_off_ms(units: u8) -> u64 {
    match units {
        0 => POWER_OFF_MS,
        u => (u as u64 * 100).min(20_000),
    }
}

/// The VisionFive 2 Lite's radio, phases V0 to V6 (`docs/wifi-aic8800.md`): the grant proven, the card
/// IDENTIFIED - CMD5 answered, its function count, and the manufacturer and device codes read out of its own
/// CIS - then the firmware uploaded and started, a station interface brought up, one scan, and the radio
/// served under `serve_radio`. A bring-up that stops short answers `radio down` with `DOWN_NOT_BUILT`.
///
/// The power-up is the vendor glue's (`aic8800_bsp`): the enable LOW for 10 ms, HIGH, 10 ms before the
/// first command - with the host's card clock stopped across the edge, so the card powers up into a quiet
/// bus. The hold-offs are the device's and live here, not in the kernel (26.10).
#[cfg(wifi_host_dw_mmc)]
fn v1_dw_mmc(ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio) -> ! {
    use godspeed_wifi::sdio as sd;
    const POWER_HOLD_MS: u64 = 10;
    let h = dwmmc::Host::new(ctx, mmio);
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 2 (dw_mmc) - VERID={:#010x} HCON={:#010x} through the grant (the kernel's census read these same registers)",
        h.verid(), h.hcon()));
    let why = scan::reply::DOWN_NOT_BUILT;
    h.park(ctx);
    let off = ctx.device_power(false);
    godspeed::task::sleep_ms(ctx, POWER_HOLD_MS);
    let on = ctx.device_power(true);
    godspeed::task::sleep_ms(ctx, POWER_HOLD_MS);
    ctx.log_fmt(format_args!(
        "wifi-driver: radio power cycled for a clean power-up - off {}, on {}",
        if off { "confirmed" } else { "REFUSED" }, if on { "confirmed" } else { "REFUSED" }));
    if !h.reset(ctx) {
        ctx.log("wifi-driver: the dw_mmc host did not come up (reset or clock update never completed), so nothing was sent to the card");
        serve_unavailable_why(ctx, Some(&h), why)
    }
    // ---- Stage 3: what is on the bus. -------------------------------------------------------------
    let Some(card) = sd::identify_once(&h, ctx) else {
        ctx.log("wifi-driver: no SDIO card identified on the VisionFive's radio bus - the lines above name the command that failed and the host's interrupt word");
        serve_unavailable_why(ctx, Some(&h), why)
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 3 - an SDIO card answered: {} I/O function(s), memory {}, OCR {:#010x}, RCA {:#06x}",
        card.funcs, card.memory, card.ocr, card.rca));
    sd::report_cccr(&h, ctx);
    // ---- Stage 4: ask the PART what it is. --------------------------------------------------------
    let is_d80 = match sd::cis_pointer(&h, ctx).and_then(|p| sd::walk_cis(&h, p, ctx)) {
        Some(m) => {
            const AIC_VENDOR: u16 = 0xC8A1;
            const AIC8800D80: u16 = 0x0082;
            let ok = m.manf == AIC_VENDOR && m.device == AIC8800D80;
            ctx.log_fmt(format_args!(
                "wifi-driver: stage 4 - the card's CIS says manufacturer {:#06x}, device {:#06x}{}",
                m.manf, m.device,
                if ok {
                    " - an AICSemi AIC8800D80, as the board's vendor image said. V1 done"
                } else {
                    " - NOT the AIC8800D80 (C8A1:0082) the design expects; stopping here"
                }));
            ok
        }
        None => {
            ctx.log("wifi-driver: stage 4 - the card answered but its CIS could not be walked to a MANFID tuple");
            false
        }
    };
    // ---- Stage 5 (V2, first exchange): one message to the chip's ROM and its answer. -------------------
    let rev = if is_d80 {
        match aic::first_exchange(&h, ctx) {
            Some(w) => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: stage 5 - the chip's ROM answered: {:#010x} at {:#010x}, chip revision {} ({}){}",
                    w, aic::CHIP_ID_ADDR, (w >> 16) & 0x3f,
                    match (w >> 16) & 0x3f { 1 => "U01", 3 => "U02", 7 => "U03", _ => "not one the vendor driver names" },
                    if (w >> 16) & 0xc0 == 0xc0 { ", the H variant" } else { "" }));
                Some((w >> 16) & 0xff)
            }
            None => {
                ctx.log("wifi-driver: stage 5 - the first message to the chip's ROM got no confirm; the lines above say which step");
                None
            }
        }
    } else {
        None
    };
    // ---- Stage 6 (V2, second card): the three patches, to where the patch table says. ----------------
    // The `u02` files serve revisions 3 and 7 of the non-H part (`aicbsp_driver_fw_init`); anything else
    // would need files this directory does not carry, so it is not attempted.
    if matches!(rev, Some(3) | Some(7)) && aic_fw::verify(ctx) {
        if let Some(pi) = aic::patch_info(aic_fw::TABLE, ctx) {
            let parts: [(&str, u32, &[u8]); 3] =
                [("ADID", pi.adid, aic_fw::ADID), ("patch", pi.patch, aic_fw::PATCH), ("ext0", pi.ext0, aic_fw::EXT0)];
            let mut ok = true;
            for (what, addr, bytes) in parts {
                if !aic::upload(&h, what, addr, bytes, ctx) || !aic::check_first_word(&h, what, addr, bytes, ctx) {
                    ok = false;
                    break;
                }
            }
            ctx.log(if ok {
                "wifi-driver: stage 6 - the ADID, patch and extension patch are in the chip's memory where its table says"
            } else {
                "wifi-driver: stage 6 - the patch upload stopped; the line above names the block"
            });
            // ---- Stage 7 (V2, third card): the table's writes, fmacfw, its patch configuration, start. --
            // In the vendor driver's order: `aicbt_patch_table_load` after the patches, then
            // `aicwifi_init`'s upload, `aicwifi_patch_config_8800d80` and `aicwifi_start_from_bootrom`.
            if ok {
                let started = aic::table_writes(&h, aic_fw::TABLE, ctx)
                    && aic::upload(&h, "fmacfw", aic::FMAC_ADDR, aic_fw::FMAC, ctx)
                    && aic::check_first_word(&h, "fmacfw", aic::FMAC_ADDR, aic_fw::FMAC, ctx)
                    && aic::patch_config(&h, aic_fw::FMAC, ctx)
                    && aic::start_app(&h, ctx).is_some();
                ctx.log(if started {
                    "wifi-driver: stage 7 - fmacfw is uploaded, configured and STARTED"
                } else {
                    "wifi-driver: stage 7 - the firmware was not started; the line above names the step"
                });
                // ---- Stage 8 (V3, first card): the running firmware's bring-up, to its version and MAC. ----
                if started {
                    match aic::bring_up(&h, ctx) {
                        Some(f) => {
                            let m = f.mac;
                            ctx.log_fmt(format_args!(
                                "wifi-driver: stage 8 - the firmware answers: version \"{}\", MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, 5 GHz {}",
                                core::str::from_utf8(&f.version[..f.version_len]).unwrap_or("(not text)"),
                                m[0], m[1], m[2], m[3], m[4], m[5],
                                if f.five_ghz { "yes" } else { "no" }));
                            // ---- Stage 9 (V3, second card): configured, started, one station interface. ----
                            match aic::bring_up_station(&h, f.mac, f.five_ghz, ctx) {
                                Some(i) => {
                                    ctx.log_fmt(format_args!(
                                        "wifi-driver: stage 9 - the firmware is up with a station interface (index {}) at its own MAC. V3 done",
                                        i.index));
                                    // ---- Stage 10 (V4, first card): one scan, logged. Not yet `wifi scan` - that is
                                    // the serve loop's, through a `Station`, and comes once this has worked. --------
                                    let mut scan = godspeed_wifi::bss::Scan::new();
                                    let e = aic::scan_once(&h, i.index, f.five_ghz, &mut scan, godspeed::driver::wait::Budget::ms(15_000), ctx);
                                    for n in scan.networks() {
                                        let b = n.bssid;
                                        ctx.log_fmt(format_args!(
                                            "wifi-driver:   {:<32}  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  ch {:>3}  {:>4} dBm  security {}",
                                            core::str::from_utf8(&n.ssid[..n.ssid_len as usize]).unwrap_or("(not text)"),
                                            b[0], b[1], b[2], b[3], b[4], b[5], n.chanspec, n.rssi, n.security));
                                    }
                                    match e.ended {
                                        Some((status, count)) => ctx.log_fmt(format_args!(
                                            "wifi-driver: stage 10 - the scan ENDED (status {}, the firmware counts {} result(s)) after {} ms: {} indication(s), {} network(s), {} unreadable, {} not kept",
                                            status, count, e.ms, e.results, scan.count(), e.unreadable, scan.dropped)),
                                        None => ctx.log_fmt(format_args!(
                                            "wifi-driver: stage 10 - the scan did not end within {} ms: {} indication(s), {} network(s) so far, {} unreadable",
                                            e.ms, e.results, scan.count(), e.unreadable)),
                                    }
                                    // ---- Stage 11 (V4, second card): the radio as a `Station`, under the serve loop
                                    // the Pi 4's runs under - `wifi scan`, `wifi join`, `/wifi.keys` and the frame
                                    // path are the loop's from here (`aic_station.rs`). It does not return. ----
                                    ctx.log("wifi-driver: stage 11 - the AIC8800 is a station; serving `wifi` and the frame path");
                                    let mut station = aic_station::Aic::new(&h, i.index, &f);
                                    serve_radio(ctx, &h, Some(&mut station as &mut dyn Station), scan::reply::DOWN_NO_RADIO, "AIC8800D80");
                                }
                                None => ctx.log("wifi-driver: stage 9 - the station bring-up stopped; the line above names the message"),
                            }
                        }
                        None => ctx.log("wifi-driver: stage 8 - the running firmware's bring-up stopped; the line above names the message"),
                    }
                }
            }
        }
    }
    ctx.log("wifi-driver: the AIC8800 did not come up as far as a station interface - the line above names the step - so this answers `radio down`, reason 4 (DOWN_NOT_BUILT)");
    serve_unavailable_why(ctx, Some(&h), why)
}

fn serve_unavailable(ctx: &ServiceContext, h: Option<&dyn SdioHost>) -> ! {
    serve_unavailable_why(ctx, h, scan::reply::DOWN_NO_RADIO)
}

/// `serve_unavailable`, saying WHY the radio is down in the status answer (`wire::DOWN_*`).
fn serve_unavailable_why(ctx: &ServiceContext, h: Option<&dyn SdioHost>, why: u8) -> ! {
    // `wifi radio off hard` from here leaves the chip powered down, and from then on this loop answers the
    // way `serve_radio`'s powered-off arms do, so the shell sees one shape for one state whichever loop
    // holds it. `h` is `None` only where no SDIO window was granted, and there no power op can be made.
    let mut powered_off = false;
    // The status reply's size, and room for `wifi hardware onboard`'s facts beside it.
    const STATUS_LEN: usize = 30 + join::MAX_SSID;
    let mut out = [0u8; 256];
    loop {
        let req = godspeed::ipc::recv(ctx);
        // No reply cap means there is nothing to answer on, and dropping is all that is left.
        let Some(reply) = godspeed::ipc::take_sent_cap(ctx) else { continue };
        // The reply cap is RECLAIMED after use (26.6): see the serve loop for what not doing so cost.
        // One byte at least, not an empty message: the kernel refused a zero-length send on three ports
        // until `e3fcf7ed`, so an "empty reply" was no reply at all; a byte says what happened.
        let (tag, p) = godspeed_wifi::serve::untag(req.payload_bytes());
        // THIS LOOP IS REACHED when the radio never got as far as its firmware - no SDIO window, the host
        // failed, no card on the bus even after the power was asserted, a backplane that would not open. A
        // firmware that trapped at start does NOT come here: it goes to `serve_radio` with DOWN_TRAPPED.
        //
        // THE POWER OPS ARE SERVED FOR REAL, because they are the way out of this state and the chip's
        // power is still this driver's to command. `off hard` was answered RADIO_DOWN here until
        // 2026-10-02, and the shell read that as a driver that could not act and advised `kill
        // wifi-driver` - wrong advice for a cut this loop could have made (docs/wifi.md 49). Reply shapes
        // match `serve_radio`'s; everything else stays "radio down".
        let op = p.first().copied().unwrap_or(0);
        let mode = p.get(1).copied().unwrap_or(1);
        out[..5].fill(0);
        let n = if op == godspeed_wifi::wire::OP_HARDWARE {
            // `wifi hardware` (`wire::OP_HARDWARE`), the serve loop's shape: the bus is known, and the chip
            // is NOT - this loop is reached before one was identified, so naming the board's usual part
            // would be a guess.
            let mut at = 1;
            out[0] = scan::reply::OK;
            for text in ["not identified", "SDIO"] {
                out[at] = text.len() as u8;
                out[at + 1..at + 1 + text.len()].copy_from_slice(text.as_bytes());
                at += 1 + text.len();
            }
            at
        } else if op == godspeed_wifi::wire::OP_HARDWARE_DETAIL {
            // `wifi hardware onboard` here, in the serve loop's shape: what is known before a chip is
            // identified - the bus as this driver sets it up, and why there is no firmware.
            out[0] = scan::reply::OK;
            let (count, len) = {
                let mut d = godspeed_wifi::serve::Details::new(&mut out[2..]);
                d.add("chip", format_args!("not identified - the bring-up stopped before one was"));
                d.add("firmware", format_args!("not running - {}",
                    if powered_off { "the chip is powered down" } else { "the radio is down" }));
                d.add("bus", format_args!("SDIO, card clock asked {} MHz, function {} at {}-byte blocks",
                    OPERATING_HZ / 1_000_000, sdio::DATA_FUNC, sdio::DATA_BLOCK));
                d.done()
            };
            out[1] = count;
            2 + len
        } else if op == scan::reply::OP_STATUS && powered_off {
            // The powered-down status `serve_radio` gives: all zero, the trailing power byte included.
            out[..STATUS_LEN].fill(0);
            out[0] = scan::reply::OK;
            STATUS_LEN
        } else if op == scan::reply::OP_RADIO && mode == scan::reply::RADIO_POWERCYCLE && (powered_off || why != scan::reply::DOWN_NOT_BUILT) {
            // A radio this driver does not drive yet (`DOWN_NOT_BUILT`) is NOT cycled while it is powered:
            // the respawn would identify it again and come back down for the same reason, so the request
            // falls through to the RADIO_DOWN answer below and the shell says why. From `off hard` it IS
            // cycled, since restoring a cut power is what the operator asked for.
            // The hold-off is the caller's (the shell asks for a fixed 2 s).
            let cycled = match h {
                Some(h) => power_cycle_device_ms(ctx, h, requested_off_ms(p.get(2).copied().unwrap_or(0))),
                None => false,
            };
            ctx.log(if cycled {
                "wifi-driver: `wifi radio powercycle` on a radio that is down - the chip's power was cut and restored; this instance expects to be restarted onto the cold chip"
            } else {
                "wifi-driver: `wifi radio powercycle` on a radio that is down, and this machine has no control over the radio's power - nothing changed"
            });
            if cycled {
                powered_off = false;
            }
            out[0] = if cycled { scan::reply::OK } else { scan::reply::NO_POWER_CONTROL };
            out[2] = cycled as u8;
            5
        } else if op == scan::reply::OP_RADIO && powered_off {
            if mode == 0 || mode == scan::reply::RADIO_HARD_OFF {
                // Already as asked.
                out[0] = scan::reply::OK;
            } else if h.is_some_and(|h| power_on_device(ctx, h)) {
                powered_off = false;
                ctx.log("wifi-driver: `wifi radio on` on a powered-down chip - the power is restored; this instance has no firmware to serve and expects to be restarted onto the cold chip");
                out[0] = scan::reply::OK;
                out[2] = 1;
                out[3] = scan::reply::COLD_START;
            } else {
                ctx.log("wifi-driver: `wifi radio on` on a powered-down chip, and the kernel refused to restore the power");
                out[0] = scan::reply::NO_POWER_CONTROL;
            }
            5
        } else if op == scan::reply::OP_RADIO && mode == scan::reply::RADIO_HARD_OFF {
            match h {
                Some(h) if ctx.device_power(false) => {
                    h.park(ctx);
                    powered_off = true;
                    ctx.log("wifi-driver: `wifi radio off hard` on a radio that is down - the chip's power is cut and stays cut until `wifi radio on`");
                    out[0] = scan::reply::OK;
                    out[2] = 1;
                    out[3] = verify_hard_off(ctx, h);
                }
                _ => {
                    ctx.log("wifi-driver: `wifi radio off hard` asked, and this machine has no control over the radio's power - the radio stays as it was");
                    out[0] = scan::reply::NO_POWER_CONTROL;
                }
            }
            5
        } else if powered_off {
            out[0] = scan::reply::RADIO_POWERED_OFF;
            1
        } else {
            out[0] = scan::reply::RADIO_DOWN;
            out[1] = why;
            2
        };
        let _ = godspeed::ipc::reply(ctx, reply, &godspeed_wifi::serve::tagged_reply(tag, &out[..n]));
    }
}

/// The shared serve loop (`godspeed_wifi::serve`), run over this driver's radio. The loop is every
/// radio's - the Broadcom's, the AIC8800's and `wifi-usb`'s dongle - and lived here until the third one;
/// what stays here is what only this driver has: the chip's power, through the kernel's `DevicePower`
/// and the SDIO host it parks across the edge.
fn serve_radio(
    ctx: &ServiceContext,
    h: &dyn SdioHost,
    mut radio: Option<&mut dyn Station>,
    down_reason: u8,
    chip: &'static str,
) -> ! {
    let mut host = SdioPower { h, chip };
    // Once, where the loop used to run it on entry: the key-derivation primitives against their vectors.
    let crypto_ok = godspeed_wifi::crypto::selftest(ctx, "wifi-driver");
    loop {
        godspeed_wifi::serve::serve(ctx, "wifi-driver", radio.as_deref_mut(), &mut host, down_reason, crypto_ok);
        // Only a host that sends notices can end the loop, and this one sends none.
        ctx.log("wifi-driver: the serve loop returned, which a radio on the board never asks it to - serving again");
    }
}

/// The chip's power, as the serve loop asks for it: `DevicePower` for the pin, and the SDIO host parked
/// whenever the power is cut, so nothing drives the lines into an unpowered chip (docs/wifi.md 48).
struct SdioPower<'a> {
    h: &'a dyn SdioHost,
    /// The chip on this board's SD host, for `wifi hardware`: said by the caller, which is the code that
    /// identified it.
    chip: &'static str,
}

impl godspeed_wifi::serve::Host for SdioPower<'_> {
    fn hardware(&self) -> godspeed_wifi::serve::Hardware {
        godspeed_wifi::serve::Hardware { chip: self.chip, bus: "SDIO" }
    }
    // `wifi hardware onboard`: the chip as identified and the bus as this driver set it up - the card clock
    // it asks for and the block size of the function that carries frames.
    fn details(&self, d: &mut godspeed_wifi::serve::Details) {
        d.add("chip", format_args!("{}", self.chip));
        d.add("bus", format_args!("SDIO, card clock asked {} MHz, function {} at {}-byte blocks",
            OPERATING_HZ / 1_000_000, sdio::DATA_FUNC, sdio::DATA_BLOCK));
    }
    fn can_cut_power(&self) -> bool {
        // The pin is reached through the kernel's `DevicePower`; whether it is granted is asked when cutting.
        true
    }
    fn cut_power(&mut self, ctx: &ServiceContext) -> bool {
        if !ctx.device_power(false) {
            return false;
        }
        self.h.park(ctx);
        true
    }
    fn verify_off(&mut self, ctx: &ServiceContext) -> u8 {
        verify_hard_off(ctx, self.h)
    }
    fn restore_power(&mut self, ctx: &ServiceContext) -> bool {
        power_on_device(ctx, self.h)
    }
    fn power_cycle(&mut self, ctx: &ServiceContext, units: u8) -> bool {
        power_cycle_device_ms(ctx, self.h, requested_off_ms(units))
    }
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column and
    // every metric published lands under a blank owner.
    godspeed::trace::as_name(&ctx, "wifi-driver");

    // ---- Stage 1: the register window. -------------------------------------------------------------
    // Numbered because this IS a sequence and each stage can only be reached through the one before
    // it, which is what makes the last line printed the diagnosis.
    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => {
            // Not a fault, and it must not read as one. The kernel grants this window only where its
            // census saw the Arasan answer, so on any other board - including QEMU's `raspi4b`, which
            // emulates no Arasan at all - arriving here is the correct outcome.
            ctx.log(
                "wifi-driver: no SDIO register window was granted, so there is no radio to drive on \
                 this machine. The kernel grants it only where its boot census saw the controller \
                 answer - look for the `sdio:` lines above",
            );
            serve_unavailable(&ctx, None);
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 1 - granted {} byte(s) of SDIO host registers",
        mmio.len()
    ));
    // THE VISIONFIVE'S RADIO LEAVES THE PI 4'S PATH HERE (docs/wifi-aic8800.md 7, phase V1): `v1_dw_mmc`
    // identifies the AIC8800 on the DesignWare host and never returns. Everything below drives the Pi 4's
    // Arasan host and CYW43455; the AIC8800's protocol, V2 onwards, is inside `v1_dw_mmc`.
    #[cfg(wifi_host_dw_mmc)]
    v1_dw_mmc(&ctx, &mmio);
    // THE CLOCK, BEFORE ANYTHING TOUCHES THE CHIP: a lease from `power` holds the Arm cores fast for the
    // bring-up, and every exit below hands it back (docs/power.md, docs/wifi.md 55).
    let lease = clock_lease(&ctx);

    // ---- Stage 2: the host controller. -----------------------------------------------------------
    // The base clock comes from the platform, not from the controller: the Arasan reports it wrongly
    // in CAPABILITIES on this family (Linux carries `.missing_caps = true` for exactly this part), and
    // a divider from a wrong base runs the identification clock at the wrong speed so that nothing
    // answers - silently, and on hardware only. 0 means the platform declined to say, and the host
    // layer refuses rather than guessing.
    let base = ctx.emmc_base_clock_hz();
    let h = host::Host::new(&ctx, &mmio, base);
    // Print the version register the KERNEL identified this controller by. If the two numbers
    // disagree, the grant is pointed somewhere other than where the census looked, and this is the one
    // line where both are visible.
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 2 - SLOTISR_VER={:#010x} (the kernel's census read this same register)",
        h.version_reg()
    ));
    if !h.reset(&ctx) {
        ctx.log("wifi-driver: the host controller did not come up, so nothing further was attempted");
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
    }

    // ---- Stage 3: what is on the bus. ------------------------------------------------------------
    let mut card = match sdio::identify(&h, &ctx) {
        Some(c) => c,
        None => {
            // NO ANSWER MAY MEAN NO POWER. An instance that died with the chip's power cut - killed in the
            // middle of a power cycle, or while `wifi radio off hard` held it down - leaves WL_ON low, and
            // its respawn finds an empty bus. Boot 2026-10-01 13:59: three respawns in a row, each "no SDIO
            // card answered", and `wifi radio on` could not bring it back. Where this service holds the
            // power, it is restored the boot's way - host parked across the edge, `power_on_device` - and
            // the bus is asked once more. A pin that is already high makes no edge, so a powered chip that
            // truly does not answer costs one more identification and the settle, then is reported as
            // before. Stated consequence: a driver that dies during `off hard` comes back with the radio
            // powered; the alternative was a radio no command short of a power cycle could reach.
            let retried = if power_on_device(&ctx, &h) {
                ctx.log("wifi-driver: no SDIO card answered - the chip's power is now asserted (a dead instance may have left it cut), and the bus is asked again");
                if h.reset(&ctx) { sdio::identify(&h, &ctx) } else { None }
            } else {
                None
            };
            match retried {
                Some(c) => c,
                None => {
                    ctx.log(
                        "wifi-driver: no SDIO card answered on this bus. The controller is ours and came up, \
                         so the remaining suspects are the ones the kernel reports at boot: the SD power \
                         domain and the GPIO34-39 mux",
                    );
                    { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
                }
            }
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: stage 3 - an SDIO card with {} function(s) at RCA {:#06x}, I/O OCR {:#08x}{}",
        card.funcs,
        card.rca,
        card.ocr,
        if card.memory { ", and memory too (a combo card)" } else { "" }
    ));

    // The identification clock has done its job, so leave it. A failure here is reported and NOT
    // fatal: everything below rides CMD52, which works at 400 kHz perfectly well, just slowly. Saying
    // so rather than returning is the difference between a degraded stage and a lost one.
    if !h.set_operating_clock(OPERATING_HZ, &ctx) {
        ctx.log(
            "wifi-driver: the operating clock would not stabilise - continuing at the identification \
             clock, which is slow but correct",
        );
    }

    // ---- Stage 4: the card's own common registers. ------------------------------------------------
    sdio::report_cccr(&h, &ctx);

    // ---- Stage 5: the CIS, which is the actual proof. ---------------------------------------------
    // A controller answering says a HOST CONTROLLER is there. A card answering CMD5 says an I/O card
    // is there. Only the CIS says WHICH part, and that is the claim `docs/wifi.md` section 4 rests on.
    match sdio::cis_pointer(&h, &ctx) {
        Some(ptr) => {
            ctx.log_fmt(format_args!("wifi-driver: stage 5 - walking the CIS from {:#07x}", ptr));
            match sdio::walk_cis(&h, ptr, &ctx) {
                // REPORTED, NOT JUDGED. The manufacturer is a meaningful check; the device code is the
                // SDIO id, which is NOT the field that identifies the part for anything this driver does
                // - see the note in `sdio::Manfid`. The verdict is stage 7's, from the chip id.
                Some(id) if id.is_broadcom() => ctx.log_fmt(format_args!(
                    "wifi-driver: a BROADCOM part is on the bus - manufacturer {:#06x}, SDIO device \
                     code {:#06x}. Which part it is comes from the chip id below, not from this code",
                    id.manf, id.device
                )),
                Some(id) => ctx.log_fmt(format_args!(
                    "wifi-driver: the part on this bus is NOT Broadcom - manufacturer {:#06x} (expected \
                     {:#06x}), SDIO device code {:#06x}. That is a finding, not a failure",
                    id.manf,
                    sdio::Manfid::BROADCOM,
                    id.device
                )),
                None => ctx.log(
                    "wifi-driver: the CIS walk found no MANFID tuple, so the part on the bus is \
                     unidentified. It answered CMD5 and CMD52, so this is the tuple chain rather than \
                     the device",
                ),
            }
        }
        None => ctx.log("wifi-driver: no CIS pointer, so the part on the bus cannot be identified"),
    }

    // ---- Stage 6: enable the backplane function, which is the first WRITE. ------------------------
    // Everything above is a read. A bus that answers reads and drops writes looks perfectly healthy
    // until here, so this is worth doing even though nothing yet uses the function: it is the path a
    // firmware image is later written through, and the readback of IO_READY is the proof it is open.
    //
    // Function 1 specifically, because that is the backplane on this part - what `brcmfmac` enables
    // first, before any firmware exists inside the chip to answer. Not fatal: identification already
    // succeeded, and saying which half failed is worth more than stopping.
    // THE BLOCK SIZES FIRST, which is the order `brcmf_sdiod_probe` uses: function 1 to 64 and function
    // 2 to 512, both BEFORE function 1 is enabled. This driver set neither, ever - and it is the one step
    // the reference performs in the stretch the fault has been narrowed to (the window is verified, the
    // card accepts the command, and then sends nothing).
    //
    // Function 2 is set even though nothing uses it yet, because that is what the reference does here and
    // the firmware upload will need it. Neither is fatal: they are reported and the sequence continues, so
    // a refusal here does not hide whatever the read does next.
    ctx.log("wifi-driver: stage 6 - block sizes, then opening function 1, the backplane");
    if card.funcs >= 1 {
        sdio::set_block_size(&h, 1, 64, &ctx);
    }
    if card.funcs >= 2 {
        sdio::set_block_size(&h, 2, 512, &ctx);
    }
    let backplane_open = card.funcs >= 1 && sdio::enable_function(&h, 1, &ctx);
    if !backplane_open {
        ctx.log(
            "wifi-driver: function 1 (the backplane) is not open, so no firmware could be written \
             through it. Identification succeeded, so the card is there and reachable for reads",
        );
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
    }

    // ---- Stage 7: ask the SILICON what it is. -----------------------------------------------------
    // The CIS device code and this board's documented part disagree, and that disagreement decides
    // which firmware blob phase 2 must upload. The CIS cannot settle it - it IS the disputed reading -
    // so this asks the chip's own identity register, reached through the backplane that stage 6 opened.
    //
    // Not a detour: the same register carries the chip TYPE, which is what says how a later phase walks
    // the core list to find where the chip's RAM is. The firmware upload needs this read anyway.
    ctx.log("wifi-driver: stage 7 - waking the backplane to read the chip's own identity");
    if !backplane::wake(&h, &ctx) {
        ctx.log(
            "wifi-driver: the backplane is not answering, so the chip cannot be asked what it is. \
             Everything through stage 6 stands: the card is on the bus, identified, and function 1 \
             reported ready",
        );
        { clock_release(&ctx, lease); serve_unavailable(&ctx, Some(&h)) }
    }
    let mut window = backplane::Window::new();
    // The radio's session, if boot brings it up. The serving loop scans on it when the shell asks; `None`
    // means every such request is answered "radio down" - loudly, and without pretending (§26.7).
    let mut radio: Option<ctrl::Session> = None;
    // Set when the firmware is seen to trap at start; it decides what `radio down` tells the shell.
    let mut trapped = false;
    // The proper read first - one CMD53, one 32-bit fetch by the bridge. If it fails, fall back to four
    // CMD52 byte reads, which is NOT how this should be done and is the command known to work on this
    // bus: whichever answers tells us something we do not have yet. See `chip_id_via_cmd52`.
    let found = backplane::chip_id(&h, &mut window, &ctx)
        .or_else(|| backplane::chip_id_via_cmd52(&h, &mut window, &ctx));
    match found {
        Some(id) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: CHIP SAYS id {:#06x} ({}) rev {} package {} type {} [raw {:#010x}]",
                id.id,
                id.describe(),
                id.rev,
                id.package,
                id.chip_type,
                id.raw
            ));
            // THE COMPARISON IS THE POINT, so it is made here rather than left to a reader with two
            // numbers in different bases. The CIS device code and the silicon's chip id are DIFFERENT
            // fields - Broadcom does not oblige them to match, and 0xA9BF/0x4345 for the 43455 is the
            // worked example - so agreement and disagreement both mean something specific.
            // WHICH FIRMWARE, selected from the chip id and revision exactly as brcmfmac's table does
            // (the revision field there is a BITMASK - see `ChipId::firmware`). This is the answer the
            // whole of phase 1 existed to get, because it is what phase 2 uploads.
            match id.firmware() {
                Some(fw) => ctx.log_fmt(format_args!(
                    "wifi-driver: this part wants firmware `{}` - so `nonfree/{}/` is the blob to \
                     upload, chosen from the chip id and revision rather than from the board's \
                     documentation",
                    fw,
                    if fw.contains("43455") { "brcm43455" } else { "<not vendored>" }
                )),
                None => ctx.log(
                    "wifi-driver: no firmware is mapped for this chip id and revision, so phase 2 has \
                     nothing to upload. A finding rather than a failure - report the id and revision",
                ),
            }

            // ---- Stage 8: enumerate the chip's internal cores. --------------------------------------
            // The firmware goes into the chip's RAM and nothing yet knows where that is. The chip
            // publishes a table - the EROM - naming every core on its internal bus with an ID, a
            // revision and a register base, and finding the ARM core in it is what makes an address to
            // write to. So this comes before the upload rather than beside it, and it is verifiable on
            // its own: it prints a table that is either a plausible CYW43455 or it is not.
            ctx.log("wifi-driver: stage 8 - walking the EROM to find the ARM core and the RAM");
            let cores = erom::scan(&h, &mut window, &ctx);
            match &cores {
                Some(cores) => {
                    // CHECK THE WRAPPER RULE BEFORE TRUSTING IT. The scan collected what the EROM
                    // published; this asks whether `base + WRAPPER_OFFSET` reproduces those, which is the
                    // only evidence from THIS die that the derivation is right.
                    cores.check_wrappers(&ctx);
                    cores.report(&ctx);
                }
                None => ctx.log(
                    "wifi-driver: the core table could not be walked, so phase 2 has no address to                      write firmware to. Everything through stage 7 stands - the chip is identified and                      its backplane reads",
                ),
            }

            // ---- ADOPT: the firmware an earlier instance loaded is running; attach to it. -------------
            // `identify` found the card initialised with function 2 up, which only a running firmware
            // holds up. Everything that would disturb that firmware is skipped - no halt, no core reset,
            // no upload (stages 9-11) - and the bus is brought up on it as it stands. Any transfer the
            // dead instance left in flight is aborted per function first (IO_ABORT, not RES). The
            // firmware is then asked the same first question the boot asks; if it answers, this instance
            // carries on with it, joins from `/wifi.keys` as a fresh one would, and the kill cost
            // seconds. If it does not answer there is nothing left to try on this chip short of cutting
            // its power (`backlog/69`), and the radio says so rather than restarting a firmware the ROM
            // will not boot.
            if card.warm {
                ctx.log("wifi-driver: ADOPT - stages 9 to 11 are skipped: no halt, no reset, no upload. The running firmware is asked to answer");
                sdio::abort(&h, 1, &ctx);
                sdio::abort(&h, 2, &ctx);
                match cores.as_ref().and_then(|c| c.sdiod.as_ref()) {
                    Some(sdiod) => {
                        if bus::bring_up(&h, &mut window, sdiod.base, &ctx) && ctrl::report_mac(&h, &mut window, &ctx) {
                            ctx.log("wifi-driver: ADOPTED - the firmware the earlier instance loaded answers; this instance carries on with it");
                            radio = scan::run(&h, &mut window, true, &ctx);
                        } else if power_cycle_device(&ctx, &h) {
                            // THE FIRMWARE IS DEAD AND THE CHIP IS NOW COLD. Everything the warm path did
                            // on the SDIO side has to be done again on the fresh card - identification,
                            // the operating clock, block sizes, function 1, the backplane - and the
                            // backplane window object forgets what it thought it had set. The core table
                            // from the EROM walk is the same silicon and stands. Then `card.warm` is
                            // cleared and the boot's own path, stage 9 on, takes over.
                            ctx.log("wifi-driver: the running firmware did not answer - the radio was power-cycled, and this instance starts it from cold");
                            // The host was PARKED across the cut and has no clock; `identify_once` sets
                            // none. Back from reset first, at the identification clock - power, then the
                            // init clock (docs/wifi.md 48).
                            let fresh_card = if h.reset(&ctx) {
                                sdio::identify_once(&h, &ctx)
                            } else {
                                ctx.log("wifi-driver: the host did not come back from its park after the power cycle");
                                None
                            };
                            match fresh_card {
                                Some(fresh) => {
                                    card = fresh;
                                    if !h.set_operating_clock(OPERATING_HZ, &ctx) {
                                        ctx.log("wifi-driver: the operating clock would not stabilise after the power cycle - continuing at the identification clock");
                                    }
                                    sdio::set_block_size(&h, 1, 64, &ctx);
                                    sdio::set_block_size(&h, 2, 512, &ctx);
                                    if !sdio::enable_function(&h, 1, &ctx) || !backplane::wake(&h, &ctx) {
                                        ctx.log("wifi-driver: the power-cycled chip did not open its backplane - the radio stays down");
                                        card.warm = true; // keep stage 9 off a chip that did not come back
                                    } else {
                                        window = backplane::Window::new();
                                    }
                                }
                                None => {
                                    ctx.log("wifi-driver: the power-cycled chip did not identify - the radio stays down");
                                }
                            }
                        } else {
                            ctx.log("wifi-driver: the running firmware did not answer, this machine has no control over the radio's power, and a firmware restarted on a warm chip traps in its ROM (docs/wifi.md 45) - the radio stays down: this machine cannot restart a firmware that stopped without cutting the chip's power");
                        }
                    }
                    None => ctx.log(
                        "wifi-driver: the EROM described no SDIO device core, so the running firmware cannot be reached - the radio stays down until a reboot",
                    ),
                }
            }

            // ---- Stage 9: how much RAM, and where the firmware goes. ---------------------------------
            // The upload needs an address and a size. The CR4 reports its TCM as a set of BANKS through
            // its own registers - reached by the core's BASE, not its wrapper, which is why a wrapper of
            // 0 does not block this - and the firmware's start address is a per-part constant the
            // reference keeps in a table rather than a formula. NOT on an adopted firmware: the read
            // below halts the core it would size.
            if !card.warm {
                ctx.log("wifi-driver: stage 9 - asking the ARM core how much TCM it has");
            }
            match if card.warm { None } else { cores.as_ref().and_then(|c| c.arm) } {
                Some(arm) => match {
                    // THE CORE IS RESET AND RELEASED WITH ITS CPU HALTED BEFORE IT IS ASKED. A chip a
                    // dead instance left running its firmware answers `ARMCR4_CAP` with zero - 50
                    // respawns under `chaos max-carnage`, 50 times "ZERO memory banks", radio down for
                    // the life of each - and so does a core HELD in reset, which the first attempt at
                    // this (a plain `aicore::disable`) found out on a fresh boot. The register reads
                    // with the core clocked, out of reset and halted: the state brcmfmac's
                    // `brcmf_chip_recognition` puts the chip in before it sizes the RAM ("assure chip
                    // is passive for core register access" - for a CR4, a reset-core with CPUHALT,
                    // not a disable). `aicore::reset(halt = true)` is that sequence and stage 11
                    // already performs it for the upload; here it happens first, where the read needs
                    // it. A fresh chip happens to answer unhalted; one running firmware does not, and
                    // asking in the reference's state serves both (26.14).
                    if let Some(wrap) = arm.wrapper() {
                        if !aicore::reset(&h, &mut window, wrap, true, &ctx) {
                            ctx.log(
                                "wifi-driver: the ARM core could not be reset and halted before its \
                                 memory is sized - the read below may say zero",
                            );
                        }
                    }
                    armcr4::probe(&h, &mut window, arm.base, id.id, &ctx)
                } {
                    Some(ram) => {
                        ram.report(&ctx);
                        // ---- Stage 10: what this build actually carries. ----------------------------
                        // Checked against the size the CHIP just reported rather than against a number
                        // from a document, and stated before any transfer starts: finding out mid-upload
                        // that 600 KB does not fit is the wrong time.
                        ctx.log("wifi-driver: stage 10 - the firmware this build carries");
                        firmware::report(ram.size, ram.base, &ctx);

                        // ---- Stage 11: halt, write, release. --------------------------------------
                        // GUARDED, not attempted. The upload needs the ARM's WRAPPER as well as the RAM
                        // it just sized, and `wrapper()` is `None` only for a core with no register base
                        // at all. Without it there is no address to halt the core through, and writing
                        // into the memory of a RUNNING core is worse than not trying - which is also why
                        // `upload::run` halts first and refuses to continue unless the halt confirms.
                        match arm.wrapper() {
                            Some(wrap) => {
                                // THE 802.11 CORE IS RESET BEFORE THE FIRMWARE GOES IN. This is the
                                // other half of the reference's passive step for a CR4 chip
                                // (`brcmf_chip_cr4_set_passive`: disable the ARM, then reset the D11
                                // core with PHYRESET|PHYCLOCKEN going in and PHYCLOCKEN coming out). A
                                // fresh chip has never run anything, so the firmware finds the D11 as
                                // the reset left it; a RESPAWN finds it as the dead instance's firmware
                                // left it, mid-whatever, and the new firmware came alive over that
                                // state and never brought function 2 ready (26.14). The log says which
                                // state the core was found in, so a boot can tell whether it mattered.
                                if let Some(dw) = cores.as_ref().and_then(|c| c.wlan).and_then(|d| d.wrapper()) {
                                    ctx.log("wifi-driver: resetting the 802.11 core before the upload");
                                    if !aicore::reset_bits(
                                        &h, &mut window, dw,
                                        aicore::D11_PHYRESET | aicore::D11_PHYCLOCKEN,
                                        aicore::D11_PHYCLOCKEN,
                                        aicore::D11_PHYCLOCKEN,
                                        &ctx,
                                    ) {
                                        ctx.log("wifi-driver: the 802.11 core did not come out of its reset cleanly - continuing, the firmware may not bring its functions up");
                                    }
                                }
                                // LINUX'S PRE-DOWNLOAD STEPS (`brcmf_sdio_probe_attach`, after the cores are
                                // passive): KSO, CARDCTRL WLANRESET and PMU RES_RELOAD. This driver never did
                                // them; the chip comes up warm after most power cuts here, and Linux's recovery
                                // rests on the same WL_ON cut plus these (docs/wifi.md 51).
                                linux_pre_download(&h, &mut window, cores.as_ref(), &ctx);
                                ctx.log("wifi-driver: stage 11 - uploading the firmware");
                                if upload::run(&h, &mut window, wrap, &ram, &mut trapped, &ctx) {
                                    ctx.log(
                                        "wifi-driver: PHASE 2 COMPLETE - firmware and NVRAM are in the \
                                         chip and its processor is running them",
                                    );

                                    // ---- Stage 12: bring the bus up for frames. -----------------------
                                    // ONLY REACHED WITH A CONFIRMED-RUNNING FIRMWARE, because `upload::run`
                                    // ends by asking the chip rather than by asserting. Enabling a data
                                    // function against a dead firmware would produce a bus that looks
                                    // ready and answers nothing, which is the failure mode this driver
                                    // keeps refusing to build.
                                    match cores.as_ref().and_then(|c| c.sdiod.as_ref()) {
                                        Some(sdiod) => {
                                            if bus::bring_up(&h, &mut window, sdiod.base, &ctx) {
                                                // ---- Stage 13: ask the firmware something. ---------
                                                // ONLY WITH A BUS THAT REPORTED ITSELF READY. A control
                                                // frame sent into a data function that never came up
                                                // would time out for a reason that has nothing to do
                                                // with the protocol being built here.
                                                if ctrl::report_mac(&h, &mut window, &ctx) {
                                                    // ---- Stage 14: scan. -----------------
                                                    // ONLY ONCE THE CONTROL CHANNEL HAS
                                                    // ANSWERED. A scan is a set plus a
                                                    // stream of events, so running it
                                                    // against a channel that has never
                                                    // replied would confuse "the scan is
                                                    // wrong" with "nothing works yet".
                                                    radio = scan::run(&h, &mut window, false, &ctx);
                                                }
                                            }
                                        }
                                        None => ctx.log(
                                            "wifi-driver: the EROM described no SDIO device core, so \
                                             there is no mailbox to announce the protocol version to - \
                                             the bus cannot be brought up for frames",
                                        ),
                                    }
                                } else {
                                    ctx.log(
                                        "wifi-driver: the upload did not complete. Everything through \
                                         stage 10 stands, and the last line above says which step \
                                         stopped it",
                                    );
                                }
                            }
                            None => ctx.log(
                                "wifi-driver: the ARM core has no register base, so no wrapper can be \
                                 derived and it cannot be halted - the upload is not attempted",
                            ),
                        }
                    }
                    None => ctx.log("wifi-driver: the ARM core memory could not be sized, so the upload has no destination yet"),
                },
                None => if !card.warm {
                    ctx.log("wifi-driver: no ARM core was found, so there is nothing to ask about TCM")
                },
            }
        }
        None => ctx.log(
            "wifi-driver: neither CMD53 nor the CMD52 fallback could read the chip's identity, so the \
             firmware question is still open. The backplane woke and its clock is granted, so this is \
             the register access rather than the chip",
        ),
    }

    // ---- The honest end of phase 1 step 1. --------------------------------------------------------
    ctx.log(
        // SAY WHAT IS TRUE AT THE POINT THIS PRINTS. This line used to assert "NO firmware is uploaded
        // and no 802.11 exists yet" - and it printed immediately AFTER "PHASE 2 COMPLETE", so the log
        // contradicted itself by one line. Third time in this effort that the code moved on and the
        // sentence did not, which is why the sentence no longer claims a phase it cannot see.
        // FOURTH TIME. This line has now been wrong in four different ways as the code moved past it, the
        // last being "no control channel" printed directly after a list of ten networks the control channel
        // fetched. It no longer describes the radio's state at all - the stages above do that, each on its
        // own line, and they are read rather than asserted. What it says is the one thing still true: the
        // SHELL has no way to ask this driver for any of it yet.
        // FIFTH TIME, and the last: it now reports the one fact the serving loop is about to act on.
        if radio.is_some() {
            "wifi-driver: the stages above are the radio's state, each reported as it was read. The \
             radio is up and the shell may ask it to sweep: `wifi scan`"
        } else {
            "wifi-driver: the stages above are the radio's state, each reported as it was read. The \
             radio did NOT come up, so every `wifi` request will be answered `radio down` rather than left waiting"
        },
    );
    let down_reason = if trapped { scan::reply::DOWN_TRAPPED } else { scan::reply::DOWN_BRINGUP };
    clock_release(&ctx, lease);
    let mut bcm = radio.map(|s| bcm::Bcm::new(&h, &mut window, s));
    serve_radio(&ctx, &h, bcm.as_mut().map(|b| b as &mut dyn Station), down_reason, "CYW43455")
}
