// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! AHCI (SATA) backend for `block-driver` (docs/ahci.md).
//!
//! A DMA + MMIO driver: the kernel maps the HBA's ABAR (MMIO) and grants a
//! physically-contiguous DMA arena at spawn (same path as the USB drivers). This
//! replaces ATA PIO on modern machines (the T630's SSD is AHCI-only).
//!
//! **Steps A+B (this file): detect + port init + IDENTIFY.** Map the ABAR, enable
//! AHCI mode, enumerate ports, then on the disk port set up the command list / FIS
//! / command table in the arena, start the port, and issue IDENTIFY DEVICE to read
//! the model + sector count. Read/write (READ/WRITE DMA EXT) come next.

use core::cell::Cell;

use godspeed_sdk::{CapHandle, Dma, Message, Mmio, ServiceContext};

// HBA Generic Host Control registers (offsets from ABAR).
const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_PI: usize = 0x0C;
const HBA_VS: usize = 0x10;
const GHC_AE: u32 = 1 << 31;

// Per-port registers: base = 0x100 + port*0x80.
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const PX_CLB: usize = 0x00;
const PX_CLBU: usize = 0x04;
const PX_FB: usize = 0x08;
const PX_FBU: usize = 0x0C;
const PX_IS: usize = 0x10;
const PX_CMD: usize = 0x18;
const PX_TFD: usize = 0x20;
const PX_SIG: usize = 0x24;
const PX_SSTS: usize = 0x28;
const PX_SERR: usize = 0x30;
const PX_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;

const SIG_SATA: u32 = 0x0000_0101;

// DMA arena layout (the arena is page-aligned, so all these meet AHCI alignment:
// command list 1 KiB, FIS 256 B, command table 128 B).
const CMD_LIST_OFF: usize = 0x000; // 32 command headers × 32 B
const RX_FIS_OFF: usize = 0x400;   // received FIS (256 B)
const CMD_TBL_OFF: usize = 0x500;  // command table: CFIS @ +0, PRDT @ +0x80
const PRDT_OFF: usize = CMD_TBL_OFF + 0x80;
const DATA_OFF: usize = 0x1000;    // data buffer (one page)

const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_EXT: u8 = 0xEA;

/// Bounded I/O retry (Phase H): a transient command error (bad/marginal sector, controller
/// hiccup) is retried this many times — with port error-recovery between attempts — before the
/// op is reported as failed. Bounded (§26.6); never an infinite retry loop.
const MAX_IO_ATTEMPTS: u32 = 3;

struct Ahci<'a> {
    hba: &'a Mmio,
    arena: Dma,
    port: u32,
    /// Total addressable 512-byte sectors, from IDENTIFY. Served on OP_CAPACITY.
    sectors: Cell<u64>,
    /// Test-only (`io-error-test` build): force the next N read/write commands to fail, to
    /// exercise the retry + recovery path. Always 0 in production.
    forced_fails: Cell<u32>,
}

impl<'a> Ahci<'a> {
    fn preg(&self, off: usize) -> usize {
        PORT_BASE + (self.port as usize) * PORT_STRIDE + off
    }
    fn pread(&self, off: usize) -> u32 {
        self.hba.read32(self.preg(off))
    }
    fn pwrite(&self, off: usize, v: u32) {
        self.hba.write32(self.preg(off), v);
    }

    /// Stop the port, point it at our command list / FIS in the arena, restart it.
    fn init_port(&self) {
        // Stop: clear ST + FRE, wait for CR + FR to clear.
        let cmd = self.pread(PX_CMD);
        self.pwrite(PX_CMD, cmd & !(CMD_ST | CMD_FRE));
        for _ in 0..1_000_000u32 {
            if self.pread(PX_CMD) & (CMD_CR | CMD_FR) == 0 {
                break;
            }
        }
        // Program the command-list + received-FIS base (physical addresses).
        let cl = self.arena.phys_at(CMD_LIST_OFF);
        self.pwrite(PX_CLB, cl as u32);
        self.pwrite(PX_CLBU, (cl >> 32) as u32);
        let fb = self.arena.phys_at(RX_FIS_OFF);
        self.pwrite(PX_FB, fb as u32);
        self.pwrite(PX_FBU, (fb >> 32) as u32);
        // Clear any latched error/interrupt state.
        self.pwrite(PX_SERR, 0xFFFF_FFFF);
        self.pwrite(PX_IS, 0xFFFF_FFFF);
        // Start: FRE first, then ST.
        let cmd = self.pread(PX_CMD);
        self.pwrite(PX_CMD, cmd | CMD_FRE);
        let cmd = self.pread(PX_CMD);
        self.pwrite(PX_CMD, cmd | CMD_ST);
    }

    /// Issue a single-PRDT command (slot 0) transferring `data_bytes` to/from the
    /// arena's data buffer. Builds the command header, command table FIS, and PRDT.
    fn issue(
        &self,
        ata_cmd: u8,
        lba: u64,
        count: u16,
        write: bool,
        data_bytes: u32,
    ) -> Result<(), &'static str> {
        // Wait until the port is idle (BSY + DRQ clear).
        let mut idle = false;
        for _ in 0..2_000_000u32 {
            if self.pread(PX_TFD) & (TFD_BSY | TFD_DRQ) == 0 {
                idle = true;
                break;
            }
        }
        if !idle {
            return Err("port busy before issue");
        }

        // Command header[0]: CFL=5 dwords (H2D register FIS); PRDTL=1 if there's
        // data, 0 for no-data commands (e.g. FLUSH).
        let prdtl = if data_bytes > 0 { 1u32 } else { 0 };
        let dw0 = 5u32 | (if write { 1 << 6 } else { 0 }) | (prdtl << 16);
        self.arena.write32(CMD_LIST_OFF, dw0);
        self.arena.write32(CMD_LIST_OFF + 4, 0); // PRDBC
        let ctba = self.arena.phys_at(CMD_TBL_OFF);
        self.arena.write32(CMD_LIST_OFF + 8, ctba as u32);
        self.arena.write32(CMD_LIST_OFF + 12, (ctba >> 32) as u32);

        // Command table — H2D Register FIS (clear the 64-byte CFIS area first).
        for i in 0..16 {
            self.arena.write32(CMD_TBL_OFF + i * 4, 0);
        }
        let dev: u32 = if ata_cmd == ATA_IDENTIFY { 0 } else { 0x40 }; // 0x40 = LBA
        // DW0: type 0x27 | C(byte1 bit7)=0x80 | command | featurel(0)
        self.arena.write32(CMD_TBL_OFF, 0x27 | (0x80 << 8) | ((ata_cmd as u32) << 16));
        // DW1: lba[7:0] | lba[15:8] | lba[23:16] | device
        self.arena.write32(
            CMD_TBL_OFF + 4,
            (lba as u32 & 0xFF)
                | (((lba >> 8) as u32 & 0xFF) << 8)
                | (((lba >> 16) as u32 & 0xFF) << 16)
                | (dev << 24),
        );
        // DW2: lba[31:24] | lba[39:32] | lba[47:40] | featureh(0)
        self.arena.write32(
            CMD_TBL_OFF + 8,
            ((lba >> 24) as u32 & 0xFF)
                | (((lba >> 32) as u32 & 0xFF) << 8)
                | (((lba >> 40) as u32 & 0xFF) << 16),
        );
        // DW3: count[7:0] | count[15:8]
        self.arena.write32(CMD_TBL_OFF + 12, (count as u32 & 0xFF) | (((count >> 8) as u32 & 0xFF) << 8));

        // PRDT[0]: data base + (byte count - 1). Only for data commands.
        if data_bytes > 0 {
            let dba = self.arena.phys_at(DATA_OFF);
            self.arena.write32(PRDT_OFF, dba as u32);
            self.arena.write32(PRDT_OFF + 4, (dba >> 32) as u32);
            self.arena.write32(PRDT_OFF + 8, 0);
            self.arena.write32(PRDT_OFF + 12, data_bytes - 1);
        }

        // Issue command slot 0 and wait for it to clear.
        self.pwrite(PX_CI, 1);
        for _ in 0..5_000_000u32 {
            if self.pread(PX_CI) & 1 == 0 {
                // Check the task-file error bit.
                if self.pread(PX_TFD) & 1 != 0 {
                    return Err("ATA error (TFD.ERR)");
                }
                return Ok(());
            }
        }
        Err("command timeout (CI stuck)")
    }

    /// Clear the port's error state so a retried command can run: clear PxSERR + PxIS
    /// (write-1-to-clear), and if the command engine halted on the error (CR clear while ST is
    /// still set), restart it (toggle ST). Best-effort — used only on the retry path.
    fn recover_port(&self) {
        self.pwrite(PX_SERR, 0xFFFF_FFFF); // W1C all SATA error bits
        self.pwrite(PX_IS, 0xFFFF_FFFF);   // W1C all port interrupt-status bits
        let cmd = self.pread(PX_CMD);
        if cmd & CMD_ST != 0 && cmd & CMD_CR == 0 {
            // Engine halted on the error — stop fully, then restart.
            self.pwrite(PX_CMD, cmd & !CMD_ST);
            for _ in 0..1_000_000u32 {
                if self.pread(PX_CMD) & CMD_CR == 0 { break; }
            }
            self.pwrite(PX_SERR, 0xFFFF_FFFF);
            let cmd2 = self.pread(PX_CMD);
            self.pwrite(PX_CMD, cmd2 | CMD_ST);
        }
    }

    /// Test-only fault injection (`io-error-test` build): force a read/write command to fail so
    /// the retry path runs. Returns `Some(Err)` to inject, `None` to issue for real. A no-op
    /// (always `None`) in production.
    #[cfg(feature = "io-error-test")]
    fn maybe_inject(&self, ata_cmd: u8) -> Option<Result<(), &'static str>> {
        if self.forced_fails.get() > 0 && (ata_cmd == ATA_READ_DMA_EXT || ata_cmd == ATA_WRITE_DMA_EXT) {
            self.forced_fails.set(self.forced_fails.get() - 1);
            return Some(Err("injected I/O error (io-error-test)"));
        }
        None
    }
    #[cfg(not(feature = "io-error-test"))]
    fn maybe_inject(&self, _ata_cmd: u8) -> Option<Result<(), &'static str>> { None }

    /// Issue an ATA command with **bounded retries + port recovery** (Phase H). A transient
    /// error is recovered transparently (logged so it's visible); a persistent one is reported
    /// loudly (§3.12) and returns Err. All data read/write/zero commands go through here.
    fn issue_io(&self, ctx: &ServiceContext, op: &str, ata_cmd: u8, lba: u64,
                count: u16, write: bool, data_bytes: u32) -> Result<(), &'static str> {
        let mut last = "unknown error";
        for attempt in 1..=MAX_IO_ATTEMPTS {
            let r = self.maybe_inject(ata_cmd)
                .unwrap_or_else(|| self.issue(ata_cmd, lba, count, write, data_bytes));
            match r {
                Ok(()) => {
                    if attempt > 1 {
                        ctx.log_fmt(format_args!(
                            "block-driver: {} lba {} recovered after {} attempt(s)", op, lba, attempt));
                    }
                    return Ok(());
                }
                Err(e) => {
                    last = e;
                    ctx.log_fmt(format_args!(
                        "block-driver: {} lba {} failed (attempt {}/{}): {} — recovering, will retry",
                        op, lba, attempt, MAX_IO_ATTEMPTS, e));
                    self.recover_port();
                }
            }
        }
        ctx.log_fmt(format_args!(
            "block-driver: {} lba {} FAILED after {} attempts: {} (reporting error, §3.12)",
            op, lba, MAX_IO_ATTEMPTS, last));
        Err(last)
    }

    /// IDENTIFY DEVICE → (model string bytes, total sectors).
    fn identify(&self) -> Result<([u8; 40], u64), &'static str> {
        self.issue(ATA_IDENTIFY, 0, 0, false, 512)?;
        // Model: words 27..47, each word's two bytes ATA-swapped.
        let mut model = [b' '; 40];
        for w in 0..20 {
            let word = self.arena.read16(DATA_OFF + (27 + w) * 2);
            model[w * 2] = (word >> 8) as u8;
            model[w * 2 + 1] = (word & 0xFF) as u8;
        }
        // LBA48 sector count: words 100..104. Fall back to LBA28 (words 60..62).
        let lba48 = (self.arena.read16(DATA_OFF + 100 * 2) as u64)
            | ((self.arena.read16(DATA_OFF + 101 * 2) as u64) << 16)
            | ((self.arena.read16(DATA_OFF + 102 * 2) as u64) << 32)
            | ((self.arena.read16(DATA_OFF + 103 * 2) as u64) << 48);
        let sectors = if lba48 != 0 {
            lba48
        } else {
            (self.arena.read16(DATA_OFF + 60 * 2) as u64)
                | ((self.arena.read16(DATA_OFF + 61 * 2) as u64) << 16)
        };
        Ok((model, sectors))
    }

    /// Read one 512-byte sector at `lba` into `out` (READ DMA EXT), with bounded retry.
    fn read_block(&self, ctx: &ServiceContext, lba: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
        self.issue_io(ctx, "read", ATA_READ_DMA_EXT, lba, 1, false, 512)?;
        for i in 0..128 {
            let w = self.arena.read32(DATA_OFF + i * 4);
            out[i * 4] = w as u8;
            out[i * 4 + 1] = (w >> 8) as u8;
            out[i * 4 + 2] = (w >> 16) as u8;
            out[i * 4 + 3] = (w >> 24) as u8;
        }
        Ok(())
    }

    /// Write one 512-byte sector of `data` to `lba` (WRITE DMA EXT + FLUSH), with bounded retry.
    fn write_block(&self, ctx: &ServiceContext, lba: u64, data: &[u8; 512]) -> Result<(), &'static str> {
        for i in 0..128 {
            let w = (data[i * 4] as u32)
                | ((data[i * 4 + 1] as u32) << 8)
                | ((data[i * 4 + 2] as u32) << 16)
                | ((data[i * 4 + 3] as u32) << 24);
            self.arena.write32(DATA_OFF + i * 4, w);
        }
        self.issue_io(ctx, "write", ATA_WRITE_DMA_EXT, lba, 1, true, 512)?;
        // Commit to the medium so writes survive a reboot (no-data command).
        self.issue_io(ctx, "flush", ATA_FLUSH_EXT, 0, 0, false, 0)?;
        Ok(())
    }

    /// Write `count` zeroed sectors from `lba`, batched into multi-sector WRITE DMA EXT
    /// commands (up to MAX_PER sectors each — bounded by the DMA arena's data area). One
    /// IPC call zeros a whole run, so `fs` can clear a big bitmap without per-block traffic.
    fn write_zeros(&self, ctx: &ServiceContext, lba: u64, count: u64) -> Result<(), &'static str> {
        if count == 0 {
            return Ok(());
        }
        const MAX_PER: u64 = 64; // 32 KiB — fits the arena's data area (DATA_OFF..64 KiB)
        // Zero the data buffer once; it stays zero across the batched commands.
        for i in 0..(MAX_PER as usize * 512 / 4) {
            self.arena.write32(DATA_OFF + i * 4, 0);
        }
        let mut lba = lba;
        let mut left = count;
        while left > 0 {
            let n = left.min(MAX_PER);
            self.issue_io(ctx, "write-zeros", ATA_WRITE_DMA_EXT, lba, n as u16, true, (n * 512) as u32)?;
            lba += n;
            left -= n;
        }
        self.issue_io(ctx, "flush", ATA_FLUSH_EXT, 0, 0, false, 0)?;
        Ok(())
    }

    /// Serve one block-IPC request (same protocol as the ATA PIO backend),
    /// replying through the client's `reply` cap.
    fn serve(&self, ctx: &ServiceContext, p: &[u8], reply: CapHandle) {
        use super::{OP_CAPACITY, OP_READ_BLOCK, OP_WRITE_BLOCK, OP_WRITE_ZEROS, STATUS_ERR, STATUS_OK};
        let err = |ctx: &ServiceContext| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[STATUS_ERR])); };
        if p.is_empty() {
            err(ctx);
            return;
        }
        // OP_CAPACITY carries no LBA; read/write carry [op, lba:u64 LE, (write: 512 data)].
        // The LBA is u64 so GSFS's u64 fields reach the device unchanged (persistence §6.3).
        if p[0] == OP_CAPACITY {
            let mut out = [0u8; 9];
            out[0] = STATUS_OK;
            out[1..9].copy_from_slice(&self.sectors.get().to_le_bytes());
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
            return;
        }
        if p.len() < 9 {
            err(ctx);
            return;
        }
        let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
        match p[0] {
            OP_READ_BLOCK => {
                let mut out = [0u8; 1 + 512];
                let mut sec = [0u8; 512];
                match self.read_block(ctx, lba, &mut sec) {
                    Ok(()) => {
                        out[0] = STATUS_OK;
                        out[1..].copy_from_slice(&sec);
                        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
                    }
                    Err(_) => err(ctx),
                }
            }
            OP_WRITE_BLOCK => {
                if p.len() < 9 + 512 {
                    err(ctx);
                    return;
                }
                let mut sec = [0u8; 512];
                sec.copy_from_slice(&p[9..9 + 512]);
                let status = match self.write_block(ctx, lba, &sec) {
                    Ok(()) => STATUS_OK,
                    Err(_) => STATUS_ERR,
                };
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
            }
            OP_WRITE_ZEROS => {
                // [op, lba:u64, count:u64] — zero `count` blocks from `lba`.
                if p.len() < 17 {
                    err(ctx);
                    return;
                }
                let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
                let status = match self.write_zeros(ctx, lba, count) {
                    Ok(()) => STATUS_OK,
                    Err(_) => STATUS_ERR,
                };
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
            }
            _ => err(ctx),
        }
    }
}

/// Steps A+B: detect the HBA + disk, init the port, IDENTIFY. Idles afterwards.
pub fn run(ctx: &ServiceContext, hba: &Mmio) -> ! {
    let cap = hba.read32(HBA_CAP);
    let vs = hba.read32(HBA_VS);
    let mut ghc = hba.read32(HBA_GHC);
    if ghc & GHC_AE == 0 {
        hba.write32(HBA_GHC, ghc | GHC_AE);
        ghc = hba.read32(HBA_GHC);
    }
    let pi = hba.read32(HBA_PI);
    ctx.log_fmt(format_args!(
        "block-driver: AHCI HBA v{:x}.{:02x} CAP={:#010x} ({} ports, {} cmd slots) GHC={:#x} PI={:#010x}",
        (vs >> 16) & 0xffff, (vs >> 8) & 0xff, cap, (cap & 0x1F) + 1, ((cap >> 8) & 0x1F) + 1, ghc, pi
    ));

    let mut disk_port = None;
    for p in 0..32u32 {
        if pi & (1 << p) == 0 {
            continue;
        }
        let base = PORT_BASE + (p as usize) * PORT_STRIDE;
        if hba.read32(base + PX_SSTS) & 0xF == 3 {
            let sig = hba.read32(base + PX_SIG);
            ctx.log_fmt(format_args!(
                "block-driver: AHCI port {}: device present (DET=3) sig={:#010x}{}",
                p, sig, if sig == SIG_SATA { " — SATA disk" } else { "" }
            ));
            if sig == SIG_SATA && disk_port.is_none() {
                disk_port = Some(p);
            }
        }
    }

    let port = match disk_port {
        Some(p) => p,
        None => {
            ctx.log("block-driver: AHCI — no SATA disk found on any implemented port");
            loop { ctx.yield_cpu(); }
        }
    };

    let arena = match ctx.dma_region() {
        Some(d) => d,
        None => {
            ctx.log("block-driver: AHCI — no DMA arena granted");
            loop { ctx.yield_cpu(); }
        }
    };
    arena.zero();
    // `io-error-test` build: arm a few forced read/write failures so the boot self-test +
    // mount exercise the retry/recovery path. Always 0 (no injection) in production.
    let forced = if cfg!(feature = "io-error-test") { 2 } else { 0 };
    let ahci = Ahci { hba, arena, port, sectors: Cell::new(0), forced_fails: Cell::new(forced) };
    ahci.init_port();

    match ahci.identify() {
        Ok((model, sectors)) => {
            ahci.sectors.set(sectors); // served on OP_CAPACITY so `fs` sizes a flash to the disk
            let model_str = core::str::from_utf8(&model).unwrap_or("?");
            let mib = sectors / 2048; // 512-byte sectors → MiB
            ctx.log_fmt(format_args!(
                "block-driver: AHCI port {} IDENTIFY OK — model='{}' sectors={} ({} MiB)",
                port, model_str.trim_end(), sectors, mib
            ));
        }
        Err(e) => ctx.log_fmt(format_args!("block-driver: AHCI IDENTIFY FAILED: {}", e)),
    }

    // Boot self-test: read sector 0 (proves the AHCI read/DMA path on this disk —
    // the key thing to confirm on real hardware). Non-destructive; write is proven
    // by the fs file round-trip (and by `drives flash` later on hardware).
    let mut s0 = [0u8; 512];
    match ahci.read_block(ctx, 0, &mut s0) {
        Ok(()) => ctx.log_fmt(format_args!(
            "block-driver: AHCI read self-test OK — sector 0 [{:02x} {:02x} {:02x} {:02x} …]",
            s0[0], s0[1], s0[2], s0[3]
        )),
        Err(e) => ctx.log_fmt(format_args!("block-driver: AHCI read self-test FAILED: {}", e)),
    }

    // Path C (Phase 4): no self-registration — the kernel name-directory records "block-driver"
    // at spawn (refreshed on restart), so `fs` reacquires us by name via the directory (§14.3).

    // Serve block read/write requests from `fs` over IPC (READ/WRITE DMA EXT).
    ctx.log("block-driver: AHCI serving block I/O");
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() {
            Some(c) => c,
            None => continue,
        };
        ahci.serve(ctx, msg.payload_bytes(), reply);
        ctx.remove_cap(reply);
    }
}
