// SPDX-License-Identifier: Apache-2.0
//! e1000 - a real, runnable userspace driver for the Intel 82540EM ("e1000") NIC.
//!
//! This is the runnable counterpart to `examples/driver-skeleton`. It is a SERVICE
//! (Commandment I): the kernel grants it only the NIC's MMIO window - by name, in
//! `kernel/src/task/mod.rs`, and only when the discovered NIC is actually an Intel
//! e1000 - and all device logic lives here. It writes NO `unsafe`: every register
//! read goes through the SDK's safe `Mmio` wrapper (Commandment X / §18.1).
//!
//! Read-only: it reports the link state and the MAC address the NIC loaded from its
//! EEPROM. A full driver would also build TX/RX descriptor rings in a DMA arena and
//! handle the NIC's interrupt - see `examples/driver-skeleton` for that shape.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Intel 82540EM register offsets (byte offsets into the BAR0 MMIO window).
const REG_STATUS: usize = 0x0008; // Device Status; bit 1 (LU) = Link Up
const REG_RAL0:   usize = 0x5400; // Receive Address Low 0  (MAC bytes 0..4, EEPROM-loaded)
const REG_RAH0:   usize = 0x5404; // Receive Address High 0 (MAC bytes 4..6 in bits [15:0])

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("e1000: starting");

    // The kernel mapped our BAR only if the discovered NIC is a real Intel e1000
    // (Commandment VII: hardware reach is an explicit, kernel-granted capability,
    // for exactly the device we were written for). On any other NIC there is no
    // mapping, so we DEGRADE rather than crash (Commandment V: no service is special).
    let mmio = match ctx.mmio() {
        Some(m) => m,
        None => {
            ctx.log("e1000: no Intel e1000 mapped (absent, or a different NIC) - idling");
            // Drain our endpoint so a flood cannot sit at 16/16 forever, then yield.
            loop {
                while ctx.try_recv().is_some() {}
                ctx.yield_cpu();
            }
        }
    };

    // Read-only bring-up: report link + MAC. Reads go through the safe SDK `Mmio`
    // wrapper, so this driver is `unsafe`-free (Commandment X / §18.1).
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
        "e1000: link {}  MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if link_up { "UP" } else { "down" },
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]));

    // This read-only example has reported what it found. A real driver would now
    // serve a network stack over IPC and re-init the controller on every restart
    // (Commandments V + IX). Idle, draining the endpoint.
    loop {
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}
