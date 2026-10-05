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
    let [lo, hi] = reg.to_le_bytes();
    let v = val.to_le_bytes();
    let mut req = [0u8; 9 + 4];
    req[..9].copy_from_slice(&[usbfn::OP_CONTROL, DIR_OUT, VENDOR_REQ, lo, hi, 0, 0, width, 0]);
    req[9..9 + width as usize].copy_from_slice(&v[..width as usize]);
    let r = host(ctx, &req[..9 + width as usize])?;
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
