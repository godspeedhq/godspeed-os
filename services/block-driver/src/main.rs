//! `block-driver` — userspace ATA PIO disk driver (persistence, v2; §6.3,
//! docs/persistence.md).
//!
//! **Phase 1 (this file):** read sector 0 of the secondary-master disk and log
//! it, proving the capability-mediated port-I/O path end to end. The block I/O
//! interface to `fs` (Read/Write blocks over IPC) comes in later phases.
//!
//! No DMA, no MMIO — pure port I/O through the kernel's `PortRead`/`PortWrite`
//! syscalls (SDK [`Pio`]), each validated against this driver's `hw_pio` grant
//! (ATA secondary channel 0x170-0x177 + 0x376). A PIO driver never points a
//! device at RAM, so it is least-privilege by construction (docs/persistence.md
//! §5.1) — no IOMMU confinement, and a clean path out of the TCB (§6.3).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Pio};

// Secondary ATA channel command block + control port (the `hw_pio` grant).
const ATA_DATA:     u16 = 0x170; // 16-bit data register
const ATA_SECCOUNT: u16 = 0x172;
const ATA_LBA0:     u16 = 0x173;
const ATA_LBA1:     u16 = 0x174;
const ATA_LBA2:     u16 = 0x175;
const ATA_DRIVE:    u16 = 0x176;
const ATA_CMD:      u16 = 0x177; // status (read) / command (write)

const ST_BSY: u8 = 0x80;
const ST_DRQ: u8 = 0x08;
const ST_ERR: u8 = 0x01;

const CMD_READ_SECTORS:  u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_CACHE_FLUSH:   u8 = 0xE7;

/// Scratch sector for the Phase-1 write/read round-trip — well clear of sector 0
/// (the magic) and the start of the data area a real filesystem would use.
const SCRATCH_LBA: u32 = 100;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("block-driver: starting (ATA PIO, secondary master)");
    let pio = ctx.pio();

    // Step 1: read sector 0 and log it.
    let mut sector = [0u8; 512];
    match read_lba(&pio, 0, &mut sector) {
        Ok(()) => {
            log_first16(&ctx, &sector);
            ctx.log("block-driver: sector 0 read OK");
        }
        Err(e) => ctx.log_fmt(format_args!("block-driver: sector 0 read FAILED: {}", e)),
    }

    // Step 2: write a known pattern to a scratch sector, read it back, compare.
    let mut pattern = [0u8; 512];
    for i in 0..512 {
        pattern[i] = (i as u8) ^ 0x5A;
    }
    match round_trip(&pio, SCRATCH_LBA, &pattern) {
        Ok(true) => ctx.log_fmt(format_args!(
            "block-driver: write/read round-trip OK (LBA {})", SCRATCH_LBA)),
        Ok(false) => ctx.log("block-driver: write/read round-trip MISMATCH"),
        Err(e) => ctx.log_fmt(format_args!("block-driver: round-trip FAILED: {}", e)),
    }

    // Phase 1 has no IPC loop yet; stay alive (and observable) idling.
    loop {
        ctx.yield_cpu();
    }
}

/// Write `data` to `lba`, read it back, and report whether the bytes match.
fn round_trip(pio: &Pio, lba: u32, data: &[u8; 512]) -> Result<bool, &'static str> {
    write_lba(pio, lba, data)?;
    let mut back = [0u8; 512];
    read_lba(pio, lba, &mut back)?;
    Ok(&back[..] == &data[..])
}

/// Poll the status port until BSY clears, bounded. Returns the last status byte.
fn wait_not_busy(pio: &Pio) -> Result<u8, &'static str> {
    for _ in 0..1_000_000u32 {
        let st = pio.read8(ATA_CMD).ok_or("port denied")?;
        if st == 0xFF {
            return Err("no drive (status 0xFF)");
        }
        if st & ST_BSY == 0 {
            return Ok(st);
        }
    }
    Err("timeout waiting BSY")
}

/// Program the LBA28 address registers and issue `cmd` (READ or WRITE SECTORS)
/// for a single sector on the secondary master.
fn issue_lba28(pio: &Pio, lba: u32, cmd: u8) -> Result<(), &'static str> {
    // Drive select: secondary master (0xE0 = LBA mode, drive 0) | LBA[27:24].
    if !pio.write8(ATA_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8) {
        return Err("drive select denied");
    }
    // ~400 ns settle after drive select: read the status port a few times.
    for _ in 0..4 {
        let _ = pio.read8(ATA_CMD);
    }
    wait_not_busy(pio)?;

    pio.write8(ATA_SECCOUNT, 1);
    pio.write8(ATA_LBA0, (lba & 0xFF) as u8);
    pio.write8(ATA_LBA1, ((lba >> 8) & 0xFF) as u8);
    pio.write8(ATA_LBA2, ((lba >> 16) & 0xFF) as u8);
    pio.write8(ATA_CMD, cmd);
    Ok(())
}

/// Spin until the controller clears BSY and asserts DRQ (data transfer ready),
/// or reports an error. Bounded.
fn wait_drq(pio: &Pio) -> Result<(), &'static str> {
    for _ in 0..1_000_000u32 {
        let st = pio.read8(ATA_CMD).ok_or("port denied")?;
        if st == 0xFF {
            return Err("no drive (status 0xFF)");
        }
        if st & ST_ERR != 0 {
            return Err("ATA ERR bit set");
        }
        if st & ST_BSY == 0 && st & ST_DRQ != 0 {
            return Ok(());
        }
    }
    Err("timeout waiting DRQ")
}

/// Read one sector at `lba` from the secondary master into `out`.
fn read_lba(pio: &Pio, lba: u32, out: &mut [u8; 512]) -> Result<(), &'static str> {
    issue_lba28(pio, lba, CMD_READ_SECTORS)?;
    wait_drq(pio)?;
    // Transfer 256 16-bit words = 512 bytes (little-endian per the ATA spec).
    for i in 0..256 {
        let w = pio.read16(ATA_DATA).ok_or("data port denied")?;
        out[i * 2] = (w & 0xFF) as u8;
        out[i * 2 + 1] = (w >> 8) as u8;
    }
    Ok(())
}

/// Write one sector of `data` to `lba` on the secondary master, then flush.
fn write_lba(pio: &Pio, lba: u32, data: &[u8; 512]) -> Result<(), &'static str> {
    issue_lba28(pio, lba, CMD_WRITE_SECTORS)?;
    wait_drq(pio)?;
    // Transfer 256 16-bit words = 512 bytes (little-endian).
    for i in 0..256 {
        let w = (data[i * 2] as u16) | ((data[i * 2 + 1] as u16) << 8);
        if !pio.write16(ATA_DATA, w) {
            return Err("data port denied");
        }
    }
    // FLUSH CACHE: commit the write to the medium before we report success.
    pio.write8(ATA_CMD, CMD_CACHE_FLUSH);
    wait_not_busy(pio)?;
    Ok(())
}

/// Log the first 16 bytes of the sector as hex + an ASCII view, so a magic the
/// host wrote into sector 0 is visible on the serial console.
fn log_first16(ctx: &ServiceContext, sector: &[u8; 512]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut hex = [b' '; 16 * 3];
    for i in 0..16 {
        let b = sector[i];
        hex[i * 3] = HEX[(b >> 4) as usize];
        hex[i * 3 + 1] = HEX[(b & 0xf) as usize];
        // hex[i*3 + 2] stays a space separator
    }
    if let Ok(s) = core::str::from_utf8(&hex) {
        ctx.log_fmt(format_args!("block-driver: sector0[0..16] hex = {}", s));
    }

    let mut ascii = [b'.'; 16];
    for i in 0..16 {
        let b = sector[i];
        ascii[i] = if b.is_ascii_graphic() || b == b' ' { b } else { b'.' };
    }
    if let Ok(s) = core::str::from_utf8(&ascii) {
        ctx.log_fmt(format_args!("block-driver: sector0 ascii = \"{}\"", s));
    }
}
