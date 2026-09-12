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

/// Whether the bring-up narrates itself.
///
/// **Off, and the reason is a screen.** Every line below earned its place while the display and the
/// USB block were being brought up: they are how a black television became a sequence of facts, and
/// several of them cost a boot each to think of. None of them earns its place on a machine that
/// works. This port printed seventy-odd lines through a forty-eight row console, so the television
/// could only ever show the tail of its own boot - and no other port prints anything like that many.
///
/// They are GATED rather than deleted, because the next person to meet a dark screen on this board
/// wants exactly these lines and should not have to invent them again. One `true` brings them all
/// back. What is never gated is a failure: those go straight to `super::print_str` below, so a
/// machine that does not come up says so at full volume whatever this is set to.
const VERBOSE: bool = false;

fn p_str(s: &str) {
    if VERBOSE {
        super::print_str(s);
    }
}

fn p_hex(v: u64) {
    if VERBOSE {
        super::print_hex(v);
    }
}

fn p_dec(v: u64) {
    if VERBOSE {
        super::print_dec(v);
    }
}


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

/// The PHY, and the two clocks that feed it.
///
/// **The controller's registers do not answer until the PHY's reference clock runs**, which is not
/// obvious from either driver and cost a boot to learn: the first version enabled the wrapper's six
/// clocks, released its five resets, and then hung the machine dead on the first read of the xHCI
/// window - a bus transaction with nothing to answer it. The wrapper's `apb` and `axi` clock the path
/// TO the controller; the controller's own domain runs on the 125 MHz reference the PHY provides.
const PHY_CLK_MODE: usize = 0x00;
const PHY_CLK_MODE_RX_NORMAL_PWR: u32 = 1 << 1;
const PHY_LS_KEEPALIVE: usize = 0x04;
const PHY_LS_KEEPALIVE_ENABLE: u32 = 1 << 4;
/// `usb_125m` is a plain DIVIDER off PLL0 in the system generator, with no enable bit - the same
/// category of register that made the display's `apb=FAIL` look like a hardware fault. PLL0 runs at
/// 1000 MHz, so 125 MHz is a divisor of eight.
const SYSCLK_USB_125M: usize = 0x5f;
const USB_125M_DIVISOR: u32 = 8;
/// `app_125m` in the system-top generator, which IS a gate.
const STGCLK_APP_125M: usize = 6;
/// In the SYSTEM syscon - a different one from the system-top syscon that holds the role strap, and
/// missing it is missing the wire between the USB 2.0 PHY and the controller.
const SYSCON_USB_SPLIT: usize = 0x18;
const USB_PDRSTN_SPLIT: u32 = 1 << 17;

/// The pin configuration the board needs before anything can be plugged into it.
///
/// **A hub that enumerates perfectly and reports nothing behind it is a hub with no VBUS.** The
/// device tree puts two pin groups on the USB node - `power-pins` and `switch-pins` - and nothing in
/// this port was programming them, so the downstream ports were whatever the boot ROM left them.
///
/// The packed value the tree gives is `din:31-24 | dout:23-16 | doen:15-10 | function:9-8 | pin:7-0`,
/// so `0xff01001a` is pin 26 driven HIGH with its output enabled and no input routed, and
/// `0xff00003e` is pin 62 driven LOW the same way. Those are the port power switch and the USB 2/3
/// mux. Decoded here from the tree's own numbers rather than restated as two magic addresses,
/// because the encoding is the thing worth writing down.
const SYS_PINCTRL_DOEN: usize = 0x000;
const SYS_PINCTRL_DOUT: usize = 0x040;
const SYS_PINCTRL_GPIOEN: usize = 0x0dc;
const PIN_DOUT_MASK: u32 = 0x7f;
const PIN_DOEN_MASK: u32 = 0x3f;
/// `(pin, dout, doen)` - output enabled is doen 0, and dout 1 is high.
const USB_PINS: [(u32, u32, u32, &str); 2] = [(26, 1, 0, "port power"), (62, 0, 0, "usb2/3 switch")];

static PINCTRL_BASE: AtomicU64 = AtomicU64::new(0);
static STGCRG_BASE: AtomicU64 = AtomicU64::new(0);
static STG_SYSCON_BASE: AtomicU64 = AtomicU64::new(0);
static XHCI_BASE: AtomicU64 = AtomicU64::new(0);
static SYSCRG_BASE: AtomicU64 = AtomicU64::new(0);
static SYS_SYSCON_BASE: AtomicU64 = AtomicU64::new(0);
static PHY_BASE: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_pinctrl_base(base: u64) {
    PINCTRL_BASE.store(base, Ordering::Relaxed);
}

/// Drive the two pins the board needs for its USB ports to have power.
///
/// One byte per pin in each of two register files, four pins to a word, which is why the offset is
/// `4 * (pin / 4)` and the shift `8 * (pin % 4)` - read, replace that byte's field, write back, so a
/// pin sharing a word with three others is not disturbed.
fn configure_pins() {
    let base = PINCTRL_BASE.load(Ordering::Relaxed);
    if base == 0 {
        super::print_str("riscv64: usb - no sys pinctrl; the ports cannot be powered
");
        return;
    }
    // The GPIO block's own enable, which the reference driver writes once at probe.
    mmio_write(base, SYS_PINCTRL_GPIOEN, 1);
    for (pin, dout, doen, _name) in USB_PINS {
        let off = 4 * (pin as usize / 4);
        let shift = 8 * (pin % 4);
        let d = mmio_read(base, SYS_PINCTRL_DOUT + off);
        mmio_write(
            base,
            SYS_PINCTRL_DOUT + off,
            (d & !(PIN_DOUT_MASK << shift)) | (dout << shift),
        );
        let e = mmio_read(base, SYS_PINCTRL_DOEN + off);
        mmio_write(
            base,
            SYS_PINCTRL_DOEN + off,
            (e & !(PIN_DOEN_MASK << shift)) | (doen << shift),
        );
    }
    p_str("riscv64: usb - port power and the usb2/3 switch driven
");
}

pub(super) fn set_bases(stgcrg: u64, syscon: u64, xhci: u64, syscrg: u64, sys_syscon: u64, phy: u64) {
    STGCRG_BASE.store(stgcrg, Ordering::Relaxed);
    STG_SYSCON_BASE.store(syscon, Ordering::Relaxed);
    XHCI_BASE.store(xhci, Ordering::Relaxed);
    SYSCRG_BASE.store(syscrg, Ordering::Relaxed);
    SYS_SYSCON_BASE.store(sys_syscon, Ordering::Relaxed);
    PHY_BASE.store(phy, Ordering::Relaxed);
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

    // THE PHY FIRST, which is a deliberate divergence from the reference and the reason is a boot.
    // Linux brings the wrapper up in its glue driver and the PHY later, when the controller core
    // probes and asks for it; done in that order here the machine hung dead on the first register
    // read, because the controller's own clock domain runs on the PHY's reference and a read into an
    // unclocked domain is a transaction nothing completes. So the PHY runs before the controller is
    // allowed out of reset.
    let syscrg = SYSCRG_BASE.load(Ordering::Relaxed);
    let sys_syscon = SYS_SYSCON_BASE.load(Ordering::Relaxed);
    let phy = PHY_BASE.load(Ordering::Relaxed);
    if syscrg == 0 || sys_syscon == 0 || phy == 0 {
        super::print_str("riscv64: usb - no PHY in the device tree; not touching the controller\n");
        XHCI_BASE.store(0, Ordering::Relaxed);
        return false;
    }
    // The CLOCKS and the connection, which is all the PHY needs before the controller can answer.
    // Its own registers come later, once its reset is off.
    mmio_write(syscrg, SYSCLK_USB_125M * 4, USB_125M_DIVISOR);
    let app = clk_enable(crg, STGCLK_APP_125M);
    mmio_write(
        sys_syscon,
        SYSCON_USB_SPLIT,
        mmio_read(sys_syscon, SYSCON_USB_SPLIT) | USB_PDRSTN_SPLIT,
    );
    p_str("riscv64: usb - phy: 125m=");
    p_hex(mmio_read(syscrg, SYSCLK_USB_125M * 4) as u64);
    p_str(" app_125m=");
    p_str(if app { "on" } else { "FAIL" });
    p_str(" split=");
    p_hex(mmio_read(sys_syscon, SYSCON_USB_SPLIT) as u64);
    p_str("
");

    // TRY THEM ALL, THEN DECIDE - the same rule the display's clocks follow, for the same reason: a
    // board boot is the expensive thing here, and stopping at the first failure spends one to learn
    // about one clock.
    p_str("riscv64: usb - clocks:");
    let mut ok = true;
    for (i, name) in STGCLK_USB {
        p_str(" ");
        p_str(name);
        if clk_enable(crg, i) {
            p_str("=on");
        } else {
            p_str("=FAIL");
            ok = false;
        }
    }
    p_str("\n");

    // THE ROLE IS SET BEFORE THE RESETS COME OFF. The strap is sampled as the controller leaves
    // reset, so a controller released first comes up as whatever the pins happened to say and has to
    // be told again - the kind of ordering that works on one board and not on the next.
    let mut v = mmio_read(syscon, STG_USB_MODE);
    v = (v & !USB_MISC_CFG_MASK) | USB_SUSPENDM_BYPS | USB_PLL_EN | USB_REFCLK_MODE;
    v = (v & !USB_STRAP_MASK) | USB_STRAP_HOST;
    v = (v & !USB_SUSPENDM_MASK) | USB_SUSPENDM_HOST;
    mmio_write(syscon, STG_USB_MODE, v);
    p_str("riscv64: usb - strapped as a HOST, mode register ");
    p_hex(mmio_read(syscon, STG_USB_MODE) as u64);
    p_str("\n");

    p_str("riscv64: usb - resets:");
    for (id, name) in STGRST_USB {
        p_str(" ");
        p_str(name);
        if reset_deassert(crg, STGCRG_RESET_ASSERT, STGCRG_RESET_STATUS, id) {
            p_str("=released");
        } else {
            p_str("=STUCK");
            ok = false;
        }
    }
    p_str("\n");

    // NOW THE PHY'S OWN REGISTERS, and they are here rather than above because the board said so.
    // Written before the resets came off they read back as ZERO - `mode=0x0 keepalive=0x0` - since a
    // register in a block still held in reset accepts nothing. The controller answered regardless,
    // its own registers being on a different domain, so this would have passed for success and failed
    // later at the one thing it governs: LOW SPEED. The keep-alive is what a host drives to hold a
    // low-speed device awake, and a keyboard is usually a low-speed device - so the register that was
    // silently lost is the one this entire stage exists for.
    mmio_write(phy, PHY_CLK_MODE, mmio_read(phy, PHY_CLK_MODE) | PHY_CLK_MODE_RX_NORMAL_PWR);
    mmio_write(phy, PHY_LS_KEEPALIVE, mmio_read(phy, PHY_LS_KEEPALIVE) | PHY_LS_KEEPALIVE_ENABLE);
    p_str("riscv64: usb - phy registers: mode=");
    p_hex(mmio_read(phy, PHY_CLK_MODE) as u64);
    p_str(" keepalive=");
    p_hex(mmio_read(phy, PHY_LS_KEEPALIVE) as u64);
    p_str("\n");

    // DOES IT ANSWER? An xHCI controller's first register holds its capability-structure length and
    // its interface version; the next says how many ports and device slots it has. A block that is
    // powered, clocked and out of reset returns a small length and a version of 0x0100 or better. One
    // that is not returns all-ones or all-zeros, and those are exactly the values that would let a
    // driver start and then fail somewhere unrelated.
    // SAID BEFORE IT IS DONE, because this read is the one thing here that can take the machine with
    // it. An access into a domain whose clock is not running does not fault on this interconnect - it
    // stalls, forever, with no output and nothing to look at. The line below is what turned last
    // boot's silent black screen into a fact about which instruction did it.
    super::print_str("riscv64: usb - reading the controller's capability registers\n");
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
    configure_pins();
    super::print_str("riscv64: usb - controller alive; the window is offered to the driver\n");
    true
}
