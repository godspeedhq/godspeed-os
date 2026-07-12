// SPDX-License-Identifier: GPL-2.0-only
//! nic-driver - the userspace NIC driver service (docs/networking.md, Phase 1).
//!
//! Model-specific driver for the Intel 82540EM ("e1000"), the QEMU dev NIC. An ordinary restartable,
//! IOMMU-confinable userspace service (Commandment I): the kernel grants it only the NIC's MMIO BAR +
//! a DMA arena, by name, and only when the discovered NIC is a real Intel e1000; all device logic
//! lives here, `unsafe`-free behind the SDK `Mmio`/`Dma` wrappers (§18.1). The T630's Realtek chipset
//! is a separate Phase-4 driver behind the same frame interface, so `net-stack` never learns the
//! difference.
//!
//! Phase 1 progress:
//!  - step 2: reset the controller + read the link state and the MAC (from EEPROM).
//!  - step 3: a TX descriptor ring in the DMA arena; transmit a raw frame.
//!  - step 4: an RX descriptor ring; receive a frame out of the arena.
//!  - step 5 (this commit): serve the **frame interface** to `net-stack`. nic-driver no longer knows
//!    ARP or any protocol (that moved up to net-stack, Commandment X - mechanism vs policy). It is a
//!    request/reply server (§8.2, like reply-server): a request payload IS a frame to transmit, and
//!    the reply payload IS the frame that came back on the wire. Mechanism only: put these bytes on
//!    the wire, hand back whatever arrived. The receive IRQ + a decoupled continuous RX loop come with
//!    Phase 2's traffic; a request/reply frame exchange is the right shape for ARP and ping.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message, Mmio, Dma};

// Intel 82540EM register offsets (byte offsets into the BAR0 MMIO window).
const REG_CTRL:   usize = 0x0000; // Device Control
const REG_STATUS: usize = 0x0008; // Device Status; bit 1 (LU) = Link Up
const REG_RCTL:   usize = 0x0100; // Receive Control
const REG_TCTL:   usize = 0x0400; // Transmit Control
const REG_TIPG:   usize = 0x0410; // Transmit Inter-Packet Gap
const REG_RDBAL:  usize = 0x2800; // RX Descriptor Base Low
const REG_RDBAH:  usize = 0x2804; // RX Descriptor Base High
const REG_RDLEN:  usize = 0x2808; // RX Descriptor ring Length (bytes)
const REG_RDH:    usize = 0x2810; // RX Descriptor Head
const REG_RDT:    usize = 0x2818; // RX Descriptor Tail
const REG_TDBAL:  usize = 0x3800; // TX Descriptor Base Low
const REG_TDBAH:  usize = 0x3804; // TX Descriptor Base High
const REG_TDLEN:  usize = 0x3808; // TX Descriptor ring Length (bytes)
const REG_TDH:    usize = 0x3810; // TX Descriptor Head
const REG_TDT:    usize = 0x3818; // TX Descriptor Tail
const REG_MTA:    usize = 0x5200; // Multicast Table Array (128 x u32)
const REG_RAL0:   usize = 0x5400; // Receive Address Low 0  (MAC bytes 0..4, EEPROM-loaded)
const REG_RAH0:   usize = 0x5404; // Receive Address High 0 (MAC bytes 4..6 in bits [15:0])

const CTRL_RST:  u32 = 1 << 26; // CTRL.RST - global device reset; the NIC self-clears it when done
const CTRL_SLU:  u32 = 1 << 6;  // CTRL.SLU - Set Link Up (else the link stays DOWN; nothing flows)
const CTRL_ASDE: u32 = 1 << 5;  // CTRL.ASDE - Auto-Speed Detection Enable

// TCTL: EN | PSP (pad short) | CT=0x0F (collision threshold) | COLD=0x40 (collision distance, FD).
// TIPG: IPGT=10, IPGR1=8, IPGR2=6 (82540EM copper).
const TCTL_VALUE: u32 = (1 << 1) | (1 << 3) | (0x0F << 4) | (0x40 << 12);
const TIPG_VALUE: u32 = 10 | (8 << 10) | (6 << 20);

// RCTL: EN | UPE (unicast promisc) | MPE (multicast promisc) | BAM (broadcast) | SECRC (strip CRC);
// buffer size 2048. Promiscuous so we receive a reply whatever MAC net-stack advertised as sender.
const RCTL_VALUE: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 15) | (1 << 26);

// Legacy TX descriptor (16 B): addr@0, length(u16)@8, cmd(u8)@11, status(u8)@12.
const TXD_CMD_EOP:  u8 = 1 << 0; // end of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // insert FCS (the NIC appends the CRC)
const TXD_CMD_RS:   u8 = 1 << 3; // report status -> the NIC sets DD when the frame is sent
const TXD_STA_DD:   u8 = 1 << 0; // (TX) descriptor done
// RX descriptor (16 B): addr@0, length(u16)@8, status(u8)@12, errors(u8)@13.
const RXD_STA_DD:   u8 = 1 << 0; // (RX) descriptor done - a frame landed in this buffer

// DMA-arena layout (64 KiB): TX ring + one TX frame buffer, then an RX ring + 4 x 2 KiB RX buffers.
const TX_RING_OFF:   usize = 0x0000;
const TX_RING_COUNT: usize = 8;
const TX_RING_BYTES: u32   = (TX_RING_COUNT * 16) as u32;
const TX_BUF_OFF:    usize = 0x1000;
const RX_RING_OFF:   usize = 0x2000;
const RX_RING_COUNT: usize = 4;
const RX_RING_BYTES: u32   = (RX_RING_COUNT * 16) as u32;
const RX_BUF_OFF:    usize = 0x3000;
const RX_BUF_SIZE:   usize = 2048;
// After RX_BUF (ends at 0x5000): a 64-byte, 64-byte-aligned buffer the NIC DMAs its hardware tally
// counters into (DTCCR dump). Layer-1 ground truth - the chip's OWN RX/TX/error counts, read straight
// off silicon and INDEPENDENT of net-stack.
const TALLY_OFF:     usize = 0x5000;

// Bounded hardware/protocol-timing polls (the exempt category, like AHCI/USB spins - NOT the
// correctness-by-time Commandment VIII forbids): wait on the TRUTH of a bit, give up LOUDLY.
const RESET_POLL_MAX: u32 = 1_000_000;
const TX_POLL_MAX:    u32 = 1_000_000;
const RX_POLL_MAX:    u32 = 8_000;     // a reply arrives in ms (caught in the first hundreds of iterations).
                                       // A MISS must give up FAST: on the T630, 50k iterations took >2s -
                                       // LONGER than net-stack's per-request deadline, so every DNS request
                                       // TIMED OUT before nic-driver could answer. Keep the no-frame poll
                                       // well under that deadline so net-stack hears back and can re-poll
                                       // ([4] collect) rather than give up (the "24 timeouts" diagnosis).

const FRAME_MAX: usize = 1600; // one Ethernet frame (<= 1518) with headroom

// --- Realtek RTL8168 C+ mode: register offsets into the MMIO BAR + 16-byte descriptor bits (Phase 4,
// Stage B). The RTL8168 has no e1000-style head/tail registers - the NIC walks the ring by the OWN bit.
const RTL_TNPDS:     usize = 0x20; // TX Normal Priority Descriptor Start Address (64-bit phys, 256B aligned)
const RTL_CR:        usize = 0x37; // Command: RST=0x10, RE=0x08, TE=0x04
const RTL_TPPOLL:    usize = 0x38; // TX Poll (write-only): NPQ=0x40 kicks the normal-priority TX queue
const RTL_TCR:       usize = 0x40; // Transmit Config
const RTL_RCR:       usize = 0x44; // Receive Config
const RTL_9346CR:    usize = 0x50; // EEPROM cmd: 0xC0 = config write ENABLE (unlock), 0x00 = lock
const RTL_IMR:       usize = 0x3C; // Interrupt Mask Register (16-bit)
const RTL_ISR:       usize = 0x3E; // Interrupt Status Register (16-bit)
const RTL_PHYSTATUS: usize = 0x6C; // PHY status: LinkSts = 0x02
const RTL_RMS:       usize = 0xDA; // RX Max packet Size (16-bit)
const RTL_RDSAR:     usize = 0xE4; // RX Descriptor Start Address (64-bit phys, 256B aligned)
const RTL_DTCCR:     usize = 0x10; // Dump Tally Counter Command Register (64-bit): buf phys | bit3 (Dump)
// The DTCCR counter dump is a DIAGNOSTIC (the chip's cumulative RxOk/TxOk tallies for `net stats`), and
// it is DMA-driven: on a healthy NIC it completes in ~us (the first few poll iterations). But it must
// NEVER delay the [3] status reply, because net-stack's `link_is_up` waits only ~1s (LINK_SECS) for that
// reply and reads the LINK byte from it - the essential truth. A NIC whose DMA is wedged (e.g. after a
// `chaos max-carnage nic-driver` reset-storm) would never finish the dump; at the old 100_000-yield bound
// (~1s of scheduler round-trips) that timed out net-stack's [3] request, so a plugged cable read as "no
// link" and `ping` froze. So the bound is TIGHT: the link byte is read from PHYSTATUS BEFORE the dump, so
// a dump that does not complete just yields ZERO counters (a degraded stat, not a slow link), reported
// loudly once (VIII - wait on truth incl. failure; X - the diagnostic must not block the essential fact).
const TALLY_POLL_MAX: u32  = 2_000;

const RTL_CR_RE:  u8 = 0x08;
const RTL_CR_TE:  u8 = 0x04;
const RTL_TPPOLL_NPQ: u8 = 0x40;

const RTL_ISR_RDU:  u16 = 1 << 4;  // Rx Descriptor Unavailable - the ring filled; RX HALTS until recovered
const RTL_ISR_FOVW: u16 = 1 << 6;  // Rx FIFO Overflow - also halts RX until the ring is re-armed

// C+ 16-byte descriptor word 0 (opts1): flags in the high bits, length/size in the low 14 bits.
const RTL_DESC_OWN: u32 = 1 << 31; // owned by the NIC (set = NIC's; it clears the bit when done)
const RTL_DESC_EOR: u32 = 1 << 30; // end of ring (the last descriptor - the NIC wraps here)
const RTL_DESC_FS:  u32 = 1 << 29; // first segment (TX)
const RTL_DESC_LS:  u32 = 1 << 28; // last segment (TX)

// RCR: AB (broadcast) | AM (multicast) | APM (physical match) | AAP (all = promiscuous), MXDMA=7
// (unlimited burst), RXFTH=7 (no FIFO threshold - DMA on whole-frame). Promiscuous so a reply lands
// whatever sender MAC net-stack advertised.
const RTL_RCR_VALUE: u32 = 0x0F | (7 << 8) | (7 << 13);
const RTL_TCR_VALUE: u32 = 7 << 8;      // MXDMA unlimited
const RTL_RMS_VALUE: u16 = RX_BUF_SIZE as u16; // accept up to one buffer (2 KiB >> a 1518-byte frame)

/// Realtek RTL8168 (the T630's NIC). Networking Phase 4, STAGE A: reset the controller, read the MAC
/// (IDR0-5) and link (PHYSTATUS), and log them - proving the MMIO BAR + register access work on real
/// hardware. TX/RX descriptor rings are Stage B; until then it serves the frame interface with EMPTY
/// replies so net-stack degrades rather than hanging (§26.7). Never returns.
fn realtek_main(ctx: ServiceContext) -> ! {
    const R_CR:        usize = 0x37; // Command: RST=0x10, RE=0x08, TE=0x04
    const R_PHYSTATUS: usize = 0x6C; // PHY status: LinkSts = 0x02
    const CR_RST:      u8    = 0x10;

    const REALTEK_RESET_MAX: u32 = 300_000; // SMALL - a wedged chip must not freeze the box for minutes

    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => { ctx.log("nic-driver: RTL8168 found but no MMIO mapped - serving empty replies"); serve_status(&ctx, &[0u8; 7]); }
    };
    // Reset: set CR.RST, wait on the bit self-clearing (bounded SMALL + loud). If MMIO is not reaching
    // the chip (D3 / no memory-space) every read is 0xff, so RST never clears - we TIME OUT, not spin.
    mmio.write8(R_CR, CR_RST);
    let mut spins = 0u32;
    while spins < REALTEK_RESET_MAX && mmio.read8(R_CR) & CR_RST != 0 { ctx.yield_cpu(); spins += 1; }
    let reset_ok = spins < REALTEK_RESET_MAX;
    // MAC = IDR0-5 (two 32-bit reads); link = PHYSTATUS bit 1.
    let lo = mmio.read32(0x00);
    let hi = mmio.read32(0x04);
    let mac = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
    let link_up = mmio.read8(R_PHYSTATUS) & 0x02 != 0;
    ctx.log_fmt(format_args!(
        "nic-driver: RTL8168 reset {}  link {}  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if reset_ok { "OK" } else { "TIMEOUT" }, if link_up { "UP" } else { "down" },
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));
    // Stage B: set up the C+ TX/RX rings and serve REAL frames. Without a DMA arena, degrade to the
    // status-only server (net-stack still degrades cleanly rather than hanging, §26.7).
    match ctx.dma_region() {
        Some(arena) => realtek_serve(&ctx, &mmio, &arena, reset_ok, &mac),
        None => {
            ctx.log("nic-driver: RTL8168 has no DMA arena - serving empty replies");
            let mut sreply = [0u8; 7];
            sreply[0] = reset_ok as u8;
            sreply[1..7].copy_from_slice(&mac);
            serve_status(&ctx, &sreply);
        }
    }
}

/// Arm RX descriptor `i`: point it at its 2 KiB buffer and hand ownership to the NIC (OWN set), with
/// EOR on the last descriptor so the NIC wraps the ring. Written OWN-last (the addr is valid first).
fn rtl_arm_rx(arena: &Dma, i: usize) {
    let d = RX_RING_OFF + i * 16;
    let buf = arena.phys_at(RX_BUF_OFF + i * RX_BUF_SIZE);
    arena.write32(d + 8, (buf & 0xffff_ffff) as u32);
    arena.write32(d + 12, (buf >> 32) as u32);
    arena.write32(d + 4, 0);
    let mut o1 = RTL_DESC_OWN | (RX_BUF_SIZE as u32 & 0x3FFF);
    if i == RX_RING_COUNT - 1 { o1 |= RTL_DESC_EOR; }
    arena.write32(d, o1);
}

/// Realtek RTL8168 C+ TX/RX (Phase 4, STAGE B): set up the C+ descriptor rings in the DMA arena, enable
/// the receiver + transmitter, and serve the frame interface FOR REAL - transmit each request frame and
/// hand back whatever arrives on the wire (§8.2, mirroring the e1000 serve loop with RTL8168 registers
/// and 16-byte C+ descriptors). A 1-byte `[3]` STATUS query still returns [reset_ok, mac(6)] (the `net`
/// nic-mac diagnostic). The receiver stays on, so background broadcasts are DRAINED before each TX.
/// Never returns.
fn realtek_serve(ctx: &ServiceContext, mmio: &Mmio, arena: &Dma, reset_ok: bool, mac: &[u8; 6]) -> ! {
    arena.zero();
    mmio.write8(RTL_9346CR, 0xC0);              // unlock the config registers
    mmio.write16(RTL_RMS, RTL_RMS_VALUE);       // max RX packet size

    // TX ring base (descriptors are written per frame).
    let tx_ring = arena.phys_at(TX_RING_OFF);
    mmio.write32(RTL_TNPDS, (tx_ring & 0xffff_ffff) as u32);
    mmio.write32(RTL_TNPDS + 4, (tx_ring >> 32) as u32);
    // RX ring: arm every descriptor to the NIC, then program its base.
    for i in 0..RX_RING_COUNT { rtl_arm_rx(arena, i); }
    let rx_ring = arena.phys_at(RX_RING_OFF);
    mmio.write32(RTL_RDSAR, (rx_ring & 0xffff_ffff) as u32);
    mmio.write32(RTL_RDSAR + 4, (rx_ring >> 32) as u32);

    mmio.write32(RTL_TCR, RTL_TCR_VALUE);
    mmio.write32(RTL_RCR, RTL_RCR_VALUE);
    mmio.write8(RTL_CR, RTL_CR_RE | RTL_CR_TE); // enable receiver + transmitter
    mmio.write16(RTL_ISR, 0xFFFF);              // clear any latched interrupt status (RDU/FOVW would halt RX)
    mmio.write8(RTL_9346CR, 0x00);              // lock the config registers again

    let link_up = mmio.read8(RTL_PHYSTATUS) & 0x02 != 0;
    ctx.log_fmt(format_args!(
        "nic-driver: RTL8168 C+ TX/RX rings up (link {}) - serving real frames",
        if link_up { "UP" } else { "down (no cable?)" }));

    let mut rxbuf = [0u8; FRAME_MAX];
    let mut tx_idx = 0usize;
    let mut rx_idx = 0usize;
    // Live stats surfaced through the [3] status query so `net` shows link/TX/RX on the TV (no serial).
    let mut last_tx_done = false;
    let mut last_rx_len  = 0u16;
    let mut tx_count     = 0u16;
    let mut rx_count     = 0u16;
    // chaos link-flap: an operator-forced link state that OVERRIDES the live PHYSTATUS read, so
    // `chaos link-flap` can simulate a cable unplug/replug with no physical access - net-stack reads the
    // same [3] link byte and self-configures on the up edge. None = report the real PHY (default); Some(b)
    // = report the forced state. Cleared ([8]) after a flap so a REAL later unplug is never masked.
    let mut force_link: Option<bool> = None;
    let mut tally_wedged_logged = false;   // one-shot loud note if the DMA counter dump ever times out
    loop {
        let req = ctx.recv();
        let reply_cap = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        // ACK any latched RX/TX interrupt status before servicing. We POLL (IMR=0), and an UNCLEARED RDU
        // (Rx Descriptor Unavailable) or FOVW (Rx FIFO Overflow) HALTS the RTL8168 receiver - `net stats`
        // showed ISR=0x95 (RDU+TDU latched). But acking ALONE does not un-halt it once the ring actually
        // FILLED: while net-stack sits idle between operations, background broadcasts pile the ring full and
        // the receiver stops (works first session, dead the next). So on RDU/FOVW, re-arm EVERY descriptor
        // (empty the ring, dropping stale broadcasts) and RESTART the receiver, so the next real frame
        // lands. Mid-session this cannot fire - net-stack drains frame-by-frame, so the ring never fills.
        let isr = mmio.read16(RTL_ISR);
        mmio.write16(RTL_ISR, 0xFFFF);
        if isr & (RTL_ISR_RDU | RTL_ISR_FOVW) != 0 {
            for i in 0..RX_RING_COUNT { rtl_arm_rx(arena, i); }
            rx_idx = 0;
            mmio.write8(RTL_CR, RTL_CR_RE | RTL_CR_TE);
        }
        // [3] STATUS query (the `net` nic-mac diagnostic) - answer the MAC, do NOT treat it as a frame.
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 3 } {
            // Fresh 15-byte status: reset_ok, mac(6), CURRENT link, last-TX-done, last-RX len, TX/RX
            // counts. The link is read LIVE (it negotiates over a few seconds after reset).
            // 32-byte NIC hardware status (Layer-1 ground truth). [0] reset_ok, [1..7] mac, [7] link,
            // [8] last-TX-done, [9..11] last-RX len, [11..13] TX req count, [13..15] RX req count,
            // [15] speed|duplex, then the CHIP's OWN cumulative tally counters (DTCCR dump, independent
            // of net-stack): [16..20] RxOk, [20..24] TxOk, [24..28] RxBroadcast, [28..30] RxErr,
            // [30..32] MissedPkt.
            let phy = mmio.read8(RTL_PHYSTATUS);
            // Report the operator-forced link state if a `chaos link-flap` set one; else the live PHY.
            let link_up = force_link.unwrap_or(phy & 0x02 != 0);
            // PHYSTATUS speed bits: 0x10=1000M, 0x08=100M, 0x04=10M; 0x01=FullDuplex.
            let speed = if phy & 0x10 != 0 { 3u8 } else if phy & 0x08 != 0 { 2 }
                        else if phy & 0x04 != 0 { 1 } else { 0 };
            // DTCCR dump: point the NIC at our 64-byte buffer and set bit 3; it DMAs its counters there.
            let tbuf = arena.phys_at(TALLY_OFF);
            for i in 0..64 { arena.write8(TALLY_OFF + i, 0); }
            mmio.write32(RTL_DTCCR + 4, (tbuf >> 32) as u32);
            mmio.write32(RTL_DTCCR, ((tbuf as u32) & !0x3F) | 0x08);   // 64B-aligned addr | Dump
            let mut td = 0u32;
            while td < TALLY_POLL_MAX && mmio.read32(RTL_DTCCR) & 0x08 != 0 { ctx.yield_cpu(); td += 1; }
            if td >= TALLY_POLL_MAX && !tally_wedged_logged {
                // The counter dump did not complete - the NIC's DMA is slow/wedged. Report it ONCE (not
                // per query - that would spam) and carry on: the counters read zero (a degraded `net stats`),
                // but the link byte below is truthful and the reply is fast, so `net`/`ping` keep working.
                ctx.log("nic-driver: RTL8168 counter dump timed out (DMA slow/wedged) - stats degraded, link still served");
                tally_wedged_logged = true;
            }
            let rx_ok  = arena.read32(TALLY_OFF + 0x08);
            let tx_ok  = arena.read32(TALLY_OFF + 0x00);
            let rx_brd = arena.read32(TALLY_OFF + 0x30);
            let rx_er  = (arena.read32(TALLY_OFF + 0x18) & 0xFFFF) as u16;
            let miss   = (arena.read32(TALLY_OFF + 0x1C) & 0xFFFF) as u16;

            let mut s = [0u8; 32];
            s[0] = reset_ok as u8;
            s[1..7].copy_from_slice(mac);
            s[7]  = link_up as u8;
            s[8]  = last_tx_done as u8;
            s[9..11].copy_from_slice(&last_rx_len.to_le_bytes());
            s[11..13].copy_from_slice(&tx_count.to_le_bytes());
            s[13..15].copy_from_slice(&rx_count.to_le_bytes());
            s[15] = speed | ((phy & 0x01) << 2);          // bits 0-1 = speed, bit 2 = full duplex
            s[16..20].copy_from_slice(&rx_ok.to_le_bytes());
            s[20..24].copy_from_slice(&tx_ok.to_le_bytes());
            s[24..28].copy_from_slice(&rx_brd.to_le_bytes());
            s[28..30].copy_from_slice(&rx_er.to_le_bytes());
            s[30..32].copy_from_slice(&miss.to_le_bytes());
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [4] RX-ONLY: poll the RX ring for ONE frame and return it (or empty) - NO drain, NO TX. Lets
        // net-stack collect frames AFTER a single query TX, so a reply arriving behind stray broadcasts
        // (mDNS etc. on a busy LAN) is caught WITHOUT re-transmitting - a re-TX drains+discards the reply.
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 4 } {
            let mut n = 0usize;
            let mut rs = 0u32;
            while rs < RX_POLL_MAX {
                let rd = RX_RING_OFF + rx_idx * 16;
                let o1 = arena.read32(rd);
                if o1 & RTL_DESC_OWN == 0 {
                    n = ((o1 & 0x3FFF) as usize).min(FRAME_MAX);
                    for i in 0..n { rxbuf[i] = arena.read8(RX_BUF_OFF + rx_idx * RX_BUF_SIZE + i); }
                    rtl_arm_rx(arena, rx_idx);
                    rx_idx = (rx_idx + 1) % RX_RING_COUNT;
                    break;
                }
                ctx.yield_cpu();
                rs += 1;
            }
            if n > 0 { last_rx_len = n as u16; rx_count = rx_count.saturating_add(1); }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n]));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [5] REGISTER DUMP: the raw RTL8168 chip state for `net stats` - CR (RE/TE), config regs, ring
        // bases, and each RX descriptor's OWN/len (are frames waiting, or is the ring armed?). Chip-tagged
        // (byte 0 = 0 realtek). No TX, no RX poll - just reads.
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 5 } {
            let mut s = [0u8; 43];
            s[0] = 0;                                     // chip: realtek
            s[1] = mmio.read8(RTL_CR);
            s[2] = mmio.read8(RTL_9346CR);
            s[3] = mmio.read8(RTL_PHYSTATUS);
            s[4] = rx_idx as u8;
            s[5..7].copy_from_slice(&mmio.read16(RTL_IMR).to_le_bytes());
            s[7..9].copy_from_slice(&mmio.read16(RTL_ISR).to_le_bytes());
            s[9..11].copy_from_slice(&mmio.read16(RTL_RMS).to_le_bytes());
            s[11..15].copy_from_slice(&mmio.read32(RTL_RCR).to_le_bytes());
            s[15..19].copy_from_slice(&mmio.read32(RTL_TCR).to_le_bytes());
            s[19..23].copy_from_slice(&mmio.read32(RTL_TNPDS).to_le_bytes());
            s[23..27].copy_from_slice(&mmio.read32(RTL_RDSAR).to_le_bytes());
            for i in 0..RX_RING_COUNT {
                let opts1 = arena.read32(RX_RING_OFF + i * 16);
                let o = 27 + i * 4;
                s[o..o + 4].copy_from_slice(&opts1.to_le_bytes());
            }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [6]/[7]/[8] FORCE-LINK (chaos link-flap): override the REPORTED link so an operator can simulate
        // a cable unplug/replug with no physical access. [6] = force DOWN, [7] = force UP, [8] = CLEAR (back
        // to the live PHY, so a real later unplug is not masked). A 1-byte ack. net-stack reads the [3] link
        // byte and reacts (down = ping stalls; up edge = self-configure). This is a report override only - it
        // does NOT touch the hardware (no SLU/reset), so the real link is unaffected.
        if { let p = req.payload_bytes(); p.len() == 1 && (p[0] == 6 || p[0] == 7 || p[0] == 8) } {
            force_link = match req.payload_bytes()[0] { 6 => Some(false), 7 => Some(true), _ => None };
            ctx.log_fmt(format_args!("nic-driver: force-link {} (chaos link-flap)",
                match force_link { Some(false) => "DOWN", Some(true) => "UP", None => "CLEAR (live)" }));
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[1]));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // RESET THE RECEIVER per frame request (mirrors the e1000 path). nic-driver is request-driven,
        // so between requests the idle RX ring FILLS with background broadcasts and the NIC hits
        // descriptor-exhaustion (RDU) and stops - and merely re-arming does NOT restart it. Disabling RX,
        // re-arming ALL descriptors, re-pointing RDSAR, and re-enabling clears that stall, so the reply we
        // are about to solicit lands in a FRESH ring. (This is exactly why DNS - which runs long after the
        // boot dance had already drained one ring's worth - saw "0 frames": the ring had stalled full.)
        mmio.write8(RTL_CR, RTL_CR_TE);                     // RX off (keep TX) while we re-arm
        for i in 0..RX_RING_COUNT { rtl_arm_rx(arena, i); }
        let rx_ring = arena.phys_at(RX_RING_OFF);
        mmio.write32(RTL_RDSAR, (rx_ring & 0xffff_ffff) as u32);
        mmio.write32(RTL_RDSAR + 4, (rx_ring >> 32) as u32);
        mmio.write8(RTL_CR, RTL_CR_RE | RTL_CR_TE);         // RX back on, from descriptor 0
        rx_idx = 0;

        // --- Transmit: copy the frame in, point descriptor tx_idx at it, kick TPPoll, wait on OWN
        // clearing (Commandment VIII, bounded + loud). ---
        let frame = req.payload_bytes();
        let flen = frame.len().min(RX_BUF_SIZE);
        for i in 0..flen { arena.write8(TX_BUF_OFF + i, frame[i]); }
        let td = TX_RING_OFF + tx_idx * 16;
        let tx_buf = arena.phys_at(TX_BUF_OFF);
        arena.write32(td + 8, (tx_buf & 0xffff_ffff) as u32);
        arena.write32(td + 12, (tx_buf >> 32) as u32);
        arena.write32(td + 4, 0);
        let mut o1 = RTL_DESC_OWN | RTL_DESC_FS | RTL_DESC_LS | (flen as u32 & 0x3FFF);
        if tx_idx == TX_RING_COUNT - 1 { o1 |= RTL_DESC_EOR; }
        arena.write32(td, o1);                  // OWN set LAST (the NIC may read the descriptor at once)
        mmio.write8(RTL_TPPOLL, RTL_TPPOLL_NPQ);
        let mut ts = 0u32;
        while ts < TX_POLL_MAX && arena.read32(td) & RTL_DESC_OWN != 0 { ctx.yield_cpu(); ts += 1; }
        let tx_done = ts < TX_POLL_MAX;
        tx_idx = (tx_idx + 1) % TX_RING_COUNT;

        // --- Receive a reply: poll the current RX descriptor's OWN bit (bounded), copy it out, re-arm. ---
        let mut n = 0usize;
        let mut rs = 0u32;
        while rs < RX_POLL_MAX {
            let rd = RX_RING_OFF + rx_idx * 16;
            let o1 = arena.read32(rd);
            if o1 & RTL_DESC_OWN == 0 {
                n = ((o1 & 0x3FFF) as usize).min(FRAME_MAX);
                for i in 0..n { rxbuf[i] = arena.read8(RX_BUF_OFF + rx_idx * RX_BUF_SIZE + i); }
                rtl_arm_rx(arena, rx_idx);       // give the descriptor back to the NIC
                rx_idx = (rx_idx + 1) % RX_RING_COUNT;
                break;
            }
            ctx.yield_cpu();
            rs += 1;
        }
        last_tx_done = tx_done;
        tx_count = tx_count.saturating_add(1);
        if n > 0 { last_rx_len = n as u16; rx_count = rx_count.saturating_add(1); }

        let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n]));
        ctx.remove_cap(reply_cap);
    }
}

/// Serve the frame interface. A 1-byte `[3]` STATUS query gets `sreply` ([ok, mac(6)]) back - the
/// `net` nic-mac diagnostic. Every other request (a frame from net-stack) gets an EMPTY reply, so
/// net-stack degrades rather than hangs (§26.7). Never returns.
fn serve_status(ctx: &ServiceContext, sreply: &[u8]) -> ! {
    loop {
        let req = ctx.recv();
        let reply_cap = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        let p = req.payload_bytes();
        if p.len() == 1 && p[0] == 3 {
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(sreply));
        } else {
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[]));
        }
        ctx.remove_cap(reply_cap);
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("nic-driver: starting");

    // Which NIC did the kernel find? nic-driver drives an Intel e1000 (the QEMU dev NIC) or a Realtek
    // RTL8168 (the T630); the kernel maps whichever one's BAR. Dispatch on the PCI identity (Phase 4).
    if ctx.nic_vendor_device() == 0x8168_10EC {
        realtek_main(ctx); // RTL8168 - a separate path that never returns
    }

    // --- Intel e1000 path. The kernel mapped our BAR + DMA arena only if the discovered NIC is a real
    // Intel e1000 (Commandment VII). On any other NIC or none, we still SERVE the frame interface -
    // with empty replies - so net-stack degrades instead of hanging on a reply (§26.7).
    let mmio  = ctx.mmio();
    let arena = ctx.dma_region();
    let active = mmio.is_some() && arena.is_some();
    let mut e1000_mac = [0u8; 6];

    if active {
        let m = mmio.as_ref().unwrap();
        let a = arena.as_ref().unwrap();

        // Reset to a known state (bring-up on EVERY spawn - Commandments V + IX), wait on the bit.
        m.write32(REG_CTRL, m.read32(REG_CTRL) | CTRL_RST);
        let mut spins = 0u32;
        while spins < RESET_POLL_MAX && m.read32(REG_CTRL) & CTRL_RST != 0 { ctx.yield_cpu(); spins += 1; }
        // Bring the link UP (else nothing flows back on the wire).
        m.write32(REG_CTRL, m.read32(REG_CTRL) | CTRL_SLU | CTRL_ASDE);

        let status  = m.read32(REG_STATUS);
        let link_up = (status >> 1) & 1 == 1;
        let ral = m.read32(REG_RAL0);
        let rah = m.read32(REG_RAH0);
        e1000_mac = [
            (ral & 0xff) as u8, ((ral >> 8) & 0xff) as u8, ((ral >> 16) & 0xff) as u8,
            ((ral >> 24) & 0xff) as u8, (rah & 0xff) as u8, ((rah >> 8) & 0xff) as u8,
        ];
        ctx.log_fmt(format_args!(
            "nic-driver: e1000 up  link {}  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            if link_up { "UP" } else { "down" },
            e1000_mac[0], e1000_mac[1], e1000_mac[2], e1000_mac[3], e1000_mac[4], e1000_mac[5]));

        a.zero();
        // TX ring registers (set up once; descriptors are written per request).
        let tx_ring_phys = a.phys_at(TX_RING_OFF);
        m.write32(REG_TDBAL, (tx_ring_phys & 0xffff_ffff) as u32);
        m.write32(REG_TDBAH, (tx_ring_phys >> 32) as u32);
        m.write32(REG_TDLEN, TX_RING_BYTES);
        m.write32(REG_TDH, 0);
        m.write32(REG_TDT, 0);
        m.write32(REG_TIPG, TIPG_VALUE);
        m.write32(REG_TCTL, TCTL_VALUE);
        // RX ring: descriptors point at the buffers; receiver stays OFF (RCTL=0) until a request
        // needs it, so an idle nic-driver is not burning QEMU-TCG cycles on background traffic.
        for i in 0..RX_RING_COUNT {
            a.write64(RX_RING_OFF + i * 16, a.phys_at(RX_BUF_OFF + i * RX_BUF_SIZE));
        }
        for i in 0..128usize { m.write32(REG_MTA + i * 4, 0); }
        let rx_ring_phys = a.phys_at(RX_RING_OFF);
        m.write32(REG_RDBAL, (rx_ring_phys & 0xffff_ffff) as u32);
        m.write32(REG_RDBAH, (rx_ring_phys >> 32) as u32);
        m.write32(REG_RDLEN, RX_RING_BYTES);
        m.write32(REG_RDH, 0);
        m.write32(REG_RDT, (RX_RING_COUNT - 1) as u32);

        ctx.log("nic-driver: serving the frame interface");
    } else {
        ctx.log("nic-driver: no Intel e1000 mapped (absent, or a different NIC) - serving empty replies");
    }

    // The frame interface: a request/reply server (§8.2, like examples/reply-server). Each request's
    // payload is a frame to transmit; we reply with the frame that came back (empty if none / no NIC).
    let mut rxbuf = [0u8; FRAME_MAX];
    let mut tx_idx = 0usize;
    loop {
        let req = ctx.recv();
        // The reply cap is the ONLY authority to answer net-stack (Commandment VII, §8.5).
        let reply_cap = match ctx.take_pending_cap() {
            Some(c) => c,
            None => { ctx.log("nic-driver: frame request had no reply cap - dropping"); continue; }
        };

        // A 1-byte `[3]` STATUS query (the `net` nic-mac diagnostic) is answered with [ok, mac], NOT
        // treated as a frame to transmit (which would stall the caller on the RX poll).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 3 } {
            let mut sreply = [0u8; 7];
            sreply[0] = 1; // e1000 is up
            sreply[1..7].copy_from_slice(&e1000_mac);
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&sreply));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [4] RX-ONLY: arm the receiver, poll for ONE frame, quiesce - NO TX. Mirrors the realtek RX-only
        // so net-stack's collect-frames-after-one-TX DNS path works on both NICs (on QEMU/slirp there are
        // no stray frames, so net-stack's first request already matches and this stays a safe no-op).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 4 } {
            let mut n = 0usize;
            if active {
                let m = mmio.as_ref().unwrap();
                let a = arena.as_ref().unwrap();
                for i in 0..RX_RING_COUNT { a.write8(RX_RING_OFF + i * 16 + 12, 0); }
                m.write32(REG_RDH, 0);
                m.write32(REG_RDT, (RX_RING_COUNT - 1) as u32);
                m.write32(REG_RCTL, RCTL_VALUE);
                let mut s = 0u32;
                while s < RX_POLL_MAX {
                    if a.read8(RX_RING_OFF + 12) & RXD_STA_DD != 0 {
                        let len = a.read16(RX_RING_OFF + 8) as usize;
                        n = len.min(FRAME_MAX);
                        for i in 0..n { rxbuf[i] = a.read8(RX_BUF_OFF + i); }
                        break;
                    }
                    ctx.yield_cpu();
                    s += 1;
                }
                m.write32(REG_RCTL, 0);
            }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n]));
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [5] REGISTER DUMP (e1000): CTRL/STATUS/RCTL/TCTL/RDH/RDT - chip-tagged (byte 0 = 1 e1000).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 5 } {
            let mut s = [0u8; 25];
            s[0] = 1;                                     // chip: e1000
            if active {
                let m = mmio.as_ref().unwrap();
                s[1..5].copy_from_slice(&m.read32(REG_CTRL).to_le_bytes());
                s[5..9].copy_from_slice(&m.read32(REG_STATUS).to_le_bytes());
                s[9..13].copy_from_slice(&m.read32(REG_RCTL).to_le_bytes());
                s[13..17].copy_from_slice(&m.read32(REG_TCTL).to_le_bytes());
                s[17..21].copy_from_slice(&m.read32(REG_RDH).to_le_bytes());
                s[21..25].copy_from_slice(&m.read32(REG_RDT).to_le_bytes());
            }
            let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s));
            ctx.remove_cap(reply_cap);
            continue;
        }

        let mut n = 0usize;
        if active {
            let m = mmio.as_ref().unwrap();
            let a = arena.as_ref().unwrap();
            let frame = req.payload_bytes();
            let flen = frame.len().min(FRAME_MAX);

            // --- Arm the RECEIVER FIRST (reset the ring to head 0, then enable), BEFORE transmitting.
            // The reply can come back faster than we could otherwise switch the receiver on - slirp's
            // ICMP echo is a trivial src/dst swap, quicker than its ARP-table reply - and a frame that
            // arrives with the receiver off is DROPPED (this is exactly why the ping's echo reply, on
            // the wire in the pcap, was never seen). Resetting head/tail per request keeps each RX
            // independent; RDH/RDT are written while the receiver is briefly off, which is safe.
            for i in 0..RX_RING_COUNT { a.write8(RX_RING_OFF + i * 16 + 12, 0); } // clear all DD bits
            m.write32(REG_RDH, 0);
            m.write32(REG_RDT, (RX_RING_COUNT - 1) as u32);
            m.write32(REG_RCTL, RCTL_VALUE);

            // --- Transmit: copy the frame into the TX buffer, point descriptor tx_idx at it, hand it
            // to the NIC (advance TDT), wait on the DD bit (Commandment VIII, bounded + loud).
            for i in 0..flen { a.write8(TX_BUF_OFF + i, frame[i]); }
            let td = TX_RING_OFF + tx_idx * 16;
            a.write64(td, a.phys_at(TX_BUF_OFF));
            a.write16(td + 8, flen as u16);
            a.write8(td + 11, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
            a.write8(td + 12, 0); // clear DD
            m.write32(REG_TDT, ((tx_idx + 1) % TX_RING_COUNT) as u32);
            let mut s = 0u32;
            while s < TX_POLL_MAX && a.read8(td + 12) & TXD_STA_DD == 0 { ctx.yield_cpu(); s += 1; }
            tx_idx = (tx_idx + 1) % TX_RING_COUNT;

            // --- Receive: the receiver is already armed, so wait on the TRUTH of a frame landing in
            // descriptor 0 (bounded), copy it out, then QUIESCE (the step-4 TCG-overhead lesson).
            let mut s = 0u32;
            while s < RX_POLL_MAX {
                if a.read8(RX_RING_OFF + 12) & RXD_STA_DD != 0 {
                    let len = a.read16(RX_RING_OFF + 8) as usize;
                    n = len.min(FRAME_MAX);
                    for i in 0..n { rxbuf[i] = a.read8(RX_BUF_OFF + i); }
                    break;
                }
                ctx.yield_cpu();
                s += 1;
            }
            m.write32(REG_RCTL, 0); // quiesce
        }

        // Reply NON-BLOCKING (§8.9): a slow/dead net-stack can never wedge us. Then reclaim the cap
        // slot so a long-running server stays bounded (§26.6).
        let _ = ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n]));
        ctx.remove_cap(reply_cap);
    }
}
