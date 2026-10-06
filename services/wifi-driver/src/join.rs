// SPDX-License-Identifier: GPL-2.0-only
//! Joining a network: association by the firmware, the WPA2 4-way handshake by the host.
//!
//! This firmware has no supplicant. `sup_wpa` - the switch that would hand it the 4-way handshake - is
//! refused `BCME_UNSUPPORTED` in every form, including the GET Linux's own feature detection rests on
//! (`docs/wifi.md` §37). So the model here is OpenBSD's, which sets `sup_wpa 0` on purpose and does the
//! handshake in net80211: the firmware authenticates and associates, the access point's EAPOL-Key frames
//! arrive on the DATA channel, the host derives the keys, answers, and installs them with the `wsec_key`
//! iovar. The reference for every step is named at the step.
//!
//! ## The sequence, quoted from `bwfm_connect`
//!
//! ```c
//! /* tell firmware to add WPA/RSN IE to (re)assoc request */
//! frm = ieee80211_add_rsn(buf, ic, ic->ic_bss);
//! bwfm_fwvar_var_set_data(sc, "wpaie", buf, frm - buf);
//! ...
//! wpa |= BWFM_WPA_AUTH_WPA2_PSK;            /* (1 << 7) */
//! wsec |= BWFM_WSEC_AES;                    /* (1 << 2) */
//! bwfm_fwvar_var_set_int(sc, "wpa_auth", wpa);
//! bwfm_fwvar_var_set_int(sc, "wsec", wsec);
//! bwfm_fwvar_var_set_int(sc, "auth", BWFM_AUTH_OPEN);   /* 0 */
//! bwfm_fwvar_var_set_int(sc, "mfp", BWFM_MFP_NONE);     /* 0 */
//! ...
//! bwfm_fwvar_cmd_set_data(sc, BWFM_C_SET_SSID, &join, sizeof(join));
//! ```
//!
//! `wpa_auth`, `wsec` and `auth` go as the integer commands 165, 134 and 22 - brcmfmac's `fwil.h` and
//! cyw43-driver agree on the numbers, and all three were accepted on this hardware on 2026-09-28. `wpaie`
//! and `mfp` are iovars. `SET_SSID` (26) carries `bwfm_ssid { uint32_t len; uint8_t ssid[32]; }` - 36
//! bytes, the form cyw43-driver sends, which lets the firmware choose the access point.
//!
//! ## The handshake, quoted from net80211
//!
//! Message 1 arrives with `PAIRWISE | KEYACK` and the ANonce (`ieee80211_recv_4way_msg1`). The station
//! draws an SNonce, derives `PTK = PRF-384(PMK, "Pairwise key expansion", Min(AA,SPA) || Max(AA,SPA) ||
//! Min(ANonce,SNonce) || Max(ANonce,SNonce))` (`ieee80211_derive_ptk`), and answers with message 2:
//! `PAIRWISE | KEYMIC`, the same replay counter, the SNonce, and the RSN element it put in its association
//! request as key data, MIC'd with the KCK (`ieee80211_send_4way_msg2`). Message 3 arrives with
//! `PAIRWISE | KEYACK | KEYMIC | INSTALL | ENCRYPTED`: its ANonce must match, its MIC must verify under the
//! KCK, and its key data - AES-key-wrapped under the KEK - holds the group key as a KDE
//! (`ieee80211_recv_4way_msg3`). Message 4 is `PAIRWISE | KEYMIC | SECURE`, empty, MIC'd
//! (`ieee80211_send_4way_msg4`). Then the pairwise key (index 0, the access point's address) and the
//! group key (its key id, `WSEC_PRIMARY_KEY`) are installed (`bwfm_set_key_cb`).
//!
//! ## How a wrong passphrase presents
//!
//! The access point never says so. It receives message 2, cannot verify a MIC made with the wrong PMK,
//! and does the only thing the standard gives it: repeats message 1, then deauthenticates with reason 15
//! (4-way handshake timeout). So "incorrect passphrase" is decided HERE: message 1 arriving again after
//! message 2 was sent, or the deauthentication after it. Nothing else produces that pattern, which is what
//! makes the sentence honest rather than a guess (`utilities/56_wifi.md` §2).
//!
//! ## The passphrase
//!
//! It never reaches this module. The serve loop turns it into the pairwise master key the moment it arrives
//! (`crypto::psk`) and keeps only the key (`utilities/56_wifi.md` §6). `Secret::Pmk` is that key.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl::{self, Session};
use crate::eapol;
use godspeed_wifi::sdio::SdioHost;
use crate::scan::{self, code, ev, status, CHANNEL_DATA, CHANNEL_EVENT, CHANNEL_MASK};

/// `WLC_SET_AUTH` / `BRCMF_C_SET_AUTH`.
const CMD_SET_AUTH: u32 = 22;
/// `WLC_SET_SSID` / `BRCMF_C_SET_SSID` / `BWFM_C_SET_SSID`.
const CMD_SET_SSID: u32 = 26;
/// `WLC_SET_WSEC` / `BRCMF_C_SET_WSEC`.
const CMD_SET_WSEC: u32 = 134;
/// `WLC_SET_WPA_AUTH`.
const CMD_SET_WPA_AUTH: u32 = 165;

/// `BWFM_WPA_AUTH_WPA2_PSK` - `(1 << 7)`.
const WPA2_AUTH_PSK: u32 = 0x0080;
/// `BWFM_WSEC_AES` - `(1 << 2)`.
const WSEC_AES: u32 = 0x0004;
/// `BWFM_WPA_AUTH_DISABLED` - `(0 << 0)`: an open network.
const WPA_AUTH_DISABLED: u32 = 0;
/// `BWFM_WSEC_NONE` - `(0 << 0)`: no cipher.
const WSEC_NONE: u32 = 0;
/// `BWFM_AUTH_OPEN` - open system; the WPA2 authentication happens in the handshake, not here.
const AUTH_OPEN_SYSTEM: u32 = 0;
/// `BWFM_MFP_NONE` - no management frame protection, as `bwfm_connect` sets it.
const MFP_NONE: [u8; 4] = [0, 0, 0, 0];

// How a join ended and what it joins with are every radio's (`godspeed_wifi::station`).
pub use godspeed_wifi::station::{Outcome, Secret};
/// The SSID limit lives in `scan`; re-exported so `join::MAX_SSID` reads naturally at the request site.
pub use crate::scan::MAX_SSID;

/// `BRCMF_EVENT_MSG_LINK`: in a `LINK` event's flags, the link is up.
pub(crate) const EVENT_MSG_LINK: u16 = 0x01;


// The handshake and the keys it keeps are every radio's (`godspeed_wifi::supplicant`); this module
// supplies the Broadcom's way of sending a key frame and installing a key, and runs the join around it.
pub use godspeed_wifi::supplicant::{forget, Handshake, Keys, Step};

/// The Broadcom's `KeyPath`: a key frame goes out on the data channel (`ctrl::send_data`), a key goes in
/// through the `wsec_key` iovar (`ctrl::install_key`).
pub struct BcmPath<'x> {
    pub h: &'x dyn SdioHost,
    pub w: &'x mut Window,
    pub s: &'x mut Session,
}

impl godspeed_wifi::supplicant::KeyPath for BcmPath<'_> {
    fn send_eapol(&mut self, eth: &[u8], ctx: &ServiceContext) -> bool {
        ctrl::send_data(self.h, self.w, self.s, eth, ctx)
    }
    fn install_key(&mut self, key_idx: u32, key: &[u8; 16], peer: Option<&[u8; 6]>, ctx: &ServiceContext) -> bool {
        ctrl::install_key(self.h, self.w, self.s, key_idx, key, peer, ctx)
    }
}

/// Join `ssid`, run the handshake if there is one, install the keys, and say how it went.
pub fn join(
    h: &dyn SdioHost,
    w: &mut Window,
    s: &mut Session,
    ssid: &[u8],
    secret: Secret,
    keys: &mut Option<Keys>,
    ctx: &ServiceContext,
) -> Outcome {
    // Whatever the last association left is gone before the next one starts.
    forget(keys);
    if ssid.is_empty() || ssid.len() > MAX_SSID {
        ctx.log("wifi-driver: join refused before sending - the name length is out of range");
        return Outcome::Failed;
    }
    let open = matches!(secret, Secret::Open);

    // Our own address is the SPA of the key derivation and the source of every frame we send.
    let mut our_mac = [0u8; 6];
    match ctrl::query_iovar(h, w, s, "cur_etheraddr", &mut our_mac, ctx) {
        Some(n) if n >= 6 => {}
        _ => {
            ctx.log("wifi-driver: the firmware would not give its own address - a join cannot be signed without it");
            return Outcome::Failed;
        }
    }

    // ---- 1. The RSN element for the association request. Refusal is loud but not fatal: the firmware
    // will then compose its own, and message 2 will have to be built from whatever it sent. ----
    if !open && !ctrl::set_iovar(h, w, s, "wpaie", &eapol::RSN_IE, ctx) {
        ctx.log(
            "wifi-driver: `wpaie` refused - the firmware will compose the association request's RSN element \
             itself, and message 2 will carry ours - if the two differ the access point may refuse",
        );
    }

    // ---- 2. Security mode, in `bwfm_connect`'s order. ----
    let (wpa_auth, wsec, mode) = if open {
        (WPA_AUTH_DISABLED, WSEC_NONE, "wpa_auth disabled (open network)")
    } else {
        (WPA2_AUTH_PSK, WSEC_AES, "wpa_auth WPA2-PSK")
    };
    if !ctrl::set_cmd_int(h, w, s, CMD_SET_WPA_AUTH, wpa_auth, mode, ctx)
        || !ctrl::set_cmd_int(h, w, s, CMD_SET_WSEC, wsec, if open { "wsec none" } else { "wsec AES" }, ctx)
        || !ctrl::set_cmd_int(h, w, s, CMD_SET_AUTH, AUTH_OPEN_SYSTEM, "auth open-system", ctx)
    {
        return Outcome::Failed;
    }
    if !ctrl::set_iovar(h, w, s, "mfp", &MFP_NONE, ctx) {
        ctx.log("wifi-driver: `mfp` refused - continuing; management frame protection stays at the firmware's default");
    }

    // ---- 3. The join itself: the 36-byte SSID structure. ----
    let mut ssid_le = [0u8; 4 + MAX_SSID];
    ssid_le[0..4].copy_from_slice(&(ssid.len() as u32).to_le_bytes());
    ssid_le[4..4 + ssid.len()].copy_from_slice(ssid);
    if !ctrl::set_cmd(h, w, s, CMD_SET_SSID, &ssid_le, "join by SSID", ctx) {
        return Outcome::Failed;
    }

    // ---- 4. Wait on the firmware's word, and then run the handshake with the access point. ----
    // The poll bound is a bound, not a duration: each empty poll sleeps a millisecond, so this is on the
    // order of ten seconds, inside the shell's thirty. An access point retries message 1 a few times over
    // several seconds before it deauthenticates, and this must outlast that to report it.
    const JOIN_EMPTY_POLLS: u32 = 10_000;
    let mut frame = [0u8; ctrl::FRAME];
    let mut link_up = false;
    let mut empty = 0u32;
    let mut events = 0u32;
    let mut eapol_frames = 0u32;
    let mut other_traffic = 0u32;

    // The handshake's state: the AP's nonce, ours, the derived keys, and how many times we have answered
    // message 1 - which is what tells a repeated message 1 apart from a first one.
    let mut hs = match secret {
        Secret::Pmk(p) => Some(Handshake::new(*p, our_mac)),
        Secret::Open => None,
    };

    while empty < JOIN_EMPTY_POLLS {
        let f = match ctrl::read_frame(h, w, &mut frame, ctx) {
            Some(f) => f,
            None => {
                empty += 1;
                ctx.sleep_ms(1);
                continue;
            }
        };
        s.note_frame(ctx, &f, &frame, false);
        // Every frame inside the frame: a superframe's sub-frames are handled one by one (docs/wifi.md 38 -
        // the association events arrive glommed, and the handshake's messages may too).
        let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
        let nsubs = ctrl::subframes(&f, &frame, s.glom_descriptor(), &mut subs, ctx);
        for sub in subs.iter().take(nsubs) {
            let channel = sub.chanflag & CHANNEL_MASK;
            let body = &frame[sub.off..sub.off + sub.len];

            if channel == CHANNEL_DATA {
                let eth = match scan::ethernet_at(body) {
                    Some(eth) => eth,
                    None => {
                        ctx.log_fmt(format_args!(
                            "wifi-driver:   a data frame of {} bytes cannot hold its BDC header - skipped",
                            body.len()
                        ));
                        continue;
                    }
                };
                let eth_frame = &body[eth..];
                if eth_frame.len() < ev::ETHHDR {
                    other_traffic += 1;
                    continue;
                }
                let ethertype = u16::from_be_bytes([eth_frame[ev::ETHERTYPE], eth_frame[ev::ETHERTYPE + 1]]);
                if ethertype != eapol::ETHERTYPE_EAPOL {
                    other_traffic += 1;
                    continue;
                }
                eapol_frames += 1;
                let hs = match hs.as_mut() {
                    Some(hs) => hs,
                    None => {
                        ctx.log("wifi-driver:   a handshake frame on an OPEN join - ignored; the network is not what the scan said it was");
                        continue;
                    }
                };
                match hs.on_key_frame(&mut BcmPath { h, w: &mut *w, s: &mut *s }, eth_frame, ctx) {
                    Step::Continue => continue,
                    Step::Joined(k) => {
                        ctrl::report_power_mode(h, w, s, ctx);
                        *keys = Some(k);
                        return Outcome::Joined;
                    }
                    Step::PassphraseRefused => return Outcome::PassphraseRefused,
                    Step::Failed => return Outcome::Failed,
                }
            }
            if channel != CHANNEL_EVENT {
                continue;
            }

            events += 1;
            let e = match scan::parse_event(body, events, ctx) {
                Some(e) => e,
                None => continue,
            };
            ctx.log_fmt(format_args!(
                "wifi-driver:   join event {} ({}), status {}, reason {}, flags {:#06x}",
                e.event_type, code::name(e.event_type), e.status, e.reason, e.flags
            ));
            match e.event_type {
                code::SET_SSID if e.status == status::NO_NETWORKS => return Outcome::NotFound,
                code::SET_SSID if e.status != status::SUCCESS => return Outcome::Failed,
                code::LINK if e.flags & EVENT_MSG_LINK != 0 => {
                    link_up = true;
                    if open {
                        // No keys, no handshake: on an open network the link coming up is the whole join.
                        ctx.log("wifi-driver: JOINED - the link is up on an open network");
                        ctrl::report_power_mode(h, w, s, ctx);
                        return Outcome::Joined;
                    }
                    ctx.log("wifi-driver:   ASSOCIATED - the link is up at the 802.11 layer; the handshake is now the access point's move");
                }
                code::DEAUTH_IND | code::DISASSOC_IND => {
                    let msg2_sent = hs.as_ref().map_or(0, |h| h.msg2_sent);
                    return if msg2_sent > 0 {
                        // We answered and were dropped anyway: the access point could not verify our MIC.
                        ctx.log_fmt(format_args!(
                            "wifi-driver: deauthenticated (reason {}) after {} answer(s) to message 1 - our key is not its key: INCORRECT PASSPHRASE",
                            e.reason, msg2_sent
                        ));
                        Outcome::PassphraseRefused
                    } else if eapol_frames > 0 {
                        ctx.log("wifi-driver: the access point ended the association before message 2 could go out");
                        Outcome::Failed
                    } else {
                        ctx.log("wifi-driver: the access point ended the association before any handshake frame arrived");
                        Outcome::Failed
                    };
                }
                _ => {}
            }
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the join produced no decision across {} empty polls ({} event(s), {} handshake frame(s), \
         {} other data frame(s); link up: {}, message 2 sent {} time(s))",
        JOIN_EMPTY_POLLS, events, eapol_frames, other_traffic, link_up, hs.as_ref().map_or(0, |h| h.msg2_sent)
    ));
    Outcome::Timeout
}
