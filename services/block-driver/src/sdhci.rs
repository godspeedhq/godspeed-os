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
/// Command Timeout Error (SDHCI Error Interrupt Status bit 0, i.e. bit 16 of the combined register):
/// the card did not respond to the command at all. Deliberately NOT part of `INT_ERR` above, which
/// follows the reference driver's mask, so it is tested explicitly where a non-response must be seen.
const INT_CMD_TIMEOUT: u32 = 0x0001_0000;
// CONTROL1 bits.
const C1_CLK_INTLEN: u32 = 1 << 0;
const C1_CLK_STABLE: u32 = 1 << 1;
const C1_CLK_EN: u32 = 1 << 2;
const C1_TOUNIT_MAX: u32 = 0x000E_0000;
const C1_SRST_HC: u32 = 1 << 24;
const C1_SRST_CMD: u32 = 1 << 25; // SOFTWARE_RESET.RESET_CMD (byte 0x2F)
const C1_SRST_DATA: u32 = 1 << 26; // SOFTWARE_RESET.RESET_DATA

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
/// CMD55 (APP_CMD). **RSPNS_TYPE = 2 (48-bit), always** - CMD55 always answers R1. Declaring "no
/// response" here (RSPNS_TYPE 0, which this was) makes the controller stop driving/sampling CMD while
/// the card replies anyway, and the resulting **CMD line conflict** is reported on the transaction as
/// Command Timeout + Command CRC together (INT bits 16|17, the SDHCI spec's signature for a conflict).
/// That is precisely how ACMD41 failed on real hardware - CMD55 "succeeded" because a no-response
/// command completes as soon as it is sent, and the conflict it left behind killed the command after it.
/// QEMU models no line conflict, so it never noticed.
const CMD_APP_CMD: u32 = 0x3702_0000; // CMD55, R1 (48-bit)
const CMD_SEND_OP_COND: u32 = 0x2902_0000; // ACMD41

fn spin() {
    for _ in 0..2000 { core::hint::spin_loop(); }
}

struct Sd<'a> {
    m: &'a Mmio,
    rca: u32,
    sdhc: bool,
    sectors: u64,
    /// The controller's base clock in Hz, from the platform (InspectKernel query 20). Every card clock
    /// is derived from it; 0 means the platform did not report one and init refuses rather than guessing.
    base_clock: u32,
    /// The INTERRUPT register captured at the moment a command failed, BEFORE the line reset that clears
    /// it. Without this a caller logging INTERRUPT after a failure always sees 0.
    last_int: core::cell::Cell<u32>,
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
            // A command TIMEOUT (the card did not respond at all) is bit 16, which INT_ERR deliberately
            // excludes - so without this the commonest real-hardware failure looked like "nothing
            // happened" and we spun out the full bounded wait with INTERRUPT reading 0. Treat it as the
            // error it is: reset the line so the next command is not left inhibited, and report.
            if i & (INT_ERR | INT_CMD_TIMEOUT) != 0 {
                // Record WHY before recovering: `reset_cmd_dat` clears INTERRUPT, so a caller that logs
                // the register afterwards sees 0 and cannot tell a timeout from an error - which is
                // exactly how a real command timeout showed up as "INT=0x00000000, nothing happened".
                self.last_int.set(i);
                self.reset_cmd_dat();
                return None;
            }
            t += 1;
            if t > 2_000_000 { self.last_int.set(self.rd(INTERRUPT)); return None; }
        }
        self.wr(INTERRUPT, INT_CMD_DONE);
        // Ncc: the SD spec requires at least 8 card-clock cycles between the end of one command and the
        // start of the next. At the 400 kHz identification clock that is 20 us; issuing back-to-back can
        // catch the card still driving CMD, which the controller reports as a line conflict. A handful of
        // spins comfortably covers it and costs nothing next to a real transfer.
        for _ in 0..10 { spin(); }
        Some(self.rd(RESP0))
    }

    /// After a command/data error the CMD and DAT lines stay inhibited (SDHCI 3.10) - every later command
    /// would then spin to its bounded timeout. Reset both lines (bounded) and clear the latched error bits
    /// so I/O is not wedged by one transient glitch. Called on every error return below.
    fn reset_cmd_dat(&self) {
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_SRST_CMD | C1_SRST_DATA);
        let mut t = 0u32;
        while self.rd(CONTROL1) & (C1_SRST_CMD | C1_SRST_DATA) != 0 {
            t += 1;
            if t > 1_000_000 { break; }
        }
        self.wr(INTERRUPT, self.rd(INTERRUPT)); // clear latched error/status bits
    }

    /// An application command: CMD55 (APP_CMD) then the ACMD itself. `ctx` is only for reporting WHICH
    /// of the two failed - "ACMD41 failed" alone cannot distinguish "the card ignored CMD55" (the card is
    /// not listening at all) from "the card took CMD55 but refused the ACMD" (it is listening but
    /// rejecting our arguments), and those lead to completely different fixes.
    fn acmd(&self, code: u32, arg: u32, ctx: &ServiceContext) -> Option<u32> {
        // The response type is part of CMD_APP_CMD itself and does not depend on the card's state - the
        // old conditional added the 48-bit response type only when `rca != 0`, i.e. exactly NOT during
        // identification, which is when ACMD41 runs. CMD55's argument is the RCA (0 while identifying).
        if self.cmd(CMD_APP_CMD, self.rca).is_none() {
            ctx.log_fmt(format_args!("block-driver: CMD55 (APP_CMD, rca={:#010x}) failed - STATUS={:#010x} INT={:#010x}",
                                     self.rca, self.rd(STATUS), self.last_int.get()));
            return None;
        }
        self.cmd(code, arg)
    }

    /// The SDHCI clock divider for a target card clock, from the controller's real base clock.
    ///
    /// The card clock is `base / (2 * divisor)`, and the divisor is split across CONTROL1 as the
    /// SDHCI-3.0 10-bit field: low 8 bits at [15:8], high 2 bits at [7:6]. Deriving it from the ACTUAL
    /// base clock is the whole point - a hardcoded divider only yields the intended clock on the board
    /// it was measured on, and running the identification clock too fast means no card ever answers
    /// (the failure this driver had on real hardware while passing under emulation).
    fn divider_for(&self, target_hz: u32) -> u32 {
        let base = self.base_clock;
        if base == 0 || target_hz == 0 { return 0; }
        let mut divisor = (base + (2 * target_hz) - 1) / (2 * target_hz); // ceil: never faster than asked
        if divisor > 0x3FF { divisor = 0x3FF; }
        divisor
    }

    /// Program the card clock. `divisor` is the value from `divider_for`.
    fn set_clock(&self, divisor: u32, ctx: &ServiceContext) -> bool {
        self.wr(CONTROL1, self.rd(CONTROL1) & !C1_CLK_EN);
        for _ in 0..5 { spin(); }
        let c1 = (self.rd(CONTROL1) & !0x0000_FFE0)
            | C1_CLK_INTLEN
            | ((divisor & 0xFF) << 8)          // divider low 8 bits  [15:8]
            | (((divisor >> 8) & 0x3) << 6)    // divider high 2 bits [7:6] (SDHCI 3.0 10-bit mode)
            | C1_TOUNIT_MAX;
        self.wr(CONTROL1, c1);
        // The Arasan loses successive writes to the same register that fall within two card-clock cycles
        // of each other (a clock-domain-crossing erratum Linux's sdhci-iproc spaces out explicitly). At
        // the 400 kHz identification clock two cycles is ~5 us, so space the CONTROL1 writes generously.
        for _ in 0..40 { spin(); }
        let mut t = 0u32;
        while self.rd(CONTROL1) & C1_CLK_STABLE == 0 {
            t += 1;
            if t > 1_000_000 {
                ctx.log_fmt(format_args!(
                    "block-driver: EMMC clock never stable (divisor={}, CONTROL1={:#010x})",
                    divisor, self.rd(CONTROL1)));
                return false;
            }
        }
        self.wr(CONTROL1, self.rd(CONTROL1) | C1_CLK_EN);
        for _ in 0..40 { spin(); }
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
        // Every card clock derives from the controller's REAL base clock, which only the platform knows
        // (the Arasan's capability register reports it wrongly on this SoC). Refuse rather than guess: a
        // divider computed from a wrong base runs the identification clock at the wrong speed, and no
        // card answers - which is a silent failure on hardware and no failure at all under emulation.
        if self.base_clock == 0 {
            ctx.log("block-driver: EMMC base clock unknown (platform reported 0) - cannot set the card clock");
            return false;
        }
        let id_div = self.divider_for(400_000); // 400 kHz identification clock (SD spec)
        ctx.log_fmt(format_args!("block-driver: EMMC base clock {} Hz, identification divisor {}",
                                 self.base_clock, id_div));
        // Each step reports WHICH one failed. A bare `return false` told us only "no usable SD card",
        // which on real hardware is the difference between "the controller is not wired to the card" and
        // "the card refused a command" - a whole debugging session apart (invariant 12: loud failure).
        if !self.set_clock(id_div, ctx) { return false; }
        self.wr(INT_EN, 0xFFFF_FFFF);
        self.wr(INT_MASK, 0xFFFF_FFFF);

        if self.cmd(CMD_GO_IDLE, 0).is_none() {
            ctx.log_fmt(format_args!("block-driver: CMD0 (GO_IDLE) failed - STATUS={:#010x} INT={:#010x}",
                                     self.rd(STATUS), self.rd(INTERRUPT)));
            return false;
        }
        // CMD8 also tells us the card ACCEPTED our voltage and echoed the check pattern: the response
        // must echo 0x1AA. A card that answers but echoes something else is not talking to us properly,
        // which is worth seeing rather than assuming.
        let ifcond = match self.cmd(CMD_SEND_IF_COND, 0x0000_01AA) {
            Some(v) => v,
            None => {
                ctx.log_fmt(format_args!("block-driver: CMD8 (SEND_IF_COND) failed - STATUS={:#010x} INT={:#010x}",
                                         self.rd(STATUS), self.rd(INTERRUPT)));
                return false;
            }
        };
        ctx.log_fmt(format_args!("block-driver: CMD8 echo={:#010x} (want 0x000001aa)", ifcond));
        let mut ocr = 0u32;
        let mut tries = 0u32;
        loop {
            // ACMD41 argument: HCS (bit30, we support high-capacity) | XPC (bit28, max performance) |
            // the 3.2-3.4 V OCR window. Deliberately NOT S18R (bit24): that requests 1.8 V signalling,
            // and we never perform the CMD11 voltage switch that must follow it - asking for a switch we
            // cannot complete is a way to leave a real card unresponsive, and it is invisible under
            // emulation where the negotiation is not modelled.
            ocr = match self.acmd(CMD_SEND_OP_COND, 0x50FF_8000, ctx) {
                Some(v) => v,
                None => {
                    // A card that is still powering up can legitimately miss an ACMD41 - the sequence is
                    // meant to be repeated until it answers ready. Treat a non-response as another
                    // attempt rather than a fatal error, bounded by the same try budget below.
                    tries += 1;
                    if tries > 200 {
                        ctx.log_fmt(format_args!(
                            "block-driver: ACMD41 never answered - STATUS={:#010x} INT={:#010x}",
                            self.rd(STATUS), self.last_int.get()));
                        return false;
                    }
                    if tries == 1 {
                        ctx.log_fmt(format_args!(
                            "block-driver: ACMD41 no response (retrying) - STATUS={:#010x} INT={:#010x}",
                            self.rd(STATUS), self.last_int.get()));
                    }
                    for _ in 0..20 { spin(); }
                    continue;
                }
            };
            if ocr & 0x8000_0000 != 0 { break; }
            tries += 1;
            if tries > 200 { ctx.log_fmt(format_args!("block-driver: ACMD41 never ready (OCR={:#010x})", ocr)); return false; }
            for _ in 0..20 { spin(); }
        }
        self.sdhc = ocr & 0x4000_0000 != 0;
        if self.cmd(CMD_ALL_SEND_CID, 0).is_none() { ctx.log("block-driver: CMD2 (ALL_SEND_CID) failed"); return false; }
        self.rca = match self.cmd(CMD_SEND_REL_ADDR, 0) {
            Some(v) => v & 0xFFFF_0000,
            None => { ctx.log("block-driver: CMD3 (SEND_REL_ADDR) failed"); return false; }
        };
        // CSD (capacity) while the card is in stand-by, addressed by RCA.
        if self.cmd(CMD_SEND_CSD, self.rca).is_none() { ctx.log("block-driver: CMD9 (SEND_CSD) failed"); return false; }
        self.sectors = self.read_capacity();
        // Identification done - step up to the normal-speed clock (25 MHz, SD spec) for data transfer.
        let _ = self.set_clock(self.divider_for(25_000_000), ctx);
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
            if i & INT_ERR != 0 { self.reset_cmd_dat(); return false; }
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
            if i & INT_ERR != 0 { self.reset_cmd_dat(); return false; }
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
            if i & INT_ERR != 0 { self.reset_cmd_dat(); return false; }
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
    let mut sd = Sd { m: mmio, rca: 0, sdhc: false, sectors: 0,
                      base_clock: ctx.emmc_base_clock_hz(), last_int: core::cell::Cell::new(0) };
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
