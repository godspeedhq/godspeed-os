// SPDX-License-Identifier: GPL-2.0-only
//! The AICSemi AIC8800D80 as a `Station` (`docs/wifi-aic8800.md`, phases V4 and V5): the serve loop the
//! Pi 4's Broadcom runs under drives this radio through the same trait, so `wifi scan`, `wifi join`, the
//! credential table and `/wifi.keys`, and the frame path to `nic-driver` are the loop's, unchanged.
//!
//! What is the chip's, and lives here, is how each of those is asked of this firmware. The messages are
//! the vendor runtime driver's (`aic8800_fdrv` at the commit `nonfree/aic8800d80/PROVENANCE` pins), read
//! as an executable datasheet (26.14): `SCANU_START_REQ` for a sweep, `SM_CONNECT_REQ` and its indication
//! for a join, `MM_KEY_ADD_REQ` and `ME_SET_CONTROL_PORT_REQ` once the host's handshake has keys,
//! `SM_DISCONNECT_REQ` to leave, `MM_GET_STA_INFO_REQ` for the signal, and data as a 28-byte host
//! descriptor in front of an ethernet payload. The WPA2 handshake is not here: it is every radio's
//! (`godspeed_wifi::supplicant`), and this module is its `KeyPath`.
//!
//! ## Where this diverges from the vendor driver, on purpose
//!
//! - **Polled, not interrupt-driven.** As everywhere in `aic.rs`: this host takes no interrupts, so the
//!   status register is read when the loop asks, and never otherwise.
//! - **No scan abort is sent.** The firmware's cancel message is only known by its position in the
//!   source's enum (`SCANU_CANCEL_REQ`, one past `SCANU_START_CFM_ADDTIONAL`), not from a call site that
//!   shows its parameters. A stopped sweep is therefore left to finish on the chip, and what it still sends
//!   is read and discarded. `scan_abort` says so by answering `false`, and the loop logs it.
//! - **Radio off is the interface's removal.** The vendor driver has no radio switch: `rwnx_close` removes
//!   the interface (`MM_REMOVE_IF_REQ`) and resets the MAC (`MM_RESET_REQ`), and `rwnx_open` builds them
//!   again. `radio_down` and `radio_up` are those two, and `is_up` is whether this driver holds an
//!   interface, because there is no message that asks the firmware.
//! - **The link's BSSID is this driver's record** of the last `SM_CONNECT_IND`, cleared by the firmware's
//!   `SM_DISCONNECT_IND` whenever one is read; the signal IS asked of the firmware each time. A disconnect
//!   the chip has not yet been read for is found at the next read, not before.
//! - **Data that arrives while a confirm is awaited is counted and dropped.** The vendor driver queues it;
//!   this one does not hold a second queue, and IP and the access point's EAPOL both retransmit.

use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::ServiceContext;
use godspeed_wifi::bss::{self, Network, Scan};
use godspeed_wifi::eapol;
use godspeed_wifi::rxq::RxQueue;
use godspeed_wifi::sdio::SdioHost;
use godspeed_wifi::station::{Link, Outcome, Pulled, ScanStep, Secret, Station};
use godspeed_wifi::supplicant::{self, Handshake, KeyPath, Keys, Rekey, Step};
use godspeed_wifi::wire as reply;
// A received 802.11 data frame as ethernet: every raw-frame radio's (`godspeed_wifi::data`).
use godspeed_wifi::data::{llc_payload, to_ethernet};

use crate::aic::{self, Packet, CFM_MAX, RX_BYTES, TASK_ME};
// Reads one pull makes at most: the Broadcom's bound, the same for this radio.
use crate::frames::PULL_MAX_READS;
use crate::aic_wire::{
    build_data_frame, channel_of, control_port, disconnect, key_add, parse_connect_ind, parse_result, rx_header,
    scanu_start, sm_connect, sta_info_rssi, FRAME_MAX, MM_GET_STA_INFO_CFM, MM_GET_STA_INFO_REQ, MM_KEY_ADD_CFM,
    MM_KEY_ADD_REQ, MM_REMOVE_IF_CFM, MM_REMOVE_IF_REQ, ME_SET_CONTROL_PORT_CFM, ME_SET_CONTROL_PORT_REQ, RX_DATA_HDR,
    SM_CONNECT_CFM, SM_CONNECT_IND, SM_CONNECT_REQ, SM_DISCONNECT_CFM, SM_DISCONNECT_IND, SM_DISCONNECT_REQ, TASK_SM,
};

/// The largest ethernet frame this module builds from a received one, and the EAPOL frames it keeps
/// across a walk: `eapol::MAX_KEY_FRAME` bounds every key frame the handshake reads.
const ETH_MAX: usize = 1600;
/// Networks remembered from the last sweep, for a join's BSSID and channel.
const SEEN_MAX: usize = 32;
/// Empty looks before a sweep the firmware never ended is given up. The loop sleeps a millisecond between
/// them and each is one CMD52, so this is somewhat over fifteen seconds; a sweep of both bands takes a few.
const SCAN_EMPTY_BOUND: u32 = 15_000;
/// How long a join waits for the firmware's decision and the access point's handshake: inside the shell's
/// thirty seconds, and longer than an access point's retries of message 1.
const JOIN_WAIT: Budget = Budget::ms(12_000);
/// A pairwise rekey's wait for the rest of the handshake, as the Broadcom's (`frames.rs`).
const REKEY_WAIT: Budget = Budget::ms(2_000);
/// A sweep a join waits out itself: a few seconds for both bands, with room.
const SWEEP_WAIT: Budget = Budget::ms(10_000);
/// A confirm's wait, as `aic::request_to`'s.
const CFM_WAIT: Budget = Budget::ms(2_000);
/// `WLAN_REASON_DEAUTH_LEAVING`, which `rwnx_close` sends.
const REASON_LEAVING: u16 = 3;

#[derive(Clone, Copy)]
struct Seen {
    ssid: [u8; 32],
    len: u8,
    bssid: [u8; 6],
    freq: u16,
    rssi: i16,
}

/// The association the firmware reported: who, its station index, where.
#[derive(Clone, Copy)]
struct Assoc {
    bssid: [u8; 6],
    ap_idx: u8,
    freq: u16,
}

/// Counters for `wifi debug`, in the Broadcom's layout where a field means the same thing.
#[derive(Default)]
struct Stats {
    sent: u32,
    confirmed: u32,
    refused: u32,
    unanswered: u32,
    msgs: u32,
    inds: u32,
    data: u32,
    data_dropped: u32,
    /// Data packets on the running link that `data_to_eth` turned away, and whether the first since the
    /// join has been shown (`refusal`).
    data_refused: u32,
    refusal_shown: bool,
    other: u32,
    tx_bytes: u32,
    rx_bytes: u32,
    last_ind: u32,
    last_ind_status: u32,
}

/// The `chanspec` the loop and the shell read - band in bits 15:14 (3 for 5 GHz), channel in the low 8 -
/// from a frequency.
fn chanspec_of(freq: u16) -> u16 {
    let band = if (4900..=5900).contains(&freq) { 3u16 << 14 } else { 0 };
    band | channel_of(freq) as u16
}

/// A received data packet as an ethernet frame in `out`: only what the vendor driver hands up (the
/// `upload` flag set, not a management frame), decrypted CCMP's header stepped over. 0 for anything else.
fn data_to_eth(pkt: &[u8], out: &mut [u8; ETH_MAX]) -> usize {
    let Some(h) = rx_header(pkt) else { return 0 };
    if !h.upload || h.mpdu {
        return 0;
    }
    let end = (RX_DATA_HDR + h.len).min(pkt.len());
    match llc_payload(&pkt[RX_DATA_HDR..end], h.ccmp) {
        Some(d) => to_ethernet(&d, out),
        None => 0,
    }
}

/// Why `data_to_eth` turned `pkt` away, for the log. A packet refused here was dropped without a word,
/// and the first boot with the loop reading the link (2026-10-05 10:23) read ten minutes of a joined
/// network and queued no frame at all - so every refusal is counted and the first one is shown.
fn refusal(pkt: &[u8]) -> &'static str {
    let Some(h) = rx_header(pkt) else { return "shorter than the receive header" };
    if !h.upload {
        return "upload flag clear";
    }
    if h.mpdu {
        return "a management frame (mpdu flag)";
    }
    let f = &pkt[RX_DATA_HDR.min(pkt.len())..];
    if f.len() < 24 || f[0] & 0x0c != 0x08 {
        return "not an 802.11 data frame";
    }
    if h.ccmp { "no LLC/SNAP after the CCMP header" } else { "no LLC/SNAP (decr_status did not say CCMP)" }
}

fn is_eapol(eth: &[u8]) -> bool {
    eth.len() >= 14 && u16::from_be_bytes([eth[12], eth[13]]) == eapol::ETHERTYPE_EAPOL
}

pub struct Aic<'a> {
    h: &'a dyn SdioHost,
    vif: Option<u8>,
    mac: [u8; 6],
    five_ghz: bool,
    version: [u8; 63],
    version_len: usize,
    seen: [Seen; SEEN_MAX],
    seen_n: usize,
    assoc: Option<Assoc>,
    /// A disconnect read while doing something else, for the next pull to report.
    dropped: Option<(u32, u32)>,
    keys: Option<Keys>,
    hostid: u32,
    st: Stats,
}

impl<'a> Aic<'a> {
    pub fn new(h: &'a dyn SdioHost, vif: u8, f: &aic::FwFacts) -> Self {
        Aic {
            h,
            vif: Some(vif),
            mac: f.mac,
            five_ghz: f.five_ghz,
            version: f.version,
            version_len: f.version_len,
            seen: [Seen { ssid: [0; 32], len: 0, bssid: [0; 6], freq: 0, rssi: 0 }; SEEN_MAX],
            seen_n: 0,
            assoc: None,
            dropped: None,
            keys: None,
            hostid: 0,
            st: Stats::default(),
        }
    }

    /// An indication read while waiting for something else. A disconnect is remembered for the next pull;
    /// a late scan message (a stopped sweep finishing) is dropped quietly; anything else is logged.
    fn note_ind(&mut self, id: u16, p: &[u8], ctx: &ServiceContext) {
        self.st.inds += 1;
        self.st.last_ind = id as u32;
        match id {
            SM_DISCONNECT_IND => {
                let reason = u16::from_le_bytes([p.first().copied().unwrap_or(0), p.get(1).copied().unwrap_or(0)]);
                self.st.last_ind_status = reason as u32;
                if self.assoc.take().is_some() {
                    self.dropped = Some((id as u32, reason as u32));
                    ctx.log_fmt(format_args!("wifi-driver: AIC the firmware reports the link DOWN (SM_DISCONNECT_IND, reason {})", reason));
                }
            }
            aic::SCANU_RESULT_IND | aic::SCANU_START_CFM | aic::SCANU_START_CFM_ADDITIONAL | aic::PER_CHANNEL_IND
            | aic::JOINED_CHANNEL_OUT | aic::JOINED_CHANNEL_BACK => {}
            _ => ctx.log_fmt(format_args!(
                "wifi-driver: AIC message {:#06x} ({} parameter bytes) that nothing here waits for - read and left", id, p.len())),
        }
    }

    /// One request and its confirm, with every other message read on the way handed to `note_ind` rather
    /// than lost - `aic::request_to`'s shape, for the running link.
    fn exchange(&mut self, id: u16, dest: u16, param: &[u8], cfm: u16, ctx: &ServiceContext) -> Option<(usize, [u8; CFM_MAX])> {
        self.st.sent += 1;
        if !aic::send_msg(self.h, id, dest, param, true, ctx) {
            self.st.refused += 1;
            return None;
        }
        let mut d = wait::Deadline::start(ctx, CFM_WAIT);
        loop {
            let mut bytes = [0u8; RX_BYTES];
            let n = aic::receive_bytes(self.h, &mut bytes, Budget::ms(200), true, ctx);
            if n > 0 {
                self.st.rx_bytes = self.st.rx_bytes.wrapping_add(n as u32);
                let mut found: Option<(usize, [u8; CFM_MAX])> = None;
                let mut inds: [(u16, [u8; 8], usize); 4] = [(0, [0; 8], 0); 4];
                let mut ninds = 0usize;
                let mut dropped_data = 0u32;
                aic::walk_packets(&bytes[..n], &mut |p| match p {
                    Packet::Msg(mid, params) if mid == cfm && found.is_none() => {
                        let mut c = [0u8; CFM_MAX];
                        let k = params.len().min(CFM_MAX);
                        c[..k].copy_from_slice(&params[..k]);
                        found = Some((params.len(), c));
                    }
                    Packet::Msg(mid, params) => {
                        // Kept short: an indication's first eight bytes carry everything `note_ind` reads.
                        if ninds < inds.len() {
                            let k = params.len().min(8);
                            inds[ninds].0 = mid;
                            inds[ninds].1[..k].copy_from_slice(&params[..k]);
                            inds[ninds].2 = params.len();
                            ninds += 1;
                        }
                    }
                    Packet::Data(_) => dropped_data += 1,
                }, ctx);
                self.st.msgs += ninds as u32 + found.is_some() as u32;
                self.st.data_dropped += dropped_data;
                for (mid, p, len) in inds.iter().take(ninds) {
                    self.note_ind(*mid, &p[..(*len).min(8)], ctx);
                }
                if let Some(r) = found {
                    self.st.confirmed += 1;
                    return Some(r);
                }
            }
            if d.expired() {
                self.st.unanswered += 1;
                ctx.log_fmt(format_args!("wifi-driver: AIC request {:#06x} - no {:#06x} confirm in {} ms", id, cfm, CFM_WAIT.as_us() / 1000));
                return None;
            }
        }
    }

    /// One ethernet frame to the access point: the host descriptor, the payload without its ethernet header,
    /// pushed under the chip's flow control. EAPOL is marked for a transmit confirm as `rwnx_tx_push` marks
    /// it (`hostid` bit 31); the confirm is read and passed over, as the vendor driver's bookkeeping only.
    fn send_eth(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        let (Some(a), Some(vif)) = (self.assoc, self.vif) else {
            ctx.log("wifi-driver: AIC a frame to send and no association to send it on - refused");
            return false;
        };
        if eth.len() < 14 {
            return false;
        }
        let mut da = [0u8; 6];
        da.copy_from_slice(&eth[0..6]);
        let mut sa = [0u8; 6];
        sa.copy_from_slice(&eth[6..12]);
        let ethertype = u16::from_be_bytes([eth[12], eth[13]]);
        self.hostid = self.hostid.wrapping_add(1) & 0x7fff_ffff;
        let hostid = if ethertype == eapol::ETHERTYPE_EAPOL { (1 << 31) | self.hostid } else { self.hostid };
        let mut frame = [0u8; FRAME_MAX];
        let Some(total) = build_data_frame(da, sa, ethertype, &eth[14..], vif, a.ap_idx, hostid, &mut frame) else {
            ctx.log_fmt(format_args!("wifi-driver: AIC a {}-byte frame does not fit the {}-byte bus frame - not sent", eth.len(), FRAME_MAX));
            return false;
        };
        let ok = aic::push_frame(self.h, &frame, total, 0xffff, ctx);
        if ok {
            self.st.tx_bytes = self.st.tx_bytes.wrapping_add(eth.len() as u32);
        }
        ok
    }

    /// Remember a network from a sweep, strongest per name, for a join to find its BSSID and channel.
    fn remember(&mut self, n: &Network, freq: u16) {
        let len = n.ssid_len as usize;
        if len == 0 {
            return;
        }
        for s in self.seen[..self.seen_n].iter_mut() {
            if s.len as usize == len && s.ssid[..len] == n.ssid[..len] {
                if n.rssi > s.rssi {
                    s.bssid = n.bssid;
                    s.freq = freq;
                    s.rssi = n.rssi;
                }
                return;
            }
        }
        if self.seen_n < SEEN_MAX {
            let mut s = Seen { ssid: [0; 32], len: n.ssid_len, bssid: n.bssid, freq, rssi: n.rssi };
            s.ssid[..len].copy_from_slice(&n.ssid[..len]);
            self.seen[self.seen_n] = s;
            self.seen_n += 1;
        }
    }

    fn find(&self, ssid: &[u8]) -> Option<Seen> {
        self.seen[..self.seen_n].iter().find(|s| s.len as usize == ssid.len() && &s.ssid[..ssid.len()] == ssid).copied()
    }

    /// One whole sweep, waited out here rather than stepped by the loop - for a join that needs a network
    /// the last sweep did not hear. Bounded by `SWEEP_WAIT`; what it hears goes to `seen` only.
    fn sweep_blocking(&mut self, ctx: &ServiceContext) {
        if !self.scan_start(ctx) {
            return;
        }
        let mut sc = Scan::new();
        let mut d = wait::Deadline::start(ctx, SWEEP_WAIT);
        loop {
            match self.scan_step(&mut sc, ctx) {
                ScanStep::Ended(_) => return,
                ScanStep::Frame => {}
                ScanStep::Empty => ctx.sleep_ms(1),
            }
            if d.expired() {
                ctx.log("wifi-driver: AIC the sweep for the join did not end in time - joining from what it heard");
                return;
            }
        }
    }

    /// A PAIRWISE REKEY on a live link, as the Broadcom's (`frames::pairwise_rekey`): the handshake run
    /// from the association's PMK, the frames that follow read until it completes or `REKEY_WAIT` runs out.
    /// The old keys stand if it does not complete.
    fn pairwise_rekey(&mut self, first: &[u8], q: &mut RxQueue, ctx: &ServiceContext) -> bool {
        let Some(old) = self.keys.take() else {
            ctx.log("wifi-driver: the access point began a four-way handshake and this driver holds no keys for it - not answered");
            return false;
        };
        ctx.log("wifi-driver: the access point began a NEW four-way handshake on the live link - answering (pairwise rekey)");
        let mut hs = Handshake::new(old.pmk, old.mac, "wifi-driver");
        let mut done: Option<bool> = match hs.on_key_frame(self, first, ctx) {
            Step::Continue => None,
            Step::Joined(k) => {
                self.keys = Some(k);
                Some(true)
            }
            Step::PassphraseRefused | Step::Failed => Some(false),
        };
        let mut d = wait::Deadline::start(ctx, REKEY_WAIT);
        while done.is_none() && !d.expired() {
            let mut bytes = [0u8; RX_BYTES];
            let n = aic::receive_bytes(self.h, &mut bytes, Budget::ms(100), true, ctx);
            let mut key_frames = [[0u8; ETH_MAX]; 2];
            let mut key_lens = [0usize; 2];
            let mut nkeys = 0usize;
            let mut inds: [(u16, u16); 4] = [(0, 0); 4];
            let mut ninds = 0usize;
            aic::walk_packets(&bytes[..n], &mut |p| match p {
                Packet::Data(pkt) => {
                    let mut eth = [0u8; ETH_MAX];
                    let k = data_to_eth(pkt, &mut eth);
                    if k > 0 && is_eapol(&eth[..k]) {
                        if nkeys < 2 {
                            key_frames[nkeys][..k].copy_from_slice(&eth[..k]);
                            key_lens[nkeys] = k;
                            nkeys += 1;
                        }
                    } else if k > 0 {
                        let _ = q.push(&eth[..k]);
                    }
                }
                Packet::Msg(mid, params) => {
                    if ninds < inds.len() {
                        inds[ninds] = (mid, u16::from_le_bytes([params.first().copied().unwrap_or(0), params.get(1).copied().unwrap_or(0)]));
                        ninds += 1;
                    }
                }
            }, ctx);
            for (mid, reason) in inds.iter().take(ninds) {
                self.note_ind(*mid, &reason.to_le_bytes(), ctx);
            }
            if self.assoc.is_none() {
                done = Some(false);
                break;
            }
            for i in 0..nkeys {
                match hs.on_key_frame(self, &key_frames[i][..key_lens[i]], ctx) {
                    Step::Continue => {}
                    Step::Joined(k) => {
                        self.keys = Some(k);
                        ctx.log("wifi-driver: pairwise rekey complete - new pairwise and group keys installed, the link continues");
                        done = Some(true);
                        break;
                    }
                    Step::PassphraseRefused | Step::Failed => {
                        done = Some(false);
                        break;
                    }
                }
            }
        }
        match done {
            Some(true) => true,
            _ => {
                if self.keys.is_none() && self.assoc.is_some() {
                    self.keys = Some(old);
                }
                if done.is_none() {
                    ctx.log("wifi-driver: the pairwise rekey did not complete inside its wait - the access point will decide the link");
                }
                false
            }
        }
    }
}

impl KeyPath for Aic<'_> {
    fn send_eapol(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        self.send_eth(eth, ctx)
    }
    /// `MM_KEY_ADD_REQ` as `rwnx_cfg80211_add_key` sends it: a pairwise key against the AP's station index
    /// with key index 0, a group key against `0xff` with its key id; the interface; CCMP.
    fn install_key(&mut self, key_idx: u32, key: &[u8; 16], peer: Option<&[u8; 6]>, ctx: &ServiceContext) -> bool {
        let (Some(a), Some(vif)) = (self.assoc, self.vif) else { return false };
        let pairwise = peer.is_some();
        let sta = if pairwise { a.ap_idx } else { 0xff };
        let idx = if pairwise { 0 } else { key_idx as u8 };
        match self.exchange(MM_KEY_ADD_REQ, aic::TASK_MM, &key_add(idx, sta, key, pairwise, vif), MM_KEY_ADD_CFM, ctx) {
            Some((_, c)) if c[0] == 0 => true,
            Some((_, c)) => {
                ctx.log_fmt(format_args!("wifi-driver: AIC refused the {} key - status {:#04x}", if pairwise { "pairwise" } else { "group" }, c[0]));
                false
            }
            None => false,
        }
    }
}

impl Station for Aic<'_> {
    fn scan_start(&mut self, ctx: &ServiceContext) -> bool {
        let Some(vif) = self.vif else { return false };
        self.seen_n = 0;
        self.st.sent += 1;
        aic::send_msg(self.h, aic::SCANU_START_REQ, aic::TASK_SCANU, &scanu_start(vif, self.five_ghz), false, ctx)
    }

    /// One look at the chip, never a wait: the loop sleeps between empty turns and answers requests between
    /// them. What the look finds is walked whole - several results, and the end, may share one read.
    fn scan_step(&mut self, sc: &mut Scan, ctx: &ServiceContext) -> ScanStep {
        let mut bytes = [0u8; RX_BYTES];
        let n = aic::receive_bytes(self.h, &mut bytes, Budget::ms(0), true, ctx);
        if n == 0 {
            return ScanStep::Empty;
        }
        self.st.rx_bytes = self.st.rx_bytes.wrapping_add(n as u32);
        let mut ended: Option<u8> = None;
        let mut heard: [(Network, u16); 16] = [(Network::blank(), 0); 16];
        let mut nheard = 0usize;
        let mut unreadable = 0u32;
        let mut inds: [(u16, u16); 4] = [(0, 0); 4];
        let mut ninds = 0usize;
        aic::walk_packets(&bytes[..n], &mut |p| match p {
            Packet::Msg(aic::SCANU_RESULT_IND, params) => match parse_result(params) {
                Some(r) => {
                    let mut net = Network::blank();
                    net.bssid = r.bssid();
                    if let Some(s) = r.ssid() {
                        net.ssid[..s.len()].copy_from_slice(s);
                        net.ssid_len = s.len() as u8;
                    }
                    net.chanspec = chanspec_of(r.freq);
                    net.rssi = r.rssi as i16;
                    net.security = bss::classify(r.ies(), r.capability());
                    sc.keep(net);
                    if nheard < heard.len() {
                        heard[nheard] = (net, r.freq);
                        nheard += 1;
                    }
                }
                None => unreadable += 1,
            },
            Packet::Msg(aic::SCANU_START_CFM, params) => ended = Some(params.get(1).copied().unwrap_or(0xff)),
            Packet::Msg(aic::SCANU_START_CFM_ADDITIONAL, _) => {}
            Packet::Msg(mid, params) => {
                if ninds < inds.len() {
                    inds[ninds] = (mid, u16::from_le_bytes([params.first().copied().unwrap_or(0), params.get(1).copied().unwrap_or(0)]));
                    ninds += 1;
                }
            }
            Packet::Data(_) => {}
        }, ctx);
        for (net, freq) in heard.iter().take(nheard) {
            self.remember(net, *freq);
        }
        for (mid, w) in inds.iter().take(ninds) {
            self.note_ind(*mid, &w.to_le_bytes(), ctx);
        }
        if unreadable > 0 {
            ctx.log_fmt(format_args!("wifi-driver: AIC {} scan result(s) could not be read as a beacon - skipped", unreadable));
        }
        match ended {
            Some(0) => ScanStep::Ended("the firmware's end-of-scan message"),
            Some(_) => ScanStep::Ended("the firmware's end-of-scan message, with a failure status"),
            None => ScanStep::Frame,
        }
    }

    fn scan_abort(&mut self, ctx: &ServiceContext) -> bool {
        ctx.log("wifi-driver: AIC no scan cancel is sent (its message is not confirmed from the source) - the firmware finishes the sweep and what it sends is discarded");
        false
    }

    fn scan_empty_bound(&self) -> u32 {
        SCAN_EMPTY_BOUND
    }

    fn join(&mut self, ssid: &[u8], secret: Secret, ctx: &ServiceContext) -> Outcome {
        supplicant::forget(&mut self.keys);
        let Some(vif) = self.vif else {
            ctx.log("wifi-driver: AIC join refused - no interface (the radio is off)");
            return Outcome::Failed;
        };
        if ssid.is_empty() || ssid.len() > 32 {
            ctx.log("wifi-driver: join refused before sending - the name length is out of range");
            return Outcome::Failed;
        }
        // The BSSID and channel a sweep heard for this name, as cfg80211 hands the vendor driver the BSS it
        // chose. A name the last sweep did not hear is swept for first (the loop's auto-join runs before
        // any `wifi scan`), and one that is still not heard is NOT FOUND - rather than handing the firmware
        // a broadcast BSSID and no channel, which the vendor driver's code permits but nothing here has
        // shown the firmware accepts.
        let known = match self.find(ssid) {
            Some(s) => Some(s),
            None => {
                ctx.log("wifi-driver: AIC the network is not in the last sweep - sweeping for it first");
                self.sweep_blocking(ctx);
                self.find(ssid)
            }
        };
        let Some(known) = known else {
            ctx.log("wifi-driver: AIC no access point with that name was heard - not found");
            return Outcome::NotFound;
        };
        let (bssid, freq) = (known.bssid, known.freq);
        let band = if (4900..=5900).contains(&freq) { 1 } else { 0 };
        let open = matches!(secret, Secret::Open);
        let ie: &[u8] = if open { &[] } else { &eapol::RSN_IE };
        let Some(req) = sm_connect(ssid, bssid, freq, band, vif, ie) else { return Outcome::Failed };
        self.assoc = None;
        self.dropped = None;
        self.st.sent += 1;
        if !aic::send_msg(self.h, SM_CONNECT_REQ, TASK_SM, &req, false, ctx) {
            return Outcome::Failed;
        }
        let mut hs = match secret {
            Secret::Pmk(p) => Some(Handshake::new(*p, self.mac, "wifi-driver")),
            Secret::Open => None,
        };
        let mut d = wait::Deadline::start(ctx, JOIN_WAIT);
        let mut key_frames_seen = 0u32;
        loop {
            let mut bytes = [0u8; RX_BYTES];
            let n = aic::receive_bytes(self.h, &mut bytes, Budget::ms(100), true, ctx);
            let mut cfm: Option<u8> = None;
            let mut ind: Option<crate::aic_wire::ConnectInd> = None;
            let mut short_ind = false;
            let mut gone: Option<u16> = None;
            let mut key_frames = [[0u8; ETH_MAX]; 2];
            let mut key_lens = [0usize; 2];
            let mut nkeys = 0usize;
            let mut other_data = 0u32;
            aic::walk_packets(&bytes[..n], &mut |p| match p {
                Packet::Msg(SM_CONNECT_CFM, params) => cfm = Some(params.first().copied().unwrap_or(0xff)),
                Packet::Msg(SM_CONNECT_IND, params) => match parse_connect_ind(params) {
                    Some(c) => ind = Some(c),
                    None => short_ind = true,
                },
                Packet::Msg(SM_DISCONNECT_IND, params) => {
                    gone = Some(u16::from_le_bytes([params.first().copied().unwrap_or(0), params.get(1).copied().unwrap_or(0)]))
                }
                Packet::Msg(mid, params) => ctx.log_fmt(format_args!(
                    "wifi-driver:   AIC message {:#06x} ({} parameter bytes) during the join", mid, params.len())),
                Packet::Data(pkt) => {
                    let mut eth = [0u8; ETH_MAX];
                    let k = data_to_eth(pkt, &mut eth);
                    if k > 0 && is_eapol(&eth[..k]) && nkeys < 2 {
                        key_frames[nkeys][..k].copy_from_slice(&eth[..k]);
                        key_lens[nkeys] = k;
                        nkeys += 1;
                    } else {
                        other_data += 1;
                    }
                }
            }, ctx);
            self.st.data_dropped += other_data;
            if let Some(status) = cfm {
                if status != 0 {
                    ctx.log_fmt(format_args!("wifi-driver: AIC refused the connect request - SM_CONNECT_CFM status {:#04x}", status));
                    return Outcome::Failed;
                }
                ctx.log("wifi-driver:   AIC connect request accepted - the firmware is authenticating and associating");
            }
            if short_ind {
                ctx.log("wifi-driver: AIC SM_CONNECT_IND arrived shorter than its structure - not joined");
                return Outcome::Failed;
            }
            if let Some(c) = ind {
                if c.status != 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: AIC the association failed - SM_CONNECT_IND status {} (802.11 status code; the firmware does not say 'not found' apart from it)",
                        c.status));
                    return Outcome::Failed;
                }
                let b = c.bssid;
                self.assoc = Some(Assoc { bssid: c.bssid, ap_idx: c.ap_idx, freq: c.freq });
                self.st.refusal_shown = false;
                ctx.log_fmt(format_args!(
                    "wifi-driver:   ASSOCIATED with {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} at {} MHz (station index {}, AID {})",
                    b[0], b[1], b[2], b[3], b[4], b[5], c.freq, c.ap_idx, c.aid));
                if open {
                    ctx.log("wifi-driver: JOINED - associated on an open network");
                    return Outcome::Joined;
                }
            }
            for i in 0..nkeys {
                key_frames_seen += 1;
                let Some(h) = hs.as_mut() else {
                    ctx.log("wifi-driver:   a handshake frame on an OPEN join - ignored; the network is not what the scan said it was");
                    continue;
                };
                if self.assoc.is_none() {
                    ctx.log("wifi-driver:   a handshake frame before the association was reported - ignored");
                    continue;
                }
                match h.on_key_frame(self, &key_frames[i][..key_lens[i]], ctx) {
                    Step::Continue => {}
                    Step::Joined(k) => {
                        let ap_idx = self.assoc.map_or(0, |a| a.ap_idx);
                        if self.exchange(ME_SET_CONTROL_PORT_REQ, TASK_ME, &control_port(ap_idx, true), ME_SET_CONTROL_PORT_CFM, ctx).is_none() {
                            ctx.log("wifi-driver: AIC the control port was not confirmed open - data would not flow; not joined");
                            return Outcome::Failed;
                        }
                        ctx.log("wifi-driver:   AIC control port open - data flows");
                        self.keys = Some(k);
                        return Outcome::Joined;
                    }
                    Step::PassphraseRefused => return Outcome::PassphraseRefused,
                    Step::Failed => return Outcome::Failed,
                }
            }
            if let Some(reason) = gone {
                self.assoc = None;
                let msg2 = hs.as_ref().map_or(0, |h| h.msg2_sent);
                return if msg2 > 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: deauthenticated (reason {}) after {} answer(s) to message 1 - our key is not its key: INCORRECT PASSPHRASE",
                        reason, msg2));
                    Outcome::PassphraseRefused
                } else {
                    ctx.log_fmt(format_args!("wifi-driver: AIC the link went down during the join (reason {}) before any answer", reason));
                    Outcome::Failed
                };
            }
            if d.expired() {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the join produced no decision in {} ms (associated: {}, {} handshake frame(s), message 2 sent {} time(s))",
                    JOIN_WAIT.as_us() / 1000, self.assoc.is_some(), key_frames_seen, hs.as_ref().map_or(0, |h| h.msg2_sent)));
                return Outcome::Timeout;
            }
        }
    }

    fn forget_keys(&mut self) {
        supplicant::forget(&mut self.keys);
    }

    fn disassoc(&mut self, ctx: &ServiceContext) -> bool {
        let Some(vif) = self.vif else { return false };
        let ok = self.exchange(SM_DISCONNECT_REQ, TASK_SM, &disconnect(REASON_LEAVING, vif), SM_DISCONNECT_CFM, ctx).is_some();
        self.assoc = None;
        self.dropped = None;
        ok
    }

    fn radio_down(&mut self, ctx: &ServiceContext) -> bool {
        let Some(vif) = self.vif else { return true };
        if self.assoc.is_some() {
            let _ = self.disassoc(ctx);
        }
        let removed = self.exchange(MM_REMOVE_IF_REQ, aic::TASK_MM, &[vif], MM_REMOVE_IF_CFM, ctx).is_some();
        let reset = self.exchange(aic::MM_RESET_REQ, aic::TASK_MM, &[], aic::MM_RESET_REQ + 1, ctx).is_some();
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC radio off as `rwnx_close` does it - interface removed {}, MAC reset {}",
            if removed { "confirmed" } else { "UNCONFIRMED" }, if reset { "confirmed" } else { "UNCONFIRMED" }));
        self.vif = None;
        removed && reset
    }

    fn radio_up(&mut self, ctx: &ServiceContext) -> bool {
        if self.vif.is_some() {
            return true;
        }
        match aic::bring_up_station(self.h, self.mac, self.five_ghz, ctx) {
            Some(i) => {
                self.vif = Some(i.index);
                true
            }
            None => false,
        }
    }

    fn is_up(&mut self, _ctx: &ServiceContext) -> Option<bool> {
        Some(self.vif.is_some())
    }

    fn link(&mut self, ctx: &ServiceContext) -> Option<Link> {
        let none = Link { bssid: [0; 6], rssi: 0, chanspec: 0 };
        let Some(a) = self.assoc else { return Some(none) };
        let (_, c) = self.exchange(MM_GET_STA_INFO_REQ, aic::TASK_MM, &[a.ap_idx], MM_GET_STA_INFO_CFM, ctx)?;
        // The exchange may have read the disconnect.
        if self.assoc.is_none() {
            return Some(none);
        }
        Some(Link { bssid: a.bssid, rssi: sta_info_rssi(&c).map_or(0, |r| r as i32), chanspec: chanspec_of(a.freq) })
    }

    fn mac(&mut self, _ctx: &ServiceContext) -> Option<[u8; 6]> {
        Some(self.mac)
    }

    /// Room is checked per frame by `push_frame` (the chip's flow-control register), so the answer here is
    /// always yes and a full chip refuses the send itself, loudly.
    fn tx_ok(&self) -> bool {
        true
    }

    fn send(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        self.send_eth(eth, ctx)
    }

    fn pull(&mut self, q: &mut RxQueue, ctx: &ServiceContext) -> Pulled {
        let mut got = Pulled { data: 0, rekeyed: 0, rekey_failed: 0, pairwise_rekeyed: 0, pairwise_failed: 0, dropped_link: self.dropped.take() };
        let mut reads = 0u32;
        while reads < PULL_MAX_READS && q.has_room() && got.dropped_link.is_none() {
            let mut bytes = [0u8; RX_BYTES];
            let n = aic::receive_bytes(self.h, &mut bytes, Budget::ms(0), true, ctx);
            if n == 0 {
                break;
            }
            reads += 1;
            self.st.rx_bytes = self.st.rx_bytes.wrapping_add(n as u32);
            // The rekey answers with frames of its own, so one EAPOL frame is copied out and handled after the
            // walk, as the Broadcom's pull does; a second in the same read is left to the access point's retry.
            let mut key_frame = [0u8; ETH_MAX];
            let mut key_len = 0usize;
            let mut inds: [(u16, u16); 4] = [(0, 0); 4];
            let mut ninds = 0usize;
            let mut queued = 0u32;
            let mut no_room = 0u32;
            let mut refused = 0u32;
            let mut first_refused = [0u8; 96];
            let mut first_refused_len = 0usize;
            aic::walk_packets(&bytes[..n], &mut |p| match p {
                Packet::Data(pkt) => {
                    let mut eth = [0u8; ETH_MAX];
                    let k = data_to_eth(pkt, &mut eth);
                    if k == 0 {
                        if refused == 0 {
                            first_refused_len = pkt.len().min(96);
                            first_refused[..first_refused_len].copy_from_slice(&pkt[..first_refused_len]);
                        }
                        refused += 1;
                        return;
                    }
                    if is_eapol(&eth[..k]) {
                        if key_len == 0 {
                            key_frame[..k].copy_from_slice(&eth[..k]);
                            key_len = k;
                        }
                    } else if q.push(&eth[..k]) {
                        queued += 1;
                    } else {
                        no_room += 1;
                    }
                }
                Packet::Msg(mid, params) => {
                    if ninds < inds.len() {
                        inds[ninds] = (mid, u16::from_le_bytes([params.first().copied().unwrap_or(0), params.get(1).copied().unwrap_or(0)]));
                        ninds += 1;
                    }
                }
            }, ctx);
            got.data += queued;
            self.st.data += queued;
            self.st.data_dropped += no_room;
            self.st.data_refused = self.st.data_refused.wrapping_add(refused);
            if refused > 0 && !self.st.refusal_shown {
                self.st.refusal_shown = true;
                let p = &first_refused[..first_refused_len];
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC a received data packet was not handed on - {}; header bytes 32..52 {:02x?}, frame begins {:02x?} (the first since the join; all are counted)",
                    refusal(p), &p[32.min(p.len())..52.min(p.len())], &p[RX_DATA_HDR.min(p.len())..]));
            }
            for (mid, w) in inds.iter().take(ninds) {
                self.note_ind(*mid, &w.to_le_bytes(), ctx);
            }
            if let Some(dl) = self.dropped.take() {
                got.dropped_link = Some(dl);
            }
            if key_len > 0 && got.dropped_link.is_none() {
                let mut keys = self.keys.take();
                let r = supplicant::group_rekey(self, &key_frame[..key_len], keys.as_mut(), "wifi-driver", ctx);
                if self.keys.is_none() {
                    self.keys = keys;
                }
                match r {
                    Rekey::Answered => got.rekeyed += 1,
                    Rekey::Refused => got.rekey_failed += 1,
                    Rekey::Pairwise => {
                        if self.pairwise_rekey(&key_frame[..key_len], q, ctx) {
                            got.pairwise_rekeyed += 1;
                        } else {
                            got.pairwise_failed += 1;
                        }
                    }
                    Rekey::NotAKey => {}
                }
            }
        }
        got
    }

    fn event_name(&self, code: u32) -> &'static str {
        match code as u16 {
            SM_DISCONNECT_IND => "SM_DISCONNECT_IND",
            _ => "an AIC8800 indication",
        }
    }

    /// `wifi debug`, in the Broadcom's reply layout (`wire::dbg`): no trace ring is kept for this radio,
    /// so the trace is empty; the firmware's version and address are the ones its bring-up read; the
    /// counters are this module's, at the Broadcom's positions where a field means the same thing and zero
    /// where it has no meaning here (superframes, event buckets, refused commands, the session clock).
    fn debug(&mut self, sub: u8, _live: bool, out: &mut [u8], _ctx: &ServiceContext) -> usize {
        out[0] = reply::OK;
        match sub {
            reply::dbg::TRACE => {
                out[1] = 0;
                2
            }
            reply::dbg::FIRMWARE => {
                let mut at = 1;
                out[at] = self.version_len as u8;
                at += 1;
                out[at..at + 128].fill(0);
                out[at..at + self.version_len].copy_from_slice(&self.version[..self.version_len]);
                at += 128;
                out[at..at + 2].copy_from_slice(&0u16.to_le_bytes());
                at += 2;
                out[at..at + 512].fill(0);
                at += 512;
                out[at..at + 6].copy_from_slice(&self.mac);
                at + 6
            }
            _ => {
                let s = &self.st;
                let words: [u32; 30] = [
                    s.sent, s.confirmed, s.refused, s.unanswered,
                    s.msgs, s.inds, s.data, s.data_refused, 0, s.other,
                    s.tx_bytes, s.rx_bytes, s.data_dropped,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    s.last_ind, s.last_ind_status, 0, 0,
                    0, 0, 0,
                ];
                let mut at = 1;
                for w in words.iter() {
                    out[at..at + 4].copy_from_slice(&w.to_le_bytes());
                    at += 4;
                }
                at
            }
        }
    }
}
