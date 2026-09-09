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
    let (up, speed, fd) = link(&ctx, &m, phy);
    ctx.log_fmt(format_args!(
        "nic-driver: dwmac link {} at {} Mbit/s {} duplex",
        if up { "UP" } else { "down" },
        speed,
        if fd { "full" } else { "half" }
    ));

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
    ctx.log("nic-driver: serving the frame interface");
    serve(&ctx, &mut d)
}

fn serve(ctx: &ServiceContext, d: &mut Dwmac) -> ! {
    let mut rxbuf = [0u8; crate::FRAME_MAX];
    let mut fails = 0u32;
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
            let n = d.receive(&mut rxbuf);
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])), ctx, &mut fails);
        } else if p.len() == 1 && p[0] == 9 {
            // BATCH RX drain: [count][len:u16 LE][bytes]... Bounded three ways - the count, the
            // reply buffer, and the ring emptying - so it always terminates.
            let mut out = [0u8; crate::BATCH_MSG_MAX];
            let mut opos = 1usize;
            let mut count = 0usize;
            while count < crate::BATCH_MAX {
                if opos + 2 + crate::FRAME_MAX > out.len() {
                    break;
                }
                let n = d.receive(&mut rxbuf);
                if n == 0 {
                    break;
                }
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
            } else if tx_reports < 3 {
                // The first few transmits only. This is the measurement that separates "we are not
                // sending" from "nothing is answering": if the descriptor came back and the status
                // shows TI, the frame left the building.
                tx_reports += 1;
                ctx.log_fmt(format_args!(
                    "nic-driver: dwmac sent {} bytes, dma 0x{:08x}", p.len(), d.dma_status()));
            }
            crate::note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), ctx, &mut fails);
        }
        ctx.remove_cap(reply_cap);
    }
}
