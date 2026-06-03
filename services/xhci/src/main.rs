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

    // Stage 2b: verify the DMA arena — write sentinels, read them back, and
    // report the physical base the controller will be programmed with.
    match ctx.dma_region() {
        Some(dma) => {
            dma.zero();
            dma.write32(0, 0xCAFE_F00D);
            dma.write64(8, 0x1122_3344_5566_7788);
            let r0 = dma.read32(0);
            let r1 = dma.read64(8);
            ctx.log_fmt(format_args!(
                "xhci: DMA arena phys={:#x} len={} bytes; readback {:#010x} {:#018x}",
                dma.phys_base(),
                dma.len(),
                r0,
                r1
            ));
            if r0 == 0xCAFE_F00D && r1 == 0x1122_3344_5566_7788 {
                ctx.log("xhci: DMA arena verified — writable + readable, phys known");
            } else {
                ctx.log("xhci: WARNING — DMA readback mismatch");
            }
        }
        None => ctx.log("xhci: no DMA arena granted"),
    }

    // Stage 2b: reset the controller. Operational registers begin at BAR +
    // CAPLENGTH. Halt (clear Run/Stop, wait HCHalted), then set HCRST and wait
    // for it to self-clear and CNR (Controller-Not-Ready) to clear.
    let op = caplen as usize;
    const USBCMD: usize = 0x00;
    const USBSTS: usize = 0x04;
    const CMD_RS: u32 = 1 << 0;
    const CMD_HCRST: u32 = 1 << 1;
    const STS_HCH: u32 = 1 << 0;
    const STS_CNR: u32 = 1 << 11;

    let cmd = mmio.read32(op + USBCMD);
    mmio.write32(op + USBCMD, cmd & !CMD_RS);
    let mut spins = 0u32;
    while mmio.read32(op + USBSTS) & STS_HCH == 0 && spins < 2_000_000 {
        spins += 1;
    }

    mmio.write32(op + USBCMD, CMD_HCRST);
    spins = 0;
    while (mmio.read32(op + USBCMD) & CMD_HCRST != 0
        || mmio.read32(op + USBSTS) & STS_CNR != 0)
        && spins < 5_000_000
    {
        spins += 1;
    }

    let usbcmd = mmio.read32(op + USBCMD);
    let usbsts = mmio.read32(op + USBSTS);
    ctx.log_fmt(format_args!(
        "xhci: after reset USBCMD={:#x} USBSTS={:#x} (op@{:#x})",
        usbcmd, usbsts, op
    ));
    if usbcmd & CMD_HCRST == 0 && usbsts & STS_CNR == 0 && usbsts & STS_HCH != 0 {
        ctx.log("xhci: controller reset complete — halted + ready for config");
    } else {
        ctx.log("xhci: WARNING — controller not ready after reset");
    }

    // Stage 2b: scan the root-hub ports for connected devices. PORTSC registers
    // begin at op + 0x400, 0x10 bytes apart; bit 0 (CCS) = Current Connect Status.
    let mut connected = 0u32;
    for port in 0..max_ports {
        let portsc = mmio.read32(op + 0x400 + (port as usize) * 0x10);
        if portsc & 1 != 0 {
            connected += 1;
            ctx.log_fmt(format_args!(
                "xhci: port {} CONNECTED (PORTSC={:#010x})",
                port + 1,
                portsc
            ));
        }
    }
    if connected == 0 {
        ctx.log("xhci: no devices on any root port");
    } else {
        ctx.log_fmt(format_args!("xhci: {} device(s) connected", connected));
    }

    loop {
        ctx.yield_cpu();
    }
}
