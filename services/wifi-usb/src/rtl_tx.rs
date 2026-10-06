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

/// The descriptor for a management frame of `frame_len` bytes, sequence number `seq`, to a group address
/// when `group` (a probe request's broadcast) - signed. Little-endian throughout, as the struct is `__le`.
pub fn mgmt(frame_len: u16, seq: u16, group: bool) -> [u8; TX_DESC_LEN] {
    let mut d = [0u8; TX_DESC_LEN];
    d[0..2].copy_from_slice(&frame_len.to_le_bytes());
    d[2] = TX_DESC_LEN as u8;
    d[3] = DW0_OWN | DW0_FIRST_SEGMENT | DW0_LAST_SEGMENT | if group { DW0_BROADMULTICAST } else { 0 };
    let dw1 = (QUEUE_MGNT << DW1_QUEUE_SHIFT) | DW1_AGG_BREAK;
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
    fn the_sequence_number_is_twelve_bits() {
        assert_eq!(word(&mgmt(60, 0x1FFF, true), 12), 0x0FFF_0000);
    }
}
