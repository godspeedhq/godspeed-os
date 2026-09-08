// SPDX-License-Identifier: GPL-2.0-only
//! ROLE: discovery
//!
//! Establishing that this board has a USB host controller, and where it is.
//!
//! **This file does not drive USB, and the line is the point.** The board's controller is a Cadence
//! USB3 dual-role part behind a StarFive wrapper, and the wrapper is all there is here: six clocks,
//! five resets, and three fields in a system register that decide whether the port is a host or a
//! device. What comes out the other side is an xHCI controller at a known address - and GodspeedOS
//! already has an xHCI DRIVER, as a userspace service, proven on two other machines. So the kernel's
//! whole involvement is to power the block, strap it as a host, check that it answers, and say so;
//! everything that parses a descriptor supplied by whatever was plugged in stays in userspace, which
//! is what CLAUDE.md 6.4 spent 2742 lines of deleted ring-0 code establishing.
//!
//! Facts from StarFive's `cdns3-starfive.c`, read as an executable datasheet (26.14). What is
//! borrowed is the register layout and the ORDER - particularly that the role strap is sampled as
//! the controller leaves reset, so it has to be set first. What is not borrowed is the shape: Linux
//! models this as a glue driver that instantiates a generic core through the device model, and here
//! it is a dozen writes at boot, because the kernel either finds a controller or reports that it did
//! not.

use core::sync::atomic::Ordering;
use portable_atomic::AtomicU64;

use super::display::{clk_enable, mmio_read, mmio_write, reset_deassert};

/// Clock indices in the system-top clock generator, in the order the device tree's `clock-names`
/// gives them on the USB wrapper: `lpm`, `stb`, `apb`, `axi`, `utmi_apb`, `phy`.
const STGCLK_USB: [(usize, &str); 6] =
    [(4, "lpm"), (5, "stb"), (1, "apb"), (3, "axi"), (2, "utmi_apb"), (9, "phy")];

/// Reset ids in the same generator, from `reset-names`: `pwrup`, `apb`, `axi`, `utmi_apb`, `phy`.
const STGRST_USB: [(u32, &str); 5] =
    [(0x0a, "pwrup"), (8, "apb"), (7, "axi"), (9, "utmi_apb"), (0x10, "phy")];

/// The system-top generator keeps its resets here - the same shape as the other two generators on
/// this SoC: one bit per id in an assert word, and a status word that follows once the hardware
/// agrees.
const STGCRG_RESET_ASSERT: usize = 0x74;
const STGCRG_RESET_STATUS: usize = 0x78;

/// The register that decides what the port IS. Both the syscon and this offset are named by the
/// device tree (`starfive,stg-syscon = <phandle 0x4>`), so neither is a constant here by choice.
const STG_USB_MODE: usize = 0x04;
/// Bits 23:20 configure the reference clock and the internal PLL; bits 18:16 strap the role; bit 19
/// is the suspend line, which a host drives and a device does not.
const USB_MISC_CFG_MASK: u32 = 0x0f << 20;
const USB_SUSPENDM_BYPS: u32 = 1 << 20;
const USB_PLL_EN: u32 = 1 << 22;
const USB_REFCLK_MODE: u32 = 1 << 23;
const USB_STRAP_MASK: u32 = 0x07 << 16;
const USB_STRAP_HOST: u32 = 1 << 17;
const USB_SUSPENDM_MASK: u32 = 1 << 19;
const USB_SUSPENDM_HOST: u32 = 1 << 19;

static STGCRG_BASE: AtomicU64 = AtomicU64::new(0);
static STG_SYSCON_BASE: AtomicU64 = AtomicU64::new(0);
static XHCI_BASE: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_bases(stgcrg: u64, syscon: u64, xhci: u64) {
    STGCRG_BASE.store(stgcrg, Ordering::Relaxed);
    STG_SYSCON_BASE.store(syscon, Ordering::Relaxed);
    XHCI_BASE.store(xhci, Ordering::Relaxed);
}

/// Where the xHCI register window is, or zero if there is no controller to offer.
///
/// **This is the whole interface between this file and the rest of the system.** The `xhci` service
/// needs an address and a DMA arena; it does not need a bus, and it does not need to know that on
/// this board the controller is soldered to the SoC rather than plugged into one. Zero here means the
/// spawn is refused rather than a service being handed a window that answers with nothing - a
/// distinction the display stage learned the expensive way.
pub fn window() -> u64 {
    XHCI_BASE.load(Ordering::Relaxed)
}

/// Bring the USB block up far enough that its xHCI half answers, and say whether it did.
///
/// Deliberately stops at "answers". Reading the controller's own capability registers back before
/// offering the window costs two register reads and means a controller that never came up is
/// reported HERE, at its cause, rather than as a driver two layers away failing to find anything.
pub fn init() -> bool {
    let crg = STGCRG_BASE.load(Ordering::Relaxed);
    let syscon = STG_SYSCON_BASE.load(Ordering::Relaxed);
    let xhci = XHCI_BASE.load(Ordering::Relaxed);
    if crg == 0 || syscon == 0 || xhci == 0 {
        super::print_str("riscv64: usb - not described by the device tree; no controller\n");
        XHCI_BASE.store(0, Ordering::Relaxed);
        return false;
    }

    // TRY THEM ALL, THEN DECIDE - the same rule the display's clocks follow, for the same reason: a
    // board boot is the expensive thing here, and stopping at the first failure spends one to learn
    // about one clock.
    super::print_str("riscv64: usb - clocks:");
    let mut ok = true;
    for (i, name) in STGCLK_USB {
        super::print_str(" ");
        super::print_str(name);
        if clk_enable(crg, i) {
            super::print_str("=on");
        } else {
            super::print_str("=FAIL");
            ok = false;
        }
    }
    super::print_str("\n");

    // THE ROLE IS SET BEFORE THE RESETS COME OFF. The strap is sampled as the controller leaves
    // reset, so a controller released first comes up as whatever the pins happened to say and has to
    // be told again - the kind of ordering that works on one board and not on the next.
    let mut v = mmio_read(syscon, STG_USB_MODE);
    v = (v & !USB_MISC_CFG_MASK) | USB_SUSPENDM_BYPS | USB_PLL_EN | USB_REFCLK_MODE;
    v = (v & !USB_STRAP_MASK) | USB_STRAP_HOST;
    v = (v & !USB_SUSPENDM_MASK) | USB_SUSPENDM_HOST;
    mmio_write(syscon, STG_USB_MODE, v);
    super::print_str("riscv64: usb - strapped as a HOST, mode register ");
    super::print_hex(mmio_read(syscon, STG_USB_MODE) as u64);
    super::print_str("\n");

    super::print_str("riscv64: usb - resets:");
    for (id, name) in STGRST_USB {
        super::print_str(" ");
        super::print_str(name);
        if reset_deassert(crg, STGCRG_RESET_ASSERT, STGCRG_RESET_STATUS, id) {
            super::print_str("=released");
        } else {
            super::print_str("=STUCK");
            ok = false;
        }
    }
    super::print_str("\n");

    // DOES IT ANSWER? An xHCI controller's first register holds its capability-structure length and
    // its interface version; the next says how many ports and device slots it has. A block that is
    // powered, clocked and out of reset returns a small length and a version of 0x0100 or better. One
    // that is not returns all-ones or all-zeros, and those are exactly the values that would let a
    // driver start and then fail somewhere unrelated.
    let cap = mmio_read(xhci, 0x00);
    let hcsparams1 = mmio_read(xhci, 0x04);
    super::print_str("riscv64: usb - xhci @");
    super::print_hex(xhci);
    super::print_str(": caplength=");
    super::print_hex((cap & 0xff) as u64);
    super::print_str(" version=");
    super::print_hex((cap >> 16) as u64);
    super::print_str(" ports=");
    super::print_dec(((hcsparams1 >> 24) & 0xff) as u64);
    super::print_str(" slots=");
    super::print_dec((hcsparams1 & 0xff) as u64);
    super::print_str("\n");

    let version = (cap >> 16) & 0xffff;
    if !ok || cap == 0 || cap == 0xffff_ffff || version < 0x0100 {
        super::print_str("riscv64: usb - the controller is not answering; none offered to userspace\n");
        XHCI_BASE.store(0, Ordering::Relaxed);
        return false;
    }
    super::print_str("riscv64: usb - controller alive; the window is offered to the driver\n");
    true
}
