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
/// **The transmit clock's GATE, and the one that was missing.**
///
/// The first frame-path boot reported the MAC's DMA software reset still set after a full second,
/// and reported it as `0x00000001` BEFORE anything was written - so the block was already held in
/// reset at boot and stayed there. A DWMAC completes that reset only when all of its clocks are
/// running, and this is the one that was not.
///
/// The mistake was reading a name instead of a clock TREE. Linux's `clk-starfive-jh7110-aon.c`
/// registers index 6 as `JH71X0__INV(gmac0_tx_inv)` whose PARENT is index 5,
/// `JH71X0_GMUX(gmac0_tx)` - a gated mux. Enabling the inverter in Linux propagates up and turns the
/// parent gate on; `clk_enable` here pokes exactly one register and propagates nothing, so index 6
/// was written (harmlessly, it has no enable bit) and index 5 - the actual gate - was never touched.
///
/// The PARENT is the board's choice, not a default: this board's device tree carries
/// `assigned-clocks = <&aoncrg 5>` with `assigned-clock-parents = <&aoncrg 4>`, which selects
/// `GMAC0_RMII_RTX` rather than the internal `GMAC0_GTXCLK`. That is the hardware meaning of the
/// `starfive,tx-use-rgmii-clk` property on the same node: the PHY drives the transmit clock, so the
/// SoC must take it from the external path. Picking `GTXCLK` because 125 MHz is what gigabit RGMII
/// "should" use would be substituting an expectation for the board in front of us (26.14).
const AONCLK_GMAC0_TX: usize = 5;
/// Parent ORDINAL, not a clock index: the mux field selects among a clock's own parent list, and
/// `gmac0_tx`'s is `[GMAC0_GTXCLK, GMAC0_RMII_RTX]`.
///
/// **GTXCLK, and this is a deliberate divergence from the device tree, which asks for RMII_RTX.**
///
/// The tree carries `assigned-clock-parents = <&aoncrg 4>`, index 4 being `GMAC0_RMII_RTX`, and that
/// is what was set. What the hardware then reported, over several boots:
/// - the MAC accepts frames and writes back `tdes3 = no-error` - it transmitted them cleanly;
/// - the PHY reports link up at 1000 Mbit/s full duplex and RECEIVES perfectly (frames scanned
///   climbing, zero CRC errors);
/// - and nothing on the network ever answers, including a real gateway ARPed directly.
///
/// Frames leaving a clean MAC and dying before the wire is a transmit-clock problem, and RMII_RTX
/// descends from `gmac0_rmii_refin` - an RMII reference. RGMII at gigabit needs 125 MHz, which is
/// what `GMAC0_GTXCLK` is for. A clock at an RMII rate explains every observation at once: with
/// store-and-forward and a 286-byte frame in a 2 KiB FIFO it drains slowly and NEVER underflows
/// (which is why the zero underflow count did not refute this, as I first claimed), the descriptor
/// completes without error, and the PHY samples at its own 125 MHz and sees nothing it can use.
/// Receive is untouched because it is clocked by the PHY's own RXIN.
///
/// So this is an EXPERIMENT with a binary outcome, recorded as one rather than presented as a fix:
/// if ARP replies appear, the tree's parent is not what this board needs and the reason wants
/// finding. If nothing changes, the clock is exonerated and the RGMII TX delay is next. Either way
/// the log says which, and 26.14 is the licence: the device tree is a claim about the hardware, and
/// where the hardware disagrees the hardware wins.
const GMAC0_TX_PARENT_GTXCLK: u32 = 0;
/// ...and the parent the DEVICE TREE actually assigns: `JH71X0_GMUX(gmac0_tx)` lists its parents as
/// `[GMAC0_GTXCLK, GMAC0_RMII_RTX]`, so `assigned-clock-parents = <&aoncrg 4>` - `GMAC0_RMII_RTX` -
/// is mux index 1.
///
/// **The experiment above was scored wrong.** It said: "if ARP replies appear, the tree's parent is
/// not what this board needs". ARP replies appear about FOUR TIMES IN TEN, and a clock that is nearly
/// right is exactly what produces a partial result - so a half-answer was read as the pass branch and
/// `GTXCLK` was kept on the strength of it.
///
/// The argument for `GTXCLK` is still the honest one and is recorded rather than deleted:
/// `GMAC0_RMII_RTX` is `JH71X0__DIV(..., 30, GMAC0_RMII_REFIN)`, and this board's device tree declares
/// `gmac0_rmii_refin` as a fixed 50 MHz while gigabit RGMII needs 125 MHz, which no divisor of 50 can
/// reach. Against that stands the board's own explicit assignment plus `starfive,tx-use-rgmii-clk`,
/// whose meaning Linux states outright: the transmit clock comes from the external source and "there
/// is no need to configure the clock internally, because rgmii_rxin will be adaptively adjusted" -
/// which is why `fix_mac_speed` is deliberately not installed on this board. A `fixed-clock` node is
/// a nominal declaration, not a measurement of what a pin carries in a mode it was not named for.
///
/// So the tie goes to the board (26.14), and this time the outcome is scored honestly in advance:
/// LOSS COLLAPSES means the tree was right; TRANSMIT DIES OUTRIGHT, with the link still negotiating
/// 1000 Mbit over MDIO, means 50 MHz is real and `GTXCLK` goes back with the question settled instead
/// of assumed. Partial improvement is NOT a pass - that is the mistake being corrected here.
const GMAC0_TX_PARENT_RMII_RTX: u32 = 1;
/// Mux select, bits 27:24 of a JH71x0 clock register - the same field the display's pixel clock uses,
/// and written down in one place here so the two cannot drift.
const CLK_MUX_MASK: u32 = 0x0f << 24;
const CLK_MUX_SHIFT: u32 = 24;
const CLK_ENABLE: u32 = 1 << 31;

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
    // THE TRANSMIT CLOCK, gate and mux together, and before the resets are released - a block whose
    // clocks arrive after its reset is deasserted is a block that has already decided it is broken.
    //
    // Read-modify-write rather than a bare store: the divisor lives in the low bits of this same
    // register and is the integrator's business, not ours.
    let txv = mmio_read(aon, AONCLK_GMAC0_TX * 4);
    mmio_write(
        aon,
        AONCLK_GMAC0_TX * 4,
        (txv & !CLK_MUX_MASK) | (GMAC0_TX_PARENT_RMII_RTX << CLK_MUX_SHIFT) | CLK_ENABLE,
    );
    let txr = mmio_read(aon, AONCLK_GMAC0_TX * 4);
    // Poked, not tested: an inverter has no enable bit, so the write is harmless and the CLAIM about
    // it is what was dropped. Kept so the call sits next to the gate it belongs to.
    clk_enable(aon, AONCLK_GMAC0_TX_INV);
    let gtxclk = clk_enable(sys, SYSCLK_GMAC0_GTXCLK);
    let gtxc = clk_enable(sys, SYSCLK_GMAC0_GTXC);
    let ptp = clk_enable(sys, SYSCLK_GMAC0_PTP);

    // SELECT THE INTERFACE, BEFORE THE RESETS COME OFF.
    //
    // The order is the reference's: `starfive_dwmac_probe` writes this syscon field and only then
    // hands over to `stmmac_dvr_probe`, which is what deasserts the block. A MAC that samples its
    // interface mode as it leaves reset would sample the wrong one if this came after, and the
    // symptom of that is not an error - it is a controller that comes up in a mode nothing on the
    // board speaks. This used to run last, after the version read, because reporting a live
    // controller first read better; that is a reason about the LOG, not about the hardware.
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
    super::print_str("), tx-gate ");
    super::print_str(if txr & CLK_ENABLE != 0 { "on" } else { "FAIL" });
    super::print_str(" parent ");
    super::print_dec(((txr & CLK_MUX_MASK) >> CLK_MUX_SHIFT) as u64);
    // AS FOUND, before this code touched it. The line used to print only the value we had just
    // written, which can only ever agree with itself - so it has never once reported what the board
    // came up with, and that is the number that says whether firmware had an opinion here at all.
    super::print_str(" (was ");
    super::print_dec(((txv & CLK_MUX_MASK) >> CLK_MUX_SHIFT) as u64);
    super::print_str(", want 1 = rmii_rtx per the device tree), gtxclk=");
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
    super::print_str("riscv64: net - controller alive; the window is offered to the driver
");
    true
}
