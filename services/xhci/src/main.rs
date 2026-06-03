//! `xhci` — USB host-controller driver (§12). Stage 2 milestone: prove that the
//! kernel maps the controller's MMIO BAR into this userspace driver by reading
//! the xHCI capability registers. No `unsafe` here — all hardware access goes
//! through the SDK's audited `Mmio` wrapper (§18).

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// xHCI Host Controller Capability Registers (at the start of the BAR). The
// first dword packs CAPLENGTH[7:0] and HCIVERSION[31:16]; read it as one u32.
const CAPLENGTH_VERSION: usize = 0x00; // u32: CAPLENGTH[7:0], HCIVERSION[31:16]
const HCSPARAMS1: usize = 0x04; // u32 — MaxSlots[7:0] MaxIntrs[18:8] MaxPorts[31:24]
const HCCPARAMS1: usize = 0x10; // u32 — capability parameters

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("xhci: driver starting");

    let mmio = match ctx.xhci_mmio() {
        Some(m) => m,
        None => {
            ctx.log("xhci: no controller MMIO granted — idling");
            loop {
                ctx.yield_cpu();
            }
        }
    };

    let cap_version = mmio.read32(CAPLENGTH_VERSION);
    let caplen = (cap_version & 0xFF) as u8;
    let version = (cap_version >> 16) as u16;
    let hcs1 = mmio.read32(HCSPARAMS1);
    let hcc1 = mmio.read32(HCCPARAMS1);

    let max_slots = hcs1 & 0xFF;
    let max_ports = (hcs1 >> 24) & 0xFF;

    ctx.log_fmt(format_args!(
        "xhci: CAPLENGTH={:#x} HCIVERSION={:#06x} MaxSlots={} MaxPorts={} HCCPARAMS1={:#010x}",
        caplen, version, max_slots, max_ports, hcc1
    ));

    // A plausible version (0x0100/0x0110/0x0120) and non-zero port count means
    // the BAR is correctly mapped and readable from ring 3 — Stage 2a proven.
    if version >= 0x0100 && max_ports > 0 {
        ctx.log("xhci: MMIO mapping verified — controller registers readable");
    } else {
        ctx.log("xhci: WARNING — implausible capability registers (mapping?)");
    }

    loop {
        ctx.yield_cpu();
    }
}
