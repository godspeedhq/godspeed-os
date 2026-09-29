// SPDX-License-Identifier: GPL-2.0-only
//! Joining a network: association by the firmware, the WPA2 handshake by the host.
//!
//! This firmware has no supplicant. `sup_wpa` - the switch that would hand it the 4-way handshake - is
//! refused `BCME_UNSUPPORTED` in every form, including the GET Linux's own feature detection rests on
//! (`docs/wifi.md` §37). So the model here is OpenBSD's, which sets `sup_wpa 0` on purpose and does the
//! handshake in net80211: the firmware authenticates and associates, the access point's EAPOL-Key frames
//! arrive on the DATA channel, the host derives the keys and installs them with the `wsec_key` iovar.
//!
//! **This slice does the first half and OBSERVES the second.** It associates, and when the access point
//! sends message 1 of the handshake it reads and reports the frame rather than pretending to answer it.
//! The access point will give up and deauthenticate, and that is reported for what it is - not as a refused
//! passphrase, which it is not. Deriving keys and answering is the next slice; the crypto is not here yet.
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
//! bytes, the form cyw43-driver sends, which lets the firmware choose the access point; `bwfm_join_params`
//! adds a BSSID and is what the numbered picker will use to choose one itself.
//!
//! ## Why `wpaie` is set at all
//!
//! The station's RSN element goes in its association request, and the access point requires message 2 of
//! the handshake to carry the SAME bytes - a mismatch is a deauthentication. Setting the element ourselves
//! makes those bytes known (`eapol::RSN_IE`) instead of whatever the firmware would compose from `wpa_auth`
//! and `wsec`. Linux does not set it because wpa_supplicant reads the element back out of the association
//! request; this driver has no such path, so it follows OpenBSD.
//!
//! ## What the events mean
//!
//! `SET_SSID` with status `NO_NETWORKS` (3): nothing of that name answered. `LINK` (16) with `flags & 0x01`
//! (`BRCMF_EVENT_MSG_LINK`): associated - the link is up at the 802.11 layer, and the handshake is now the
//! access point's move. `DEAUTH_IND` (6) / `DISASSOC_IND` (12): the access point ended it. `PSK_SUP` (46)
//! is the firmware supplicant's report and cannot occur on this firmware; it is not waited on.
//!
//! ## The passphrase
//!
//! It never reaches this module. The serve loop turns it into the pairwise master key the moment it arrives
//! (`crypto::psk`, IEEE 802.11 §12.7.1.2) and keeps only the key, in the one credential slot the driver
//! holds (`utilities/56_wifi.md` §6). `Secret::Pmk` is that key; the handshake that will use it is the next
//! slice, and until then a WPA2 join associates and reports `HandshakeUnimplemented` rather than pretending.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl::{self, Session};
use crate::eapol;
use crate::host::Host;
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

/// `CYW43_WPA_MAX_PASSWORD_LEN` - the longest passphrase the request carries.
pub const MAX_PASSPHRASE: usize = 64;
/// The SSID limit lives in `scan`; re-exported so `join::MAX_SSID` reads naturally at the request site.
pub use crate::scan::MAX_SSID;

/// `BRCMF_EVENT_MSG_LINK`: in a `LINK` event's flags, the link is up.
const EVENT_MSG_LINK: u16 = 0x01;

/// How a join ended. Mirrors `scan::reply`'s connect statuses one for one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Joined,
    NotFound,
    PassphraseRefused,
    Failed,
    Timeout,
    /// Associated, and the access point began the handshake this driver cannot yet answer.
    HandshakeUnimplemented,
}

/// What the station joins WITH. The passphrase never reaches this module: it is turned into the pairwise
/// master key the moment it arrives (`crypto::psk`) and only the key is kept.
#[derive(Clone, Copy)]
pub enum Secret<'a> {
    /// No key at all - `bwfm_connect`'s final `else`: `wpa_auth` DISABLED, `wsec` NONE, no RSN element, and
    /// no handshake to wait for. The link coming up IS the join.
    Open,
    /// WPA2-PSK with this pairwise master key. Held for the handshake, which is not built yet; until it is,
    /// the join associates and reports `HandshakeUnimplemented`.
    Pmk(&'a [u8; crate::crypto::PMK_LEN]),
}

/// Join `ssid`, and wait for the firmware - and then the access point - to say how it went.
pub fn join(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    ssid: &[u8],
    secret: Secret,
    ctx: &ServiceContext,
) -> Outcome {
    if ssid.is_empty() || ssid.len() > MAX_SSID {
        ctx.log("wifi-driver: join refused before sending - the name length is out of range");
        return Outcome::Failed;
    }
    let open = matches!(secret, Secret::Open);

    // ---- 1. The RSN element for the association request. Refusal is loud but not fatal: the firmware
    // will then compose its own, and message 2 will have to be built from whatever it sent. ----
    if !open && !ctrl::set_iovar(h, w, s, "wpaie", &eapol::RSN_IE, ctx) {
        ctx.log(
            "wifi-driver: `wpaie` refused - the firmware will compose the association request's RSN element \
             itself, and the handshake's message 2 cannot yet know what it sent",
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

    // ---- 4. Wait on the firmware's word, and then the access point's. ----
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
        // the association events arrive glommed, and the handshake's third message may too).
        let mut subs = [ctrl::Sub::default(); ctrl::MAX_SUBS];
        let nsubs = ctrl::subframes(&f, &frame, s.glom_descriptor(), &mut subs, ctx);
        for sub in subs.iter().take(nsubs) {
        let channel = sub.chanflag & CHANNEL_MASK;
        let body = &frame[sub.off..sub.off + sub.len];

        if channel == CHANNEL_DATA {
            // TRAFFIC. Before any key is installed the only frames that can matter are the handshake's.
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
            if ethertype == eapol::ETHERTYPE_EAPOL {
                eapol_frames += 1;
                let _ = eapol::describe(eth_frame, ctx);
                // Not answered. Said once, at the first one, so the log explains the deauthentication that
                // follows rather than leaving it to look like the access point's fault.
                if eapol_frames == 1 {
                    ctx.log(
                        "wifi-driver:   the access point has begun the WPA2 handshake. This driver cannot yet \
                         answer it (docs/wifi.md 37) - the access point will retry, then deauthenticate",
                    );
                }
            } else {
                other_traffic += 1;
            }
            continue;
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
                    return Outcome::Joined;
                }
                ctx.log("wifi-driver:   ASSOCIATED - the link is up at the 802.11 layer; the handshake is now the access point's move");
            }
            code::DEAUTH_IND | code::DISASSOC_IND => {
                return if eapol_frames > 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: the access point ended the association after {} unanswered handshake \
                         frame(s) - expected until the host supplicant exists",
                        eapol_frames
                    ));
                    Outcome::HandshakeUnimplemented
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
         {} other data frame(s); link up: {})",
        JOIN_EMPTY_POLLS, events, eapol_frames, other_traffic, link_up
    ));
    if link_up && eapol_frames > 0 {
        Outcome::HandshakeUnimplemented
    } else {
        Outcome::Timeout
    }
}
