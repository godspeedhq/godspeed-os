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
#[cfg(target_arch = "arm")]
use godspeed_sdk::DeadlineOutcome;

/// The Pi 4's on-board GENET MAC, driven from HERE instead of from the kernel (Commandment I).
///
/// Compiled on every aarch64 build so it is type-checked by the same command that builds the shipping
/// image - a backend that only compiles under its own feature is a backend that quietly rots. Which
/// one actually runs is decided in `service_main` by the target architecture: on aarch64 this backend
/// is the ONLY path - the kernel drives no ethernet at all (Commandment I).
#[cfg(target_arch = "aarch64")]
mod genet;

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

// DMA-arena layout (64 KiB): TX ring + one TX frame buffer, then an RX ring + 8 x 2 KiB RX buffers.
const TX_RING_OFF:   usize = 0x0000;
const TX_RING_COUNT: usize = 8;
const TX_RING_BYTES: u32   = (TX_RING_COUNT * 16) as u32;
const TX_BUF_OFF:    usize = 0x1000;
const RX_RING_OFF:   usize = 0x2000;
// EIGHT, BECAUSE THE DEVICE REQUIRES IT - not because eight buffers felt better than four.
//
// The e1000 wants a descriptor ring whose LENGTH IN BYTES is a multiple of 128. At four descriptors
// RDLEN was 64, which is not, and the hardware simply never used the ring: measured with RCTL
// 0x0400801a (receiver enabled, promiscuous, broadcast accepted), STATUS 0x80080783 (link up, full
// duplex), RDT=3 so descriptors were available - and RDH frozen at 0 across every drain. Not one
// descriptor written, while the wire carried thirty frames.
//
// The asymmetry is what named it: TX_RING_COUNT is 8, so TDLEN was 128 and legal, and transmit
// always worked. Receive never did. Four theories about the consuming side (a MAC filter, too few
// buffers, a stale DD bit, a desync) were all downstream of a ring the device had rejected outright.
//
// This is the silicon's requirement, borrowed as such (§26.14) - nothing about our design wanted
// four. Eight descriptors is 128 bytes exactly, the smallest legal ring.
const RX_RING_COUNT: usize = 8;
const RX_RING_BYTES: u32   = (RX_RING_COUNT * 16) as u32;
const RX_BUF_OFF:    usize = 0x3000;
const RX_BUF_SIZE:   usize = 2048;
// After RX_BUF (8 x 2 KiB, so it now ends at 0x7000): a 64-byte, 64-byte-aligned buffer the NIC DMAs
// its hardware tally counters into (DTCCR dump). Layer-1 ground truth - the chip's OWN RX/TX/error
// counts, read straight off silicon and INDEPENDENT of net-stack.
const TALLY_OFF:     usize = 0x7000;

// Bounded hardware/protocol-timing polls (the exempt category, like AHCI/USB spins - NOT the
// correctness-by-time Commandment VIII forbids): wait on the TRUTH of a bit, give up LOUDLY.
const RESET_POLL_MAX: u32 = 1_000_000;
// A healthy RTL8168 clears the TX descriptor's OWN bit in ~us (the first few poll iterations). The old
// 1_000_000-yield bound meant a NIC that FAILED to complete a transmit froze the whole ping for ~1s per
// send. Bound it TIGHT so a stuck TX fails FAST and is recovered (§26.6 bounded, §26.7 loud), instead of
// stalling the box. 30_000 is ~30x headroom over a us-scale success but ~ms, not seconds, on failure.
/// How long to wait for the NIC to confirm a transmit, IN MILLISECONDS.
///
/// It used to be a count - 30,000 yields - and a count is not a duration: the same loop is a
/// different wall-clock wait on every machine, and under QEMU it outlasted the caller's one-second
/// budget entirely. The frame WAS on the wire; this driver was still spinning for the done bit when
/// `net-stack` gave up, so a sent frame was reported as "never left the host - the driver refused
/// them". The wire showed 7 DHCP REQUESTs and 7 ACKs while the log said all six were refused.
///
/// A real transmit completes in microseconds. 20 ms is far beyond that and far inside any caller's
/// budget, so the answer arrives while it is still wanted. Commandment VIII: wait on the truth (the
/// descriptor's done bit), bounded by a CLOCK.
const TX_CONFIRM_MS:  u64 = 20;
const RX_POLL_MAX:    u32 = 8_000;     // a reply arrives in ms (caught in the first hundreds of iterations).
                                       // A MISS must give up FAST: on the T630, 50k iterations took >2s -
                                       // LONGER than net-stack's per-request deadline, so every DNS request
                                       // TIMED OUT before nic-driver could answer. Keep the no-frame poll
                                       // well under that deadline so net-stack hears back and can re-poll
                                       // ([4] collect) rather than give up (the "24 timeouts" diagnosis).

const FRAME_MAX: usize = 1600; // one Ethernet frame (<= 1518) with headroom

// [9] BATCH RX DRAIN (the frame interface, net-stack <-> nic-driver): pull up to BATCH_MAX frames off the
// RX ring in ONE bounded poll and return them length-prefixed - [count:u8] then [len:u16 LE, bytes] per
// frame. net-stack scans the batch for its reply (its ICMP echo / DNS answer) PAST any stray broadcasts,
// in a SINGLE round-trip. This replaces the old "one [4] request per look-ahead frame" approach, whose N
// slow round-trips (each polling the full RX budget when the ring was momentarily empty) pushed net-stack
// past the shell's deadline. Mechanism only: the driver hands back RAW frames, it never learns what a
// "reply" is (Commandment X). Bounded by BATCH_MAX, BATCH_MSG_MAX (< the 4 KiB IPC cap), and RX_POLL_MAX.
const BATCH_MAX:     usize = 8;
const BATCH_MSG_MAX: usize = 3072;

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
/// Note a reply that could not be delivered, instead of discarding the outcome.
///
/// Every reply here is `try_send` and not `send`, which is right: this is a server, and §8.9 requires
/// the reply direction to be non-blocking or one slow caller wedges the driver for everyone. But
/// `let _ =` on the result made a reply that never arrived indistinguishable from one that did.
///
/// The caller does recover - it is blocked on this reply and its deadline fires - so this is not a
/// wedge. It is still a failure, and §26.7 says a failure is reported and never swallowed: without
/// this, a caller timing out looks like a slow device rather than a reply the queue had no room for.
///
/// Rate-limited on the same pattern as `tx_fail` above: the first, then every 64th. A reply fails when
/// the caller's queue is full, which under a chaos storm is a burst rather than a one-off, and an
/// unbounded log there would bury the thing it is reporting.
fn note_reply<E>(r: Result<(), E>, ctx: &ServiceContext, fails: &mut u32) {
    if r.is_err() {
        *fails = fails.saturating_add(1);
        if *fails == 1 || *fails % 64 == 0 {
            ctx.log_fmt(format_args!(
                "nic-driver: reply send FAILED x{} (caller is gone, or its queue is full - it will time out)", fails));
        }
    }
}

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
    let mut rx_idx = 0usize;   // the single TX descriptor (slot 0) is used directly, no tx index needed
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
    let mut tx_fail_logged = 0u32;         // diagnose the first few TX timeouts to guide the root-cause fix
    // Counts replies that could not be delivered; see `note_reply`.
    let mut reply_fails = 0u32;
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
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s)), &ctx, &mut reply_fails);
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
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [9] BATCH RX DRAIN (see BATCH_MAX doc): drain up to BATCH_MAX frames off the ring in ONE bounded
        // poll, length-prefixed, so net-stack scans past stray broadcasts for its reply in a single
        // round-trip. Ready descriptors are drained back-to-back (no yield); when the ring is empty we
        // poll (RX_POLL_MAX total) for more to arrive - so the whole call is ONE bounded poll, not N.
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 9 } {
            let mut out = [0u8; BATCH_MSG_MAX];
            let mut opos = 1usize;   // out[0] = frame count, filled at the end
            let mut nfr = 0u8;
            let mut rs = 0u32;
            while rs < RX_POLL_MAX && (nfr as usize) < BATCH_MAX {
                let rd = RX_RING_OFF + rx_idx * 16;
                let o1 = arena.read32(rd);
                if o1 & RTL_DESC_OWN == 0 {
                    let flen = ((o1 & 0x3FFF) as usize).min(FRAME_MAX);
                    if opos + 2 + flen > out.len() { break; }   // reply full - stop cleanly
                    out[opos..opos + 2].copy_from_slice(&(flen as u16).to_le_bytes());
                    opos += 2;
                    for i in 0..flen { out[opos + i] = arena.read8(RX_BUF_OFF + rx_idx * RX_BUF_SIZE + i); }
                    opos += flen;
                    nfr += 1;
                    last_rx_len = flen as u16;
                    rtl_arm_rx(arena, rx_idx);                  // give the descriptor back to the NIC
                    rx_idx = (rx_idx + 1) % RX_RING_COUNT;
                } else {
                    ctx.yield_cpu();
                    rs += 1;
                }
            }
            out[0] = nfr;
            if nfr > 0 { rx_count = rx_count.saturating_add(nfr as u16); }
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out[..opos])), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [5] REGISTER DUMP: the raw RTL8168 chip state for `net stats` - CR (RE/TE), config regs, ring
        // bases, and each RX descriptor's OWN/len (are frames waiting, or is the ring armed?). Chip-tagged
        // (byte 0 = 0 realtek). No TX, no RX poll - just reads.
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 5 } {
            // SIZED FROM THE RING, not hard-coded. This was `[0u8; 43]` - exactly 27 + 4*4, matching
            // the OLD four-descriptor ring - and the loop below writes four bytes per descriptor. When
            // RX_RING_COUNT went to 8 (to make RDLEN a legal length) this ran off the end and PANICKED
            // the driver on `net stats`: "range end index 47 out of range for slice of length 43".
            // A coupled constant I changed one half of.
            //
            // The supervisor restarted the driver and the machine carried on, which is the system
            // behaving correctly - but `net stats` was dead until now. Deriving the size means the two
            // cannot disagree again.
            const STAT_FIXED: usize = 27;                       // header fields, before the ring dump
            let mut s = [0u8; STAT_FIXED + RX_RING_COUNT * 4];
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
                let o = STAT_FIXED + i * 4;
                s[o..o + 4].copy_from_slice(&opts1.to_le_bytes());
            }
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s)), &ctx, &mut reply_fails);
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
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[1])), &ctx, &mut reply_fails);
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

        // --- Transmit: copy the frame in, point THE SINGLE TX descriptor (slot 0) at it, kick TPPoll,
        // wait on OWN clearing (Commandment VIII, bounded + loud). ---
        //
        // A SINGLE descriptor (EOR always set, so the NIC's TX ring is one entry that wraps 0->0) is
        // used deliberately. The frame path is strictly one-at-a-time - net-stack sends a frame, waits
        // for the reply, then sends the next - so an 8-deep ring buys no parallelism and only lets the
        // NIC's internal TX head DESYNC from the driver's index. That desync is the CONFIRMED cause of
        // the RTL8168 TX timeout on the Wyse: the diagnostic showed `desc=0xb000004a isr=0x0091` -
        // the descriptor still OWN-set + FS/LS (untouched by the NIC) while ISR latched TDU (Tx
        // Descriptor Unavailable), i.e. the NIC's head was NOT at the descriptor we wrote, so it saw
        // "no work" and skipped ours. With exactly one descriptor the head cannot drift from it.
        let frame = req.payload_bytes();
        let flen = frame.len().min(RX_BUF_SIZE);
        for i in 0..flen { arena.write8(TX_BUF_OFF + i, frame[i]); }
        let td = TX_RING_OFF;                       // the single TX descriptor (slot 0)
        let tx_buf = arena.phys_at(TX_BUF_OFF);
        arena.write32(td + 8, (tx_buf & 0xffff_ffff) as u32);
        arena.write32(td + 12, (tx_buf >> 32) as u32);
        arena.write32(td + 4, 0);
        // OWN + EOR (single-entry ring) + FS + LS + length. OWN set LAST (the NIC may read at once).
        let o1 = RTL_DESC_OWN | RTL_DESC_EOR | RTL_DESC_FS | RTL_DESC_LS | (flen as u32 & 0x3FFF);
        arena.write32(td, o1);
        mmio.write8(RTL_TPPOLL, RTL_TPPOLL_NPQ);
        let mut ts = 0u32;
        // Same clock bound as the e1000 path - see TX_CONFIRM_MS for why a yield COUNT was wrong.
        let t_end = ctx.read_tsc().wrapping_add(ctx.duration_cycles(TX_CONFIRM_MS));
        while arena.read32(td) & RTL_DESC_OWN != 0 && ctx.read_tsc() < t_end { ctx.yield_cpu(); ts += 1; }
        let tx_done = arena.read32(td) & RTL_DESC_OWN == 0;
        if !tx_done {
            // With a single descriptor a ring desync is impossible, so a timeout here is a genuine NIC
            // hiccup, not a cascade. Fail FAST (the tight bound above), DIAGNOSE (first few, to confirm
            // the desync is gone), and RECOVER the engine: TE off/on to reset it, clear latched status,
            // drop the stuck OWN. RE stays on, so RX is undisturbed (VIII bounded+loud, IX recover).
            let isr = mmio.read16(RTL_ISR);
            let cr  = mmio.read8(RTL_CR);
            if tx_fail_logged < 6 {
                ctx.log_fmt(format_args!(
                    "nic-driver: RTL8168 TX timeout - desc={:#010x} isr={:#06x} cr={:#04x} len={} - recovering",
                    arena.read32(td), isr, cr, flen));
                tx_fail_logged += 1;
            }
            mmio.write8(RTL_CR, RTL_CR_RE);             // TE off: reset the TX engine (RX stays up)
            mmio.write16(RTL_ISR, 0xFFFF);             // clear any latched TX error/status
            arena.write32(td, 0);                       // drop the stuck descriptor's OWN
            let tx_ring = arena.phys_at(TX_RING_OFF);
            mmio.write32(RTL_TNPDS, (tx_ring & 0xffff_ffff) as u32);
            mmio.write32(RTL_TNPDS + 4, (tx_ring >> 32) as u32);
            mmio.write8(RTL_CR, RTL_CR_RE | RTL_CR_TE);  // TE back on - re-reads TNPDS
        }

        // TRANSMIT ONLY - no coupled receive. This used to poll an RX descriptor here and return the
        // frame it found as the answer to the SEND, which is a frame sink attached to an operation that
        // has nothing to do with receiving: any caller that ignored the reply to its own transmit
        // destroyed a frame. On the ARM port that destroyed essentially every ARP reply, because a
        // gateway answers within a millisecond and lands inside exactly that poll.
        //
        // Frames now arrive only through the ops whose job is receiving ([4] and [9]). Nothing is
        // stranded: the descriptor is simply left owned by us for the next drain, which reads and
        // re-arms it identically, and every caller that transmits already drains afterwards.
        last_tx_done = tx_done;
        tx_count = tx_count.saturating_add(1);

        // ONE BYTE, NOT EMPTY - a zero-length message cannot be delivered at all. The kernel rejects
        // the send (`validate_user_ptr` fails on `len == 0`), so the caller waits out its full
        // deadline for a reply that never left. Same defect and same fix as the transmit reply
        // further down; no caller of a transmit reads its payload.
        note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), &ctx, &mut reply_fails);
        ctx.remove_cap(reply_cap);
    }
}

/// Serve the frame interface. A 1-byte `[3]` STATUS query gets `sreply` ([ok, mac(6)]) back - the
/// `net` nic-mac diagnostic. Every other request (a frame from net-stack) gets an EMPTY reply, so
/// net-stack degrades rather than hangs (§26.7). Never returns.
fn serve_status(ctx: &ServiceContext, sreply: &[u8]) -> ! {
    // Counts replies that could not be delivered; see `note_reply`.
    let mut reply_fails = 0u32;
    loop {
        let req = ctx.recv();
        let reply_cap = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        let p = req.payload_bytes();
        if p.len() == 1 && p[0] == 3 {
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(sreply)), &ctx, &mut reply_fails);
        } else {
            // Unrecognised op: still ANSWER. An empty reply is undeliverable (the kernel refuses a
            // zero-length send), so this used to leave the caller waiting out its deadline for a
            // request the driver had already decided it would not serve. One byte says so.
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[1u8])), &ctx, &mut reply_fails);
        }
        ctx.remove_cap(reply_cap);
    }
}

/// Kernel-NIC backend: bridge the frame IPC (the request/reply contract net-stack speaks) to whatever
/// network device the kernel drives, via the NET_DEVICE syscalls. Pure mechanism, mirroring the
/// e1000/rtl serve loops - the frame IS the message; net-stack owns all protocol. A request payload of
/// exactly 1 byte 3/4/5/6/7/8/9 is an opcode; any other payload is a raw ethernet frame to transmit.
///
/// Used by both ARM ports, and deliberately named for the SEAM rather than the device behind it:
/// - **Pi 2 (arm)**: an in-kernel DWC2 CDC-ECM USB-net device, pinned to core 0 by its contract
///   because that is where the single-channel DWC2 is driven from.
/// - **Pi 4 (aarch64)**: the on-board GENET Ethernet MAC, with no core constraint - GENET is reached
///   by MMIO from whichever core makes the syscall, so it sits on core 1 with net-stack and fs.
///
/// This function knows about neither. It was `usb_net_main` while USB was the only thing behind the
/// syscalls; on the Pi 4 that name would have described the transport of a different board.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn kernel_net_main(ctx: ServiceContext) -> ! {
    // How many bulk-IN polls to try when a request wants a received frame (net-stack also re-polls via
    // ops 4/9 under its own deadline, so this is a bounded best-effort, not a spin).
    // ONE check per request on arm32, eight elsewhere.
    //
    // Off arm a poll is a cheap syscall, so re-asking gives the device a moment at no real cost. On
    // arm32 the device now lives behind IPC, and eight round trips is eight times the latency of the
    // answer net-stack is BLOCKED waiting for - which is why the shell kept reporting "net-stack not
    // responding" while ping ran. A service blocking its own caller past their patience is the failure
    // this system exists to avoid, and the poll loop was mine.
    //
    // It is also pointless work now: the bulk-IN is armed CONTINUOUSLY in the background, so asking
    // eight times in one request cannot make a frame arrive sooner. It either has one or it does not,
    // and net-stack polls again immediately.
    #[cfg(target_arch = "arm")]
    const RX_TRIES: usize = 1;
    #[cfg(not(target_arch = "arm"))]
    const RX_TRIES: usize = 8;
    const FRAME_MAX: usize = 1600;
    const BATCH_MAX: u8 = 8;
    const BATCH_MSG_MAX: usize = 3072;
    // Failed transmits, so the report is rate-limited rather than per-frame. Serve-loop-local (this
    // service's own state, threaded through the loop) - not a file-scope static.
    let mut tx_fail: u32 = 0;

    // On arm32 the USB stack is a SERVICE now, so the NET_DEVICE syscalls are replaced by IPC to
    // `dwc2` - the same move `block-driver` made in slice 3c, and the same reason: the driver left the
    // kernel, so the syscalls that existed only to reach it have nothing behind them.
    //
    // Opcodes start at 0x10 because `dwc2` serves the BLOCK protocol on the same endpoint and block
    // uses 1..5. One endpoint, two protocols, one opcode space - a collision here would route a frame
    // to the disk.
    // ONE request to `dwc2`, with a single reacquire-and-retry. `find_send_slot` does NOT resolve a
    // name - it reads the spawn-time wiring and a cache - so a peer spawned AFTER us is unreachable
    // forever unless we reacquire. `dwc2` is exactly that peer (it is spawned by hand today), and the
    // failure is silent from here: `request_with_reply` returns None INSTANTLY, which reads as a dead
    // cable rather than a missing cap. block-driver learned this in slice 3c; this is the same edge.
    #[cfg(target_arch = "arm")]
    let dwc2_rpc = |ctx: &ServiceContext, msg: &Message| -> Option<Message> {
        // Bounded on the lean await: a dwc2 that is alive but silent hung this driver, net-stack
        // behind it, and the shell behind that.
        const DWC2_SECS: i64 = 10;
        // The op we asked about. Every reply is tagged with it (see `net_dispatch` in dwc2), which is
        // what lets a stale answer be recognised instead of believed.
        let want = msg.payload_bytes().first().copied().unwrap_or(0);
        // NEVER RE-SEND AFTER A TIMEOUT. This used to retry unconditionally, and for the RX op that is
        // actively destructive: dwc2 POPS a frame to build each reply, so re-asking after a timeout pops
        // a SECOND frame while the first reply is still queued unread. The retry exists for one real
        // case - `find_send_slot` reads spawn-time wiring, so a peer spawned after us is unreachable
        // until reacquired - and that case is a SEND failure, which is distinguishable from a timeout.
        let got = match ctx.request_with_reply_deadline_outcome("dwc2", msg, DWC2_SECS) {
            DeadlineOutcome::Reply(r) => r,
            DeadlineOutcome::SendFailed if ctx.reacquire_by_name("dwc2") =>
                ctx.request_with_reply_deadline("dwc2", msg, DWC2_SECS)?,
            _ => return None,
        };
        // RE-SYNC ON THE TAG, BY DROPPING THE STALE ANSWER AND NOT ASKING AGAIN.
        //
        // A reply that answers a different op is a previous request's answer arriving after we stopped
        // waiting for it. Believing it is what destroyed frames: an RX reply read as an INFO reply is a
        // frame consumed as a status word, and every reply afterwards is one behind, permanently.
        //
        // The repair is to CONSUME it and report failure - not to re-ask. Re-asking is the destructive
        // move this function was just fixed to stop doing: each RX request pops a frame off the device
        // to build its reply, so a resend costs a frame every time round. Consuming the stale one has
        // already shortened the queue by one, so the alignment improves with each occurrence and repairs
        // itself. The caller sees "nothing this time" - `dev_rx` returns 0, the batch loop stops, and
        // net-stack polls again a few milliseconds later - which is a normal quiet-network answer and
        // costs only the frame that was already late.
        if got.payload_bytes().first().copied() != Some(want) {
            return None;
        }
        Some(got)
    };
    #[cfg(target_arch = "arm")]
    let dev_info = |ctx: &ServiceContext, out: &mut [u8; 7]| -> bool {
        // Replies are [op, body...]; `dwc2_rpc` has already checked the op, so the body starts at 1.
        match dwc2_rpc(ctx, &Message::from_bytes(&[0x10])) {
            Some(r) => {
                let p = r.payload_bytes();
                if p.len() < 9 || p[1] == 0 { return false; }
                out[0..6].copy_from_slice(&p[2..8]);
                out[6] = p[8];
                true
            }
            None => false,
        }
    };
    #[cfg(target_arch = "arm")]
    let dev_tx = |ctx: &ServiceContext, frame: &[u8]| -> bool {
        // C3-1: these were bare 1514 literals - a SEVENTH copy of the frame size, and the one the
        // compiler could not even see disagreeing. Use the module's FRAME_MAX so there is one fewer.
        let mut req = [0u8; 1 + FRAME_MAX];
        let n = frame.len().min(FRAME_MAX);
        req[0] = 0x11;
        req[1..1 + n].copy_from_slice(&frame[..n]);
        match dwc2_rpc(ctx, &Message::from_bytes(&req[..1 + n])) {
            Some(r) => { let p = r.payload_bytes(); p.len() > 1 && p[1] != 0 }
            None => false,
        }
    };
    #[cfg(target_arch = "arm")]
    let dev_rx = |ctx: &ServiceContext, buf: &mut [u8]| -> usize {
        match dwc2_rpc(ctx, &Message::from_bytes(&[0x12])) {
            Some(r) => {
                let p = r.payload_bytes();
                if p.len() < 3 { return 0; }
                let n = (p[1] as usize) | ((p[2] as usize) << 8);
                // A length the reply cannot back is a malformed answer, not a short frame - taking it
                // would hand the stack whatever followed in the message buffer.
                if n == 0 || p.len() < 3 + n || n > buf.len() { return 0; }
                buf[..n].copy_from_slice(&p[3..3 + n]);
                n
            }
            None => 0,
        }
    };
    #[cfg(not(target_arch = "arm"))]
    let dev_info = |ctx: &ServiceContext, out: &mut [u8; 7]| -> bool { ctx.net_info(out) };
    #[cfg(not(target_arch = "arm"))]
    let dev_tx = |ctx: &ServiceContext, frame: &[u8]| -> bool { ctx.net_frame_tx(frame) };
    #[cfg(not(target_arch = "arm"))]
    let dev_rx = |ctx: &ServiceContext, buf: &mut [u8]| -> usize { ctx.net_frame_rx(buf) };

    let mut info = [0u8; 7];
    if dev_info(&ctx, &mut info) {
        ctx.log_fmt(format_args!(
            // "NIC up", not "kernel NIC up": on ARM the device is reached through the `dwc2`
            // SERVICE, not through the kernel, and the old wording sent me looking for an in-kernel
            // backend that this port does not have while diagnosing a receive fault.
            "nic-driver: NIC up  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  link {}",
            info[0], info[1], info[2], info[3], info[4], info[5], if info[6] != 0 { "UP" } else { "down" }));
    } else {
        ctx.log("nic-driver: no usb-net device - serving empty replies (net degrades, not hangs)");
    }
    ctx.log("nic-driver: serving frame interface");

    // Poll the bulk IN endpoint up to RX_TRIES times for one received frame; returns its length (0 = none).
    let rx_one = |ctx: &ServiceContext, buf: &mut [u8]| -> usize {
        for _ in 0..RX_TRIES {
            let n = dev_rx(ctx, buf);
            if n > 0 { return n; }
            ctx.yield_cpu();                      // give the device / QEMU a moment to queue a frame
        }
        0
    };

    // Counts replies that could not be delivered; see `note_reply`.
    let mut reply_fails = 0u32;
    // Attempts at the read-only info query before reporting failure. See the STATUS arm below.
    const INFO_TRIES: u8 = 3;
    // Info queries the device could not answer. Serve-loop-local, not a file-scope static.
    let mut info_fails: u32 = 0;
    loop {
        let _req = ctx.recv();
        let reply_cap = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        let p = _req.payload_bytes();

        if p.len() == 1 && p[0] == 3 {
            // STATUS: [ok, mac(6), link] - net-stack reads MAC at [1..7] and link at [7].
            let mut out = [0u8; 8];
            let mut ni = [0u8; 7];
            // RETRY before giving up. On failure this replies all zeros, and a zero link byte in a
            // well-formed reply is indistinguishable from a dead cable - so ONE timed-out query to
            // dwc2 reached the operator as "cable unplugged" with the cable plainly in.
            //
            // Why the retry belongs HERE, from the evidence rather than from reasoning about the
            // code: dwc2's own link read never answered "down" once the link was up, and net-stack
            // never failed to get a reply. Only this query was failing. It is read-only, so
            // re-asking costs nothing and pops nothing - unlike the RX op next door, whose comment
            // rightly forbids a re-send.
            let mut got = false;
            for _ in 0..INFO_TRIES {
                if dev_info(&ctx, &mut ni) { got = true; break; }
            }
            if got {
                out[0] = 1;
                out[1..7].copy_from_slice(&ni[0..6]);
                out[7] = ni[6];
            } else {
                // SAY SO. On failure this replies all zeros, which means ok=0 AND link=0 - and a
                // reader that checks only the link byte reads that as "the cable is out". It did:
                // ping on a Pi 2 blamed an unplugged cable while the cable was in. The ok byte now
                // carries the distinction to the caller, and this line says which half failed here,
                // because "the driver could not determine the link" and "dwc2 did not answer" are
                // different faults and the reply alone cannot tell them apart.
                //
                // Rate-limited: the first few, then every 64th, so a persistently silent device is
                // loud once rather than once per ping.
                info_fails += 1;
                if info_fails <= 3 || info_fails % 64 == 0 {
                    ctx.log_fmt(format_args!(
                        "nic-driver: dwc2 did not answer the info query after {} tries - reporting link                          down, which may be wrong ({} so far)",
                        INFO_TRIES, info_fails
                    ));
                }
            }
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out)), &ctx, &mut reply_fails);
        } else if p.len() == 1 && p[0] == 4 {
            // RX-only: one frame, no TX.
            let mut rx = [0u8; FRAME_MAX];
            let n = rx_one(&ctx, &mut rx);
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rx[..n])), &ctx, &mut reply_fails);
        } else if p.len() == 1 && p[0] == 9 {
            // BATCH RX drain: [count:u8] then per frame [len:u16 LE][bytes].
            let mut out = [0u8; BATCH_MSG_MAX];
            let mut opos = 1usize;
            let mut count = 0u8;
            while count < BATCH_MAX {
                // Check a MAX-size frame would fit BEFORE dequeuing, so a frame is never pulled off the
                // device only to be dropped for lack of room (userspace-audit Audit 5, A5-U2). net-stack
                // re-polls (op 4/9) for whatever we stop short of.
                if opos + 2 + FRAME_MAX > out.len() { break; }
                let mut rx = [0u8; FRAME_MAX];
                let n = dev_rx(&ctx, &mut rx);
                if n == 0 { break; }
                out[opos] = (n & 0xff) as u8;
                out[opos + 1] = ((n >> 8) & 0xff) as u8;
                opos += 2;
                out[opos..opos + n].copy_from_slice(&rx[..n]);
                opos += n;
                count += 1;
            }
            out[0] = count;
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out[..opos])), &ctx, &mut reply_fails);
        } else if p.len() == 1 && matches!(p[0], 5 | 6 | 7 | 8) {
            // UNSUPPORTED on this backend - answered `[0]`, not `[1]`.
            //
            // Ops 6/7/8 are the chaos force-link override. Acking them with `1` (success) meant
            // `chaos link-flap` printed "forcing link DOWN ... net-stack should self-configure ... done"
            // while nothing had been overridden: a chaos trial reporting it had exercised link recovery
            // having exercised nothing. A test that cannot fail is not a test (Commandment II), and this
            // one was worse than absent because it read as passing.
            //
            // Op 5 is a register dump; answering 1 byte to a caller expecting 25 is the same lie in
            // miniature. The original comment was right that a caller must not hang and wrong that an
            // ack was the remedy: the caller needs an ANSWER, and "not supported here" is one.
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), &ctx, &mut reply_fails);
        } else {
            // TX FRAME (any multi-byte payload). TRANSMIT ONLY - the reply carries NO received frame.
            //
            // It used to: "transmit, then hand back one received frame". That coupling put a frame sink
            // on an operation that has nothing to do with receiving, and it cost days. A caller that did
            // not care about the reply to its send - which is most of them, reasonably - destroyed
            // whatever frame the coupled poll had just pulled off the device.
            //
            // It was not a rare loss either. A gateway answers an ARP request in about a millisecond,
            // squarely inside that poll, so the coupled receive caught the ARP reply essentially every
            // time and `arp_resolve` dropped it. ARP never resolved on this board while DHCP worked
            // perfectly, because a DHCP offer takes tens of milliseconds and lands after the poll gives
            // up, arriving through the drain where somebody is looking.
            //
            // Scanning the send's reply fixed the symptom; this removes the shape that caused it. Now a
            // frame can only arrive through an operation whose job is receiving ([4] and [9]), so no
            // caller can lose one by ignoring an answer it never asked for.
            //
            // Nothing is stranded by the change: not fetching the frame leaves it queued for the next
            // drain, and every caller that transmits already drains afterwards. What is lost is at most
            // one poll interval of latency on the first frame after a send.
            if !dev_tx(&ctx, p) {
                tx_fail = tx_fail.saturating_add(1);
                if tx_fail == 1 || tx_fail % 64 == 0 {
                    ctx.log_fmt(format_args!("nic-driver: usb-net TX FAILED x{} (frame not sent)", tx_fail));
                }
            }
            // A ONE-BYTE ANSWER, BECAUSE AN EMPTY MESSAGE CANNOT BE DELIVERED AT ALL.
            //
            // The kernel rejects a zero-length send outright - `validate_user_ptr` returns false for
            // `len == 0`, so `build_message` fails and the reply never leaves. The caller then waits
            // out its whole deadline for an answer that was never on the wire. Measured on hardware:
            // every ping cost `nic-driver gave NO ANSWER after 2012 ms (budget 1 s) for op 0 [why -1]`
            // - two attempts timing out - while the ICMP round trip itself took 66 ms. The ping was
            // not slow; this acknowledgement was undeliverable.
            //
            // The comment that used to be here argued an empty reply was SAFER, because a caller
            // might miscount a status byte as a received frame. That was true of the OLD coupled
            // behaviour, where this reply carried a frame. It does not carry one any more (see the
            // TRANSMIT ONLY note above), and every transmit call site now tests only `is_none()` /
            // `is_some()` - none reads the payload. The comment outlived the code it described.
            //
            // 0 = sent. `fs` reached the same conclusion for its own protocol and wrote it down
            // there: an empty reply is not an answer. It is not even a message.
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[0u8])), &ctx, &mut reply_fails);
        }
        ctx.remove_cap(reply_cap);
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("nic-driver: starting");

    // Pi 4, with the driver where it belongs: the GENET MAC is ours, reached through the register
    // window and DMA arena the kernel granted this service by name. No NET_DEVICE syscall is involved
    // and the kernel drives no ethernet at all - which is the whole point (Commandment I, §4.4).
    #[cfg(target_arch = "aarch64")]
    genet::genet_main(ctx);

    // Both ARM ports, otherwise: there is no PCIe NIC to scan for. The device is driven in-kernel
    // (Pi 2: a DWC2 CDC-ECM USB adapter; Pi 4: the on-board GENET MAC) and this backend bridges the
    // same frame IPC net-stack speaks to the NET_DEVICE syscalls. Same request/reply contract,
    // different transport - exactly the block-driver x86/ARM split.
    #[cfg(target_arch = "arm")]
    kernel_net_main(ctx);

    // Which NIC did the kernel find? nic-driver drives an Intel e1000 (the QEMU dev NIC) or a Realtek
    // RTL8168 (the T630); the kernel maps whichever one's BAR. Dispatch on the PCI identity (Phase 4).
    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    if ctx.nic_vendor_device() == 0x8168_10EC {
        realtek_main(ctx); // RTL8168 - a separate path that never returns
    }

    // --- Intel e1000 path. The kernel mapped our BAR + DMA arena only if the discovered NIC is a real
    // Intel e1000 (Commandment VII). On any other NIC or none, we still SERVE the frame interface -
    // with empty replies - so net-stack degrades instead of hanging on a reply (§26.7).
    let mmio  = ctx.mmio();
    let arena = ctx.dma_region();
    // NOTE: there is deliberately no `active` boolean here any more. It was a second copy of a fact
    // the two Options already hold (Commandment III), and every site that consulted it then re-asserted
    // that fact with `unwrap()` - a service declaring that its own failure should halt the machine
    // (Commandment V, and the Rule Above The Rules). `if let` binds and proves in the same step, so the
    // fact is checked exactly where it is used and there is no second truth to keep in sync.
    let mut e1000_mac = [0u8; 6];

    if let (Some(m), Some(a)) = (mmio.as_ref(), arena.as_ref()) {

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
        // HAND THE NIC DESCRIPTORS IT OWNS: buffer address set, and status CLEAR.
        //
        // A descriptor with DD still set reads as "a frame landed here" the moment the receiver is
        // armed. The arena is not guaranteed zeroed, so the very first drain could consume a phantom,
        // advance next-to-clean past a descriptor the NIC had not written, and desync from RDH - after
        // which the ring never recovers. Measured exactly that: ONE frame handed out per driver
        // lifetime, then nothing, while the wire carried thirty.
        //
        // This step was previously done by the per-request ring reset, which also destroyed every
        // frame that had arrived meanwhile - so removing that reset (the right fix) exposed an init
        // that had never done its own job. Doing it here is where it always belonged: arming a ring is
        // an init concern, not something to redo on every request.
        for i in 0..RX_RING_COUNT {
            a.write8(RX_RING_OFF + i * 16 + 12, 0);   // status: not done
            a.write8(RX_RING_OFF + i * 16 + 13, 0);   // errors
        }
        for i in 0..128usize { m.write32(REG_MTA + i * 4, 0); }
        let rx_ring_phys = a.phys_at(RX_RING_OFF);
        m.write32(REG_RDBAL, (rx_ring_phys & 0xffff_ffff) as u32);
        m.write32(REG_RDBAH, (rx_ring_phys >> 32) as u32);
        m.write32(REG_RDLEN, RX_RING_BYTES);
        m.write32(REG_RDH, 0);
        m.write32(REG_RDT, (RX_RING_COUNT - 1) as u32);

        // ENABLE THE RECEIVER ONCE, AND LEAVE IT ON.
        //
        // It used to be switched on at the top of every drain request and off again at the bottom, so
        // the NIC was deaf except during the microseconds a client happened to be asking - and each
        // drain first wiped every descriptor's status byte, destroying whatever HAD arrived meanwhile.
        // The wire capture is unambiguous about the cost: 12 DHCP replies and 12 ARP frames came back
        // from the server and `net-stack` scanned ZERO frames, so DHCP reported "no offer" and blamed
        // the driver for refusing transmits that had in fact gone out.
        //
        // A NIC is not a device you switch on to ask it a question. The receiver runs, frames land in
        // the ring, and software consumes them at its own pace - that is what the ring is FOR, and it
        // is the silicon's design rather than a policy choice of ours (26.14). The Realtek path in
        // this same file already did exactly this; only e1000 was left as the step-4 stub the module
        // header still describes.
        m.write32(REG_RCTL, RCTL_VALUE);
        ctx.log("nic-driver: serving the frame interface");
    } else {
        ctx.log("nic-driver: no Intel e1000 mapped (absent, or a different NIC) - serving empty replies");
    }

    // The frame interface: a request/reply server (§8.2, like examples/reply-server). Each request's
    // payload is a frame to transmit; we reply with the frame that came back (empty if none / no NIC).
    let mut rxbuf = [0u8; FRAME_MAX];
    let mut tx_idx = 0usize;
    // Next RX descriptor to clean. Persists across requests because the RING does - see the drain.
    let mut rx_idx = 0usize;
    // Counts replies that could not be delivered; see `note_reply`.
    let mut reply_fails = 0u32;
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
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&sreply)), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [4] RX-ONLY: arm the receiver, poll for ONE frame, quiesce - NO TX. Mirrors the realtek RX-only
        // so net-stack's collect-frames-after-one-TX DNS path works on both NICs (on QEMU/slirp there are
        // no stray frames, so net-stack's first request already matches and this stays a safe no-op).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 4 } {
            let mut n = 0usize;
            if let (Some(m), Some(a)) = (mmio.as_ref(), arena.as_ref()) {
                // Same ring discipline as the batch drain: take the next descriptor the NIC has
                // finished with, hand it straight back, and leave the receiver alone.
                let d = RX_RING_OFF + rx_idx * 16;
                if a.read8(d + 12) & RXD_STA_DD != 0 {
                    n = (a.read16(d + 8) as usize).min(FRAME_MAX);
                    for i in 0..n { rxbuf[i] = a.read8(RX_BUF_OFF + rx_idx * RX_BUF_SIZE + i); }
                    a.write8(d + 12, 0);
                    a.write64(d, a.phys_at(RX_BUF_OFF + rx_idx * RX_BUF_SIZE));
                    m.write32(REG_RDT, rx_idx as u32);
                    rx_idx = (rx_idx + 1) % RX_RING_COUNT;
                }
            }
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&rxbuf[..n])), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [9] BATCH RX DRAIN (e1000): QEMU's slirp is quiet, so this yields at most the single frame the
        // reset-and-read model gives, formatted as the batch [count:u8][len:u16 LE, bytes] - enough to
        // exercise net-stack's batch scan. The multi-frame drain that matters is the RTL8168 path (a
        // busy physical LAN).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 9 } {
            let mut out = [0u8; BATCH_MSG_MAX];
            let mut opos = 1usize;   // out[0] = frame count
            let mut nfr = 0u8;
            if let (Some(m), Some(a)) = (mmio.as_ref(), arena.as_ref()) {
                // Consume every descriptor the NIC has finished with, from where the last drain left
                // off, and hand each buffer straight back. The ring is shared state between the NIC
                // and this driver; resetting it (as this used to) discards frames already landed.
                //
                // RETURNS WHAT IS THERE, IMMEDIATELY - it does not wait for the network. A driver that
                // blocks until a frame arrives holds a core on someone else's schedule; the caller
                // paces its own polling and carries its own budget.
                while (nfr as usize) < BATCH_MAX {
                    let d = RX_RING_OFF + rx_idx * 16;
                    if a.read8(d + 12) & RXD_STA_DD == 0 { break; }   // NIC still owns it
                    let len = (a.read16(d + 8) as usize).min(FRAME_MAX);
                    if opos + 2 + len > out.len() { break; }          // reply full - stop cleanly
                    out[opos..opos + 2].copy_from_slice(&(len as u16).to_le_bytes());
                    opos += 2;
                    for i in 0..len { out[opos + i] = a.read8(RX_BUF_OFF + rx_idx * RX_BUF_SIZE + i); }
                    opos += len;
                    nfr += 1;
                    // Return the descriptor: clear status, restore its buffer address, then move the
                    // tail onto it. RDT is the last descriptor the NIC may write, so advancing it to
                    // the one just freed is what re-arms the ring.
                    a.write8(d + 12, 0);
                    a.write64(d, a.phys_at(RX_BUF_OFF + rx_idx * RX_BUF_SIZE));
                    m.write32(REG_RDT, rx_idx as u32);
                    rx_idx = (rx_idx + 1) % RX_RING_COUNT;
                }

            }
            out[0] = nfr;
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&out[..opos])), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // [5] REGISTER DUMP (e1000): CTRL/STATUS/RCTL/TCTL/RDH/RDT - chip-tagged (byte 0 = 1 e1000).
        if { let p = req.payload_bytes(); p.len() == 1 && p[0] == 5 } {
            let mut s = [0u8; 25];
            s[0] = 1;                                     // chip: e1000
            if let Some(m) = mmio.as_ref() {
                s[1..5].copy_from_slice(&m.read32(REG_CTRL).to_le_bytes());
                s[5..9].copy_from_slice(&m.read32(REG_STATUS).to_le_bytes());
                s[9..13].copy_from_slice(&m.read32(REG_RCTL).to_le_bytes());
                s[13..17].copy_from_slice(&m.read32(REG_TCTL).to_le_bytes());
                s[17..21].copy_from_slice(&m.read32(REG_RDH).to_le_bytes());
                s[21..25].copy_from_slice(&m.read32(REG_RDT).to_le_bytes());
            }
            note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&s)), &ctx, &mut reply_fails);
            ctx.remove_cap(reply_cap);
            continue;
        }

        // Whether the NIC confirmed the send within TX_CONFIRM_MS - reported in the reply below.
        let mut tx_confirmed = false;
        if let (Some(m), Some(a)) = (mmio.as_ref(), arena.as_ref()) {
            let frame = req.payload_bytes();
            let flen = frame.len().min(FRAME_MAX);

            // --- Arm the RECEIVER FIRST (reset the ring to head 0, then enable), BEFORE transmitting.
            // The reply can come back faster than we could otherwise switch the receiver on - slirp's
            // ICMP echo is a trivial src/dst swap, quicker than its ARP-table reply - and a frame that
            // arrives with the receiver off is DROPPED (this is exactly why the ping's echo reply, on
            // the wire in the pcap, was never seen). Resetting head/tail per request keeps each RX
            // independent; RDH/RDT are written while the receiver is briefly off, which is safe.
            // TRANSMIT TOUCHES NOTHING ON THE RECEIVE SIDE. This used to wipe every RX descriptor's
            // status, reset RDH/RDT and cycle RCTL around each send - so every frame sent DESTROYED
            // the receive ring. Send a DISCOVER, the ring is wiped; the OFFER lands; the next send
            // erases it. That is why 16 replies sat on the wire while `net-stack` scanned zero frames
            // and then blamed the driver for transmits that had in fact gone out.
            //
            // Nothing in the e1000 requires it: TX and RX are separate rings with separate head/tail
            // registers, and the receiver runs continuously. Cycling RCTL to send was never the
            // silicon's requirement, only this driver's habit (26.14).

            // --- Transmit: copy the frame into the TX buffer, point descriptor tx_idx at it, hand it
            // to the NIC (advance TDT), wait on the DD bit (Commandment VIII, bounded + loud).
            for i in 0..flen { a.write8(TX_BUF_OFF + i, frame[i]); }
            let td = TX_RING_OFF + tx_idx * 16;
            a.write64(td, a.phys_at(TX_BUF_OFF));
            a.write16(td + 8, flen as u16);
            a.write8(td + 11, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
            a.write8(td + 12, 0); // clear DD
            m.write32(REG_TDT, ((tx_idx + 1) % TX_RING_COUNT) as u32);
            let t_end = ctx.read_tsc().wrapping_add(ctx.duration_cycles(TX_CONFIRM_MS));
            while a.read8(td + 12) & TXD_STA_DD == 0 && ctx.read_tsc() < t_end { ctx.yield_cpu(); }
            tx_confirmed = a.read8(td + 12) & TXD_STA_DD != 0;
            tx_idx = (tx_idx + 1) % TX_RING_COUNT;

            // TRANSMIT ONLY - no coupled receive. There used to be a bounded wait here for a frame to
            // land in descriptor 0, returned as the answer to the SEND. That is a frame sink attached
            // to an operation with nothing to do with receiving, and any caller that ignored the reply
            // to its own transmit destroyed whatever had just been picked up. Receiving is [4] and [9];
            // the receiver stays armed and the frame waits for one of them.
            // (no RCTL quiesce: the receiver stays on - see the note above)
        }

        // Reply NON-BLOCKING (§8.9): a slow/dead net-stack can never wedge us. Then reclaim the cap
        // slot so a long-running server stays bounded (§26.6). EMPTY, not a status byte - callers
        // already guard on `is_empty()` for "no frame", so nothing downstream changes and a status
        // byte would be miscounted as a received frame by `udp_roundtrip`.
        // A ONE-BYTE ANSWER, NEVER AN EMPTY ONE.
        //
        // This replied with a zero-length payload, and transmit was the ONLY op in this driver that
        // did - every other one answers with at least a status or a count. Transmit was also the only
        // op whose caller reported no answer: measured at 2241 ms against a 1 s budget, which is the
        // deadline expiring twice (the call, then the reacquire-and-retry). Meanwhile the wire showed
        // those frames going out and the server answering them, so `net-stack` reported "never left
        // the host - the driver refused them" about frames it had sent perfectly well.
        //
        // `fs` reached the same conclusion in its own serve loop and wrote it down there: an empty
        // reply is not an answer in this protocol. Say something.
        //
        // 0 = handed to the NIC and confirmed done; 1 = handed over but not confirmed within
        // TX_CONFIRM_MS. No caller needs the distinction today, but a driver that knows and says
        // nothing is the silent degradation §26.7 forbids.
        let tx_status: u8 = if tx_confirmed { 0 } else { 1 };
        note_reply(ctx.try_send_by_handle(reply_cap, &Message::from_bytes(&[tx_status])), &ctx, &mut reply_fails);
        ctx.remove_cap(reply_cap);
    }
}
