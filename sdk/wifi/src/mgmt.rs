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
//! Pure, and named nothing outside `core`, so `scripts/host_test_check.py` runs the tests at the bottom on
//! every build. The layout is IEEE 802.11-2020 9.3.3.3 (beacon) and 9.3.3.11 (probe response), which share it.

/// The frame control's first byte for the two frames that carry a network's description: version 0, type
/// management (0), subtype 8 for a beacon and 5 for a probe response.
const FC_BEACON: u8 = 0x80;
const FC_PROBE_RESP: u8 = 0x50;

/// The 24-byte management header, then the timestamp (8), the beacon interval (2) and the capability (2).
const CAPABILITY_AT: usize = 34;
const ELEMENTS_AT: usize = 36;

/// Element IDs: the SSID (at most 32 bytes; zero is a hidden network) and the DS Parameter Set, whose one
/// byte is the channel the network is on.
const EID_SSID: u8 = 0;
const EID_DS_PARAMS: u8 = 3;
const SSID_MAX: usize = 32;

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
