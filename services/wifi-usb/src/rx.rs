// SPDX-License-Identifier: GPL-2.0-only
//! R3b, the driver's half: frames from the air. The host keeps one bulk IN armed and says `NOTE_BULK_IN`
//! when a transfer is held (`usbfn::OP_BULK_IN`); this collects it, reads its packets (`rtl_rx`) and each
//! frame (`godspeed_wifi::mgmt`), and says what was heard. No scan yet, and nothing goes further up: the
//! card's question is only whether beacons arrive on the channel R3a tuned (`docs/wifi-usb.md` 9).

use godspeed_sdk::ServiceContext;
use godspeed_wifi::{mgmt, usbfn};

use crate::rtl_rx;

/// Networks remembered, so each is said once. Bounded: past it, beacons are still counted, not named.
const NETWORKS: usize = 8;
/// Transfers collected per notice, at most. The host holds one transfer at a time and arms again on each
/// collection, so a busy channel can keep this loop fed; the bound gives the loop back between notices.
const PER_NOTICE: u32 = 8;
/// A summary every this many transfers - a count, not a timer (the service blocks between notices).
const SUMMARY_EVERY: u32 = 256;

/// What has been heard since the radio came up.
pub struct Heard {
    transfers: u32,
    frames: u32,
    beacons: u32,
    crc: u32,
    cut: u32,
    seen: [[u8; 6]; NETWORKS],
    n_seen: usize,
    no_bulk_said: bool,
}

impl Heard {
    pub const fn new() -> Self {
        Heard { transfers: 0, frames: 0, beacons: 0, crc: 0, cut: 0, seen: [[0; 6]; NETWORKS], n_seen: 0, no_bulk_said: false }
    }
}

/// Ask the host for the first time: it holds nothing yet, and the ask is what arms its IN.
pub fn start(ctx: &ServiceContext) -> bool {
    match ask(ctx) {
        Ok(None) | Ok(Some(_)) => {
            ctx.log("wifi-usb: receive started - the host's bulk IN is armed; frames are taken on its interrupt");
            true
        }
        Err(why) => {
            ctx.log_fmt(format_args!("wifi-usb: receive did not start - {}", why));
            false
        }
    }
}

/// The host's answer to one `OP_BULK_IN`: `Ok(None)` when it held nothing.
fn ask(ctx: &ServiceContext) -> Result<Option<godspeed_sdk::Message>, &'static str> {
    let r = crate::host(ctx, &[usbfn::OP_BULK_IN])?;
    let p = r.payload_bytes();
    if p.len() < 2 || p[0] != usbfn::OP_BULK_IN {
        return Err("the host answered something other than BULK_IN - not speaking usbfn");
    }
    match p[1] {
        usbfn::ST_OK if p.len() > 2 => Ok(Some(r)),
        usbfn::ST_OK => Ok(None),
        usbfn::ST_NO_DEVICE => Err("the dongle is no longer bound"),
        usbfn::ST_FAILED => Err("the host has no bulk IN for this dongle (see its log for why)"),
        _ => Err("the host refused BULK_IN as malformed"),
    }
}

/// `NOTE_BULK_IN` arrived: collect what the host holds, up to `PER_NOTICE` transfers.
pub fn collect(ctx: &ServiceContext, h: &mut Heard) {
    for _ in 0..PER_NOTICE {
        let m = match ask(ctx) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(why) => {
                if !h.no_bulk_said {
                    h.no_bulk_said = true;
                    ctx.log_fmt(format_args!("wifi-usb: collecting a transfer - {}", why));
                }
                return;
            }
        };
        let t = &m.payload_bytes()[2..];
        h.transfers = h.transfers.wrapping_add(1);
        let first = h.frames == 0;
        rtl_rx::walk(t, &mut |pk| heard(ctx, h, &pk));
        if first && h.frames > 0 {
            ctx.log_fmt(format_args!(
                "wifi-usb: the FIRST frame from the air - a {}-byte transfer; R3b done", t.len()));
        }
        if h.transfers % SUMMARY_EVERY == 0 {
            ctx.log_fmt(format_args!(
                "wifi-usb: rx - {} transfers, {} frames ({} beacons, {} failed their CRC, {} cut short), {} networks named",
                h.transfers, h.frames, h.beacons, h.crc, h.cut, h.n_seen));
        }
    }
}

/// One packet: counted, and a beacon from a network not yet named, named.
fn heard(ctx: &ServiceContext, h: &mut Heard, pk: &rtl_rx::Packet) {
    if pk.desc.rpt_sel != 0 {
        return;
    }
    h.frames = h.frames.wrapping_add(1);
    if pk.desc.crc_err || pk.desc.icv_err {
        h.crc = h.crc.wrapping_add(1);
        return;
    }
    if pk.truncated {
        h.cut = h.cut.wrapping_add(1);
        return;
    }
    let Some(b) = mgmt::beacon(pk.frame) else { return };
    h.beacons = h.beacons.wrapping_add(1);
    let bssid = b.bssid();
    if h.n_seen >= NETWORKS || h.seen[..h.n_seen].contains(&bssid) {
        return;
    }
    h.seen[h.n_seen] = bssid;
    h.n_seen += 1;
    let mut name = [0u8; 32];
    let shown = printable(b.ssid().unwrap_or(&[]), &mut name);
    let rssi = rtl_rx::rssi(pk.phy, pk.desc.rxmcs);
    ctx.log_fmt(format_args!(
        "wifi-usb: {} '{}' {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} on channel {}, {} dBm",
        if b.answers_a_probe() { "probe response" } else { "beacon" },
        shown, bssid[0], bssid[1], bssid[2], bssid[3], bssid[4], bssid[5],
        b.channel().map(|c| c as i16).unwrap_or(-1), rssi.unwrap_or(0)));
}

/// An SSID as text: printable ASCII kept, anything else a `?`. It is the air's, so nothing about it is trusted.
fn printable<'a>(ssid: &[u8], out: &'a mut [u8; 32]) -> &'a str {
    let n = ssid.len().min(out.len());
    for (o, &c) in out.iter_mut().zip(&ssid[..n]) {
        *o = if (0x20..0x7f).contains(&c) { c } else { b'?' };
    }
    core::str::from_utf8(&out[..n]).unwrap_or("?")
}
