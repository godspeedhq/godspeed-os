// SPDX-License-Identifier: GPL-2.0-only
//! ACPI soft-off (S5): the x86 answer to `power_off` (`docs/shutdown.md` 5-6).
//!
//! Turning a PC off is not a wire, as reset is: it is ACPI sleep state S5. The firmware publishes, in its
//! tables, WHICH port controls the power state (`PM1a_CNT_BLK`, and sometimes `PM1b`, in the FADT) and
//! WHICH value means "off" (`SLP_TYPa` / `SLP_TYPb`, the first two elements of the `\_S5` package in the
//! DSDT). Writing that value with `SLP_EN` set powers the machine down.
//!
//! **What this does not do, said where it would otherwise be assumed.** The ACPI specification asks the OS
//! to run the firmware's `\_PTS` (and on older machines `\_GTS`) methods first. Those are ACPI bytecode,
//! and running them needs an interpreter this kernel does not have. Many machines power off without them
//! (QEMU does); the HP T630 does not (`utilities/14_poweroff.md`). So `power_off` writes S5, waits a
//! moment, and if the machine is still running it says why and returns - the caller then halts every
//! core, and the last line says it is safe to switch off. The `\_S5` package is read by a byte scan of
//! the DSDT, not by running it, which is the one piece of AML this needs and the common way it is done.
//!
//! **Each table is read as one bounds-checked slice.** The only `unsafe` in the walk is turning a table's
//! address into a slice of its own stated length (`region`, which `table` calls with the header's length); every field and the whole `\_S5` scan are then
//! ordinary indexing, so a malformed table is an `Err`, never an out-of-bounds read.
//!
//! The tables are walked when `power_off` is called, not at boot: the only thing kept from boot is where
//! the RSDP is, which `iommu::detect` already receives and passes here.

use core::sync::atomic::{AtomicU64, Ordering};

/// The system-table header, as `iommu` reads it.
const SDT_HEADER_LEN: usize = super::iommu::SDT_HEADER_LEN as usize;
/// No firmware table this reads is anywhere near this long; a length beyond it is corrupt, not large.
const TABLE_MAX: usize = 1 << 20;

/// Where the RSDP is, and the higher-half direct map base, as Limine supplied them. Written once on the
/// boot path (`remember`) and read when the machine is asked to turn off.
static RSDP: AtomicU64 = AtomicU64::new(0);
static HHDM: AtomicU64 = AtomicU64::new(0);

/// Keep where the ACPI tables start, for `power_off`. Called from `iommu::detect`, which walks the same
/// tables at boot.
pub fn remember(rsdp_addr: u64, hhdm: u64) {
    RSDP.store(rsdp_addr, Ordering::Relaxed);
    HHDM.store(hhdm, Ordering::Relaxed);
}

/// What the firmware published for S5.
struct S5 {
    pm1a_cnt: u16,
    pm1b_cnt: u16,
    slp_typa: u16,
    slp_typb: u16,
    smi_cmd: u16,
    acpi_enable: u8,
}

fn virt(phys: u64) -> u64 {
    let hhdm = HHDM.load(Ordering::Relaxed);
    if phys < hhdm { phys + hhdm } else { phys }
}

/// `n` bytes at the physical address `phys`, as a slice: the RSDP (whose length is fixed by its revision)
/// or a system table (whose length is in its own header, at offset 4). `None` for a null address or a
/// length that cannot be a real table.
fn region(phys: u64, n: usize) -> Option<&'static [u8]> {
    if phys == 0 || n == 0 || n > TABLE_MAX {
        return None;
    }
    // SAFETY: `phys` is an address the firmware's own ACPI tables gave (the RSDP from Limine, every other
    // table from a table before it), and Limine maps every ACPI region in the HHDM for the life of the
    // kernel; `n` is that table's stated length, bounded by TABLE_MAX. Nothing writes these tables.
    Some(unsafe { core::slice::from_raw_parts(virt(phys) as *const u8, n) })
}

/// A whole system table, by its header's length.
fn table(phys: u64) -> Option<&'static [u8]> {
    let head = region(phys, SDT_HEADER_LEN)?;
    let len = le(head, 4, 4)? as usize;
    if len < SDT_HEADER_LEN { return None; }
    region(phys, len)
}

/// A little-endian field of `n` bytes (at most 8) at `off`, or `None` past the end.
fn le(t: &[u8], off: usize, n: usize) -> Option<u64> {
    let f = t.get(off..off.checked_add(n)?)?;
    Some(f.iter().rev().fold(0u64, |v, &b| (v << 8) | b as u64))
}

/// Find the FADT ("FACP") and read what S5 needs from it and from the DSDT.
fn find_s5() -> Result<S5, &'static str> {
    let rsdp_phys = RSDP.load(Ordering::Relaxed);
    let rsdp = region(rsdp_phys, 20).ok_or("the bootloader gave no ACPI tables")?;
    if &rsdp[..8] != b"RSD PTR " {
        return Err("the RSDP signature is wrong");
    }
    let (sdt_phys, entry) = if rsdp[15] >= 2 {
        let r2 = region(rsdp_phys, 36).ok_or("the RSDP is too short for its revision")?;
        (le(r2, 24, 8).unwrap_or(0), 8)
    } else {
        (le(rsdp, 16, 4).unwrap_or(0), 4)
    };
    let sdt = table(sdt_phys).ok_or("the RSDP names no readable RSDT or XSDT")?;
    let fadt = sdt[SDT_HEADER_LEN..]
        .chunks_exact(entry)
        .filter_map(|e| le(e, 0, entry))
        .filter_map(table)
        .find(|t| &t[..4] == b"FACP")
        .ok_or("there is no FADT")?;
    let pm1a = le(fadt, 64, 4).ok_or("the FADT is too short to name a PM1 control block")?;
    if pm1a == 0 || pm1a > 0xFFFF {
        return Err("the FADT names no PM1a control port");
    }
    let pm1b = le(fadt, 68, 4).unwrap_or(0);
    let smi_cmd = le(fadt, 48, 4).unwrap_or(0);
    let acpi_enable = le(fadt, 52, 1).unwrap_or(0) as u8;
    // X_DSDT (offset 140, 64-bit) wins where the FADT carries it and it is set.
    let dsdt_phys = match le(fadt, 140, 8) { Some(x) if x != 0 => x, _ => le(fadt, 40, 4).unwrap_or(0) };
    let dsdt = table(dsdt_phys).ok_or("the FADT names no readable DSDT")?;
    let (a, b) = scan_s5(dsdt).ok_or("the DSDT has no \\_S5 package")?;
    Ok(S5 {
        pm1a_cnt: pm1a as u16,
        pm1b_cnt: if pm1b <= 0xFFFF { pm1b as u16 } else { 0 },
        slp_typa: a,
        slp_typb: b,
        smi_cmd: if smi_cmd <= 0xFFFF { smi_cmd as u16 } else { 0 },
        acpi_enable,
    })
}

/// Find `Name(\_S5, Package() { a, b, .. })` in the DSDT's bytecode and return `a` and `b`.
///
/// A scan, not an interpreter: the name `_S5_` preceded by `NameOp` (0x08, optionally with a root `\`),
/// then `PackageOp` (0x12), its length (one to four bytes, the count of extra bytes in the first byte's top
/// two bits), the element count, and the two values - each a `BytePrefix` (0x0A) and a byte, or a bare
/// `Zero` (0x00) / `One` (0x01) opcode. `Ones` (0xFF) is read as 0 and any other byte as the value itself, so a
/// `\_S5` written with a Word or DWord prefix is not handled.
fn scan_s5(dsdt: &[u8]) -> Option<(u16, u16)> {
    let aml = dsdt.get(SDT_HEADER_LEN..)?;
    for i in 2..aml.len().saturating_sub(4) {
        if &aml[i..i + 4] != b"_S5_" { continue; }
        let named = aml[i - 1] == 0x08 || (aml[i - 2] == 0x08 && aml[i - 1] == b'\\');
        if !named || *aml.get(i + 4)? != 0x12 { continue; }
        let extra = (*aml.get(i + 5)? >> 6) as usize;
        let mut q = i + 5 + 1 + extra + 1; // past the package length and the element count
        let mut vals = [0u16; 2];
        for v in vals.iter_mut() {
            let op = *aml.get(q)?;
            if op == 0x0A {
                *v = *aml.get(q + 1)? as u16;
                q += 2;
            } else {
                *v = if op == 0xFF { 0 } else { op as u16 };
                q += 1;
            }
        }
        return Some((vals[0], vals[1]));
    }
    None
}

const SCI_EN: u16 = 1 << 0;
const SLP_EN: u16 = 1 << 13;
const SLP_TYP_MASK: u16 = 0b111 << 10;

/// Write S5. Returns only if the machine is still running afterwards, with the reason.
pub fn power_off() -> &'static str {
    let s5 = match find_s5() {
        Ok(s) => s,
        Err(why) => return why,
    };
    crate::kprintln!("power-off: ACPI S5 - SLP_TYPa {} on PM1a_CNT {:#06x}{}", s5.slp_typa, s5.pm1a_cnt,
                     if s5.pm1b_cnt != 0 { " (and PM1b)" } else { "" });
    let inw = |port: u16| -> u16 {
        let v: u16;
        // SAFETY: a 16-bit read of a port this machine's FADT named as a PM1 control block.
        unsafe { core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags)) };
        v
    };
    let outw = |port: u16, v: u16| {
        // SAFETY: a 16-bit write of the sleep type to a port the FADT named for exactly that.
        unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) };
    };
    // SAFETY: interrupts off on this core so nothing runs between the writes below; the machine is going
    // down either way, and the caller halts every core if it does not.
    unsafe { core::arch::asm!("cli", options(nomem, nostack)) };
    // A machine booted in legacy mode has SCI_EN clear and ignores SLP_EN until ACPI is enabled through
    // SMI_CMD. Bounded: a firmware that never sets SCI_EN gets the write anyway.
    if inw(s5.pm1a_cnt) & SCI_EN == 0 && s5.smi_cmd != 0 && s5.acpi_enable != 0 {
        // SAFETY: an 8-bit write of the FADT's ACPI_ENABLE value to the FADT's SMI_CMD port - the
        // specification's handshake for handing the chipset to the OS.
        unsafe { core::arch::asm!("out dx, al", in("dx") s5.smi_cmd, in("al") s5.acpi_enable, options(nomem, nostack, preserves_flags)) };
        let mut n = 0u32;
        while inw(s5.pm1a_cnt) & SCI_EN == 0 && n < 3_000_000 {
            core::hint::spin_loop();
            n += 1;
        }
    }
    outw(s5.pm1a_cnt, (inw(s5.pm1a_cnt) & !SLP_TYP_MASK) | ((s5.slp_typa & 7) << 10) | SLP_EN);
    if s5.pm1b_cnt != 0 {
        outw(s5.pm1b_cnt, (inw(s5.pm1b_cnt) & !SLP_TYP_MASK) | ((s5.slp_typb & 7) << 10) | SLP_EN);
    }
    // The power should be gone within microseconds. A generous bounded wait, then the honest answer.
    for _ in 0..50_000_000u64 {
        core::hint::spin_loop();
    }
    "ACPI S5 was written and the machine is still running - its firmware wants \\_PTS run first, \
     which is ACPI bytecode this kernel does not interpret"
}
