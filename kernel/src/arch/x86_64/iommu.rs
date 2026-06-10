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

// ===========================================================================
// H1 Phase 1a — IOMMU MMIO bring-up + capability/feature register readout
// ===========================================================================
//
// The IOMMU control interface is a block of memory-mapped registers at the base
// the IVRS advertised. Before building any translation structures we map that
// block (uncached, like the APIC) and read the Extended Feature Register and the
// current control state. This proves the kernel can talk to the IOMMU and tells
// us its capabilities (page-table levels, command/event-log support) — the facts
// the later phases depend on. No structures are programmed yet.

/// MMIO register offsets from the IOMMU base (AMD I/O Virtualization spec).
mod reg {
    pub const DEVICE_TABLE_BASE: u64 = 0x0000;
    pub const COMMAND_BUF_BASE:  u64 = 0x0008;
    pub const EVENT_LOG_BASE:    u64 = 0x0010;
    pub const CONTROL:           u64 = 0x0018;
    pub const EXT_FEATURE:       u64 = 0x0030;
    pub const STATUS:            u64 = 0x2020;
}

/// Kernel virtual address the IOMMU MMIO block is mapped at (HHDM alias,
/// re-mapped uncached). 0 until [`bringup`] runs.
pub static IOMMU_MMIO_VA: AtomicU64 = AtomicU64::new(0);

/// Bytes of MMIO to map. The registers we use span 0x0000..0x2028, so four
/// 4 KiB pages (0x4000) cover the whole control + head/tail register window.
const IOMMU_MMIO_LEN: u64 = 0x4000;

/// Read a 64-bit IOMMU MMIO register at `off`.
///
/// # Safety
/// [`bringup`] must have mapped the MMIO block; `off + 8 <= IOMMU_MMIO_LEN`.
#[inline]
unsafe fn mmio_read64(va: u64, off: u64) -> u64 {
    // SAFETY: va+off is inside the uncached IOMMU MMIO mapping established by
    // bringup; aligned 64-bit volatile read of a hardware register.
    unsafe { core::ptr::read_volatile((va + off) as *const u64) }
}

/// Write a 64-bit IOMMU MMIO register at `off`.
///
/// # Safety
/// [`bringup`] must have mapped the MMIO block; `off + 8 <= IOMMU_MMIO_LEN`.
#[inline]
unsafe fn mmio_write64(va: u64, off: u64, val: u64) {
    // SAFETY: va+off is inside the uncached IOMMU MMIO mapping; aligned 64-bit
    // volatile write of a hardware register.
    unsafe { core::ptr::write_volatile((va + off) as *mut u64, val) }
}

// ---------------------------------------------------------------------------
// Phase 1b — device table, command buffer, event log
// ---------------------------------------------------------------------------
//
// The IOMMU checks every upstream DMA against a Device Table Entry (DTE) indexed
// by the originating device's PCI BDF. We allocate the full 64K-entry table (one
// 256-bit DTE each = 2 MiB) so every device has an entry, default every entry to
// *passthrough* (so the disk and everything else keep DMAing untranslated), and
// later switch just the USB controllers' entries to a confined domain (Phase 1c).
// The command buffer and event log are the IOMMU's two rings: we issue cache
// invalidations through the command buffer and read translation faults from the
// event log.

/// One AMD-Vi Device Table Entry: 256 bits = four little-endian u64 words.
/// Field encoding (AMD I/O Virtualization spec; matches Linux amd_iommu):
///   data[0]: V(0) | TV(1) | (mode<<9) | (page_table_root & 0xF_FFFF_FFFF_F000)
///   data[1]: DomainID[15:0] | IR(61) | IW(62)
const DTE_V:  u64 = 1 << 0;   // Valid
const DTE_TV: u64 = 1 << 1;   // Translation info (mode + root) valid
const DTE_IR: u64 = 1 << 61;  // I/O read permission   (data[1])
const DTE_IW: u64 = 1 << 62;  // I/O write permission  (data[1])
const DTE_MODE_SHIFT: u64 = 9;
const PT_ROOT_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Number of device-table entries (full 16-bit BDF space) and bytes per entry.
const DEV_TABLE_ENTRIES: u64 = 1 << 16;
const DTE_BYTES: u64 = 32;
/// 2 MiB device table = 512 contiguous 4 KiB frames.
const DEV_TABLE_PAGES: usize = ((DEV_TABLE_ENTRIES * DTE_BYTES) / 4096) as usize;

/// Physical + HHDM-virtual base of each IOMMU structure (0 until allocated).
pub static DEV_TABLE_PHYS: AtomicU64 = AtomicU64::new(0);
static DEV_TABLE_VA: AtomicU64 = AtomicU64::new(0);
static CMD_BUF_PHYS: AtomicU64 = AtomicU64::new(0);
static EVENT_LOG_PHYS: AtomicU64 = AtomicU64::new(0);

/// Write one DTE (`bdf`-th entry) in the device table at HHDM VA `dt_va`.
///
/// # Safety
/// `dt_va` must be the mapped device table and `bdf < DEV_TABLE_ENTRIES`.
unsafe fn write_dte(dt_va: u64, bdf: u32, data0: u64, data1: u64) {
    let entry = dt_va + (bdf as u64) * DTE_BYTES;
    // SAFETY: entry is within the 2 MiB device table (bdf bounded by caller);
    // 8-byte aligned writes of the four DTE words.
    unsafe {
        core::ptr::write_volatile(entry as *mut u64, data0);
        core::ptr::write_volatile((entry + 8) as *mut u64, data1);
        core::ptr::write_volatile((entry + 16) as *mut u64, 0);
        core::ptr::write_volatile((entry + 24) as *mut u64, 0);
    }
}

/// Allocate and initialise the device table (all-passthrough), command buffer,
/// and event log, then program the IOMMU base registers. Does NOT enable
/// translation yet (Phase 1d). Returns false if allocation failed.
fn setup_structures(hhdm: u64, mmio_va: u64) -> bool {
    use crate::memory::allocator::alloc_contiguous;

    // --- Device table: 2 MiB contiguous ---
    let dt_phys = match alloc_contiguous(DEV_TABLE_PAGES) {
        Some(p) => p,
        None => {
            crate::kprintln!("iommu: WARN no 2 MiB contiguous block for device table; aborting 1b");
            return false;
        }
    };
    let dt_va = phys_to_virt(dt_phys, hhdm);
    // Default every DTE to passthrough: V=1, TV=0, mode=0, IR=1, IW=1. Untranslated
    // access with full permission — the disk and all non-USB devices keep working
    // once the IOMMU is enabled. The USB controllers are switched to a confined
    // domain in Phase 1c.
    for bdf in 0..DEV_TABLE_ENTRIES {
        // SAFETY: dt_va is the freshly-allocated mapped table; bdf in range.
        unsafe { write_dte(dt_va, bdf as u32, DTE_V, DTE_IR | DTE_IW) };
    }

    // --- Command buffer + event log: one 4 KiB frame each ---
    let cmd_phys = match alloc_contiguous(1) {
        Some(p) => p,
        None => { crate::kprintln!("iommu: WARN no frame for command buffer"); return false; }
    };
    let evt_phys = match alloc_contiguous(1) {
        Some(p) => p,
        None => { crate::kprintln!("iommu: WARN no frame for event log"); return false; }
    };
    // SAFETY: both frames just allocated and HHDM-mapped; zero them.
    unsafe {
        core::ptr::write_bytes(phys_to_virt(cmd_phys, hhdm) as *mut u8, 0, 4096);
        core::ptr::write_bytes(phys_to_virt(evt_phys, hhdm) as *mut u8, 0, 4096);
    }

    DEV_TABLE_PHYS.store(dt_phys, Ordering::Relaxed);
    DEV_TABLE_VA.store(dt_va, Ordering::Relaxed);
    CMD_BUF_PHYS.store(cmd_phys, Ordering::Relaxed);
    EVENT_LOG_PHYS.store(evt_phys, Ordering::Relaxed);

    // Make sure all the table writes land in RAM before the IOMMU reads them.
    // SAFETY: SFENCE has no memory-safety effect; orders prior stores.
    unsafe { core::arch::asm!("sfence", options(nostack, nomem, preserves_flags)) };

    // --- Program base registers (translation still disabled) ---
    // DTBR: base | size, size = (pages - 1) in bits[8:0].
    let dtbr = (dt_phys & PT_ROOT_MASK) | ((DEV_TABLE_PAGES as u64) - 1);
    // CMDBR / ELBR: base | (len<<56); len=8 => 4 KiB (256 entries).
    let cmdbr = (cmd_phys & PT_ROOT_MASK) | (8u64 << 56);
    let elbr = (evt_phys & PT_ROOT_MASK) | (8u64 << 56);
    // SAFETY: mmio_va mapped in bringup; standard base-register programming.
    unsafe {
        mmio_write64(mmio_va, reg::DEVICE_TABLE_BASE, dtbr);
        mmio_write64(mmio_va, reg::COMMAND_BUF_BASE, cmdbr);
        mmio_write64(mmio_va, reg::EVENT_LOG_BASE, elbr);
    }

    crate::kprintln!(
        "iommu: structures ready — devtab {:#x} ({} entries, passthrough), cmdbuf {:#x}, evtlog {:#x}",
        dt_phys, DEV_TABLE_ENTRIES, cmd_phys, evt_phys
    );
    crate::kprintln!("iommu: H1 Phase 1b OK -> device table + rings programmed (translation still off)");
    true
}

/// Map the IOMMU MMIO block and report its capabilities. Detection-and-readout
/// only — programs nothing. Call after [`detect`]; no-op if no IOMMU was found.
pub fn bringup(hhdm: u64) {
    if !IOMMU_PRESENT.load(Ordering::Relaxed) {
        return;
    }
    let phys = IOMMU_MMIO_BASE.load(Ordering::Relaxed);
    if phys == 0 {
        crate::kprintln!("iommu: IVRS gave no MMIO base; bring-up skipped");
        return;
    }

    // Map the MMIO block at its HHDM alias, uncached (PCD|PWT) — exactly the
    // APIC pattern in boot::init_local_apic. Limine's HHDM covers RAM but not
    // MMIO, so we add the pages to the active tables ourselves.
    let va = phys_to_virt(phys, hhdm);
    {
        use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
        let flags = PageFlags::PRESENT.bits()
            | PageFlags::WRITABLE.bits()
            | PageFlags::NO_EXEC.bits()
            | PageFlags::PWT.bits()
            | PageFlags::PCD.bits();
        let mut off = 0u64;
        while off < IOMMU_MMIO_LEN {
            // SAFETY: called after set_hhdm_offset; va/phys page-aligned; the
            // region is the IOMMU's MMIO window. Already-present is a no-op.
            if let Err(_e) = unsafe { map_in_active_tables(va + off, phys + off, flags) } {
                crate::kprintln!("iommu: WARN failed to map MMIO page at {:#x}", phys + off);
            }
            off += 0x1000;
        }
    }
    IOMMU_MMIO_VA.store(va, Ordering::Relaxed);

    // SAFETY: MMIO just mapped above; offsets are within IOMMU_MMIO_LEN.
    let efr = unsafe { mmio_read64(va, reg::EXT_FEATURE) };
    let control = unsafe { mmio_read64(va, reg::CONTROL) };
    let status = unsafe { mmio_read64(va, reg::STATUS) };
    let dev_tab = unsafe { mmio_read64(va, reg::DEVICE_TABLE_BASE) };
    let cmd_buf = unsafe { mmio_read64(va, reg::COMMAND_BUF_BASE) };
    let evt_log = unsafe { mmio_read64(va, reg::EVENT_LOG_BASE) };

    // Host Address Translation Size (EFR[14:13]): 0 => 4-level, 1 => 5-level,
    // 2 => 6-level I/O page tables. Determines the paging mode we set per DTE.
    let hats = (efr >> 13) & 0x3;
    let levels = match hats {
        0 => 4,
        1 => 5,
        2 => 6,
        _ => 0,
    };
    let iommu_enabled = control & 1 != 0;

    crate::kprintln!(
        "iommu: MMIO mapped at VA {:#x} (uncached); EFR={:#x} control={:#x} status={:#x}",
        va, efr, control, status
    );
    crate::kprintln!(
        "iommu: HATS={} -> {}-level I/O page tables; IommuEn={} (DTBR={:#x} CMDBR={:#x} ELBR={:#x})",
        hats, levels, iommu_enabled as u8, dev_tab, cmd_buf, evt_log
    );
    crate::kprintln!("iommu: H1 Phase 1a OK -> MMIO reachable; capabilities read");

    // Phase 1b: allocate + program the device table, command buffer, event log.
    setup_structures(hhdm, va);
}
