// SPDX-License-Identifier: GPL-2.0-only
//! The frame interface, served to `nic-driver`: ops `0x10` INFO, `0x11` TX and `0x12` RX - the three
//! `dwc2` serves it on the Pi 2, and the three it serves `net-stack` upward (`docs/wifi.md` 2). Once a
//! station is associated the radio is a frame source, `nic-driver` is the link front end, and this is its
//! fifth backend. Nothing above it changes shape.
//!
//! What lives here is the pull that fills the shared receive queue (`godspeed_wifi::rxq`). The driver blocks in `recv` when it is
//! idle, so a frame the access point sends waits in the chip until somebody asks. `nic-driver` asks on
//! `net-stack`'s pace, and each ask reads what the chip has waiting - at most `PULL_MAX_READS` frames -
//! into a bounded queue (26.6.1), watching the link as it goes: a `LINK` event without its up bit, or a
//! deauthentication, means the access point dropped us, and the join is forgotten then and there rather
//! than at the next `wifi status`.
//!
//! The pull is also where the access point's GROUP-KEY REKEY is answered (`docs/wifi.md` 42). A WPA2
//! access point replaces the group key on a timer - an EAPOL-Key frame with the new key wrapped under the
//! KEK - and a station that does not answer is deauthenticated at its retry budget. The join keeps the
//! confirmation and encryption halves of its PTK (`join::Keys`) for exactly this; the frame is verified,
//! unwrapped, the key installed and the acknowledgement sent, all with the same primitives the four-way
//! handshake used, and each step from OpenBSD's `ieee80211_recv_rsn_group_msg1` / `ieee80211_send_group_msg2`.
//!
//! Transmit is `ctrl::send_data`, the path the handshake proved. Credit for it comes back on RECEIVED
//! frames (`Session::tx_max_seq`), so a stack that only sends would run dry; a dry window pulls first.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl::{self, Session};
use crate::eapol;
use godspeed_wifi::sdio::SdioHost;
use crate::join::{BcmPath, Handshake, Keys, Step, EVENT_MSG_LINK};
use crate::scan::{self, code, ev, CHANNEL_DATA, CHANNEL_EVENT, CHANNEL_MASK};

/// `[0x10]` -> `[0x10, ok, mac(6), link, peer(6)]`; `peer` is the access point, zeros when not known.
pub const OP_NET_INFO: u8 = 0x10;
/// `[0x11, ethernet frame...]` -> `[0x11, sent]`.
pub const OP_NET_TX: u8 = 0x11;
/// `[0x12]` -> `[0x12, len_lo, len_hi, ethernet frame...]`; a length of 0 is "nothing waiting".
pub const OP_NET_RX: u8 = 0x12;

// The received-frame queue and what a pull reports are every radio's (`godspeed_wifi::rxq`, `::station`).
pub use godspeed_wifi::rxq::RxQueue;
pub use godspeed_wifi::station::Pulled;

/// Frames read off the chip in one pull. A bound in READS, not time: each read is one SDIO header fetch
/// and, when a frame is there, its body; the queue's room bounds it again from the other side.
pub const PULL_MAX_READS: u32 = 8;



/// Read what the chip has waiting, queueing data frames, answering a group-key rekey, and watching the
/// link.
///
/// The same walk as the join loop's: every frame is noted for the trace and stats, split into its
/// sub-frames if it is a superframe, and each one is either a DATA frame (an ethernet frame behind a BDC
/// header - queued, unless it is EAPOL, which is the rekey path) or an EVENT (read for the link). Anything
/// else is counted by `note_frame` and left.
pub fn pull(
    h: &dyn SdioHost,
    w: &mut Window,
    s: &mut Session,
    q: &mut RxQueue,
    frame: &mut [u8; ctrl::FRAME],
    mut keys: Option<&mut Keys>,
    ctx: &ServiceContext,
) -> Pulled {
    let mut got = Pulled { data: 0, rekeyed: 0, rekey_failed: 0, pairwise_rekeyed: 0, pairwise_failed: 0, dropped_link: None };
    let mut reads = 0u32;
    while reads < PULL_MAX_READS && q.has_room() {
        let f = match ctrl::read_frame(h, w, frame, ctx) {
            Some(f) => f,
            None => break,
        };
        reads += 1;
        s.note_frame(ctx, &f, &frame[..], false);
        let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
        let nsubs = ctrl::subframes(&f, &frame[..], s.glom_descriptor(), &mut subs, ctx);
        // The rekey answers with frames of its own, and `send_data` needs the session - so an EAPOL frame
        // is copied out of the read buffer and handled after the walk, once per pull. One is all a rekey
        // ever carries; a second in the same read is left for the next pull by being counted, not lost:
        // the access point retries.
        let mut eapol_frame = [0u8; eapol::MAX_KEY_FRAME];
        let mut eapol_len = 0usize;
        for sub in subs.iter().take(nsubs) {
            let channel = sub.chanflag & CHANNEL_MASK;
            let body = &frame[sub.off..sub.off + sub.len];
            if channel == CHANNEL_DATA {
                let eth = match scan::ethernet_at(body) {
                    Some(eth) => eth,
                    None => continue,
                };
                let eth_frame = &body[eth..];
                if eth_frame.len() < ev::ETHHDR {
                    continue;
                }
                let ethertype = u16::from_be_bytes([eth_frame[ev::ETHERTYPE], eth_frame[ev::ETHERTYPE + 1]]);
                if ethertype == eapol::ETHERTYPE_EAPOL {
                    if eapol_len == 0 && eth_frame.len() <= eapol_frame.len() {
                        eapol_frame[..eth_frame.len()].copy_from_slice(eth_frame);
                        eapol_len = eth_frame.len();
                    }
                    continue;
                }
                if q.push(eth_frame) {
                    got.data += 1;
                }
            } else if channel == CHANNEL_EVENT {
                let e = match scan::parse_event(body, s.stats.rx_event, ctx) {
                    Some(e) => e,
                    None => continue,
                };
                match e.event_type {
                    code::LINK if e.flags & EVENT_MSG_LINK == 0 => {
                        got.dropped_link = Some((e.event_type, e.reason));
                    }
                    code::DEAUTH | code::DEAUTH_IND | code::DISASSOC | code::DISASSOC_IND => {
                        got.dropped_link = Some((e.event_type, e.reason));
                    }
                    _ => {}
                }
            }
        }
        if eapol_len > 0 {
            match group_rekey(&mut BcmPath { h, w: &mut *w, s: &mut *s }, &eapol_frame[..eapol_len], keys.as_deref_mut(), ctx) {
                Rekey::Answered => got.rekeyed += 1,
                Rekey::Refused => got.rekey_failed += 1,
                Rekey::Pairwise => {
                    // The access point restarted the four-way handshake. It is run here to completion,
                    // reading the frames that follow; data frames that arrive meanwhile are queued as
                    // they would be by any pull.
                    if pairwise_rekey(h, w, s, q, frame, &eapol_frame[..eapol_len], keys.as_deref_mut(), ctx) {
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

// The rekey itself is every radio's (`godspeed_wifi::supplicant::group_rekey`); the Broadcom supplies its
// `KeyPath` (`join::BcmPath`).
use godspeed_wifi::supplicant::{group_rekey, Rekey};

/// Reads of the count register before a pairwise rekey gives up: frames are polled a millisecond apart, so
/// this is about two seconds - the access point's own retry window is longer.
const REKEY_EMPTY_POLLS: u32 = 2_000;

/// A PAIRWISE REKEY: message 1 of a new four-way handshake arrived on a live link. Answer it and run the
/// handshake to message 4 with the same state machine the join uses (`join::Handshake`), from the PMK the
/// association was made with. The keys are replaced in place on success; the access point drops the link
/// on failure and the log names the step.
fn pairwise_rekey(
    h: &dyn SdioHost,
    w: &mut Window,
    s: &mut Session,
    q: &mut RxQueue,
    frame: &mut [u8; ctrl::FRAME],
    first: &[u8],
    keys: Option<&mut Keys>,
    ctx: &ServiceContext,
) -> bool {
    let keys = match keys {
        Some(k) => k,
        None => {
            ctx.log("wifi-driver: the access point began a four-way handshake and this driver holds no keys for it - not answered");
            return false;
        }
    };
    ctx.log("wifi-driver: the access point began a NEW four-way handshake on the live link - answering (pairwise rekey)");
    let mut hs = Handshake::new(keys.pmk, keys.mac);
    match hs.on_key_frame(&mut BcmPath { h, w: &mut *w, s: &mut *s }, first, ctx) {
        Step::Continue => {}
        Step::Joined(k) => { ctrl::report_power_mode(h, w, s, ctx); *keys = k; return true; }
        Step::PassphraseRefused | Step::Failed => return false,
    }
    let mut empty = 0u32;
    while empty < REKEY_EMPTY_POLLS {
        let f = match ctrl::read_frame(h, w, frame, ctx) {
            Some(f) => f,
            None => {
                empty += 1;
                ctx.sleep_ms(1);
                continue;
            }
        };
        s.note_frame(ctx, &f, &frame[..], false);
        let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
        let nsubs = ctrl::subframes(&f, &frame[..], s.glom_descriptor(), &mut subs, ctx);
        for sub in subs.iter().take(nsubs) {
            let channel = sub.chanflag & CHANNEL_MASK;
            let body = &frame[sub.off..sub.off + sub.len];
            if channel == CHANNEL_DATA {
                let eth = match scan::ethernet_at(body) {
                    Some(eth) => eth,
                    None => continue,
                };
                let eth_frame = &body[eth..];
                if eth_frame.len() < ev::ETHHDR {
                    continue;
                }
                let ethertype = u16::from_be_bytes([eth_frame[ev::ETHERTYPE], eth_frame[ev::ETHERTYPE + 1]]);
                if ethertype != eapol::ETHERTYPE_EAPOL {
                    let _ = q.push(eth_frame);
                    continue;
                }
                match hs.on_key_frame(&mut BcmPath { h, w: &mut *w, s: &mut *s }, eth_frame, ctx) {
                    Step::Continue => {}
                    Step::Joined(k) => {
                        ctrl::report_power_mode(h, w, s, ctx);
                        *keys = k;
                        ctx.log("wifi-driver: pairwise rekey complete - new pairwise and group keys installed, the link continues");
                        return true;
                    }
                    Step::PassphraseRefused | Step::Failed => return false,
                }
            } else if channel == CHANNEL_EVENT {
                if let Some(e) = scan::parse_event(body, s.stats.rx_event, ctx) {
                    match e.event_type {
                        code::LINK if e.flags & EVENT_MSG_LINK == 0 => return false,
                        code::DEAUTH | code::DEAUTH_IND | code::DISASSOC | code::DISASSOC_IND => return false,
                        _ => {}
                    }
                }
            }
        }
    }
    ctx.log("wifi-driver: the pairwise rekey did not complete inside its wait - the access point will decide the link");
    false
}
