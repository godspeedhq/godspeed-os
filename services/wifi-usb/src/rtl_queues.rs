// SPDX-License-Identifier: GPL-2.0-only
//! The RTL8192CU family's transmit queues, as values: which queues the dongle's bulk OUT endpoints serve,
//! how the chip's 0xF8 pages are reserved between them, and the queue priority word - set after the
//! power-on and BEFORE the firmware download, as both Linux drivers do it (`rtl8xxxu_init_device`,
//! `rtlwifi`'s `_rtl92cu_init_mac`; `docs/wifi-usb.md` section 6).
//!
//! Pure, and named nothing outside `core`, so `scripts/host_test_check.py` runs the tests at the bottom on
//! every build. Every rule here is `rtl8xxxu`'s (26.14): `rtl8xxxu_config_endpoints_sie`,
//! `rtl8xxxu_config_endpoints_no_sie`, `rtl8xxxu_init_queue_reserved_page`, `rtl8xxxu_init_queue_priority`.

/// The dongle's transmit queues, one per bulk OUT endpoint.
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct TxQueues {
    pub high: bool,
    pub normal: bool,
    pub low: bool,
}

impl TxQueues {
    pub fn count(&self) -> u8 {
        self.high as u8 + self.normal as u8 + self.low as u8
    }
}

/// From `REG_NORMAL_SIE_EP_TX` (0xFE66): a nibble per queue, non-zero where an endpoint serves it.
pub fn from_sie(v: u16) -> TxQueues {
    TxQueues { high: v & 0x000F != 0, normal: v & 0x00F0 != 0, low: v & 0x0F00 != 0 }
}

/// The fallback for a dongle that reports nothing there: from the count of bulk OUT endpoints - one is
/// high; two add normal; three or more add low. `None` for zero, which is not a radio this driver can send on.
pub fn from_out_endpoints(n: u8) -> Option<TxQueues> {
    match n {
        0 => None,
        1 => Some(TxQueues { high: true, normal: false, low: false }),
        2 => Some(TxQueues { high: true, normal: true, low: false }),
        _ => Some(TxQueues { high: true, normal: true, low: true }),
    }
}

/// How many bulk OUT endpoints a configuration descriptor (the whole of it, from `GET_DESCRIPTOR`) declares.
pub fn out_endpoints(cfg: &[u8]) -> u8 {
    let mut n = 0u8;
    let mut i = 0usize;
    while i + 2 <= cfg.len() {
        let len = cfg[i] as usize;
        if len < 2 || i + len > cfg.len() {
            break;
        }
        // An endpoint descriptor (type 5): direction OUT (bit 7 of the address clear), transfer type bulk (2).
        if cfg[i + 1] == 0x05 && len >= 7 && cfg[i + 2] & 0x80 == 0 && cfg[i + 3] & 0x03 == 0x02 {
            n += 1;
        }
        i += len;
    }
    n
}

/// The chip's page budget for this family (`TX_TOTAL_PAGE_NUM`, `TX_PAGE_NUM_*_PQ`).
const TOTAL_PAGES: u32 = 0xF8;
const PAGES_HIGH: u32 = 0x0C;
const PAGES_LOW: u32 = 0x02;
const PAGES_NORMAL: u32 = 0x02;

/// `REG_RQPN_NPQ` (0x214) and `REG_RQPN` (0x200): the normal queue's pages, then the high and low queues'
/// and the public pool - the rest, less one - with `RQPN_LOAD` (bit 31) to latch them.
pub fn reserved_pages(q: TxQueues) -> (u32, u32) {
    let hq = if q.high { PAGES_HIGH } else { 0 };
    let lq = if q.low { PAGES_LOW } else { 0 };
    let nq = if q.normal { PAGES_NORMAL } else { 0 };
    let pubq = TOTAL_PAGES - hq - lq - nq - 1;
    (nq, (1 << 31) | hq | (lq << 8) | (pubq << 16))
}

/// `TRXDMA_QUEUE_*`: the hardware queue each traffic class is sent from.
const Q_LOW: u16 = 1;
const Q_NORMAL: u16 = 2;
const Q_HIGH: u16 = 3;

/// `REG_TRXDMA_CTRL` (0x10C, 16-bit) with its low three bits kept from `old` and the traffic classes mapped to
/// the queues the dongle has: VO at 4, VI at 6, BE at 8, BK at 10, MG at 12, HI at 14. `None` for a set of
/// queues `rtl8xxxu` does not know how to map.
pub fn priority(q: TxQueues, old: u16) -> Option<u16> {
    // (voq, viq, beq, bkq, mgq, hiq)
    let m = match q.count() {
        1 => {
            let h = if q.high { Q_HIGH } else if q.low { Q_LOW } else { Q_NORMAL };
            (h, h, h, h, h, h)
        }
        2 => {
            let (hi, lo) = if q.high && q.low {
                (Q_HIGH, Q_LOW)
            } else if q.normal && q.low {
                (Q_NORMAL, Q_LOW)
            } else {
                (Q_HIGH, Q_NORMAL)
            };
            (hi, hi, lo, lo, hi, hi)
        }
        3 => (Q_HIGH, Q_NORMAL, Q_LOW, Q_LOW, Q_HIGH, Q_HIGH),
        _ => return None,
    };
    Some((old & 0x7) | (m.0 << 4) | (m.1 << 6) | (m.2 << 8) | (m.3 << 10) | (m.4 << 12) | (m.5 << 14))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sie_register_and_the_fallback() {
        assert_eq!(from_sie(0x0F0F), TxQueues { high: true, normal: false, low: true });
        assert_eq!(from_sie(0), TxQueues { high: false, normal: false, low: false });
        assert_eq!(from_out_endpoints(2), Some(TxQueues { high: true, normal: true, low: false }));
        assert_eq!(from_out_endpoints(0), None);
    }

    #[test]
    fn rtl8xxxus_own_example_values() {
        // High and low queues: `0x200 = 0x80E9020C` and `0x10C = (old & 7) | 0xF5F0`.
        let hl = TxQueues { high: true, normal: false, low: true };
        assert_eq!(reserved_pages(hl), (0, 0x80E9_020C));
        assert_eq!(priority(hl, 0xFFFF), Some(0xF5F7));
        // All three: the public pool loses the normal queue's two pages as well.
        let all = TxQueues { high: true, normal: true, low: true };
        assert_eq!(reserved_pages(all), (0x02, 0x80E7_020C));
        assert_eq!(priority(all, 0), Some((3 << 4) | (2 << 6) | (1 << 8) | (1 << 10) | (3 << 12) | (3 << 14)));
        assert_eq!(priority(TxQueues { high: false, normal: false, low: false }, 0), None);
    }

    #[test]
    fn bulk_out_endpoints_in_a_configuration_descriptor() {
        // Configuration, interface, then IN 0x81 bulk, OUT 0x02 bulk, OUT 0x03 bulk, IN 0x84 interrupt.
        let cfg = [
            9, 2, 46, 0, 1, 1, 0, 0x80, 250,
            9, 4, 0, 0, 4, 0xFF, 0xFF, 0xFF, 0,
            7, 5, 0x81, 2, 0, 2, 0,
            7, 5, 0x02, 2, 0, 2, 0,
            7, 5, 0x03, 2, 0, 2, 0,
            7, 5, 0x84, 3, 64, 0, 1,
        ];
        assert_eq!(out_endpoints(&cfg), 2);
        assert_eq!(out_endpoints(&cfg[..9]), 0);
    }
}
