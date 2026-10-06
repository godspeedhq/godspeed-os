// SPDX-License-Identifier: GPL-2.0-only
//! Frames from the air, and the USB host's other notices. The host keeps one bulk IN armed and says
//! `NOTE_BULK_IN` when a transfer is held (`usbfn::OP_BULK_IN`); this collects it, reads its packets
//! (`rtl_rx`) and each frame (`godspeed_wifi::mgmt`), and says what was heard (R3b). While a sweep runs,
//! each beacon also goes into it, which is all a passive scan is (R4, `station.rs`).
//!
//! These arrive with no reply cap, so they reach this service through the serve loop's [`Host`]: the
//! loop answers the shell, and hands the host's notices here. `NOTE_RADIO` - the dongle bound or
//! removed - ends the loop, so `main.rs` can bring up whatever is there now.

use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::{self, Network, Scan};
use godspeed_wifi::serve::{Host, Notice};
use godspeed_wifi::{mgmt, usbfn, wire};

use crate::rtl_rx;

/// Networks remembered, so each is said once. Bounded: past it, beacons are still counted, not named.
const NETWORKS: usize = 8;
/// Transfers collected per notice, at most. The host holds one transfer at a time and arms again on each
/// collection, so a busy channel can keep this loop fed; the bound gives the loop back between notices.
const PER_NOTICE: u32 = 8;
/// A summary every this many transfers - a count, not a timer (the service blocks between notices).
const SUMMARY_EVERY: u32 = 256;

/// What has been heard since the radio came up - and the dongle's [`Host`]: its notices are the USB host's.
pub struct Heard {
    transfers: u32,
    frames: u32,
    beacons: u32,
    crc: u32,
    cut: u32,
    seen: [[u8; 6]; NETWORKS],
    n_seen: usize,
    no_bulk_said: bool,
    /// What the host said was bound when the serve loop was entered (`main.rs`). A `NOTE_RADIO` ends the
    /// loop only when the host now says something else.
    pub serving: Result<Option<(u16, u16)>, &'static str>,
}

impl Heard {
    pub const fn new() -> Self {
        Heard {
            transfers: 0, frames: 0, beacons: 0, crc: 0, cut: 0, seen: [[0; 6]; NETWORKS], n_seen: 0,
            no_bulk_said: false, serving: Ok(None),
        }
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

impl Host for Heard {
    fn notice(&mut self, msg: &[u8], sweep: Option<&mut Scan>, ctx: &ServiceContext) -> Notice {
        match msg {
            [usbfn::NOTE_BULK_IN] => {
                collect(ctx, self, sweep);
                Notice::Taken
            }
            // The binding MAY have changed: ask, and end the loop only if it did. `dwc2` sends more than one
            // of these for one bind, and the first card of R4 (2026-10-06) showed what ending the loop on
            // each costs: the loop entered three times at boot, `/wifi.keys` loaded three times, and the
            // auto-join tried three times - harmless while a join is refused, and three real joins once R5
            // makes one. `main.rs` asked the same question of every notice before the loop was shared.
            [usbfn::NOTE_RADIO] => {
                if crate::bound(ctx) == self.serving { Notice::Taken } else { Notice::Changed }
            }
            _ => Notice::Ignored,
        }
    }
    // No power operations: the dongle's power is its USB port's, and not this service's to cut. The loop
    // answers `wifi radio off hard` and `powercycle` "no control over the radio's power", which is true.
}

/// `NOTE_BULK_IN` arrived: collect what the host holds, up to `PER_NOTICE` transfers, keeping each beacon
/// in `sweep` when one is running.
fn collect(ctx: &ServiceContext, h: &mut Heard, mut sweep: Option<&mut Scan>) {
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
        rtl_rx::walk(t, &mut |pk| heard(ctx, h, sweep.as_deref_mut(), &pk));
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

/// One packet: counted; a beacon kept in the sweep when one runs; a network not yet named, named.
fn heard(ctx: &ServiceContext, h: &mut Heard, sweep: Option<&mut Scan>, pk: &rtl_rx::Packet) {
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
    let rssi = rtl_rx::rssi(pk.phy, pk.desc.rxmcs);
    if let Some(scan) = sweep {
        keep(scan, &b, rssi);
    }
    if h.n_seen >= NETWORKS || h.seen[..h.n_seen].contains(&bssid) {
        return;
    }
    h.seen[h.n_seen] = bssid;
    h.n_seen += 1;
    let mut name = [0u8; 32];
    let shown = printable(b.ssid().unwrap_or(&[]), &mut name);
    ctx.log_fmt(format_args!(
        "wifi-usb: {} '{}' {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} on channel {}, {} dBm",
        if b.answers_a_probe() { "probe response" } else { "beacon" },
        shown, bssid[0], bssid[1], bssid[2], bssid[3], bssid[4], bssid[5],
        b.channel().map(|c| c as i16).unwrap_or(-1), rssi.unwrap_or(0)));
}

/// A beacon heard during a sweep, as the record `wifi list` prints: the network's own channel from its DS
/// Parameter Set (a neighbour's beacon leaks onto the channel tuned, so the tuned one would be wrong), and
/// its security read from its elements by the one classifier every radio uses (`bss::classify`).
///
/// No channel element means a channel of 0 rather than a guess. No signal reading means 0 dBm, which the
/// shell prints as it is; the descriptor carries one on every frame this chip has handed up so far.
fn keep(scan: &mut Scan, b: &mgmt::Beacon, rssi: Option<i16>) {
    let mut n = Network::blank();
    n.bssid = b.bssid();
    if let Some(ssid) = b.ssid() {
        let len = ssid.len().min(wire::SSID_MAX);
        n.ssid[..len].copy_from_slice(&ssid[..len]);
        n.ssid_len = len as u8;
    }
    // 2.4 GHz: band bits 15:14 are 0, so the chanspec is the channel (`wire::RECORD`).
    n.chanspec = b.channel().unwrap_or(0) as u16;
    n.rssi = rssi.unwrap_or(0);
    n.security = bss::classify(b.elements(), b.capability());
    scan.results = scan.results.wrapping_add(1);
    scan.keep(n);
}

/// An SSID as text: printable ASCII kept, anything else a `?`. It is the air's, so nothing about it is trusted.
fn printable<'a>(ssid: &[u8], out: &'a mut [u8; 32]) -> &'a str {
    let n = ssid.len().min(out.len());
    for (o, &c) in out.iter_mut().zip(&ssid[..n]) {
        *o = if (0x20..0x7f).contains(&c) { c } else { b'?' };
    }
    core::str::from_utf8(&out[..n]).unwrap_or("?")
}
