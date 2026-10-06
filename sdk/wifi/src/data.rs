// SPDX-License-Identifier: GPL-2.0-only
//! An 802.11 DATA frame and the ethernet frame it carries, both ways - for every radio whose host sees the
//! raw frames: the AIC8800 hands up 802.11 data frames, and the RTL8188CUS both hands them up and takes
//! them from the host to send.
//!
//! `llc_payload` and `to_ethernet` were the AIC8800's (`aic_wire.rs`), written from the vendor driver's
//! `rwnx_rxdataind_aicwf`. They are 802.11, not AIC8800, and the USB dongle needed exactly them for the
//! WPA2 handshake (R5c, `docs/wifi-usb.md`), so they moved here rather than being written twice.
//! `to_80211` is the other direction, for a station sending to its access point.
//!
//! Pure, and naming nothing outside `core`, so `scripts/host_test_check.py` runs the tests at the bottom on
//! every build. The layout is IEEE 802.11-2020 9.3.2.1 (the data frame) with the RFC 1042 LLC/SNAP header.

/// LLC/SNAP for an ethertype-carrying frame (RFC 1042): DSAP and SSAP `aa`, UI `03`, OUI `00 00 00`.
const LLC_SNAP: [u8; 6] = [0xaa, 0xaa, 0x03, 0x00, 0x00, 0x00];
/// An ethernet header: destination, source, ethertype.
pub const ETH_HEADER: usize = 14;
/// What `to_80211` puts in front of the ethernet payload: the 24-byte non-QoS header, LLC/SNAP and the
/// ethertype. An ethernet frame of `n` bytes becomes `n - ETH_HEADER + DATA_OVERHEAD`.
pub const DATA_OVERHEAD: usize = 24 + 8;

/// A received data frame, as the vendor driver turns it into an ethernet frame (`rwnx_rxdataind_aicwf`):
/// DA is address 1, SA is address 3 on a frame from the access point (address 2 otherwise).
pub struct DataIn<'a> {
    pub ethertype: u16,
    pub da: [u8; 6],
    pub sa: [u8; 6],
    pub body: &'a [u8],
}

/// The ethertype and body of a received 802.11 data frame: the MAC header (24 bytes, 26 with QoS, 4 more
/// when the order bit is set - the vendor's own test), the 8-byte CCMP header when `ccmp` (the firmware
/// decrypts but leaves it; the frame's protected bit is not what decides it, `decr_status` is), then
/// LLC/SNAP `aa aa 03 00 00 00` and the ethertype. `None` for anything that is not a data frame carrying
/// LLC/SNAP. Whether a CCMP frame's length counts its 8-byte MIC is not in the source (the line that would
/// strip it is commented out): a body may carry 8 trailing bytes, which EAPOL and IP both bound by their
/// own length fields.
pub fn llc_payload(frame: &[u8], ccmp: bool) -> Option<DataIn<'_>> {
    if frame.len() < 24 || frame[0] & 0x0c != 0x08 {
        return None;
    }
    let qos = frame[0] & 0x80 != 0;
    let order = frame[1] & 0x80 != 0;
    let from_ds = frame[1] & 0x03 == 0x02;
    let mut at = 24 + if qos { 2 } else { 0 } + if order { 4 } else { 0 };
    if ccmp {
        at += 8;
    }
    if frame.len() < at + 8 || frame[at..at + 6] != LLC_SNAP {
        return None;
    }
    let mut da = [0u8; 6];
    da.copy_from_slice(&frame[4..10]);
    let mut sa = [0u8; 6];
    sa.copy_from_slice(if from_ds { &frame[16..22] } else { &frame[10..16] });
    Some(DataIn { ethertype: u16::from_be_bytes([frame[at + 6], frame[at + 7]]), da, sa, body: &frame[at + 8..] })
}

/// `d` as an ethernet frame into `out`: DA, SA, ethertype, body. 0 when `out` cannot hold it.
pub fn to_ethernet(d: &DataIn, out: &mut [u8]) -> usize {
    let n = ETH_HEADER + d.body.len();
    if n > out.len() {
        return 0;
    }
    out[0..6].copy_from_slice(&d.da);
    out[6..12].copy_from_slice(&d.sa);
    out[12..14].copy_from_slice(&d.ethertype.to_be_bytes());
    out[ETH_HEADER..n].copy_from_slice(d.body);
    n
}

/// An ethernet frame from a station, as the 802.11 data frame it sends its access point (9.3.2.1): non-QoS
/// data (frame control `08`), To DS (`01`), address 1 the BSSID (the receiver), address 2 the ethernet
/// source (this station), address 3 the ethernet destination; sequence `seq`; LLC/SNAP and the ethertype;
/// the payload. 0 when `eth` is shorter than its header or `out` cannot hold the frame.
pub fn to_80211(eth: &[u8], bssid: &[u8; 6], seq: u16, out: &mut [u8]) -> usize {
    if eth.len() < ETH_HEADER {
        return 0;
    }
    let body = &eth[ETH_HEADER..];
    let n = DATA_OVERHEAD + body.len();
    if n > out.len() {
        return 0;
    }
    out[0] = 0x08;
    out[1] = 0x01;
    out[2] = 0;
    out[3] = 0;
    out[4..10].copy_from_slice(bssid);
    out[10..16].copy_from_slice(&eth[6..12]);
    out[16..22].copy_from_slice(&eth[0..6]);
    out[22..24].copy_from_slice(&((seq & 0x0FFF) << 4).to_le_bytes());
    out[24..30].copy_from_slice(&LLC_SNAP);
    out[30..32].copy_from_slice(&eth[12..14]);
    out[32..n].copy_from_slice(body);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EAPOL frame in: a QoS data frame from the AP, LLC/SNAP, ethertype 888e, turned into ethernet
    /// with DA = address 1 and SA = address 3. And a frame the firmware decrypted, with the 8-byte CCMP
    /// header it leaves in place - decided by the header's `decr_status`, not the protected bit.
    #[test]
    fn eapol_in_a_data_frame() {
        let mut f = [0u8; 26 + 8 + 4];
        f[0] = 0x88; // QoS data
        f[1] = 0x02; // from DS
        f[4..10].copy_from_slice(&[1; 6]);
        f[10..16].copy_from_slice(&[7; 6]);
        f[16..22].copy_from_slice(&[9; 6]);
        f[26..34].copy_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
        f[34..38].copy_from_slice(&[2, 3, 0, 0x5f]);
        let Some(d) = llc_payload(&f, false) else {
            assert!(false, "an EAPOL data frame was not recognised");
            return;
        };
        assert_eq!((d.ethertype, d.da, d.sa), (0x888e, [1; 6], [9; 6]));
        assert_eq!(d.body, &[2, 3, 0, 0x5f]);
        let mut eth = [0u8; 32];
        assert_eq!(to_ethernet(&d, &mut eth), 18);
        assert_eq!(&eth[..14], &[1, 1, 1, 1, 1, 1, 9, 9, 9, 9, 9, 9, 0x88, 0x8e]);
        assert_eq!(to_ethernet(&d, &mut [0u8; 17]), 0);
        let mut g = [0u8; 26 + 8 + 8 + 4];
        g[0] = 0x88;
        g[1] = 0x42; // from DS, protected
        g[34..42].copy_from_slice(&[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
        assert_eq!(llc_payload(&g, true).map(|d| (d.ethertype, d.body.len())), Some((0x888e, 4)));
        assert!(llc_payload(&g, false).is_none()); // the IV read as LLC/SNAP is not LLC/SNAP
        assert!(llc_payload(&[0x80; 40], false).is_none()); // a beacon is not data
    }

    /// An ethernet frame out: to the BSSID, from the station, for the destination, with LLC/SNAP and the
    /// ethertype - and read back by `llc_payload` as the same ethernet frame from the other side.
    #[test]
    fn eapol_out_and_back() {
        let (us, ap) = ([2u8; 6], [9u8; 6]);
        let mut eth = [0u8; 18];
        eth[0..6].copy_from_slice(&ap);
        eth[6..12].copy_from_slice(&us);
        eth[12..14].copy_from_slice(&[0x88, 0x8e]);
        eth[14..18].copy_from_slice(&[1, 3, 0, 0x5f]);
        let mut f = [0u8; 64];
        let n = to_80211(&eth, &ap, 0x123, &mut f);
        assert_eq!(n, 32 + 4);
        assert_eq!((f[0], f[1]), (0x08, 0x01), "data, to DS");
        assert_eq!(&f[4..10], &ap, "to the access point");
        assert_eq!(&f[10..16], &us);
        assert_eq!(&f[16..22], &ap, "for the destination");
        assert_eq!(u16::from_le_bytes([f[22], f[23]]), 0x1230);
        assert_eq!(&f[24..32], &[0xaa, 0xaa, 3, 0, 0, 0, 0x88, 0x8e]);
        assert_eq!(&f[32..36], &[1, 3, 0, 0x5f]);
        // Read back as a frame from the station (not from DS), it is the ethernet frame it was made from.
        let d = llc_payload(&f[..n], false).map(|d| (d.ethertype, d.da, d.sa, d.body.len()));
        assert_eq!(d, Some((0x888e, ap, us, 4)));
        assert_eq!(to_80211(&eth[..13], &ap, 0, &mut f), 0, "too short to be ethernet");
        assert_eq!(to_80211(&eth, &ap, 0, &mut [0u8; 35]), 0, "no room");
    }
}
