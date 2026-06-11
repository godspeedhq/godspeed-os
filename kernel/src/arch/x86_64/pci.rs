//! Minimal PCI enumeration (§12) — Stage 1 of the USB stack.
//!
//! Uses legacy PCI configuration mechanism #1 (port `0xCF8` address / `0xCFC`
//! data) to scan the bus and locate the xHCI USB host controller. The
//! discovered MMIO base and IRQ line are recorded so the kernel can later mint
//! an `hw_mmio` + `hw_interrupt` capability for the userspace `xhci` driver
//! service (§12.3) — the driver owns the controller; the kernel only routes its
//! interrupt and grants register access.
//!
//! Port I/O is hardware access, so this lives in the arch layer (§18.1).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

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
/// PCI BDF (bus<<8 | dev<<3 | func) of the driver's xHCI — the index into the
/// IOMMU device table for DMA confinement (H1). 0xFFFF if none found.
pub static XHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);

/// All discovered xHCI controllers (a system may have several; the boot drive
/// and the keyboard often sit on different ones). Recorded during the scan.
pub static XHCI_COUNT: AtomicU32 = AtomicU32::new(0);
pub static XHCI_BASES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
pub static XHCI_IRQS: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];
pub static XHCI_BDFS: [AtomicU32; 4] = [const { AtomicU32::new(0xFFFF) }; 4];

/// Discovered EHCI (USB 2.0) controller — the T630's back ports hang off it
/// (§12). The userspace `ehci` driver gets this BAR mapped at spawn, exactly as
/// the `xhci` driver gets the xHCI BAR. First EHCI found wins.
pub static EHCI_FOUND: AtomicBool = AtomicBool::new(false);
pub static EHCI_MMIO_BASE: AtomicU64 = AtomicU64::new(0);
pub static EHCI_IRQ: AtomicU8 = AtomicU8::new(0);
/// PCI BDF of the EHCI controller — IOMMU device-table index (H1). 0xFFFF if none.
pub static EHCI_BDF: AtomicU32 = AtomicU32::new(0xFFFF);

/// Build a 16-bit PCI BDF (bus<<8 | dev<<3 | func) — the IOMMU device-table index.
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
    // SAFETY: `out dx, eax` — standard 32-bit port write.
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
    // SAFETY: `in eax, dx` — standard 32-bit port read.
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
    // SAFETY: standard PCI config mechanism #1; ring-0 port I/O.
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
    // SAFETY: standard PCI config mechanism #1; ring-0 port I/O.
    unsafe {
        outl(CONFIG_ADDRESS, addr);
        outl(CONFIG_DATA, val);
    }
}

/// Take ownership of the EHCI controller from the firmware (BIOS→OS handoff).
///
/// EHCI's HCCPARAMS register (MMIO capability offset 0x08) carries the EHCI
/// Extended Capabilities Pointer (EECP) — an offset into *PCI config space*
/// (not MMIO) where a capability list lives. The USB Legacy Support capability
/// (ID 0x01) has a BIOS-Owned and an OS-Owned semaphore. Until the OS sets
/// OS-Owned and the firmware clears BIOS-Owned, the firmware (SMM) keeps poking
/// the controller and running its own periodic schedule out of firmware memory.
///
/// Without an IOMMU that firmware DMA was invisible. With H1 confinement it
/// faults (the firmware buffers are outside the driver's arena) and breaks the
/// back-port keyboard. This handoff makes the firmware release the controller —
/// standard OS behaviour at init. EECP lives in PCI config space, which the
/// userspace `ehci` driver cannot reach, so the kernel must do it. Idempotent;
/// no-op if no EHCI, no extended caps, or no USB Legacy Support capability.
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
                // we can see the full USB topology — devices may live on a second
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
