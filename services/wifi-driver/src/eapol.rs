// SPDX-License-Identifier: GPL-2.0-only
//! EAPOL-Key frames: the four messages of the WPA2 handshake, as they arrive on the data channel.
//!
//! This firmware has no supplicant (`docs/wifi.md` §37), so the handshake is the host's - as it is for
//! OpenBSD, whose `bwfm_rx` hands every frame with ethertype `ETHERTYPE_EAPOL` to
//! `ieee80211_eapol_key_input`. This module is the first step of that: recognising the frame and reading
//! its header. Deriving keys and answering come after, and are not pretended here.
//!
//! ## The frame, quoted
//!
//! `sys/net80211/ieee80211.h`:
//!
//! ```c
//! struct ieee80211_eapol_key {
//!     u_int8_t  version;        /* EAPOL_VERSION 1 */
//!     u_int8_t  type;           /* EAPOL_KEY 3 */
//!     u_int8_t  len[2];
//!     u_int8_t  desc;           /* EAPOL_KEY_DESC_IEEE80211 2, EAPOL_KEY_DESC_WPA 254 */
//!     u_int8_t  info[2];
//!     u_int8_t  keylen[2];
//!     u_int8_t  replaycnt[8];
//!     u_int8_t  nonce[32];
//!     u_int8_t  iv[16];
//!     u_int8_t  rsc[8];
//!     u_int8_t  reserved[8];
//!     u_int8_t  mic[16];
//!     u_int8_t  paylen[2];
//! } __packed;
//! ```
//!
//! Every multi-byte field is big-endian - this is an 802.1X frame, not a firmware structure. The header is
//! 99 bytes and the key data (`paylen` bytes of it) follows.

use godspeed_sdk::ServiceContext;

use crate::scan::ev;

/// `ETHERTYPE_EAPOL`.
pub const ETHERTYPE_EAPOL: u16 = 0x888E;

/// Field offsets within the EAPOL-Key header, from the struct above.
mod at {
    pub const PKT_VERSION: usize = 0;
    pub const PKT_TYPE: usize = 1;
    pub const PKT_LEN: usize = 2;
    pub const KEY_DESC: usize = 4;
    pub const KEY_INFO: usize = 5;
    pub const KEY_LEN: usize = 7;
    pub const KEY_REPLAY: usize = 9;
    pub const KEY_NONCE: usize = 17;
    pub const KEY_MIC: usize = 81;
    pub const KEY_PAYLEN: usize = 97;
    /// `sizeof(struct ieee80211_eapol_key)`.
    pub const KEY_HEADER: usize = 99;
}

/// `EAPOL_KEY` - the packet type that carries a key descriptor.
const TYPE_KEY: u8 = 3;
/// `EAPOL_KEY_DESC_IEEE80211` - the RSN (WPA2) descriptor.
const DESC_RSN: u8 = 2;
/// `EAPOL_KEY_DESC_WPA` - the pre-standard WPA descriptor.
const DESC_WPA: u8 = 254;

/// Bits of the key-information field, quoted from the same header.
pub mod info {
    pub const VERSION_MASK: u16 = 0x7;
    pub const PAIRWISE: u16 = 1 << 3;
    pub const INSTALL: u16 = 1 << 6;
    pub const KEYACK: u16 = 1 << 7;
    pub const KEYMIC: u16 = 1 << 8;
    pub const SECURE: u16 = 1 << 9;
    pub const ERROR: u16 = 1 << 10;
    pub const REQUEST: u16 = 1 << 11;
    pub const ENCRYPTED: u16 = 1 << 12;
}

/// The RSN element this station puts in its association request, and must repeat in message 2.
///
/// `bwfm_connect` builds it with `ieee80211_add_rsn` and hands it to the firmware as the `wpaie` iovar
/// ("tell firmware to add WPA/RSN IE to (re)assoc request"). Setting it ourselves is what makes the bytes
/// in message 2 KNOWN rather than whatever the firmware would have composed: the access point compares the
/// two and deauthenticates on a mismatch. IEEE 802.11 RSNE, one pairwise cipher and one AKM:
///
/// | bytes | field |
/// |---|---|
/// | `30 14` | element id 48, length 20 |
/// | `01 00` | version 1 |
/// | `00 0f ac 04` | group cipher CCMP-128 |
/// | `01 00` `00 0f ac 04` | one pairwise cipher: CCMP-128 |
/// | `01 00` `00 0f ac 02` | one AKM: PSK |
/// | `00 00` | RSN capabilities: none |
pub const RSN_IE: [u8; 22] = [
    0x30, 0x14, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00, 0x00, 0x0f, 0xac, 0x04, 0x01, 0x00,
    0x00, 0x0f, 0xac, 0x02, 0x00, 0x00,
];

/// What one EAPOL-Key header said.
pub struct Key {
    pub desc: u8,
    pub info: u16,
    pub key_len: u16,
    pub replay: u64,
    pub pay_len: u16,
}

impl Key {
    /// Which of the four messages this is, by the bits that identify it in a WPA2-PSK handshake:
    /// message 1 has ACK and no MIC; message 3 has ACK, MIC, INSTALL and ENCRYPTED. Messages 2 and 4 are
    /// the station's own and never arrive.
    pub fn which(&self) -> &'static str {
        let ack = self.info & info::KEYACK != 0;
        let mic = self.info & info::KEYMIC != 0;
        let pairwise = self.info & info::PAIRWISE != 0;
        match (pairwise, ack, mic) {
            (true, true, false) => "message 1 of 4 (ANonce)",
            (true, true, true) => "message 3 of 4 (GTK, install)",
            (false, true, true) => "a group key handshake message 1",
            _ => "not one of the handshake's shapes this driver names",
        }
    }
}

/// Read an EAPOL-Key header out of an ethernet frame whose ethertype is `ETHERTYPE`. Logs what it read.
///
/// `frame` starts at the ethernet header. Returns `None`, with the reason logged, for anything that is not
/// a key descriptor this driver knows - which is reported, never silently skipped.
pub fn describe(frame: &[u8], ctx: &ServiceContext) -> Option<Key> {
    if frame.len() < ev::ETHHDR + at::KEY_HEADER {
        ctx.log_fmt(format_args!(
            "wifi-driver:   an EAPOL frame of {} bytes is shorter than the {} its ethernet and key headers \
             need - not read",
            frame.len(),
            ev::ETHHDR + at::KEY_HEADER
        ));
        return None;
    }
    let k = &frame[ev::ETHHDR..];
    let be16 = |o: usize| u16::from_be_bytes([k[o], k[o + 1]]);
    let version = k[at::PKT_VERSION];
    let ptype = k[at::PKT_TYPE];
    let len = be16(at::PKT_LEN);
    if ptype != TYPE_KEY {
        ctx.log_fmt(format_args!(
            "wifi-driver:   EAPOL version {} type {} ({} bytes) - not a key frame, so not the handshake",
            version, ptype, len
        ));
        return None;
    }
    let desc = k[at::KEY_DESC];
    if desc != DESC_RSN && desc != DESC_WPA {
        ctx.log_fmt(format_args!(
            "wifi-driver:   EAPOL-Key with descriptor type {} - neither RSN (2) nor WPA (254), not read",
            desc
        ));
        return None;
    }
    let key = Key {
        desc,
        info: be16(at::KEY_INFO),
        key_len: be16(at::KEY_LEN),
        replay: u64::from_be_bytes([
            k[at::KEY_REPLAY],
            k[at::KEY_REPLAY + 1],
            k[at::KEY_REPLAY + 2],
            k[at::KEY_REPLAY + 3],
            k[at::KEY_REPLAY + 4],
            k[at::KEY_REPLAY + 5],
            k[at::KEY_REPLAY + 6],
            k[at::KEY_REPLAY + 7],
        ]),
        pay_len: be16(at::KEY_PAYLEN),
    };
    let i = key.info;
    ctx.log_fmt(format_args!(
        "wifi-driver:   EAPOL-Key from {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} - {} - descriptor {} \
         version {} info {:#06x} [{}{}{}{}{}{}{}] key_len {} replay {} key_data {} bytes; nonce begins \
         {:02x}{:02x}{:02x}{:02x}, mic begins {:02x}{:02x}",
        frame[6], frame[7], frame[8], frame[9], frame[10], frame[11],
        key.which(),
        if desc == DESC_RSN { "RSN" } else { "WPA" },
        i & info::VERSION_MASK,
        i,
        if i & info::PAIRWISE != 0 { "pairwise " } else { "group " },
        if i & info::KEYACK != 0 { "ack " } else { "" },
        if i & info::KEYMIC != 0 { "mic " } else { "" },
        if i & info::INSTALL != 0 { "install " } else { "" },
        if i & info::SECURE != 0 { "secure " } else { "" },
        if i & info::ENCRYPTED != 0 { "encrypted " } else { "" },
        if i & (info::ERROR | info::REQUEST) != 0 { "error/request " } else { "" },
        key.key_len,
        key.replay,
        key.pay_len,
        k[at::KEY_NONCE], k[at::KEY_NONCE + 1], k[at::KEY_NONCE + 2], k[at::KEY_NONCE + 3],
        k[at::KEY_MIC], k[at::KEY_MIC + 1],
    ));
    Some(key)
}
