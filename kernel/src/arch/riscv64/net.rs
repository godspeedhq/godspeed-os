// SPDX-License-Identifier: GPL-2.0-only
//! ROLE: discovery
//!
//! Establishing that this board has an ethernet controller, and where it is.
//!
//! **This file does not drive ethernet, for the same reason `usb.rs` does not drive USB.** The board
//! has two Synopsys DesignWare MACs; the device tree enables one, at 0x1603_0000, wired RGMII to a
//! PHY at MDIO address 0. Bringing a MAC up - descriptor rings, MDIO, PHY negotiation, RGMII timing -
//! is a driver, and drivers are services (CLAUDE.md 4.4). What the kernel owes is to power the block,
//! release it, check that it answers, and say where it is.
//!
//! The clocks are split across two generators and that is worth stating, because it is the sort of
//! thing that reads as a mistake later: the MAC's bus clocks live in the ALWAYS-ON generator at
//! 0x1700_0000 (this controller is in the always-on domain), while its transmit and timestamp clocks
//! come from the system generator. Both are named by the device tree rather than assumed here.
//!
//! Facts from the device tree and from Linux's `clk-starfive-jh7110-aon.c` and
//! `reset-starfive-jh7110.c`, read as executable datasheets (26.14).

use core::sync::atomic::Ordering;
use portable_atomic::AtomicU64;

use super::display::{clk_enable, mmio_read, mmio_write, reset_deassert};

/// The always-on generator's gates for this MAC: its AHB (register) and AXI (data) clocks. Register
/// access needs both, which is why these two are the ones that must succeed.
const AONCLK_GMAC0_AHB: usize = 2;
const AONCLK_GMAC0_AXI: usize = 3;
/// The transmit clock, which this SoC provides as an INVERTER rather than a gate.
///
/// **It has no enable bit, and reporting it as a failed gate was wrong.** The device tree names this
/// index as the MAC's `tx` clock, and Linux registers it as `JH71X0__INV(GMAC0_TX_INV)` - an
/// inverter, whose register carries a phase bit and no enable. So writing bit 31 and reading it back
/// can only ever report absence, which this stage printed for several boots as `tx=FAIL` next to a
/// controller that was working perfectly. An instrument that cannot succeed is not a measurement,
/// and a FAIL nobody can act on trains the reader to ignore the line it is printed on.
///
/// Same class as the display's pixel divider, which this port already documents for the same
/// reason: `JH71X0__DIV` entries hold a divisor and no enable bit either.
const AONCLK_GMAC0_TX_INV: usize = 6;
/// In the SYSTEM generator: the gigabit transmit clock and the timestamp reference.
const SYSCLK_GMAC0_GTXCLK: usize = 108;
const SYSCLK_GMAC0_PTP: usize = 109;
const SYSCLK_GMAC0_GTXC: usize = 111;

/// The always-on generator keeps its resets here - a different pair of offsets from the system and
/// video-out generators, which is exactly why they are looked up per generator rather than shared.
const AONCRG_RESET_ASSERT: usize = 0x38;
const AONCRG_RESET_STATUS: usize = 0x3c;
/// Reset ids, from the device tree's `reset-names`: `stmmaceth` and `ahb`.
const AONRST_GMAC0_AXI: u32 = 0;
const AONRST_GMAC0_AHB: u32 = 1;

/// The MAC's version register. DesignWare MAC 4 and 5 put it here; the 3.x parts put it at 0x20, and
/// reading the wrong one of those is how a present controller reports itself absent. The device tree
/// says `snps,dwmac-5.20`, so this is the 4/5 register map.
const GMAC_VERSION: usize = 0x110;

/// The system controller, and where in it this MAC's interface mode is selected.
///
/// **Board glue, and without it no frame can move whatever the driver does.** The MAC is a
/// licensable core wired to pads by the integrator, and which pads - RGMII, RMII, SGMII - is a
/// three-bit field outside the controller entirely. The device tree states it as
/// `starfive,syscon = <&sys_syscon 0xc 0x12>`: register 0xc of the system controller, field at bit
/// 18. Linux's `dwmac-starfive.c` writes `phy_intf_sel` there and does nothing else for this SoC.
///
/// It belongs HERE and not in the driver for the same reason the USB host strap does: it is a fact
/// about how this board is wired, not about how a MAC is driven, and the driver is granted the
/// controller's window and nothing else. A driver that had to reach the system controller to work
/// would need authority over every other block behind it (Commandment VII).
const SYSCON_PHY_INTF: usize = 0x0c;
const SYSCON_PHY_INTF_SHIFT: u32 = 18;
const SYSCON_PHY_INTF_MASK: u32 = 0x7;
/// `phy_intf_sel` for RGMII, from `stmmac_get_phy_intf_sel`. The device tree says `rgmii-id`, which
/// is RGMII with the delays applied inside the PHY - the same interface as far as this field is
/// concerned, since the delay is the PHY's business and not the pad mux's.
const PHY_INTF_SEL_RGMII: u32 = 1;

static AONCRG_BASE: AtomicU64 = AtomicU64::new(0);
static SYSCRG_BASE: AtomicU64 = AtomicU64::new(0);
static MAC_BASE: AtomicU64 = AtomicU64::new(0);
static SYSCON_BASE: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_bases(aoncrg: u64, syscrg: u64, mac: u64, syscon: u64) {
    AONCRG_BASE.store(aoncrg, Ordering::Relaxed);
    SYSCRG_BASE.store(syscrg, Ordering::Relaxed);
    MAC_BASE.store(mac, Ordering::Relaxed);
    SYSCON_BASE.store(syscon, Ordering::Relaxed);
}

/// Where the MAC's register window is, or zero if there is no controller to offer.
///
/// The whole interface between this file and the rest of the system, exactly as `usb::window()` is.
/// Zero means the spawn is refused rather than a driver being handed a window that answers with
/// nothing.
pub fn window() -> u64 {
    MAC_BASE.load(Ordering::Relaxed)
}

/// Bring the ethernet MAC up far enough to answer, and say whether it did.
pub fn init() -> bool {
    let aon = AONCRG_BASE.load(Ordering::Relaxed);
    let sys = SYSCRG_BASE.load(Ordering::Relaxed);
    let mac = MAC_BASE.load(Ordering::Relaxed);
    if aon == 0 || sys == 0 || mac == 0 {
        super::print_str("riscv64: net - not described by the device tree; no controller\n");
        MAC_BASE.store(0, Ordering::Relaxed);
        return false;
    }

    // THE TWO THAT MUST WORK. Everything else on this block is about moving frames; these two are
    // what make the register window answer at all, and a read into an unclocked block on this
    // interconnect stalls rather than faulting - which took the machine down once already during the
    // USB stage and is not worth repeating.
    let ahb = clk_enable(aon, AONCLK_GMAC0_AHB);
    let axi = clk_enable(aon, AONCLK_GMAC0_AXI);
    if !ahb || !axi {
        super::print_str("riscv64: net - the MAC's bus clocks did not enable (ahb=");
        super::print_str(if ahb { "on" } else { "FAIL" });
        super::print_str(" axi=");
        super::print_str(if axi { "on" } else { "FAIL" });
        super::print_str("); not touching the controller\n");
        MAC_BASE.store(0, Ordering::Relaxed);
        return false;
    }

    // The rest, best-effort and reported: they carry frames rather than register accesses, so a
    // failure here is a fact the driver stage needs rather than a reason to stop this one.
    // Poked, not tested: see the constant. The write is harmless on an inverter and keeps the call
    // in one place if this index ever becomes a real gate; what is dropped is the CLAIM about it.
    clk_enable(aon, AONCLK_GMAC0_TX_INV);
    let gtxclk = clk_enable(sys, SYSCLK_GMAC0_GTXCLK);
    let gtxc = clk_enable(sys, SYSCLK_GMAC0_GTXC);
    let ptp = clk_enable(sys, SYSCLK_GMAC0_PTP);

    let mut ok = true;
    for (id, name) in [(AONRST_GMAC0_AXI, "stmmaceth"), (AONRST_GMAC0_AHB, "ahb")] {
        if !reset_deassert(aon, AONCRG_RESET_ASSERT, AONCRG_RESET_STATUS, id) {
            super::print_str("riscv64: net - MAC reset STUCK: ");
            super::print_str(name);
            super::print_str("\n");
            ok = false;
        }
    }
    if !ok {
        MAC_BASE.store(0, Ordering::Relaxed);
        return false;
    }

    // DOES IT ANSWER? The version register's low byte is the Synopsys release: 0x52 is 5.20, which is
    // what the device tree claims this part is. All-ones or all-zeros is a block that is not there,
    // and those are precisely the values that would let a driver start and fail somewhere unrelated.
    super::print_str("riscv64: net - reading the MAC's version register\n");
    let ver = mmio_read(mac, GMAC_VERSION);
    let snps = ver & 0xff;
    super::print_str("riscv64: net - dwmac @");
    super::print_hex(mac);
    super::print_str(": version ");
    super::print_hex(ver as u64);
    super::print_str(" (snps ");
    super::print_dec((snps >> 4) as u64);
    super::print_str(".");
    super::print_dec(((snps & 0xf) * 10) as u64);
    super::print_str("), gtxclk=");
    super::print_str(if gtxclk { "on" } else { "FAIL" });
    super::print_str(" gtxc=");
    super::print_str(if gtxc { "on" } else { "FAIL" });
    super::print_str(" ptp=");
    super::print_str(if ptp { "on" } else { "FAIL" });
    super::print_str("\n");

    if ver == 0 || ver == 0xffff_ffff || snps == 0 {
        super::print_str("riscv64: net - the controller is not answering; none offered to userspace\n");
        MAC_BASE.store(0, Ordering::Relaxed);
        return false;
    }
    // SELECT THE INTERFACE. Last, because it is the one write this stage makes to anything other
    // than a clock or a reset, and because doing it before the controller has answered would be
    // configuring a block that might not be there.
    let syscon = SYSCON_BASE.load(Ordering::Relaxed);
    if syscon == 0 {
        super::print_str("riscv64: net - no sys-syscon in the device tree, so the interface mode
");
        super::print_str("riscv64: net - cannot be selected; the MAC is offered but frames may not move
");
    } else {
        let v = mmio_read(syscon, SYSCON_PHY_INTF);
        let want = (v & !(SYSCON_PHY_INTF_MASK << SYSCON_PHY_INTF_SHIFT))
            | (PHY_INTF_SEL_RGMII << SYSCON_PHY_INTF_SHIFT);
        mmio_write(syscon, SYSCON_PHY_INTF, want);
        // READ IT BACK. A syscon field that is write-protected, or shifted by one, fails silently and
        // presents later as a MAC that transmits into nothing - a full day of driver debugging for a
        // register that never took the value.
        let got = (mmio_read(syscon, SYSCON_PHY_INTF) >> SYSCON_PHY_INTF_SHIFT) & SYSCON_PHY_INTF_MASK;
        super::print_str("riscv64: net - interface select = ");
        super::print_dec(got as u64);
        if got == PHY_INTF_SEL_RGMII {
            super::print_str(" (RGMII, as asked)
");
        } else {
            super::print_str(" but RGMII is 1 - the field did NOT take; frames will not move
");
        }
    }

    super::print_str("riscv64: net - controller alive; the window is offered to the driver
");
    true
}
