// SPDX-License-Identifier: GPL-2.0-only
//! Joining a network: WPA2-PSK, with the firmware doing the handshake.
//!
//! This tree has no cryptography, so the host cannot derive the pairwise master key from a passphrase.
//! The firmware can, and the driver that uses exactly that path from a host with no crypto is cyw43-driver
//! (the Pico W, same firmware family), which is the reference of record for this one step; Linux's
//! brcmfmac hands the firmware a key wpa_supplicant already derived and is quoted for everything else.
//! The two agree on every command number.
//!
//! ## The sequence, quoted
//!
//! From cyw43-driver's `cyw43_ll_wifi_join`:
//!
//! ```c
//! cyw43_set_ioctl_u32(self, WLC_SET_WPA_AUTH, wpa_auth, WWD_STA_INTERFACE);
//! cyw43_set_ioctl_u32(self, WLC_SET_WSEC, auth_type & 0xff, WWD_STA_INTERFACE);
//! cyw43_write_iovar_u32_u32(self, "bsscfg:sup_wpa", 0, auth_type == 0 ? 0 : 1, WWD_STA_INTERFACE);
//! cyw43_put_le16(buf, key_len);
//! cyw43_put_le16(buf + 2, 1);
//! memcpy(buf + 4, key, key_len);
//! cyw43_do_ioctl(self, SDPCM_SET, WLC_SET_WSEC_PMK, 4 + CYW43_WPA_MAX_PASSWORD_LEN, buf, ...);
//! cyw43_do_ioctl(self, SDPCM_SET, WLC_SET_SSID, 36, self->last_ssid_joined, ...);
//! ```
//!
//! with `WLC_SET_INFRA (20)`, `WLC_SET_AUTH (22)`, `WLC_SET_SSID (26)`, `WLC_SET_WSEC (134)`,
//! `WLC_SET_WPA_AUTH (165)`, `WLC_SET_WSEC_PMK (268)`, `CYW43_WPA_MAX_PASSWORD_LEN 64`,
//! `CYW43_WPA2_AUTH_PSK (0x0080)`. brcmfmac's `fwil.h` gives the same 20/22/26/134/268, `AES_ENABLED 0x0004`
//! and `WPA2_AUTH_PSK 0x0080`, `brcmf_set_auth_type` sets `auth` to 0 for open system, and its
//! `brcmf_wsec_pmk_le` is `{ __le16 key_len; __le16 flags; u8 key[]; }` with `BRCMF_WSEC_PASSPHRASE BIT(0)`
//! - the flag cyw43 writes as `1`. The SSID goes as `brcmf_ssid_le { __le32 SSID_len; u8 SSID[32]; }`, 36
//! bytes, which is cyw43's `36` too.
//!
//! ## What "joined" means, and how a wrong passphrase presents
//!
//! The firmware reports in events. `LINK` (16) with `flags & 0x01` (`BRCMF_EVENT_MSG_LINK`) is the link
//! up; `PSK_SUP` (46) with `status 6` (`BRCMF_E_STATUS_FWSUP_COMPLETED`) is the handshake done. Both are
//! required before this reports success. A wrong passphrase does not produce a "wrong passphrase" event:
//! the 4-way handshake fails to complete, which the firmware reports as `PSK_SUP` with `status 7`
//! (`FWSUP_TIMEOUT`) or reasons 15-17 (`BRCMF_E_REASON_FWSUP_WPA_PSK_TMO`, `BRCMF_E_REASON_FWSUP_WPA_PSK_M1_TMO`,
//! `BRCMF_E_REASON_FWSUP_WPA_PSK_M3_TMO`), or the access point sends a
//! `DEAUTH_IND`. Those are what `utilities/56_wifi.md` §5's "refused the passphrase" is built on. A network
//! that is not there answers the `SET_SSID` event with `NO_NETWORKS` (3).
//!
//! ## The passphrase
//!
//! It arrives in the request, is copied into one stack buffer sized for the command, sent, and the buffer
//! is zeroed before this returns. It is never logged and never kept: the firmware holds the derived key.

use godspeed_sdk::ServiceContext;

use crate::backplane::Window;
use crate::ctrl::{self, Session};
use crate::host::Host;
use crate::scan::{self, code, status, CHANNEL_DATA, CHANNEL_EVENT, CHANNEL_MASK};

/// `WLC_SET_AUTH` / `BRCMF_C_SET_AUTH`.
const CMD_SET_AUTH: u32 = 22;
/// `WLC_SET_SSID` / `BRCMF_C_SET_SSID`.
const CMD_SET_SSID: u32 = 26;
/// `WLC_SET_WSEC` / `BRCMF_C_SET_WSEC`.
const CMD_SET_WSEC: u32 = 134;
/// `WLC_SET_WPA_AUTH`.
const CMD_SET_WPA_AUTH: u32 = 165;
/// `WLC_SET_WSEC_PMK` / `BRCMF_C_SET_WSEC_PMK`.
const CMD_SET_WSEC_PMK: u32 = 268;

/// `WPA2_AUTH_PSK`.
const WPA2_AUTH_PSK: u32 = 0x0080;
/// `AES_ENABLED`.
const AES_ENABLED: u32 = 0x0004;
/// `brcmf_set_auth_type`, open system: `val = 0`.
const AUTH_OPEN_SYSTEM: u32 = 0;
/// `BRCMF_WSEC_PASSPHRASE` - `BIT(0)`; cyw43 writes it as `1`.
const WSEC_PASSPHRASE: u16 = 1;
/// `CYW43_WPA_MAX_PASSWORD_LEN`.
pub const MAX_PASSPHRASE: usize = 64;
/// The SSID limit lives in `scan`; re-exported so `join::MAX_SSID` reads naturally at the request site.
pub use crate::scan::MAX_SSID;

/// `BRCMF_EVENT_MSG_LINK`: in a `LINK` event's flags, the link is up.
const EVENT_MSG_LINK: u16 = 0x01;
/// `BRCMF_E_STATUS_FWSUP_COMPLETED`, in a `PSK_SUP` event.
const FWSUP_COMPLETED: u32 = 6;
/// `BRCMF_E_STATUS_FWSUP_TIMEOUT`.
const FWSUP_TIMEOUT: u32 = 7;
/// `BRCMF_E_REASON_FWSUP_WPA_PSK_TMO` through `..._M3_TMO`: the handshake did not complete.
const FWSUP_REASON_PSK_TMO_FIRST: u32 = 15;
const FWSUP_REASON_PSK_TMO_LAST: u32 = 17;

/// How a join ended. Mirrors `scan::reply`'s connect statuses one for one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Joined,
    NotFound,
    PassphraseRefused,
    Failed,
    Timeout,
}

/// Join `ssid` with `passphrase`, and wait for the firmware to say how it went.
pub fn join(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    ssid: &[u8],
    passphrase: &[u8],
    ctx: &ServiceContext,
) -> Outcome {
    if ssid.is_empty() || ssid.len() > MAX_SSID || passphrase.len() > MAX_PASSPHRASE {
        ctx.log("wifi-driver: join refused before sending - the name or passphrase length is out of range");
        return Outcome::Failed;
    }

    // ---- 1. Security mode, in the reference's order. ----
    if !ctrl::set_cmd_int(h, w, s, CMD_SET_WPA_AUTH, WPA2_AUTH_PSK, "wpa_auth WPA2-PSK", ctx)
        || !ctrl::set_cmd_int(h, w, s, CMD_SET_AUTH, AUTH_OPEN_SYSTEM, "auth open-system", ctx)
        || !ctrl::set_cmd_int(h, w, s, CMD_SET_WSEC, AES_ENABLED, "wsec AES", ctx)
        || !ctrl::set_iovar(h, w, s, "sup_wpa", &1u32.to_le_bytes(), ctx)
    {
        return Outcome::Failed;
    }

    // ---- 2. The passphrase, to the firmware's supplicant. One buffer, zeroed on the way out. ----
    let mut pmk = [0u8; 4 + MAX_PASSPHRASE];
    pmk[0..2].copy_from_slice(&(passphrase.len() as u16).to_le_bytes());
    pmk[2..4].copy_from_slice(&WSEC_PASSPHRASE.to_le_bytes());
    pmk[4..4 + passphrase.len()].copy_from_slice(passphrase);
    let sent = ctrl::set_cmd(h, w, s, CMD_SET_WSEC_PMK, &pmk, "passphrase to the supplicant", ctx);
    for b in pmk.iter_mut() {
        *b = 0;
    }
    if !sent {
        return Outcome::Failed;
    }

    // ---- 3. The join itself: the 36-byte SSID structure. ----
    let mut ssid_le = [0u8; 4 + MAX_SSID];
    ssid_le[0..4].copy_from_slice(&(ssid.len() as u32).to_le_bytes());
    ssid_le[4..4 + ssid.len()].copy_from_slice(ssid);
    if !ctrl::set_cmd(h, w, s, CMD_SET_SSID, &ssid_le, "join by SSID", ctx) {
        return Outcome::Failed;
    }

    // ---- 4. Wait on the firmware's word. ----
    // Two facts make a join: the link up, and the handshake completed. Either failure event ends the
    // wait early. The poll bound underneath is exactly that - a bound - and the log says which ended it.
    const JOIN_EMPTY_POLLS: u32 = 3000;
    let mut frame = [0u8; ctrl::FRAME];
    let mut link_up = false;
    let mut handshake_done = false;
    let mut empty = 0u32;
    let mut events = 0u32;
    while empty < JOIN_EMPTY_POLLS {
        let f = match ctrl::read_frame(h, w, &mut frame, ctx) {
            Some(f) => f,
            None => {
                empty += 1;
                ctx.sleep_ms(1);
                continue;
            }
        };
        let channel = f.chanflag & CHANNEL_MASK;
        if channel != CHANNEL_EVENT && channel != CHANNEL_DATA {
            continue;
        }
        events += 1;
        let p = f.off;
        let e = match scan::parse_event(&frame[p..p + f.len], events, ctx) {
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
            code::LINK if e.flags & EVENT_MSG_LINK != 0 => link_up = true,
            code::PSK_SUP if e.status == FWSUP_COMPLETED => handshake_done = true,
            code::PSK_SUP
                if e.status == FWSUP_TIMEOUT
                    || (FWSUP_REASON_PSK_TMO_FIRST..=FWSUP_REASON_PSK_TMO_LAST).contains(&e.reason) =>
            {
                return Outcome::PassphraseRefused
            }
            code::DEAUTH_IND | code::DISASSOC_IND => return Outcome::PassphraseRefused,
            _ => {}
        }
        if link_up && handshake_done {
            ctx.log("wifi-driver: JOINED - the link is up and the firmware's supplicant completed the handshake");
            return Outcome::Joined;
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: the join produced no decision across {} empty polls ({} event(s) seen; link up: {}, \
         handshake done: {})",
        JOIN_EMPTY_POLLS, events, link_up, handshake_done
    ));
    Outcome::Timeout
}
