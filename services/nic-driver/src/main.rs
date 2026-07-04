// SPDX-License-Identifier: GPL-2.0-only
//! nic-driver - the userspace NIC driver service (docs/networking.md, Phase 1).
//!
//! Model-specific driver for the Intel 82540EM ("e1000"), the QEMU dev NIC. It is an
//! ordinary restartable, IOMMU-confinable userspace service (Commandment I): the kernel
//! grants it only the NIC's MMIO BAR, by name, and only when the discovered NIC is a real
//! Intel e1000; all device logic lives here, `unsafe`-free behind the SDK `Mmio` wrapper
//! (§18.1). The T630's Realtek chipset is a separate Phase-4 driver behind the same
//! (future) NIC-agnostic frame interface, so `net-stack` never learns the difference.
//!
//! Phase 1, step 2 (this commit): bring the controller up - reset it, then read and report
//! the link state + the MAC it reloaded from EEPROM. That single boot line proves the whole
//! path end to end: PCI discovery -> MMIO capability -> register read/write -> device reset.
//! TX/RX descriptor rings, the receive IRQ, and the frame interface to `net-stack` follow in
//! the next steps (docs/networking.md Phase 1).

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Intel 82540EM register offsets (byte offsets into the BAR0 MMIO window).
const REG_CTRL:   usize = 0x0000; // Device Control
const REG_STATUS: usize = 0x0008; // Device Status; bit 1 (LU) = Link Up
const REG_RAL0:   usize = 0x5400; // Receive Address Low 0  (MAC bytes 0..4, EEPROM-loaded)
const REG_RAH0:   usize = 0x5404; // Receive Address High 0 (MAC bytes 4..6 in bits [15:0])

const CTRL_RST: u32 = 1 << 26; // CTRL.RST - global device reset; the NIC self-clears it when done

// Bounded budget for the reset self-clear poll. The e1000 clears RST in ~microseconds; this
// is a HARDWARE-timing bound (the exempt category, like the AHCI/USB register spins - NOT the
// correctness-by-time Commandment VIII forbids): we wait on the TRUTH of the bit clearing, and
// give up LOUDLY if the device never does rather than wedging the core forever (§26.6 bounded,
// §26.7 loud).
const RESET_POLL_MAX: u32 = 1_000_000;

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
            loop {
                while ctx.try_recv().is_some() {}
                ctx.yield_cpu();
            }
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

    // Bring-up done. TX/RX rings + the RX IRQ + the frame interface to net-stack come next
    // (docs/networking.md Phase 1). For now idle, draining the endpoint so a flood cannot sit
    // at 16/16 (Commandment II).
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
