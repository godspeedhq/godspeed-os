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
