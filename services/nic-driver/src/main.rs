// SPDX-License-Identifier: GPL-2.0-only
//! nic-driver - the userspace NIC driver service (docs/networking.md, Phase 1).
//!
//! Model-specific driver for the Intel 82540EM ("e1000"), the QEMU dev NIC. It is an
//! ordinary restartable, IOMMU-confinable userspace service (Commandment I): the kernel
//! grants it only the NIC's MMIO BAR + a DMA arena, by name, and only when the discovered
//! NIC is a real Intel e1000; all device logic lives here, `unsafe`-free behind the SDK
//! `Mmio`/`Dma` wrappers (§18.1). The T630's Realtek chipset is a separate Phase-4 driver
//! behind the same (future) NIC-agnostic frame interface, so `net-stack` never learns the
//! difference.
//!
//! Phase 1 progress:
//!  - step 2: reset the controller + read the link state and the MAC (from EEPROM).
//!  - step 3 (this commit): set up a TX descriptor ring in the DMA arena and transmit one
//!    raw Ethernet frame, waiting on the TRUTH of the NIC's DD (descriptor-done) bit. That
//!    proves the DMA path - arena -> descriptor ring -> the card puts bytes on the wire.
//!    RX rings, the receive IRQ, and the frame interface to `net-stack` follow.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Intel 82540EM register offsets (byte offsets into the BAR0 MMIO window).
const REG_CTRL:   usize = 0x0000; // Device Control
const REG_STATUS: usize = 0x0008; // Device Status; bit 1 (LU) = Link Up
const REG_TCTL:   usize = 0x0400; // Transmit Control
const REG_TIPG:   usize = 0x0410; // Transmit Inter-Packet Gap
const REG_RAL0:   usize = 0x5400; // Receive Address Low 0  (MAC bytes 0..4, EEPROM-loaded)
const REG_RAH0:   usize = 0x5404; // Receive Address High 0 (MAC bytes 4..6 in bits [15:0])
const REG_TDBAL:  usize = 0x3800; // TX Descriptor Base Low
const REG_TDBAH:  usize = 0x3804; // TX Descriptor Base High
const REG_TDLEN:  usize = 0x3808; // TX Descriptor ring Length (bytes)
const REG_TDH:    usize = 0x3810; // TX Descriptor Head
const REG_TDT:    usize = 0x3818; // TX Descriptor Tail

const CTRL_RST: u32 = 1 << 26; // CTRL.RST - global device reset; the NIC self-clears it when done

// TCTL: EN (enable) | PSP (pad short packets) | CT=0x0F (collision threshold) | COLD=0x40
// (collision distance, full-duplex). TIPG: IPGT=10, IPGR1=8, IPGR2=6 (82540EM copper).
const TCTL_VALUE: u32 = (1 << 1) | (1 << 3) | (0x0F << 4) | (0x40 << 12);
const TIPG_VALUE: u32 = 10 | (8 << 10) | (6 << 20);

// Legacy TX descriptor (16 bytes): addr(u64)@0, length(u16)@8, cso(u8)@10, cmd(u8)@11,
// status(u8)@12, css(u8)@13, special(u16)@14.
const TXD_CMD_EOP:  u8 = 1 << 0; // end of packet
const TXD_CMD_IFCS: u8 = 1 << 1; // insert FCS (the NIC appends the CRC)
const TXD_CMD_RS:   u8 = 1 << 3; // report status -> the NIC sets DD when the frame is sent
const TXD_STA_DD:   u8 = 1 << 0; // descriptor done

// DMA-arena layout: an 8-descriptor TX ring (128 B, so TDLEN is 128-byte aligned) at offset
// 0, and a page-aligned frame buffer at 0x1000. Both live in our granted DMA arena.
const TX_RING_OFF:   usize = 0;
const TX_RING_BYTES: u32   = 8 * 16;
const TX_BUF_OFF:    usize = 0x1000;

// Bounded budget for hardware self-clear / done polls. These are HARDWARE-timing bounds (the
// exempt category, like AHCI/USB spins - NOT the correctness-by-time Commandment VIII forbids):
// we wait on the TRUTH of a bit, and give up LOUDLY rather than wedge the core (§26.6, §26.7).
const RESET_POLL_MAX: u32 = 1_000_000;
const TX_POLL_MAX:    u32 = 1_000_000;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("nic-driver: starting (e1000)");

    // The kernel mapped our BAR only if the discovered NIC is a real Intel e1000
    // (Commandment VII: hardware reach is explicit, for exactly the device we drive). On any
    // other NIC - the T630's Realtek, or none at all - there is no mapping, so we DEGRADE
    // rather than crash (Commandment V: no service is special).
    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => {
            ctx.log("nic-driver: no Intel e1000 mapped (absent, or a different NIC) - idling");
            idle(&ctx);
        }
    };

    // Reset the controller to a known state. Bring-up runs on EVERY spawn, including a restart
    // (Commandments V + IX: never assume the device kept its state). Set CTRL.RST, then wait on
    // the TRUTH of the bit self-clearing - bounded, and loud if it never does.
    mmio.write32(REG_CTRL, mmio.read32(REG_CTRL) | CTRL_RST);
    let mut cleared = false;
    let mut spins = 0u32;
    while spins < RESET_POLL_MAX {
        if mmio.read32(REG_CTRL) & CTRL_RST == 0 {
            cleared = true;
            break;
        }
        ctx.yield_cpu(); // only conserves CPU; the bit, not the delay, decides readiness
        spins += 1;
    }
    if !cleared {
        ctx.log("nic-driver: e1000 reset did not self-clear (RST stuck) - reporting best-effort");
    }

    // Read the link state + the MAC the NIC (re)loaded from its EEPROM. Through the safe SDK
    // `Mmio` wrapper, so this driver is `unsafe`-free (Commandment X / §18.1).
    let status  = mmio.read32(REG_STATUS);
    let link_up = (status >> 1) & 1 == 1;
    let ral = mmio.read32(REG_RAL0);
    let rah = mmio.read32(REG_RAH0);
    let mac = [
        (ral         & 0xff) as u8, ((ral >> 8)  & 0xff) as u8,
        ((ral >> 16) & 0xff) as u8, ((ral >> 24) & 0xff) as u8,
        (rah         & 0xff) as u8, ((rah >> 8)  & 0xff) as u8,
    ];
    ctx.log_fmt(format_args!(
        "nic-driver: e1000 up  link {}  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if link_up { "UP" } else { "down" },
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));

    // --- Phase 1 step 3: transmit one raw Ethernet frame via a TX descriptor ring ---
    // Our DMA arena holds the ring + a frame buffer; the NIC DMAs the frame out of it. No
    // arena -> we cannot DMA, so degrade (Commandment V).
    let arena = match ctx.dma_region() {
        Some(a) => a,
        None => {
            ctx.log("nic-driver: no DMA arena granted - cannot set up TX, idling");
            idle(&ctx);
        }
    };
    arena.zero();

    // Build the frame: broadcast destination, our MAC as source, a local-experimental
    // ethertype (0x88B5), and an identifiable ASCII payload a host frame-dump can spot.
    let mut frame = [0u8; 64];
    for b in frame.iter_mut().take(6) { *b = 0xff; }   // dest = broadcast
    frame[6..12].copy_from_slice(&mac);                // src = our MAC
    frame[12] = 0x88;
    frame[13] = 0xB5;                                  // ethertype (IEEE local experimental)
    let payload = b"GODSPEED-OS e1000 TX";
    frame[14..14 + payload.len()].copy_from_slice(payload);
    let frame_len = 14 + payload.len();                // 34; the NIC pads to 60 via TCTL.PSP
    for (i, &b) in frame.iter().take(frame_len).enumerate() {
        arena.write8(TX_BUF_OFF + i, b);
    }

    // Descriptor 0 -> our frame buffer. EOP (whole frame in one buffer), IFCS (let the NIC
    // append the CRC), RS (report status via the DD bit).
    arena.write64(TX_RING_OFF, arena.phys_at(TX_BUF_OFF));
    arena.write16(TX_RING_OFF + 8, frame_len as u16);
    arena.write8(TX_RING_OFF + 11, TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
    // status byte (+12) stays 0 (zeroed above) until the NIC sets DD.

    // Program the TX engine: ring base + length, head/tail at 0, inter-packet gap, enable.
    let ring_phys = arena.phys_at(TX_RING_OFF);
    mmio.write32(REG_TDBAL, (ring_phys & 0xffff_ffff) as u32);
    mmio.write32(REG_TDBAH, (ring_phys >> 32) as u32);
    mmio.write32(REG_TDLEN, TX_RING_BYTES);
    mmio.write32(REG_TDH, 0);
    mmio.write32(REG_TDT, 0);
    mmio.write32(REG_TIPG, TIPG_VALUE);
    mmio.write32(REG_TCTL, TCTL_VALUE);

    // Hand descriptor 0 to the NIC by advancing the tail past it, then wait on the TRUTH the
    // NIC reports when the frame is on the wire (the DD bit) - bounded, and loud if it never
    // comes (Commandment VIII).
    mmio.write32(REG_TDT, 1);
    let mut sent = false;
    let mut spins = 0u32;
    while spins < TX_POLL_MAX {
        if arena.read8(TX_RING_OFF + 12) & TXD_STA_DD != 0 {
            sent = true;
            break;
        }
        ctx.yield_cpu();
        spins += 1;
    }
    if sent {
        ctx.log_fmt(format_args!(
            "nic-driver: TX ok - {}-byte frame on the wire (DD set)", frame_len));
    } else {
        ctx.log("nic-driver: TX did not complete (DD never set) - reporting loudly");
    }

    // Bring-up + first TX done. RX rings + the RX IRQ + the frame interface to net-stack come
    // next (docs/networking.md Phase 1).
    idle(&ctx);
}

/// Idle forever, draining our endpoint so a flood cannot sit at 16/16 (Commandment II).
fn idle(ctx: &ServiceContext) -> ! {
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
