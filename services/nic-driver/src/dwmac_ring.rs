// SPDX-License-Identifier: GPL-2.0-only
//! The DesignWare MAC's frame path: one TX ring, one RX ring, and the bring-up that arms them.
//!
//! Split from `dwmac.rs` because identification and driving are different jobs with different risk.
//! Everything here WRITES to the controller; everything there only asks it questions.
//!
//! # What the hardware told us, and what it therefore is
//!
//! The identification pass is not decoration - the numbers below are read off it rather than
//! assumed, which is the whole reason it was a separate boot:
//!
//! - `HW_FEATURE2 = 0x01000000`: **one** RX queue, **one** TX queue, **one** DMA channel each. So
//!   there is exactly one channel to program (channel 0 at `0x1100`) and no queue-mapping to do.
//!   A driver written for the general case would carry a loop over channels that can only ever run
//!   once, and a reader would not know that from the code.
//! - `HW_FEATURE1 = 0x09845904`: 2048-byte TX and RX FIFOs, and `ADDR64 = 1` (40-bit addressing).
//!   The FIFO size sets `TQS`/`RQS` below; the address width means the ring-base HIGH registers are
//!   real and must be written, even though this board's arena sits under 4 GiB so they are written
//!   with zero.
//! - `HW_FEATURE0 = 0x1a2173f7`: gigabit-capable, MDIO master fitted, `ACTPHYSEL = 1` (RGMII).
//!
//! # Provenance
//!
//! Register offsets and descriptor bits from Linux `stmmac`, read as an executable datasheet:
//! `dwmac4_dma.h` (channel base `0x1100`, stride `0x80`, and the per-channel offsets),
//! `dwmac4_descs.h` (the TDES/RDES bit positions), `dwmac4.h` (MTL at `0xd00`, stride `0x40`).
//! Cited individually at each constant. The DEVICE's requirements are borrowed; how the ring is
//! owned, bounded and reported is ours (26.14).

use godspeed_sdk::{Dma, Mmio, ServiceContext};

// ---- DMA block. `dwmac4_dma.h`. -----------------------------------------------------------------
const DMA_BUS_MODE: usize = 0x1000;
const DMA_BUS_MODE_SFT_RESET: u32 = 1 << 0;
const DMA_SYS_BUS_MODE: usize = 0x1004;
/// Channel 0. `DMA_CHAN_BASE_ADDR 0x1100`, `DMA_CHAN_BASE_OFFSET 0x80` - and there is only one
/// channel on this part, so the stride never gets used.
const CH: usize = 0x1100;
const DMA_CH_TX_CONTROL: usize = CH + 0x04;
const DMA_CH_RX_CONTROL: usize = CH + 0x08;
const DMA_CH_TX_BASE_HI: usize = CH + 0x10;
const DMA_CH_TX_BASE: usize = CH + 0x14;
const DMA_CH_RX_BASE_HI: usize = CH + 0x18;
const DMA_CH_RX_BASE: usize = CH + 0x1c;
const DMA_CH_TX_END: usize = CH + 0x20;
const DMA_CH_RX_END: usize = CH + 0x28;
const DMA_CH_TX_RING_LEN: usize = CH + 0x2c;
const DMA_CH_RX_RING_LEN: usize = CH + 0x30;
const DMA_CH_STATUS: usize = CH + 0x60;
/// `DMA_CONTROL_ST` and `DMA_CONTROL_SR` are both `BIT(0)`, in their own registers.
const DMA_CONTROL_START: u32 = 1 << 0;
/// `DMA_CONTROL_OSP` - operate on second packet, so the engine does not stall between frames.
const DMA_CONTROL_OSP: u32 = 1 << 4;
/// `DMA_CHAN_TX_CTRL_TXPBL_MASK` / `RXPBL_MASK` = `GENMASK(21, 16)`. A burst of 8 beats: small
/// enough that a 2 KiB FIFO cannot be overrun by one burst, which is the only property that matters
/// here and the reason not to reach for the largest number available.
const PBL_SHIFT: u32 = 16;
const PBL_8: u32 = 8;
/// `DMA_RBSZ_MASK = GENMASK(14, 1)` - the receive buffer size, stored shifted left by one.
const RBSZ_SHIFT: u32 = 1;

// ---- MTL block. `dwmac4.h`: `MTL_CHAN_BASE_ADDR 0xd00`, `MTL_CHAN_BASE_OFFSET 0x40`. ------------
const MTL_TX_OP_MODE: usize = 0x0d00;
const MTL_RX_OP_MODE: usize = 0x0d00 + 0x30;
/// `MTL_OP_MODE_TSF` - store and forward on transmit: the MAC starts sending only once a whole frame
/// is in the FIFO. With a 2 KiB FIFO and 1514-byte frames that always fits, and it removes underrun
/// as a failure mode entirely rather than tuning a threshold against it.
const MTL_OP_MODE_TSF: u32 = 1 << 1;
/// `MTL_OP_MODE_TXQEN` - enable the queue.
const MTL_OP_MODE_TXQEN: u32 = 1 << 3;
/// `MTL_OP_MODE_RSF` - store and forward on receive, for the same reason.
const MTL_OP_MODE_RSF: u32 = 1 << 5;
/// `MTL_OP_MODE_TQS_MASK = GENMASK(24, 16)` and `RQS_MASK = GENMASK(29, 20)`, both counted in
/// 256-byte blocks minus one. `HW_FEATURE1` said 2048 bytes, so 2048/256 - 1 = 7.
const MTL_TQS_SHIFT: u32 = 16;
const MTL_RQS_SHIFT: u32 = 20;
const FIFO_BLOCKS: u32 = 7;

// ---- MAC block. `dwmac4.h`. ---------------------------------------------------------------------
const GMAC_CONFIG: usize = 0x0000;
const GMAC_CONFIG_RE: u32 = 1 << 0;
const GMAC_CONFIG_TE: u32 = 1 << 1;
/// `GMAC_CONFIG_DM` - full duplex.
const GMAC_CONFIG_DM: u32 = 1 << 13;
/// `GMAC_CONFIG_FES` (fast, 100M) and `GMAC_CONFIG_PS` (port select, MII rather than GMII). The
/// three speeds are PS+FES for 100, PS alone for 10, and neither for 1000.
const GMAC_CONFIG_FES: u32 = 1 << 14;
const GMAC_CONFIG_PS: u32 = 1 << 15;
/// `GMAC_CONFIG_ACS` exists at BIT(20) and is DELIBERATELY NOT SET. Read the reference before
/// reaching for it: ACS strips the pad and FCS only from frames whose length/type field is below
/// 1536, which means 802.3 length-framed traffic and nothing else. IP and ARP are Ethernet II with a
/// type field of 0x0800 and 0x0806, both far above that, so ACS would strip their CRC never - while
/// stripping it from the occasional 802.3 frame, giving TWO different length conventions on one
/// ring depending on what arrived.
///
/// dwmac4 has no CST bit to force stripping for type frames either; `dwmac4.h` defines every
/// `GMAC_CONFIG_` bit and there is no such name. So the honest arrangement is the uniform one: leave
/// the CRC on every frame and subtract it once, in `receive`. One rule, no per-frame guessing, and
/// it matches what every other backend in this service hands upward.
const RX_FCS_BYTES: usize = 4;
const GMAC_PACKET_FILTER: usize = 0x0008;
const GMAC_RXQ_CTRL0: usize = 0x00a0;
/// `GMAC_RX_DCB_QUEUE_ENABLE(0) = BIT(1)`.
const GMAC_RX_QUEUE0_DCB: u32 = 1 << 1;
const GMAC_ADDR_HIGH0: usize = 0x0300;
const GMAC_ADDR_LOW0: usize = 0x0304;
/// Address enable, the top bit of the HIGH register.
const GMAC_ADDR_ENABLE: u32 = 1 << 31;

// ---- The rings. --------------------------------------------------------------------------------
/// Four descriptors each way. Bounded and small on purpose: this is a request/reply driver, so a
/// deep ring buys nothing that the caller's own pacing does not already provide, and every extra
/// descriptor is arena that something else could be using.
const TX_DESCS: usize = 4;
const RX_DESCS: usize = 4;
/// A descriptor is four 32-bit words.
const DESC_BYTES: usize = 16;
/// One buffer per descriptor. 2048 rather than 1536 because `RBSZ` wants a multiple of the bus width
/// and because it matches the FIFO the part reported.
const BUF_BYTES: usize = 2048;

/// Arena layout. Written out as constants rather than computed inline so the whole map is visible in
/// one place and cannot drift between the register writes and the copies.
const TX_RING_OFF: usize = 0x0000;
const RX_RING_OFF: usize = 0x0100;
const TX_BUF_OFF: usize = 0x1000;
const RX_BUF_OFF: usize = 0x3000;
/// What the layout above needs. Asserted at compile time against nothing here - the arena's real
/// size is a spawn-time grant - so it is checked at bring-up and reported rather than assumed.
const ARENA_NEEDED: usize = RX_BUF_OFF + RX_DESCS * BUF_BYTES;

// ---- Descriptor bits. `dwmac4_descs.h`. ---------------------------------------------------------
const TDES2_IOC: u32 = 1 << 31;
const TDES3_OWN: u32 = 1 << 31;
const TDES3_FD: u32 = 1 << 29;
const TDES3_LD: u32 = 1 << 28;
const RDES3_OWN: u32 = 1 << 31;
const RDES3_IOC: u32 = 1 << 30;
const RDES3_BUF1V: u32 = 1 << 24;
const RDES3_ES: u32 = 1 << 15;
const RDES3_LEN_MASK: u32 = 0x7fff;

/// Yields to allow a hardware bit to change before calling it stuck.
///
/// Bounded by a yield rather than a spin, so it is "up to N reschedules" instead of N iterations of
/// unknown length. Nothing here is allowed to wait forever: a MAC that never clears its reset must
/// leave this service SERVING, with empty replies, rather than take the machine's networking down
/// with it.
const HW_YIELDS: u32 = 200;

pub struct Dwmac {
    pub m: Mmio,
    pub a: Dma,
    pub mac: [u8; 6],
    tx_next: usize,
    rx_next: usize,
}

fn wait_clear(ctx: &ServiceContext, m: &Mmio, reg: usize, bit: u32) -> bool {
    let mut spins = 0u32;
    while spins < HW_YIELDS {
        if m.read32(reg) & bit == 0 {
            return true;
        }
        ctx.yield_cpu();
        spins += 1;
    }
    false
}

impl Dwmac {
    /// Bring the controller up around an already-negotiated link, or report why not.
    ///
    /// `speed` and `full_duplex` come from the PHY, resolved by the caller through standard
    /// clause-22 registers rather than a vendor one - see `dwmac::link`.
    pub fn bring_up(
        ctx: &ServiceContext,
        m: Mmio,
        a: Dma,
        mac: [u8; 6],
        speed: u32,
        full_duplex: bool,
    ) -> Option<Self> {
        if a.len() < ARENA_NEEDED {
            ctx.log_fmt(format_args!(
                "nic-driver: dwmac needs {} bytes of DMA arena and was granted {} - not bringing the MAC up",
                ARENA_NEEDED,
                a.len()
            ));
            return None;
        }

        // RESET THE DMA FIRST. Everything below programs registers whose reset values this then
        // guarantees; doing it after would undo the configuration, which is a bug that presents as
        // "works on the second spawn".
        m.write32(DMA_BUS_MODE, m.read32(DMA_BUS_MODE) | DMA_BUS_MODE_SFT_RESET);
        if !wait_clear(ctx, &m, DMA_BUS_MODE, DMA_BUS_MODE_SFT_RESET) {
            ctx.log("nic-driver: dwmac DMA reset never cleared - the block is not responding, serving empty replies");
            return None;
        }
        // Reported rather than programmed. The AXI burst-length field lives here, and its reset
        // value permits undefined-length bursts - which is what this needs. Writing a burst policy
        // read off another SoC's device tree would be borrowing their INTEGRATION, not the silicon's
        // requirement (26.14), so the value is printed and left alone until something measures a
        // reason to change it.
        ctx.log_fmt(format_args!(
            "nic-driver: dwmac sys-bus mode 0x{:08x} (left at reset; bursts undefined-length)",
            m.read32(DMA_SYS_BUS_MODE)
        ));

        a.zero();
        let mut d = Dwmac { m, a, mac, tx_next: 0, rx_next: 0 };
        d.arm_rx_ring();
        d.program(speed, full_duplex);
        Some(d)
    }

    fn desc_write(&self, off: usize, word: usize, v: u32) {
        self.a.write32(off + word * 4, v);
    }
    fn desc_read(&self, off: usize, word: usize) -> u32 {
        self.a.read32(off + word * 4)
    }

    /// Hand every receive descriptor to the engine, pointing at its own buffer.
    ///
    /// The read format of an RDES is buffer address low, buffer address high, unused, then the
    /// control word - and OWN is written LAST, because it is the bit that hands the descriptor over.
    /// Writing it first would let the engine consume a descriptor whose address fields are still
    /// being filled in.
    fn arm_rx_ring(&mut self) {
        for i in 0..RX_DESCS {
            let off = RX_RING_OFF + i * DESC_BYTES;
            let buf = self.a.phys_at(RX_BUF_OFF + i * BUF_BYTES);
            self.desc_write(off, 0, (buf & 0xffff_ffff) as u32);
            self.desc_write(off, 1, (buf >> 32) as u32);
            self.desc_write(off, 2, 0);
            self.desc_write(off, 3, RDES3_OWN | RDES3_BUF1V | RDES3_IOC);
        }
        self.rx_next = 0;
    }

    fn program(&self, speed: u32, full_duplex: bool) {
        let m = &self.m;
        let tx_ring = self.a.phys_at(TX_RING_OFF);
        let rx_ring = self.a.phys_at(RX_RING_OFF);

        // Ring bases. The HIGH halves are written because this part reported 40-bit addressing;
        // they are zero on this board and would be a silent corruption if the part ever sat behind
        // an arena above 4 GiB and nobody had written them.
        m.write32(DMA_CH_TX_BASE_HI, (tx_ring >> 32) as u32);
        m.write32(DMA_CH_TX_BASE, (tx_ring & 0xffff_ffff) as u32);
        m.write32(DMA_CH_RX_BASE_HI, (rx_ring >> 32) as u32);
        m.write32(DMA_CH_RX_BASE, (rx_ring & 0xffff_ffff) as u32);
        // Ring length is the LAST INDEX, not the count.
        m.write32(DMA_CH_TX_RING_LEN, (TX_DESCS - 1) as u32);
        m.write32(DMA_CH_RX_RING_LEN, (RX_DESCS - 1) as u32);
        // Tail pointers. TX starts equal to its base, which means "nothing to send"; RX starts one
        // past the last descriptor, which means "all of them are yours".
        m.write32(DMA_CH_TX_END, (tx_ring & 0xffff_ffff) as u32);
        m.write32(
            DMA_CH_RX_END,
            ((rx_ring + (RX_DESCS * DESC_BYTES) as u64) & 0xffff_ffff) as u32,
        );

        // MTL: store and forward both ways, the queue enabled, the FIFO sizes the part reported.
        m.write32(
            MTL_TX_OP_MODE,
            MTL_OP_MODE_TSF | MTL_OP_MODE_TXQEN | (FIFO_BLOCKS << MTL_TQS_SHIFT),
        );
        m.write32(MTL_RX_OP_MODE, MTL_OP_MODE_RSF | (FIFO_BLOCKS << MTL_RQS_SHIFT));
        // Route queue 0 to the DCB path. Without this the MAC receives nothing at all, however
        // correct the ring is: frames arrive and are dropped before they reach the DMA.
        m.write32(GMAC_RXQ_CTRL0, GMAC_RX_QUEUE0_DCB);
        // Perfect filtering on our own address, plus broadcast (which the MAC accepts unless told
        // otherwise). Deliberately NOT promiscuous: a driver that hears everything cannot tell you
        // its address filter is wrong.
        m.write32(GMAC_PACKET_FILTER, 0);

        // Our address, in the shape `stmmac_dwmac4_set_mac_addr` writes it: bytes 4 and 5 in the low
        // half of HIGH with the enable bit, bytes 0 to 3 in LOW.
        m.write32(
            GMAC_ADDR_HIGH0,
            GMAC_ADDR_ENABLE | ((self.mac[5] as u32) << 8) | self.mac[4] as u32,
        );
        m.write32(
            GMAC_ADDR_LOW0,
            (self.mac[3] as u32) << 24
                | (self.mac[2] as u32) << 16
                | (self.mac[1] as u32) << 8
                | self.mac[0] as u32,
        );

        // Start the DMA engines, then enable the MAC. This order matters: a receiver enabled before
        // its ring is running has nowhere to put the first frame.
        m.write32(
            DMA_CH_RX_CONTROL,
            DMA_CONTROL_START | ((BUF_BYTES as u32) << RBSZ_SHIFT) | (PBL_8 << PBL_SHIFT),
        );
        m.write32(
            DMA_CH_TX_CONTROL,
            DMA_CONTROL_START | DMA_CONTROL_OSP | (PBL_8 << PBL_SHIFT),
        );

        let mut cfg = GMAC_CONFIG_TE | GMAC_CONFIG_RE;
        if full_duplex {
            cfg |= GMAC_CONFIG_DM;
        }
        match speed {
            1000 => {}
            100 => cfg |= GMAC_CONFIG_PS | GMAC_CONFIG_FES,
            _ => cfg |= GMAC_CONFIG_PS,
        }
        m.write32(GMAC_CONFIG, cfg);
    }

    /// Re-apply just the speed and duplex, for a cable that arrived after bring-up.
    ///
    /// Separate from `program` because it must NOT rebuild the rings: the receiver is live by then,
    /// and re-arming descriptors under a running engine discards whatever is in flight.
    pub fn set_link(&self, speed: u32, full_duplex: bool) {
        let mut cfg = self.m.read32(GMAC_CONFIG) & !(GMAC_CONFIG_PS | GMAC_CONFIG_FES | GMAC_CONFIG_DM);
        if full_duplex {
            cfg |= GMAC_CONFIG_DM;
        }
        match speed {
            1000 => {}
            100 => cfg |= GMAC_CONFIG_PS | GMAC_CONFIG_FES,
            _ => cfg |= GMAC_CONFIG_PS,
        }
        self.m.write32(GMAC_CONFIG, cfg);
    }

    /// The DMA channel's own account of itself, for a log line that can tell a dead ring from a dead
    /// cable. `DMA_CHAN_STATUS`: TI/RI are normal completions, FBE is a bus error, RBU means the
    /// engine ran out of descriptors we gave it.
    pub fn dma_status(&self) -> u32 {
        self.m.read32(DMA_CH_STATUS)
    }

    /// Transmit one frame. Returns false if the descriptor was never given back.
    pub fn transmit(&mut self, ctx: &ServiceContext, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() > BUF_BYTES {
            return false;
        }
        let i = self.tx_next;
        let off = TX_RING_OFF + i * DESC_BYTES;
        let buf_off = TX_BUF_OFF + i * BUF_BYTES;
        // A descriptor still owned by the engine means the ring is full, which with four descriptors
        // and a request/reply caller means the previous frame has not gone out yet. Report rather
        // than overwrite: overwriting sends a frame that is half one packet and half another.
        if self.desc_read(off, 3) & TDES3_OWN != 0 {
            return false;
        }
        // Byte at a time, as the GENET backend does: the arena is an uncached device mapping on
        // this port, so there is no wide-copy helper that is safe to reach for here.
        for (k, b) in frame.iter().enumerate() {
            self.a.write8(buf_off + k, *b);
        }
        let buf = self.a.phys_at(buf_off);
        let len = frame.len() as u32;
        self.desc_write(off, 0, (buf & 0xffff_ffff) as u32);
        self.desc_write(off, 1, (buf >> 32) as u32);
        self.desc_write(off, 2, TDES2_IOC | (len & 0x3fff));
        // OWN last, again: it is the handover.
        self.desc_write(off, 3, TDES3_OWN | TDES3_FD | TDES3_LD | (len & 0x7fff));

        // Tell the engine where the ring now ends - one past the descriptor just filled. This is the
        // write that actually starts the transmission; without it the descriptor sits there owned by
        // hardware that has not been told to look.
        let tail = self.a.phys_at(TX_RING_OFF + ((i + 1) % TX_DESCS) * DESC_BYTES);
        self.m.write32(DMA_CH_TX_END, (tail & 0xffff_ffff) as u32);
        self.tx_next = (i + 1) % TX_DESCS;

        // Wait for the engine to hand the descriptor back, bounded. Waiting is what makes the reply
        // to net-stack mean "sent" rather than "queued"; the bound is what stops a wedged engine
        // from wedging this service.
        let mut spins = 0u32;
        while spins < HW_YIELDS {
            if self.desc_read(off, 3) & TDES3_OWN == 0 {
                return true;
            }
            ctx.yield_cpu();
            spins += 1;
        }
        false
    }

    /// Take one frame off the receive ring, or return 0 if none has arrived.
    ///
    /// Never blocks. An empty ring is an ordinary answer, not a failure: the caller polls.
    pub fn receive(&mut self, out: &mut [u8]) -> usize {
        let i = self.rx_next;
        let off = RX_RING_OFF + i * DESC_BYTES;
        let d3 = self.desc_read(off, 3);
        if d3 & RDES3_OWN != 0 {
            return 0; // still the engine's
        }
        let mut n = 0usize;
        if d3 & RDES3_ES == 0 {
            // The reported length INCLUDES the CRC, because nothing strips it - see RX_FCS_BYTES.
            // `saturating_sub` rather than a subtraction: a runt shorter than its own FCS is a
            // corrupt descriptor, and the answer to that is a zero-length frame the caller ignores,
            // not an underflow that becomes a gigantic copy.
            let len = (d3 & RDES3_LEN_MASK) as usize;
            let len = len.saturating_sub(RX_FCS_BYTES);
            n = len.min(out.len()).min(BUF_BYTES);
            if n > 0 {
                let base = RX_BUF_OFF + i * BUF_BYTES;
                for (k, b) in out[..n].iter_mut().enumerate() {
                    *b = self.a.read8(base + k);
                }
            }
        }
        // Give the descriptor straight back, whether the frame was good or not - a descriptor left
        // in our hands is one the engine cannot use, and four of those is a receiver that stops.
        let buf = self.a.phys_at(RX_BUF_OFF + i * BUF_BYTES);
        self.desc_write(off, 0, (buf & 0xffff_ffff) as u32);
        self.desc_write(off, 1, (buf >> 32) as u32);
        self.desc_write(off, 2, 0);
        self.desc_write(off, 3, RDES3_OWN | RDES3_BUF1V | RDES3_IOC);
        let tail = self.a.phys_at(RX_RING_OFF + i * DESC_BYTES) + DESC_BYTES as u64;
        self.m.write32(DMA_CH_RX_END, (tail & 0xffff_ffff) as u32);
        self.rx_next = (i + 1) % RX_DESCS;
        n
    }
}
