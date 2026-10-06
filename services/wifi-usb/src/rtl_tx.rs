// SPDX-License-Identifier: GPL-2.0-only
//! The RTL8188CUS's transmit descriptor: the 32 bytes in front of every frame sent to the chip (R5a,
//! `docs/wifi-usb.md` 11).
//!
//! Read from Linux's `rtl8xxxu` (`core.c`, fetched 2026-10-06): `rtl8xxxu_tx` builds the common words and
//! `rtl8xxxu_fill_txdesc_v1` - the "gen1" format the 8188CU, 8192CU and 8723AU use - the chip-specific
//! ones, then `rtl8xxxu_calc_tx_desc_csum` signs the first 32 bytes. Only what a MANAGEMENT frame takes is
//! built here: a data frame's QoS, aggregation and protection bits are R6's (26.14: the silicon's layout,
//! taken whole; which frames this driver sends is ours).
//!
//! Pure, and naming nothing outside `core`, so `scripts/host_test_check.py` runs its tests on every build.

/// `sizeof(struct rtl8xxxu_txdesc32)`, and the `pkt_offset` every frame is given: the frame starts there.
pub const TX_DESC_LEN: usize = 32;

/// `txdw0` (a byte, at offset 3): `TXDESC_OWN | TXDESC_FIRST_SEGMENT | TXDESC_LAST_SEGMENT`, which
/// `rtl8xxxu_tx` sets for every chip but the 8192F, and `TXDESC_BROADMULTICAST` for a group address.
const DW0_OWN: u8 = 1 << 7;
const DW0_FIRST_SEGMENT: u8 = 1 << 3;
const DW0_LAST_SEGMENT: u8 = 1 << 2;
const DW0_BROADMULTICAST: u8 = 1 << 0;
/// `txdw1`: the queue in bits 12:8 (`TXDESC_QUEUE_SHIFT`), and `TXDESC32_AGG_BREAK`, which
/// `fill_txdesc_v1` sets whenever aggregation is not on - for every management frame.
const DW1_QUEUE_SHIFT: u32 = 8;
const DW1_AGG_BREAK: u32 = 1 << 6;
/// `TXDESC_QUEUE_MGNT`: `rtl8xxxu_queue_select`'s queue for a management frame that is not a beacon.
pub const QUEUE_MGNT: u32 = 0x12;
/// `txdw3`: the sequence number in bits 27:16 (`TXDESC32_SEQ_SHIFT`).
const DW3_SEQ_SHIFT: u32 = 16;
/// `txdw4`: `TXDESC32_USE_DRIVER_RATE` - a management frame goes at the rate in `txdw5`, not the chip's choice.
const DW4_USE_DRIVER_RATE: u32 = 1 << 8;
/// `txdw5`: the rate in the low bits, and for a management frame a retry limit of 6
/// (`6 << TXDESC32_RETRY_LIMIT_SHIFT`, with `TXDESC32_RETRY_LIMIT_ENABLE`).
const DW5_RETRY_LIMIT_SHIFT: u32 = 18;
const DW5_RETRY_LIMIT_ENABLE: u32 = 1 << 17;
const MGMT_RETRY_LIMIT: u32 = 6;
/// `DESC_RATE_1M`: 1 Mb/s DSSS, the rate every 2.4 GHz station can hear. Linux passes `rate = 0` from
/// `fill_txdesc_v1` for a management frame, which is this.
pub const RATE_1M: u32 = 0x00;

/// The bulk OUT endpoint a management frame is sent on, as a POSITION among the radio's OUT endpoints
/// (`usbfn::OP_BULK_OUT`'s `out`). `rtl8xxxu_init_queue_priority` makes it `out_ep[mgp]`, and `mgp` is 0 for
/// one endpoint, 0 for two, and `TRXDMA_QUEUE_HIGH ^ 3` = 0 for three: always the first.
pub const MGNT_OUT: u8 = 0;

/// `TXDESC_QUEUE_BE`: best effort, where a data frame goes when the association has no QoS - this station
/// offers no WMM element, so every data frame it sends is best effort.
pub const QUEUE_BE: u32 = 0x0;

/// The bulk OUT endpoint, as a POSITION, that best-effort data is sent on: `out_ep[bep]` in
/// `rtl8xxxu_init_queue_priority`, where `bep` is 0 for one endpoint, 1 for two (`bkp = bep = 1`), and
/// `TRXDMA_QUEUE_LOW ^ 3` = 2 for three. `queues` is how many transmit queues the dongle's endpoints serve.
pub fn be_out(queues: u8) -> u8 {
    match queues {
        2 => 1,
        3 => 2,
        _ => 0,
    }
}

/// The descriptor for a management frame of `frame_len` bytes, sequence number `seq`, to a group address
/// when `group` (a probe request's broadcast) - signed. Little-endian throughout, as the struct is `__le`.
pub fn mgmt(frame_len: u16, seq: u16, group: bool) -> [u8; TX_DESC_LEN] {
    driver_rate(frame_len, seq, group, QUEUE_MGNT)
}

/// `TXDESC_SEC_AES` in `txdw1`: the chip encrypts the frame with CCMP, using the key the CAM holds for its
/// receiver (`rtl8xxxu_tx` sets it when mac80211 hands the frame a hardware key).
const DW1_SEC_AES: u32 = 0x00C0_0000;

/// `txdw5`'s data bits for a frame the FIRMWARE picks the rate of: `fill_txdesc_v1` ORs `0x0001ff00` in for
/// every data frame, and sets no driver rate - the rate is the firmware's, adapting within the mask the
/// driver gave it after the association (`rtl8188::rate_mask`, R8).
const DW5_DATA: u32 = 0x0001_FF00;

/// The descriptor for a unicast DATA frame - the link's traffic (R6) - on the best-effort queue, protected
/// for the chip to encrypt (`DW1_SEC_AES`) when `protected`, signed. Since R8 the rate is the firmware's
/// choice, as `rtl8xxxu_fill_txdesc_v1` leaves it for a data frame; R6 sent these at the driver's 1 Mb/s.
pub fn data(frame_len: u16, seq: u16, protected: bool) -> [u8; TX_DESC_LEN] {
    let mut d = [0u8; TX_DESC_LEN];
    d[0..2].copy_from_slice(&frame_len.to_le_bytes());
    d[2] = TX_DESC_LEN as u8;
    d[3] = DW0_OWN | DW0_FIRST_SEGMENT | DW0_LAST_SEGMENT;
    let dw1 = (QUEUE_BE << DW1_QUEUE_SHIFT) | DW1_AGG_BREAK | if protected { DW1_SEC_AES } else { 0 };
    d[4..8].copy_from_slice(&dw1.to_le_bytes());
    let dw3 = ((seq as u32) & 0x0FFF) << DW3_SEQ_SHIFT;
    d[12..16].copy_from_slice(&dw3.to_le_bytes());
    d[20..24].copy_from_slice(&DW5_DATA.to_le_bytes());
    sign(&mut d);
    d
}

/// The rates `rtl8xxxu`'s table lists, in its order (`rtl8xxxu_legacy_ratetable`: 1, 2, 5.5, 11, then 6 to
/// 54 Mb/s), in the 500 kb/s units a rate element carries. A rate's position is its bit in the mask.
const LEGACY_RATES: [u8; 12] = [2, 4, 11, 22, 12, 18, 24, 36, 48, 72, 96, 108];

/// The rate mask for the firmware (`update_rate_mask`'s `ramask`, the legacy part of
/// `sta->deflink.supp_rates[0]`): a bit for each rate the access point lists in its Supported Rates (1) and
/// Extended Supported Rates (50) elements. Basic-rate flags (bit 7) are ignored; a rate this chip does not
/// list is ignored; an element walk that would run past the end stops - the lengths are from the air.
pub fn rate_mask(ies: &[u8]) -> u32 {
    let mut mask = 0u32;
    let mut at = 0usize;
    while at + 2 <= ies.len() {
        let (id, len) = (ies[at], ies[at + 1] as usize);
        let end = at + 2 + len;
        if end > ies.len() {
            break;
        }
        if id == 1 || id == 50 {
            for &r in &ies[at + 2..end] {
                if let Some(i) = LEGACY_RATES.iter().position(|&x| x == r & 0x7F) {
                    mask |= 1 << i;
                }
            }
        }
        at = end;
    }
    mask
}

/// The descriptor for an unprotected unicast DATA frame - an EAPOL frame of the four-way handshake (R5c) -
/// on the best-effort queue, signed.
///
/// **A deliberate difference from Linux, recorded (26.14).** `fill_txdesc_v1` sends a data frame at the
/// rate the chip's firmware chooses, from the rate mask `rtl8xxxu_bss_info_changed` hands it once the
/// association is up (`update_rate_mask`). This driver hands it over after the handshake instead (R8,
/// `rtl8188::joined`), so the handshake's frames go at the driver's rate - 1 Mb/s with a retry limit of 6,
/// as the management frames do - the one rate every access point takes. Four small frames; the link's
/// traffic after them is `data`'s.
pub fn eapol(frame_len: u16, seq: u16) -> [u8; TX_DESC_LEN] {
    driver_rate(frame_len, seq, false, QUEUE_BE)
}

/// A descriptor whose frame goes at the driver's rate, 1 Mb/s, retried up to 6 times, on `queue`.
fn driver_rate(frame_len: u16, seq: u16, group: bool, queue: u32) -> [u8; TX_DESC_LEN] {
    let mut d = [0u8; TX_DESC_LEN];
    d[0..2].copy_from_slice(&frame_len.to_le_bytes());
    d[2] = TX_DESC_LEN as u8;
    d[3] = DW0_OWN | DW0_FIRST_SEGMENT | DW0_LAST_SEGMENT | if group { DW0_BROADMULTICAST } else { 0 };
    let dw1 = (queue << DW1_QUEUE_SHIFT) | DW1_AGG_BREAK;
    d[4..8].copy_from_slice(&dw1.to_le_bytes());
    let dw3 = ((seq as u32) & 0x0FFF) << DW3_SEQ_SHIFT;
    d[12..16].copy_from_slice(&dw3.to_le_bytes());
    d[16..20].copy_from_slice(&DW4_USE_DRIVER_RATE.to_le_bytes());
    let dw5 = RATE_1M | (MGMT_RETRY_LIMIT << DW5_RETRY_LIMIT_SHIFT) | DW5_RETRY_LIMIT_ENABLE;
    d[20..24].copy_from_slice(&dw5.to_le_bytes());
    sign(&mut d);
    d
}

/// `rtl8xxxu_calc_tx_desc_csum`: the XOR of the descriptor's sixteen little-endian 16-bit words, taken with
/// the checksum field (offset 28) zeroed, written into it.
pub fn sign(d: &mut [u8; TX_DESC_LEN]) {
    d[28] = 0;
    d[29] = 0;
    let mut c: u16 = 0;
    for w in d.chunks_exact(2) {
        c ^= u16::from_le_bytes([w[0], w[1]]);
    }
    d[28..30].copy_from_slice(&c.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(d: &[u8; TX_DESC_LEN], at: usize) -> u32 {
        u32::from_le_bytes([d[at], d[at + 1], d[at + 2], d[at + 3]])
    }

    #[test]
    fn a_broadcast_management_descriptor_has_linuxs_words() {
        let d = mgmt(60, 0x123, true);
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 60);
        assert_eq!(d[2], 32);
        assert_eq!(d[3], 0x8D, "OWN | FIRST | LAST | BROADMULTICAST");
        assert_eq!(word(&d, 4), 0x1240, "queue MGNT (0x12) at bit 8, and AGG_BREAK");
        assert_eq!(word(&d, 8), 0);
        assert_eq!(word(&d, 12), 0x0123_0000, "the sequence number at bit 16");
        assert_eq!(word(&d, 16), 0x100, "USE_DRIVER_RATE");
        assert_eq!(word(&d, 20), 0x001A_0000, "1 Mb/s, retry limit 6, enabled");
        assert_eq!(word(&d, 24), 0);
    }

    #[test]
    fn a_unicast_one_has_no_group_bit() {
        assert_eq!(mgmt(60, 0, false)[3], 0x8C);
    }

    #[test]
    fn the_signed_descriptor_xors_to_zero() {
        // The checksum is the XOR of the other fifteen words, so all sixteen XOR to zero - for any input.
        for (len, seq, group) in [(60u16, 0x123u16, true), (1500, 0xFFF, false), (0, 0, false)] {
            let d = mgmt(len, seq, group);
            let x = d.chunks_exact(2).fold(0u16, |a, w| a ^ u16::from_le_bytes([w[0], w[1]]));
            assert_eq!(x, 0);
        }
    }

    #[test]
    fn an_eapol_descriptor_is_best_effort_and_unicast() {
        let d = eapol(99, 7);
        assert_eq!(d[3], 0x8C, "OWN | FIRST | LAST, no group bit");
        assert_eq!(word(&d, 4), 0x0040, "queue BE (0) and AGG_BREAK");
        assert_eq!(word(&d, 16), 0x100, "the driver's rate");
        assert_eq!(word(&d, 20), 0x001A_0000, "1 Mb/s, retry limit 6");
    }

    #[test]
    fn a_data_descriptor_leaves_the_rate_to_the_firmware() {
        let d = data(120, 3, false);
        assert_eq!(word(&d, 16), 0, "no driver rate");
        assert_eq!(word(&d, 20), 0x0001_FF00, "fill_txdesc_v1's data bits");
        assert_eq!(word(&d, 4), 0x0040, "queue BE, AGG_BREAK, not protected");
    }

    #[test]
    fn the_rate_mask_reads_both_rate_elements() {
        // 1, 2, 5.5, 11 (basic) and 6, 9, 12, 18 in Supported Rates; 24, 36, 48, 54 extended - all twelve.
        let ies = [0, 0, 1, 8, 0x82, 0x84, 0x8B, 0x96, 0x0C, 0x12, 0x18, 0x24, 3, 1, 1, 50, 4, 0x30, 0x48, 0x60, 0x6C];
        assert_eq!(rate_mask(&ies), 0xFFF);
        // A g-only network (no CCK): the four low bits clear.
        let g = [1, 8, 0x8C, 0x12, 0x98, 0x24, 0xB0, 0x48, 0x60, 0x6C];
        assert_eq!(rate_mask(&g), 0xFF0);
        assert_eq!(rate_mask(&[1, 9, 2]), 0, "an element longer than what is left stops the walk");
    }

    #[test]
    fn a_protected_descriptor_asks_the_chip_for_ccmp() {
        let d = data(120, 3, true);
        assert_eq!(word(&d, 4), 0x00C0_0040, "SEC_AES, queue BE, AGG_BREAK");
        let x = d.chunks_exact(2).fold(0u16, |a, w| a ^ u16::from_le_bytes([w[0], w[1]]));
        assert_eq!(x, 0, "signed after the security bits went in");
    }

    #[test]
    fn best_effort_takes_the_endpoint_linux_maps_it_to() {
        assert_eq!((be_out(1), be_out(2), be_out(3)), (0, 1, 2));
    }

    #[test]
    fn the_sequence_number_is_twelve_bits() {
        assert_eq!(word(&mgmt(60, 0x1FFF, true), 12), 0x0FFF_0000);
    }
}
