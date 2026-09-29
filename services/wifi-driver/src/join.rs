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
//! group key (its key id, `PRIMARY_KEY`) are installed (`bwfm_set_key_cb`).
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
use crate::eapol::{self, info};
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
    /// Kept for the reply table; no path produces it now that the handshake is answered.
    HandshakeUnimplemented,
}

/// What the station joins WITH. The passphrase never reaches this module: it is turned into the pairwise
/// master key the moment it arrives (`crypto::psk`) and only the key is kept.
#[derive(Clone, Copy)]
pub enum Secret<'a> {
    /// No key at all - `bwfm_connect`'s final `else`: `wpa_auth` DISABLED, `wsec` NONE, no RSN element, and
    /// no handshake to wait for. The link coming up IS the join.
    Open,
    /// WPA2-PSK with this pairwise master key.
    Pmk(&'a [u8; crate::crypto::PMK_LEN]),
}

/// The station's nonce for one handshake. The hardware RNG where the kernel exposes one (`hw_random`);
/// where it does not - the aarch64 kernel's is a stub today - the cycle counter, the access point's own
/// nonce and our address hashed together, and the log SAYS SO, because a nonce from a counter is a real
/// weakening that must not pass unremarked.
fn snonce(ctx: &ServiceContext, anonce: &[u8; 32], mac: &[u8; 6]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let mut from_hw = true;
    for i in 0..8 {
        match ctx.hw_random() {
            Some(r) => out[4 * i..4 * i + 4].copy_from_slice(&r.to_le_bytes()),
            None => {
                from_hw = false;
                break;
            }
        }
    }
    if from_hw {
        return out;
    }
    ctx.log("wifi-driver: NO HARDWARE RNG on this board - the SNonce is hashed from the cycle counter, the AP's nonce and our address (weaker than the standard intends; recorded, not hidden)");
    let mut seed = [0u8; 8 + 8 + 32 + 6 + 1];
    seed[0..8].copy_from_slice(&ctx.read_tsc().to_le_bytes());
    seed[8..16].copy_from_slice(&ctx.epoch_secs_monotonic().to_le_bytes());
    seed[16..48].copy_from_slice(anonce);
    seed[48..54].copy_from_slice(mac);
    for half in 0..2u8 {
        seed[54] = half;
        let mut s = crate::crypto::Sha1::new();
        s.update(&seed);
        let d = s.finish();
        let take = if half == 0 { 20 } else { 12 };
        out[half as usize * 20..half as usize * 20 + take].copy_from_slice(&d[..take]);
    }
    out
}

/// Join `ssid`, run the handshake if there is one, install the keys, and say how it went.
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
    let mut anonce = [0u8; 32];
    let mut have_anonce = false;
    let mut our_nonce = [0u8; 32];
    let mut ptk: Option<eapol::Ptk> = None;
    let mut msg2_sent = 0u32;
    let mut tx = [0u8; ev::ETHHDR + 99 + 64];

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
                let key = match eapol::describe(eth_frame, ctx) {
                    Some(k) => k,
                    None => continue,
                };
                let pmk = match secret {
                    Secret::Pmk(p) => p,
                    Secret::Open => {
                        ctx.log("wifi-driver:   a handshake frame on an OPEN join - ignored; the network is not what the scan said it was");
                        continue;
                    }
                };
                let is_pairwise = key.info & info::PAIRWISE != 0;
                let is_ack = key.info & info::KEYACK != 0;
                let is_mic = key.info & info::KEYMIC != 0;

                // ---- MESSAGE 1: pairwise, ack, no MIC. ----
                if is_pairwise && is_ack && !is_mic {
                    if msg2_sent >= 2 && have_anonce && key.nonce == anonce {
                        // The access point did not accept two answers and is asking a third time. The one cause
                        // is a MIC it could not verify: our PMK is not its PMK.
                        ctx.log("wifi-driver: the access point repeated message 1 after two answers - our key is not its key: INCORRECT PASSPHRASE");
                        return Outcome::PassphraseRefused;
                    }
                    anonce = key.nonce;
                    have_anonce = true;
                    if msg2_sent == 0 {
                        our_nonce = snonce(ctx, &anonce, &our_mac);
                    }
                    let derived = eapol::derive_ptk(pmk, &key.from, &our_mac, &anonce, &our_nonce);
                    let n = eapol::build_key_frame(
                        &mut tx, &key.from, &our_mac,
                        info::PAIRWISE | info::KEYMIC,
                        key.replay, &our_nonce, &eapol::RSN_IE, Some(&derived.kck),
                    );
                    ptk = Some(derived);
                    if n == 0 || !ctrl::send_data(h, w, s, &tx[..n], ctx) {
                        ctx.log("wifi-driver: message 2 of the handshake could not be sent - not joined");
                        return Outcome::Failed;
                    }
                    msg2_sent += 1;
                    ctx.log_fmt(format_args!(
                        "wifi-driver:   message 2 of 4 sent ({} bytes, replay {}) - our nonce and the RSN element, signed",
                        n, key.replay
                    ));
                    continue;
                }

                // ---- MESSAGE 3: pairwise, ack, MIC, install; the key data encrypted. ----
                if is_pairwise && is_ack && is_mic {
                    let p = match ptk.as_ref() {
                        Some(p) => p,
                        None => {
                            ctx.log("wifi-driver:   message 3 before any message 1 - ignored");
                            continue;
                        }
                    };
                    if !have_anonce || key.nonce != anonce {
                        ctx.log("wifi-driver:   message 3's ANonce does not match message 1's - ignored (`ieee80211_recv_4way_msg3`)");
                        continue;
                    }
                    let eapol_body = &eth_frame[ev::ETHHDR..];
                    if !eapol::check_mic(eapol_body, &p.kck) {
                        ctx.log("wifi-driver: message 3's MIC does not verify under our KCK - the keys disagree; not joined");
                        return Outcome::PassphraseRefused;
                    }
                    if key.info & info::ENCRYPTED == 0 {
                        ctx.log("wifi-driver: message 3's key data is not encrypted - refused (a group key in the clear is not one this driver installs)");
                        return Outcome::Failed;
                    }
                    let wrapped = &eth_frame[key.key_data_at..key.key_data_at + key.key_data_len];
                    let mut key_data = [0u8; 512];
                    if wrapped.len() < 24 || wrapped.len() > key_data.len() + 8
                        || !crate::crypto::aes_key_unwrap(&p.kek, wrapped, &mut key_data)
                    {
                        ctx.log_fmt(format_args!(
                            "wifi-driver: message 3's {} bytes of key data did not unwrap under our KEK - not joined",
                            wrapped.len()
                        ));
                        return Outcome::Failed;
                    }
                    let plain = &key_data[..wrapped.len() - 8];
                    let (kid, gtk_tx, gtk) = match eapol::find_gtk(plain) {
                        Some(g) => g,
                        None => {
                            ctx.log("wifi-driver: message 3 carried no group key - not joined");
                            return Outcome::Failed;
                        }
                    };
                    if gtk.len() != 16 {
                        ctx.log_fmt(format_args!("wifi-driver: the group key is {} bytes, not the 16 of CCMP - not joined", gtk.len()));
                        return Outcome::Failed;
                    }
                    let mut gtk16 = [0u8; 16];
                    gtk16.copy_from_slice(gtk);
                    // Message 4: pairwise, MIC, secure; empty; the AP's replay counter.
                    let n = eapol::build_key_frame(
                        &mut tx, &key.from, &our_mac,
                        info::PAIRWISE | info::KEYMIC | info::SECURE,
                        key.replay, &[0u8; 32], &[], Some(&p.kck),
                    );
                    if n == 0 || !ctrl::send_data(h, w, s, &tx[..n], ctx) {
                        ctx.log("wifi-driver: message 4 of the handshake could not be sent - not joined");
                        return Outcome::Failed;
                    }
                    ctx.log_fmt(format_args!(
                        "wifi-driver:   message 3 verified (MIC, ANonce, {} bytes of key data unwrapped); message 4 sent",
                        plain.len()
                    ));
                    // Install: the pairwise key at index 0 for the access point, then the group key.
                    let tk = p.tk;
                    if !ctrl::install_key(h, w, s, 0, &tk, Some(&key.from), ctx) {
                        ctx.log("wifi-driver: the firmware refused the pairwise key - not joined");
                        return Outcome::Failed;
                    }
                    if !ctrl::install_key(h, w, s, kid as u32, &gtk16, None, ctx) {
                        ctx.log("wifi-driver: the firmware refused the group key - not joined");
                        return Outcome::Failed;
                    }
                    ctx.log_fmt(format_args!(
                        "wifi-driver: JOINED - handshake complete, pairwise key installed, group key {} installed{}",
                        kid,
                        if gtk_tx { " (tx)" } else { "" }
                    ));
                    return Outcome::Joined;
                }
                ctx.log_fmt(format_args!(
                    "wifi-driver:   an EAPOL-Key frame this handshake does not expect (info {:#06x}) - ignored",
                    key.info
                ));
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
        JOIN_EMPTY_POLLS, events, eapol_frames, other_traffic, link_up, msg2_sent
    ));
    Outcome::Timeout
}
