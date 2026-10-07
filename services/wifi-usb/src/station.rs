// SPDX-License-Identifier: GPL-2.0-only
//! R4: the dongle as a `Station`, so the serve loop every radio shares (`godspeed_wifi::serve`) answers the
//! shell for it - `wifi scan`, `wifi list`, `wifi status` - exactly as it does for the Pi 4's and the
//! VisionFive's radios.
//!
//! **The sweep is the host's to run.** A full-MAC radio is told "scan" and reports what its firmware heard;
//! this chip is soft-MAC, so the sweep is this file: tune a channel, listen for `DWELL_MS`, tune the next,
//! through 1 to 13, then back to the channel it was on. Since R5a it also ASKS: one wildcard probe request
//! on each channel as it is tuned, the first frame this driver sends. Networks that beacon answer it with a
//! probe response addressed to us (`rx.rs` counts those); a hidden network answers only a probe that names
//! it, which is the join's to send.
//!
//! **Frames arrive as they always did** (R3b): the host takes the bulk IN on its interrupt and sends
//! `NOTE_BULK_IN`, which the serve loop hands to this service's `Host` (`rx.rs`) with the running sweep, and
//! each beacon heard is kept in it. `scan_step` only moves the dial.
//!
//! **The join, so far (R5b).** Find the network by a probe that NAMES it, on each channel, keeping the
//! strongest answer; tune there and set the chip's BSSID; Open System authentication, then the association
//! request - each answer awaited 200 ms and tried three times, mac80211's `IEEE80211_AUTH_TIMEOUT` and
//! `_MAX_TRIES` and their `ASSOC` twins (`net/mac80211/mlme.c`). Then (R5c) the WPA2 four-way handshake, run
//! by the supplicant every radio shares (`godspeed_wifi::supplicant`) over this station as its `KeyPath`:
//! EAPOL frames go out as 802.11 data frames (`godspeed_wifi::data::to_80211`) and come in the same way,
//! and the two keys go into the chip's CAM (`rtl8188::install_key`). The frame path (R6), the rate
//! mask (R8), WMM and HT (R12) and the rekey (R7) follow below.

use core::cell::RefCell;

use godspeed as gs;
use gs::driver::wait::{Budget, Since};
use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::Scan;
use godspeed_wifi::rxq::RxQueue;
use godspeed_wifi::station::{Link, Outcome, Pulled, ScanStep, Secret, Station};
use godspeed_wifi::wire;

use godspeed_wifi::supplicant::{self, Handshake, KeyPath, Keys, Step};
use godspeed_wifi::{data, eapol, mgmt, usbfn};

use crate::{rtl8188, rtl_rx, rtl_tx, rx};

/// The channels swept: 1 to 13, the 2.4 GHz channels outside Japan's 14. Listening transmits nothing, so a
/// channel a regulatory domain does not allow us to TRANSMIT on is still one we may hear.
const FIRST: u8 = 1;
const LAST: u8 = 13;
/// How long each channel is listened to. A network beacons every 100 TU (102.4 ms) unless it says otherwise,
/// so this covers one interval with room for the hop itself; a network with a longer interval may be missed
/// on one sweep and heard on the next.
const DWELL_MS: u64 = 150;
/// The serve loop's bound on empty turns, each about a millisecond (`ScanStep::Empty` sleeps 1 ms). A sweep
/// is 13 dwells; three times that is room for a slow turn, and a sweep that runs past it is discarded loudly.
const EMPTY_BOUND: u32 = (LAST - FIRST + 1) as u32 * DWELL_MS as u32 * 3;
/// How long the join's find listens on each channel after its probe: an answer to a probe comes in
/// milliseconds, and a beacon is a bonus, so this is shorter than a sweep's dwell.
const FIND_DWELL_MS: u64 = 60;
/// mac80211's `IEEE80211_AUTH_TIMEOUT` and `IEEE80211_ASSOC_TIMEOUT` (`HZ / 5`), and their `_MAX_TRIES`.
const ANSWER_MS: u64 = 200;
const TRIES: u32 = 3;
/// The listen interval the association request offers, in beacon intervals. CHOSEN, not read: mac80211
/// takes it from the driver's configuration (`local->hw.conf.listen_interval`), and this station does not
/// sleep yet, so the value only tells the access point how long it may hold frames for us.
const LISTEN_INTERVAL: u16 = 10;
/// Deauthentication reason 3: "deauthenticated because sending STA is leaving".
const REASON_LEAVING: u16 = 3;
/// How long the handshake may take once associated. An access point resends message 1 when it hears no
/// message 2 and gives up after a few tries, so a wrong key shows as message 1 repeated (the supplicant
/// counts it) or as the access point leaving; this bound is for an access point that does neither. CHOSEN:
/// about four of the one-second retries an access point is commonly configured with, and room.
const HANDSHAKE_MS: u64 = 8_000;
/// The longest EAPOL frame this station takes in or sends, as ethernet: the supplicant's own message buffer.
// The longest EAPOL frame is `rx::EAPOL_MAX`, one fact for both sides.
use crate::rx::EAPOL_MAX;
/// Frame control first bytes of the two frames by which an access point ends an association.
const FC_DEAUTH: u8 = 0xC0;
const FC_DISASSOC: u8 = 0xA0;

/// An association that held: to whom, where, its ID, and the signal the find heard.
#[derive(Clone, Copy)]
struct Assoc {
    bssid: [u8; 6],
    channel: u8,
    aid: u16,
    rssi: i16,
    /// The access point's rates as the firmware's mask (`rtl_tx::rate_mask`, R8).
    rates: u32,
    /// A WMM (QoS) association (R12a): data goes as QoS data.
    wmm: bool,
    /// An HT association (R12b): the access point's HT capabilities, when both sides offered them.
    ht: Option<mgmt::HtCap>,
}

/// The network a join found: where it is, and what its beacon or probe response says about it.
#[derive(Clone, Copy)]
struct Found {
    bssid: [u8; 6],
    channel: u8,
    rssi: i16,
    capability: u16,
    /// `mgmt::rsn_is_ccmp`: `None` with no RSN element (an open or WEP network).
    ccmp: Option<bool>,
    rates: u32,
    /// The access point advertises WMM (`mgmt::has_wmm`).
    wmm: bool,
    /// Its HT capabilities (`mgmt::ht_cap`), when it is an 802.11n access point.
    ht: Option<mgmt::HtCap>,
}

/// The dongle, brought up: its address, the channel it rests on, and the sweep when one is running.
pub struct Dongle<'l> {
    /// The link shared with the receive side (`rx::Link`): the joined BSSID it filters on, and the frames
    /// it has taken for `pull`.
    link: &'l RefCell<rx::Link>,
    /// The CCMP packet number of the last protected frame sent; 0 before the first. mac80211 counts from 1
    /// with `atomic64_inc_return`, and a new pairwise key starts it again.
    pn: u64,
    /// Protected frames sent, and refused, for the log.
    sent: u32,
    send_failed: u32,
    mac: [u8; 6],
    /// The channel R3a tuned, and the one a sweep returns to.
    home: u8,
    sweep: Option<Hop>,
    /// Hops the RF chip did not take, this boot - each one is said once by the sweep that met it.
    hops_failed: u32,
    /// The 802.11 sequence number of the next frame sent, 12 bits.
    seq: u16,
    /// Probe requests the host took, and refused, in the sweep running now.
    probes_sent: u32,
    probes_refused: u32,
    /// How many transmit queues the dongle's endpoints serve (R2), which decides the endpoint a data frame
    /// is sent on (`rtl_tx::be_out`).
    queues: u8,
    /// The association, once one is up - with keys when it is JOINED.
    assoc: Option<Assoc>,
    /// The keys a WPA2 join keeps for the rekeys to come (`supplicant::Keys`); zeroed on every end.
    keys: Option<Keys>,
    /// The next CAM entry a key goes into: Linux takes the first free one, so the pairwise key is 0 and the
    /// group key 1 on a fresh join; and how many are in, to empty them on leaving.
    cam_next: u8,
    /// `wifi radio off` took (R6b): the RF is down and the receive filters closed.
    off: bool,
    /// For the power-off (R9).
    chip: rtl8188::Chip,
    /// The transmit power calibration, applied on every tune (R11).
    power: crate::rtl_power::TxPower,
    /// The firmware mailbox the next host-to-firmware command takes (`rtl8188::h2c`, R8).
    mbox: u8,
    /// The firmware was told the station is connected, so leaving tells it otherwise.
    reported: bool,
    /// The group key in each key-id slot, as installed - kept ONLY to refuse a reinstall of the same key,
    /// the group half of the KRACK attack: an access point, or someone replaying its frames, sending a
    /// group key the station already holds would, reinstalled, reset that key's replay counter. wpa_supplicant
    /// skips such a reinstall; so does this. Zeroed when the station leaves.
    gtk: [Option<[u8; 16]>; 4],
    /// The last `install_key` for a group key found it already installed; its `group_rsc` must not lower
    /// the counter.
    gtk_reinstall: bool,
    /// The CAM entry each group key slot uses, so a rekey into a slot already holding a key overwrites its
    /// entry rather than taking a new one - the CAM is bounded and the access point rekeys for as long as
    /// the station stays.
    gtk_entry: [Option<u8>; 4],
    /// The pairwise key is in the CAM: from then on every data frame sent - the EAPOL ones of a rekey
    /// included - goes protected. Decided by what the chip holds, not by what the supplicant keeps.
    ptk_in: bool,
    /// The key store was read back on this join's first undecrypted frame (`say_key_store`): once per
    /// join, so a link whose every frame is refused says it one time.
    keys_said: bool,
    /// `rx::Link::undecrypted` when this join began: that count lives as long as the instance, so the
    /// first undecrypted frame OF THIS JOIN is the first one above it.
    undecrypted_at_join: u32,
}

/// Where a sweep is: the channel tuned now, and when it was tuned.
struct Hop {
    channel: u8,
    since: Since,
}

impl<'l> Dongle<'l> {
    pub fn new(
        mac: [u8; 6], home: u8, queues: u8, chip: rtl8188::Chip, power: crate::rtl_power::TxPower,
        link: &'l RefCell<rx::Link>,
    ) -> Self {
        Dongle {
            chip,
            power,
            link, pn: 0, sent: 0, send_failed: 0,
            mac, home, sweep: None, hops_failed: 0, seq: 0, probes_sent: 0, probes_refused: 0, queues,
            assoc: None, keys: None, cam_next: 0, off: false, mbox: 0, reported: false, gtk: [None; 4], gtk_reinstall: false, gtk_entry: [None; 4], ptk_in: false, keys_said: false, undecrypted_at_join: 0,
        }
    }

    /// The dongle's own address, from its efuse.
    pub fn address(&self) -> [u8; 6] {
        self.mac
    }

    /// What the power-off needs to know about this chip (R9, `rtl8188::power_off`).
    pub fn chip(&self) -> rtl8188::Chip {
        self.chip
    }

    /// The next 802.11 sequence number, 12 bits.
    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = (self.seq + 1) & 0x0FFF;
        s
    }

    /// One management frame to the air: its descriptor (`rtl_tx::mgmt`, `group` for a broadcast) in front,
    /// on the host's bulk OUT for the management queue. `Err` with the host's reason.
    fn send_mgmt(&mut self, ctx: &ServiceContext, frame: &[u8], seq: u16, group: bool) -> Result<(), &'static str> {
        const MAX: usize = mgmt::ASSOC_REQUEST_MAX;
        if frame.len() > MAX {
            return Err("the frame is longer than this driver sends");
        }
        let desc = rtl_tx::mgmt(frame.len() as u16, seq, group);
        let mut req = [0u8; 2 + rtl_tx::TX_DESC_LEN + MAX];
        req[0] = usbfn::OP_BULK_OUT;
        req[1] = rtl_tx::MGNT_OUT;
        req[2..2 + rtl_tx::TX_DESC_LEN].copy_from_slice(&desc);
        req[2 + rtl_tx::TX_DESC_LEN..2 + rtl_tx::TX_DESC_LEN + frame.len()].copy_from_slice(frame);
        match crate::host(ctx, &req[..2 + rtl_tx::TX_DESC_LEN + frame.len()]) {
            Ok(r) => match r.payload_bytes() {
                [usbfn::OP_BULK_OUT, usbfn::ST_OK, ..] => Ok(()),
                [usbfn::OP_BULK_OUT, st, ..] => Err(match *st {
                    usbfn::ST_NO_DEVICE => "the dongle is no longer bound",
                    usbfn::ST_BAD_REQUEST => "the host has no such bulk OUT, or refused the transfer as malformed",
                    _ => "the device or the bus did not take it",
                }),
                _ => Err("the host answered something other than BULK_OUT - not speaking it yet"),
            },
            Err(why) => Err(why),
        }
    }

    /// One probe request on the channel tuned now - for any network (`ssid` empty: a sweep's) or one by
    /// name (a join's). Counted; the first refusal is said, with the host's status.
    fn probe(&mut self, ctx: &ServiceContext, ssid: &[u8]) {
        let mut frame = [0u8; mgmt::PROBE_REQUEST_MAX];
        let seq = self.next_seq();
        let n = mgmt::probe_request(&self.mac, seq, ssid, &mut frame);
        match self.send_mgmt(ctx, &frame[..n], seq, true) {
            Ok(()) => self.probes_sent = self.probes_sent.saturating_add(1),
            Err(why) => {
                if self.probes_refused == 0 {
                    ctx.log_fmt(format_args!("wifi-usb: a probe request was not sent - {}", why));
                }
                self.probes_refused = self.probes_refused.saturating_add(1);
            }
        }
    }

    /// The join's find: a probe naming `ssid` on each channel, and `FIND_DWELL_MS` listening for a beacon
    /// or probe response that carries the name. The strongest is kept - several access points may share a
    /// name. Read from the network's own channel element where it has one, since a neighbour's answer leaks
    /// onto the channel tuned.
    fn find(&mut self, ctx: &ServiceContext, ssid: &[u8]) -> Option<Found> {
        let mut best: Option<Found> = None;
        for ch in FIRST..=LAST {
            if !self.tune(ctx, ch) {
                continue;
            }
            self.probe(ctx, ssid);
            rx::wait_frames(ctx, FIND_DWELL_MS, &mut |pk: &rtl_rx::Packet| {
                if let Some(b) = mgmt::beacon(pk.frame) {
                    if b.ssid() == Some(ssid) {
                        let rssi = rtl_rx::rssi(pk.phy, pk.desc.rxmcs).unwrap_or(i16::MIN);
                        if best.map_or(true, |f| rssi > f.rssi) {
                            best = Some(Found {
                                bssid: b.bssid(),
                                channel: b.channel().unwrap_or(ch),
                                rssi,
                                capability: b.capability(),
                                ccmp: mgmt::rsn_is_ccmp(b.elements()),
                                rates: rtl_tx::rate_mask(b.elements()),
                                wmm: mgmt::has_wmm(b.elements()),
                                ht: mgmt::ht_cap(b.elements()),
                            });
                        }
                    }
                }
                // Never "done": the dwell is spent whole, so a stronger access point on this channel is
                // not missed for having answered second.
                false
            });
        }
        best
    }

    /// Wait for the answer `read` recognises, up to `ANSWER_MS`.
    fn answer<T: Copy>(ctx: &ServiceContext, read: &dyn Fn(&[u8]) -> Option<T>) -> Option<T> {
        let mut got = None;
        rx::wait_frames(ctx, ANSWER_MS, &mut |pk: &rtl_rx::Packet| {
            got = read(pk.frame);
            got.is_some()
        });
        got
    }

    /// Leave: a deauthentication to `bssid`, the BSSID cleared, and back on the channel the dongle rests on.
    fn leave(&mut self, ctx: &ServiceContext, bssid: &[u8; 6]) {
        let mut f = [0u8; mgmt::DEAUTH_LEN];
        let seq = self.next_seq();
        let n = mgmt::deauth(&self.mac, bssid, seq, REASON_LEAVING, &mut f);
        if let Err(why) = self.send_mgmt(ctx, &f[..n], seq, false) {
            ctx.log_fmt(format_args!("wifi-usb: the deauthentication was not sent - {}", why));
        }
        if self.reported {
            self.reported = false;
            if let Err(why) = rtl8188::left(ctx, &mut self.mbox) {
                ctx.log_fmt(format_args!("wifi-usb: the firmware was not told the station left - {}", why));
            }
        }
        let _ = rtl8188::set_bssid(ctx, &[0; 6]);
        self.drop_keys(ctx);
        self.assoc = None;
        {
            let mut l = self.link.borrow_mut();
            l.bssid = None;
            l.frames.clear();
        }
        self.home = FIRST;
        let _ = self.tune(ctx, FIRST);
    }

    /// The keys out: the kept ones zeroed (`supplicant::forget`) and every CAM entry this join filled emptied.
    /// THE KEY STORE, READ BACK (`docs/wifi-usb.md` 40) - an instrument for the card where a joined link
    /// took no frame: every one came up `security 0`, the chip finding no key, with both keys written.
    /// It says what the chip holds, not what this driver believes it wrote: the security enable in
    /// `REG_CR`, `REG_SECURITY_CFG` (0xcf is what `install_key` writes), and each CAM entry's control word
    /// - valid, group, key id, cipher - and address. Never the key itself.
    fn say_key_store(&self, ctx: &ServiceContext, when: &str) {
        match rtl8188::key_store(ctx) {
            Ok(k) => {
                ctx.log_fmt(format_args!(
                    "wifi-usb: key store {} - REG_CR={:#06x} (security {}), SECURITY_CFG={:#04x}",
                    when, k.cr, if k.cr & (1 << 9) != 0 { "on" } else { "OFF" }, k.sec_cfg));
                for (e, w) in k.cam.iter().enumerate() {
                    let c = w[0];
                    let m = [(c >> 16) as u8, (c >> 24) as u8, w[1] as u8, (w[1] >> 8) as u8, (w[1] >> 16) as u8, (w[1] >> 24) as u8];
                    ctx.log_fmt(format_args!(
                        "wifi-usb:   CAM entry {}: control {:#06x} - {}, {}, key id {}, cipher {}; address {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        e, c & 0xffff,
                        if c & (1 << 15) != 0 { "valid" } else { "NOT valid" },
                        if c & (1 << 6) != 0 { "group" } else { "pairwise" },
                        c & 0x3, (c >> 2) & 0x7, m[0], m[1], m[2], m[3], m[4], m[5]));
                }
            }
            Err(why) => ctx.log_fmt(format_args!("wifi-usb: key store {} - not read back: {}", when, why)),
        }
    }

    fn drop_keys(&mut self, ctx: &ServiceContext) {
        supplicant::forget(&mut self.keys);
        for g in self.gtk.iter_mut() {
            if let Some(k) = g.as_mut() {
                k.fill(0);
            }
            *g = None;
        }
        self.gtk_entry = [None; 4];
        self.ptk_in = false;
        for e in 0..self.cam_next {
            let _ = rtl8188::clear_key(ctx, e);
        }
        self.cam_next = 0;
    }

    /// The four-way handshake on the association `a`, with `pmk`: the supplicant reads each EAPOL frame from
    /// the access point and answers through this station (`KeyPath`), until it says the association has
    /// keys, or refused, or failed - or the access point ends it, or `HANDSHAKE_MS` passes.
    fn handshake(&mut self, ctx: &ServiceContext, a: Assoc, pmk: &[u8; 32]) -> Outcome {
        let mut hs = Handshake::new(*pmk, self.mac, "wifi-usb");
        let mut verdict: Option<Outcome> = None;
        let mut frames = 0u32;
        let us = self.mac;
        rx::wait_frames(ctx, HANDSHAKE_MS, &mut |pk: &rtl_rx::Packet| {
            let f = pk.frame;
            // The access point ending it: a deauthentication or disassociation from it, to us.
            if f.len() >= 26 && (f[0] == FC_DEAUTH || f[0] == FC_DISASSOC) && f[4..10] == us && f[10..16] == a.bssid {
                let reason = u16::from_le_bytes([f[24], f[25]]);
                verdict = Some(if hs.msg2_sent > 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-usb: join - the access point ended the association (reason {}) after {} answer(s) to message 1 - our key is not its key: INCORRECT PASSPHRASE",
                        reason, hs.msg2_sent));
                    Outcome::PassphraseRefused
                } else {
                    ctx.log_fmt(format_args!(
                        "wifi-usb: join - the access point ended the association (reason {}) before the handshake began", reason));
                    Outcome::Failed
                });
                return true;
            }
            // A handshake frame: an unprotected data frame from the access point, carrying EAPOL.
            if f.len() < 16 || f[1] & 0x40 != 0 || f[10..16] != a.bssid {
                return false;
            }
            let Some(d) = data::llc_payload(f, false) else { return false };
            if d.ethertype != eapol::ETHERTYPE_EAPOL || d.da != us {
                return false;
            }
            let mut eth = [0u8; EAPOL_MAX];
            let n = data::to_ethernet(&d, &mut eth);
            if n == 0 {
                return false;
            }
            frames += 1;
            match hs.on_key_frame(self, &eth[..n], ctx) {
                Step::Continue => false,
                Step::Joined(k) => {
                    self.keys = Some(k);
                    verdict = Some(Outcome::Joined);
                    true
                }
                Step::PassphraseRefused => {
                    verdict = Some(Outcome::PassphraseRefused);
                    true
                }
                Step::Failed => {
                    verdict = Some(Outcome::Failed);
                    true
                }
            }
        });
        verdict.unwrap_or_else(|| {
            ctx.log_fmt(format_args!(
                "wifi-usb: join - the handshake reached no verdict in {} ms ({} handshake frame(s), message 2 sent {} time(s))",
                HANDSHAKE_MS, frames, hs.msg2_sent));
            Outcome::Timeout
        })
    }

    /// R5b's join: find, authenticate, associate. `Ok` with the association when the access point accepted it; `Err` with the outcome to report and the line already said.
    fn associate(&mut self, ctx: &ServiceContext, ssid: &[u8], secret: Secret) -> Result<Assoc, Outcome> {
        let shown = core::str::from_utf8(ssid).unwrap_or("(not text)");
        self.probes_sent = 0;
        self.probes_refused = 0;
        let Some(net) = self.find(ctx, ssid) else {
            ctx.log_fmt(format_args!(
                "wifi-usb: join - no access point answered a probe for '{}' on channels {} to {} ({} probes sent)",
                shown, FIRST, LAST, self.probes_sent));
            let home = self.home;
            let _ = self.tune(ctx, home);
            return Err(Outcome::NotFound);
        };
        let b = net.bssid;
        ctx.log_fmt(format_args!(
            "wifi-usb: join - '{}' found at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} on channel {}, {} dBm, {}",
            shown, b[0], b[1], b[2], b[3], b[4], b[5], net.channel, net.rssi,
            match net.ccmp {
                Some(true) => "WPA2 with CCMP",
                Some(false) => "WPA2 WITHOUT CCMP for both ciphers",
                None => "no RSN element",
            }));
        // What the join is about to offer must be what the network takes: the RSN element R5c's message 2
        // repeats is CCMP for both ciphers (`eapol::RSN_IE`), and an open join offers none.
        let rsn: Option<&[u8]> = match (secret, net.ccmp) {
            (Secret::Pmk(_), Some(true)) => Some(&eapol::RSN_IE[..]),
            (Secret::Pmk(_), _) => {
                ctx.log("wifi-usb: join - a key was given, and this network does not offer CCMP for both ciphers, which is all this station speaks - not attempted");
                self.home_again(ctx);
                return Err(Outcome::Failed);
            }
            (Secret::Open, None) => None,
            (Secret::Open, Some(_)) => {
                ctx.log("wifi-usb: join - no key was given, and this network uses WPA2 - not attempted");
                self.home_again(ctx);
                return Err(Outcome::Failed);
            }
        };
        if !self.tune(ctx, net.channel) || rtl8188::set_bssid(ctx, &b).is_err() {
            ctx.log("wifi-usb: join - could not tune to the network's channel or set its BSSID");
            self.home_again(ctx);
            return Err(Outcome::Failed);
        }
        let us = self.mac;
        // ---- Authentication: Open System, transaction 1 out, 2 back. ----
        let mut auth_status = None;
        for _ in 0..TRIES {
            let mut f = [0u8; mgmt::AUTH_LEN];
            let seq = self.next_seq();
            let n = mgmt::auth_request(&us, &b, seq, &mut f);
            if let Err(why) = self.send_mgmt(ctx, &f[..n], seq, false) {
                ctx.log_fmt(format_args!("wifi-usb: join - the authentication was not sent - {}", why));
                break;
            }
            auth_status = Self::answer(ctx, &|fr: &[u8]| mgmt::auth_answer(fr, &b, &us));
            if auth_status.is_some() {
                break;
            }
        }
        match auth_status {
            Some(0) => ctx.log("wifi-usb: join - AUTHENTICATED (Open System, status 0)"),
            Some(st) => {
                ctx.log_fmt(format_args!("wifi-usb: join - the access point refused the authentication, status {}", st));
                self.leave(ctx, &b);
                return Err(Outcome::Failed);
            }
            None => {
                ctx.log_fmt(format_args!("wifi-usb: join - no authentication answer in {} tries of {} ms", TRIES, ANSWER_MS));
                self.leave(ctx, &b);
                return Err(Outcome::Timeout);
            }
        }
        // ---- Association. ----
        let mut cap = mgmt::CAP_ESS | mgmt::CAP_SHORT_PREAMBLE | mgmt::CAP_SHORT_SLOT_TIME;
        if net.capability & mgmt::CAP_PRIVACY != 0 {
            cap |= mgmt::CAP_PRIVACY;
        }
        let mut assoc = None;
        for _ in 0..TRIES {
            let mut f = [0u8; mgmt::ASSOC_REQUEST_MAX];
            let seq = self.next_seq();
            // R12a: a QoS station where the access point does WMM, as mac80211 associates - the 802.11n
            // rates the next card asks for are given only to one.
            // R12b: and an HT station where the access point is one too - only on a WMM association, since
            // an HT station is a QoS one. The HT element before the vendor one, in the standard's order.
            let mut extra_buf = [0u8; mgmt::HT_CAP.len() + mgmt::WMM_INFO.len()];
            let extra: &[u8] = match (net.wmm, net.ht.is_some()) {
                (true, true) => {
                    extra_buf[..mgmt::HT_CAP.len()].copy_from_slice(&mgmt::HT_CAP);
                    extra_buf[mgmt::HT_CAP.len()..].copy_from_slice(&mgmt::WMM_INFO);
                    &extra_buf
                }
                (true, false) => &mgmt::WMM_INFO,
                (false, _) => &[],
            };
            let Some(n) = mgmt::assoc_request(&us, &b, seq, cap, LISTEN_INTERVAL, ssid, rsn, extra, &mut f) else {
                break;
            };
            if let Err(why) = self.send_mgmt(ctx, &f[..n], seq, false) {
                ctx.log_fmt(format_args!("wifi-usb: join - the association request was not sent - {}", why));
                break;
            }
            assoc = Self::answer(ctx, &|fr: &[u8]| mgmt::assoc_answer(fr, &b, &us));
            if assoc.is_some() {
                break;
            }
        }
        match assoc {
            Some((0, aid)) => {
                ctx.log_fmt(format_args!("wifi-usb: join - {}",
                    if net.wmm { "the access point does WMM: associated as a QoS station, data goes as QoS data (R12a)" }
                    else { "the access point does no WMM: a non-QoS association, as before" }));
                let ht = if net.wmm { net.ht } else { None };
                if let Some(h) = ht {
                    ctx.log_fmt(format_args!(
                        "wifi-usb: join - an 802.11n access point: associated as an HT station; it receives MCS {:#04x}/{:#04x}, short GI {} (R12b)",
                        h.mcs[0], h.mcs[1], if h.cap & (mgmt::HT_CAP_SGI_20 | mgmt::HT_CAP_SGI_40) != 0 { "yes" } else { "no" }));
                }
                Ok(Assoc { bssid: b, channel: net.channel, aid, rssi: net.rssi, rates: net.rates, wmm: net.wmm, ht })
            }
            Some((st, _)) => {
                ctx.log_fmt(format_args!("wifi-usb: join - the access point refused the association, status {}", st));
                self.leave(ctx, &b);
                Err(Outcome::Failed)
            }
            None => {
                ctx.log_fmt(format_args!("wifi-usb: join - no association answer in {} tries of {} ms", TRIES, ANSWER_MS));
                self.leave(ctx, &b);
                Err(Outcome::Timeout)
            }
        }
    }

    /// Back on the channel the dongle rests on, after a join that went no further than the find.
    fn home_again(&mut self, ctx: &ServiceContext) {
        let home = self.home;
        let _ = self.tune(ctx, home);
    }

    /// Tune `channel` for the sweep; `false` when the RF chip did not take it, said here.
    fn tune(&mut self, ctx: &ServiceContext, channel: u8) -> bool {
        match rtl8188::set_channel(ctx, channel, &self.power) {
            Ok(()) => true,
            Err(why) => {
                self.hops_failed = self.hops_failed.saturating_add(1);
                ctx.log_fmt(format_args!("wifi-usb: the sweep could not tune channel {} - {}", channel, why));
                false
            }
        }
    }
}

impl Station for Dongle<'_> {
    fn scan_start(&mut self, ctx: &ServiceContext) -> bool {
        if !self.tune(ctx, FIRST) {
            return false;
        }
        self.probes_sent = 0;
        self.probes_refused = 0;
        self.probe(ctx, &[]);
        self.sweep = Some(Hop { channel: FIRST, since: Since::now(ctx) });
        ctx.log_fmt(format_args!(
            "wifi-usb: sweep started - listening on channels {} to {}, {} ms each", FIRST, LAST, DWELL_MS));
        true
    }

    fn scan_step(&mut self, scan: &mut Scan, ctx: &ServiceContext) -> ScanStep {
        let Some(hop) = self.sweep.as_ref() else {
            return ScanStep::Ended("no sweep was running");
        };
        if !hop.since.passed(ctx, Budget::ms(DWELL_MS)) {
            return ScanStep::Empty;
        }
        let next = hop.channel + 1;
        if next > LAST {
            self.sweep = None;
            let home = self.home;
            let back = self.tune(ctx, home);
            ctx.log_fmt(format_args!(
                "wifi-usb: sweep done - {} network(s) on channels {} to {}, {} probe request(s) sent, {} not{}",
                scan.count(), FIRST, LAST, self.probes_sent, self.probes_refused,
                if back { "; back on the channel it was on" } else { "; NOT back on the channel it was on (above)" }));
            return ScanStep::Ended("the last channel's dwell");
        }
        if self.tune(ctx, next) {
            self.probe(ctx, &[]);
        }
        // A channel the chip would not take is skipped, said once above, and the sweep goes on: the rest of
        // the band is still worth hearing - and is not probed, since a probe would go out on the wrong one.
        self.sweep = Some(Hop { channel: next, since: Since::now(ctx) });
        ScanStep::Frame
    }

    fn scan_abort(&mut self, ctx: &ServiceContext) -> bool {
        self.sweep = None;
        let home = self.home;
        self.tune(ctx, home)
    }

    fn scan_empty_bound(&self) -> u32 {
        EMPTY_BOUND
    }

    fn join(&mut self, ssid: &[u8], secret: Secret, ctx: &ServiceContext) -> Outcome {
        // A join replaces whatever this station was on: leave it first, so its keys do not outlive it.
        if let Some(old) = self.assoc {
            self.leave(ctx, &old.bssid);
        }
        self.cam_next = 0;
        self.keys_said = false;
        self.undecrypted_at_join = self.link.borrow().undecrypted;
        self.link.borrow_mut().new_keys();
        let a = match self.associate(ctx, ssid, secret) {
            Ok(a) => a,
            Err(outcome) => return outcome,
        };
        ctx.log_fmt(format_args!("wifi-usb: join - ASSOCIATED, association ID {}", a.aid));
        self.assoc = Some(a);
        // Sweeps return here now, not to channel 1: this is where the access point is.
        self.home = a.channel;
        let outcome = match secret {
            // An open network has no handshake: the association is the join.
            Secret::Open => Outcome::Joined,
            Secret::Pmk(pmk) => self.handshake(ctx, a, pmk),
        };
        if outcome == Outcome::Joined {
            self.pn = 0;
            self.link.borrow_mut().joined(a.bssid);
            // R8: the firmware told what the link is - the access point's rates, the association - so it
            // adapts the data rate within them, as Linux's `bss_info_changed` does once associated.
            // R12b: the access point's HT receive set into the mask, as `rtl8xxxu` builds it.
            let (mask, sgi) = match a.ht {
                Some(h) => (
                    // R12c: the two-stream rates (MCS 8-15) only where this chip has two transmit paths.
                    // `rtl8xxxu` puts the access point's whole receive set in; on a one-path chip the
                    // firmware could then pick rates it cannot send. The R12b card, with them in, ran a
                    // 1024-byte ping at 26-36 ms against R12a's 8-18 (26.14: the transmitter count is a
                    // fact of the device; recorded here as the deliberate difference).
                    a.rates | (h.mcs[0] as u32) << 12
                        | if self.power.tx_paths > 1 { (h.mcs[1] as u32) << 20 } else { 0 },
                    h.cap & (mgmt::HT_CAP_SGI_20 | mgmt::HT_CAP_SGI_40) != 0,
                ),
                None => (a.rates, false),
            };
            match rtl8188::joined(ctx, &mut self.mbox, mask, sgi, a.aid) {
                Ok(()) => {
                    self.reported = true;
                    ctx.log_fmt(format_args!(
                        "wifi-usb: join - the firmware has the rate mask {:#010x}{} and the association; it picks the data rate from here",
                        mask, if sgi { ", short GI" } else { "" }));
                }
                Err(why) => ctx.log_fmt(format_args!(
                    "wifi-usb: join - the firmware was not given the rates ({}) - data falls back to its own default", why)),
            }
            ctx.log("wifi-usb: join - JOINED; frames to and from the network go through nic-driver when the cable is out (R6)");
            self.say_key_store(ctx, "after the join");
        } else {
            self.leave(ctx, &a.bssid);
        }
        outcome
    }

    fn forget_keys(&mut self) {
        // The kept keys only: the CAM entries go when the association does (`leave`), which needs the bus.
        supplicant::forget(&mut self.keys);
    }

    fn disassoc(&mut self, ctx: &ServiceContext) -> bool {
        if let Some(a) = self.assoc {
            self.leave(ctx, &a.bssid);
        }
        true
    }

    /// `wifi radio off`. The serve loop has already left the network (it disassociates first); this takes
    /// the radio itself down (`rtl8188::radio_off`, R6b). Until R6b this was refused while the leave still
    /// happened - so the shell said the power command did not take on a station that had in fact left.
    fn radio_down(&mut self, ctx: &ServiceContext) -> bool {
        self.sweep = None;
        match rtl8188::radio_off(ctx) {
            Ok(()) => {
                self.off = true;
                ctx.log("wifi-usb: radio off - transmit paused, receive filters closed, the RF module down");
                true
            }
            Err(why) => {
                ctx.log_fmt(format_args!("wifi-usb: radio off did not complete - {}", why));
                false
            }
        }
    }

    /// `wifi radio on`: the RF back up and receive open, on the channel the dongle rests on; the serve loop
    /// then rejoins the network last joined.
    fn radio_up(&mut self, ctx: &ServiceContext) -> bool {
        match rtl8188::radio_on(ctx, self.home, &self.power) {
            Ok(()) => {
                self.off = false;
                ctx.log("wifi-usb: radio on - RF up, receive filters open");
                true
            }
            Err(why) => {
                ctx.log_fmt(format_args!("wifi-usb: radio on did not complete - {}", why));
                false
            }
        }
    }

    fn is_up(&mut self, _ctx: &ServiceContext) -> Option<bool> {
        Some(!self.off)
    }

    fn link(&mut self, _ctx: &ServiceContext) -> Option<Link> {
        // What this station holds, not a fresh reading from the chip: it does not yet track the access
        // point's beacons after the join (a lost link is not noticed yet: `pull` reports none), and the
        // signal is the find's.
        Some(match self.assoc {
            Some(a) => Link { bssid: a.bssid, rssi: a.rssi as i32, chanspec: a.channel as u16 },
            None => Link { bssid: [0; 6], rssi: 0, chanspec: 0 },
        })
    }

    fn mac(&mut self, _ctx: &ServiceContext) -> Option<[u8; 6]> {
        Some(self.mac)
    }

    fn tx_ok(&self) -> bool {
        self.assoc.is_some()
    }

    /// One ethernet frame from the stack, as an 802.11 data frame to the access point: protected for the
    /// chip to encrypt with the pairwise key (`rtl_tx::protected`, a fresh packet number, key id 0) when the
    /// join made keys, plain on an open network.
    fn send(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        let Some(a) = self.assoc else { return false };
        let keyed = self.ptk_in;
        const MAX: usize = godspeed_wifi::rxq::FRAME_MAX + data::DATA_OVERHEAD + data::QOS_CONTROL + data::CCMP_HEADER;
        let mut frame = [0u8; MAX];
        let seq = self.next_seq();
        let ccmp = if keyed {
            self.pn += 1;
            Some((self.pn, 0))
        } else {
            None
        };
        // TID 0, best effort, on a WMM association: the queue every frame here already goes on.
        let qos = a.wmm.then_some(0u8);
        let n = data::to_80211(eth, &a.bssid, seq, qos, ccmp, &mut frame);
        if n == 0 {
            return false;
        }
        let sgi = a.ht.is_some_and(|h| h.cap & (mgmt::HT_CAP_SGI_20 | mgmt::HT_CAP_SGI_40) != 0);
        let desc = rtl_tx::data(n as u16, seq, keyed, a.wmm, sgi);
        let mut req = [0u8; 2 + rtl_tx::TX_DESC_LEN + MAX];
        req[0] = usbfn::OP_BULK_OUT;
        req[1] = rtl_tx::be_out(self.queues);
        req[2..2 + rtl_tx::TX_DESC_LEN].copy_from_slice(&desc);
        req[2 + rtl_tx::TX_DESC_LEN..2 + rtl_tx::TX_DESC_LEN + n].copy_from_slice(&frame[..n]);
        let ok = matches!(crate::host(ctx, &req[..2 + rtl_tx::TX_DESC_LEN + n]),
                          Ok(r) if r.payload_bytes().get(..2) == Some(&[usbfn::OP_BULK_OUT, usbfn::ST_OK][..]));
        if ok {
            self.sent = self.sent.wrapping_add(1);
            if self.sent == 1 {
                ctx.log_fmt(format_args!(
                    "wifi-usb: the FIRST data frame sent through the link - {} bytes, {}; R6 transmit works",
                    n, if keyed { "encrypted by the chip (CCMP)" } else { "open network, unencrypted" }));
            }
        } else {
            self.send_failed = self.send_failed.wrapping_add(1);
            if self.send_failed == 1 || self.send_failed % 64 == 0 {
                ctx.log_fmt(format_args!("wifi-usb: a data frame was not sent (x{})", self.send_failed));
            }
        }
        ok
    }

    /// What the receive side has taken through the link since the last pull, into `rxq` - and a key frame
    /// waiting, answered (R7). The serve loop pulls at least every 250 ms while joined, read or not, so a
    /// group rekey is answered within that.
    fn pull(&mut self, rxq: &mut RxQueue, ctx: &ServiceContext) -> Pulled {
        let mut p = Pulled { data: 0, rekeyed: 0, rekey_failed: 0, pairwise_rekeyed: 0, pairwise_failed: 0, dropped_link: None };
        let mut key = [0u8; EAPOL_MAX];
        let mut key_len = 0usize;
        let addba;
        let undecrypted;
        {
            let mut l = self.link.borrow_mut();
            undecrypted = l.undecrypted;
            addba = l.addba.take().map(|r| (r, l.addbas));
            while rxq.has_room() {
                let mut b = [0u8; godspeed_wifi::rxq::FRAME_MAX];
                let n = l.frames.pop(&mut b);
                if n == 0 || !rxq.push(&b[..n]) {
                    break;
                }
                p.data += 1;
            }
            if l.rekey_len != 0 {
                key_len = l.rekey_len;
                key[..key_len].copy_from_slice(&l.rekey[..key_len]);
                l.rekey_len = 0;
            }
        }
        // The first frame of this join the chip did not decrypt: what the key store holds, once.
        if undecrypted != self.undecrypted_at_join && !self.keys_said {
            self.keys_said = true;
            self.say_key_store(ctx, "on the first frame the chip did not decrypt");
        }
        // R12b: an access point's ADDBA request, declined in kind (`mgmt::addba_decline`): this station does
        // not reorder aggregated frames, so the access point sends its frames to it unaggregated.
        if let (Some(((dialog, params), n)), Some(a)) = (addba, self.assoc) {
            let mut f = [0u8; mgmt::ADDBA_RESPONSE_LEN];
            let seq = self.next_seq();
            let len = mgmt::addba_decline(&self.mac, &a.bssid, seq, dialog, params, &mut f);
            match self.send_mgmt(ctx, &f[..len], seq, false) {
                Ok(()) if n == 1 => ctx.log_fmt(format_args!(
                    "wifi-usb: the access point asked to aggregate (ADDBA, TID {}) - declined, as a station that does not reorder; R12b",
                    (params >> 2) & 0x0F)),
                Ok(()) => {}
                Err(why) => ctx.log_fmt(format_args!("wifi-usb: the ADDBA decline was not sent - {}", why)),
            }
        }
        if key_len > 0 {
            // The supplicant every radio shares answers it, through this station's `KeyPath` (the new group
            // key into the CAM, the acknowledgement out protected) - as the AIC8800's pull does.
            let mut keys = self.keys.take();
            let r = supplicant::group_rekey(self, &key[..key_len], keys.as_mut(), "wifi-usb", ctx);
            if self.keys.is_none() {
                self.keys = keys;
            }
            match r {
                supplicant::Rekey::Answered => {
                    p.rekeyed += 1;
                    ctx.log("wifi-usb: the access point's group rekey answered - the new group key installed and acknowledged; R7 done");
                }
                supplicant::Rekey::Refused => p.rekey_failed += 1,
                supplicant::Rekey::Pairwise => {
                    p.pairwise_failed += 1;
                    ctx.log("wifi-usb: the access point restarted the four-way handshake on the live link (a pairwise rekey) - not answered by this station yet; it will drop the link, and `wifi join` returns");
                }
                supplicant::Rekey::NotAKey => {}
            }
        }
        p
    }

    fn event_name(&self, _code: u32) -> &'static str {
        "(this radio has no firmware events)"
    }

    // `wifi hardware usb`, after the host's facts: what the running chip holds. Nothing here asks the chip -
    // the address is the efuse's, read at bring-up, and the firmware is the file this service carries.
    fn details(&mut self, d: &mut godspeed_wifi::serve::Details, _live: bool, _ctx: &ServiceContext) {
        let m = self.mac;
        d.add("address", format_args!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (efuse)", m[0], m[1], m[2], m[3], m[4], m[5]));
        match crate::rtl_fw::header(crate::FIRMWARE) {
            Ok(h) => d.add("firmware", format_args!("rtl8192cufw_TMSC.bin, version {}.{}, running", h.major, h.minor)),
            Err(_) => d.add("firmware", format_args!("running")),
        }
        d.add("queues", format_args!("{} transmit queue(s) on the bulk OUTs", self.queues));
    }

    fn debug(&mut self, _sub: u8, _live: bool, out: &mut [u8], _ctx: &ServiceContext) -> usize {
        out[0] = wire::UNKNOWN_OP;
        1
    }
}

/// The supplicant's two needs, answered by this chip (R5c).
impl KeyPath for Dongle<'_> {
    /// An EAPOL frame as ethernet, sent as the 802.11 data frame to the access point
    /// (`godspeed_wifi::data::to_80211`) with its descriptor (`rtl_tx::eapol`), on the best-effort queue's
    /// endpoint.
    fn send_eapol(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        // Once the pairwise key is in, an EAPOL frame is data like any other and goes protected - a group
        // rekey's acknowledgement. Before it, the four-way handshake's own frames go plain (below).
        if self.ptk_in {
            return self.send(eth, ctx);
        }
        let Some(a) = self.assoc else { return false };
        let mut frame = [0u8; EAPOL_MAX + data::DATA_OVERHEAD];
        let seq = self.next_seq();
        let n = data::to_80211(eth, &a.bssid, seq, None, None, &mut frame);
        if n == 0 {
            return false;
        }
        let desc = rtl_tx::eapol(n as u16, seq);
        let mut req = [0u8; 2 + rtl_tx::TX_DESC_LEN + EAPOL_MAX + data::DATA_OVERHEAD];
        req[0] = usbfn::OP_BULK_OUT;
        req[1] = rtl_tx::be_out(self.queues);
        req[2..2 + rtl_tx::TX_DESC_LEN].copy_from_slice(&desc);
        req[2 + rtl_tx::TX_DESC_LEN..2 + rtl_tx::TX_DESC_LEN + n].copy_from_slice(&frame[..n]);
        match crate::host(ctx, &req[..2 + rtl_tx::TX_DESC_LEN + n]) {
            Ok(r) if r.payload_bytes().get(..2) == Some(&[usbfn::OP_BULK_OUT, usbfn::ST_OK][..]) => true,
            Ok(r) => {
                ctx.log_fmt(format_args!(
                    "wifi-usb: an EAPOL frame was not sent - the host answered status {:?}", r.payload_bytes().get(1)));
                false
            }
            Err(why) => {
                ctx.log_fmt(format_args!("wifi-usb: an EAPOL frame was not sent - {}", why));
                false
            }
        }
    }

    /// A CCMP key into the next CAM entry: against the peer for the pairwise key, against the BSSID for a
    /// group key, as `rtl8xxxu_set_key` does.
    fn install_key(&mut self, key_idx: u32, key: &[u8; 16], peer: Option<&[u8; 6]>, ctx: &ServiceContext) -> bool {
        let Some(a) = self.assoc else { return false };
        let (mac, group) = match peer {
            Some(p) => (*p, false),
            None => (a.bssid, true),
        };
        let slot = key_idx as usize & 3;
        self.gtk_reinstall = false;
        // A group key into a slot already holding one takes that slot's CAM entry back.
        let reuse = if group { self.gtk_entry[slot] } else { None };
        if group && self.gtk[slot] == Some(*key) {
            // THE SAME GROUP KEY AGAIN: not reinstalled, and its counter not reset (`group_rsc` below).
            self.gtk_reinstall = true;
            ctx.log_fmt(format_args!(
                "wifi-usb: group key {} is the one already installed - not reinstalled, its replay counter kept (KRACK)",
                key_idx));
            return true;
        }
        let entry = reuse.unwrap_or(self.cam_next);
        match rtl8188::install_key(ctx, entry, key_idx as u8, key, &mac, group) {
            Ok(()) => {
                if reuse.is_none() {
                    self.cam_next += 1;
                }
                if group {
                    self.gtk[slot] = Some(*key);
                    self.gtk_entry[slot] = Some(entry);
                } else {
                    self.ptk_in = true;
                }
                ctx.log_fmt(format_args!(
                    "wifi-usb: {} key {} in CAM entry {}", if group { "group" } else { "pairwise" }, key_idx, entry));
                true
            }
            Err(why) => {
                ctx.log_fmt(format_args!("wifi-usb: the key did not go into the CAM - {}", why));
                false
            }
        }
    }

    /// The group key's starting packet number (its Key RSC) into the receive side's replay counter: this
    /// host checks replay, because the chip does not (`rx::Link`).
    ///
    /// For a key just installed the counter is SET to the RSC; for a key already held it is never lowered.
    /// No frame can be taken between the install and this: both run in one call of the supplicant on this
    /// one thread, and a join's data frames are not taken until it completes.
    fn group_rsc(&mut self, key_idx: u32, rsc: u64, ctx: &ServiceContext) {
        let mut l = self.link.borrow_mut();
        if self.gtk_reinstall {
            l.group_rsc_at_least(key_idx, rsc);
            return;
        }
        l.group_rsc(key_idx, rsc);
        ctx.log_fmt(format_args!("wifi-usb: group key {} accepts packet numbers above {} (its Key RSC)", key_idx, rsc));
    }
}
