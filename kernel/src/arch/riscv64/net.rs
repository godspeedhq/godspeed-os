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

use super::display::{clk_enable, mmio_read, reset_deassert};

/// The always-on generator's gates for this MAC: its AHB (register) and AXI (data) clocks. Register
/// access needs both, which is why these two are the ones that must succeed.
const AONCLK_GMAC0_AHB: usize = 2;
const AONCLK_GMAC0_AXI: usize = 3;
/// The transmit clock's inverter. Enabled best-effort: it matters for moving frames, not for
/// answering a register read, so a failure here is reported and does not stop the stage.
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

static AONCRG_BASE: AtomicU64 = AtomicU64::new(0);
static SYSCRG_BASE: AtomicU64 = AtomicU64::new(0);
static MAC_BASE: AtomicU64 = AtomicU64::new(0);

pub(super) fn set_bases(aoncrg: u64, syscrg: u64, mac: u64) {
    AONCRG_BASE.store(aoncrg, Ordering::Relaxed);
    SYSCRG_BASE.store(syscrg, Ordering::Relaxed);
    MAC_BASE.store(mac, Ordering::Relaxed);
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
    let tx = clk_enable(aon, AONCLK_GMAC0_TX_INV);
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
    super::print_str("), tx=");
    super::print_str(if tx { "on" } else { "FAIL" });
    super::print_str(" gtxclk=");
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
    super::print_str("riscv64: net - controller alive; the window is offered to the driver\n");
    true
}
