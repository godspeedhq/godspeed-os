// SPDX-License-Identifier: GPL-2.0-only
//! Minimal PCI enumeration (§12) - Stage 1 of the USB stack.
//!
//! Uses legacy PCI configuration mechanism #1 (port `0xCF8` address / `0xCFC`
//! data) to scan the bus and locate the xHCI USB host controller. The
//! discovered MMIO base and IRQ line are recorded so the kernel can later mint
//! an `hw_mmio` + `hw_interrupt` capability for the userspace `xhci` driver
//! service (§12.3) - the driver owns the controller; the kernel only routes its
//! interrupt and grants register access.
//!
//! Port I/O is hardware access, so this lives in the arch layer (§18.1).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use portable_atomic::AtomicU64;
use crate::smp::SpinLock;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Serializes PCI config access. The 0xCF8 (address) / 0xCFC (data) pair is a single global register
/// pair: an access is "latch address, then read/write data". Boot access is single-threaded, but the
/// runtime kill path now clears bus-mastering on a dying DMA driver while another core may be
/// re-initialising a respawned one - an unserialized interleave would latch one core's address and then
/// read/write it with the other core's data, hitting the WRONG register. The lock makes each access (and
/// `clear_bus_master`'s read-modify-write) atomic.
static PCI_CONFIG_LOCK: SpinLock<()> = SpinLock::new(());

// xHCI is PCI class 0x0C (serial-bus controller), subclass 0x03 (USB),
// programming interface 0x30 (eXtensible Host Controller Interface).
const CLASS_SERIAL_BUS: u8 = 0x0C;
const SUBCLASS_USB: u8 = 0x03;
const PROGIF_XHCI: u8 = 0x30;
const PROGIF_EHCI: u8 = 0x20;

/// Discovered-xHCI record. Written once by `init` on the BSP during boot,
/// read later when minting the driver's caps. Plain atomics: single writer at
/// boot, no concurrent access.
pub static XHCI_FOUND: AtomicBool = AtomicBool::new(false);
pub static XHCI_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
pub static XHCI_IRQ: AtomicU8 = AtomicU8::new(0);
/// PCI BDF (bus<<8 | dev<<3 | func) of the driver's xHCI - the index into the
/// IOMMU device table for DMA confinement (H1). 0xFFFF if none found.
pub static XHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);

/// All discovered xHCI controllers (a system may have several; the boot drive
/// and the keyboard often sit on different ones). Recorded during the scan.
pub static XHCI_COUNT: AtomicU32 = AtomicU32::new(0);
pub static XHCI_BASES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
pub static XHCI_IRQS: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];
pub static XHCI_BDFS: [AtomicU32; 4] = [const { AtomicU32::new(0xFFFF) }; 4];

/// Discovered EHCI (USB 2.0) controller - the T630's back ports hang off it
/// (§12). The userspace `ehci` driver gets this BAR mapped at spawn, exactly as
/// the `xhci` driver gets the xHCI BAR. First EHCI found wins.
pub static EHCI_FOUND: AtomicBool = AtomicBool::new(false);
pub static EHCI_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
pub static EHCI_IRQ: AtomicU8 = AtomicU8::new(0);
/// PCI BDF of the EHCI controller - IOMMU device-table index (H1). 0xFFFF if none.
pub static EHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);

// AHCI is PCI class 0x01 (mass storage), subclass 0x06 (SATA), progif 0x01 (AHCI).
const CLASS_MASS_STORAGE: u8 = 0x01;
const SUBCLASS_SATA: u8 = 0x06;
const PROGIF_AHCI: u8 = 0x01;

/// Discovered AHCI (SATA) controller - the `block-driver` gets its ABAR (BAR5)
/// mapped + a DMA arena at spawn, exactly as the USB drivers do (§12,
/// docs/ahci.md). First AHCI found wins. ABAR is a 32-bit MMIO BAR.
pub static AHCI_FOUND: AtomicBool = AtomicBool::new(false);
pub static AHCI_ABAR: AtomicU64 = AtomicU64::new(0);
pub static AHCI_IRQ: AtomicU8 = AtomicU8::new(0);
/// PCI BDF of the AHCI controller - IOMMU device-table index (H1). 0xFFFF if none.
pub static AHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);

// Network controller (PCI class 0x02) - the NIC. Networking Phase 0 (docs/networking.md): identify
// what NIC hardware is present so we know whether QEMU's e1000 and the T630's chipset share a driver.
// The first NIC found is recorded for the future `nic-driver` to receive its MMIO BAR + IRQ at spawn,
// exactly as `block-driver` gets the AHCI ABAR. No driver is bound yet - this is pure identification.
const CLASS_NETWORK: u8 = 0x02;
/// Discovered network controller. First NIC found wins. 0xFFFF BDF if none.
pub static NIC_FOUND:         AtomicBool = AtomicBool::new(false);
pub static NIC_MMIO_BASE:     AtomicU64  = AtomicU64::new(0);     // BAR0 register space
pub static NIC_IRQ:           AtomicU8   = AtomicU8::new(0);
pub static NIC_BDF:           AtomicU32  = AtomicU32::new(0xFFFF); // IOMMU device-table index (H1)
pub static NIC_VENDOR_DEVICE: AtomicU32  = AtomicU32::new(0);     // config 0x00: vendor | device<<16

/// Build a 16-bit PCI BDF (bus<<8 | dev<<3 | func) - the IOMMU device-table index.
#[inline]
pub fn make_bdf(bus: u8, dev: u8, func: u8) -> u32 {
    ((bus as u32) << 8) | ((dev as u32) << 3) | (func as u32)
}

/// Write a 32-bit value to an I/O port.
///
/// # Safety
/// Port I/O is ring-0 only; the caller must target a valid port.
#[inline]
unsafe fn outl(port: u16, val: u32) {
    // SAFETY: `out dx, eax` - standard 32-bit port write.
    unsafe {
        core::arch::asm!("out dx, eax", in("dx") port, in("eax") val,
            options(nomem, nostack, preserves_flags));
    }
}

/// Read a 32-bit value from an I/O port.
///
/// # Safety
/// Port I/O is ring-0 only; the caller must target a valid port.
#[inline]
unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    // SAFETY: `in eax, dx` - standard 32-bit port read.
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") val, in("dx") port,
            options(nomem, nostack, preserves_flags));
    }
    val
}

/// Read one 32-bit dword from PCI config space (mechanism #1). `offset` is
/// dword-aligned (low two bits ignored).
fn config_read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    // Lock held across the address+data pair (§ PCI_CONFIG_LOCK). SAFETY: mechanism #1; ring-0 port I/O.
    let _g = PCI_CONFIG_LOCK.lock();
    unsafe {
        outl(CONFIG_ADDRESS, addr);
        inl(CONFIG_DATA)
    }
}

/// Write one 32-bit dword to PCI config space (mechanism #1). `offset` is
/// dword-aligned (low two bits ignored).
fn config_write32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    let addr = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    // Lock held across the address+data pair (§ PCI_CONFIG_LOCK). SAFETY: mechanism #1; ring-0 port I/O.
    let _g = PCI_CONFIG_LOCK.lock();
    unsafe {
        outl(CONFIG_ADDRESS, addr);
        outl(CONFIG_DATA, val);
    }
}

/// Clear PCI Bus Master Enable (Command register bit 2) for `bdf` - stop the device issuing ANY DMA.
/// Called on a DMA-capable driver's death BEFORE its frames are reclaimed: a controller whose DMA frames
/// are about to be freed + reused (e.g. as a page table) would otherwise scribble a stale/in-flight DMA
/// into them - the chaos `max-carnage` page-table corruption (a freed DMA frame realloc'd as a page table,
/// written by EHCI's still-running periodic schedule or an in-flight AHCI command). On the T630 ehci +
/// block-driver run in IOMMU PASSTHROUGH, so nothing else stops the stray write. The respawned driver
/// re-enables bus-mastering during init. Device-agnostic (any PCI DMA device). `bdf == 0xFFFF` = no-op.
pub fn clear_bus_master(bdf: u32) {
    if bdf == 0xFFFF { return; }
    let addr = 0x8000_0000u32
        | (((bdf >> 8) & 0xFF) << 16)  // bus
        | (((bdf >> 3) & 0x1F) << 11)  // device
        | ((bdf & 0x07) << 8)          // function
        | 0x04;                        // Command register
    // Hold the lock across the whole read-modify-write so it is atomic vs a concurrent respawn's
    // bus-master ENABLE on another core. SAFETY: ring-0 port I/O, PCI mechanism #1.
    let changed = {
        let _g = PCI_CONFIG_LOCK.lock();
        unsafe {
            outl(CONFIG_ADDRESS, addr);
            let cmd = inl(CONFIG_DATA);
            let new = cmd & !(1u32 << 2); // clear Bus Master Enable (bit 2)
            if new != cmd {
                outl(CONFIG_ADDRESS, addr);
                outl(CONFIG_DATA, new);
                true
            } else {
                false
            }
        }
    };
    if changed {
        crate::kprintln!("pci: BDF {:#06x} bus-master DISABLED on driver death (DMA quiesced)", bdf & 0xFFFF);
    }
}

/// Set PCI Bus Master Enable (Command bit 2) for `bdf` - allow the device to issue DMA. The counterpart to
/// `clear_bus_master`: the kill path clears it, so a DMA driver's spawn/respawn re-enables it here.
/// Firmware sets it once at boot, but a respawned driver would otherwise have it cleared + its DMA would
/// silently never start. Idempotent (no-op if already set). `bdf == 0xFFFF` = no-op.
pub fn set_bus_master(bdf: u32) {
    if bdf == 0xFFFF { return; }
    let addr = 0x8000_0000u32
        | (((bdf >> 8) & 0xFF) << 16)  // bus
        | (((bdf >> 3) & 0x1F) << 11)  // device
        | ((bdf & 0x07) << 8)          // function
        | 0x04;                        // Command register
    let changed = {
        let _g = PCI_CONFIG_LOCK.lock();
        unsafe {
            outl(CONFIG_ADDRESS, addr);
            let cmd = inl(CONFIG_DATA);
            // Enable Memory Space (bit 1) AND Bus Master (bit 2). A real chipset IGNORES MMIO to a
            // device's memory BAR unless Memory Space is set - QEMU is lenient (so the e1000 worked
            // without it), but on the T630 the RTL8168 returned 0xff for every register read, so its
            // reset bit looked permanently set and the driver spun forever. Setting bit 1 fixes it.
            let new = cmd | (1u32 << 2) | (1u32 << 1);
            if new != cmd {
                outl(CONFIG_ADDRESS, addr);
                outl(CONFIG_DATA, new);
                true
            } else {
                false
            }
        }
    };
    if changed {
        crate::kprintln!("pci: BDF {:#06x} bus-master + memory-space ENABLED for DMA driver spawn", bdf & 0xFFFF);
    }
}

/// Bring a device to power state D0 (fully on). Firmware may leave a non-boot device (e.g. the T630's
/// RTL8168) in D3hot, where its memory-mapped registers are INACCESSIBLE - MMIO reads return 0xff and a
/// driver hangs (its reset bit looks permanently set). Walk the PCI capability list for the Power
/// Management cap (ID 0x01) and clear PMCSR's PowerState field to D0. Idempotent. `bdf == 0xFFFF` = no-op.
pub fn set_power_d0(bdf: u32) {
    if bdf == 0xFFFF { return; }
    let bus  = ((bdf >> 8) & 0xFF) as u8;
    let dev  = ((bdf >> 3) & 0x1F) as u8;
    let func = (bdf & 0x07) as u8;
    // Status register bit 4 (= bit 20 of the 0x04 dword) = capability list present.
    if config_read32(bus, dev, func, 0x04) & (1 << 20) == 0 { return; }
    let mut cap = (config_read32(bus, dev, func, 0x34) & 0xFC) as u8;
    let mut guard = 0;
    while cap != 0 && guard < 48 {
        let dw = config_read32(bus, dev, func, cap);
        if (dw & 0xFF) as u8 == 0x01 {
            // Power Management cap: PMCSR is at cap+4, PowerState = bits 1:0.
            let pm = config_read32(bus, dev, func, cap + 4);
            if pm & 0x3 != 0 {
                config_write32(bus, dev, func, cap + 4, pm & !0x3); // -> D0
                crate::kprintln!("pci: BDF {:#06x} power D{} -> D0", bdf & 0xFFFF, pm & 0x3);
            }
            return;
        }
        cap = ((dw >> 8) & 0xFC) as u8; // next cap pointer
        guard += 1;
    }
}

/// Take ownership of the EHCI controller from the firmware (BIOS→OS handoff).
///
/// EHCI's HCCPARAMS register (MMIO capability offset 0x08) carries the EHCI
/// Extended Capabilities Pointer (EECP) - an offset into *PCI config space*
/// (not MMIO) where a capability list lives. The USB Legacy Support capability
/// (ID 0x01) has a BIOS-Owned and an OS-Owned semaphore. Until the OS sets
/// OS-Owned and the firmware clears BIOS-Owned, the firmware (SMM) keeps poking
/// the controller and running its own periodic schedule out of firmware memory.
///
/// Without an IOMMU that firmware DMA was invisible. With H1 confinement it
/// faults (the firmware buffers are outside the driver's arena) and breaks the
/// back-port keyboard. This handoff makes the firmware release the controller -
/// standard OS behaviour at init. EECP lives in PCI config space, which the
/// userspace `ehci` driver cannot reach, so the kernel must do it. Idempotent;
/// no-op if no EHCI, no extended caps, or no USB Legacy Support capability.
///
/// Retained but not currently called: EHCI is left in IOMMU passthrough (firmware
/// co-owns it, the configuration the back-port keyboard works in), so it is not
/// handed off. Re-enable when EHCI confinement is revisited.
#[allow(dead_code)]
pub fn ehci_bios_handoff() {
    if !EHCI_FOUND.load(Ordering::Relaxed) {
        return;
    }
    let mmio = EHCI_MMIO_BASE.load(Ordering::Relaxed);
    let bdf = EHCI_BDF.load(Ordering::Relaxed);
    if mmio == 0 || bdf == 0xFFFF {
        return;
    }
    let bus = ((bdf >> 8) & 0xff) as u8;
    let dev = ((bdf >> 3) & 0x1f) as u8;
    let func = (bdf & 0x7) as u8;

    // Read HCCPARAMS (MMIO cap offset 0x08) to extract the EECP. Map the MMIO
    // page uncached (PCD|PWT), like the APIC/IOMMU, since the HHDM covers RAM
    // but not MMIO.
    let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
    let va = hhdm + (mmio & !0xFFF);
    {
        use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
        let flags = PageFlags::PRESENT.bits()
            | PageFlags::WRITABLE.bits()
            | PageFlags::NO_EXEC.bits()
            | PageFlags::PWT.bits()
            | PageFlags::PCD.bits();
        // SAFETY: called after set_hhdm_offset; mapping the EHCI MMIO page
        // (page-aligned). Already-present is a no-op.
        if unsafe { map_in_active_tables(va, mmio & !0xFFF, flags) }.is_err() {
            crate::kprintln!("ehci-handoff: WARN could not map MMIO {:#x}", mmio);
            return;
        }
    }
    let hccparams_va = hhdm + mmio + 0x08;
    // SAFETY: HCCPARAMS is within the mapped MMIO page; aligned 32-bit read.
    let hccparams = unsafe { core::ptr::read_volatile(hccparams_va as *const u32) };
    let eecp = ((hccparams >> 8) & 0xFF) as u8;
    if eecp < 0x40 {
        crate::kprintln!("ehci-handoff: no extended capabilities (eecp={:#x})", eecp);
        return;
    }

    // Walk the PCI-config extended-capability list for USB Legacy Support (ID 1).
    let mut ptr = eecp;
    let mut guard = 16; // cap lists are short; bound the walk
    while ptr >= 0x40 && guard > 0 {
        guard -= 1;
        let cap = config_read32(bus, dev, func, ptr);
        let cap_id = cap & 0xFF;
        if cap_id == 0x01 {
            // USBLEGSUP at `ptr`: bit16 = HC BIOS Owned, bit24 = HC OS Owned.
            if cap & (1 << 16) != 0 {
                // Claim OS ownership and wait for the firmware to release.
                config_write32(bus, dev, func, ptr, cap | (1 << 24));
                let mut ok = false;
                for _ in 0..1_000_000u32 {
                    let v = config_read32(bus, dev, func, ptr);
                    if v & (1 << 16) == 0 {
                        ok = true;
                        break;
                    }
                    core::hint::spin_loop();
                }
                crate::kprintln!(
                    "ehci-handoff: USBLEGSUP@{:#x} OS-owned, BIOS released={} (was {:#010x})",
                    ptr, ok as u8, cap
                );
            } else {
                crate::kprintln!("ehci-handoff: already OS-owned (USBLEGSUP={:#010x})", cap);
            }
            // Disable all firmware SMIs on this controller (USBLEGCTLSTS at ptr+4).
            config_write32(bus, dev, func, ptr + 4, 0);
            return;
        }
        let next = ((cap >> 8) & 0xFF) as u8;
        if next == 0 {
            break;
        }
        ptr = next;
    }
    crate::kprintln!("ehci-handoff: no USB Legacy Support capability found");
}

/// Take ownership of the xHCI controller from the firmware (BIOS→OS handoff).
///
/// Unlike EHCI (whose handoff register is in PCI config space), xHCI's extended
/// capabilities - including USB Legacy Support - live in MMIO, reached via the
/// xECP field of HCCPARAMS1 (MMIO cap offset 0x10). We could put this in the
/// userspace driver, but doing it here keeps both controllers handed off
/// uniformly before the IOMMU confines them: otherwise the firmware SMM keeps
/// poking the controller (running DMA out of firmware memory, which faults under
/// confinement) and the device that was leaning on firmware support breaks.
/// Idempotent; no-op if no xHCI, no extended caps, or no Legacy Support cap.
pub fn xhci_bios_handoff() {
    if !XHCI_FOUND.load(Ordering::Relaxed) {
        return;
    }
    let mmio = XHCI_MMIO_BASE.load(Ordering::Relaxed);
    if mmio == 0 {
        return;
    }
    let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
    let base = mmio & !0xFFF;
    {
        use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
        let flags = PageFlags::PRESENT.bits()
            | PageFlags::WRITABLE.bits()
            | PageFlags::NO_EXEC.bits()
            | PageFlags::PWT.bits()
            | PageFlags::PCD.bits();
        // Map 16 pages (64 KiB) - enough to reach the extended-capability list.
        for i in 0..16u64 {
            let off = i * 0x1000;
            // SAFETY: called after set_hhdm_offset; mapping the xHCI MMIO pages
            // (page-aligned) uncached. Already-present is a no-op.
            if unsafe { map_in_active_tables(hhdm + base + off, base + off, flags) }.is_err() {
                crate::kprintln!("xhci-handoff: WARN could not map MMIO {:#x}", base + off);
                return;
            }
        }
    }
    let va = hhdm + mmio;
    // HCCPARAMS1 (cap offset 0x10): xECP = bits [31:16], a dword offset from base.
    // SAFETY: within the mapped MMIO; aligned 32-bit read.
    let hccparams1 = unsafe { core::ptr::read_volatile((va + 0x10) as *const u32) };
    let xecp = (hccparams1 >> 16) & 0xFFFF;
    if xecp == 0 {
        crate::kprintln!("xhci-handoff: no extended capabilities");
        return;
    }
    // Walk the MMIO extended-capability list for USB Legacy Support (ID 1).
    let mut off = (xecp as u64) * 4;
    let mut guard = 64;
    while off != 0 && off < 0x10000 && guard > 0 {
        guard -= 1;
        let cap_va = va + off;
        // SAFETY: off bounded < 0x10000, within the mapped 64 KiB; aligned read.
        let cap = unsafe { core::ptr::read_volatile(cap_va as *const u32) };
        let cap_id = cap & 0xFF;
        if cap_id == 0x01 {
            // USBLEGSUP at `off`: bit16 = BIOS Owned, bit24 = OS Owned.
            if cap & (1 << 16) != 0 {
                // SAFETY: claim OS ownership.
                unsafe { core::ptr::write_volatile(cap_va as *mut u32, cap | (1 << 24)) };
                let mut ok = false;
                for _ in 0..1_000_000u32 {
                    // SAFETY: poll the same register for BIOS release.
                    if unsafe { core::ptr::read_volatile(cap_va as *const u32) } & (1 << 16) == 0 {
                        ok = true;
                        break;
                    }
                    core::hint::spin_loop();
                }
                crate::kprintln!(
                    "xhci-handoff: USBLEGSUP@{:#x} OS-owned, BIOS released={} (was {:#010x})",
                    off, ok as u8, cap
                );
            } else {
                crate::kprintln!("xhci-handoff: already OS-owned (USBLEGSUP={:#010x})", cap);
            }
            // USBLEGCTLSTS (off+4): disable all BIOS SMIs, clear SMI events (RW1C).
            // Masks per the xHCI spec / Linux quirk_usb_handoff_xhci.
            const DISABLE_SMI: u32 = (0x7 << 1) | (0xff << 5) | (0x7 << 17);
            const SMI_EVENTS: u32 = 0x7 << 29;
            // SAFETY: ctlsts is the dword after USBLEGSUP, within the mapping.
            let ctl = unsafe { core::ptr::read_volatile((cap_va + 4) as *const u32) };
            unsafe {
                core::ptr::write_volatile(
                    (cap_va + 4) as *mut u32,
                    (ctl & !DISABLE_SMI) | SMI_EVENTS,
                )
            };
            return;
        }
        let next = (cap >> 8) & 0xFF; // next pointer, in dwords
        if next == 0 {
            break;
        }
        off += (next as u64) * 4;
    }
    crate::kprintln!("xhci-handoff: no USB Legacy Support capability found");
}

/// Report whether the EHCI controller supports a PCI Function-Level Reset (FLR),
/// and dump its PCI capability list. FLR is a bit in the PCI Express capability's
/// Device Capabilities register (bit 28); performing it (Device Control bit 15)
/// resets the function far more thoroughly than the EHCI `HCRESET`, which on this
/// machine does not scrub the controller's stale firmware-era internal DMA state.
/// Detection only - does not perform the reset. No-op if no EHCI.
pub fn ehci_flr_probe() {
    if !EHCI_FOUND.load(Ordering::Relaxed) {
        return;
    }
    let bdf = EHCI_BDF.load(Ordering::Relaxed);
    if bdf == 0xFFFF {
        return;
    }
    let bus = ((bdf >> 8) & 0xff) as u8;
    let dev = ((bdf >> 3) & 0x1f) as u8;
    let func = (bdf & 0x7) as u8;

    // Status register (offset 0x06) bit 4 = capabilities list present.
    let status = (config_read32(bus, dev, func, 0x04) >> 16) as u16;
    if status & (1 << 4) == 0 {
        crate::kprintln!("ehci-flr: no PCI capability list -> no PCIe cap -> no FLR");
        return;
    }
    // Walk the capability list from the Capabilities Pointer (offset 0x34, low byte).
    let mut cap = (config_read32(bus, dev, func, 0x34) & 0xFC) as u8;
    let mut guard = 48;
    let mut found_pcie = false;
    while cap >= 0x40 && guard > 0 {
        guard -= 1;
        let hdr = config_read32(bus, dev, func, cap);
        let cap_id = (hdr & 0xFF) as u8;
        let next = ((hdr >> 8) & 0xFF) as u8;
        crate::kprintln!("ehci-flr: cap@{:#04x} id={:#04x}", cap, cap_id);
        if cap_id == 0x10 {
            // PCI Express capability. Device Capabilities at cap+0x04; FLR = bit 28.
            found_pcie = true;
            let dev_cap = config_read32(bus, dev, func, cap + 0x04);
            let flr = (dev_cap >> 28) & 1;
            crate::kprintln!(
                "ehci-flr: PCIe cap@{:#04x} DevCap={:#010x} FLR_supported={}",
                cap, dev_cap, flr
            );
        }
        if next == 0 {
            break;
        }
        cap = next;
    }
    if !found_pcie {
        crate::kprintln!("ehci-flr: no PCI Express capability (legacy-PCI function) -> no FLR");
    }
}

/// Program a device's MSI capability to deliver interrupts to local-APIC `vector` on the
/// BSP, then enable MSI. Returns `true` if an MSI capability (id 0x05) was found and
/// programmed. Edge-triggered, fixed delivery, a single message vector.
///
/// MSI is the kernel's device-interrupt path (§12): the device writes the message -
/// address `0xFEE00000` (LAPIC, dest BSP) and data = `vector` - straight to the local APIC,
/// so no IOAPIC or ACPI `_PRT` routing is needed. The caller must have installed an IDT
/// handler for `vector` (→ `interrupt::route::deliver`) before the device starts raising it.
///
/// Note: only legacy MSI (cap 0x05) here, not MSI-X (cap 0x11, a separate MMIO table). USB
/// controllers expose MSI; if a device is MSI-X-only this returns false and the caller keeps
/// polling.
pub fn program_msi(bdf: u32, vector: u8, dest_apic: u8) -> bool {
    let bus = ((bdf >> 8) & 0xff) as u8;
    let dev = ((bdf >> 3) & 0x1f) as u8;
    let func = (bdf & 0x7) as u8;

    // Capabilities list present? Status register (0x06) bit 4.
    let status = (config_read32(bus, dev, func, 0x04) >> 16) as u16;
    if status & (1 << 4) == 0 {
        return false;
    }
    // Walk the capability list from the Capabilities Pointer (0x34, low byte).
    let mut cap = (config_read32(bus, dev, func, 0x34) & 0xFC) as u8;
    let mut guard = 48;
    while cap >= 0x40 && guard > 0 {
        guard -= 1;
        let hdr = config_read32(bus, dev, func, cap);
        let cap_id = (hdr & 0xFF) as u8;
        let next = ((hdr >> 8) & 0xFF) as u8;
        if cap_id == 0x05 {
            // MSI capability. Message Control = hdr[31:16]; bit 7 = 64-bit address capable.
            let ctrl = (hdr >> 16) as u16;
            let is_64 = ctrl & (1 << 7) != 0;
            // Message Address (low) at cap+0x04 = 0xFEE00000 | (dest_apic << 12).
            config_write32(bus, dev, func, cap + 0x04, 0xFEE0_0000 | ((dest_apic as u32) << 12));
            if is_64 {
                config_write32(bus, dev, func, cap + 0x08, 0);                 // addr high
                config_write32(bus, dev, func, cap + 0x0C, vector as u32);      // data (edge/fixed)
            } else {
                config_write32(bus, dev, func, cap + 0x08, vector as u32);      // data
            }
            // Enable MSI (ctrl bit 0); Multiple Message Enable = 0 (bits[6:4]) → 1 vector.
            let new_ctrl = (ctrl & !(0x7u16 << 4)) | 1;
            let new_hdr = (hdr & 0x0000_FFFF) | ((new_ctrl as u32) << 16);
            config_write32(bus, dev, func, cap, new_hdr);
            crate::kprintln!(
                "pci: MSI enabled on {:02x}:{:02x}.{} vector={:#x} ({}-bit addr)",
                bus, dev, func, vector, if is_64 { 64 } else { 32 }
            );
            return true;
        }
        if next == 0 {
            break;
        }
        cap = next;
    }
    crate::kprintln!("pci: no MSI capability (id 0x05) on {:02x}:{:02x}.{}", bus, dev, func);
    false
}

/// Program a device's MSI-X capability (id 0x11): point table entry 0 at local-APIC
/// `vector` on the BSP, unmask it, and enable MSI-X. Returns `true` if MSI-X was found and
/// programmed. Most modern controllers (incl. `qemu-xhci`) expose MSI-X rather than MSI.
///
/// MSI-X's message table lives in **MMIO** (inside a BAR), not config space, so this maps
/// the table's page uncached at its HHDM alias (the same pattern the IOMMU MMIO uses) and
/// writes entry 0. The device must also be a bus master (Command bit 2) to issue the
/// upstream MSI memory write. The caller installs the IDT handler for `vector` first.
pub fn program_msix(bdf: u32, vector: u8, dest_apic: u8) -> bool {
    let bus = ((bdf >> 8) & 0xff) as u8;
    let dev = ((bdf >> 3) & 0x1f) as u8;
    let func = (bdf & 0x7) as u8;

    let status = (config_read32(bus, dev, func, 0x04) >> 16) as u16;
    if status & (1 << 4) == 0 {
        return false;
    }
    let mut cap = (config_read32(bus, dev, func, 0x34) & 0xFC) as u8;
    let mut guard = 48u8;
    while cap >= 0x40 && guard > 0 {
        guard -= 1;
        let hdr = config_read32(bus, dev, func, cap);
        let cap_id = (hdr & 0xFF) as u8;
        let next = ((hdr >> 8) & 0xFF) as u8;
        if cap_id == 0x11 {
            // Table Offset/BIR (cap+0x04): bits[2:0] = BAR index, bits[31:3] = byte offset.
            let tbl = config_read32(bus, dev, func, cap + 0x04);
            let bir = (tbl & 0x7) as u8;
            let tbl_off = (tbl & !0x7u32) as u64;
            // Physical base of BAR[bir] (handle a 64-bit memory BAR).
            let bar_off = 0x10u8 + bir * 4;
            let bar = config_read32(bus, dev, func, bar_off);
            let bar_phys = if bar & 0x6 == 0x4 {
                let bar_hi = config_read32(bus, dev, func, bar_off + 4);
                ((bar_hi as u64) << 32) | ((bar & 0xFFFF_FFF0) as u64)
            } else {
                (bar & 0xFFFF_FFF0) as u64
            };
            let tbl_phys = bar_phys + tbl_off;

            // Map the table's page uncached at its HHDM alias (Limine's HHDM covers RAM but
            // not MMIO, so add the page to the active tables ourselves - like the IOMMU).
            let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
            let page_phys = tbl_phys & !0xFFFu64;
            let va_page = hhdm.wrapping_add(page_phys);
            {
                use crate::arch::x86_64::page_tables::{map_in_active_tables, PageFlags};
                let flags = PageFlags::PRESENT.bits()
                    | PageFlags::WRITABLE.bits()
                    | PageFlags::NO_EXEC.bits()
                    | PageFlags::PWT.bits()
                    | PageFlags::PCD.bits();
                // SAFETY: page-aligned MMIO page (the device's MSI-X table BAR), uncached;
                // already-present is a no-op.
                let _ = unsafe { map_in_active_tables(va_page, page_phys, flags) };
            }
            let entry = hhdm.wrapping_add(tbl_phys); // table entry 0 (16 bytes)
            // SAFETY: `entry` addresses MSI-X table entry 0 in the just-mapped MMIO page.
            unsafe {
                core::ptr::write_volatile((entry + 0x00) as *mut u32,
                    0xFEE0_0000 | ((dest_apic as u32) << 12));                        // addr lo (LAPIC dest)
                core::ptr::write_volatile((entry + 0x04) as *mut u32, 0);           // addr hi
                core::ptr::write_volatile((entry + 0x08) as *mut u32, vector as u32);// data (edge/fixed)
                core::ptr::write_volatile((entry + 0x0C) as *mut u32, 0);           // vector control: unmask
            }

            // Bus master enable (Command bit 2) so the device can issue the MSI write.
            let cmd = config_read32(bus, dev, func, 0x04);
            config_write32(bus, dev, func, 0x04, cmd | (1 << 2));

            // Enable MSI-X (Message Control bit 15), clear the function mask (bit 14).
            let ctrl = (hdr >> 16) as u16;
            let new_ctrl = (ctrl & !(1u16 << 14)) | (1u16 << 15);
            let new_hdr = (hdr & 0x0000_FFFF) | ((new_ctrl as u32) << 16);
            config_write32(bus, dev, func, cap, new_hdr);

            crate::kprintln!(
                "pci: MSI-X enabled on {:02x}:{:02x}.{} vector={:#x} bir={} tbl@{:#x}",
                bus, dev, func, vector, bir, tbl_phys
            );
            return true;
        }
        if next == 0 {
            break;
        }
        cap = next;
    }
    false
}

/// Program the picked xHCI controller's MSI to deliver to the kernel's xHCI MSI vector
/// (P1, USB interrupts). No-op (returns false) if no xHCI was found. The controller's own
/// interrupter must be enabled by the driver before any MSI actually fires (P2); this only
/// sets up the message so it *can*. Call after `init()` and after the local APIC is up.
pub fn program_xhci_msi() -> bool {
    if !XHCI_FOUND.load(Ordering::Relaxed) {
        return false;
    }
    let bdf = XHCI_BDF.load(Ordering::Relaxed);
    let vector = crate::arch::x86_64::interrupts::XHCI_MSI_VECTOR;
    let dest = usb_irq_dest_lapic(crate::task::XHCI_CORE);
    // Prefer plain MSI; fall back to MSI-X (what qemu-xhci and most real xHCIs expose).
    program_msi(bdf, vector, dest) || program_msix(bdf, vector, dest)
}

/// LAPIC id to deliver a USB controller's interrupt to: the core `driver_core` the owning driver is
/// pinned to (`task::XHCI_CORE` / `task::EHCI_CORE`, the single source of truth). Delivering the IRQ to
/// the driver's OWN core means a device event (a keypress) wakes that core directly out of its idle
/// `hlt` and the wake stays core-local - no cross-core IPI or BSP scan, which an idle AP on this
/// hardware (ARAT `hlt` idle) does not service promptly (§12). Falls back to the BSP if that core is not
/// ready (single-core), where the driver runs on the BSP anyway. Previously hardcoded core 1, which had
/// drifted away from the drivers' actual cores (2/3) - the co-location is now real (docs/power.md).
fn usb_irq_dest_lapic(driver_core: u32) -> u8 {
    if crate::smp::core::is_ready(driver_core) {
        crate::smp::core::core_lapic_id(driver_core) as u8
    } else {
        crate::arch::x86_64::ioapic::bsp_lapic_id()
    }
}

/// Try to program the EHCI controller's MSI/MSI-X (interrupt-driven USB, §12). Returns true
/// if MSI or MSI-X was found and programmed (→ the easy path, like xHCI). Logs the outcome.
/// Classic Intel-ICH EHCI exposes neither (legacy INTx only - would need IOAPIC routing);
/// other EHCIs (e.g. AMD) may have MSI. This both does P1 (when MSI exists) AND tells us at
/// boot which interrupt path the running machine's EHCI needs.
pub fn program_ehci_msi() -> bool {
    if !EHCI_FOUND.load(Ordering::Relaxed) {
        return false;
    }
    let bdf = EHCI_BDF.load(Ordering::Relaxed);
    let vector = crate::arch::x86_64::interrupts::EHCI_MSI_VECTOR;
    let dest = usb_irq_dest_lapic(crate::task::EHCI_CORE);
    let ok = program_msi(bdf, vector, dest) || program_msix(bdf, vector, dest);
    if !ok {
        crate::kprintln!(
            "ehci: no MSI/MSI-X capability - controller uses legacy INTx (IOAPIC routing needed)"
        );
    }
    ok
}

/// Route the EHCI's legacy INTx pin through the IOAPIC to the kernel's EHCI vector (§12), for
/// a controller with no MSI.
///
/// We have no ACPI `_PRT` parser, so the exact GSI the EHCI's INTx pin maps to is unknown: the
/// PCI interrupt-line register holds the legacy 8259 IRQ (usually 11), but an AMD FCH routes PCI
/// INTx to a *higher* GSI in the 16-23 range. Rather than gamble on one, we program a **candidate
/// set** - the legacy line plus the platform PCI-INTx range - all to the same EHCI vector,
/// level-triggered + active-low (PCI INTx), destination = the real BSP local-APIC id. Only the
/// EHCI uses INTx (AHCI polls, xHCI is MSI), so the spurious candidates never fire; the one that
/// matches the hardware delivers. Each is registered as a level route so dispatch masks - and the
/// driver unmasks - the whole set together. No-op if no EHCI. Call after `ioapic::init()`.
pub fn route_ehci_intx() {
    if !EHCI_FOUND.load(Ordering::Relaxed) {
        return;
    }
    let vector = crate::arch::x86_64::interrupts::EHCI_MSI_VECTOR;
    // Deliver to the BSP (core 0) - a legacy PCI INTx pin routes through the IOAPIC only to the
    // BSP on this hardware (unlike an MSI, which can target any core). The EHCI driver is pinned
    // to core 0 to match (task/mod.rs), so the keypress wakes core 0 from its idle hlt, deliver()
    // runs on core 0, and the wake to the core-0 driver is local - no cross-core wake (which an
    // idle, halted AP doesn't service promptly). Verified on the T630: INTx delivers to the BSP;
    // routing it to an AP's LAPIC id silently dropped it.
    let dest = crate::arch::x86_64::ioapic::bsp_lapic_id();
    let legacy = EHCI_IRQ.load(Ordering::Relaxed);

    // Legacy INTx only asserts the device's INTx# pin when PCI Command bit 10 (Interrupt
    // Disable) is CLEAR. If firmware left it set (common after MSI-style init elsewhere), even
    // a correct IOAPIC route delivers nothing. Clear it (and keep bus-master for the EHCI's DMA).
    {
        let bdf = EHCI_BDF.load(Ordering::Relaxed);
        let bus = ((bdf >> 8) & 0xFF) as u8;
        let dev = ((bdf >> 3) & 0x1F) as u8;
        let func = (bdf & 0x07) as u8;
        let cmd = config_read32(bus, dev, func, 0x04);
        // bit10 = Interrupt Disable (clear), bit2 = Bus Master (set), bit1 = Memory Space (set).
        let new = (cmd & !(1 << 10)) | (1 << 2) | (1 << 1);
        if new != cmd {
            config_write32(bus, dev, func, 0x04, new);
            crate::kprintln!("ehci: PCI command {:#06x} -> {:#06x} (INTx-disable cleared)",
                cmd & 0xFFFF, new & 0xFFFF);
        }
    }
    // Candidate GSIs: the legacy interrupt-line value (usually 11) + the AMD FCH PCI-INTx range.
    let mut candidates: [u8; 9] = [legacy, 16, 17, 18, 19, 20, 21, 22, 23];
    for i in 0..candidates.len() {
        let gsi = candidates[i];
        // Skip a duplicate if the legacy line already falls in 16..=23.
        if candidates[..i].contains(&gsi) {
            continue;
        }
        crate::arch::x86_64::ioapic::set_redir(gsi, vector, dest, false);
        crate::arch::x86_64::ioapic::set_level_route(vector, gsi);
    }
    let _ = &mut candidates;
    crate::kprintln!(
        "ehci: legacy INTx routed via IOAPIC candidates [{} ,16..=23] -> vector={:#x} dest_apic={}",
        legacy, vector, dest
    );
}

/// Scan the PCI bus for the xHCI controller and record its MMIO base + IRQ.
/// Called once on the BSP during boot. Logs the result either way.
pub fn init() {
    for bus in 0u16..256 {
        for dev in 0u8..32 {
            for func in 0u8..8 {
                let vendor = (config_read32(bus as u8, dev, func, 0x00) & 0xFFFF) as u16;
                if vendor == 0xFFFF {
                    continue; // no device/function present
                }
                let class_reg = config_read32(bus as u8, dev, func, 0x08);
                let class = (class_reg >> 24) as u8;
                let subclass = (class_reg >> 16) as u8;
                let progif = (class_reg >> 8) as u8;
                // Log EVERY USB host controller (subclass 0x03), of any kind, so
                // we can see the full USB topology - devices may live on a second
                // xHCI or an EHCI/OHCI the boot-port controller doesn't cover.
                if class == CLASS_SERIAL_BUS && subclass == SUBCLASS_USB {
                    // BAR0 (offset 0x10). 64-bit memory BAR if bits[2:1]=10.
                    let bar0 = config_read32(bus as u8, dev, func, 0x10);
                    let mmio_base = if bar0 & 0x6 == 0x4 {
                        let bar1 = config_read32(bus as u8, dev, func, 0x14);
                        ((bar1 as u64) << 32) | ((bar0 & 0xFFFF_FFF0) as u64)
                    } else {
                        (bar0 & 0xFFFF_FFF0) as u64
                    };
                    let irq = (config_read32(bus as u8, dev, func, 0x3C) & 0xFF) as u8;
                    let kind = match progif {
                        0x00 => "UHCI",
                        0x10 => "OHCI",
                        0x20 => "EHCI",
                        0x30 => "xHCI",
                        _ => "USB?",
                    };
                    crate::kprintln!(
                        "pci: {} at {:02x}:{:02x}.{} vendor={:#06x} MMIO={:#x} IRQ={}",
                        kind, bus, dev, func, vendor, mmio_base, irq
                    );
                    // Record every xHCI into the array.
                    if progif == PROGIF_XHCI {
                        let n = XHCI_COUNT.load(Ordering::Relaxed) as usize;
                        if n < 4 {
                            XHCI_BASES[n].store(mmio_base, Ordering::Relaxed);
                            XHCI_IRQS[n].store(irq, Ordering::Relaxed);
                            XHCI_BDFS[n].store(make_bdf(bus as u8, dev, func), Ordering::Relaxed);
                            XHCI_COUNT.store((n + 1) as u32, Ordering::Relaxed);
                        }
                    }
                    // Record the first EHCI controller (T630 back ports, §12).
                    if progif == PROGIF_EHCI && !EHCI_FOUND.load(Ordering::Relaxed) {
                        EHCI_MMIO_BASE.store(mmio_base, Ordering::Relaxed);
                        EHCI_IRQ.store(irq, Ordering::Relaxed);
                        EHCI_BDF.store(make_bdf(bus as u8, dev, func), Ordering::Relaxed);
                        EHCI_FOUND.store(true, Ordering::Relaxed);
                    }
                }
                // AHCI (SATA) controller - the block driver's disk (docs/ahci.md).
                if class == CLASS_MASS_STORAGE && subclass == SUBCLASS_SATA
                    && progif == PROGIF_AHCI && !AHCI_FOUND.load(Ordering::Relaxed)
                {
                    // ABAR is BAR5 (offset 0x24), a 32-bit memory BAR.
                    let bar5 = config_read32(bus as u8, dev, func, 0x24);
                    let abar = (bar5 & 0xFFFF_FFF0) as u64;
                    let irq = (config_read32(bus as u8, dev, func, 0x3C) & 0xFF) as u8;
                    AHCI_ABAR.store(abar, Ordering::Relaxed);
                    AHCI_IRQ.store(irq, Ordering::Relaxed);
                    AHCI_BDF.store(make_bdf(bus as u8, dev, func), Ordering::Relaxed);
                    AHCI_FOUND.store(true, Ordering::Relaxed);
                    crate::kprintln!(
                        "pci: AHCI at {:02x}:{:02x}.{} vendor={:#06x} ABAR={:#x} IRQ={}",
                        bus, dev, func, vendor, abar, irq
                    );
                }
                // Network controller (PCI class 0x02) - networking Phase 0 (docs/networking.md):
                // identify what NIC is present. Log EVERY one; record the first for the future nic-driver.
                if class == CLASS_NETWORK {
                    // The NIC's register space = its first MEMORY BAR. The e1000's is BAR0; the
                    // RTL8168's BAR0 is an I/O port and its MMIO lives in BAR2 - so SCAN the BARs for
                    // the first mapped memory BAR rather than assuming BAR0 (networking Phase 4).
                    let mut mmio_base = 0u64;
                    let mut off = 0x10u8;
                    while off <= 0x24 {
                        let bar = config_read32(bus as u8, dev, func, off);
                        let is_64 = bar & 0x6 == 0x4;
                        if bar & 0x1 == 0 && bar & 0xFFFF_FFF0 != 0 {
                            // a mapped 32- or 64-bit memory BAR
                            mmio_base = if is_64 {
                                let hi = config_read32(bus as u8, dev, func, off + 4);
                                ((hi as u64) << 32) | ((bar & 0xFFFF_FFF0) as u64)
                            } else {
                                (bar & 0xFFFF_FFF0) as u64
                            };
                            break;
                        }
                        off += if is_64 { 8 } else { 4 };
                    }
                    let irq    = (config_read32(bus as u8, dev, func, 0x3C) & 0xFF) as u8;
                    let vd     = config_read32(bus as u8, dev, func, 0x00);
                    let device = (vd >> 16) as u16;
                    crate::kprintln!(
                        "pci: NIC (network ctrl, subclass {:#04x}) at {:02x}:{:02x}.{} vendor={:#06x} device={:#06x} MMIO={:#x} IRQ={}",
                        subclass, bus, dev, func, vendor, device, mmio_base, irq
                    );
                    if !NIC_FOUND.load(Ordering::Relaxed) {
                        NIC_MMIO_BASE.store(mmio_base, Ordering::Relaxed);
                        NIC_IRQ.store(irq, Ordering::Relaxed);
                        NIC_BDF.store(make_bdf(bus as u8, dev, func), Ordering::Relaxed);
                        NIC_VENDOR_DEVICE.store(vd, Ordering::Relaxed);
                        NIC_FOUND.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    // Use the first xHCI for the driver. (Multiple xHCIs are recorded above; a
    // general design would enumerate every controller + device and bind by class.)
    let count = XHCI_COUNT.load(Ordering::Relaxed);
    if count > 0 {
        let pick = 0;
        let base = XHCI_BASES[pick].load(Ordering::Relaxed);
        let bdf = XHCI_BDFS[pick].load(Ordering::Relaxed);
        XHCI_MMIO_BASE.store(base, Ordering::Relaxed);
        XHCI_IRQ.store(XHCI_IRQS[pick].load(Ordering::Relaxed), Ordering::Relaxed);
        XHCI_BDF.store(bdf, Ordering::Relaxed);
        XHCI_FOUND.store(true, Ordering::Relaxed);
        crate::kprintln!(
            "pci: driver uses xHCI #{} of {} (MMIO={:#x} BDF={:02x}:{:02x}.{})",
            pick, count, base, (bdf >> 8) & 0xff, (bdf >> 3) & 0x1f, bdf & 0x7
        );
    } else {
        crate::kprintln!("pci: no xHCI controller found");
    }
}
