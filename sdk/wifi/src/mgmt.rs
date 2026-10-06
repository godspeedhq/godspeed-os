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
//! And the one frame a passive listener needs to SEND to stop being passive: a probe request
//! ([`probe_request`]), for a radio whose host builds its frames (the RTL8188CUS, from R5a).
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
