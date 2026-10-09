// SPDX-License-Identifier: GPL-2.0-only
//! What the RTL8188CUS hands up its bulk IN endpoint: one transfer of one or more received packets, each a
//! 24-byte receive descriptor, the PHY status the chip appended, a shift, and the 802.11 frame - the next
//! packet starting on the next 128-byte boundary. This reads a transfer into those parts; what a frame MEANS
//! is the 802.11 layer's (`godspeed_wifi::mgmt`).
//!
//! Pure, and named nothing outside `core`, so `scripts/host_test_check.py` runs the tests at the bottom on
//! every build. Every rule here is `rtl8xxxu`'s (26.14), read from the source rather than summarised:
//! `struct rtl8xxxu_rxdesc16` and `struct rtl8723au_phy_stats` (`rtl8xxxu.h`), `rtl8xxxu_parse_rxdesc16`
//! (`core.c`), `rtl8723au_rx_parse_phystats` (`core.c`) and `rtl8723a_cck_rssi` (`8723a.c`, which the 8192C
//! family's `fops` names). R3b in `docs/wifi-usb.md`; `rx.rs` walks every transfer the host's bulk IN hands up.

/// `sizeof(struct rtl8xxxu_rxdesc16)`: six little-endian words.
pub const DESC_LEN: usize = 24;
/// Each packet in a transfer starts on this boundary: `roundup(..., 128)`.
const PACKET_ALIGN: usize = 128;
/// `DESC_RATE_6M`: below it the rate is CCK (1, 2, 5.5, 11 Mb/s), and the signal is read the CCK way.
const DESC_RATE_6M: u8 = 0x04;

/// The fields of a receive descriptor this driver reads.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct Desc {
    /// The 802.11 frame's length (`pktlen`, word 0 bits 0-13). NO FCS: this said "FCS included" until R6,
    /// and the chip's own receive configuration says otherwise - `RCR` appends the PHY status, the ICV and
    /// the MIC (bits 28-30, as Linux's `rtl8xxxu_init_device` sets them) and not the FCS (`RCR_APPEND_FCS`,
    /// bit 31). A frame the chip decrypted keeps its 8-byte CCMP header and ends with the 8-byte MIC.
    pub pkt_len: usize,
    /// The frame failed its CRC (`crc32`, bit 14) or its ICV (`icverr`, bit 15).
    pub crc_err: bool,
    pub icv_err: bool,
    /// The PHY status's length, in bytes (`drvinfo_sz`, bits 16-19, counted in 8-byte units).
    pub drvinfo: usize,
    /// The cipher the frame arrived under (`security`, bits 20-22): 0 none, 4 AES (CCMP) - Linux's
    /// `RX_DESC_ENC_*` - and whether it was left for software to decrypt (`swdec`, bit 27). The chip
    /// decrypted it when `security` is set and `swdec` is not (`rtl8xxxu_parse_rxdesc16`'s
    /// `RX_FLAG_DECRYPTED`).
    pub security: u8,
    pub swdec: bool,
    /// Padding between the PHY status and the frame (`shift`, bits 24-25).
    pub shift: usize,
    /// A PHY status is present (`phy_stats`, bit 26).
    pub phy_stats: bool,
    /// How many packets this transfer holds - read from the FIRST descriptor only (`pkt_cnt`, word 2 bits 16-23).
    pub pkt_cnt: u8,
    /// The rate (`rxmcs`, word 3 bits 0-5) and whether it was an HT rate (`rxht`, bit 6).
    pub rxmcs: u8,
    pub rxht: bool,
    /// A firmware report rather than a frame (`rpt_sel`, word 3 bits 14-15; the 8188E's, zero on this chip).
    pub rpt_sel: u8,
    /// The low 32 bits of the TSF when it arrived (word 5).
    pub tsfl: u32,
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

/// Read the descriptor at the start of `b`. `None` for fewer than 24 bytes.
pub fn desc(b: &[u8]) -> Option<Desc> {
    if b.len() < DESC_LEN {
        return None;
    }
    let (w0, w2, w3) = (word(b, 0), word(b, 2), word(b, 3));
    Some(Desc {
        pkt_len: (w0 & 0x3FFF) as usize,
        crc_err: w0 & (1 << 14) != 0,
        icv_err: w0 & (1 << 15) != 0,
        drvinfo: ((w0 >> 16) & 0xF) as usize * 8,
        security: ((w0 >> 20) & 0x7) as u8,
        swdec: w0 & (1 << 27) != 0,
        shift: ((w0 >> 24) & 0x3) as usize,
        phy_stats: w0 & (1 << 26) != 0,
        pkt_cnt: ((w2 >> 16) & 0xFF) as u8,
        rxmcs: (w3 & 0x3F) as u8,
        rxht: w3 & (1 << 6) != 0,
        rpt_sel: ((w3 >> 14) & 0x3) as u8,
        tsfl: word(b, 5),
    })
}

/// One packet out of a transfer.
pub struct Packet<'a> {
    pub desc: Desc,
    /// The PHY status, when the descriptor says there is one and it arrived whole.
    pub phy: Option<&'a [u8]>,
    /// The 802.11 frame, cut to `pkt_len` (no FCS: RCR does not append it) - or to what arrived, if the transfer ended first
    /// (`truncated` then says so; such a frame is not to be trusted past its header).
    pub frame: &'a [u8],
    pub truncated: bool,
}

/// Walk a bulk IN transfer, calling `f` for each packet in it, and return how many were read. A firmware
/// report (`rpt_sel`) is passed up like a frame; the caller decides. The walk stops at the count the first
/// descriptor gave, or when what is left cannot hold a descriptor - `rtl8xxxu`'s two conditions.
pub fn walk<'a>(mut t: &'a [u8], f: &mut dyn FnMut(Packet<'a>)) -> usize {
    let mut read = 0usize;
    let mut left: Option<u8> = None;
    while let Some(d) = desc(t) {
        // `pkt_cnt` is read from the first descriptor only; a chip with aggregation off reports 0 or 1.
        let cnt = *left.get_or_insert(d.pkt_cnt.max(1));
        let body = &t[DESC_LEN..];
        let phy = if d.phy_stats && d.drvinfo > 0 && body.len() >= d.drvinfo { Some(&body[..d.drvinfo]) } else { None };
        let start = (d.drvinfo + d.shift).min(body.len());
        let end = (start + d.pkt_len).min(body.len());
        f(Packet { desc: d, phy, frame: &body[start..end], truncated: end - start < d.pkt_len });
        read += 1;
        let next = round_up(DESC_LEN + d.drvinfo + d.shift + d.pkt_len, PACKET_ALIGN);
        if cnt <= 1 || next >= t.len() {
            break;
        }
        left = Some(cnt - 1);
        t = &t[next..];
    }
    read
}

fn round_up(n: usize, to: usize) -> usize {
    (n + to - 1) / to * to
}

/// The signal strength in dBm, from a PHY status: for a CCK rate, the CCK AGC report (the LNA index in the
/// top two bits picking the offset, `rtl8723a_cck_rssi`); for an OFDM rate, the all-paths power, `pwdb / 2 -
/// 110`. `None` without a PHY status, or one too short to hold the byte it needs.
pub fn rssi(phy: Option<&[u8]>, rxmcs: u8) -> Option<i16> {
    let p = phy?;
    // `path_agc[2]`, `ch_corr[2]`, then `cck_sig_qual_ofdm_pwdb_all` (4) and `cck_agc_rpt_ofdm_cfosho_a` (5).
    if rxmcs < DESC_RATE_6M {
        let agc = *p.get(5)?;
        let gain = (agc & 0x3E) as i16;
        Some(match agc & 0xC0 {
            0xC0 => -46 - gain,
            0x80 => -26 - gain,
            0x40 => -12 - gain,
            _ => 16 - gain,
        })
    } else {
        Some((*p.get(4)? >> 1) as i16 - 110)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a descriptor for a `len`-byte frame with an `info`-unit PHY status and `shift`.
    fn put_desc(b: &mut [u8], len: usize, info: u32, shift: u32, cnt: u32, mcs: u32) {
        let w0 = len as u32 | (info << 16) | (shift << 24) | (if info > 0 { 1 << 26 } else { 0 }) | (1 << 28) | (1 << 29);
        b[0..4].copy_from_slice(&w0.to_le_bytes());
        b[8..12].copy_from_slice(&(cnt << 16).to_le_bytes());
        b[12..16].copy_from_slice(&mcs.to_le_bytes());
        b[20..24].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    }

    #[test]
    fn a_descriptor_read_field_by_field() {
        let mut b = [0u8; 24];
        put_desc(&mut b, 0x123, 4, 2, 3, 0x0B);
        b[1] |= 0x40; // crc32
        let Some(d) = desc(&b) else { return assert!(false, "a whole descriptor read as none") };
        assert_eq!(d.pkt_len, 0x123);
        assert!(d.crc_err && !d.icv_err);
        assert_eq!((d.drvinfo, d.shift, d.phy_stats), (32, 2, true));
        assert_eq!((d.pkt_cnt, d.rxmcs, d.rxht, d.rpt_sel), (3, 0x0B, false, 0));
        assert_eq!(d.tsfl, 0x1234_5678);
        assert!(desc(&b[..23]).is_none());
    }

    #[test]
    fn one_packet_its_phy_status_and_frame() {
        let mut t = [0u8; 200];
        put_desc(&mut t, 40, 4, 0, 1, 0);
        t[24 + 5] = 0x84; // CCK AGC report: LNA 0x80, gain 4
        t[24 + 32] = 0x80; // the frame's first byte
        let mut got = 0;
        let n = walk(&t, &mut |p| {
            got += 1;
            assert_eq!(p.phy.map(|s| s.len()), Some(32));
            assert_eq!(p.frame.len(), 40);
            assert_eq!(p.frame[0], 0x80);
            assert!(!p.truncated);
            assert_eq!(rssi(p.phy, p.desc.rxmcs), Some(-30));
        });
        assert_eq!((n, got), (1, 1));
    }

    #[test]
    fn an_aggregate_is_walked_on_128_byte_boundaries() {
        // Two packets: 24 + 32 + 1 + 100 = 157 rounds to 256 for the second.
        let mut t = [0u8; 512];
        put_desc(&mut t, 100, 4, 1, 2, 0x0C);
        t[24 + 4] = 120; // OFDM pwdb: 120 / 2 - 110 = -50
        put_desc(&mut t[256..], 30, 0, 0, 0, 0);
        t[256 + 24] = 0x50;
        let mut lens = [0usize; 2];
        let mut i = 0;
        let n = walk(&t, &mut |p| {
            lens[i] = p.frame.len();
            if i == 0 {
                assert_eq!(rssi(p.phy, p.desc.rxmcs), Some(-50));
            } else {
                assert_eq!(p.frame[0], 0x50);
                assert!(p.phy.is_none());
                assert_eq!(rssi(p.phy, p.desc.rxmcs), None);
            }
            i += 1;
        });
        assert_eq!(n, 2);
        assert_eq!(lens, [100, 30]);
    }

    #[test]
    fn the_count_and_the_end_both_stop_the_walk() {
        // pkt_cnt 1 with a second descriptor present: one packet, as rtl8xxxu reads it.
        let mut t = [0u8; 512];
        put_desc(&mut t, 10, 0, 0, 1, 0);
        put_desc(&mut t[128..], 10, 0, 0, 0, 0);
        assert_eq!(walk(&t, &mut |_| {}), 1);
        // pkt_cnt 5 and a transfer that ends after two: two.
        put_desc(&mut t, 10, 0, 0, 5, 0);
        assert_eq!(walk(&t[..128 + 30], &mut |_| {}), 2);
        // A frame cut short by the end of the transfer is passed up, marked.
        let mut short = [0u8; 40];
        put_desc(&mut short, 100, 0, 0, 1, 0);
        walk(&short, &mut |p| {
            assert!(p.truncated);
            assert_eq!(p.frame.len(), 16);
        });
        assert_eq!(walk(&[0u8; 10], &mut |_| {}), 0);
    }

    #[test]
    fn rtl8723a_cck_rssis_four_cases() {
        let phy = |agc: u8| [0, 0, 0, 0, 0, agc];
        assert_eq!(rssi(Some(&phy(0xC0 | 0x3E)), 0), Some(-46 - 62));
        assert_eq!(rssi(Some(&phy(0x80 | 0x02)), 1), Some(-28));
        assert_eq!(rssi(Some(&phy(0x40)), 2), Some(-12));
        assert_eq!(rssi(Some(&phy(0x3F)), 3), Some(16 - 62)); // bit 0 is not part of the gain
        assert_eq!(rssi(Some(&[0u8; 5]), 0), None);
        assert_eq!(rssi(Some(&[0, 0, 0, 0, 0]), 4), Some(-110));
    }
}
