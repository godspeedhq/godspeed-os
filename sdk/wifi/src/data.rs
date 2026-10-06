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
/// ethertype - and the 8-byte CCMP header too when the frame is protected (`CCMP_HEADER`). An ethernet frame
/// of `n` bytes becomes `n - ETH_HEADER + DATA_OVERHEAD` (+ `CCMP_HEADER`).
pub const DATA_OVERHEAD: usize = 24 + 8;
/// The CCMP header (IEEE 802.11-2020 12.5.3.2): the packet number's two low bytes, a reserved byte, the key
/// id byte with the Ext IV bit, and the packet number's four high bytes. The MIC that ends a CCMP frame is
/// not written here: on a radio that encrypts in hardware the radio appends it, as mac80211 leaves it to
/// (`ccmp_encrypt_skb` adds no tail when the key is in hardware).
pub const CCMP_HEADER: usize = 8;
/// The QoS Control field a QoS data frame adds after the 24-byte header (9.2.4.5): the TID in its low four
/// bits, normal acknowledgement, no A-MSDU.
pub const QOS_CONTROL: usize = 2;
/// Where a receiver keeps the replay counter of a frame with no TID (a non-QoS one): after the sixteen
/// TIDs, as mac80211 does (`IEEE80211_NUM_TIDS`).
pub const NON_QOS: usize = 16;
/// Replay counters per key: one per TID, and one for non-QoS frames.
pub const REPLAY_SLOTS: usize = 17;

/// The replay slot a received data frame counts under: its TID for a QoS data frame, `NON_QOS` otherwise
/// (`ieee80211_crypto_ccmp_decrypt` keys the packet number on `rx->security_idx` the same way). `None` for
/// a frame too short to carry the QoS Control field it says it has.
pub fn replay_slot(frame: &[u8]) -> Option<usize> {
    if frame.len() < 24 || frame[0] & 0x0c != 0x08 {
        return None;
    }
    if frame[0] & 0x80 == 0 {
        return Some(NON_QOS);
    }
    frame.get(24).map(|q| (q & 0x0F) as usize)
}

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

/// The packet number and key id in a protected data frame's CCMP header - what a receiver checks for
/// replay. A radio that decrypts in hardware does not check it for the host: `rtl8xxxu` never marks a frame
/// `RX_FLAG_PN_VALIDATED`, so mac80211 checks it in software (`ieee80211_crypto_ccmp_decrypt`), and a host
/// that hands such frames up must do the same. `None` when `frame` is not a protected data frame long enough
/// to carry the header. The header is where `llc_payload` steps over it: after 24 bytes, 26 with QoS, 4
/// more with the order bit; its bytes 0-1 and 4-7 are the packet number, low first, and byte 3's top two
/// bits the key id (`ccmp_hdr2pn`).
pub fn ccmp_pn(frame: &[u8]) -> Option<(u64, u8)> {
    if frame.len() < 24 || frame[0] & 0x0c != 0x08 || frame[1] & 0x40 == 0 {
        return None;
    }
    let at = 24 + if frame[0] & 0x80 != 0 { 2 } else { 0 } + if frame[1] & 0x80 != 0 { 4 } else { 0 };
    let h = frame.get(at..at + CCMP_HEADER)?;
    let pn = u64::from_le_bytes([h[0], h[1], h[4], h[5], h[6], h[7], 0, 0]);
    Some((pn, h[3] >> 6))
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

/// An ethernet frame from a station, as the 802.11 data frame it sends its access point (9.3.2.1): data
/// (frame control `08`), To DS (`01`), address 1 the BSSID (the receiver), address 2 the ethernet source
/// (this station), address 3 the ethernet destination; sequence `seq`; LLC/SNAP and the ethertype; the
/// payload. With `qos` - a TID - it is a QoS data frame (`88`) with the QoS Control field after the header
/// (R12a, a WMM association). With `ccmp` - `(packet number, key id)` - the Protected bit is set and the
/// CCMP header goes after the MAC header, where mac80211's `ccmp_pn2hdr` puts it for a key the hardware
/// encrypts with; the hardware does the rest. 0 when `eth` is shorter than its header or `out` cannot hold
/// the frame.
pub fn to_80211(eth: &[u8], bssid: &[u8; 6], seq: u16, qos: Option<u8>, ccmp: Option<(u64, u8)>, out: &mut [u8]) -> usize {
    if eth.len() < ETH_HEADER {
        return 0;
    }
    let body = &eth[ETH_HEADER..];
    let qc = if qos.is_some() { QOS_CONTROL } else { 0 };
    let iv = if ccmp.is_some() { CCMP_HEADER } else { 0 };
    let n = DATA_OVERHEAD + qc + iv + body.len();
    if n > out.len() {
        return 0;
    }
    out[0] = if qos.is_some() { 0x88 } else { 0x08 };
    out[1] = 0x01 | if ccmp.is_some() { 0x40 } else { 0 };
    out[2] = 0;
    out[3] = 0;
    out[4..10].copy_from_slice(bssid);
    out[10..16].copy_from_slice(&eth[6..12]);
    out[16..22].copy_from_slice(&eth[0..6]);
    out[22..24].copy_from_slice(&((seq & 0x0FFF) << 4).to_le_bytes());
    if let Some(tid) = qos {
        out[24] = tid & 0x0F;
        out[25] = 0;
    }
    let h = 24 + qc;
    if let Some((pn, key_id)) = ccmp {
        let p = pn.to_le_bytes();
        out[h..h + 8].copy_from_slice(&[p[0], p[1], 0, 0x20 | (key_id & 0x3) << 6, p[2], p[3], p[4], p[5]]);
    }
    let at = h + iv;
    out[at..at + 6].copy_from_slice(&LLC_SNAP);
    out[at + 6..at + 8].copy_from_slice(&eth[12..14]);
    out[at + 8..n].copy_from_slice(body);
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
        let n = to_80211(&eth, &ap, 0x123, None, None, &mut f);
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
        assert_eq!(to_80211(&eth[..13], &ap, 0, None, None, &mut f), 0, "too short to be ethernet");
        assert_eq!(to_80211(&eth, &ap, 0, None, None, &mut [0u8; 35]), 0, "no room");
        assert_eq!(replay_slot(&f[..n]), Some(NON_QOS));
    }

    /// A protected frame: the Protected bit, and the CCMP header where `ccmp_pn2hdr` writes it - the packet
    /// number's low two bytes, zero, the Ext IV bit with the key id, then the high four - read back by
    /// `llc_payload` the way a frame the radio decrypted is read.
    #[test]
    fn a_protected_frame_carries_the_ccmp_header() {
        let mut eth = [0u8; 18];
        eth[12..14].copy_from_slice(&[0x08, 0x00]);
        eth[14..18].copy_from_slice(&[0x45, 0, 0, 4]);
        let mut f = [0u8; 64];
        let n = to_80211(&eth, &[9; 6], 0, None, Some((0x0000_0605_0403_0201, 1)), &mut f);
        assert_eq!(n, 32 + 8 + 4);
        assert_eq!(f[1], 0x41, "to DS, protected");
        assert_eq!(&f[24..32], &[0x01, 0x02, 0, 0x60, 0x03, 0x04, 0x05, 0x06]);
        let d = llc_payload(&f[..n], true).map(|d| (d.ethertype, d.body.len()));
        assert_eq!(d, Some((0x0800, 4)));
        // And the packet number and key id read back out of it, as a receiver checks them for replay.
        assert_eq!(ccmp_pn(&f[..n]), Some((0x0605_0403_0201, 1)));
        assert_eq!(ccmp_pn(&f[..23]), None, "too short");
        f[1] = 0x01;
        assert_eq!(ccmp_pn(&f[..n]), None, "not protected");
    }

    /// A QoS data frame (R12a): subtype QoS, the QoS Control field with the TID after the header, the CCMP
    /// header after that - and the receive side reads all three back where they are.
    #[test]
    fn a_qos_frame_carries_its_tid() {
        let mut eth = [0u8; 18];
        eth[12..14].copy_from_slice(&[0x08, 0x00]);
        eth[14..18].copy_from_slice(&[0x45, 0, 0, 4]);
        let mut f = [0u8; 64];
        let n = to_80211(&eth, &[9; 6], 0, Some(5), Some((0x0201, 0)), &mut f);
        assert_eq!(n, 32 + 2 + 8 + 4);
        assert_eq!((f[0], f[24], f[25]), (0x88, 5, 0), "QoS data, TID 5");
        assert_eq!(&f[26..28], &[0x01, 0x02], "the CCMP header after the QoS Control field");
        assert_eq!(replay_slot(&f[..n]), Some(5));
        assert_eq!(ccmp_pn(&f[..n]), Some((0x0201, 0)));
        assert_eq!(llc_payload(&f[..n], true).map(|d| d.ethertype), Some(0x0800));
        assert_eq!(replay_slot(&f[..24]), None, "QoS, but too short for the field");
    }
}
