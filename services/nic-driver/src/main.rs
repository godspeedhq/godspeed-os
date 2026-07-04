// SPDX-License-Identifier: GPL-2.0-only
//! nic-driver - the userspace NIC driver service (docs/networking.md, Phase 1).
//!
//! Model-specific driver for the Intel 82540EM ("e1000"), the QEMU dev NIC. An ordinary
//! restartable, IOMMU-confinable userspace service (Commandment I): the kernel grants it only
//! the NIC's MMIO BAR + a DMA arena, by name, and only when the discovered NIC is a real Intel
//! e1000; all device logic lives here, `unsafe`-free behind the SDK `Mmio`/`Dma` wrappers
//! (§18.1). The T630's Realtek chipset is a separate Phase-4 driver behind the same (future)
//! NIC-agnostic frame interface, so `net-stack` never learns the difference.
//!
//! Phase 1 progress:
//!  - step 2: reset the controller + read the link state and the MAC (from EEPROM).
//!  - step 3: a TX descriptor ring in the DMA arena; transmit a raw frame, wait on the DD bit.
//!  - step 4 (this commit): an RX descriptor ring + enable the receiver; send a broadcast ARP
//!    request (which QEMU's user-net gateway answers), then RECEIVE the reply out of the arena.
//!    That proves the full path in both directions - arena <-> ring <-> the wire. RX here is
//!    busy-poll of the ring (the proven pattern: both USB drivers wire an IRQ but busy-poll, as
//!    blocked-wake was unreliable on the T630); the receive IRQ + true blocking are a follow-up.
//!    The frame interface to `net-stack` comes next.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

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
// buffer size 2048 (BSIZE=00, BSEX=0). Promiscuous so we receive the reply regardless of MAC filter.
const RCTL_VALUE: u32 = (1 << 1) | (1 << 3) | (1 << 4) | (1 << 15) | (1 << 26);

// Legacy TX descriptor (16 B): addr@0, length(u16)@8, cmd(u8)@11, status(u8)@12.
const TXD_CMD_EOP:  u8 = 1 << 0; // end of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // insert FCS (the NIC appends the CRC)
const TXD_CMD_RS:   u8 = 1 << 3; // report status -> the NIC sets DD when the frame is sent
const TXD_STA_DD:   u8 = 1 << 0; // (TX) descriptor done
// RX descriptor (16 B): addr@0, length(u16)@8, status(u8)@12, errors(u8)@13.
const RXD_STA_DD:   u8 = 1 << 0; // (RX) descriptor done - a frame landed in this buffer

// DMA-arena layout (our granted 64 KiB arena): TX ring + one TX frame buffer, then an RX ring of
// 4 descriptors + 4 x 2 KiB receive buffers.
const TX_RING_OFF:   usize = 0x0000;
const TX_RING_BYTES: u32   = 8 * 16;
const TX_BUF_OFF:    usize = 0x1000;
const RX_RING_OFF:   usize = 0x2000;
const RX_RING_COUNT: u32   = 4;
const RX_RING_BYTES: u32   = RX_RING_COUNT * 16;
const RX_BUF_OFF:    usize = 0x3000;
const RX_BUF_SIZE:   usize = 2048;

// Bounded budgets for hardware self-clear / done polls. HARDWARE/protocol-timing bounds (the exempt
// category, like AHCI/USB spins - NOT the correctness-by-time Commandment VIII forbids): we wait on
// the TRUTH of a bit and give up LOUDLY rather than wedge the core (§26.6, §26.7).
const RESET_POLL_MAX: u32 = 1_000_000;
const TX_POLL_MAX:    u32 = 1_000_000;
const RX_POLL_MAX:    u32 = 3_000_000; // a round-trip through QEMU's user-net gateway (arrives in ms)

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("nic-driver: starting (e1000)");

    // The kernel mapped our BAR only if the discovered NIC is a real Intel e1000 (Commandment VII).
    // On any other NIC (the T630's Realtek) or none, there is no mapping - DEGRADE (Commandment V).
    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => {
            ctx.log("nic-driver: no Intel e1000 mapped (absent, or a different NIC) - idling");
            idle(&ctx);
        }
    };

    // Reset to a known state (bring-up runs on EVERY spawn - Commandments V + IX). Set CTRL.RST,
    // wait on the TRUTH of the bit self-clearing, bounded + loud.
    mmio.write32(REG_CTRL, mmio.read32(REG_CTRL) | CTRL_RST);
    let mut spins = 0u32;
    while spins < RESET_POLL_MAX && mmio.read32(REG_CTRL) & CTRL_RST != 0 {
        ctx.yield_cpu();
        spins += 1;
    }
    if spins == RESET_POLL_MAX {
        ctx.log("nic-driver: e1000 reset did not self-clear - reporting best-effort");
    }

    // Bring the link UP (Set Link Up + Auto-Speed Detection). Without SLU the e1000 link stays down
    // in QEMU: TX still reaches the backend dump, but no reply ever comes back to the receiver.
    mmio.write32(REG_CTRL, mmio.read32(REG_CTRL) | CTRL_SLU | CTRL_ASDE);

    // Link + the MAC the NIC reloaded from EEPROM (through the safe SDK `Mmio` - unsafe-free, §18.1).
    let status  = mmio.read32(REG_STATUS);
    let link_up = (status >> 1) & 1 == 1;
    let ral = mmio.read32(REG_RAL0);
    let rah = mmio.read32(REG_RAH0);
    let mac = [
        (ral & 0xff) as u8, ((ral >> 8) & 0xff) as u8, ((ral >> 16) & 0xff) as u8,
        ((ral >> 24) & 0xff) as u8, (rah & 0xff) as u8, ((rah >> 8) & 0xff) as u8,
    ];
    ctx.log_fmt(format_args!(
        "nic-driver: e1000 up  link {}  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if link_up { "UP" } else { "down" },
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));

    let arena = match ctx.dma_region() {
        Some(a) => a,
        None => {
            ctx.log("nic-driver: no DMA arena granted - cannot set up rings, idling");
            idle(&ctx);
        }
    };
    arena.zero();

    // --- Set up the RECEIVER FIRST, so we are ready before our own request goes out and the reply
    // comes back (docs/networking.md Phase 1 step 4). RX ring of 4 descriptors, each pointing at a
    // 2 KiB buffer the NIC DMAs a received frame into and marks with the DD status bit.
    for i in 0..RX_RING_COUNT as usize {
        arena.write64(RX_RING_OFF + i * 16, arena.phys_at(RX_BUF_OFF + i * RX_BUF_SIZE));
        // status byte (+12) stays 0 until the NIC fills the buffer and sets DD.
    }
    for i in 0..128usize { mmio.write32(REG_MTA + i * 4, 0); } // no multicast filtering
    let rx_ring_phys = arena.phys_at(RX_RING_OFF);
    mmio.write32(REG_RDBAL, (rx_ring_phys & 0xffff_ffff) as u32);
    mmio.write32(REG_RDBAH, (rx_ring_phys >> 32) as u32);
    mmio.write32(REG_RDLEN, RX_RING_BYTES);
    mmio.write32(REG_RDH, 0);
    mmio.write32(REG_RDT, RX_RING_COUNT - 1); // hand descriptors 0..count-1 to the NIC
    mmio.write32(REG_RCTL, RCTL_VALUE);       // enable the receiver

    // --- Build + transmit a broadcast ARP request for QEMU's user-net gateway (10.0.2.2). The
    // gateway answers with an ARP reply, giving the receiver a real frame to catch. This is a
    // fixed request packet, not the ARP subsystem (that is Phase 2) - just enough to elicit an RX.
    let mut frame = [0u8; 64];
    for b in frame.iter_mut().take(6) { *b = 0xff; }        // eth dest = broadcast
    frame[6..12].copy_from_slice(&mac);                     // eth src  = our MAC
    frame[12] = 0x08; frame[13] = 0x06;                     // ethertype = ARP
    frame[14] = 0x00; frame[15] = 0x01;                     // htype = Ethernet
    frame[16] = 0x08; frame[17] = 0x00;                     // ptype = IPv4
    frame[18] = 0x06; frame[19] = 0x04;                     // hlen 6, plen 4
    frame[20] = 0x00; frame[21] = 0x01;                     // oper = request
    frame[22..28].copy_from_slice(&mac);                    // sender hw = our MAC
    frame[28] = 10; frame[29] = 0; frame[30] = 2; frame[31] = 15;  // sender ip = 10.0.2.15
    // target hw (32..38) = 0 (unknown)
    frame[38] = 10; frame[39] = 0; frame[40] = 2; frame[41] = 2;   // target ip = 10.0.2.2 (gateway)
    let frame_len = 42;
    for (i, &b) in frame.iter().take(frame_len).enumerate() { arena.write8(TX_BUF_OFF + i, b); }

    arena.write64(TX_RING_OFF, arena.phys_at(TX_BUF_OFF));
    arena.write16(TX_RING_OFF + 8, frame_len as u16);
    arena.write8(TX_RING_OFF + 11, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
    let tx_ring_phys = arena.phys_at(TX_RING_OFF);
    mmio.write32(REG_TDBAL, (tx_ring_phys & 0xffff_ffff) as u32);
    mmio.write32(REG_TDBAH, (tx_ring_phys >> 32) as u32);
    mmio.write32(REG_TDLEN, TX_RING_BYTES);
    mmio.write32(REG_TDH, 0);
    mmio.write32(REG_TDT, 0);
    mmio.write32(REG_TIPG, TIPG_VALUE);
    mmio.write32(REG_TCTL, TCTL_VALUE);
    mmio.write32(REG_TDT, 1); // hand descriptor 0 to the NIC

    let mut spins = 0u32;
    while spins < TX_POLL_MAX && arena.read8(TX_RING_OFF + 12) & TXD_STA_DD == 0 {
        ctx.yield_cpu();
        spins += 1;
    }
    if spins < TX_POLL_MAX {
        ctx.log_fmt(format_args!("nic-driver: TX ok - {}-byte ARP request on the wire (DD set)", frame_len));
    } else {
        ctx.log("nic-driver: TX did not complete (DD never set) - reporting loudly");
    }

    // --- Wait on the TRUTH of a received frame: poll RX descriptor 0's DD bit, bounded + loud.
    let mut spins = 0u32;
    let mut got = false;
    while spins < RX_POLL_MAX {
        if arena.read8(RX_RING_OFF + 12) & RXD_STA_DD != 0 { got = true; break; }
        ctx.yield_cpu();
        spins += 1;
    }
    if got {
        let len  = arena.read16(RX_RING_OFF + 8);
        let etype = ((arena.read8(RX_BUF_OFF + 12) as u16) << 8) | arena.read8(RX_BUF_OFF + 13) as u16;
        arena.write8(RX_RING_OFF + 12, 0);        // clear DD
        mmio.write32(REG_RDT, 0);                  // hand the buffer back to the NIC
        ctx.log_fmt(format_args!(
            "nic-driver: RX - got a {}-byte frame (ethertype {:#06x})", len, etype));
    } else {
        ctx.log("nic-driver: RX - no frame within budget - reporting loudly");
    }

    // Bring-up + a full TX/RX round-trip done. Quiesce the receiver so an idle nic-driver is not
    // DMAing background traffic into a full ring (harmless, but it burns QEMU emulation cycles under
    // TCG). A real CONTINUOUS RX loop - re-arming buffers, the receive IRQ, and the frame interface
    // to net-stack - comes next (docs/networking.md Phase 1).
    mmio.write32(REG_RCTL, 0);
    idle(&ctx);
}

/// Idle forever, draining our endpoint so a flood cannot sit at 16/16 (Commandment II).
fn idle(ctx: &ServiceContext) -> ! {
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
