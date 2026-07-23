// SPDX-License-Identifier: GPL-2.0-only
//! `block-driver` ARM backend: BCM2835 EMMC (Arasan SDHCI), PIO/polled.
//!
//! The Raspberry Pi 2's SD card is on the Arasan EMMC (a standard SD Host Controller) at
//! peripheral + 0x30_0000. The kernel grants this service an MMIO cap to that window at spawn
//! (`arch::arm::map_fixed_driver_mmio`, the §12.3 fixed-peripheral grant), so the driver reaches the
//! registers through the SDK `Mmio` wrapper - **no `unsafe` in the service** (§18.1/§18.2).
//!
//! PIO, not DMA: the CPU reads/writes each 512-byte block word-by-word through `EMMC_DATA`. This
//! sidesteps the two ARM blockers - DMA cache coherence (SEC-28) and the missing device-IRQ-to-userspace
//! routing - so a userspace driver works today. Every hardware wait is bounded (invariant 12).
//!
//! Register sequence reimplemented from the behaviour of a working polled bare-metal reference (bztsrc's
//! raspi tutorial `sd.c`) + the BCM2835 peripherals datasheet section 5 (EMMC); no code copied.

use godspeed_sdk::{Message, Mmio, ServiceContext};

// Register offsets from the EMMC base.
const BLKSIZECNT: usize = 0x04;
const ARG1: usize = 0x08;
const CMDTM: usize = 0x0C;
const RESP0: usize = 0x10;
const RESP1: usize = 0x14;
const RESP2: usize = 0x18;
const RESP3: usize = 0x1C;
const DATA: usize = 0x20;
const STATUS: usize = 0x24;
const CONTROL1: usize = 0x2C;
const INTERRUPT: usize = 0x30;
const INT_MASK: usize = 0x34;
const INT_EN: usize = 0x38;
const SLOTISR_VER: usize = 0xFC;

// STATUS bits.
const SR_CMD_INHIBIT: u32 = 1 << 0;
const SR_DAT_INHIBIT: u32 = 1 << 1;
// INTERRUPT bits.
const INT_CMD_DONE: u32 = 1 << 0;
const INT_DATA_DONE: u32 = 1 << 1; // transfer complete
const INT_WRITE_RDY: u32 = 1 << 4;
const INT_READ_RDY: u32 = 1 << 5;
const INT_ERR: u32 = 0x017E_8000;
// CONTROL1 bits.
const C1_CLK_INTLEN: u32 = 1 << 0;
const C1_CLK_STABLE: u32 = 1 << 1;
const C1_CLK_EN: u32 = 1 << 2;
const C1_TOUNIT_MAX: u32 = 0x000E_0000;
const C1_SRST_HC: u32 = 1 << 24;

// CMDTM command encodings: index<<24 | RSPNS_TYPE<<16 | flags. RSPNS 1=136-bit, 2=48-bit, 3=48-bit-busy.
// CMD_ISDATA=1<<21, TM_DAT_DIR read=1<<4.
const CMD_GO_IDLE: u32 = 0x0000_0000; // CMD0
const CMD_ALL_SEND_CID: u32 = 0x0201_0000; // CMD2, 136-bit
const CMD_SEND_REL_ADDR: u32 = 0x0302_0000; // CMD3
const CMD_SEND_CSD: u32 = 0x0901_0000; // CMD9, 136-bit
const CMD_CARD_SELECT: u32 = 0x0703_0000; // CMD7, 48-bit-busy
const CMD_SEND_IF_COND: u32 = 0x0802_0000; // CMD8
const CMD_READ_SINGLE: u32 = 0x1122_0010; // CMD17, 48-bit + data + read
const CMD_WRITE_SINGLE: u32 = 0x1822_0000; // CMD24, 48-bit + data + write
const CMD_APP_CMD: u32 = 0x3700_0000; // CMD55
const CMD_SEND_OP_COND: u32 = 0x2902_0000; // ACMD41

fn spin() {
    for _ in 0..2000 { core::hint::spin_loop(); }
}

struct Sd<'a> {
    m: &'a Mmio,
    rca: u32,
    sdhc: bool,
    sectors: u64,
}

impl<'a> Sd<'a> {
    fn rd(&self, off: usize) -> u32 { self.m.read32(off) }
    fn wr(&self, off: usize, v: u32) { self.m.write32(off, v) }

    /// Issue a command, wait CMD_DONE (bounded), return RESP0 or None.
    fn cmd(&self, code: u32, arg: u32) -> Option<u32> {
        let mut t = 0u32;
        while self.rd(STATUS) & SR_CMD_INHIBIT != 0 {
            t += 1;
            if t > 1_000_000 { return None; }
        }
        self.wr(INTERRUPT, self.rd(INTERRUPT)); // clear stale
        self.wr(ARG1, arg);
        self.wr(CMDTM, code);
        if code == CMD_SEND_OP_COND { for _ in 0..40 { spin(); } }
        else if code == CMD_SEND_IF_COND { for _ in 0..10 { spin(); } }
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_CMD_DONE != 0 { break; }
            if i & INT_ERR != 0 { return None; }
            t += 1;
            if t > 2_000_000 { return None; }
        }
        self.wr(INTERRUPT, INT_CMD_DONE);
        Some(self.rd(RESP0))
    }

    fn acmd(&self, code: u32, arg: u32) -> Option<u32> {
        let app = if self.rca != 0 { CMD_APP_CMD | 0x0002_0000 } else { CMD_APP_CMD };
        self.cmd(app, self.rca)?;
        self.cmd(code, arg)
    }

    fn set_clock(&self, divider: u32) -> bool {
        self.wr(CONTROL1, self.rd(CONTROL1) & !C1_CLK_EN);
        for _ in 0..5 { spin(); }
        let c1 = (self.rd(CONTROL1) & !0x0000_FFE0) | C1_CLK_INTLEN | ((divider & 0xFF) << 8) | C1_TOUNIT_MAX;
        self.wr(CONTROL1, c1);
        let mut t = 0u32;
        while self.rd(CONTROL1) & C1_CLK_STABLE == 0 {
            t += 1;
            if t > 1_000_000 { return false; }
        }
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_CLK_EN);
        for _ in 0..5 { spin(); }
        true
    }

    /// Capacity in 512-byte sectors, from the CSD (CMD9). The 136-bit response comes back in RESP0-3
    /// with the CRC byte stripped (content is CSD[127:8]), so shift left by 8 to CSD-align, then extract.
    fn read_capacity(&self) -> u64 {
        let r0 = self.rd(RESP0);
        let r1 = self.rd(RESP1);
        let r2 = self.rd(RESP2);
        let r3 = self.rd(RESP3);
        let csd: u128 = (((r3 as u128) << 96) | ((r2 as u128) << 64) | ((r1 as u128) << 32) | (r0 as u128)) << 8;
        let structure = ((csd >> 126) & 0x3) as u32;
        if structure == 1 {
            // CSD v2 (SDHC/SDXC): C_SIZE at [69:48]; capacity = (C_SIZE+1) * 512 KiB = (C_SIZE+1)*1024 sectors.
            let c_size = ((csd >> 48) & 0x3F_FFFF) as u64;
            (c_size + 1) * 1024
        } else {
            // CSD v1 (SDSC): C_SIZE[73:62], C_SIZE_MULT[49:47], READ_BL_LEN[83:80].
            let c_size = ((csd >> 62) & 0xFFF) as u64;
            let c_mult = ((csd >> 47) & 0x7) as u32;
            let read_bl = ((csd >> 80) & 0xF) as u32;
            let blocknr = (c_size + 1) * (1u64 << (c_mult + 2));
            let bytes = blocknr * (1u64 << read_bl);
            bytes / 512
        }
    }

    /// Reset + initialise the card. Returns false if none is present.
    fn init(&mut self, ctx: &ServiceContext) -> bool {
        ctx.log_fmt(format_args!("block-driver: EMMC SLOTISR_VER={:#010x}", self.rd(SLOTISR_VER)));
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_SRST_HC);
        let mut t = 0u32;
        while self.rd(CONTROL1) & C1_SRST_HC != 0 {
            t += 1;
            if t > 1_000_000 { ctx.log("block-driver: EMMC SRST_HC never cleared"); return false; }
        }
        if !self.set_clock(0x68) { return false; }
        self.wr(INT_EN, 0xFFFF_FFFF);
        self.wr(INT_MASK, 0xFFFF_FFFF);

        if self.cmd(CMD_GO_IDLE, 0).is_none() { return false; }
        if self.cmd(CMD_SEND_IF_COND, 0x0000_01AA).is_none() { return false; }
        let mut ocr = 0u32;
        let mut tries = 0u32;
        loop {
            ocr = match self.acmd(CMD_SEND_OP_COND, 0x51FF_8000) { Some(v) => v, None => return false };
            if ocr & 0x8000_0000 != 0 { break; }
            tries += 1;
            if tries > 200 { ctx.log("block-driver: ACMD41 never ready"); return false; }
            for _ in 0..20 { spin(); }
        }
        self.sdhc = ocr & 0x4000_0000 != 0;
        if self.cmd(CMD_ALL_SEND_CID, 0).is_none() { return false; }
        self.rca = match self.cmd(CMD_SEND_REL_ADDR, 0) { Some(v) => v & 0xFFFF_0000, None => return false };
        // CSD (capacity) while the card is in stand-by, addressed by RCA.
        if self.cmd(CMD_SEND_CSD, self.rca).is_none() { return false; }
        self.sectors = self.read_capacity();
        let _ = self.set_clock(0x04);
        if self.cmd(CMD_CARD_SELECT, self.rca).is_none() { return false; }
        ctx.log_fmt(format_args!(
            "block-driver: SD card ready ({}, {} sectors = {} MiB)",
            if self.sdhc { "SDHC" } else { "SDSC" }, self.sectors, self.sectors / 2048
        ));
        true
    }

    /// The device address for LBA: block for SDHC, byte for SDSC.
    fn addr(&self, lba: u64) -> u32 { if self.sdhc { lba as u32 } else { (lba * 512) as u32 } }

    fn read_block(&self, lba: u64, buf: &mut [u8; 512]) -> bool {
        let mut t = 0u32;
        while self.rd(STATUS) & SR_DAT_INHIBIT != 0 {
            t += 1;
            if t > 1_000_000 { return false; }
        }
        self.wr(BLKSIZECNT, (1 << 16) | 512);
        if self.cmd(CMD_READ_SINGLE, self.addr(lba)).is_none() { return false; }
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_READ_RDY != 0 { break; }
            if i & INT_ERR != 0 { return false; }
            t += 1;
            if t > 2_000_000 { return false; }
        }
        self.wr(INTERRUPT, INT_READ_RDY);
        for i in 0..128 {
            let w = self.rd(DATA);
            buf[i * 4] = w as u8;
            buf[i * 4 + 1] = (w >> 8) as u8;
            buf[i * 4 + 2] = (w >> 16) as u8;
            buf[i * 4 + 3] = (w >> 24) as u8;
        }
        true
    }

    fn write_block(&self, lba: u64, buf: &[u8; 512]) -> bool {
        let mut t = 0u32;
        while self.rd(STATUS) & SR_DAT_INHIBIT != 0 {
            t += 1;
            if t > 1_000_000 { return false; }
        }
        self.wr(BLKSIZECNT, (1 << 16) | 512);
        if self.cmd(CMD_WRITE_SINGLE, self.addr(lba)).is_none() { return false; }
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_WRITE_RDY != 0 { break; }
            if i & INT_ERR != 0 { return false; }
            t += 1;
            if t > 2_000_000 { return false; }
        }
        self.wr(INTERRUPT, INT_WRITE_RDY);
        for i in 0..128 {
            let w = (buf[i * 4] as u32)
                | ((buf[i * 4 + 1] as u32) << 8)
                | ((buf[i * 4 + 2] as u32) << 16)
                | ((buf[i * 4 + 3] as u32) << 24);
            self.wr(DATA, w);
        }
        // Wait for the write to finish landing on the medium.
        let mut t = 0u32;
        loop {
            let i = self.rd(INTERRUPT);
            if i & INT_DATA_DONE != 0 { break; }
            if i & INT_ERR != 0 { return false; }
            t += 1;
            if t > 2_000_000 { return false; }
        }
        self.wr(INTERRUPT, INT_DATA_DONE);
        true
    }
}

/// Decode one block-IPC request and reply. Mirrors the AHCI backend's `serve` (same wire protocol).
fn serve(sd: &Sd, ctx: &ServiceContext, p: &[u8], reply: godspeed_sdk::CapHandle) {
    use super::{OP_CAPACITY, OP_READ_BLOCK, OP_WRITE_BLOCK, OP_WRITE_ZEROS, STATUS_ERR, STATUS_OK};
    let err = |ctx: &ServiceContext| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[STATUS_ERR])); };
    if p.is_empty() { return err(ctx); }
    if p[0] == OP_CAPACITY {
        let mut out = [0u8; 9];
        out[0] = STATUS_OK;
        out[1..9].copy_from_slice(&sd.sectors.to_le_bytes());
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
        return;
    }
    if p.len() < 9 { return err(ctx); }
    let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    match p[0] {
        OP_READ_BLOCK => {
            let mut buf = [0u8; 512];
            if sd.read_block(lba, &mut buf) {
                let mut out = [0u8; 513];
                out[0] = STATUS_OK;
                out[1..].copy_from_slice(&buf);
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
            } else { err(ctx); }
        }
        OP_WRITE_BLOCK => {
            if p.len() < 521 { return err(ctx); }
            let mut buf = [0u8; 512];
            buf.copy_from_slice(&p[9..521]);
            let status = if sd.write_block(lba, &buf) { STATUS_OK } else { STATUS_ERR };
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        }
        OP_WRITE_ZEROS => {
            if p.len() < 17 { return err(ctx); }
            let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
            let zero = [0u8; 512];
            let mut ok = true;
            for i in 0..count {
                if !sd.write_block(lba + i, &zero) { ok = false; break; }
            }
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[if ok { STATUS_OK } else { STATUS_ERR }]));
        }
        _ => err(ctx),
    }
}

/// Entry point: initialise the EMMC + serve block I/O to `fs`.
pub fn run(ctx: &ServiceContext, mmio: &Mmio) -> ! {
    let mut sd = Sd { m: mmio, rca: 0, sdhc: false, sectors: 0 };
    if !sd.init(ctx) {
        ctx.log("block-driver: no usable SD card - serving errors so fs never hangs");
        loop {
            let _msg = ctx.recv();
            if let Some(reply) = ctx.take_pending_cap() {
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[super::STATUS_ERR]));
                ctx.remove_cap(reply);
            }
        }
    }
    ctx.log("block-driver: SD (EMMC/PIO) serving block I/O");
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        serve(&sd, ctx, msg.payload_bytes(), reply);
        ctx.remove_cap(reply);
    }
}
