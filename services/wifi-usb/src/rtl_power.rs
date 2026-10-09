// SPDX-License-Identifier: GPL-2.0-only
//! The RTL8188CUS's transmit power, from its own calibration (R11, `docs/wifi-usb.md` 21).
//!
//! Read from Linux's `rtl8xxxu` (`core.c` `rtl8xxxu_gen1_set_tx_power`, `8192c.c` `rtl8192cu_parse_efuse`
//! and the two power base tables, fetched 2026-10-06). The factory writes per-channel-group power indexes
//! into each dongle's efuse; the driver turns them into the baseband's transmit gain words every time it
//! tunes a channel. Until R11 this driver left the gain registers at what the baseband table wrote, the
//! same for every dongle and every channel.
//!
//! Pure, and naming nothing outside `core`, so `scripts/host_test_check.py` runs its tests on every build.
//! The arithmetic is Linux's to the byte, wrapping where C wraps: a `u8` sum truncated, the gain words
//! added as whole 32-bit values so a carry crosses bytes exactly as it does there (26.14: the silicon's
//! numbers, taken whole).

/// Where the power indexes start in the efuse's logical map (`struct rtl8192cu_efuse`,
/// `cck_tx_power_index_A` at 0x5a), and how many bytes they take: seven arrays of three.
pub const EFUSE_OFF: usize = 0x5A;
pub const EFUSE_LEN: usize = 21;

/// `RF6052_MAX_TX_PWR`: the largest index the gain registers take.
const MAX_TX_PWR: u8 = 0x3F;
/// The ceiling an RTL8188RU's high-power amplifier puts on the CCK index (`priv->hi_pa`).
const HI_PA_CCK_MAX: u8 = 0x20;

/// The efuse's power calibration, one value per channel group (`rtl8xxxu_gen1_channel_to_group`) per RF
/// path; the `_diff` arrays hold path A in their low nibble and path B in the high, each signed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Calibration {
    pub cck: [[u8; 3]; 2],
    pub ht40_1s: [[u8; 3]; 2],
    pub ht40_2s_diff: [u8; 3],
    pub ht20_diff: [u8; 3],
    pub ofdm_diff: [u8; 3],
}

impl Calibration {
    /// From the efuse bytes at `EFUSE_OFF`, in `struct rtl8192cu_efuse`'s order.
    pub fn from_efuse(b: &[u8; EFUSE_LEN]) -> Self {
        let three = |at: usize| [b[at], b[at + 1], b[at + 2]];
        Calibration {
            cck: [three(0), three(3)],
            ht40_1s: [three(6), three(9)],
            ht40_2s_diff: three(12),
            ht20_diff: three(15),
            ofdm_diff: three(18),
        }
    }

    /// An efuse that was never programmed reads 0xFF throughout: no calibration to use.
    pub fn programmed(&self) -> bool {
        self.cck[0][0] != 0xFF && self.ht40_1s[0][0] != 0xFF
    }
}

/// `struct rtl8xxxu_power_base`: what each gain word is offset by, per rate group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Base {
    pub r0e00: u32, pub r0e04: u32, pub r0e10: u32, pub r0e14: u32, pub r0e18: u32, pub r0e1c: u32,
    pub r0830: u32, pub r0834: u32, pub r083c: u32, pub r0848: u32, pub r084c: u32, pub r0868: u32,
}

/// `rtl8192c_power_base`, for every chip of the family but the 8188RU.
pub const BASE_8192C: Base = Base {
    r0e00: 0x07090c0c, r0e04: 0x01020405, r0e10: 0x0b0c0c0e, r0e14: 0x01030506, r0e18: 0x0b0c0d0e,
    r0e1c: 0x01030509, r0830: 0x07090c0c, r0834: 0x01020405, r083c: 0x0b0c0d0e, r0848: 0x01030509,
    r084c: 0x0b0c0d0e, r0868: 0x01030509,
};

/// `rtl8188r_power_base`, the 8188RU's (a high-power amplifier, so lower offsets).
pub const BASE_8188R: Base = Base {
    r0e00: 0x06080808, r0e04: 0x00040406, r0e10: 0x04060608, r0e14: 0x00020204, r0e18: 0x04060608,
    r0e1c: 0x00020204, r0830: 0x06080808, r0834: 0x00040406, r083c: 0x04060608, r0848: 0x00020204,
    r084c: 0x04060608, r0868: 0x00020204,
};

/// The chip's power setup: its calibration, how many paths transmit, and whether it is an 8188RU.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TxPower {
    pub cal: Calibration,
    pub tx_paths: u8,
    pub is_8188r: bool,
}

/// Registers the gain words go to.
pub const REG_TX_AGC_A_RATE18_06: u16 = 0x0E00;
pub const REG_TX_AGC_A_RATE54_24: u16 = 0x0E04;
pub const REG_TX_AGC_A_CCK1_MCS32: u16 = 0x0E08;
pub const REG_TX_AGC_A_MCS03_MCS00: u16 = 0x0E10;
pub const REG_TX_AGC_A_MCS07_MCS04: u16 = 0x0E14;
pub const REG_TX_AGC_A_MCS11_MCS08: u16 = 0x0E18;
pub const REG_TX_AGC_A_MCS15_MCS12: u16 = 0x0E1C;
pub const REG_TX_AGC_B_RATE18_06: u16 = 0x0830;
pub const REG_TX_AGC_B_RATE54_24: u16 = 0x0834;
pub const REG_TX_AGC_B_CCK1_55_MCS32: u16 = 0x0838;
pub const REG_TX_AGC_B_MCS03_MCS00: u16 = 0x083C;
pub const REG_TX_AGC_B_MCS07_MCS04: u16 = 0x0848;
pub const REG_TX_AGC_B_MCS11_MCS08: u16 = 0x084C;
pub const REG_TX_AGC_B_MCS15_MCS12: u16 = 0x0868;
pub const REG_TX_AGC_B_CCK11_A_CCK2_11: u16 = 0x086C;
pub const REG_OFDM0_XC_TX_IQ_IMBALANCE: u16 = 0x0C90;
pub const REG_OFDM0_XD_TX_IQ_IMBALANCE: u16 = 0x0C98;

/// What [`words`] produces: the two CCK indexes (written by read-modify-write, the registers being shared
/// with other fields), the twelve gain words in Linux's order, and the three IQ-imbalance bytes for each
/// path that follow the last word of each.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Words {
    pub cck: [u8; 2],
    /// `ofdm`, after the two-path adjustment and the ceiling, for the log.
    pub ofdm: [u8; 2],
    pub gains: [(u16, u32); 12],
    pub iq_c: [u8; 3],
    pub iq_d: [u8; 3],
}

/// `rtl8xxxu_gen1_channel_to_group`.
pub fn group(channel: u8) -> usize {
    if channel < 4 {
        0
    } else if channel < 10 {
        1
    } else {
        2
    }
}

/// A `struct rtl8723au_idx` field: path A's signed low nibble, or path B's signed high one.
fn nibble(v: u8, path_b: bool) -> i8 {
    let n = if path_b { v >> 4 } else { v & 0x0F };
    ((n << 4) as i8) >> 4
}

/// The bytes of `w` replicated into all four lanes, as Linux builds `ofdm_a` and `mcs_a`.
fn four(w: u8) -> u32 {
    let w = w as u32;
    w | w << 8 | w << 16 | w << 24
}

/// `rtl8xxxu_gen1_set_tx_power`'s arithmetic for `channel`, 20 MHz (this driver tunes no 40 MHz channel).
pub fn words(p: &TxPower, channel: u8) -> Words {
    let g = group(channel);
    let base = if p.is_8188r { BASE_8188R } else { BASE_8192C };
    let mut cck = [p.cal.cck[0][g], p.cal.cck[1][g]];
    if p.is_8188r {
        for c in cck.iter_mut() {
            *c = (*c).min(HI_PA_CCK_MAX);
        }
    }
    let mut ofdm = [p.cal.ht40_1s[0][g], p.cal.ht40_1s[1][g]];
    let ofdmbase = [
        ofdm[0].wrapping_add_signed(nibble(p.cal.ofdm_diff[g], false)),
        ofdm[1].wrapping_add_signed(nibble(p.cal.ofdm_diff[g], true)),
    ];
    // 20 MHz: the HT20 difference applies (`!ht40`).
    let mcsbase = [
        ofdm[0].wrapping_add_signed(nibble(p.cal.ht20_diff[g], false)),
        ofdm[1].wrapping_add_signed(nibble(p.cal.ht20_diff[g], true)),
    ];
    if p.tx_paths > 1 {
        for (i, o) in ofdm.iter_mut().enumerate() {
            let d = nibble(p.cal.ht40_2s_diff[g], i == 1);
            // Linux compares the u8 with the signed nibble and subtracts it.
            if (*o as i16) > d as i16 {
                *o = o.wrapping_add_signed(d.wrapping_neg());
            }
        }
    }
    for i in 0..2 {
        cck[i] = cck[i].min(MAX_TX_PWR);
        ofdm[i] = ofdm[i].min(MAX_TX_PWR);
    }
    let (ofdm_a, ofdm_b) = (four(ofdmbase[0]), four(ofdmbase[1]));
    let (mcs_a, mcs_b) = (four(mcsbase[0]), four(mcsbase[1]));
    let a1c = mcs_a.wrapping_add(base.r0e1c);
    let b68 = mcs_b.wrapping_add(base.r0868);
    let iq = |top: u32| {
        let mut v = (top >> 24) as u8;
        let mut out = [0u8; 3];
        for (i, o) in out.iter_mut().enumerate() {
            let step = if i != 2 { 8 } else { 6 };
            v = v.saturating_sub(step);
            *o = v;
        }
        out
    };
    Words {
        cck,
        ofdm,
        gains: [
            (REG_TX_AGC_A_RATE18_06, ofdm_a.wrapping_add(base.r0e00)),
            (REG_TX_AGC_B_RATE18_06, ofdm_b.wrapping_add(base.r0830)),
            (REG_TX_AGC_A_RATE54_24, ofdm_a.wrapping_add(base.r0e04)),
            (REG_TX_AGC_B_RATE54_24, ofdm_b.wrapping_add(base.r0834)),
            (REG_TX_AGC_A_MCS03_MCS00, mcs_a.wrapping_add(base.r0e10)),
            (REG_TX_AGC_B_MCS03_MCS00, mcs_b.wrapping_add(base.r083c)),
            (REG_TX_AGC_A_MCS07_MCS04, mcs_a.wrapping_add(base.r0e14)),
            (REG_TX_AGC_B_MCS07_MCS04, mcs_b.wrapping_add(base.r0848)),
            (REG_TX_AGC_A_MCS11_MCS08, mcs_a.wrapping_add(base.r0e18)),
            (REG_TX_AGC_B_MCS11_MCS08, mcs_b.wrapping_add(base.r084c)),
            (REG_TX_AGC_A_MCS15_MCS12, a1c),
            (REG_TX_AGC_B_MCS15_MCS12, b68),
        ],
        iq_c: iq(a1c),
        iq_d: iq(b68),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(cck: u8, ht40: u8, ofdm_diff: u8, ht20_diff: u8) -> Calibration {
        Calibration {
            cck: [[cck; 3]; 2],
            ht40_1s: [[ht40; 3]; 2],
            ht40_2s_diff: [0; 3],
            ht20_diff: [ht20_diff; 3],
            ofdm_diff: [ofdm_diff; 3],
        }
    }

    #[test]
    fn the_channel_groups_are_linuxs() {
        assert_eq!((group(1), group(3), group(4), group(9), group(10), group(13)), (0, 0, 1, 1, 2, 2));
    }

    #[test]
    fn a_nibble_is_signed() {
        assert_eq!(nibble(0x0F, false), -1);
        assert_eq!(nibble(0x07, false), 7);
        assert_eq!(nibble(0x80, true), -8);
        assert_eq!(nibble(0x21, true), 2);
    }

    #[test]
    fn the_words_add_the_base_to_the_replicated_index() {
        let w = words(&TxPower { cal: cal(0x2A, 0x2C, 0x00, 0x00), tx_paths: 1, is_8188r: false }, 1);
        assert_eq!(w.cck, [0x2A, 0x2A]);
        assert_eq!(w.gains[0], (0x0E00, 0x2C2C2C2C + 0x07090C0C));
        assert_eq!(w.gains[4], (0x0E10, 0x2C2C2C2C + 0x0B0C0C0E));
        // The IQ bytes walk down from the top byte of the last path-A word: 0x2C + 0x01 = 0x2D.
        assert_eq!(w.iq_c, [0x25, 0x1D, 0x17]);
    }

    #[test]
    fn the_differences_are_applied_signed() {
        // OFDM 2 above HT40 (low nibble 2), HT20 1 below it (low nibble 0xF).
        let w = words(&TxPower { cal: cal(0x20, 0x20, 0x02, 0x0F), tx_paths: 1, is_8188r: false }, 6);
        assert_eq!(w.gains[0].1, 0x22222222 + 0x07090C0C);
        assert_eq!(w.gains[4].1, 0x1F1F1F1F + 0x0B0C0C0E);
    }

    #[test]
    fn the_ceilings_hold() {
        let w = words(&TxPower { cal: cal(0x50, 0x50, 0, 0), tx_paths: 1, is_8188r: false }, 1);
        assert_eq!(w.cck, [0x3F, 0x3F]);
        let r = words(&TxPower { cal: cal(0x30, 0x30, 0, 0), tx_paths: 1, is_8188r: true }, 1);
        assert_eq!(r.cck, [0x20, 0x20], "an 8188RU's CCK ceiling");
        assert_eq!(r.gains[0].1, 0x30303030 + 0x06080808, "and its own base");
    }

    #[test]
    fn the_iq_bytes_stop_at_zero() {
        let w = words(&TxPower { cal: cal(0, 0, 0, 0), tx_paths: 1, is_8188r: false }, 1);
        // Top byte of 0 + 0x01030509 is 0x01: 0x01 - 8 floors at 0.
        assert_eq!(w.iq_c, [0, 0, 0]);
    }

    #[test]
    fn an_unprogrammed_efuse_is_noticed() {
        assert!(!Calibration::from_efuse(&[0xFF; EFUSE_LEN]).programmed());
        let mut b = [0u8; EFUSE_LEN];
        b[0] = 0x2A;
        b[6] = 0x2C;
        let c = Calibration::from_efuse(&b);
        assert!(c.programmed());
        assert_eq!((c.cck[0][0], c.ht40_1s[0][0]), (0x2A, 0x2C));
    }
}
