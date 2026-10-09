// SPDX-License-Identifier: GPL-2.0-only
//! An 802.11 beacon or probe response, read from the raw frame: who sent it, its capability field, and its
//! information elements - the SSID and the channel among them.
//!
//! A radio whose firmware forwards the frames themselves needs this; one that digests them for the host (the
//! Broadcom's BSS-info records) does not. The AIC8800 forwards each beacon inside a scan indication and the
//! RTL8188CUS hands up whatever it hears, so both read the same frame, and this is the one reading of it.
//! (The AIC8800's scan still reads its frame with `aic_wire::ResultInd`'s own accessors, written before this
//! existed; moving it here is recorded in `docs/wifi-usb.md` as the one-way debt it is.)
//!
//! Every length here comes from the air. A frame too short for its fixed fields is not a beacon, and an
//! element walk that runs off the end stops rather than reads past it.
//!
//! And the frames a host-built station SENDS (the RTL8188CUS): a probe request ([`probe_request`], R5a),
//! then the open-system authentication, the association request and the deauthentication that leaves
//! ([`auth_request`], [`assoc_request`], [`deauth`], R5b) - with the two answers read ([`auth_answer`],
//! [`assoc_answer`]).
//!
//! Pure, and named nothing outside `core`, so `scripts/host_test_check.py` runs the tests at the bottom on
//! every build. The layout is IEEE 802.11-2020 9.3.3.3 (beacon) and 9.3.3.11 (probe response), which share it,
//! and 9.3.3.10 (probe request).

/// The frame control's first byte for the two frames that carry a network's description: version 0, type
/// management (0), subtype 8 for a beacon and 5 for a probe response.
const FC_BEACON: u8 = 0x80;
const FC_PROBE_RESP: u8 = 0x50;

/// The 24-byte management header, then the timestamp (8), the beacon interval (2) and the capability (2).
const CAPABILITY_AT: usize = 34;
const ELEMENTS_AT: usize = 36;

/// Element IDs: the SSID (at most 32 bytes; zero is a hidden network) and the DS Parameter Set, whose one
/// byte is the channel the network is on; and the two rate lists a probe request carries.
const EID_SSID: u8 = 0;
const EID_SUPP_RATES: u8 = 1;
const EID_DS_PARAMS: u8 = 3;
const EID_EXT_SUPP_RATES: u8 = 50;
const SSID_MAX: usize = 32;

/// A probe request's frame control: version 0, type management, subtype 4.
const FC_PROBE_REQ: u8 = 0x40;
/// The rates a 2.4 GHz station offers, in the elements' unit of 500 kb/s: 1, 2, 5.5 and 11 Mb/s (DSSS and
/// CCK, flagged basic with bit 7, as every 2.4 GHz network requires them), then 6, 9, 12 and 18 - eight,
/// the most the Supported Rates element holds - and 24, 36, 48 and 54 in the Extended Supported Rates.
const RATES: [u8; 8] = [0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24];
const EXT_RATES: [u8; 4] = [0x30, 0x48, 0x60, 0x6C];
/// The longest probe request [`probe_request`] builds: the 24-byte header, an SSID element of up to 32
/// bytes, and the two rate elements.
pub const PROBE_REQUEST_MAX: usize = 24 + 2 + SSID_MAX + 2 + RATES.len() + 2 + EXT_RATES.len();

/// A probe request from `sa`, sequence number `seq`, into `out`; the bytes written. Broadcast, to any
/// network (the wildcard BSSID); for `ssid` empty, any SSID (the wildcard SSID) - which networks that
/// beacon answer and hidden ones do not - and for a name, that network, hidden or not. No FCS: the radio
/// appends it.
pub fn probe_request(sa: &[u8; 6], seq: u16, ssid: &[u8], out: &mut [u8; PROBE_REQUEST_MAX]) -> usize {
    let ssid = &ssid[..ssid.len().min(SSID_MAX)];
    out.fill(0);
    out[0] = FC_PROBE_REQ;
    // [2..4] duration: 0, the station's to leave for the access point to fill.
    out[4..10].copy_from_slice(&[0xFF; 6]);
    out[10..16].copy_from_slice(sa);
    out[16..22].copy_from_slice(&[0xFF; 6]);
    out[22..24].copy_from_slice(&((seq & 0x0FFF) << 4).to_le_bytes());
    let mut at = 24;
    for (id, body) in [(EID_SSID, ssid), (EID_SUPP_RATES, &RATES[..]), (EID_EXT_SUPP_RATES, &EXT_RATES[..])] {
        out[at] = id;
        out[at + 1] = body.len() as u8;
        out[at + 2..at + 2 + body.len()].copy_from_slice(body);
        at += 2 + body.len();
    }
    at
}

/// Whether `frame` (a beacon or probe response) was addressed to `us` - a probe response answering our
/// own probe request, rather than one heard on its way to somebody else's.
pub fn addressed_to(frame: &[u8], us: &[u8; 6]) -> bool {
    frame.len() >= 10 && &frame[4..10] == us
}

/// Frame control first bytes for the join's frames (type management): authentication (subtype 11),
/// association request (0) and response (1), deauthentication (12).
const FC_AUTH: u8 = 0xB0;
const FC_ASSOC_REQ: u8 = 0x00;
const FC_ASSOC_RESP: u8 = 0x10;
const FC_DEAUTH: u8 = 0xC0;
/// Element ID of the RSN element (WPA2), and the OUI and suite type of CCMP in it.
const EID_RSN: u8 = 48;
const SUITE_CCMP: [u8; 4] = [0x00, 0x0F, 0xAC, 4];

/// The capability field a station sends: ESS, short preamble and short slot time on 2.4 GHz, and Privacy
/// when the network's own capability has it - what mac80211's association request sets
/// (`ieee80211_add_link_elems`, `ieee80211_send_assoc`; `net/mac80211/mlme.c`).
pub const CAP_ESS: u16 = 1 << 0;
pub const CAP_PRIVACY: u16 = 1 << 4;
pub const CAP_SHORT_PREAMBLE: u16 = 1 << 5;
pub const CAP_SHORT_SLOT_TIME: u16 = 1 << 10;

/// The longest association request [`assoc_request`] builds: the header, capability and listen interval,
/// the SSID, the two rate elements, an RSN element of up to 40 bytes and up to `EXTRA_MAX` more.
pub const ASSOC_REQUEST_MAX: usize = 24 + 4 + 2 + SSID_MAX + 2 + RATES.len() + 2 + EXT_RATES.len() + 40 + EXTRA_MAX;
/// Room for the elements a station adds after the RSN element: the WMM information element (9 bytes,
/// R12a) and the HT capabilities element (28, R12b), with a little over.
pub const EXTRA_MAX: usize = 40;

/// The WMM information element a station sends to be a QoS station (WMM 2.2.1, the Wi-Fi Alliance's form of
/// 802.11e): vendor-specific (221), OUI 00:50:F2, type 2, subtype 0 (information), version 1, and a QoS Info
/// of 0 - no U-APSD - as mac80211 builds it (`ieee80211_add_wmm_info_ie`) for a station not asking for it.
pub const WMM_INFO: [u8; 9] = [221, 7, 0x00, 0x50, 0xF2, 0x02, 0x00, 0x01, 0x00];

/// The element ID of HT Capabilities (9.4.2.55), and its two short guard interval bits in the capability
/// field (bit 5 for 20 MHz, bit 6 for 40 MHz).
const EID_HT_CAP: u8 = 45;
pub const HT_CAP_SGI_20: u16 = 1 << 5;
pub const HT_CAP_SGI_40: u16 = 1 << 6;

/// The HT Capabilities element a one-stream, 20 MHz station sends (R12b), as mac80211 builds it from
/// `rtl8xxxu`'s band (`ieee80211_add_ht_ie`, `rtl8xxxu_probe`):
/// - capability `0x002C`: short guard interval at 20 MHz, and SM power save disabled (`SMPS_OFF`, 3 at
///   bits 2-3), nothing for 40 MHz, which this driver never tunes;
/// - A-MPDU parameters `0x1F`: up to 64 KiB (3) with a 16 us minimum spacing (7 at bits 2-4);
/// - the MCS set: MCS 0-7 received (`rx_mask[0] = 0xff`), MCS 32 (`rx_mask[4] = 0x01`), and the transmit
///   set defined (`IEEE80211_HT_MCS_TX_DEFINED`, byte 12);
/// - extended capabilities, beamforming and antenna selection all zero.
pub const HT_CAP: [u8; 28] = [
    EID_HT_CAP, 26,
    0x2C, 0x00,
    0x1F,
    0xFF, 0, 0, 0, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x01, 0, 0, 0,
    0, 0,
    0, 0, 0, 0,
    0,
];

/// What a station needs from its access point's HT Capabilities element: the capability field (for the
/// short guard interval) and the first two bytes of the MCS set it receives (one and two streams) - the
/// firmware's rate mask (`rtl8xxxu`'s `ht_cap.mcs.rx_mask[0] << 12 | rx_mask[1] << 20`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HtCap {
    pub cap: u16,
    pub mcs: [u8; 2],
}

/// The access point's HT capabilities, when `ies` carry the element at full length.
pub fn ht_cap(ies: &[u8]) -> Option<HtCap> {
    let e = element(ies, EID_HT_CAP)?;
    if e.len() < 26 {
        return None;
    }
    Some(HtCap { cap: u16::from_le_bytes([e[0], e[1]]), mcs: [e[3], e[4]] })
}

/// Management subtype Action (9.3.3.14), and the Block Ack category's ADDBA request and response.
const FC_ACTION: u8 = 0xD0;
const CATEGORY_BLOCK_ACK: u8 = 3;
const ADDBA_REQUEST: u8 = 0;
const ADDBA_RESPONSE: u8 = 1;
/// The status a station that does not take aggregated frames answers with (`WLAN_STATUS_REQUEST_DECLINED`,
/// what mac80211 sends when it will not start a receive session).
const STATUS_REQUEST_DECLINED: u16 = 37;
/// An ADDBA response: the header, category, action, dialog token, status, Block Ack parameters, timeout.
pub const ADDBA_RESPONSE_LEN: usize = 24 + 9;

/// An access point's ADDBA request to `us` (9.6.4.2): its dialog token and Block Ack parameter set, for
/// the answer to repeat. `None` for any other frame.
pub fn addba_request(frame: &[u8], bssid: &[u8; 6], us: &[u8; 6]) -> Option<(u8, u16)> {
    if frame.len() < 24 + 9 || frame[0] != FC_ACTION || !between(frame, bssid, us) {
        return None;
    }
    let b = &frame[24..];
    (b[0] == CATEGORY_BLOCK_ACK && b[1] == ADDBA_REQUEST).then(|| (b[2], u16::from_le_bytes([b[3], b[4]])))
}

/// The answer that declines it (9.6.4.3): the same dialog token and parameter set, status 37, no timeout.
/// The access point then sends this station's frames unaggregated, which is what a station that does not
/// reorder aggregated frames needs.
pub fn addba_decline(
    sa: &[u8; 6], bssid: &[u8; 6], seq: u16, dialog: u8, params: u16, out: &mut [u8; ADDBA_RESPONSE_LEN],
) -> usize {
    unicast_header(out, FC_ACTION, sa, bssid, seq);
    let p = params.to_le_bytes();
    let s = STATUS_REQUEST_DECLINED.to_le_bytes();
    out[24..33].copy_from_slice(&[CATEGORY_BLOCK_ACK, ADDBA_RESPONSE, dialog, s[0], s[1], p[0], p[1], 0, 0]);
    ADDBA_RESPONSE_LEN
}

/// Whether `ies` carry a WMM element - the information (subtype 0) or parameter (subtype 1) form, which an
/// access point advertises in its beacons when it does QoS. A walk over every vendor element, since a
/// beacon carries several (WPS, the vendor's own) and only one is WMM. An element running past the end
/// stops the walk: the lengths are from the air.
pub fn has_wmm(ies: &[u8]) -> bool {
    let mut at = 0usize;
    while at + 2 <= ies.len() {
        let (id, len) = (ies[at], ies[at + 1] as usize);
        let end = at + 2 + len;
        if end > ies.len() {
            return false;
        }
        let b = &ies[at + 2..end];
        if id == 221 && b.len() >= 6 && b[..4] == [0x00, 0x50, 0xF2, 0x02] && b[4] <= 1 && b[5] == 1 {
            return true;
        }
        at = end;
    }
    false
}
/// An authentication or deauthentication frame from a station: the header and six (or two) body bytes.
pub const AUTH_LEN: usize = 24 + 6;
pub const DEAUTH_LEN: usize = 24 + 2;

/// The header every unicast management frame from a station to an access point has: to `bssid`, from `sa`,
/// in `bssid`'s network, sequence `seq`.
fn unicast_header(out: &mut [u8], fc: u8, sa: &[u8; 6], bssid: &[u8; 6], seq: u16) {
    out[0] = fc;
    out[1] = 0;
    out[2] = 0;
    out[3] = 0;
    out[4..10].copy_from_slice(bssid);
    out[10..16].copy_from_slice(sa);
    out[16..22].copy_from_slice(bssid);
    out[22..24].copy_from_slice(&((seq & 0x0FFF) << 4).to_le_bytes());
}

/// Open System authentication, transaction 1 (IEEE 802.11-2020 9.3.3.12): algorithm 0, sequence 1, status 0.
pub fn auth_request(sa: &[u8; 6], bssid: &[u8; 6], seq: u16, out: &mut [u8; AUTH_LEN]) -> usize {
    unicast_header(out, FC_AUTH, sa, bssid, seq);
    out[24..30].copy_from_slice(&[0, 0, 1, 0, 0, 0]);
    AUTH_LEN
}

/// An association request (9.3.3.6): `cap`, a listen interval of `listen` beacon intervals, the SSID, the
/// station's rates, `rsn` when the network is joined with WPA2 - the element the four-way handshake's
/// message 2 must then repeat byte for byte - and `extra`, whole elements after it (`WMM_INFO`, R12a).
/// `None` when `rsn` or `extra` is too long for the frame.
pub fn assoc_request(
    sa: &[u8; 6], bssid: &[u8; 6], seq: u16, cap: u16, listen: u16, ssid: &[u8], rsn: Option<&[u8]>,
    extra: &[u8], out: &mut [u8; ASSOC_REQUEST_MAX],
) -> Option<usize> {
    let ssid = &ssid[..ssid.len().min(SSID_MAX)];
    let rsn = rsn.unwrap_or(&[]);
    if rsn.len() > 40 || extra.len() > EXTRA_MAX {
        return None;
    }
    out.fill(0);
    unicast_header(&mut out[..], FC_ASSOC_REQ, sa, bssid, seq);
    out[24..26].copy_from_slice(&cap.to_le_bytes());
    out[26..28].copy_from_slice(&listen.to_le_bytes());
    let mut at = 28;
    for (id, body) in [(EID_SSID, ssid), (EID_SUPP_RATES, &RATES[..]), (EID_EXT_SUPP_RATES, &EXT_RATES[..])] {
        out[at] = id;
        out[at + 1] = body.len() as u8;
        out[at + 2..at + 2 + body.len()].copy_from_slice(body);
        at += 2 + body.len();
    }
    // `rsn` is a whole element, id and length included (`eapol::RSN_IE`), and so is each of `extra`.
    out[at..at + rsn.len()].copy_from_slice(rsn);
    at += rsn.len();
    out[at..at + extra.len()].copy_from_slice(extra);
    Some(at + extra.len())
}

/// A deauthentication (9.3.3.13) with `reason` - 3, "leaving", for a station that is going.
pub fn deauth(sa: &[u8; 6], bssid: &[u8; 6], seq: u16, reason: u16, out: &mut [u8; DEAUTH_LEN]) -> usize {
    unicast_header(out, FC_DEAUTH, sa, bssid, seq);
    out[24..26].copy_from_slice(&reason.to_le_bytes());
    DEAUTH_LEN
}

/// `frame` is from `bssid` (its transmitter and its BSSID) to `us`.
fn between(frame: &[u8], bssid: &[u8; 6], us: &[u8; 6]) -> bool {
    frame.len() >= 24 && &frame[4..10] == us && &frame[10..16] == bssid && &frame[16..22] == bssid
}

/// The access point's answer to our Open System authentication: its status code (0 is success), when
/// `frame` is an authentication frame from `bssid` to `us`, Open System, transaction 2.
pub fn auth_answer(frame: &[u8], bssid: &[u8; 6], us: &[u8; 6]) -> Option<u16> {
    if frame.len() < 30 || frame[0] != FC_AUTH || !between(frame, bssid, us) {
        return None;
    }
    let alg = u16::from_le_bytes([frame[24], frame[25]]);
    let trans = u16::from_le_bytes([frame[26], frame[27]]);
    (alg == 0 && trans == 2).then(|| u16::from_le_bytes([frame[28], frame[29]]))
}

/// The access point's answer to our association request: `(status, association ID)` when `frame` is an
/// association response from `bssid` to `us` (9.3.3.7). The ID's two top bits are always set on the air
/// and are not part of it.
pub fn assoc_answer(frame: &[u8], bssid: &[u8; 6], us: &[u8; 6]) -> Option<(u16, u16)> {
    if frame.len() < 30 || frame[0] != FC_ASSOC_RESP || !between(frame, bssid, us) {
        return None;
    }
    Some((u16::from_le_bytes([frame[26], frame[27]]), u16::from_le_bytes([frame[28], frame[29]]) & 0x3FFF))
}

/// What a network's RSN element (in its beacon or probe response) says the `eapol::RSN_IE` join needs:
/// CCMP as the group cipher, and CCMP among the pairwise ones. `None` when there is no RSN element, or it
/// is too short to read those.
pub fn rsn_is_ccmp(ies: &[u8]) -> Option<bool> {
    let r = element(ies, EID_RSN)?;
    // version(2), group(4), pairwise count(2), pairwise suites(4 each)...
    if r.len() < 8 {
        return None;
    }
    let group = &r[2..6];
    let n = u16::from_le_bytes([r[6], r[7]]) as usize;
    if r.len() < 8 + 4 * n {
        return None;
    }
    let pairwise = (0..n).any(|i| r[8 + 4 * i..12 + 4 * i] == SUITE_CCMP);
    Some(group == SUITE_CCMP && pairwise)
}

/// A beacon or probe response, borrowed from the frame it was read from.
pub struct Beacon<'a> {
    frame: &'a [u8],
}

/// Read `frame` (the 802.11 frame from its frame control on, no FCS needed) as a beacon or probe response.
/// `None` for any other frame, or one too short for the fixed fields.
pub fn beacon(frame: &[u8]) -> Option<Beacon<'_>> {
    if frame.len() < ELEMENTS_AT || (frame[0] != FC_BEACON && frame[0] != FC_PROBE_RESP) {
        return None;
    }
    Some(Beacon { frame })
}

impl<'a> Beacon<'a> {
    /// Whether it was a probe response (an answer to somebody's probe) rather than a beacon.
    pub fn answers_a_probe(&self) -> bool {
        self.frame[0] == FC_PROBE_RESP
    }

    /// The BSSID: the third address, at offset 16.
    pub fn bssid(&self) -> [u8; 6] {
        let mut b = [0u8; 6];
        b.copy_from_slice(&self.frame[16..22]);
        b
    }

    /// The capability field, after the timestamp and the beacon interval.
    pub fn capability(&self) -> u16 {
        u16::from_le_bytes([self.frame[CAPABILITY_AT], self.frame[CAPABILITY_AT + 1]])
    }

    /// The information elements, everything after the fixed fields. An FCS left on the end reads as a
    /// trailing element the walk stops at, so a caller need not know whether its radio strips it.
    pub fn elements(&self) -> &'a [u8] {
        &self.frame[ELEMENTS_AT..]
    }

    /// The SSID element's body, if there is one of a legal length. Empty for a hidden network.
    pub fn ssid(&self) -> Option<&'a [u8]> {
        element(self.elements(), EID_SSID).filter(|s| s.len() <= SSID_MAX)
    }

    /// The channel the network says it is on (the DS Parameter Set), if it says. A radio tuned to one
    /// channel hears its neighbours' beacons too, so this, not the tuned channel, is the network's own.
    pub fn channel(&self) -> Option<u8> {
        element(self.elements(), EID_DS_PARAMS).filter(|b| b.len() == 1).map(|b| b[0])
    }
}

/// The body of the first element `id` in an element walk, or `None` if it is not there or the walk is cut
/// short before it.
pub fn element(ies: &[u8], id: u8) -> Option<&[u8]> {
    let mut at = 0usize;
    while at + 2 <= ies.len() {
        let len = ies[at + 1] as usize;
        let end = at + 2 + len;
        if end > ies.len() {
            return None;
        }
        if ies[at] == id {
            return Some(&ies[at + 2..end]);
        }
        at = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wildcard_probe_request_is_the_standards_frame() {
        let sa = [0x02, 0, 0, 0, 0, 9];
        let mut f = [0u8; PROBE_REQUEST_MAX];
        let n = probe_request(&sa, 0x123, &[], &mut f);
        assert_eq!(n, 24 + 2 + 2 + 8 + 2 + 4);
        assert_eq!(f[0], 0x40);
        assert_eq!(&f[4..10], &[0xFF; 6], "to everyone");
        assert_eq!(&f[10..16], &sa);
        assert_eq!(&f[16..22], &[0xFF; 6], "any network");
        assert_eq!(u16::from_le_bytes([f[22], f[23]]), 0x1230, "the sequence number above the fragment number");
        assert_eq!(&f[24..26], &[0, 0], "the wildcard SSID: element 0, length 0");
        assert_eq!(&f[26..28], &[1, 8]);
        assert_eq!(f[28], 0x82, "1 Mb/s, basic");
        assert_eq!(&f[36..38], &[50, 4]);
        assert_eq!(f[41], 0x6C, "54 Mb/s last");
    }

    #[test]
    fn a_directed_probe_request_carries_the_name() {
        let mut f = [0u8; PROBE_REQUEST_MAX];
        let n = probe_request(&[2, 0, 0, 0, 0, 1], 0, b"net", &mut f);
        assert_eq!(&f[24..29], &[0, 3, b'n', b'e', b't']);
        assert_eq!(n, 24 + 5 + 10 + 6);
        // A name over 32 bytes is cut to the element's limit rather than written past it.
        assert_eq!(probe_request(&[0; 6], 0, &[b'a'; 40], &mut f), PROBE_REQUEST_MAX);
    }

    #[test]
    fn the_ht_element_is_a_one_stream_20_mhz_stations() {
        assert_eq!(HT_CAP.len(), 2 + HT_CAP[1] as usize);
        // Read back as an access point's would be: SGI 20, MCS 0-7.
        let c = ht_cap(&HT_CAP).unwrap();
        assert_eq!((c.cap & HT_CAP_SGI_20 != 0, c.cap & HT_CAP_SGI_40 != 0, c.mcs), (true, false, [0xFF, 0]));
        assert_eq!(ht_cap(&HT_CAP[..20]), None, "short");
    }

    #[test]
    fn an_addba_request_is_declined_in_kind() {
        let (us, ap) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 9]);
        let mut f = [0u8; 24 + 9];
        f[0] = 0xD0;
        f[4..10].copy_from_slice(&us);
        f[10..16].copy_from_slice(&ap);
        f[16..22].copy_from_slice(&ap);
        f[24..33].copy_from_slice(&[3, 0, 0x17, 0x02, 0x10, 0, 0, 0x10, 0]);
        assert_eq!(addba_request(&f, &ap, &us), Some((0x17, 0x1002)));
        assert_eq!(addba_request(&f, &ap, &ap), None, "not to us");
        let mut r = [0u8; ADDBA_RESPONSE_LEN];
        assert_eq!(addba_decline(&us, &ap, 4, 0x17, 0x1002, &mut r), 33);
        assert_eq!(r[0], 0xD0);
        assert_eq!(&r[24..33], &[3, 1, 0x17, 37, 0, 0x02, 0x10, 0, 0]);
    }

    #[test]
    fn wmm_is_found_among_the_vendor_elements() {
        // A WPS element (type 4) first, then the WMM parameter element (subtype 1, version 1).
        let ies = [0, 0, 221, 4, 0x00, 0x50, 0xF2, 0x04, 221, 7, 0x00, 0x50, 0xF2, 0x02, 0x01, 0x01, 0x80];
        assert!(has_wmm(&ies));
        assert!(has_wmm(&WMM_INFO));
        assert!(!has_wmm(&ies[..8]), "only the WPS element");
        assert!(!has_wmm(&[221, 9, 0x00, 0x50, 0xF2, 0x02]), "an element longer than what is left");
    }

    #[test]
    fn the_join_frames_are_the_standards() {
        let (us, ap) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 9]);
        let mut a = [0u8; AUTH_LEN];
        assert_eq!(auth_request(&us, &ap, 5, &mut a), 30);
        assert_eq!(a[0], 0xB0);
        assert_eq!(&a[4..10], &ap, "to the access point");
        assert_eq!(&a[10..16], &us);
        assert_eq!(&a[16..22], &ap, "in its network");
        assert_eq!(&a[24..30], &[0, 0, 1, 0, 0, 0], "open system, transaction 1, status 0");

        let rsn = [0x30, 0x02, 0x01, 0x00];
        let mut q = [0u8; ASSOC_REQUEST_MAX];
        let cap = CAP_ESS | CAP_PRIVACY;
        let n = assoc_request(&us, &ap, 6, cap, 10, b"net", Some(&rsn), &[], &mut q).unwrap();
        assert_eq!(q[0], 0x00);
        assert_eq!(u16::from_le_bytes([q[24], q[25]]), 0x11);
        assert_eq!(u16::from_le_bytes([q[26], q[27]]), 10);
        assert_eq!(&q[28..33], &[0, 3, b'n', b'e', b't']);
        assert_eq!(&q[n - 4..n], &rsn, "the RSN element last, whole");
        assert_eq!(n, 28 + 5 + 10 + 6 + 4);
        assert!(assoc_request(&us, &ap, 6, cap, 10, b"net", Some(&[0u8; 41]), &[], &mut q).is_none());
        // The WMM element after the RSN one, whole.
        let n = assoc_request(&us, &ap, 6, cap, 10, b"net", Some(&rsn), &WMM_INFO, &mut q).unwrap();
        assert_eq!(&q[n - 13..n - 9], &rsn);
        assert_eq!(&q[n - 9..n], &WMM_INFO);
        assert!(assoc_request(&us, &ap, 6, cap, 10, b"net", None, &[0u8; EXTRA_MAX + 1], &mut q).is_none());

        let mut d = [0u8; DEAUTH_LEN];
        assert_eq!(deauth(&us, &ap, 7, 3, &mut d), 26);
        assert_eq!((d[0], d[24], d[25]), (0xC0, 3, 0));
    }

    #[test]
    fn the_answers_are_read_only_from_our_access_point_to_us() {
        let (us, ap, other) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 9], [2, 0, 0, 0, 0, 8]);
        let mut f = [0u8; 32];
        f[0] = 0xB0;
        f[4..10].copy_from_slice(&us);
        f[10..16].copy_from_slice(&ap);
        f[16..22].copy_from_slice(&ap);
        f[24..30].copy_from_slice(&[0, 0, 2, 0, 0, 0]);
        assert_eq!(auth_answer(&f[..30], &ap, &us), Some(0));
        assert_eq!(auth_answer(&f[..30], &other, &us), None, "another access point's");
        f[26] = 1;
        assert_eq!(auth_answer(&f[..30], &ap, &us), None, "transaction 1 is a request, not the answer");
        f[0] = 0x10;
        f[24..32].copy_from_slice(&[0x11, 0, 0, 0, 0x05, 0xC0, 1, 8]);
        assert_eq!(assoc_answer(&f, &ap, &us), Some((0, 5)), "the two top bits of the ID are not part of it");
        f[26] = 17;
        assert_eq!(assoc_answer(&f, &ap, &us), Some((17, 5)));
    }

    #[test]
    fn rsn_is_ccmp_reads_group_and_pairwise() {
        let ccmp = [48, 20, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 4, 1, 0, 0, 0x0F, 0xAC, 2, 0, 0];
        assert_eq!(rsn_is_ccmp(&ccmp), Some(true));
        let mut tkip_group = ccmp;
        tkip_group[7] = 2;
        assert_eq!(rsn_is_ccmp(&tkip_group), Some(false));
        assert_eq!(rsn_is_ccmp(&[0, 0]), None, "no RSN element at all");
        assert_eq!(rsn_is_ccmp(&[48, 4, 1, 0, 0, 0]), None, "too short to read");
    }

    #[test]
    fn addressed_to_reads_the_first_address() {
        let us = [2, 0, 0, 0, 0, 7];
        let (mut f, n) = frame(FC_PROBE_RESP, 0, &[]);
        assert!(!addressed_to(&f[..n], &us), "the helper frames go to broadcast");
        f[4..10].copy_from_slice(&us);
        assert!(addressed_to(&f[..n], &us));
        assert!(!addressed_to(&f[..9], &us), "too short to say");
    }

    /// A beacon with the given fixed fields and elements appended.
    fn frame(fc: u8, cap: u16, ies: &[u8]) -> ([u8; 128], usize) {
        let mut f = [0u8; 128];
        f[0] = fc;
        f[4..10].copy_from_slice(&[0xff; 6]); // broadcast
        f[10..16].copy_from_slice(&[2, 0, 0, 0, 0, 1]); // transmitter
        f[16..22].copy_from_slice(&[2, 0, 0, 0, 0, 1]); // BSSID
        f[32] = 100; // beacon interval
        f[34..36].copy_from_slice(&cap.to_le_bytes());
        f[36..36 + ies.len()].copy_from_slice(ies);
        (f, 36 + ies.len())
    }

    #[test]
    fn a_beacon_and_its_elements() {
        let (f, n) = frame(0x80, 0x0411, &[0, 4, b'h', b'o', b'm', b'e', 1, 1, 0x82, 3, 1, 6]);
        let b = beacon(&f[..n]).expect("a beacon");
        assert!(!b.answers_a_probe());
        assert_eq!(b.bssid(), [2, 0, 0, 0, 0, 1]);
        assert_eq!(b.capability(), 0x0411);
        assert_eq!(b.ssid(), Some(&b"home"[..]));
        assert_eq!(b.channel(), Some(6));
    }

    #[test]
    fn a_probe_response_reads_the_same() {
        let (f, n) = frame(0x50, 0, &[0, 0]);
        let b = beacon(&f[..n]).expect("a probe response");
        assert!(b.answers_a_probe());
        assert_eq!(b.ssid(), Some(&b""[..])); // hidden
        assert_eq!(b.channel(), None);
    }

    #[test]
    fn what_is_not_a_beacon() {
        let (f, n) = frame(0x08, 0, &[0, 0]); // a data frame
        assert!(beacon(&f[..n]).is_none());
        let (f, _) = frame(0x80, 0, &[]);
        assert!(beacon(&f[..35]).is_none()); // one byte short of the fixed fields
    }

    #[test]
    fn lengths_from_the_air_are_not_trusted() {
        // An SSID element claiming 40 bytes where 4 arrived: no SSID, and no read past the end.
        let (f, n) = frame(0x80, 0, &[0, 40, b'a', b'b', b'c', b'd']);
        assert_eq!(beacon(&f[..n]).unwrap().ssid(), None);
        // An SSID longer than 32 is not an SSID.
        let mut ies = [b'x'; 35];
        ies[0] = 0;
        ies[1] = 33;
        let (f, n) = frame(0x80, 0, &ies);
        assert_eq!(beacon(&f[..n]).unwrap().ssid(), None);
        // A DS element of the wrong length is not a channel.
        let (f, n) = frame(0x80, 0, &[3, 2, 6, 6]);
        assert_eq!(beacon(&f[..n]).unwrap().channel(), None);
        // A truncated element BEFORE the one asked for ends the walk.
        assert_eq!(element(&[7, 9, 1], 3), None);
    }
}
