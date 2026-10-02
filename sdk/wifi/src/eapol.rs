// SPDX-License-Identifier: GPL-2.0-only
//! EAPOL-Key frames: the four messages of the WPA2 handshake, as they arrive on the data channel.
//!
//! This firmware has no supplicant (`docs/wifi.md` §37), so the handshake is the host's - as it is for
//! OpenBSD, whose `bwfm_rx` hands every frame with ethertype `ETHERTYPE_EAPOL` to
//! `ieee80211_eapol_key_input`. This module is the frame layer of that: recognising a key frame and
//! reading its header (`describe`), deriving the pairwise transient key (`derive_ptk`), building and
//! signing our own messages (`build_key_frame`), verifying the access point's (`check_mic`), and finding
//! the group key inside message 3 (`find_gtk`). The state machine that orders them is `join.rs` for the
//! four-way handshake and `frames::group_rekey` for the group-key rekeys that follow it.
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

/// An Ethernet header: the EAPOL frame follows it (Linux names this length ETH_HLEN).
const ETHHDR: usize = 14;

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
    /// The authenticator's nonce (message 1 and 3 carry it; 2 and 4 carry ours).
    pub nonce: [u8; 32],
    /// The access point's address - the ethernet source, which is also the AA of the key derivation.
    pub from: [u8; 6],
    /// Where the key data begins in the frame this was read from, and how long it is (`pay_len`, bounded
    /// by what actually arrived).
    pub key_data_at: usize,
    pub key_data_len: usize,
}

/// The pairwise transient key, as IEEE 802.11-2020 §12.7.1.3 cuts the 384 PRF bits: the confirmation key
/// signs the handshake, the encryption key opens message 3's key data, the temporal key encrypts frames.
pub struct Ptk {
    pub kck: [u8; 16],
    pub kek: [u8; 16],
    pub tk: [u8; 16],
}

/// `ieee80211_derive_ptk`, quoted: `PRF-384(PMK, "Pairwise key expansion", Min(AA,SPA) || Max(AA,SPA) ||
/// Min(ANonce,SNonce) || Max(ANonce,SNonce))`. The label goes in WITH its NUL (23 bytes) for the SHA-1 PRF.
pub fn derive_ptk(pmk: &[u8; 32], aa: &[u8; 6], spa: &[u8; 6], anonce: &[u8; 32], snonce: &[u8; 32]) -> Ptk {
    let mut ctx = [0u8; 12 + 64];
    let aa_first = aa < spa;
    ctx[0..6].copy_from_slice(if aa_first { aa } else { spa });
    ctx[6..12].copy_from_slice(if aa_first { spa } else { aa });
    let a_first = anonce < snonce;
    ctx[12..44].copy_from_slice(if a_first { anonce } else { snonce });
    ctx[44..76].copy_from_slice(if a_first { snonce } else { anonce });
    let mut out = [0u8; 48];
    crate::crypto::prf_sha1(pmk, b"Pairwise key expansion\0", &ctx, &mut out);
    let mut ptk = Ptk { kck: [0; 16], kek: [0; 16], tk: [0; 16] };
    ptk.kck.copy_from_slice(&out[0..16]);
    ptk.kek.copy_from_slice(&out[16..32]);
    ptk.tk.copy_from_slice(&out[32..48]);
    ptk
}

/// Build an EAPOL-Key frame from the station, as `ieee80211_send_eapol_key` does: ethernet header, then
/// the 99-byte key descriptor, then `key_data`; `key->len` is the byte count after the 4-byte 802.1X
/// header; the descriptor version is 2 (CCMP, HMAC-SHA1 MIC); the MIC - if `kck` is given - is
/// `HMAC-SHA1(KCK, version .. end)` with the MIC field zeroed, truncated to its first 16 bytes
/// (`ieee80211_eapol_key_mic`, `EAPOL_KEY_DESC_V2`). Returns the frame length, or 0 if `out` is too small.
pub fn build_key_frame(
    out: &mut [u8],
    to: &[u8; 6],
    from: &[u8; 6],
    info: u16,
    replay: u64,
    nonce: &[u8; 32],
    key_data: &[u8],
    kck: Option<&[u8; 16]>,
) -> usize {
    let total = ETHHDR + at::KEY_HEADER + key_data.len();
    if out.len() < total {
        return 0;
    }
    out[..total].fill(0);
    out[0..6].copy_from_slice(to);
    out[6..12].copy_from_slice(from);
    out[12..14].copy_from_slice(&ETHERTYPE_EAPOL.to_be_bytes());
    let k = &mut out[ETHHDR..total];
    k[at::PKT_VERSION] = 1;
    k[at::PKT_TYPE] = TYPE_KEY;
    let body_len = (at::KEY_HEADER - 4 + key_data.len()) as u16;
    k[at::PKT_LEN..at::PKT_LEN + 2].copy_from_slice(&body_len.to_be_bytes());
    k[at::KEY_DESC] = DESC_RSN;
    let info = info | 2; // descriptor version 2: HMAC-SHA1 MIC, AES key wrap
    k[at::KEY_INFO..at::KEY_INFO + 2].copy_from_slice(&info.to_be_bytes());
    // `keylen` stays 0 for RSN - only WPA sets it in message 2 (`ieee80211_send_4way_msg2`).
    k[at::KEY_REPLAY..at::KEY_REPLAY + 8].copy_from_slice(&replay.to_be_bytes());
    k[at::KEY_NONCE..at::KEY_NONCE + 32].copy_from_slice(nonce);
    k[at::KEY_PAYLEN..at::KEY_PAYLEN + 2].copy_from_slice(&(key_data.len() as u16).to_be_bytes());
    k[at::KEY_HEADER..].copy_from_slice(key_data);
    if let Some(kck) = kck {
        let mic = mic_over(k, kck);
        k[at::KEY_MIC..at::KEY_MIC + 16].copy_from_slice(&mic);
    }
    total
}

/// The MIC of an EAPOL-Key frame body (from `version` to the end of the key data), MIC field as it stands.
fn mic_over(eapol: &[u8], kck: &[u8; 16]) -> [u8; 16] {
    let d = crate::crypto::hmac_sha1(kck, eapol);
    let mut mic = [0u8; 16];
    mic.copy_from_slice(&d[..16]);
    mic
}

/// `ieee80211_eapol_key_check_mic`: recompute with the MIC field zeroed and compare. `eapol` is the frame
/// from the `version` byte to the end of the key data. Bounded copy: a key frame this driver accepts is at
/// most `MAX_KEY_FRAME` bytes.
pub const MAX_KEY_FRAME: usize = 1024;
pub fn check_mic(eapol: &[u8], kck: &[u8; 16]) -> bool {
    if eapol.len() < at::KEY_HEADER || eapol.len() > MAX_KEY_FRAME {
        return false;
    }
    let mut copy = [0u8; MAX_KEY_FRAME];
    let c = &mut copy[..eapol.len()];
    c.copy_from_slice(eapol);
    c[at::KEY_MIC..at::KEY_MIC + 16].fill(0);
    let want = mic_over(c, kck);
    // Compared in full, then decided - not byte by byte with an early exit.
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= want[i] ^ eapol[at::KEY_MIC + i];
    }
    diff == 0
}

/// The GTK KDE inside message 3's (decrypted) key data (`ieee80211_recv_4way_msg3`): element 0xdd, OUI
/// 00:0f:ac, data type 1, then `key id (2 bits) | tx (bit 2)`, a reserved byte, the key. Returns
/// `(key id, tx, key)`.
pub fn find_gtk(key_data: &[u8]) -> Option<(u8, bool, &[u8])> {
    let mut at = 0usize;
    while at + 2 <= key_data.len() {
        let id = key_data[at];
        let len = key_data[at + 1] as usize;
        if at + 2 + len > key_data.len() {
            return None;
        }
        // The key data is padded with 0xdd 0x00.. after the last element; a zero-length vendor element is
        // that padding and ends the walk.
        if id == 0xdd && len >= 6 && &key_data[at + 2..at + 5] == &[0x00, 0x0f, 0xac] && key_data[at + 5] == 1 {
            let kid = key_data[at + 6] & 3;
            let tx = key_data[at + 6] & 4 != 0;
            return Some((kid, tx, &key_data[at + 8..at + 2 + len]));
        }
        if id == 0xdd && len == 0 {
            return None;
        }
        at += 2 + len;
    }
    None
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
    if frame.len() < ETHHDR + at::KEY_HEADER {
        ctx.log_fmt(format_args!(
            "wifi-driver:   an EAPOL frame of {} bytes is shorter than the {} its ethernet and key headers \
             need - not read",
            frame.len(),
            ETHHDR + at::KEY_HEADER
        ));
        return None;
    }
    let k = &frame[ETHHDR..];
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
    let pay_len = be16(at::KEY_PAYLEN);
    let mut nonce = [0u8; 32];
    nonce.copy_from_slice(&k[at::KEY_NONCE..at::KEY_NONCE + 32]);
    let mut from = [0u8; 6];
    from.copy_from_slice(&frame[6..12]);
    let key_data_at = ETHHDR + at::KEY_HEADER;
    let key_data_len = core::cmp::min(pay_len as usize, frame.len().saturating_sub(key_data_at));
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
        pay_len,
        nonce,
        from,
        key_data_at,
        key_data_len,
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
