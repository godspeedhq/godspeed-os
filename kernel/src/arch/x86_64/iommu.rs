//! AMD-Vi IOMMU detection (H1 Phase 0) — DMA confinement feasibility probe.
//!
//! GodspeedOS has no IOMMU today, so a DMA-capable driver (`xhci`, `ehci`) that
//! is programmed with a physical address can make the controller read or write
//! *anywhere* in physical RAM — kernel-equivalent power. That is why those
//! drivers are still in the TCB (§6.1). The flagship hardening item H1 is to put
//! an AMD-Vi IOMMU translation domain in front of each driver so it can only
//! touch its own granted arena, then drop it from the TCB.
//!
//! That is a large, hardware-specific subsystem. Before building it we must know
//! whether this machine even exposes a usable AMD-Vi IOMMU — embedded G-series
//! APUs vary and firmware often disables it. This module does **detection only**:
//! it walks the ACPI tables (RSDP → RSDT/XSDT → IVRS) and reports whether an
//! IVRS table exists and the IOMMU MMIO base it advertises. No behaviour change,
//! loud output (§3.12). Phase 1 (translation setup) is gated on this saying yes.
//!
//! ACPI table access is hardware/firmware memory, so this lives in the arch
//! layer (§18.1). Every raw read carries a SAFETY argument.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Set true once an IVRS table with at least one IVHD block has been found.
pub static IOMMU_PRESENT: AtomicBool = AtomicBool::new(false);
/// MMIO base address of the first IOMMU described by IVRS (0 if none).
pub static IOMMU_MMIO_BASE: AtomicU64 = AtomicU64::new(0);

/// ACPI System Description Table header — the common 36-byte prefix on every
/// ACPI table (RSDT, XSDT, IVRS, …).
const SDT_HEADER_LEN: u64 = 36;

/// Translate a physical address to its HHDM virtual address.
#[inline]
fn phys_to_virt(phys: u64, hhdm: u64) -> u64 {
    hhdm.wrapping_add(phys)
}

/// Read `N` bytes from a HHDM-mapped virtual address into a fixed buffer.
///
/// # Safety
/// `virt` must point at `N` readable bytes inside the HHDM (ACPI tables Limine
/// kept mapped). Used only for firmware tables during single-threaded boot.
#[inline]
unsafe fn read_bytes<const N: usize>(virt: u64) -> [u8; N] {
    let mut buf = [0u8; N];
    // SAFETY: caller guarantees `virt` covers N mapped bytes; copy is in-bounds.
    unsafe { core::ptr::copy_nonoverlapping(virt as *const u8, buf.as_mut_ptr(), N) };
    buf
}

/// Read a little-endian u32 at a HHDM virtual address.
///
/// # Safety
/// As [`read_bytes`]; `virt..virt+4` must be mapped.
#[inline]
unsafe fn read32(virt: u64) -> u32 {
    // SAFETY: caller guarantees 4 mapped bytes at `virt`.
    u32::from_le_bytes(unsafe { read_bytes::<4>(virt) })
}

/// Read a little-endian u64 at a HHDM virtual address.
///
/// # Safety
/// As [`read_bytes`]; `virt..virt+8` must be mapped.
#[inline]
unsafe fn read64(virt: u64) -> u64 {
    // SAFETY: caller guarantees 8 mapped bytes at `virt`.
    u64::from_le_bytes(unsafe { read_bytes::<8>(virt) })
}

/// Probe ACPI for an AMD-Vi IOMMU (IVRS). Logs the outcome loudly either way.
///
/// `rsdp_addr` is the pointer Limine supplied; `hhdm` is the higher-half direct
/// map base. On Limine base revision 6 `rsdp_addr` is already a virtual address;
/// we normalise defensively so a physical address would also work.
pub fn detect(rsdp_addr: u64, hhdm: u64) {
    if rsdp_addr == 0 {
        crate::kprintln!("iommu: no ACPI RSDP from bootloader; IOMMU detection skipped");
        return;
    }

    // Normalise to a virtual address: a value below the HHDM base is physical.
    let rsdp = if rsdp_addr < hhdm { phys_to_virt(rsdp_addr, hhdm) } else { rsdp_addr };

    // SAFETY: rsdp points at the firmware RSDP, which Limine keeps mapped in the
    // HHDM. The RSDP is at least 20 bytes (ACPI 1.0) / 36 bytes (2.0+).
    let sig = unsafe { read_bytes::<8>(rsdp) };
    if &sig != b"RSD PTR " {
        crate::kprintln!("iommu: RSDP signature invalid; IOMMU detection aborted");
        return;
    }

    // Revision byte at offset 15: 0 => ACPI 1.0 (RSDT, 32-bit), 2 => 2.0+ (XSDT).
    // SAFETY: RSDP is mapped; offset 15 is within the 20-byte ACPI 1.0 RSDP.
    let revision = unsafe { read_bytes::<1>(rsdp + 15) }[0];

    let (sdt_phys, entry_size) = if revision >= 2 {
        // XSDT physical address at offset 24 (8 bytes), entries are 8 bytes.
        // SAFETY: ACPI 2.0+ RSDP is 36 bytes; offset 24..32 is in range.
        (unsafe { read64(rsdp + 24) }, 8u64)
    } else {
        // RSDT physical address at offset 16 (4 bytes), entries are 4 bytes.
        // SAFETY: offset 16..20 is within the 20-byte ACPI 1.0 RSDP.
        ((unsafe { read32(rsdp + 16) }) as u64, 4u64)
    };

    if sdt_phys == 0 {
        crate::kprintln!("iommu: RSDP has no RSDT/XSDT pointer; IOMMU detection aborted");
        return;
    }

    let sdt = phys_to_virt(sdt_phys, hhdm);
    // SDT length (total bytes incl. header) at header offset 4.
    // SAFETY: the RSDT/XSDT is an ACPI table mapped in the HHDM; header is 36 B.
    let sdt_len = (unsafe { read32(sdt + 4) }) as u64;
    if sdt_len < SDT_HEADER_LEN {
        crate::kprintln!("iommu: RSDT/XSDT length {} too small; aborting", sdt_len);
        return;
    }

    let entry_count = (sdt_len - SDT_HEADER_LEN) / entry_size;
    let mut ivrs_phys = 0u64;
    for i in 0..entry_count {
        let entry_ptr = sdt + SDT_HEADER_LEN + i * entry_size;
        let table_phys = if entry_size == 8 {
            // SAFETY: entry_ptr is within [sdt, sdt+sdt_len), all mapped.
            unsafe { read64(entry_ptr) }
        } else {
            // SAFETY: as above; 4-byte entry.
            (unsafe { read32(entry_ptr) }) as u64
        };
        if table_phys == 0 {
            continue;
        }
        let table = phys_to_virt(table_phys, hhdm);
        // SAFETY: each referenced table is an ACPI table mapped in the HHDM with
        // at least a 4-byte signature.
        let tsig = unsafe { read_bytes::<4>(table) };
        if &tsig == b"IVRS" {
            ivrs_phys = table_phys;
            break;
        }
    }

    if ivrs_phys == 0 {
        crate::kprintln!(
            "iommu: no IVRS table in {} ACPI entries -> no AMD-Vi IOMMU on this machine \
             (H1 DMA confinement not available; drivers stay in TCB)",
            entry_count
        );
        return;
    }

    // Found IVRS. Parse the first IVHD block to extract the IOMMU MMIO base.
    let ivrs = phys_to_virt(ivrs_phys, hhdm);
    // SAFETY: IVRS is mapped; its header is the standard 36-byte SDT header and
    // the length field at offset 4 bounds the whole table.
    let ivrs_len = (unsafe { read32(ivrs + 4) }) as u64;

    // IVHD blocks begin after the 36-byte header + 4-byte IVinfo + 8-byte
    // reserved = offset 48. Each IVHD has: type(1) flags(1) length(2)
    // device_id(2) cap_offset(2) iommu_base(8 @ off 8) ...
    let mut off = 48u64;
    let mut base = 0u64;
    while off + 24 <= ivrs_len {
        let block = ivrs + off;
        // SAFETY: block is within [ivrs, ivrs+ivrs_len); ≥24 bytes remain.
        let btype = unsafe { read_bytes::<1>(block) }[0];
        // SAFETY: as above; length at block offset 2.
        let blen = u16::from_le_bytes(unsafe { read_bytes::<2>(block + 2) }) as u64;
        if blen == 0 {
            break; // malformed; stop rather than loop forever
        }
        // IVHD block types describing an IOMMU: 0x10, 0x11, 0x40.
        if btype == 0x10 || btype == 0x11 || btype == 0x40 {
            // SAFETY: IVHD header is ≥24 bytes; iommu_base at block offset 8.
            base = unsafe { read64(block + 8) };
            break;
        }
        off += blen;
    }

    IOMMU_PRESENT.store(true, Ordering::Relaxed);
    IOMMU_MMIO_BASE.store(base, Ordering::Relaxed);
    crate::kprintln!(
        "iommu: AMD-Vi IVRS found (table {:#x}, {} bytes); IOMMU MMIO base {:#x}",
        ivrs_phys, ivrs_len, base
    );
    crate::kprintln!(
        "iommu: H1 Phase 0 OK -> hardware supports DMA confinement; Phase 1 (translation) viable"
    );
}
