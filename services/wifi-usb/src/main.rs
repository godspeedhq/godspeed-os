// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
#![no_std]
#![no_main]
//! `wifi-usb` - the USB WiFi dongle's driver (`docs/wifi-usb.md`).
//!
//! A Realtek RTL8188CUS today: a soft-MAC radio whose register file is reached by a vendor control
//! request. This service holds no hardware at all. The USB host service that enumerated the dongle -
//! `dwc2` on the Pi 2, `xhci` on the T630 (U2a) - bound it as the radio, and answers `godspeed_wifi::usbfn` for that one device;
//! every register read, and later the firmware and the frames, is a request to it.
//!
//! **U1, this card: the plumbing.** Wait for the host to report the dongle, then read the same two
//! registers milestone 1 read inside `dwc2` (`docs/wifi.md`), now through two services, and decode
//! `SYS_CFG` from Linux's `rtl8xxxu` (26.14). Then R1: the efuse and the power-on (`rtl8188.rs`).
//!
//! **Told, not polled (U1b).** The service asks the host once at start whether a dongle is bound, then
//! blocks; the host sends `usbfn::NOTE_RADIO` when a dongle is bound or removed, and only then is it asked
//! again. The waits inside a bring-up are polls of the chip's own status bits, which it reports only when
//! read; frames come the way the binding does - the host told by its interrupt, and `NOTE_BULK_IN` to us
//! (R3b, `rx.rs`).
//!
//! **The shell's `wifi` is answered by the loop every radio shares (R4).** Once the dongle is up it is a
//! `Station` (`station.rs`) under `godspeed_wifi::serve`, the loop the Pi 4's and the VisionFive's radios
//! run under in `wifi-driver`: `wifi scan`, `wifi list` and `wifi status` are that loop's, and the sweep is
//! this dongle's (a passive one, channels 1 to 13). With no dongle, or one whose bring-up stopped, the same
//! loop answers `radio down` and says why. The host's notices reach `rx.rs` through the loop's `Host`;
//! `NOTE_RADIO` ends it, and the binding is asked again.

use godspeed as gs;
use core::cell::RefCell;

use godspeed_sdk::{Message, ServiceContext};
use godspeed_wifi::station::Station;
use godspeed_wifi::{usbfn, wire};

mod rtl8188;
mod rtl_fw;
mod rtl_power;
mod rtl_queues;
// Read by `rx` (R3b): what the host's bulk IN hands up. Host-tested as well.
mod rtl_rx;
// What goes in front of a frame the host sends (R5a). Host-tested as well.
mod rtl_tx;
mod rtl_tables;
mod rx;
mod station;

/// The 8051's firmware, embedded (`build.rs`, `nonfree/rtl8192cu/PROVENANCE`), and the hash the build measured
/// on disk, which `bring_up` recomputes over what the binary actually holds.
pub(crate) static FIRMWARE: &[u8] = include_bytes!(env!("RTL_FW_TMSC"));
const FIRMWARE_FNV: u32 = rtl_fw::decimal(env!("RTL_FW_TMSC_FNV"));
/// How many times the download is tried before giving up - `rtl8xxxu_init_device`'s figure.
const DOWNLOAD_TRIES: u32 = 6;
use rtl8188::{read32, REG_SYS_CFG, REG_SYS_ISO_CTRL};

/// The USB host services that serve `usbfn`. This service is wired at spawn to the one its board has - the
/// supervisor's spawn row decides, from the board's facts - so it asks whichever of these it holds, and
/// names no board itself (U2).
const HOSTS: [&str; 2] = ["dwc2", "xhci"];

/// The host this service was given: the first of `HOSTS` it holds a capability for, else the first it can
/// REACQUIRE by name. A host spawned after this service is declared but not yet wired: on the T630 (U2a,
/// 2026-10-06) `xhci` came up after `wifi-usb`, this fell back to `dwc2` - absent there - and the first ask
/// failed with "the service could not be reached". `dwc2` when neither works, whose requests then fail and
/// say so.
fn host_name(ctx: &ServiceContext) -> &'static str {
    HOSTS.iter().copied().find(|h| gs::ipc::peer(ctx, h).is_some())
        .or_else(|| HOSTS.iter().copied().find(|h| gs::cap::reacquire(ctx, h)))
        .unwrap_or(HOSTS[0])
}
/// The bound on one request to the host. A control transfer takes milliseconds; the host retries a
/// transient itself, so a request still unanswered after this is a host that is not serving.
const HOST_SECS: i64 = 2;

/// `SYS_CFG` fields, as `rtl8192cu_identify_chip` reads them (`rtl8xxxu.h`): the cut in bits 15:12,
/// UMC (1) or TSMC (0) in bit 19, a TEST chip in bit 23 (`TRP_VAUX_EN`), and 8192C (1) or 8188C (0) in bit 27.
const SYS_CFG_CHIP_VER_SHIFT: u32 = 12;
const SYS_CFG_VENDOR_UMC: u32 = 1 << 19;
const SYS_CFG_TEST_CHIP: u32 = 1 << 23;
const SYS_CFG_TYPE_92C: u32 = 1 << 27;

/// One request to the host, bounded: `gs::call::request_within`, which reacquires the host's cap once if
/// the send failed - the host is spawned by the supervisor and may be respawned after us (14.3) - and never
/// re-sends after a deadline. The failure as the stdlib words it.
pub(crate) fn host(ctx: &ServiceContext, body: &[u8]) -> Result<Message, &'static str> {
    let ask = |m: &[u8]| gs::call::request_within(ctx, host_name(ctx), &Message::from_bytes(m), HOST_SECS).map_err(|e| e.as_str());
    let mut r = ask(body)?;
    // A NOTICE IN PLACE OF THE ANSWER. An instance with no reply mailbox - one spawned past the routing
    // table's reserve with no mailbox credit to take back (docs/wifi-usb.md 19, 20) - awaits its replies on
    // its own endpoint, and then the kernel hands
    // a call the host's next message - its notices included. Taken as the answer, a notice puts every
    // answer after it one behind: R9's first power cycle stopped at "the host answered something other
    // than CONTROL" and then every request was "malformed". `OP_SYNC` names the notice and is never
    // answered, so the call returns the answer still on its way and leaves nothing owed; the host sends
    // the notice again once this service is quiet. Bounded: a host that keeps sending notices in place of
    // answers is said, not chased.
    let mut syncs = 0u32;
    while let Some(&note) = r.payload_bytes().first().filter(|&&b| b == usbfn::NOTE_BULK_IN || b == usbfn::NOTE_RADIO) {
        if syncs == SYNC_MAX {
            return Err("the host's notices kept arriving in place of its answer");
        }
        syncs += 1;
        r = ask(&[usbfn::OP_SYNC, note])?;
    }
    Ok(r)
}

/// Notices taken in place of one answer before `host` gives up on it.
const SYNC_MAX: u32 = 4;

/// What the host says about the radio: `Some((vid, pid))` when one is bound, `None` when none is or the
/// host did not answer - the second said once by the caller.
pub(crate) fn bound(ctx: &ServiceContext) -> Result<Option<(u16, u16)>, &'static str> {
    let r = host(ctx, &[usbfn::OP_INFO])?;
    let p = r.payload_bytes();
    if p.len() < 2 || p[0] != usbfn::OP_INFO {
        return Err("answered something other than INFO - not speaking usbfn");
    }
    match p[1] {
        usbfn::ST_OK if p.len() >= 6 => Ok(Some((u16::from_le_bytes([p[2], p[3]]), u16::from_le_bytes([p[4], p[5]])))),
        usbfn::ST_NO_DEVICE => Ok(None),
        _ => Err("answered INFO with an error"),
    }
}

/// Where the host says the dongle is (`usbfn::OP_INFO`'s optional part), for `wifi hardware usb`. `None`
/// from a host that does not say, or one that did not answer.
fn where_bound(ctx: &ServiceContext) -> Option<rx::Where> {
    let r = host(ctx, &[usbfn::OP_INFO]).ok()?;
    let p = r.payload_bytes();
    let w = usbfn::INFO_WHERE_AT;
    if p.len() < w + 7 || p[0] != usbfn::OP_INFO || p[1] != usbfn::ST_OK {
        return None;
    }
    let n_out = (p[w + 6] as usize).min(4).min(p.len() - (w + 7));
    let mut outs = [0u8; 4];
    outs[..n_out].copy_from_slice(&p[w + 7..w + 7 + n_out]);
    Some(rx::Where {
        root: p[w], hub: p[w + 1], slot: p[w + 2], in_addr: p[w + 3],
        in_mps: u16::from_le_bytes([p[w + 4], p[w + 5]]), outs, n_out: n_out as u8,
    })
}

/// The chip `SYS_CFG` names, as the identity line logs it: `RTL8188C, 1T1R, cut A, TSMC`.
pub(crate) fn chip_text(c: u32) -> (&'static str, &'static str, char, &'static str) {
    (
        if c & SYS_CFG_TYPE_92C != 0 { "RTL8192C" } else { "RTL8188C" },
        if c & SYS_CFG_TYPE_92C != 0 { "2T2R" } else { "1T1R" },
        (b'A' + ((c >> SYS_CFG_CHIP_VER_SHIFT) & 0xF) as u8) as char,
        if c & SYS_CFG_VENDOR_UMC != 0 { "UMC" } else { "TSMC" },
    )
}

/// U1's reads, logged and decoded, then the bring-up. The radio as a `Station` when it came up as far as
/// receiving; `None` when it stopped, said where.
fn identify<'l>(ctx: &ServiceContext, vid: u16, pid: u16, link: &'l RefCell<rx::Link>, sys_cfg: &mut Option<u32>) -> Option<station::Dongle<'l>> {
    ctx.log_fmt(format_args!("wifi-usb: {} has bound a radio at {:04x}:{:04x}", host_name(ctx), vid, pid));
    let cfg = read32(ctx, REG_SYS_CFG);
    let iso = read32(ctx, REG_SYS_ISO_CTRL);
    match (cfg, iso) {
        (Ok(c), Ok(i)) => {
            *sys_cfg = Some(c);
            ctx.log_fmt(format_args!(
                "wifi-usb: SYS_CFG(0xF0)={:#010x} ISO_CTRL(0x00)={:#010x}, read through {}", c, i, host_name(ctx)));
            let plausible = |v: u32| v != 0 && v != 0xFFFF_FFFF;
            if !(plausible(c) && plausible(i)) {
                ctx.log("wifi-usb: all-zeros or all-ones - a bus or power fault, not a register file");
                return None;
            }
            ctx.log_fmt(format_args!(
                "wifi-usb: the chip is an RTL{} ({}), cut {}, made by {} - a {} chip, so its firmware is {}",
                if c & SYS_CFG_TYPE_92C != 0 { "8192C" } else { "8188C" },
                if c & SYS_CFG_TYPE_92C != 0 { "2T2R" } else { "1T1R" },
                (b'A' + ((c >> SYS_CFG_CHIP_VER_SHIFT) & 0xF) as u8) as char,
                if c & SYS_CFG_VENDOR_UMC != 0 { "UMC" } else { "TSMC" },
                if c & SYS_CFG_TEST_CHIP != 0 { "TEST" } else { "normal" },
                // `rtl8192cu_load_firmware`'s choice: not UMC -> _TMSC; UMC and (a later cut or a 92C) -> _B; else _A.
                // (Linux excludes a 1T2R 8191C from "a 92C"; this log line does not.)
                if c & SYS_CFG_VENDOR_UMC == 0 {
                    "rtl8192cufw_TMSC.bin"
                } else if (c >> SYS_CFG_CHIP_VER_SHIFT) & 0xF != 0 || c & SYS_CFG_TYPE_92C != 0 {
                    "rtl8192cufw_B.bin"
                } else {
                    "rtl8192cufw_A.bin"
                }));
            ctx.log("wifi-usb: U1 done - the dongle answers through the host");
            // Receive starts only once the chip's own is set up (R3a): the host's IN armed at a chip that
            // has not been told where to put frames would only be NAKed. A radio that cannot receive
            // cannot scan, so it is not a station.
            // How many paths transmit (`rtl8192cu_identify_chip`): one on an 8188C, and on an 8192C two
            // unless its bonding says 1T2R. Asked here, beside the rest of the identity, for the power (R11).
            let tx_paths = if c & SYS_CFG_TYPE_92C == 0 {
                1
            } else {
                match read32(ctx, rtl8188::REG_HPON_FSM) {
                    Ok(h) if h & rtl8188::HPON_FSM_BONDING_MASK == rtl8188::HPON_FSM_BONDING_1T2R => 1,
                    Ok(_) => 2,
                    Err(why) => {
                        ctx.log_fmt(format_args!("wifi-usb: the bonding read failed - {}", why));
                        return None;
                    }
                }
            };
            let (mac, queues, power) = bring_up(ctx, tx_paths)?;
            let is_8188r = power.is_8188r;
            if !rx::start(ctx) {
                return None;
            }
            // What the power-off needs (R9): the efuse's word, and a UMC chip of cut B from `SYS_CFG`.
            let chip = rtl8188::Chip {
                is_8188r,
                umc_cut_b: c & SYS_CFG_VENDOR_UMC != 0 && (c >> SYS_CFG_CHIP_VER_SHIFT) & 0xF == 1,
            };
            Some(station::Dongle::new(mac, FIRST_CHANNEL, queues, chip, power, link))
        }
        (c, i) => {
            ctx.log_fmt(format_args!(
                "wifi-usb: the register reads did not complete - SYS_CFG: {}, ISO_CTRL: {}",
                c.err().unwrap_or("ok"), i.err().unwrap_or("ok")));
            None
        }
    }
}

/// R11: what the efuse's calibration put in the transmit gain registers on the first channel, and the first
/// word read back - the proof the writes landed. Said once, at bring-up; every later tune sets its own
/// channel's power without a line.
fn report_power(ctx: &ServiceContext, power: &rtl_power::TxPower) {
    if !power.cal.programmed() {
        ctx.log("wifi-usb: the efuse carries no transmit power calibration - the baseband table's gain stays (R11)");
        return;
    }
    let w = rtl_power::words(power, FIRST_CHANNEL);
    let (reg, wrote) = w.gains[0];
    match read32(ctx, reg) {
        Ok(back) => ctx.log_fmt(format_args!(
            "wifi-usb: transmit power from the efuse, channel {} (group {}): CCK {:#04x}, OFDM {:#04x}, {} path(s){}; TX_AGC_A_RATE18_06 written {:#010x}, reads {:#010x}{}",
            FIRST_CHANNEL, rtl_power::group(FIRST_CHANNEL), w.cck[0], w.ofdm[0], power.tx_paths,
            if power.is_8188r { ", 8188RU tables" } else { "" }, wrote, back,
            if back == wrote { " - R11 done" } else { " - NOT what was written" })),
        Err(why) => ctx.log_fmt(format_args!("wifi-usb: the transmit power read-back failed - {}", why)),
    }
}

/// The ceiling on a bring-up step's REPORTED duration; see `bring_up`.
const REPORT_CEILING_MS: u64 = 60_000;

/// R1 (`docs/wifi-usb.md`): the efuse, then the power-on - Linux's order (`rtl8xxxu_init_device`) - and on
/// through R2 and R3a. The dongle's own address, from its efuse, and how many transmit queues its endpoints
/// serve, when every stage completed; `None` when one stopped, said where.
fn bring_up(ctx: &ServiceContext, tx_paths: u8) -> Option<([u8; 6], u8, rtl_power::TxPower)> {
    // How long each half took, for the log - `Deadline::elapsed_us` is the stdlib's measure of a wait. The
    // bound is a ceiling for the report only; every wait inside the efuse walk and the power-on is
    // bounded on its own (`rtl8188.rs`).
    let clock = gs::driver::wait::Deadline::start(ctx, gs::driver::wait::Budget::ms(REPORT_CEILING_MS));
    let (mac, power) = match rtl8188::read_efuse(ctx) {
        Ok(e) => {
            let m = e.mac;
            ctx.log_fmt(format_args!(
                "wifi-usb: efuse ID {:#06x} ({}), VID:PID {:04x}:{:04x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} - {} physical bytes, {} sections, {} ms",
                e.id, if rtl8188::efuse_id_ok(&e) { "as expected" } else { "NOT the 0x8129 this family carries" },
                e.vid, e.pid, m[0], m[1], m[2], m[3], m[4], m[5], e.walked, e.sections,
                clock.elapsed_us() / 1000));
            let power = rtl_power::TxPower {
                cal: rtl_power::Calibration::from_efuse(&e.power),
                tx_paths,
                is_8188r: e.is_8188r,
            };
            (e.mac, power)
        }
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: the efuse read stopped - {}", why));
            return None;
        }
    };
    // Asked BEFORE the power-on, as Linux asks them: whether the MAC is cold, and which transmit queues the
    // dongle's endpoints serve - both decide how the queues are set up after it (R2).
    let (cold, queues) = match (rtl8188::mac_is_cold(ctx), rtl8188::tx_queues(ctx)) {
        (Ok(c), Ok((q, eps))) => {
            ctx.log_fmt(format_args!(
                "wifi-usb: the MAC is {}; transmit queues: high {}, normal {}, low {} ({})",
                if c { "cold" } else { "warm - set up since power came on" },
                q.high, q.normal, q.low,
                match eps {
                    None => "from NORMAL_SIE_EP_TX",
                    Some(_) => "NORMAL_SIE_EP_TX read 0 - from the bulk OUT endpoints in its configuration descriptor",
                }));
            (c, q)
        }
        (c, q) => {
            ctx.log_fmt(format_args!(
                "wifi-usb: could not read the MAC's state or the dongle's queues - {}",
                c.err().or(q.err()).unwrap_or("?")));
            return None;
        }
    };
    let clock = gs::driver::wait::Deadline::start(ctx, gs::driver::wait::Budget::ms(REPORT_CEILING_MS));
    match rtl8188::power_on(ctx) {
        Ok(cr) => ctx.log_fmt(format_args!(
            "wifi-usb: powered on in {} ms - CR={:#06x}; R1 done", clock.elapsed_us() / 1000, cr)),
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: the power-on stopped at {}", why));
            return None;
        }
    }
    if let Err(why) = rtl8188::init_queues(ctx, queues, cold) {
        ctx.log_fmt(format_args!("wifi-usb: the transmit queues were not set up - {}", why));
        return None;
    }
    ctx.log("wifi-usb: transmit queues set up - priority, the receive boundary, and the page reservation where the MAC was cold");
    if !firmware(ctx) {
        return None;
    }
    // R3a: the MAC, the baseband and the RF set up, tuned to one channel, and the channel read back out of
    // the RF chip itself - the one register here that only a working RF path can answer.
    let clock = gs::driver::wait::Deadline::start(ctx, gs::driver::wait::Budget::ms(REPORT_CEILING_MS));
    match rtl8188::init_radio(ctx, cold, FIRST_CHANNEL, &power) {
        Ok((rf, mode)) => {
            let on = (mode & rtl8188::RF_CHANNEL_MASK) as u8;
            ctx.log_fmt(format_args!(
                "wifi-usb: MAC, baseband and RF set up in {} ms ({} RF registers); RF_MODE_AG reads {:#07x} - channel {}{}",
                clock.elapsed_us() / 1000, rf, mode, on,
                if on == FIRST_CHANNEL { ", as asked; R3a done" } else { " - NOT the channel asked for" }));
            if on != FIRST_CHANNEL {
                return None;
            }
            // R5a: a station at its own address, so the chip passes up what is addressed to it.
            match rtl8188::set_station(ctx, &mac) {
                Ok(()) => ctx.log("wifi-usb: the chip is a station at its efuse address (REG_MACID, REG_MSR)"),
                Err(why) => {
                    ctx.log_fmt(format_args!("wifi-usb: the station address was not set - {}", why));
                    return None;
                }
            }
            report_power(ctx, &power);
            Some((mac, queues.count(), power))
        }
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: the radio's set-up stopped - {}", why));
            None
        }
    }
}

/// The channel R3a tunes to: 1, the first in every regulatory domain, so a beacon heard on it proves the
/// receive path without a scan.
const FIRST_CHANNEL: u8 = 1;

/// R2: the embedded firmware checked, its header read, downloaded (up to `DOWNLOAD_TRIES` times, as Linux
/// tries), and started; the chip's own `WINT_INIT_READY` is the word that it runs.
fn firmware(ctx: &ServiceContext) -> bool {
    let fnv = rtl_fw::fnv1a(FIRMWARE);
    if fnv != FIRMWARE_FNV {
        ctx.log_fmt(format_args!(
            "wifi-usb: the embedded firmware does NOT match the file the build read (fnv {:#010x}, the build measured {:#010x}) - not downloading it",
            fnv, FIRMWARE_FNV));
        return false;
    }
    let h = match rtl_fw::header(FIRMWARE) {
        Ok(h) => h,
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: the embedded firmware is refused - {}", why));
            return false;
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-usb: firmware rtl8192cufw_TMSC.bin VERIFIES - signature {:#06x}, version {}.{}, {} bytes of code",
        h.signature, h.major, h.minor, h.code_len));
    let code = &FIRMWARE[rtl_fw::HEADER_LEN..];
    let clock = gs::driver::wait::Deadline::start(ctx, gs::driver::wait::Budget::ms(REPORT_CEILING_MS));
    let mut tries = 0;
    let blocks = loop {
        tries += 1;
        match rtl8188::download_firmware(ctx, code) {
            Ok(n) => break n,
            Err(why) if tries < DOWNLOAD_TRIES => ctx.log_fmt(format_args!(
                "wifi-usb: the download stopped ({}) - try {} of {}", why, tries, DOWNLOAD_TRIES)),
            Err(why) => {
                ctx.log_fmt(format_args!("wifi-usb: the download did not complete in {} tries - {}", tries, why));
                return false;
            }
        }
    };
    ctx.log_fmt(format_args!(
        "wifi-usb: firmware downloaded - {} blocks of up to {} bytes in {} ms ({} {})",
        blocks, rtl_fw::BLOCK, clock.elapsed_us() / 1000, tries, if tries == 1 { "try" } else { "tries" }));
    match rtl8188::start_firmware(ctx) {
        Ok(dl) => {
            ctx.log_fmt(format_args!("wifi-usb: the firmware is RUNNING - MCU_FW_DL={:#010x}; R2 done", dl));
            true
        }
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: the firmware did not start - {}", why));
            false
        }
    }
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.trace_as("wifi-usb");
    ctx.log("wifi-usb: starting - the USB WiFi dongle's driver; asks its USB host once whether a dongle is bound, then waits to be told");
    // What was last said, so each change is said once: none yet, bound (vid, pid), gone, or a host fault.
    let mut said: Option<Result<Option<(u16, u16)>, &'static str>> = None;
    // The dongle as a station, once a bring-up has made one; `None` while there is no dongle or its
    // bring-up stopped - and then the loop below answers `radio down`, with why.
    let mut dongle: Option<station::Dongle> = None;
    // What the radio has heard since it was last brought up (R3b), and the host's notices (`rx.rs`).
    // The link the station and the receive side share (`rx::Link`): owned here, for the service's life.
    let link = RefCell::new(rx::Link::new());
    let mut heard = rx::Heard::new(&link, host_name(&ctx));
    // The key-derivation primitives against their published vectors, once: the serve loop is entered again
    // on every replug, and this result does not change.
    let crypto_ok = godspeed_wifi::crypto::selftest(&ctx, "wifi-usb");
    loop {
        let now = bound(&ctx);
        if said != Some(now) {
            dongle = None;
            match now {
                Ok(Some((vid, pid))) => {
                    heard = rx::Heard::new(&link, host_name(&ctx));
                    *link.borrow_mut() = rx::Link::new();
                    heard.ids = Some((vid, pid));
                    heard.place = where_bound(&ctx);
                    dongle = identify(&ctx, vid, pid, &link, &mut heard.sys_cfg);
                    if let Some(d) = dongle.as_ref() {
                        heard.us = d.address();
                        heard.chip = Some(d.chip());
                    }
                }
                Ok(None) => ctx.log_fmt(format_args!(
                    "wifi-usb: {} has no dongle bound - waiting to be told when one is", host_name(&ctx))),
                Err(why) => ctx.log_fmt(format_args!(
                    "wifi-usb: asking {} about the radio: {} - waiting to be told when its binding changes", host_name(&ctx), why)),
            }
            said = Some(now);
        }
        // The shell's `wifi` and the host's notices, until the host says the binding changed (U1b's
        // `NOTE_RADIO`, which ends the loop) - no timer. A dongle that is bound but did not come up is
        // answered as one whose bring-up stopped; no dongle, as no radio.
        let why = if matches!(now, Ok(Some(_))) { wire::DOWN_BRINGUP } else { wire::DOWN_NO_RADIO };
        heard.serving = now;
        godspeed_wifi::serve::serve(
            &ctx, "wifi-usb", dongle.as_mut().map(|d| d as &mut dyn Station), &mut heard, why, crypto_ok);
    }
}
