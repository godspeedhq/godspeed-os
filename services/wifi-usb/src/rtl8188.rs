// SPDX-License-Identifier: GPL-2.0-only
//! The RTL8188CUS itself: its register file over the host's control transfers, its efuse, and its
//! power-on sequence (`docs/wifi-usb.md`, phase R1).
//!
//! Every register value and the order they go in are the SILICON's requirement, read from Linux's
//! `rtl8xxxu` (`rtl8xxxu_read_efuse`, `rtl8192cu_power_on`) with `rtlwifi`'s `rtl8192cu` beside it where
//! the two disagree (26.14). What is ours: every wait is bounded in TIME rather than in a count of reads,
//! because a read here is a USB round trip through two services, not a bus cycle - Linux's "1000 reads"
//! means nothing at this distance - and each step that fails says which it was.

use godspeed::driver::delay;
use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::ServiceContext;
use godspeed_wifi::usbfn;

use crate::host;
use crate::rtl_queues::{self, TxQueues};
use crate::rtl_tables;

/// The register access (`rtl8xxxu_read32` and friends): vendor request 0x05, register in `wValue`.
const VENDOR_REQ: u8 = 0x05;
const DIR_IN: u8 = 0xC0;
const DIR_OUT: u8 = 0x40;

/// One register read of `width` bytes (1, 2 or 4), or why not.
pub fn read(ctx: &ServiceContext, reg: u16, width: u8) -> Result<u32, &'static str> {
    let [lo, hi] = reg.to_le_bytes();
    let r = host(ctx, &[usbfn::OP_CONTROL, DIR_IN, VENDOR_REQ, lo, hi, 0, 0, width, 0])?;
    let p = r.payload_bytes();
    if p.len() < 2 || p[0] != usbfn::OP_CONTROL {
        return Err("the host answered something other than CONTROL");
    }
    match p[1] {
        usbfn::ST_OK if p.len() >= 2 + width as usize => {
            let mut v = [0u8; 4];
            v[..width as usize].copy_from_slice(&p[2..2 + width as usize]);
            Ok(u32::from_le_bytes(v))
        }
        usbfn::ST_NO_DEVICE => Err("the dongle is no longer bound"),
        usbfn::ST_FAILED => Err("the transfer did not complete"),
        _ => Err("the host refused the request as malformed"),
    }
}

/// One register write of `width` bytes.
pub fn write(ctx: &ServiceContext, reg: u16, width: u8, val: u32) -> Result<(), &'static str> {
    write_bytes(ctx, reg, &val.to_le_bytes()[..width as usize])
}

/// `bytes` written from register `reg` in ONE control transfer - a register's 1, 2 or 4, or one block of the
/// firmware download (`rtl8xxxu_writeN`, 128 at a time for this family).
pub fn write_bytes(ctx: &ServiceContext, reg: u16, bytes: &[u8]) -> Result<(), &'static str> {
    let [lo, hi] = reg.to_le_bytes();
    let [nlo, nhi] = (bytes.len() as u16).to_le_bytes();
    let mut req = [0u8; 9 + crate::rtl_fw::BLOCK];
    if bytes.len() > crate::rtl_fw::BLOCK {
        return Err("a write longer than one 128-byte block");
    }
    req[..9].copy_from_slice(&[usbfn::OP_CONTROL, DIR_OUT, VENDOR_REQ, lo, hi, 0, 0, nlo, nhi]);
    req[9..9 + bytes.len()].copy_from_slice(bytes);
    let r = host(ctx, &req[..9 + bytes.len()])?;
    let p = r.payload_bytes();
    match (p.first().copied(), p.get(1).copied()) {
        (Some(usbfn::OP_CONTROL), Some(usbfn::ST_OK)) => Ok(()),
        (Some(usbfn::OP_CONTROL), Some(usbfn::ST_NO_DEVICE)) => Err("the dongle is no longer bound"),
        (Some(usbfn::OP_CONTROL), Some(usbfn::ST_FAILED)) => Err("the transfer did not complete"),
        _ => Err("the host refused the write or answered something else"),
    }
}

pub fn read8(ctx: &ServiceContext, reg: u16) -> Result<u8, &'static str> { read(ctx, reg, 1).map(|v| v as u8) }
pub fn read16(ctx: &ServiceContext, reg: u16) -> Result<u16, &'static str> { read(ctx, reg, 2).map(|v| v as u16) }
pub fn read32(ctx: &ServiceContext, reg: u16) -> Result<u32, &'static str> { read(ctx, reg, 4) }
pub fn write8(ctx: &ServiceContext, reg: u16, v: u8) -> Result<(), &'static str> { write(ctx, reg, 1, v as u32) }
pub fn write16(ctx: &ServiceContext, reg: u16, v: u16) -> Result<(), &'static str> { write(ctx, reg, 2, v as u32) }
pub fn write32(ctx: &ServiceContext, reg: u16, v: u32) -> Result<(), &'static str> { write(ctx, reg, 4, v) }

/// Poll `reg` (of `width`) until `done(value)`, for at most `ms`; the value that satisfied it, or `None`.
fn poll(ctx: &ServiceContext, reg: u16, width: u8, ms: u64, done: impl Fn(u32) -> bool) -> Result<Option<u32>, &'static str> {
    let mut d = wait::Deadline::start(ctx, Budget::ms(ms));
    loop {
        let v = read(ctx, reg, width)?;
        if done(v) {
            return Ok(Some(v));
        }
        if d.expired() {
            return Ok(None);
        }
    }
}

// ---- Registers, by Linux's names (`rtl8xxxu_regs.h`). ---------------------------------------------
pub(crate) const REG_SYS_ISO_CTRL: u16 = 0x0000;
/// The chip's version and vendor bits - U1's identification (`main.rs`).
pub(crate) const REG_SYS_CFG: u16 = 0x00F0;
const REG_SYS_FUNC: u16 = 0x0002;
const REG_APS_FSMCO: u16 = 0x0004;
const REG_SYS_CLKR: u16 = 0x0008;
const REG_9346CR: u16 = 0x000A;
const REG_SPS0_CTRL: u16 = 0x0011;
const REG_RSV_CTRL: u16 = 0x001C;
const REG_LDOA15_CTRL_HI: u16 = 0x0021;
const REG_EFUSE_CTRL: u16 = 0x0030;
const REG_EFUSE_TEST: u16 = 0x00CF;
const REG_CR: u16 = 0x0100;
const REG_APSD_CTRL: u16 = 0x0600;
const REG_USB_UNDOCUMENTED: u16 = 0xFE10;
const REG_MCU_FW_DL: u16 = 0x0080;
const REG_TRXDMA_CTRL: u16 = 0x010C;
const REG_TRXFF_BNDY: u16 = 0x0114;
const REG_RQPN: u16 = 0x0200;
const REG_RQPN_NPQ: u16 = 0x0214;
const REG_NORMAL_SIE_EP_TX: u16 = 0xFE66;
/// The MAC's clock in `SYS_CLKR` (`SYS_CLK_MAC_CLK_ENABLE`), and the value `CR` reads on a MAC never set up.
const SYS_CLK_MAC_CLK_ENABLE: u16 = 1 << 11;
const CR_COLD: u8 = 0xEA;
/// The receive FIFO's boundary, `trxff_boundary` for this family, written at `TRXFF_BNDY + 2`.
const RX_BOUNDARY: u16 = 0x27FF;
/// `MCU_FW_DL` bits (`rtl8xxxu_regs.h`).
const MCU_FW_DL_ENABLE: u32 = 1 << 0;
const MCU_FW_DL_READY: u32 = 1 << 1;
const MCU_FW_DL_CSUM_REPORT: u32 = 1 << 2;
const MCU_WINT_INIT_READY: u32 = 1 << 6;
const MCU_FW_RAM_SEL: u32 = 1 << 7;
/// The 8051's enable in `SYS_FUNC` (`SYS_FUNC_CPU_ENABLE`).
const SYS_FUNC_CPU_ENABLE: u16 = 1 << 10;

/// The logical efuse is 512 bytes on this family (`EFUSE_MAP_LEN`), built from a physical stream of
/// headers and words; the physical stream is walked at most this far (`EFUSE_REAL_CONTENT_LEN_8192C`).
const EFUSE_MAP_LEN: usize = 512;
const EFUSE_PHYSICAL_MAX: u16 = 512;
/// `struct rtl8192cu_efuse`: the ID that must be there, the VID and PID, and the MAC address.
const EFUSE_ID: u16 = 0x8129;
const EFUSE_OFF_VID: usize = 0x0A;
const EFUSE_OFF_PID: usize = 0x0C;
const EFUSE_OFF_MAC: usize = 0x16;

/// One physical efuse byte (`rtl8xxxu_read_efuse8`): the address in `EFUSE_CTRL` bytes 1-2, bit 31 clear
/// to start the read, then bit 31 SET means the byte in bits 7:0 is ready.
fn efuse_byte(ctx: &ServiceContext, addr: u16) -> Result<u8, &'static str> {
    write8(ctx, REG_EFUSE_CTRL + 1, (addr & 0xFF) as u8)?;
    let b2 = read8(ctx, REG_EFUSE_CTRL + 2)?;
    write8(ctx, REG_EFUSE_CTRL + 2, (b2 & 0xFC) | ((addr >> 8) & 0x3) as u8)?;
    let b3 = read8(ctx, REG_EFUSE_CTRL + 3)?;
    write8(ctx, REG_EFUSE_CTRL + 3, b3 & 0x7F)?;
    match poll(ctx, REG_EFUSE_CTRL, 4, 50, |v| v & 0x8000_0000 != 0)? {
        Some(v) => Ok(v as u8),
        None => Err("an efuse byte never reported ready"),
    }
}

/// What the efuse says about this dongle.
pub struct Efuse {
    pub id: u16,
    pub vid: u16,
    pub pid: u16,
    pub mac: [u8; 6],
    /// Physical bytes walked, and the sections found - for the log.
    pub walked: u16,
    pub sections: u16,
}

/// Read the efuse into its logical map (`rtl8xxxu_read_efuse`), the loader enabled as Linux enables it
/// first and `EFUSE_TEST` put back after.
pub fn read_efuse(ctx: &ServiceContext) -> Result<Efuse, &'static str> {
    let cr = read8(ctx, REG_9346CR)?;
    ctx.log_fmt(format_args!(
        "wifi-usb: 9346CR={:#04x} - {}, {}",
        cr,
        if cr & 0x20 != 0 { "efuse autoload OK" } else { "efuse autoload NOT reported" },
        if cr & 0x10 != 0 { "boot from EEPROM" } else { "boot from efuse" }));
    // `rtl8xxxu_read_efuse`: the loader's power, clock and enable, set only where they are clear.
    write8(ctx, REG_EFUSE_TEST, 0x69)?;
    let iso = read16(ctx, REG_SYS_ISO_CTRL)?;
    if iso & (1 << 15) == 0 {
        write16(ctx, REG_SYS_ISO_CTRL, iso | (1 << 15))?;
    }
    let func = read16(ctx, REG_SYS_FUNC)?;
    if func & (1 << 12) == 0 {
        write16(ctx, REG_SYS_FUNC, func | (1 << 12))?;
    }
    let clk = read16(ctx, REG_SYS_CLKR)?;
    if clk & ((1 << 5) | (1 << 1)) != ((1 << 5) | (1 << 1)) {
        write16(ctx, REG_SYS_CLKR, clk | (1 << 5) | (1 << 1))?;
    }

    let mut map = [0xFFu8; EFUSE_MAP_LEN];
    let mut addr: u16 = 0;
    let mut sections = 0u16;
    let walk = (|| -> Result<(), &'static str> {
        while addr < EFUSE_PHYSICAL_MAX {
            let h = efuse_byte(ctx, addr)?;
            addr += 1;
            if h == 0xFF {
                break;
            }
            let (section, mask) = if h & 0x1F == 0x0F {
                let ext = efuse_byte(ctx, addr)?;
                addr += 1;
                ((((h & 0xE0) >> 5) | ((ext & 0xF0) >> 1)) as usize, ext & 0x0F)
            } else {
                ((h >> 4) as usize, h & 0x0F)
            };
            sections += 1;
            for word in 0..4 {
                if mask & (1 << word) != 0 {
                    continue; // a SET bit means the word is absent
                }
                let lo = efuse_byte(ctx, addr)?;
                let hi = efuse_byte(ctx, addr + 1)?;
                addr += 2;
                let at = section * 8 + word * 2;
                if at + 1 < EFUSE_MAP_LEN {
                    map[at] = lo;
                    map[at + 1] = hi;
                }
            }
        }
        Ok(())
    })();
    let _ = write8(ctx, REG_EFUSE_TEST, 0x00);
    walk?;

    let le16 = |at: usize| u16::from_le_bytes([map[at], map[at + 1]]);
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&map[EFUSE_OFF_MAC..EFUSE_OFF_MAC + 6]);
    Ok(Efuse { id: le16(0), vid: le16(EFUSE_OFF_VID), pid: le16(EFUSE_OFF_PID), mac, walked: addr, sections })
}

pub fn efuse_id_ok(e: &Efuse) -> bool {
    e.id == EFUSE_ID
}

/// Whether the MAC is COLD - never set up since power came on - asked BEFORE the power-on, as
/// `rtl8xxxu_init_device` asks it: `CR` reading `0xEA`, or the MAC's clock off. A cold MAC has its page
/// reservation written; a warm one keeps what it has.
pub fn mac_is_cold(ctx: &ServiceContext) -> Result<bool, &'static str> {
    Ok(read8(ctx, REG_CR)? == CR_COLD || read16(ctx, REG_SYS_CLKR)? & SYS_CLK_MAC_CLK_ENABLE == 0)
}

/// The dongle's transmit queues (`rtl8xxxu_config_endpoints_sie`): from `NORMAL_SIE_EP_TX`, or, where that
/// reads nothing, from the count of bulk OUT endpoints in its configuration descriptor - asked of the dongle
/// with a standard `GET_DESCRIPTOR` through the host. The queues, and the endpoint count when it was needed.
pub fn tx_queues(ctx: &ServiceContext) -> Result<(TxQueues, Option<u8>), &'static str> {
    let q = rtl_queues::from_sie(read16(ctx, REG_NORMAL_SIE_EP_TX)?);
    if q.count() > 0 {
        return Ok((q, None));
    }
    let n = rtl_queues::out_endpoints(&config_descriptor(ctx)?);
    rtl_queues::from_out_endpoints(n)
        .map(|q| (q, Some(n)))
        .ok_or("the dongle reports no transmit queue and declares no bulk OUT endpoint")
}

/// The configuration descriptor, whole (up to `CONTROL_MAX`): `GET_DESCRIPTOR(CONFIGURATION, 0)`, a standard
/// request every USB device answers, asked twice - nine bytes for its total length, then the total.
fn config_descriptor(ctx: &ServiceContext) -> Result<[u8; usbfn::CONTROL_MAX], &'static str> {
    let mut out = [0u8; usbfn::CONTROL_MAX];
    let head = control_in(ctx, [0x80, 0x06, 0x00, 0x02, 0, 0, 9, 0], &mut out)?;
    if head < 4 {
        return Err("the configuration descriptor's header came back short");
    }
    let total = (u16::from_le_bytes([out[2], out[3]]) as usize).min(usbfn::CONTROL_MAX);
    let [lo, hi] = (total as u16).to_le_bytes();
    control_in(ctx, [0x80, 0x06, 0x00, 0x02, 0, 0, lo, hi], &mut out)?;
    Ok(out)
}

/// One control transfer IN with the given setup packet; the bytes returned are copied into `out`.
fn control_in(ctx: &ServiceContext, setup: [u8; 8], out: &mut [u8]) -> Result<usize, &'static str> {
    let mut req = [0u8; 9];
    req[0] = usbfn::OP_CONTROL;
    req[1..9].copy_from_slice(&setup);
    let r = host(ctx, &req)?;
    let p = r.payload_bytes();
    match (p.first().copied(), p.get(1).copied()) {
        (Some(usbfn::OP_CONTROL), Some(usbfn::ST_OK)) => {
            let n = (p.len() - 2).min(out.len());
            out[..n].copy_from_slice(&p[2..2 + n]);
            Ok(n)
        }
        _ => Err("a standard request to the dongle did not complete"),
    }
}

/// The transmit queues set up, after the power-on and BEFORE the firmware download, as both Linux drivers
/// order it (`rtl8xxxu_init_device`; `rtlwifi`'s `_rtl92cu_init_mac` runs before `rtl92c_download_fw` too):
/// the page reservation on a cold MAC, the queue priority, and the receive FIFO's boundary.
pub fn init_queues(ctx: &ServiceContext, q: TxQueues, cold: bool) -> Result<(), &'static str> {
    if cold {
        let (npq, rqpn) = rtl_queues::reserved_pages(q);
        write32(ctx, REG_RQPN_NPQ, npq)?;
        write32(ctx, REG_RQPN, rqpn)?;
    }
    let old = read16(ctx, REG_TRXDMA_CTRL)?;
    let p = rtl_queues::priority(q, old).ok_or("no queue priority for this set of transmit queues")?;
    write16(ctx, REG_TRXDMA_CTRL, p)?;
    write16(ctx, REG_TRXFF_BNDY + 2, RX_BOUNDARY)
}

// ---- R3a: the MAC, the baseband and the RF, after the firmware (`rtl8xxxu_init_device`, then
// `rtl8xxxu_start`, then `rtl8xxxu_gen1_config_channel`). Registers by Linux's names (`rtl8xxxu_regs.h`). ----
const REG_LDOA15_CTRL: u16 = 0x0020;
const REG_AFE_XTAL_CTRL: u16 = 0x0024;
const REG_AFE_PLL_CTRL: u16 = 0x0028;
const REG_RF_CTRL: u16 = 0x001F;
const REG_LEDCFG2: u16 = 0x004E;
const REG_PBP: u16 = 0x0104;
const REG_LLT_INIT: u16 = 0x01E0;
const REG_TDECTRL: u16 = 0x0208;
const REG_TXDMA_OFFSET_CHK: u16 = 0x020C;
const REG_HIMR: u16 = 0x0120;
const REG_HISR: u16 = 0x0124;
const REG_HWSEQ_CTRL: u16 = 0x0423;
const REG_TXPKTBUF_BCNQ_BDNY: u16 = 0x0424;
const REG_TXPKTBUF_MGQ_BDNY: u16 = 0x0425;
const REG_TXPKTBUF_WMAC_LBK_BF_HD: u16 = 0x045D;
const REG_FAST_EDCA_CTRL: u16 = 0x0460;
const REG_MAX_AGGR_NUM: u16 = 0x04CA;
const REG_BAR_MODE_CTRL: u16 = 0x04CC;
const REG_SIFS_CCK: u16 = 0x0514;
const REG_SIFS_OFDM: u16 = 0x0516;
const REG_TXPAUSE: u16 = 0x0522;
const REG_BW_OPMODE: u16 = 0x0603;
const REG_RCR: u16 = 0x0608;
const REG_RX_DRVINFO_SZ: u16 = 0x060F;
const REG_MAR: u16 = 0x0620;
const REG_R2T_SIFS: u16 = 0x063C;
const REG_T2T_SIFS: u16 = 0x063E;
const REG_CAM_CMD: u16 = 0x0670;
const REG_RXFLTMAP0: u16 = 0x06A0;
const REG_RXFLTMAP2: u16 = 0x06A4;
const REG_FPGA0_RF_MODE: u16 = 0x0800;
const REG_FPGA0_TX_INFO: u16 = 0x0804;
const REG_FPGA0_XA_HSSI_PARM1: u16 = 0x0820;
const REG_FPGA0_XA_HSSI_PARM2: u16 = 0x0824;
const REG_FPGA0_XA_LSSI_PARM: u16 = 0x0840;
const REG_FPGA0_XA_RF_INT_OE: u16 = 0x0860;
const REG_FPGA0_XA_RF_SW_CTRL: u16 = 0x0870;
const REG_FPGA0_XAB_RF_PARM: u16 = 0x0878;
const REG_FPGA0_ANALOG2: u16 = 0x0884;
const REG_FPGA0_XA_LSSI_READBACK: u16 = 0x08A0;
const REG_HSPI_XA_READBACK: u16 = 0x08B8;
const REG_FPGA1_RF_MODE: u16 = 0x0900;
const REG_OFDM0_TRX_PATH_ENABLE: u16 = 0x0C04;
const REG_OFDM0_XA_AGC_CORE1: u16 = 0x0C50;
const REG_OFDM1_LSTF: u16 = 0x0D00;
const REG_RX_WAIT_CCA: u16 = 0x0E70;
const REG_USB_SPECIAL_OPTION: u16 = 0xFE55;
/// RF registers (`RF6052_REG_*`): the mode and channel word, and the receive/transmit mode.
const RF_MODE_AG: u8 = 0x18;
const RF_AC: u8 = 0x00;
/// The bits of `RF_MODE_AG` a channel and a 20 MHz width occupy (`MODE_AG_CHANNEL_MASK`, `MODE_AG_BW_MASK`).
pub const RF_CHANNEL_MASK: u32 = 0x3FF;
const RF_BW_MASK: u32 = (1 << 10) | (1 << 11);
const RF_BW_20MHZ: u32 = 1 << 10;
/// The receive configuration while scanning (`init_device`'s `RCR`): accept frames to us, multicast,
/// broadcast and management, append the PHY status and the decryption results - and NOT the BSSID checks, so
/// any network's beacons pass.
const RCR_SCAN: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 13) | (1 << 14) | (1 << 28) | (1 << 29) | (1 << 30);
/// The RF table's pseudo-register for a 50 ms pause (`rtl8xxxu_init_rf_regs`); the others are unused by this table.
const RF_TABLE_PAUSE_50MS: u8 = 0xFE;

fn set32(ctx: &ServiceContext, reg: u16, set: u32, clear: u32) -> Result<(), &'static str> {
    let v = read32(ctx, reg)?;
    write32(ctx, reg, (v & !clear) | set)
}

/// One RF register on path A (`rtl8xxxu_write_rfreg`): address and 20 bits of data through the LSSI parameter
/// register, which serialises them to the RF chip. No read-back, as Linux.
pub fn write_rf(ctx: &ServiceContext, reg: u8, data: u32) -> Result<(), &'static str> {
    write32(ctx, REG_FPGA0_XA_LSSI_PARM, ((reg as u32) << 20) | (data & 0xF_FFFF))?;
    delay::hold(ctx, Budget::us(1));
    Ok(())
}

/// One RF register on path A read back (`rtl8xxxu_read_rfreg`): the address put in HSSI parameter 2 with an
/// edge on its read bit, then the value from the HSPI or the LSSI read-back register, as parameter 1 says.
pub fn read_rf(ctx: &ServiceContext, reg: u8) -> Result<u32, &'static str> {
    let hssia = read32(ctx, REG_FPGA0_XA_HSSI_PARM2)?;
    let v = (hssia & !0x7F80_0000) | ((reg as u32) << 23) | (1 << 31);
    write32(ctx, REG_FPGA0_XA_HSSI_PARM2, hssia & !(1 << 31))?;
    delay::hold(ctx, Budget::us(10));
    write32(ctx, REG_FPGA0_XA_HSSI_PARM2, v)?;
    delay::hold(ctx, Budget::us(100));
    write32(ctx, REG_FPGA0_XA_HSSI_PARM2, hssia | (1 << 31))?;
    delay::hold(ctx, Budget::us(10));
    let from = if read32(ctx, REG_FPGA0_XA_HSSI_PARM1)? & (1 << 8) != 0 { REG_HSPI_XA_READBACK } else { REG_FPGA0_XA_LSSI_READBACK };
    Ok(read32(ctx, from)? & 0xF_FFFF)
}

/// `rtl8xxxu_gen1_init_phy_bb`, then the 1T tables: the PLL, the baseband out of reset, the RF gate open, the
/// RF enabled, the baseband and AGC tables, the LDO.
fn init_phy_bb(ctx: &ServiceContext) -> Result<(), &'static str> {
    let p = read8(ctx, REG_AFE_PLL_CTRL)?;
    delay::hold(ctx, Budget::us(2));
    write8(ctx, REG_AFE_PLL_CTRL, p | (1 << 1))?;
    delay::hold(ctx, Budget::us(2));
    write8(ctx, REG_AFE_PLL_CTRL + 1, 0xFF)?;
    delay::hold(ctx, Budget::us(2));
    let f = read16(ctx, REG_SYS_FUNC)?;
    write16(ctx, REG_SYS_FUNC, f | 0x03)?;
    set32(ctx, REG_AFE_XTAL_CTRL, 0, 1 << 14)?;
    write8(ctx, REG_RF_CTRL, 0x07)?;
    for (reg, val) in rtl_tables::PHY_1T.iter().chain(rtl_tables::AGC_STANDARD.iter()) {
        write32(ctx, *reg, *val)?;
        delay::hold(ctx, Budget::us(1));
    }
    write32(ctx, REG_LDOA15_CTRL, 0x0157_2505)
}

/// `rtl8xxxu_init_phy_rf` on path A: the RF environment saved, the interface's output enables and its 3-wire
/// address and data lengths set, the radio table written (its 0xFE entries are 50 ms pauses), the environment
/// restored. The number of RF registers written.
fn init_phy_rf(ctx: &ServiceContext) -> Result<usize, &'static str> {
    let rfenv = read16(ctx, REG_FPGA0_XA_RF_SW_CTRL)? & (1 << 4);
    set32(ctx, REG_FPGA0_XA_RF_INT_OE, 1 << 20, 0)?;
    delay::hold(ctx, Budget::us(1));
    set32(ctx, REG_FPGA0_XA_RF_INT_OE, 1 << 4, 0)?;
    delay::hold(ctx, Budget::us(1));
    set32(ctx, REG_FPGA0_XA_HSSI_PARM2, 0, 0x400)?;
    delay::hold(ctx, Budget::us(1));
    set32(ctx, REG_FPGA0_XA_HSSI_PARM2, 0, 0x800)?;
    delay::hold(ctx, Budget::us(1));
    let mut n = 0usize;
    for (reg, val) in rtl_tables::RADIO_A_1T.iter() {
        if *reg == RF_TABLE_PAUSE_50MS {
            delay::hold_parked(ctx, Budget::ms(50));
            continue;
        }
        write_rf(ctx, *reg, *val)?;
        n += 1;
    }
    let s = read16(ctx, REG_FPGA0_XA_RF_SW_CTRL)?;
    write16(ctx, REG_FPGA0_XA_RF_SW_CTRL, (s & !(1 << 4)) | rfenv)?;
    Ok(n)
}

/// The link-list table, entry by entry, each confirmed by the chip clearing its operation bits.
fn init_llt(ctx: &ServiceContext) -> Result<(), &'static str> {
    for (entry, next) in rtl_queues::llt_entries() {
        write32(ctx, REG_LLT_INIT, (1 << 30) | ((entry as u32) << 8) | next as u32)?;
        if poll(ctx, REG_LLT_INIT, 4, 20, |v| v & (0x3 << 30) == 0)?.is_none() {
            return Err("a link-list entry was never taken (LLT_INIT stayed busy)");
        }
    }
    Ok(())
}

/// `rtl8xxxu_gen1_usb_quirks`: the USB PHY writes Linux makes for the interface's interference, the second
/// block because this part is not a UMC A-cut.
fn usb_quirks(ctx: &ServiceContext) -> Result<(), &'static str> {
    for (a, b) in [(0xE0, 0x8D)] {
        write8(ctx, 0xFE40, a)?;
        write8(ctx, 0xFE41, b)?;
        write8(ctx, 0xFE42, 0x80)?;
    }
    write32(ctx, REG_TXDMA_OFFSET_CHK, 0x00FD_0320)?;
    for (a, b) in [(0xE6, 0x94), (0xE0, 0x19), (0xE5, 0x91), (0xE2, 0x81)] {
        write8(ctx, 0xFE40, a)?;
        write8(ctx, 0xFE41, b)?;
        write8(ctx, 0xFE42, 0x80)?;
    }
    Ok(())
}

/// `rtl8723a_phy_lc_calibrate`: transmit paused, the synthesiser's LC calibration started in `RF_MODE_AG` and
/// given 100 ms, transmit resumed. (The continuous-transmit branch cannot apply: nothing has transmitted.)
fn lc_calibrate(ctx: &ServiceContext) -> Result<(), &'static str> {
    let lstf = read32(ctx, REG_OFDM1_LSTF)?;
    if lstf & 0x7000_0000 != 0 {
        return Err("continuous transmit is on before calibration - not a state this driver puts the chip in");
    }
    write8(ctx, REG_TXPAUSE, 0xFF)?;
    let m = read_rf(ctx, RF_MODE_AG)?;
    write_rf(ctx, RF_MODE_AG, m | 0x0_8000)?;
    delay::hold_parked(ctx, Budget::ms(100));
    write8(ctx, REG_TXPAUSE, 0x00)
}

/// `rtl8xxxu_gen1_enable_rf`: the regulator, the RF parameter word, path A as the transmit path, Japan mode
/// off, the CCA wait, and RF register 0 into its receive mode.
fn enable_rf(ctx: &ServiceContext) -> Result<(), &'static str> {
    let s = read8(ctx, REG_SPS0_CTRL)?;
    write8(ctx, REG_SPS0_CTRL, s | 0x09)?;
    set32(ctx, REG_FPGA0_XAB_RF_PARM, 1 << 3, (1 << 4) | (1 << 5))?;
    set32(ctx, REG_OFDM0_TRX_PATH_ENABLE, 1 << 4, 0xF0)?;
    set32(ctx, REG_FPGA0_RF_MODE, 0, 1 << 1)?;
    write32(ctx, REG_RX_WAIT_CCA, 0x631B_25A0)?;
    write_rf(ctx, RF_AC, 0x3_2D95)?;
    write8(ctx, REG_TXPAUSE, 0x00)
}

/// `rtl8xxxu_gen1_config_channel` for a 20 MHz HT channel: the band width registers, the channel into
/// `RF_MODE_AG`, the SIFS timings, the 20 MHz bit. `channel` is 1 to 14.
pub fn set_channel(ctx: &ServiceContext, channel: u8) -> Result<(), &'static str> {
    let o = read8(ctx, REG_BW_OPMODE)?;
    write8(ctx, REG_BW_OPMODE, o | (1 << 2))?;
    set32(ctx, REG_FPGA0_RF_MODE, 0, 1 << 0)?;
    set32(ctx, REG_FPGA1_RF_MODE, 0, 1 << 0)?;
    set32(ctx, REG_FPGA0_ANALOG2, 1 << 10, 0)?;
    let m = read_rf(ctx, RF_MODE_AG)?;
    write_rf(ctx, RF_MODE_AG, (m & !RF_CHANNEL_MASK) | channel as u32)?;
    write8(ctx, REG_SIFS_CCK + 1, 0x0E)?;
    write8(ctx, REG_SIFS_OFDM + 1, 0x0E)?;
    write16(ctx, REG_R2T_SIFS, 0x0808)?;
    write16(ctx, REG_T2T_SIFS, 0x0A0A)?;
    let m = read_rf(ctx, RF_MODE_AG)?;
    write_rf(ctx, RF_MODE_AG, (m & !RF_BW_MASK) | RF_BW_20MHZ)
}

/// R3a: everything `rtl8xxxu_init_device` does after the firmware that bears on RECEIVING, then
/// `rtl8xxxu_start`'s RF enable, filters and gain, then `channel`. Left out, and recorded in
/// `docs/wifi-usb.md`: the transmit side (power, the response rate set and retry limits, the EDCA, ACK and
/// beacon timings), the IQ calibration and the thermal meter - none decides whether a beacon is heard.
/// `RF_MODE_AG` read back after the channel is set, for the caller to check.
pub fn init_radio(ctx: &ServiceContext, cold: bool, channel: u8) -> Result<(usize, u32), &'static str> {
    for (reg, val) in rtl_tables::MAC_INIT.iter() {
        write8(ctx, *reg, *val)?;
    }
    write8(ctx, REG_MAX_AGGR_NUM, 0x0A)?;
    init_phy_bb(ctx)?;
    let rf = init_phy_rf(ctx)?;
    write32(ctx, REG_FPGA0_TX_INFO, 0x0000_0003)?;
    // The T/R and antenna switches, and the PA enable (`no_pape` is 0 for this part): 0x07000760.
    write32(ctx, REG_FPGA0_XA_RF_SW_CTRL, 0x0700_0760)?;
    write32(ctx, REG_FPGA0_XA_RF_INT_OE, 0x66F6_0210)?;
    if cold {
        for reg in [REG_TXPKTBUF_BCNQ_BDNY, REG_TXPKTBUF_MGQ_BDNY, REG_TXPKTBUF_WMAC_LBK_BF_HD, REG_TRXFF_BNDY, REG_TDECTRL + 1] {
            write8(ctx, reg, 0xF9)?;
        }
    }
    write8(ctx, REG_PBP, 0x11)?;
    if cold {
        init_llt(ctx)?;
        usb_quirks(ctx)?;
    }
    write8(ctx, REG_RX_DRVINFO_SZ, 4)?;
    write32(ctx, REG_HISR, 0xFFFF_FFFF)?;
    write32(ctx, REG_HIMR, 0xFFFF_FFFF)?;
    write32(ctx, REG_RCR, RCR_SCAN)?;
    write32(ctx, REG_MAR, 0xFFFF_FFFF)?;
    write32(ctx, REG_MAR + 4, 0xFFFF_FFFF)?;
    // Receive aggregation OFF (`rtl8xxxu_gen1_init_aggregation`, its default): one frame per bulk transfer.
    let u = read8(ctx, REG_USB_SPECIAL_OPTION)?;
    write8(ctx, REG_USB_SPECIAL_OPTION, u & !(1 << 3))?;
    let t = read8(ctx, REG_TRXDMA_CTRL)?;
    write8(ctx, REG_TRXDMA_CTRL, t & !(1 << 2))?;
    set32(ctx, REG_FPGA0_RF_MODE, (1 << 24) | (1 << 25), 0)?;
    write32(ctx, REG_CAM_CMD, (1 << 31) | (1 << 30))?;
    let l = read8(ctx, REG_LEDCFG2)?;
    write8(ctx, REG_LEDCFG2, l | (1 << 7))?;
    write8(ctx, REG_HWSEQ_CTRL, 0xFF)?;
    write32(ctx, REG_BAR_MODE_CTRL, 0x0201_FFFF)?;
    write16(ctx, REG_FAST_EDCA_CTRL, 0)?;
    lc_calibrate(ctx)?;
    enable_rf(ctx)?;
    write16(ctx, REG_RXFLTMAP2, 0xFFFF)?;
    write16(ctx, REG_RXFLTMAP0, 0xFFFF)?;
    set32(ctx, REG_OFDM0_XA_AGC_CORE1, 0x1E, 0x7F)?;
    set_channel(ctx, channel)?;
    Ok((rf, read_rf(ctx, RF_MODE_AG)?))
}

/// `rtl8xxxu_reset_8051`: the 8051 held and released - `RSV_CTRL + 1` bit 0 and the CPU enable, off then on.
fn reset_8051(ctx: &ServiceContext) -> Result<(), &'static str> {
    let r = read8(ctx, REG_RSV_CTRL + 1)?;
    write8(ctx, REG_RSV_CTRL + 1, r & !0x01)?;
    let f = read16(ctx, REG_SYS_FUNC)? & !SYS_FUNC_CPU_ENABLE;
    write16(ctx, REG_SYS_FUNC, f)?;
    let r = read8(ctx, REG_RSV_CTRL + 1)?;
    write8(ctx, REG_RSV_CTRL + 1, r | 0x01)?;
    write16(ctx, REG_SYS_FUNC, f | SYS_FUNC_CPU_ENABLE)
}

/// `rtl8xxxu_download_firmware`: the 8051 enabled, a running firmware reset, the download enabled and the
/// checksum report reset, then every block of `code` (the file after its header) into its page, and the
/// download DISABLED whatever happened - as Linux does on its abort path. The number of blocks written.
pub fn download_firmware(ctx: &ServiceContext, code: &[u8]) -> Result<usize, &'static str> {
    let f = read8(ctx, REG_SYS_FUNC + 1)?;
    write8(ctx, REG_SYS_FUNC + 1, f | 0x04)?;
    let f = read16(ctx, REG_SYS_FUNC)?;
    write16(ctx, REG_SYS_FUNC, f | SYS_FUNC_CPU_ENABLE)?;
    if read8(ctx, REG_MCU_FW_DL)? as u32 & MCU_FW_RAM_SEL != 0 {
        ctx.log("wifi-usb: a firmware is already running from RAM - resetting the 8051 first, as Linux does");
        write8(ctx, REG_MCU_FW_DL, 0x00)?;
        reset_8051(ctx)?;
    }
    let d = read8(ctx, REG_MCU_FW_DL)?;
    write8(ctx, REG_MCU_FW_DL, d | MCU_FW_DL_ENABLE as u8)?;
    let d = read32(ctx, REG_MCU_FW_DL)?;
    write32(ctx, REG_MCU_FW_DL, d & !(1 << 19))?;
    let d = read8(ctx, REG_MCU_FW_DL)?;
    write8(ctx, REG_MCU_FW_DL, d | MCU_FW_DL_CSUM_REPORT as u8)?;
    let written = (|| -> Result<usize, &'static str> {
        let mut n = 0usize;
        let mut page = u8::MAX;
        for b in crate::rtl_fw::blocks(code) {
            if b.page != page {
                let p = read8(ctx, REG_MCU_FW_DL + 2)?;
                write8(ctx, REG_MCU_FW_DL + 2, (p & 0xF8) | b.page)?;
                page = b.page;
            }
            write_bytes(ctx, b.addr, b.bytes)?;
            n += 1;
        }
        Ok(n)
    })();
    let d = read16(ctx, REG_MCU_FW_DL)?;
    write16(ctx, REG_MCU_FW_DL, d & !(MCU_FW_DL_ENABLE as u16))?;
    written
}

/// `rtl8xxxu_start_firmware`: the checksum the chip computed must be reported, then READY set and
/// `WINT_INIT_READY` cleared, the 8051 reset so it starts from RAM, and `WINT_INIT_READY` waited for - the
/// firmware's own word that it is running. `MCU_FW_DL` as it read then.
pub fn start_firmware(ctx: &ServiceContext) -> Result<u32, &'static str> {
    if poll(ctx, REG_MCU_FW_DL, 4, 200, |v| v & MCU_FW_DL_CSUM_REPORT != 0)?.is_none() {
        return Err("the chip never reported the download's checksum (MCU_FW_DL bit 2)");
    }
    let d = read32(ctx, REG_MCU_FW_DL)?;
    write32(ctx, REG_MCU_FW_DL, (d | MCU_FW_DL_READY) & !MCU_WINT_INIT_READY)?;
    reset_8051(ctx)?;
    match poll(ctx, REG_MCU_FW_DL, 4, 500, |v| v & MCU_WINT_INIT_READY != 0)? {
        Some(v) => Ok(v),
        None => Err("the firmware never reported it was running (MCU_FW_DL bit 6, WINT_INIT_READY)"),
    }
}

/// `rtl8192cu_power_on`, step by step; `Err` names the step that did not complete.
pub fn power_on(ctx: &ServiceContext) -> Result<u16, &'static str> {
    // 1. The autoload must be done (`PFM_ALDN`, bit 1 of APS_FSMCO).
    if poll(ctx, REG_APS_FSMCO, 1, 200, |v| v & 0x02 != 0)?.is_none() {
        return Err("step 1: the efuse autoload never reported done (APS_FSMCO bit 1)");
    }
    // 2. Unlock the ISO, clock and power registers.
    write8(ctx, REG_RSV_CTRL, 0x00)?;
    // 3. The switching regulator into PWM mode, then 100 us.
    write8(ctx, REG_SPS0_CTRL, 0x2B)?;
    delay::hold(ctx, Budget::us(100));
    // 4. The LDO, if it is off; then the MD2PP isolation off.
    let ldo = read8(ctx, REG_LDOA15_CTRL_HI)?;
    if ldo & 0x01 == 0 {
        write8(ctx, REG_LDOA15_CTRL_HI, ldo | 0x01)?;
        delay::hold(ctx, Budget::us(100));
        let iso = read8(ctx, REG_SYS_ISO_CTRL)?;
        write8(ctx, REG_SYS_ISO_CTRL, iso & !0x01)?;
    }
    // 5. MAC_ENABLE (bit 8 of APS_FSMCO), which the chip clears when the power-up is done.
    let f = read16(ctx, REG_APS_FSMCO)?;
    write16(ctx, REG_APS_FSMCO, f | 0x0100)?;
    if poll(ctx, REG_APS_FSMCO, 2, 200, |v| v & 0x0100 == 0)?.is_none() {
        return Err("step 5: MAC_ENABLE never cleared - the MAC did not power up");
    }
    // 6. HW_SUSPEND, ENABLE_POWERDOWN and PFM_ALDN, as a plain write (both drivers write this literal).
    write16(ctx, REG_APS_FSMCO, 0x0812)?;
    // 7. The RF isolation off (DIOR, bit 9 of SYS_ISO_CTRL).
    let iso = read16(ctx, REG_SYS_ISO_CTRL)?;
    write16(ctx, REG_SYS_ISO_CTRL, iso & !(1 << 9))?;
    // 8. APSD off, and wait for its state bit to clear. rtlwifi carries on if it does not; this says so.
    let apsd = read8(ctx, REG_APSD_CTRL)?;
    write8(ctx, REG_APSD_CTRL, apsd & !0x40)?;
    if poll(ctx, REG_APSD_CTRL, 1, 200, |v| v & 0x80 == 0)?.is_none() {
        ctx.log("wifi-usb: step 8: APSD_CTRL bit 7 did not clear - carrying on, as rtlwifi does");
    }
    // 9. The DMA, protocol, schedule and MAC TX/RX blocks (rtl8xxxu's `| 0x00FF`; rtlwifi also sets ENSEC).
    let cr = read16(ctx, REG_CR)?;
    write16(ctx, REG_CR, cr | 0x00FF)?;
    // 10. rtl8xxxu writes 0x19 here and does not say why; rtlwifi does not. Done as rtl8xxxu does, since
    // the firmware path R2 follows is rtl8xxxu's.
    write8(ctx, REG_USB_UNDOCUMENTED, 0x19)?;
    read16(ctx, REG_CR)
}
