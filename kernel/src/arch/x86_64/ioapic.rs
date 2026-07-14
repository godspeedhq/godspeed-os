// SPDX-License-Identifier: GPL-2.0-only
//! I/O APIC - routing legacy (pin-based, INTx) device interrupts to the local APIC (§12).
//!
//! MSI-capable devices (the xHCI) deliver their interrupt straight to the LAPIC, so they
//! need no IOAPIC. Legacy-INTx-only devices (the EHCI on the T630) assert a *level-triggered*
//! PCI interrupt pin; the IOAPIC translates that pin (a Global System Interrupt) into a
//! local-APIC vector. This module programs one redirection-table entry per such device and
//! provides mask/unmask - the kernel masks a level entry while a userspace driver handles it
//! (so it doesn't re-fire / storm) and the driver unmasks after clearing the device's source.
//!
//! v1 assumes the IOAPIC at the architectural default `0xFEC00000` (true on essentially all
//! PCs incl. the T630/QEMU) and a single IOAPIC; a multi-IOAPIC machine would parse the ACPI
//! MADT for the base + GSI ranges. Uncached MMIO mapping mirrors the IOMMU/MSI-X pattern.

use core::sync::atomic::{AtomicU8, Ordering};
use portable_atomic::AtomicU64;

/// Architectural default IOAPIC physical base (all standard PCs).
const IOAPIC_PHYS_DEFAULT: u64 = 0xFEC0_0000;

/// Kernel virtual address the IOAPIC MMIO is mapped at (HHDM alias, uncached). 0 until init.
static IOAPIC_VA: AtomicU64 = AtomicU64::new(0);

/// IOAPIC register window: IOREGSEL (index) at +0x00, IOWIN (data) at +0x10.
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

/// Read IOAPIC register `reg` (indirect: write the index to IOREGSEL, read IOWIN).
///
/// # Safety
/// `init` must have mapped the MMIO.
unsafe fn read(va: u64, reg: u32) -> u32 {
    // SAFETY: IOREGSEL/IOWIN are in the mapped uncached IOAPIC MMIO page.
    unsafe {
        core::ptr::write_volatile((va + IOREGSEL) as *mut u32, reg);
        core::ptr::read_volatile((va + IOWIN) as *const u32)
    }
}

/// Write IOAPIC register `reg`.
///
/// # Safety
/// `init` must have mapped the MMIO.
unsafe fn write(va: u64, reg: u32, val: u32) {
    // SAFETY: IOREGSEL/IOWIN are in the mapped uncached IOAPIC MMIO page.
    unsafe {
        core::ptr::write_volatile((va + IOREGSEL) as *mut u32, reg);
        core::ptr::write_volatile((va + IOWIN) as *mut u32, val);
    }
}

/// Map the IOAPIC MMIO (uncached). Idempotent; safe to call once on the BSP at boot.
pub fn init() {
    if IOAPIC_VA.load(Ordering::Relaxed) != 0 {
        return;
    }
    let phys = IOAPIC_PHYS_DEFAULT;
    let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
    let va = hhdm.wrapping_add(phys);
    {
        use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
        let flags = PageFlags::PRESENT.bits()
            | PageFlags::WRITABLE.bits()
            | PageFlags::NO_EXEC.bits()
            | PageFlags::PWT.bits()
            | PageFlags::PCD.bits();
        // SAFETY: page-aligned MMIO page (the IOAPIC register window), uncached.
        let _ = unsafe { map_in_active_tables(va, phys, flags) };
    }
    IOAPIC_VA.store(va, Ordering::Relaxed);
    // IOAPIC Version register (0x01) bits[23:16] = max redirection entry (table size - 1).
    // SAFETY: MMIO just mapped.
    let ver = unsafe { read(va, 0x01) };
    crate::kprintln!(
        "ioapic: mapped at {:#x} (ver={:#04x}, {} redirection entries)",
        phys, ver & 0xFF, ((ver >> 16) & 0xFF) + 1
    );
}

/// Program redirection entry `gsi` to deliver to local-APIC `vector` on `dest_apic`, as a
/// **level-triggered, active-low** interrupt (the PCI INTx convention), masked per `masked`.
/// Fixed delivery, physical destination. No-op if `init` has not mapped the IOAPIC.
pub fn set_redir(gsi: u8, vector: u8, dest_apic: u8, masked: bool) {
    let va = IOAPIC_VA.load(Ordering::Relaxed);
    if va == 0 {
        return;
    }
    // Low dword: vector[7:0], delivery=fixed(000), destmode=physical(0), polarity=active-low
    // (bit13=1), trigger=level (bit15=1), mask (bit16). High dword: dest APIC id in [31:24].
    let low = (vector as u32)
        | (1 << 13)
        | (1 << 15)
        | ((masked as u32) << 16);
    let high = (dest_apic as u32) << 24;
    let idx = 0x10 + 2 * gsi as u32;
    // SAFETY: IOAPIC mapped; write high first then low (Intel-recommended: program the masked
    // entry's data before clearing the mask isn't needed here since we set both atomically
    // per-dword and the entry is consistent before any unmask).
    unsafe {
        write(va, idx + 1, high);
        write(va, idx, low);
    }
}

/// Mask (`true`) or unmask (`false`) redirection entry `gsi` by toggling bit 16. Used to
/// gate a level interrupt while a userspace driver handles it (§12).
pub fn set_mask(gsi: u8, masked: bool) {
    let va = IOAPIC_VA.load(Ordering::Relaxed);
    if va == 0 {
        return;
    }
    let idx = 0x10 + 2 * gsi as u32;
    // SAFETY: IOAPIC mapped; read-modify-write the low dword's mask bit.
    unsafe {
        let low = read(va, idx);
        let new = if masked { low | (1 << 16) } else { low & !(1 << 16) };
        write(va, idx, new);
    }
}

/// BSP local-APIC id - the destination for level INTx routes. Captured at boot from the
/// Limine SMP response (`mod.rs`). `0xFF` until set; `set_redir` then falls back to 0, the
/// BSP id on essentially all machines, so routing still works if capture is skipped.
static BSP_LAPIC_ID: AtomicU8 = AtomicU8::new(0xFF);

/// Record the BSP's local-APIC id (called once at boot before any device routing).
pub fn set_bsp_lapic_id(id: u8) {
    BSP_LAPIC_ID.store(id, Ordering::Relaxed);
}

/// The BSP local-APIC id to route level interrupts to (0 if not captured).
pub fn bsp_lapic_id() -> u8 {
    let id = BSP_LAPIC_ID.load(Ordering::Relaxed);
    if id == 0xFF { 0 } else { id }
}

// Level-triggered (INTx) route table: each entry binds an IDT `vector` to an IOAPIC `gsi`,
// so the generic IRQ dispatch (`route::deliver`) can mask the source while the driver handles
// it and the driver can unmask after acking. A *single* vector may have *several* GSI entries
// - on a machine with no ACPI _PRT parser we route a legacy-INTx device to a candidate set of
// GSIs (the real one plus the platform's PCI-INTx range) and mask/unmask the whole set, since
// only one such device exists and the spurious entries never fire. Empty slots hold vector
// `0xFF`. Edge MSI vectors (the xHCI's) are never registered here, so they are never masked.
const MAX_LEVEL_ROUTES: usize = 16;
static LEVEL_ROUTE_VEC: [AtomicU8; MAX_LEVEL_ROUTES] =
    [const { AtomicU8::new(0xFF) }; MAX_LEVEL_ROUTES];
static LEVEL_ROUTE_GSI: [AtomicU8; MAX_LEVEL_ROUTES] =
    [const { AtomicU8::new(0xFF) }; MAX_LEVEL_ROUTES];

/// Record that IDT `vector` is a level-triggered IOAPIC route on `gsi` (enables mask/unmask).
/// May be called several times for one vector to register a candidate GSI set.
pub fn set_level_route(vector: u8, gsi: u8) {
    for i in 0..MAX_LEVEL_ROUTES {
        if LEVEL_ROUTE_VEC[i].load(Ordering::Relaxed) == 0xFF {
            LEVEL_ROUTE_GSI[i].store(gsi, Ordering::Relaxed);
            LEVEL_ROUTE_VEC[i].store(vector, Ordering::Relaxed);
            return;
        }
    }
}

/// Mask the IOAPIC source(s) for `vector` if it has level route(s) (no-op for edge/MSI vectors).
/// Called from interrupt dispatch so a level INTx doesn't re-fire while the driver handles it.
pub fn mask_vector(vector: u8) {
    for i in 0..MAX_LEVEL_ROUTES {
        if LEVEL_ROUTE_VEC[i].load(Ordering::Relaxed) == vector {
            set_mask(LEVEL_ROUTE_GSI[i].load(Ordering::Relaxed), true);
        }
    }
}

/// Unmask the IOAPIC source(s) for `vector` if it has level route(s) - the driver calls this
/// (via a syscall) after clearing the device's interrupt source. No-op for edge/MSI vectors.
pub fn unmask_vector(vector: u8) {
    for i in 0..MAX_LEVEL_ROUTES {
        if LEVEL_ROUTE_VEC[i].load(Ordering::Relaxed) == vector {
            set_mask(LEVEL_ROUTE_GSI[i].load(Ordering::Relaxed), false);
        }
    }
}
