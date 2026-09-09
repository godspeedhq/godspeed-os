// SPDX-License-Identifier: GPL-2.0-only
//! `dwmac` - the Synopsys DesignWare MAC on the StarFive JH7110 (VisionFive 2).
//!
//! **This file IDENTIFIES the part; it does not yet move frames.** The split is deliberate and it is
//! the first step of the driver rather than a throwaway. Before a single descriptor ring is built,
//! four things have to be true, and each is a fact the silicon can be asked for: what the controller
//! says it IS (`GMAC_VERSION`), what it says it HAS (`HW_FEATURE0..3` - how wide its DMA addresses
//! are, how many channels, whether an MDIO master is even fitted), what the bootloader left behind
//! (`MAC_ADDR0`, which on this board is the only place a real MAC address can come from), and
//! whether MDIO works at all - answered by reading a PHY's own identifier out of it.
//!
//! A ring built before those are known is a ring built on assumptions, and each one fails SILENTLY:
//! the wrong DMA address width writes descriptors the engine never reads, an unfitted MDIO master
//! returns zeros that look exactly like a PHY sitting at address 0, and an invented MAC address
//! produces a link that comes up and answers nobody. Asking first costs one boot.
//!
//! # Provenance
//!
//! Every offset and bit position below was read from Linux's `stmmac` as an executable datasheet
//! (`arch/CLAUDE.md`, "Porting a driver"), not from memory and not invented: `dwmac4.h` for the
//! register map, `dwmac4_core.c` (`dwmac4_setup`) for the MDIO address register's field masks, and
//! `stmmac_mdio.c` (`stmmac_mdio_format_addr` / `stmmac_mdio_access`) for the access sequence. What
//! is borrowed is the SILICON's requirement; the shape of this file - a capability service driving a
//! granted window, every hardware wait bounded, failure reported rather than retried forever - is
//! ours (26.14).
//!
//! The device tree calls this part `snps,dwmac-5.20`, which is the DWMAC4/5 register map. The board
//! has already confirmed the map is right: `GMAC_VERSION` read 0x4152, whose low byte 0x52 is
//! Synopsys release 5.20, exactly what the tree claims. A 3.x part would have put that register at
//! 0x20 instead and read here as absent.

use godspeed_sdk::{Mmio, ServiceContext};

// ---- MAC block. `dwmac4.h`. ---------------------------------------------------------------------
/// Synopsys release and user version. Confirmed on hardware: 0x4152 = release 5.20.
const GMAC_VERSION: usize = 0x0110;
/// What this instance was synthesised with. Read rather than assumed, because the JH7110 integrates
/// its own configuration of a licensable core and the licensee chooses these.
const GMAC_HW_FEATURE0: usize = 0x011c;
const GMAC_HW_FEATURE1: usize = 0x0120;
const GMAC_HW_FEATURE2: usize = 0x0124;
const GMAC_HW_FEATURE3: usize = 0x0128;
/// The RGMII interface's own view of the link. NOT the PHY's view: this is what the MAC believes the
/// in-band status lines are telling it, which is a different question and disagrees with the PHY
/// exactly when the RGMII timing is wrong - so having both is worth more than having either.
const GMAC_PHYIF_CONTROL_STATUS: usize = 0x00f8;
/// MDIO. `stmmac_mdio_access` writes DATA first, then ADDR, then polls ADDR's busy bit.
const GMAC_MDIO_ADDR: usize = 0x0200;
const GMAC_MDIO_DATA: usize = 0x0204;
/// Perfect-match address filter 0: `GMAC_ADDR_HIGH(n) = 0x300 + n * 8`, LOW is that plus 4.
const GMAC_ADDR_HIGH0: usize = 0x0300;
const GMAC_ADDR_LOW0: usize = 0x0304;

// ---- MDIO address-register fields. `dwmac4_setup` in `dwmac4_core.c`. ---------------------------
/// `mac->mii.addr_mask = GENMASK_U32(25, 21)` - the PHY's address on the bus.
const MDIO_PA_SHIFT: u32 = 21;
/// `mac->mii.reg_mask = GENMASK_U32(20, 16)` - the register within that PHY.
const MDIO_RDA_SHIFT: u32 = 16;
/// `mac->mii.clk_csr_mask = GENMASK_U32(11, 8)` - which divider produces MDC from the CSR clock.
const MDIO_CR_SHIFT: u32 = 8;
/// `MII_GMAC4_READ = 3 << MII_GMAC4_GOC_SHIFT`, with `MII_GMAC4_GOC_SHIFT = 2`.
const MDIO_OP_READ: u32 = 3 << 2;
/// `MII_ADDR_GBUSY = BIT(0)`. Software sets it to start; the controller clears it when done.
const MDIO_BUSY: u32 = 1 << 0;
/// `MII_DATA_GD_MASK = GENMASK(15, 0)`.
const MDIO_DATA_MASK: u32 = 0xffff;

/// MDC divider: `STMMAC_CSR_300_500M = 0x6`, which is CSR/204.
///
/// **Chosen so the bound holds whatever the CSR clock turns out to be, because nothing here knows
/// it.** MDIO's own ceiling is 2.5 MHz, and the divider is normally picked from the AHB rate feeding
/// the block - a rate this board's device tree does not state for this MAC and no register reports.
/// Rather than guess a rate and derive a divider from it, take the largest ordinary divider: at
/// CSR/204 the MDC stays under 2.5 MHz for any CSR clock up to 510 MHz, which this AHB certainly is
/// not above. Too SLOW costs a few microseconds per register; too FAST is a PHY that answers with
/// garbage or not at all, and those two are indistinguishable in a log. For the handful of reads
/// below the safe direction is free.
const MDIO_CR_DIV204: u32 = 0x6;

/// PHY registers, IEEE 802.3 clause 22. The same three on every PHY ever made, which is exactly why
/// they are the right thing to ask before knowing which PHY this is.
const PHY_BMSR: u32 = 1; // basic status: bit 2 link, bit 5 auto-negotiation complete
const PHY_ID1: u32 = 2;
const PHY_ID2: u32 = 3;

/// How many yields to give the MDIO busy bit before giving up.
///
/// A COUNT is not a duration, so what bounds this is the yield: each iteration hands the core away,
/// which makes the wait "up to N reschedules" rather than N spins of an unknown length. Linux allows
/// 10 ms in total, polling every 100 us; a scheduler quantum here is 10 ms, so a hundred yields is
/// far past any transfer that was ever going to complete. What matters is that it RETURNS - a driver
/// that spins forever on a bit an absent MDIO master will never clear takes the machine's networking
/// down with it, and the Rule Above The Rules says it must report instead.
const MDIO_YIELDS: u32 = 100;

/// Wait, bounded, for the MDIO master to report itself idle.
fn mdio_idle(ctx: &ServiceContext, m: &Mmio) -> bool {
    let mut spins = 0u32;
    while spins < MDIO_YIELDS {
        if m.read32(GMAC_MDIO_ADDR) & MDIO_BUSY == 0 {
            return true;
        }
        ctx.yield_cpu();
        spins += 1;
    }
    false
}

/// Read one clause-22 register out of one PHY, or `None` if the master never went idle.
///
/// The sequence is `stmmac_mdio_access`: wait for a bus that is already idle, write the data
/// register, write the address register with BUSY set, wait for BUSY to clear, read the data. That
/// FIRST wait is not redundant - it is what makes a second caller safe after a first one timed out,
/// which is precisely the state this function can leave the bus in.
fn mdio_read(ctx: &ServiceContext, m: &Mmio, phy: u32, reg: u32) -> Option<u16> {
    if !mdio_idle(ctx, m) {
        return None;
    }
    m.write32(GMAC_MDIO_DATA, 0);
    let addr = ((phy & 0x1f) << MDIO_PA_SHIFT)
        | ((reg & 0x1f) << MDIO_RDA_SHIFT)
        | (MDIO_CR_DIV204 << MDIO_CR_SHIFT)
        | MDIO_OP_READ
        | MDIO_BUSY;
    m.write32(GMAC_MDIO_ADDR, addr);
    if !mdio_idle(ctx, m) {
        return None;
    }
    Some((m.read32(GMAC_MDIO_DATA) & MDIO_DATA_MASK) as u16)
}

/// Ask the controller what it is, what it has, and what is on its MDIO bus - and say so.
///
/// READ-ONLY on the MAC, deliberately. The one register this writes is the MDIO address register,
/// which is how a READ is issued on that bus; nothing else on the part is touched. So it cannot
/// leave the controller in a state a later bring-up has to undo, and it cannot make the board worse
/// than it found it.
pub fn identify(ctx: &ServiceContext, mmio: Option<&Mmio>) {
    let Some(m) = mmio else {
        ctx.log("nic-driver: dwmac - no register window was granted; nothing to identify");
        return;
    };

    let ver = m.read32(GMAC_VERSION);
    let snps = ver & 0xff;
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac version 0x{:08x} (synopsys {}.{:02})",
        ver,
        snps >> 4,
        (snps & 0xf) * 10
    ));

    // The four feature words, printed RAW. Raw because a decode is a claim about which synthesis
    // options this licensee took, and a wrong decode printed as prose is much harder to disbelieve
    // than a hex word is. These are the numbers the ring layout will be derived from, and they want
    // to be checkable against the datasheet by eye before anything is built on them.
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac features 0x{:08x} 0x{:08x} 0x{:08x} 0x{:08x}",
        m.read32(GMAC_HW_FEATURE0),
        m.read32(GMAC_HW_FEATURE1),
        m.read32(GMAC_HW_FEATURE2),
        m.read32(GMAC_HW_FEATURE3)
    ));

    // What the bootloader left in the filter. `stmmac_dwmac4_set_mac_addr` packs bytes 0..3 into LOW
    // and 4..5 into the bottom half of HIGH, with bit 31 of HIGH the address-enable. A zero here is
    // not a fault - it means U-Boot did not bring ethernet up - but it IS the difference between a
    // MAC address this board owns and one a driver would have to invent.
    let hi = m.read32(GMAC_ADDR_HIGH0);
    let lo = m.read32(GMAC_ADDR_LOW0);
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ({}), phyif 0x{:08x}",
        lo & 0xff,
        (lo >> 8) & 0xff,
        (lo >> 16) & 0xff,
        (lo >> 24) & 0xff,
        hi & 0xff,
        (hi >> 8) & 0xff,
        if hi & (1 << 31) != 0 { "enabled" } else { "DISABLED - U-Boot left no address" },
        m.read32(GMAC_PHYIF_CONTROL_STATUS)
    ));

    // THE ONE THAT DECIDES THE REST. The device tree puts this board's PHY at MDIO address 0, but a
    // bus answering 0xffff at every address is an idle bus with nothing driving it, and one
    // answering 0x0000 everywhere is a master that is not clocked. BOTH look like "a PHY" to code
    // that only ever asks address 0, which is why this sweeps all 32 and reports what is really out
    // there rather than confirming what the tree already told us.
    let mut found = 0u32;
    for phy in 0..32u32 {
        let (Some(id1), Some(id2)) = (
            mdio_read(ctx, m, phy, PHY_ID1),
            mdio_read(ctx, m, phy, PHY_ID2),
        ) else {
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac MDIO TIMED OUT at address {} - the master never went idle, so nothing here is a reading",
                phy
            ));
            return;
        };
        if id1 == 0xffff || (id1 == 0 && id2 == 0) {
            continue; // nothing driving this address
        }
        found += 1;
        let bmsr = mdio_read(ctx, m, phy, PHY_BMSR).unwrap_or(0);
        ctx.log_fmt(format_args!(
            "nic-driver: dwmac PHY at address {}: id 0x{:04x}{:04x}, bmsr 0x{:04x} (link {}, autoneg {})",
            phy,
            id1,
            id2,
            bmsr,
            if bmsr & (1 << 2) != 0 { "UP" } else { "down" },
            if bmsr & (1 << 5) != 0 { "complete" } else { "incomplete" }
        ));
    }
    if found == 0 {
        ctx.log("nic-driver: dwmac - MDIO answered, but NO PHY on any of the 32 addresses");
    }
}
