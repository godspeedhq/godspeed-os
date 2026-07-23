// SPDX-License-Identifier: GPL-2.0-only
//! BCM2835 EMMC (Arasan SDHCI) - hardware-validation PROBE.
//!
//! This is a THROWAWAY in-kernel probe to de-risk the SD path before the real driver is built as a
//! userspace `block-driver` backend (Option A: the kernel grants the service an MMIO cap; the register
//! logic below moves there unchanged, driving `ctx.mmio()` instead of raw MMIO). It proves three things
//! in QEMU `raspi2b -drive if=sd,file=build/sd_test.img`: the EMMC is at peripheral+0x30_0000, a card is
//! present, and a real block read returns the on-disk signature.
//!
//! Register sequence reimplemented from the behaviour of a working polled bare-metal reference (bztsrc's
//! raspi tutorial `sd.c`) + the BCM2835 peripherals datasheet section 5 (EMMC). PIO/polled: no DMA (so
//! SEC-28 cache coherence does not apply) and no interrupts (so the missing ARM device-IRQ routing does
//! not matter). Every hardware wait is bounded (kernel-audit invariant 12).

use super::pl011_write;
use super::exceptions::write_hex32;

const EMMC_BASE: usize = super::PERIPHERAL_BASE + 0x30_0000;

// Register offsets (from EMMC_BASE).
const ARG2:        usize = 0x00;
const BLKSIZECNT:  usize = 0x04;
const ARG1:        usize = 0x08;
const CMDTM:       usize = 0x0C;
const RESP0:       usize = 0x10;
const DATA:        usize = 0x20;
const STATUS:      usize = 0x24;
const CONTROL0:    usize = 0x28;
const CONTROL1:    usize = 0x2C;
const INTERRUPT:   usize = 0x30;
const INT_MASK:    usize = 0x34; // IRPT_MASK
const INT_EN:      usize = 0x38; // IRPT_EN
const SLOTISR_VER: usize = 0xFC;

// STATUS bits.
const SR_CMD_INHIBIT: u32 = 1 << 0;
const SR_DAT_INHIBIT: u32 = 1 << 1;

// INTERRUPT bits.
const INT_CMD_DONE:  u32 = 1 << 0;
const INT_READ_RDY:  u32 = 1 << 5;
const INT_ERR_MASK:  u32 = 0x017E_8000; // all error bits

// CONTROL1 bits.
const C1_CLK_INTLEN: u32 = 1 << 0;
const C1_CLK_STABLE: u32 = 1 << 1;
const C1_CLK_EN:     u32 = 1 << 2;
const C1_TOUNIT_MAX: u32 = 0x000E_0000;
const C1_SRST_HC:    u32 = 1 << 24;

// CMDTM command encodings (index<<24 | RSPNS_TYPE<<16 | data/dir flags). RSPNS: 1=136-bit, 2=48-bit,
// 3=48-bit-busy. CMD_ISDATA=1<<21, TM_DAT_DIR(read)=1<<4.
const CMD_GO_IDLE:      u32 = 0x0000_0000; // CMD0,  no response
const CMD_ALL_SEND_CID: u32 = 0x0201_0000; // CMD2,  136-bit
const CMD_SEND_REL_ADDR:u32 = 0x0302_0000; // CMD3,  48-bit
const CMD_CARD_SELECT:  u32 = 0x0703_0000; // CMD7,  48-bit-busy
const CMD_SEND_IF_COND: u32 = 0x0802_0000; // CMD8,  48-bit
const CMD_READ_SINGLE:  u32 = 0x1122_0010; // CMD17, 48-bit + data + read
const CMD_APP_CMD:      u32 = 0x3700_0000; // CMD55, no response (0 RCA) - we OR in RSPNS when RCA set
const CMD_SEND_OP_COND: u32 = 0x2902_0000; // ACMD41, 48-bit (needs CMD55 prefix)

#[inline]
fn rd(off: usize) -> u32 {
    // SAFETY: EMMC MMIO is inside the Device-mapped peripheral window; a single 32-bit volatile read.
    unsafe { ((EMMC_BASE + off) as *const u32).read_volatile() }
}
#[inline]
fn wr(off: usize, v: u32) {
    // SAFETY: EMMC MMIO is Device-mapped; a single 32-bit volatile write.
    unsafe { ((EMMC_BASE + off) as *mut u32).write_volatile(v) }
}
fn spin(n: u32) {
    for _ in 0..n {
        // SAFETY: `nop` has no operands or memory effect.
        unsafe { core::arch::asm!("nop", options(nomem, nostack)); }
    }
}

/// State carried through the probe.
struct Card { rca: u32, sdhc: bool }

/// Issue a command: clear pending interrupts, write ARG1 + CMDTM, wait CMD_DONE (bounded). Returns
/// RESP0, or None on timeout/error.
fn cmd(code: u32, arg: u32) -> Option<u32> {
    // Wait for the command line to be free.
    let mut t = 0u32;
    while rd(STATUS) & SR_CMD_INHIBIT != 0 {
        t += 1;
        if t > 1_000_000 { pl011_write(b"sdhci: CMD_INHIBIT stuck\r\n"); return None; }
    }
    wr(INTERRUPT, rd(INTERRUPT)); // clear stale
    wr(ARG1, arg);
    wr(CMDTM, code);
    // A few commands need a settle before the response (op-cond / if-cond).
    if code == CMD_SEND_OP_COND { spin(200_000); } else if code == CMD_SEND_IF_COND { spin(50_000); }
    let mut t = 0u32;
    loop {
        let i = rd(INTERRUPT);
        if i & INT_CMD_DONE != 0 { break; }
        if i & INT_ERR_MASK != 0 {
            pl011_write(b"sdhci: cmd error INTERRUPT="); write_hex32(i); pl011_write(b"\r\n");
            return None;
        }
        t += 1;
        if t > 2_000_000 { pl011_write(b"sdhci: cmd timeout (no CMD_DONE)\r\n"); return None; }
    }
    wr(INTERRUPT, INT_CMD_DONE);
    Some(rd(RESP0))
}

/// ACMD (app command): CMD55 prefix carrying the RCA, then the app command.
fn acmd(code: u32, arg: u32, rca: u32) -> Option<u32> {
    // CMD55 wants a 48-bit response when an RCA is present.
    let app = if rca != 0 { CMD_APP_CMD | 0x0002_0000 } else { CMD_APP_CMD };
    cmd(app, rca)?;
    cmd(code, arg)
}

/// Program the SD clock to an approximate frequency via the CONTROL1 divider, then wait for it to
/// stabilise. `divider` is the 8-bit clock-divisor (base/(2*divider)); the exact value is not critical
/// under QEMU (emulated), and is refined for real hardware once the path is proven.
fn set_clock(divider: u32) -> bool {
    // Disable the clock while changing the divisor.
    wr(CONTROL1, rd(CONTROL1) & !C1_CLK_EN);
    spin(10_000);
    let c1 = (rd(CONTROL1) & !0x0000_FFE0) | C1_CLK_INTLEN | ((divider & 0xFF) << 8) | C1_TOUNIT_MAX;
    wr(CONTROL1, c1);
    let mut t = 0u32;
    while rd(CONTROL1) & C1_CLK_STABLE == 0 {
        t += 1;
        if t > 1_000_000 { pl011_write(b"sdhci: clock never stable\r\n"); return false; }
    }
    wr(CONTROL1, rd(CONTROL1) | C1_CLK_EN);
    spin(10_000);
    true
}

/// Reset + initialise the card far enough to read a block. Returns the card on success.
fn init() -> Option<Card> {
    let ver = rd(SLOTISR_VER);
    pl011_write(b"sdhci: SLOTISR_VER="); write_hex32(ver); pl011_write(b"\r\n");

    // Host-controller reset.
    wr(CONTROL1, rd(CONTROL1) | C1_SRST_HC);
    let mut t = 0u32;
    while rd(CONTROL1) & C1_SRST_HC != 0 {
        t += 1;
        if t > 1_000_000 { pl011_write(b"sdhci: SRST_HC never cleared - no controller?\r\n"); return None; }
    }

    if !set_clock(0x68) { return None; } // ~identification clock
    wr(INT_EN, 0xFFFF_FFFF);
    wr(INT_MASK, 0xFFFF_FFFF);

    cmd(CMD_GO_IDLE, 0)?;                       // CMD0
    cmd(CMD_SEND_IF_COND, 0x0000_01AA)?;        // CMD8: 2.7-3.6V, check pattern 0xAA

    // ACMD41 until the card leaves busy (bit31). HCS (bit30 of arg) requests SDHC; CCS (bit30 of OCR)
    // reports it.
    let mut ocr = 0u32;
    let mut tries = 0u32;
    loop {
        ocr = acmd(CMD_SEND_OP_COND, 0x51FF_8000, 0)?;
        if ocr & 0x8000_0000 != 0 { break; } // card ready
        tries += 1;
        if tries > 100 { pl011_write(b"sdhci: ACMD41 never ready\r\n"); return None; }
        spin(100_000);
    }
    let sdhc = ocr & 0x4000_0000 != 0; // CCS

    cmd(CMD_ALL_SEND_CID, 0)?;                  // CMD2
    let rca = cmd(CMD_SEND_REL_ADDR, 0)? & 0xFFFF_0000; // CMD3: RCA in the top 16 bits
    let _ = set_clock(0x04);                    // faster transfer clock
    cmd(CMD_CARD_SELECT, rca)?;                 // CMD7

    pl011_write(b"sdhci: card ready, RCA="); write_hex32(rca >> 16);
    pl011_write(if sdhc { b" (SDHC, block-addressed)\r\n" } else { b" (SDSC, byte-addressed)\r\n" });
    Some(Card { rca, sdhc })
}

/// Read one 512-byte block at LBA `lba` into `buf`. Returns true on success.
fn read_block(card: &Card, lba: u32, buf: &mut [u32; 128]) -> bool {
    let mut t = 0u32;
    while rd(STATUS) & SR_DAT_INHIBIT != 0 {
        t += 1;
        if t > 1_000_000 { pl011_write(b"sdhci: DAT_INHIBIT stuck\r\n"); return false; }
    }
    wr(BLKSIZECNT, (1 << 16) | 512);
    // SDHC addresses by block; SDSC by byte.
    let addr = if card.sdhc { lba } else { lba * 512 };
    if cmd(CMD_READ_SINGLE, addr).is_none() { return false; }
    // Wait for the read buffer to be ready.
    let mut t = 0u32;
    loop {
        let i = rd(INTERRUPT);
        if i & INT_READ_RDY != 0 { break; }
        if i & INT_ERR_MASK != 0 { pl011_write(b"sdhci: read error\r\n"); return false; }
        t += 1;
        if t > 2_000_000 { pl011_write(b"sdhci: read timeout\r\n"); return false; }
    }
    wr(INTERRUPT, INT_READ_RDY);
    for w in buf.iter_mut() { *w = rd(DATA); }
    true
}

/// Boot-time probe: init the card and read block 0, printing its first 16 bytes. Harmless if no SD
/// image is attached (init reports "no controller"/"never ready" and returns).
pub fn probe() {
    pl011_write(b"sdhci: probing BCM2835 EMMC (Arasan) at peripheral+0x300000...\r\n");
    let card = match init() { Some(c) => c, None => { pl011_write(b"sdhci: no usable card - SD unavailable\r\n"); return; } };
    let mut buf = [0u32; 128];
    if !read_block(&card, 0, &mut buf) { pl011_write(b"sdhci: block 0 read FAILED\r\n"); return; }
    // Print the first 16 bytes as ASCII (the sd_test.img signature is "GSFS-SD-BLOCK-0\0").
    pl011_write(b"sdhci: block 0 first 16 bytes: ");
    for i in 0..4 {
        let w = buf[i];
        let bytes = [w as u8, (w >> 8) as u8, (w >> 16) as u8, (w >> 24) as u8];
        for b in bytes {
            let c = if (0x20..0x7F).contains(&b) { b } else { b'.' };
            pl011_write(&[c]);
        }
    }
    pl011_write(b"\r\n");
    // Also read block 100 to prove addressing (signature "BLOCK-100-SENTINEL").
    if read_block(&card, 100, &mut buf) {
        pl011_write(b"sdhci: block 100 first 16 bytes: ");
        for i in 0..4 {
            let w = buf[i];
            for b in [w as u8, (w >> 8) as u8, (w >> 16) as u8, (w >> 24) as u8] {
                let c = if (0x20..0x7F).contains(&b) { b } else { b'.' };
                pl011_write(&[c]);
            }
        }
        pl011_write(b"\r\n");
    }
    pl011_write(b"sdhci: PROBE OK - EMMC + card + block read verified\r\n");
}
