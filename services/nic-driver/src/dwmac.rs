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

use crate::dwmac_ring::Dwmac;
use godspeed_sdk::{Message, Mmio, ServiceContext};

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
/// `MII_GMAC4_WRITE = 1 << MII_GMAC4_GOC_SHIFT`.
const MDIO_OP_WRITE: u32 = 1 << 2;
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

/// How long to give the MDIO busy bit, in microseconds. Linux's total budget for the same wait.
///
/// This was a count of yields. It happened to WORK - the sweep found the PHY on the first boot - and
/// that is exactly why it is being changed anyway: the identical count-shaped bound on the DMA reset
/// in `dwmac_ring.rs` did not work, and a bound that is right by luck on one register and wrong on
/// another is not a bound, it is a coin. A count of yields is not a duration; on an idle machine it
/// can be microseconds and under load it can be seconds, and neither is what the datasheet meant.
const MDIO_US: u64 = 10_000;

/// Iterations to allow when the machine reports no counter calibration. A plain ceiling whose only
/// job is to terminate, and not dressed up as a time.
const MDIO_UNCALIBRATED_POLLS: u32 = 20_000;

/// Wait, bounded in REAL TIME, for the MDIO master to report itself idle.
fn mdio_idle(ctx: &ServiceContext, m: &Mmio) -> bool {
    let per_10ms = ctx.tsc_ticks_per_10ms();
    if per_10ms == 0 {
        let mut polls = 0u32;
        while polls < MDIO_UNCALIBRATED_POLLS {
            if m.read32(GMAC_MDIO_ADDR) & MDIO_BUSY == 0 {
                return true;
            }
            polls += 1;
            core::hint::spin_loop();
        }
        return false;
    }
    let budget = (per_10ms.saturating_mul(MDIO_US) / 10_000).max(1);
    let start = ctx.read_tsc();
    loop {
        if m.read32(GMAC_MDIO_ADDR) & MDIO_BUSY == 0 {
            return true;
        }
        if ctx.read_tsc().wrapping_sub(start) >= budget {
            return false;
        }
        core::hint::spin_loop();
    }
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

/// Standard clause-22 registers for resolving what the link actually negotiated.
const PHY_ADVERTISE: u32 = 4; // what we offered
const PHY_LPA: u32 = 5; // what the partner offered
const PHY_CTRL1000: u32 = 9; // our gigabit offer
const PHY_STAT1000: u32 = 10; // the partner's gigabit answer

/// The MAC address this port uses, and why it is invented.
///
/// **The board did not give us one.** `MAC_ADDR0` reads all-ones at boot, which is that register's
/// reset value: U-Boot did not bring ethernet up, so it left nothing behind, and the device tree on
/// the card carries neither `local-mac-address` nor `mac-address`. The JH7110 keeps its assigned
/// address in OTP, a separate block this service is not granted and has no business reaching.
///
/// So this is a LOCALLY ADMINISTERED address, which is the standard answer for a device with no
/// assigned one: bit 1 of the first byte set, bit 0 clear (unicast). It is fixed, so a DHCP lease is
/// stable across boots. What it is NOT is globally unique - two of these boards on one network would
/// collide - and that is recorded here rather than hidden (26.7). The fix is to read the OTP in the
/// kernel's discovery stage and pass it down, which is real work and not a constant.
const LOCAL_MAC: [u8; 6] = [0x02, 0x47, 0x53, 0x56, 0x46, 0x01];

/// What the PHY negotiated: (link up, speed in Mbit/s, full duplex).
///
/// Resolved from the STANDARD registers rather than the YT8531's vendor status register. The vendor
/// register is one read instead of four and gives the answer directly, which is tempting - but it is
/// also a bet on this exact part, and the bet stays invisible until someone fits a different PHY and
/// gets a plausible wrong answer out of a register that means something else there. The standard
/// path is the arithmetic every driver does: intersect what we advertised with what the partner
/// advertised, and take the best mode both agreed to.
pub fn link(ctx: &ServiceContext, m: &Mmio, phy: u32) -> (bool, u32, bool) {
    let Some(first) = mdio_read(ctx, m, phy, PHY_BMSR) else {
        return (false, 0, false);
    };
    // BMSR bit 2 LATCHES LOW: a link that dropped and returned still reads down until the register
    // has been read twice. Read it again so the answer describes now, not the worst moment since the
    // last question.
    let bmsr = mdio_read(ctx, m, phy, PHY_BMSR).unwrap_or(first);
    if bmsr & (1 << 2) == 0 {
        return (false, 0, false);
    }
    let adv = mdio_read(ctx, m, phy, PHY_ADVERTISE).unwrap_or(0);
    let lpa = mdio_read(ctx, m, phy, PHY_LPA).unwrap_or(0);
    let ctrl1000 = mdio_read(ctx, m, phy, PHY_CTRL1000).unwrap_or(0);
    let stat1000 = mdio_read(ctx, m, phy, PHY_STAT1000).unwrap_or(0);

    // Gigabit lives in registers 9 and 10: our offer in 9 bits 9 (full) and 8 (half), the partner's
    // answer in 10 bits 11 (full) and 10 (half).
    if ctrl1000 & (1 << 9) != 0 && stat1000 & (1 << 11) != 0 {
        return (true, 1000, true);
    }
    if ctrl1000 & (1 << 8) != 0 && stat1000 & (1 << 10) != 0 {
        return (true, 1000, false);
    }
    // Everything else is registers 4 and 5, the same bit layout in both: 8 = 100 full, 7 = 100 half,
    // 6 = 10 full, 5 = 10 half.
    let both = adv & lpa;
    if both & (1 << 8) != 0 {
        return (true, 100, true);
    }
    if both & (1 << 7) != 0 {
        return (true, 100, false);
    }
    if both & (1 << 6) != 0 {
        return (true, 10, true);
    }
    (true, 10, false)
}

/// Drive the DesignWare MAC. Never returns.
pub fn dwmac_main(ctx: ServiceContext) -> ! {
    let (Some(m), Some(a)) = (ctx.mmio(), ctx.dma_region()) else {
        ctx.log("nic-driver: no dwmac register window or DMA arena granted - serving empty replies");
        crate::serve_status(&ctx, &[0u8; 8]);
    };

    // The identification pass stays, and runs on every spawn. It costs one MDIO sweep, and it is the
    // log every failure below gets read against: "the PHY was there and the link was up" is the
    // first thing anyone needs to know when frames are not moving.
    identify(&ctx, Some(&m));

    let phy = 0; // `ethernet-phy@0` in the device tree, and the sweep confirms it answers

    // RESET, THEN CONFIGURE, THEN WAIT - in that order, and the order is the fix.
    //
    // A reboot does not reset this PHY, so it carries the previous image's register state into this
    // boot. And a reset CLEARS the vendor delay registers, so the delays have to be applied after
    // it. Doing this the other way round is how a boot ended up negotiating for twelve seconds and
    // programming the MAC as "link down, 0 Mbit/s, half duplex".
    if !phy_reset(&ctx, &m, phy) {
        ctx.log("nic-driver: dwmac PHY reset did not complete - continuing from whatever state it is in");
    }
    configure_phy_delays(&ctx, &m, phy);
    let (up, speed, fd) = wait_for_link(&ctx, &m, phy);
    // The transmit clock edge, now that there is a speed to be right about. Only when the link
    // actually settled: with no cable there is no negotiated speed, and the cable-arrival edge in the
    // serve loop applies it then.
    if up {
        configure_tx_clk_edge(&ctx, &m, phy, speed);
    }

    // Come up around whatever the link is NOW, including no link at all. A MAC configured at a
    // default speed still answers and still serves, and the link edge in the serve loop re-applies
    // the settings when a cable arrives. Refusing to come up without a cable is exactly how the Pi 4
    // ended up with a receiver that stayed unclocked forever after a hot-plug.
    let Some(mut d) = Dwmac::bring_up(
        &ctx, m, a, LOCAL_MAC,
        if up { speed } else { 1000 },
        if up { fd } else { true },
    ) else {
        crate::serve_status(&ctx, &[0u8; 8]);
    };
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac up  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (locally administered - the board has none)  dma 0x{:08x}",
        LOCAL_MAC[0], LOCAL_MAC[1], LOCAL_MAC[2], LOCAL_MAC[3], LOCAL_MAC[4], LOCAL_MAC[5],
        d.dma_status()
    ));
    // NOT RUN. The sweep returned `sent 12, returned 0` at all four delays, which is the outcome
    // its own doc comment names as proving nothing: everything failing means the loopback never
    // engaged, not that transmit is broken at every setting. A YT8531 wants autoneg disabled and the
    // speed forced before BMCR bit 14 does anything, and this did neither.
    //
    // Kept rather than deleted because the reasoning behind it is sound and the fix is small - if
    // the receive ring turns out not to be the whole story, this is the next instrument and it
    // wants one addition, not a rewrite. Running it now would cost a second of every boot to print
    // four zeros nobody should read.
    let _ = rgmii_loopback_sweep;

    ctx.log("nic-driver: serving the frame interface");
    serve(&ctx, &mut d)
}

fn serve(ctx: &ServiceContext, d: &mut Dwmac) -> ! {
    let mut rxbuf = [0u8; crate::FRAME_MAX];
    let mut fails = 0u32;
    // COUNTERS ON BOTH ENDS OF THIS HOP, before theorising about where frames go.
    //
    // net-stack reports `88 drains, 0 frames seen` across a full 900 ms ping window, and then a
    // reply arrives moments after it closes. Two explanations fit that equally well and they have
    // nothing in common: either no frame reached this driver during the window, or frames reached it
    // and it did not hand them over. The MAC's own `rx` counter covers wire-to-MAC; nothing covered
    // MAC-to-net-stack, which is exactly the hop in question.
    //
    // This is the measurement the Pi 2's own notes say cracked the identical symptom there:
    // "13 PARSED, 13 HANDED OUT" proved the loss was ABOVE the driver and killed half the search
    // space in one boot.
    let mut asked: u64 = 0; // drain requests received (ops 4 and 9)
    let mut handed: u64 = 0; // frames actually returned
    let mut empty: u64 = 0; // drains that found the ring empty
    let mut last_report = ctx.read_tsc();
    // Once-only latches, OUTSIDE the loop they guard: one declared inside resets every iteration and
    // reports every time, which is the flood it exists to prevent.
    let mut capless_logged = false;
    let mut tx_reports = 0u32;
    // Seeded from the link as it is now, so a cable already present at bring-up is not treated as a
    // fresh arrival - its settings have just been applied.
    let mut link_was_up = link(ctx, &d.m, 0).0;

    loop {
        let req = ctx.recv();
        let Some(reply_cap) = ctx.take_pending_cap() else {
            if !capless_logged {
                capless_logged = true;
                ctx.log("nic-driver: request had no reply cap - dropping (cannot answer without one)");
            }
            continue;
        };
        let p = req.payload_bytes();

        if p.len() == 1 && p[0] == 3 {
            // STATUS: [ok, mac(6), link]. Eight bytes exactly - net-stack and the shell both require
            // that length before reading the link byte, so a shorter answer is UNREADABLE rather
            // than a report of "down".
            let (up, speed, fd) = link(ctx, &d.m, 0);
            if up && !link_was_up {
                // A cable arrived after bring-up. Re-apply the speed, because a MAC left at the
                // wrong clock receives nothing at all - and consume the edge only if the re-apply
                // actually took, so one unlucky MDIO read cannot leave the receiver dead until a
                // physical replug.
                ctx.log_fmt(format_args!(
                    "nic-driver: dwmac link came up at {} Mbit/s - re-applying MAC speed", speed));
                if speed != 0 {
                    // The clock edge is speed-dependent, so it is re-applied here for the same
                    // reason the MAC speed is: this may be the first speed this boot has seen, or a
                    // different one from the last cable.
                    configure_tx_clk_edge(ctx, &d.m, 0, speed);
                    d.set_link(speed, fd);
                    link_was_up = true;
                }
            } else {
                link_was_up = up;
            }
            let mut out = [0u8; 8];
            out[0] = 1;
            out[1..7].copy_from_slice(&d.mac);
            out[7] = up as u8;
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out)), ctx, &mut fails);
        } else if p.len() == 1 && p[0] == 4 {
            asked += 1;
            let n = d.receive(&mut rxbuf);
            if n == 0 { empty += 1 } else { handed += 1 }
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])), ctx, &mut fails);
        } else if p.len() == 1 && p[0] == 9 {
            // BATCH RX drain: [count][len:u16 LE][bytes]... Bounded three ways - the count, the
            // reply buffer, and the ring emptying - so it always terminates.
            let mut out = [0u8; crate::BATCH_MSG_MAX];
            let mut opos = 1usize;
            let mut count = 0usize;
            asked += 1;
            while count < crate::BATCH_MAX {
                if opos + 2 + crate::FRAME_MAX > out.len() {
                    break;
                }
                let n = d.receive(&mut rxbuf);
                if n == 0 {
                    if count == 0 { empty += 1 }
                    break;
                }
                handed += 1;
                out[opos..opos + 2].copy_from_slice(&(n as u16).to_le_bytes());
                opos += 2;
                out[opos..opos + n].copy_from_slice(&rxbuf[..n]);
                opos += n;
                count += 1;
            }
            out[0] = count as u8;
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out[..opos])), ctx, &mut fails);
        } else if p.len() == 1 && matches!(p[0], 5 | 6 | 7 | 8) {
            // Not supported on this backend, answered `[0]` rather than `[1]`: acking a chaos
            // link-flap override we did not perform would make a test print that it had exercised
            // link recovery having exercised nothing.
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), ctx, &mut fails);
        } else {
            // A frame to transmit. The acknowledgement carries NOTHING, deliberately: answering a
            // send with a received frame hands it to a caller that did not ask for one (destroying
            // it), and when no frame is waiting the reply is empty, which cannot be delivered at
            // all, so the caller waits out its whole deadline. That pair was the Pi 4's ping loss.
            let sent = d.transmit(ctx, p);
            if !sent {
                ctx.log_fmt(format_args!(
                    "nic-driver: dwmac did NOT send a {} byte frame (ring full, or the engine never returned the descriptor) - dma 0x{:08x}",
                    p.len(), d.dma_status()));
            } else if tx_reports < 8 {
                // The first several transmits, because these counters only mean something as a
                // SEQUENCE: one sample cannot show whether `tx good` is climbing with `tx gb` or
                // standing still beside it, and that difference is the entire diagnosis. Eight is
                // enough to span a DHCP attempt and still bounded, so a busy link cannot turn this
                // into a console flood.
                tx_reports += 1;
                let (tgb, tg, tuf, tce, rgb, rcrc, dbg) = d.mac_counters();
                ctx.log_fmt(format_args!(
                    "nic-driver: dwmac sent {} bytes | tdes3 0x{:08x} = {} | MAC tx {}/{} good, underflow {}, carrier {}, rx {} crc-err {} | dma 0x{:08x} debug 0x{:08x}",
                    p.len(), d.last_tx_status, d.tx_error_name(),
                    tg, tgb, tuf, tce, rgb, rcrc,
                    d.dma_status(), dbg));
            }
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), ctx, &mut fails);
        }
        ctx.remove_cap(reply_cap);

        // Paced on a WALL CLOCK, not per request: a drain-rate report printed per drain would be
        // ninety lines a second, and an instrument that floods the console changes the timing of the
        // thing it is measuring.
        let per_10ms = ctx.tsc_ticks_per_10ms();
        if per_10ms != 0 && ctx.read_tsc().wrapping_sub(last_report) > per_10ms * 500 {
            last_report = ctx.read_tsc();
            let (tgb, tg, _tuf, _tce, rgb, rcrc, _dbg) = d.mac_counters();
            // ASK THE MAC WHY, whenever it says it transmitted frames it does not call good. Silent
            // when the two agree, so a healthy machine prints nothing and this cannot become noise
            // that hides the line beneath it.
            if tg != tgb {
                let (sc, mc, df, lc, ec, ed, ogb, og) = d.tx_fault_counters();
                ctx.log_fmt(format_args!(
                    "nic-driver: dwmac tx {} sent but only {} good | single-col {} multi-col {} deferred {} late-col {} excess-col {} excess-defer {} | octets {}/{} good",
                    tgb, tg, sc, mc, df, lc, ec, ed, og, ogb));
            }
            // MAC rx against frames handed out is the whole question. If `rx` climbs while `handed`
            // does not, the frames are arriving and this driver is losing them. If neither climbs,
            // they never reached the MAC and the fault is below us.
            // SAY "DROPPED" ONLY WHEN SOMETHING WAS. The verdict used to be unconditional text in
            // the format string, so every line read `RBU 0 (ring ran dry - frames DROPPED)` - a
            // reading and its own contradiction on the same line, printed hundreds of times across a
            // run in which the ring never once ran dry. An instrument that states a conclusion the
            // number beside it refutes is worse than no instrument: it is a false lead with a
            // timestamp on it.
            let rbu = if d.rbu == 0 { "" } else { " - THE RING RAN DRY, frames were dropped" };
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac hop | MAC rx {} crc-err {} tx {} | drains asked {} handed {} empty {} | RBU {}{}",
                rgb, rcrc, tgb, asked, handed, empty, d.rbu, rbu));
            let _ = tg;
        }
    }
}

// ---- The PHY's RGMII timing. `motorcomm.c`. ----------------------------------------------------
//
// **The board asks for this explicitly and nothing was doing it.** The device tree's PHY node
// carries `rx-internal-delay-ps = <1500>` and `tx-internal-delay-ps = <1500>` alongside
// `phy-mode = "rgmii-id"`, and those numbers are not decoration: at gigabit the RGMII clock and data
// are edge-aligned as they leave the transmitter, so SOMETHING has to shift one relative to the
// other before the receiver samples it. `rgmii-id` says the PHY does it, at both ends, and a PHY
// that has not been told simply samples at the wrong instant. The frames still leave the MAC and
// still count as transmitted - the failure is entirely on the wire, which is why it presents as a
// network that can be heard but never answers.
//
// Vendor registers, so they are gated on the vendor ID actually read back. A wrong guess about which
// PHY this is would otherwise write 0xA003 on a part where that address means something else.

/// The part this board carries, confirmed against `motorcomm.c`: `PHY_ID_YT8531 0x4f51e91b`.
const PHY_ID_YT8531: u32 = 0x4f51_e91b;
/// The extended-register window: write the address to 0x1E, then read or write 0x1F.
const YTPHY_PAGE_SELECT: u32 = 0x1e;
const YTPHY_PAGE_DATA: u32 = 0x1f;
/// `YT8521_RGMII_CONFIG1_REG`, in that extended space.
const YT8521_RGMII_CONFIG1: u16 = 0xa003;
/// `YT8521_CHIP_CONFIG_REG`, which carries the receive clock's COARSE delay.
///
/// **The receive delay is two fields, not one, and missing that broke reception.** The fine field in
/// `RGMII_CONFIG1` covers 0 to 2250 ps in 150 ps steps; `YT8521_CCR_RXC_DLY_EN` here adds a flat
/// 1900 ps on top of it. Linux's table is 32 entries for exactly this reason - sixteen fine values,
/// then the same sixteen again with the coarse bit set - and `ytphy_get_delay_reg_value` clears the
/// coarse bit whenever the requested delay is found in the first half.
///
/// Setting the fine field to the tree's 1500 ps while leaving the coarse bit alone therefore asked
/// for 3400 ps, and the board answered by receiving nothing at all: `frames scanned` went from 15
/// to 0 and the DMA status stopped reporting RI. The power-on state that DID work was the mirror
/// image - fine 0 with the coarse bit on, which is a perfectly ordinary 1900 ps.
const YT8521_CHIP_CONFIG: u16 = 0xa001;
/// `YT8521_CCR_RXC_DLY_EN = BIT(8)`, worth 1900 ps when set.
const CCR_RXC_DLY_EN: u16 = 1 << 8;
/// `YT8521_CCR_RXC_DLY_1_900_NS`. Named rather than inlined because the comparison below is the
/// whole of the coarse-bit decision, and a bare 1900 in an `if` says nothing about where it came
/// from.
const CCR_RXC_DLY_PS: u32 = 1900;
/// `YT8521_RC1R_RX_DELAY_MASK = GENMASK(13, 10)`.
const RC1R_RX_DELAY_SHIFT: u32 = 10;
/// `YT8521_RC1R_FE_TX_DELAY_MASK = GENMASK(7, 4)` - the 10/100 transmit delay.
const RC1R_FE_TX_DELAY_SHIFT: u32 = 4;
/// `YT8521_RC1R_GE_TX_DELAY_MASK = GENMASK(3, 0)` - the gigabit transmit delay.
const RC1R_GE_TX_DELAY_SHIFT: u32 = 0;
const RC1R_DELAY_FIELD: u16 = 0xf;

/// `YT8521_RC1R_TX_CLK_SEL_INVERTED = BIT(14)`, in the same `RGMII_CONFIG1` register as the delays.
///
/// **This is the transmit CLOCK EDGE, and it is a board fact, not a PHY default.** The delay fields
/// beside it shift data against the clock in picoseconds; this picks which edge of that clock the PHY
/// samples the MAC's transmit data on. Get it wrong and the data is sampled off the eye: the frame
/// leaves the MAC intact, the MAC scores it transmitted the instant it hands it over, and what
/// reaches the wire is noise that the far end drops on FCS without ever telling us.
const RC1R_TX_CLK_SEL_INVERTED: u16 = 1 << 14;

// Whether to invert, per speed. Straight off THIS board's device tree - `ethernet@16030000`'s PHY node
// in `jh7110s-starfive-visionfive-2-lite.dtb`, which is the MAC this driver owns:
//
//     motorcomm,tx-clk-adj-enabled      (true)
//     motorcomm,tx-clk-100-inverted     (true)
//     motorcomm,tx-clk-1000-inverted    (true)
//     rx-internal-delay-ps              <1500>
//     tx-internal-delay-ps              <1500>
//
// The OTHER port on the same SoC, `ethernet@16040000`, declares `tx-clk-100-inverted` and NOT
// `tx-clk-1000-inverted` - which is the proof that this is per-port wiring rather than something the
// PHY or the vendor driver would arrive at on its own. Ours needs it at exactly the speed we run.
// `motorcomm,tx-clk-10-inverted` is absent from both, so ten megabit is left alone.
const TX_CLK_ADJ_ENABLED: bool = true;
const TX_CLK_1000_INVERTED: bool = true;
const TX_CLK_100_INVERTED: bool = true;
const TX_CLK_10_INVERTED: bool = false;

/// 1500 ps, as the device tree asks, in this register's units.
///
/// The encoding is a 16-step table in 150 ps increments starting at zero, so the value is simply the
/// picoseconds divided by the step. Written as the arithmetic rather than as a magic `10` so the
/// device tree's number stays visible in the code that consumes it - if the board is ever respun
/// with a different delay, the line to change is obvious and the units are stated.
const DELAY_STEP_PS: u32 = 150;
const DELAY_PS: u32 = 1500;
const DELAY_CODE: u16 = ((DELAY_PS / DELAY_STEP_PS) & 0xf) as u16;

/// Write one clause-22 register. Same sequence as a read with the write opcode, and the data
/// register loaded before the address register starts the transfer.
fn mdio_write(ctx: &ServiceContext, m: &Mmio, phy: u32, reg: u32, val: u16) -> bool {
    if !mdio_idle(ctx, m) {
        return false;
    }
    m.write32(GMAC_MDIO_DATA, val as u32);
    let addr = ((phy & 0x1f) << MDIO_PA_SHIFT)
        | ((reg & 0x1f) << MDIO_RDA_SHIFT)
        | (MDIO_CR_DIV204 << MDIO_CR_SHIFT)
        | MDIO_OP_WRITE
        | MDIO_BUSY;
    m.write32(GMAC_MDIO_ADDR, addr);
    mdio_idle(ctx, m)
}

fn ytphy_read_ext(ctx: &ServiceContext, m: &Mmio, phy: u32, ext: u16) -> Option<u16> {
    if !mdio_write(ctx, m, phy, YTPHY_PAGE_SELECT, ext) {
        return None;
    }
    mdio_read(ctx, m, phy, YTPHY_PAGE_DATA)
}

fn ytphy_write_ext(ctx: &ServiceContext, m: &Mmio, phy: u32, ext: u16, val: u16) -> bool {
    mdio_write(ctx, m, phy, YTPHY_PAGE_SELECT, ext) && mdio_write(ctx, m, phy, YTPHY_PAGE_DATA, val)
}

/// Apply the RGMII internal delays the board's device tree specifies, and say what took.
///
/// Returns false only when the PHY is not the part this knows how to configure, or MDIO failed -
/// both of which leave the link exactly as it was rather than half-programmed.
/// Set the transmit clock edge for the speed we actually negotiated. `yt8531_link_change_notify`.
///
/// **Speed-dependent, so it cannot be done at bring-up with the delays.** Linux hangs this off the
/// link-change notifier for that reason, and so do we: the register bit means "invert at THIS speed",
/// and until autonegotiation finishes there is no speed to be right about. Called once the link is
/// settled, and again on the cable-arrival edge.
///
/// A speed we do not recognise leaves the bit ALONE rather than clearing it. Clearing would be a
/// guess dressed as a decision, and the failure it produces is the invisible one described on
/// `RC1R_TX_CLK_SEL_INVERTED`.
pub fn configure_tx_clk_edge(ctx: &ServiceContext, m: &Mmio, phy: u32, speed: u32) {
    if !TX_CLK_ADJ_ENABLED {
        return;
    }
    let invert = match speed {
        1000 => TX_CLK_1000_INVERTED,
        100 => TX_CLK_100_INVERTED,
        10 => TX_CLK_10_INVERTED,
        _ => {
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac tx clock edge left as it is - speed {} is not one this board describes",
                speed));
            return;
        }
    };
    let Some(before) = ytphy_read_ext(ctx, m, phy, YT8521_RGMII_CONFIG1) else {
        ctx.log("nic-driver: dwmac could not read RGMII_CONFIG1 - transmit clock edge NOT set");
        return;
    };
    let want = if invert {
        before | RC1R_TX_CLK_SEL_INVERTED
    } else {
        before & !RC1R_TX_CLK_SEL_INVERTED
    };
    if !ytphy_write_ext(ctx, m, phy, YT8521_RGMII_CONFIG1, want) {
        ctx.log("nic-driver: dwmac could not write RGMII_CONFIG1 - transmit clock edge NOT set");
        return;
    }
    // READ IT BACK. A register that accepts a write has proved the address is writable and nothing
    // more, which is a lesson this project has already paid for once on the Pi 4.
    let after = ytphy_read_ext(ctx, m, phy, YT8521_RGMII_CONFIG1).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac tx clock at {} Mbit/s: {} (RGMII_CONFIG1 0x{:04x} -> 0x{:04x}, wanted 0x{:04x})",
        speed,
        if invert { "INVERTED" } else { "not inverted" },
        before, after, want));
}

pub fn configure_phy_delays(ctx: &ServiceContext, m: &Mmio, phy: u32) -> bool {
    let (Some(id1), Some(id2)) = (mdio_read(ctx, m, phy, PHY_ID1), mdio_read(ctx, m, phy, PHY_ID2))
    else {
        ctx.log("nic-driver: dwmac could not read the PHY id - RGMII delays NOT applied");
        return false;
    };
    let id = ((id1 as u32) << 16) | id2 as u32;
    if id != PHY_ID_YT8531 {
        // Loud, not silent. An unconfigured RGMII link is the failure that looks like a dead network
        // rather than a misconfigured one, so a reader needs to know it was skipped and why.
        ctx.log_fmt(format_args!(
            "nic-driver: PHY id 0x{:08x} is not the YT8531 this knows - RGMII delays NOT applied, and a link that carries nothing is the expected result",
            id));
        return false;
    }

    // THE COARSE RECEIVE DELAY FIRST, because the fine field below is only half the number. The
    // rule is the reference's: a requested delay under 1900 ps is expressible in the fine field
    // alone, so the coarse bit is cleared; at or above it, the bit carries 1900 and the fine field
    // carries the remainder.
    let Some(chip_before) = ytphy_read_ext(ctx, m, phy, YT8521_CHIP_CONFIG) else {
        ctx.log("nic-driver: dwmac could not read the PHY's chip config - delays NOT applied");
        return false;
    };
    let chip_want = if DELAY_PS >= CCR_RXC_DLY_PS {
        chip_before | CCR_RXC_DLY_EN
    } else {
        chip_before & !CCR_RXC_DLY_EN
    };
    if !ytphy_write_ext(ctx, m, phy, YT8521_CHIP_CONFIG, chip_want) {
        ctx.log("nic-driver: dwmac could not write the PHY's chip config - delays NOT applied");
        return false;
    }
    let chip_after = ytphy_read_ext(ctx, m, phy, YT8521_CHIP_CONFIG).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac PHY coarse rx delay {}: chip config 0x{:04x} -> 0x{:04x} (wanted 0x{:04x})",
        if chip_want & CCR_RXC_DLY_EN != 0 { "ON (+1900 ps)" } else { "off" },
        chip_before, chip_after, chip_want
    ));

    let Some(before) = ytphy_read_ext(ctx, m, phy, YT8521_RGMII_CONFIG1) else {
        ctx.log("nic-driver: dwmac could not read the PHY's RGMII config - delays NOT applied");
        return false;
    };
    let want = (before
        & !((RC1R_DELAY_FIELD << RC1R_RX_DELAY_SHIFT)
            | (RC1R_DELAY_FIELD << RC1R_FE_TX_DELAY_SHIFT)
            | (RC1R_DELAY_FIELD << RC1R_GE_TX_DELAY_SHIFT)))
        | (DELAY_CODE << RC1R_RX_DELAY_SHIFT)
        | (DELAY_CODE << RC1R_FE_TX_DELAY_SHIFT)
        | (DELAY_CODE << RC1R_GE_TX_DELAY_SHIFT);
    if !ytphy_write_ext(ctx, m, phy, YT8521_RGMII_CONFIG1, want) {
        ctx.log("nic-driver: dwmac could not write the PHY's RGMII config - delays NOT applied");
        return false;
    }
    // READ IT BACK. A vendor register behind a page-select is exactly the kind of write that can go
    // to the wrong place and report nothing; and this one's failure mode is a link that comes up,
    // counts packets and delivers none.
    let after = ytphy_read_ext(ctx, m, phy, YT8521_RGMII_CONFIG1).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac PHY fine delays {} ps (rx total {}): config1 0x{:04x} -> 0x{:04x} (wanted 0x{:04x})",
        DELAY_PS,
        DELAY_PS + if chip_want & CCR_RXC_DLY_EN != 0 { CCR_RXC_DLY_PS } else { 0 },
        before, after, want
    ));
    after == want && chip_after == chip_want
}

/// PHY BMCR, and its internal loopback bit. IEEE 802.3 clause 22, register 0 bit 14.
const PHY_BMCR: u32 = 0;
const BMCR_LOOPBACK: u16 = 1 << 14;

/// Frames per delay setting in the loopback sweep. Small and fixed: this runs once at bring-up and
/// its job is to separate "most get through" from "most do not", which a dozen frames answers as
/// well as a thousand and without a second of boot spent on it.
const LOOPBACK_FRAMES: usize = 12;

/// **Does the RGMII TRANSMIT path work, and at which delay?** Answered locally, with no network.
///
/// The board loses 58% of pings to its own gateway one hop away, identically to a public address, so
/// the loss is ours. Everything else has been eliminated by measurement rather than argument: the
/// driver hands out every frame the MAC receives, transmits complete with `tdes3 = no-error`, the
/// address filter reads back correct AND disabling it entirely changed nothing, and 547 frames come
/// in with ZERO CRC errors - which proves the RGMII RECEIVE path is clean. The one direction never
/// independently tested is transmit, and the one setting applied without confirmation is its delay.
///
/// The PHY's internal loopback closes that. With BMCR bit 14 set, a frame goes MAC -> RGMII -> PHY,
/// turns around inside the PHY, and comes back RGMII -> MAC. It therefore exercises the transmit
/// path in exactly the way the wire does, while depending on nothing outside this board - no
/// gateway, no cable, no far end that might be rate-limiting. If frames come back, the MAC-to-PHY
/// direction is sound at that delay; if they do not, it is not.
///
/// SWEPT rather than tested at one value, because a single reading cannot distinguish "this setting
/// is wrong" from "loopback does not work here". A curve across the range says which: several
/// settings passing and one failing is a delay problem, and everything failing means the test itself
/// proved nothing and should be believed accordingly.
///
/// **RUN 2026-09-10: `sent 12, returned 0` at ALL FOUR delays - so it proved nothing, exactly as the
/// paragraph above says to read that.** The loopback never engaged: a YT8531 wants autoneg disabled
/// and the speed forced before BMCR bit 14 does anything, and this does neither. Not deleted,
/// because the reasoning holds and the fix is one addition rather than a rewrite - but not run
/// either, since a second of every boot spent printing four zeros is worse than nothing.
///
/// Bounded and restorative: fixed frame count, bounded receive poll, and both the delay and BMCR are
/// put back before it returns. A diagnostic that leaves a PHY in loopback would take the network down
/// far more convincingly than the bug it is chasing.
#[allow(dead_code)]
pub fn rgmii_loopback_sweep(ctx: &ServiceContext, d: &mut Dwmac, phy: u32) {
    let Some(bmcr0) = mdio_read(ctx, &d.m, phy, PHY_BMCR) else {
        ctx.log("nic-driver: dwmac loopback sweep SKIPPED - the PHY did not answer");
        return;
    };
    let Some(cfg0) = ytphy_read_ext(ctx, &d.m, phy, YT8521_RGMII_CONFIG1) else {
        ctx.log("nic-driver: dwmac loopback sweep SKIPPED - could not read the RGMII config");
        return;
    };
    if !mdio_write(ctx, &d.m, phy, PHY_BMCR, bmcr0 | BMCR_LOOPBACK) {
        ctx.log("nic-driver: dwmac loopback sweep SKIPPED - could not enter loopback");
        return;
    }

    // A minimal well-formed frame: broadcast destination, our source, an unused EtherType, padded to
    // the 60-byte Ethernet minimum so nothing downstream can reject it as a runt. Content does not
    // matter - only whether the bytes come back.
    let mut probe = [0u8; 60];
    probe[..6].copy_from_slice(&[0xff; 6]);
    probe[6..12].copy_from_slice(&d.mac);
    probe[12] = 0x88;
    probe[13] = 0xb5; // IEEE 802.1 local experimental EtherType
    let mut rx = [0u8; crate::FRAME_MAX];

    for code in [0u16, 5, 10, 15] {
        let want = (cfg0 & !(RC1R_DELAY_FIELD << RC1R_GE_TX_DELAY_SHIFT))
            | (code << RC1R_GE_TX_DELAY_SHIFT);
        if !ytphy_write_ext(ctx, &d.m, phy, YT8521_RGMII_CONFIG1, want) {
            continue;
        }
        // Let the PHY settle after a timing change before trusting anything it does.
        ctx.sleep_ms(20);
        while d.receive(&mut rx) != 0 {} // discard anything already in the ring

        let mut sent = 0usize;
        let mut back = 0usize;
        for _ in 0..LOOPBACK_FRAMES {
            if !d.transmit(ctx, &probe) {
                continue;
            }
            sent += 1;
            // Bounded wait for the turnaround. A loopback is microseconds; a millisecond is three
            // orders of magnitude of headroom and still terminates on a path that is simply dead.
            let mut spins = 0u32;
            loop {
                let n = d.receive(&mut rx);
                if n != 0 {
                    back += 1;
                    break;
                }
                spins += 1;
                if spins > 20_000 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
        ctx.log_fmt(format_args!(
            "nic-driver: dwmac loopback tx-delay {} ps: sent {}, returned {}",
            code as u32 * DELAY_STEP_PS,
            sent,
            back
        ));
    }

    // PUT IT BACK. Both of them, and in this order, so the PHY leaves loopback already carrying the
    // delay the device tree asked for rather than whichever one the sweep ended on.
    let _ = ytphy_write_ext(ctx, &d.m, phy, YT8521_RGMII_CONFIG1, cfg0);
    let _ = mdio_write(ctx, &d.m, phy, PHY_BMCR, bmcr0);
    ctx.sleep_ms(20);
    ctx.log("nic-driver: dwmac loopback sweep done - PHY restored");
}

/// BMCR reset. IEEE 802.3 clause 22, register 0 bit 15: self-clearing when the PHY is ready.
const BMCR_RESET: u16 = 1 << 15;

/// Reset the PHY to a known state, then wait - bounded - for it to finish.
///
/// **A reboot does not reset this PHY.** It is a separate chip with its own reset line, so whatever
/// the last image left in its registers survives into the next boot. This session proved it the
/// hard way: a diagnostic that toggled BMCR loopback and rewrote the delay registers was followed by
/// a boot where autonegotiation took TWELVE SECONDS instead of two, and the driver programmed the
/// MAC from a link that was still down.
///
/// So bring-up starts from a defined state rather than from whatever happened last. The order
/// matters and is the whole point: a reset CLEARS the vendor delay registers, so the delays must be
/// applied after it, not before - which is why `configure_phy_delays` moved below this call.
fn phy_reset(ctx: &ServiceContext, m: &Mmio, phy: u32) -> bool {
    let Some(bmcr) = mdio_read(ctx, m, phy, PHY_BMCR) else {
        return false;
    };
    if !mdio_write(ctx, m, phy, PHY_BMCR, bmcr | BMCR_RESET) {
        return false;
    }
    // Self-clearing, and bounded because a PHY that never clears it is a PHY that is not there.
    // 100 ms is ten times the datasheet figure for a clause-22 reset.
    for _ in 0..20 {
        ctx.sleep_ms(5);
        if let Some(v) = mdio_read(ctx, m, phy, PHY_BMCR) {
            if v & BMCR_RESET == 0 {
                return true;
            }
        }
    }
    false
}

/// Wait for autonegotiation to finish and the link to come up, bounded, and report what it settled on.
///
/// **The MAC's speed and duplex are programmed from this, so reading it too early programs the wrong
/// thing.** Bring-up used to take whatever `link()` said at the instant it ran - about two seconds
/// into boot, while the PHY was still negotiating - and on a slow negotiation that meant configuring
/// a gigabit MAC as "link down, 0 Mbit/s, half duplex" and hoping the serve loop's edge-detect fixed
/// it later. It does fix it, twelve seconds later, which is not the same as being right.
///
/// WAITS ON THE ANSWER, WITH A BOUND, rather than on a fixed delay - Commandment VIII. A cable that
/// is genuinely unplugged returns "down" after the ceiling and the driver comes up anyway, serving
/// with no link, because refusing to start without a cable is how a machine ends up needing a reboot
/// after someone plugs one in.
fn wait_for_link(ctx: &ServiceContext, m: &Mmio, phy: u32) -> (bool, u32, bool) {
    const CEILING_MS: u64 = 5_000;
    const STEP_MS: u64 = 100;
    let mut waited = 0;
    loop {
        let (up, speed, fd) = link(ctx, m, phy);
        if up && speed != 0 {
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac link settled after {} ms: {} Mbit/s {} duplex",
                waited, speed, if fd { "full" } else { "half" }));
            return (up, speed, fd);
        }
        if waited >= CEILING_MS {
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac no link after {} ms - coming up anyway and serving; the serve loop re-applies when a cable arrives",
                waited));
            return (false, 0, false);
        }
        ctx.sleep_ms(STEP_MS);
        waited += STEP_MS;
    }
}
