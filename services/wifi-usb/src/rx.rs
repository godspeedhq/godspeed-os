// SPDX-License-Identifier: GPL-2.0-only
//! Frames from the air, and the USB host's other notices. The host keeps one bulk IN armed and says
//! `NOTE_BULK_IN` when a transfer is held (`usbfn::OP_BULK_IN`); this collects it, reads its packets
//! (`rtl_rx`) and each frame (`godspeed_wifi::mgmt`), and says what was heard (R3b). While a sweep runs,
//! each beacon also goes into it, which is all a passive scan is (R4, `station.rs`).
//!
//! These arrive with no reply cap, so they reach this service through the serve loop's [`Host`]: the
//! loop answers the shell, and hands the host's notices here. `NOTE_RADIO` - the dongle bound or
//! removed - ends the loop, so `main.rs` can bring up whatever is there now.
//!
//! **Data frames (R6).** Once the station is joined, a data frame from its access point to it (or to a
//! group address) that the chip decrypted becomes the ethernet frame inside it and waits in the [`Link`]'s
//! queue, which the station's `pull` hands `nic-driver`. The queue is shared with the station through a
//! `RefCell` `main.rs` owns, because the serve loop holds the station and this host apart.

use core::cell::RefCell;

use godspeed as gs;
use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::{self, Network, Scan};
use godspeed_wifi::data::{self, DataIn};
use godspeed_wifi::rxq::RxQueue;
use godspeed_wifi::serve::{Host, Notice};
use godspeed_wifi::{eapol, mgmt, usbfn, wire};

use crate::rtl_rx;

/// Networks remembered, so each is said once. Bounded: past it, beacons are still counted, not named.
const NETWORKS: usize = 8;
/// Transfers collected per notice, at most. The host holds one transfer at a time and arms again on each
/// collection, so a busy channel can keep this loop fed; the bound gives the loop back between notices.
const PER_NOTICE: u32 = 8;
/// A summary every this many transfers - a count, not a timer (the service blocks between notices).
const SUMMARY_EVERY: u32 = 256;

/// `RX_DESC_ENC_AES`: the receive descriptor's `security` for a frame protected with CCMP.
const RX_ENC_AES: u8 = 4;
/// The CCMP MIC at the end of a frame the chip decrypted: `RCR` appends it (`RCR_APPEND_MIC`), and the
/// 802.11 layer above a radio strips it, as mac80211 does for a frame marked decrypted and not
/// `RX_FLAG_MIC_STRIPPED` - which `rtl8xxxu` never sets.
const CCMP_MIC: usize = 8;

/// The longest EAPOL frame taken or sent, as ethernet: the supplicant's own message buffer (its header,
/// the 99-byte key descriptor, a nonce's worth, and 512 bytes of key data).
pub const EAPOL_MAX: usize = 14 + 99 + 64 + 512;

/// The link as the station and this host share it: the network joined, and the frames received through
/// it waiting for the station's `pull`. Owned by `main.rs` for the life of the service.
pub struct Link {
    /// The joined network's BSSID, set by the station when it joins and cleared when it leaves; `None`
    /// means no data frame is taken.
    pub bssid: Option<[u8; 6]>,
    pub frames: RxQueue,
    /// Frames taken, frames the queue had no room for, protected frames the chip did not decrypt, and
    /// rekey frames not answered (R7).
    pub data_in: u32,
    pub dropped: u32,
    pub undecrypted: u32,
    pub rekeys: u32,
    /// REPLAY PROTECTION: the highest CCMP packet number accepted under the pairwise key, and under each
    /// group key id. The chip decrypts and does not check them; a frame whose number is not above the last
    /// one accepted for its key is a replay - an old frame sent again - and is dropped. mac80211 keeps one
    /// per TID for the pairwise key; this association is non-QoS, so it has one.
    pub pairwise_pn: u64,
    pub group_pn: [u64; 4],
    pub replays: u32,
    /// A key frame from the access point on the joined link (a group rekey, R7), as ethernet, waiting for
    /// the station's `pull` to answer it. One slot: the access point sends the next only after this one is
    /// answered or timed out, and a second arriving meanwhile is counted in `rekeys_dropped`.
    pub rekey: [u8; EAPOL_MAX],
    pub rekey_len: usize,
    pub rekeys_dropped: u32,
}

impl Link {
    pub fn new() -> Self {
        Link {
            bssid: None, frames: RxQueue::new(), data_in: 0, dropped: 0, undecrypted: 0, rekeys: 0,
            pairwise_pn: 0, group_pn: [0; 4], replays: 0,
            rekey: [0; EAPOL_MAX], rekey_len: 0, rekeys_dropped: 0,
        }
    }

    /// A join is starting: every replay counter back to zero, for the keys it is about to install - a new
    /// pairwise key starts its packet numbers at 1. The group key's counter is then set from its Key RSC
    /// when it is installed (`group_rsc`), which is why this runs at the START of a join and not its end.
    pub fn new_keys(&mut self) {
        self.pairwise_pn = 0;
        self.group_pn = [0; 4];
    }

    /// The group key at `key_id` starts above `rsc` (its Key RSC): a broadcast frame the access point sent
    /// under it before this station joined cannot be replayed to it.
    pub fn group_rsc(&mut self, key_id: u32, rsc: u64) {
        self.group_pn[key_id as usize & 3] = rsc;
    }

    /// The same, for a key already held: the counter only ever rises (no KRACK-style reset).
    pub fn group_rsc_at_least(&mut self, key_id: u32, rsc: u64) {
        let k = key_id as usize & 3;
        self.group_pn[k] = self.group_pn[k].max(rsc);
    }

    /// A join completed: take its network's frames from now, with nothing left queued from before. The
    /// replay counters stand as the join set them.
    pub fn joined(&mut self, bssid: [u8; 6]) {
        self.bssid = Some(bssid);
        self.frames.clear();
        self.rekey_len = 0;
    }
}

/// What has been heard since the radio came up - and the dongle's [`Host`]: its notices are the USB host's.
pub struct Heard<'l> {
    link: &'l RefCell<Link>,
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
    /// The dongle's own address, once it is a station (R5a): a probe response addressed to it answers OUR
    /// probe request, which is the proof a frame we sent reached the air.
    pub us: [u8; 6],
    /// Probe responses addressed to `us`.
    answers: u32,
}

impl<'l> Heard<'l> {
    pub fn new(link: &'l RefCell<Link>) -> Self {
        Heard {
            link,
            transfers: 0, frames: 0, beacons: 0, crc: 0, cut: 0, seen: [[0; 6]; NETWORKS], n_seen: 0,
            no_bulk_said: false, serving: Ok(None), us: [0; 6], answers: 0,
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

impl Host for Heard<'_> {
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

/// How far apart the join's own asks are (`wait_frames`): often enough that a 200 ms answer window holds
/// dozens of looks, rarely enough that the host is not asked for nothing hundreds of times a second.
const JOIN_PACE_MS: u64 = 2;

/// THE ONE PLACE THIS SERVICE ASKS RATHER THAN IS TOLD. A join (R5b) is a sequence of exchanges with an
/// access point inside ONE `Station::join` call, and while it runs the serve loop is not receiving, so the
/// host's `NOTE_BULK_IN` cannot reach the code waiting for the answer. So the join asks the host for held
/// transfers itself, `JOIN_PACE_MS` apart, for at most `budget_ms`, and hands each good packet to `f` until
/// `f` says it has what it was waiting for. The notices the host sends meanwhile stay queued, and the loop
/// takes them afterwards as it would any notice - a collection that finds nothing held, which is harmless.
/// `true` when `f` said so before the budget ran out.
pub fn wait_frames(ctx: &ServiceContext, budget_ms: u64, f: &mut dyn FnMut(&rtl_rx::Packet) -> bool) -> bool {
    let since = gs::driver::wait::Since::now(ctx);
    let budget = gs::driver::wait::Budget::ms(budget_ms);
    loop {
        match ask(ctx) {
            Ok(Some(m)) => {
                let mut done = false;
                rtl_rx::walk(&m.payload_bytes()[2..], &mut |pk| {
                    if !done && pk.desc.rpt_sel == 0 && !pk.desc.crc_err && !pk.desc.icv_err && !pk.truncated {
                        done = f(&pk);
                    }
                });
                if done {
                    return true;
                }
            }
            Ok(None) => gs::task::sleep_ms(ctx, JOIN_PACE_MS),
            Err(_) => return false,
        }
        if since.passed(ctx, budget) {
            return false;
        }
    }
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
                "wifi-usb: rx - {} transfers, {} frames ({} beacons, {} failed their CRC, {} cut short), {} networks named, {} answers to our probes",
                h.transfers, h.frames, h.beacons, h.crc, h.cut, h.n_seen, h.answers));
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
    if pk.frame.len() >= 24 && pk.frame[0] & 0x0c == 0x08 {
        data_frame(ctx, h, pk);
        return;
    }
    let Some(b) = mgmt::beacon(pk.frame) else { return };
    h.beacons = h.beacons.wrapping_add(1);
    let bssid = b.bssid();
    if b.answers_a_probe() && h.us != [0; 6] && mgmt::addressed_to(pk.frame, &h.us) {
        h.answers = h.answers.wrapping_add(1);
        if h.answers == 1 {
            ctx.log("wifi-usb: the FIRST probe response addressed to us - a probe request we sent reached the air; R5a done");
        }
    }
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

/// A data frame (R6): taken into the link's queue as ethernet when it is from the joined network's access
/// point, to us or to a group, and the chip decrypted it. Everything else is not this station's.
fn data_frame(ctx: &ServiceContext, h: &mut Heard, pk: &rtl_rx::Packet) {
    let mut l = h.link.borrow_mut();
    let Some(bssid) = l.bssid else { return };
    let f = pk.frame;
    // From DS, transmitted by our access point, and to us or to a group.
    if f[1] & 0x03 != 0x02 || f[10..16] != bssid || (f[4..10] != h.us && f[4] & 1 == 0) {
        return;
    }
    if f[1] & 0x40 == 0 || pk.desc.security != RX_ENC_AES || pk.desc.swdec {
        l.undecrypted = l.undecrypted.wrapping_add(1);
        if l.undecrypted == 1 {
            ctx.log_fmt(format_args!(
                "wifi-usb: a data frame from the access point the chip did not decrypt (protected {}, security {}, swdec {}) - not taken",
                f[1] & 0x40 != 0, pk.desc.security, pk.desc.swdec));
        }
        return;
    }
    // REPLAY: the packet number must climb, per key - the pairwise one for a frame to us, the group key
    // its header names for a frame to a group.
    let Some((pn, key_id)) = data::ccmp_pn(f) else { return };
    let group = f[4] & 1 != 0;
    let k = key_id as usize & 3;
    let last = if group { l.group_pn[k] } else { l.pairwise_pn };
    if pn <= last {
        l.replays = l.replays.wrapping_add(1);
        if l.replays == 1 || l.replays % 64 == 0 {
            ctx.log_fmt(format_args!(
                "wifi-usb: a data frame replayed - packet number {} not above the {} already accepted (x{}) - dropped",
                pn, last, l.replays));
        }
        return;
    }
    if group {
        l.group_pn[k] = pn;
    } else {
        l.pairwise_pn = pn;
    }
    let Some(d) = data::llc_payload(f, true) else { return };
    // The MIC off first, for every frame - a key frame's own MIC is computed over its exact length, so a
    // trailing 8 bytes would fail it.
    let body = &d.body[..d.body.len().saturating_sub(CCMP_MIC)];
    let inner = DataIn { ethertype: d.ethertype, da: d.da, sa: d.sa, body };
    if d.ethertype == eapol::ETHERTYPE_EAPOL {
        // A key frame on the joined link: the access point's group rekey, for the station's `pull` (R7).
        l.rekeys = l.rekeys.wrapping_add(1);
        if l.rekey_len != 0 {
            l.rekeys_dropped = l.rekeys_dropped.wrapping_add(1);
            return;
        }
        let mut eth = [0u8; EAPOL_MAX];
        let n = data::to_ethernet(&inner, &mut eth);
        if n == 0 {
            l.rekeys_dropped = l.rekeys_dropped.wrapping_add(1);
            return;
        }
        l.rekey[..n].copy_from_slice(&eth[..n]);
        l.rekey_len = n;
        return;
    }
    let mut eth = [0u8; godspeed_wifi::rxq::FRAME_MAX];
    let n = data::to_ethernet(&inner, &mut eth);
    if n == 0 || !l.frames.push(&eth[..n]) {
        l.dropped = l.dropped.wrapping_add(1);
        return;
    }
    l.data_in = l.data_in.wrapping_add(1);
    if l.data_in == 1 {
        ctx.log_fmt(format_args!(
            "wifi-usb: the FIRST data frame through the link - ethertype {:#06x}, {} bytes, decrypted by the chip; R6 receive works",
            d.ethertype, n));
    }
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
