//! `xhci` — USB host-controller driver (§12). Stage 3: run the controller and
//! enumerate the device on a connected port — reset the port, Enable Slot,
//! Address Device. All hardware access is via the SDK's audited Mmio / Dma
//! wrappers (§18); no `unsafe` here.

#![no_std]
#![no_main]

use godspeed_sdk::{Dma, Mmio, ServiceContext};

// Capability registers (BAR + 0).
const CAP_CAPLEN_VERSION: usize = 0x00;
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_HCCPARAMS1: usize = 0x10;
const CAP_DBOFF: usize = 0x14;
const CAP_RTSOFF: usize = 0x18;

// Operational registers (BAR + CAPLENGTH).
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTSC_BASE: usize = 0x400; // PORTSC[n] = base + n*0x10

const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_RW1C: u32 = 0x00FE_0000; // change bits 17..23 (write 0 to preserve)

// DMA arena layout (64 KiB).
const DCBAA_OFF: usize = 0x0000;
const CMD_RING_OFF: usize = 0x1000;
const EVENT_RING_OFF: usize = 0x2000;
const ERST_OFF: usize = 0x3000;
const INPUT_CTX_OFF: usize = 0x6000;
const DEVICE_CTX_OFF: usize = 0x7000;
const EP0_TR_OFF: usize = 0x8000;

const EVENT_RING_TRBS: usize = 16;
const TRB_SIZE: usize = 16;

const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

const DATA_BUF_OFF: usize = 0x9000; // control-transfer data buffer (page 9)
const CONFIG_BUF_OFF: usize = 0xA000; // config-descriptor buffer (page 10)

fn spin<F: Fn() -> bool>(cond: F) {
    let mut n = 0u32;
    while !cond() && n < 5_000_000 {
        n += 1;
    }
}

fn idle(ctx: &ServiceContext) -> ! {
    loop {
        ctx.yield_cpu();
    }
}

/// Poll the event ring for the next event TRB. Returns (trb_type, completion,
/// slot_id) and advances the dequeue pointer, or None on timeout.
fn next_event(
    dma: &Dma,
    mmio: &Mmio,
    ir0: usize,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
) -> Option<(u32, u32, u32)> {
    let mut tries = 0u32;
    while tries < 10_000_000 {
        tries += 1;
        let off = EVENT_RING_OFF + *ev_idx * TRB_SIZE;
        let ctrl = dma.read32(off + 12);
        if (ctrl & 1) != *ev_cycle {
            continue;
        }
        let trb_type = (ctrl >> 10) & 0x3F;
        let completion = dma.read32(off + 8) >> 24;
        let slot_id = (ctrl >> 24) & 0xFF;
        *ev_idx += 1;
        if *ev_idx == EVENT_RING_TRBS {
            *ev_idx = 0;
            *ev_cycle ^= 1;
        }
        mmio.write64(ir0 + 0x18, dma.phys_at(EVENT_RING_OFF + *ev_idx * TRB_SIZE) | (1 << 3));
        return Some((trb_type, completion, slot_id));
    }
    None
}

/// Issue a command TRB and wait for its Command Completion Event, skipping any
/// intervening events (e.g. Port Status Change). Returns (completion, slot_id).
fn run_command(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    cmd_trb_off: usize,
    d0: u32,
    d1: u32,
    d2: u32,
    d3: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
) -> Option<(u32, u32)> {
    dma.write32(cmd_trb_off, d0);
    dma.write32(cmd_trb_off + 4, d1);
    dma.write32(cmd_trb_off + 8, d2);
    dma.write32(cmd_trb_off + 12, d3);
    mmio.write32(dboff, 0); // command doorbell

    for _ in 0..8 {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle) {
            Some((TRB_CMD_COMPLETION, completion, slot)) => return Some((completion, slot)),
            Some((TRB_PORT_STATUS_CHANGE, _, _)) => {
                ctx.log("xhci: (port status change event)");
            }
            Some((t, _, _)) => {
                ctx.log_fmt(format_args!("xhci: (event type {})", t));
            }
            None => return None,
        }
    }
    None
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("xhci: driver starting");

    let mmio = match ctx.xhci_mmio() {
        Some(m) => m,
        None => {
            ctx.log("xhci: no controller MMIO granted — idling");
            idle(&ctx);
        }
    };
    let dma = match ctx.dma_region() {
        Some(d) => d,
        None => {
            ctx.log("xhci: no DMA arena granted — idling");
            idle(&ctx);
        }
    };

    // Capability registers.
    let cap_version = mmio.read32(CAP_CAPLEN_VERSION);
    let caplen = (cap_version & 0xFF) as usize;
    let version = (cap_version >> 16) as u16;
    let hcs1 = mmio.read32(CAP_HCSPARAMS1);
    let hcc1 = mmio.read32(CAP_HCCPARAMS1);
    let max_slots = hcs1 & 0xFF;
    let max_ports = (hcs1 >> 24) & 0xFF;
    let _hcs2 = mmio.read32(CAP_HCSPARAMS2);
    let ctx_size = if hcc1 & (1 << 2) != 0 { 64 } else { 32 }; // CSZ
    let dboff = (mmio.read32(CAP_DBOFF) & !0x3) as usize;
    let rtsoff = (mmio.read32(CAP_RTSOFF) & !0x1F) as usize;
    let op = caplen;
    let ir0 = rtsoff + 0x20;

    ctx.log_fmt(format_args!(
        "xhci: v{:#06x} slots={} ports={} ctx_size={} dboff={:#x} rtsoff={:#x}",
        version, max_slots, max_ports, ctx_size, dboff, rtsoff
    ));

    // Reset.
    let cmd = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
    spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH != 0);
    mmio.write32(op + OP_USBCMD, CMD_HCRST);
    spin(|| {
        mmio.read32(op + OP_USBCMD) & CMD_HCRST == 0 && mmio.read32(op + OP_USBSTS) & STS_CNR == 0
    });

    // Build DMA structures + run.
    dma.zero();
    mmio.write64(op + OP_DCBAAP, dma.phys_at(DCBAA_OFF));
    mmio.write64(op + OP_CRCR, dma.phys_at(CMD_RING_OFF) | 1);
    dma.write64(ERST_OFF, dma.phys_at(EVENT_RING_OFF));
    dma.write32(ERST_OFF + 8, EVENT_RING_TRBS as u32);
    mmio.write32(ir0 + 0x08, 1);
    mmio.write64(ir0 + 0x10, dma.phys_at(ERST_OFF));
    mmio.write64(ir0 + 0x18, dma.phys_at(EVENT_RING_OFF));
    mmio.write32(op + OP_CONFIG, max_slots);
    let c = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, c | CMD_RS);
    spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH == 0);
    ctx.log("xhci: controller running");

    // Find a connected port.
    let mut port = 0u32;
    for p in 1..=max_ports {
        let psc = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10);
        if psc & PORT_CCS != 0 {
            port = p;
            break;
        }
    }
    if port == 0 {
        ctx.log("xhci: no connected port — idling");
        idle(&ctx);
    }
    let portsc_off = op + OP_PORTSC_BASE + (port as usize - 1) * 0x10;

    // Reset the port: set PR (preserving non-change bits), wait for enable.
    let psc = mmio.read32(portsc_off);
    mmio.write32(portsc_off, (psc & !PORT_RW1C) | PORT_PR);
    spin(|| mmio.read32(portsc_off) & PORT_PED != 0);
    let psc = mmio.read32(portsc_off);
    let speed = (psc >> 10) & 0xF;
    let max_packet: u32 = match speed {
        2 => 8,   // low-speed
        4 => 512, // super-speed
        _ => 64,  // full / high-speed
    };
    ctx.log_fmt(format_args!(
        "xhci: port {} reset; PORTSC={:#010x} speed={} max_packet={}",
        port, psc, speed, max_packet
    ));

    let mut ev_idx = 0usize;
    let mut ev_cycle = 1u32;
    let mut cmd_idx = 0usize; // command ring producer index

    // Enable Slot.
    let cmd_off = CMD_RING_OFF + cmd_idx * TRB_SIZE;
    cmd_idx += 1;
    let (comp, slot) = match run_command(
        &ctx, &dma, &mmio, dboff, ir0, cmd_off, 0, 0, 0,
        (TRB_ENABLE_SLOT << 10) | 1, &mut ev_idx, &mut ev_cycle,
    ) {
        Some(r) => r,
        None => {
            ctx.log("xhci: Enable Slot — no completion");
            idle(&ctx);
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Enable Slot failed (completion={})", comp));
        idle(&ctx);
    }
    ctx.log_fmt(format_args!("xhci: slot {} enabled", slot));

    // Build the Input Context for Address Device.
    //   +0:            Input Control Context — Add flags A0(slot)|A1(ep0)
    //   +ctx_size:     Slot Context
    //   +2*ctx_size:   Endpoint 0 Context
    let ictl = INPUT_CTX_OFF;
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    dma.write32(ictl + 4, 0b11); // Add Context flags: slot + EP0

    // Slot Context: Context Entries=1 [31:27], Speed [23:20]; Root Hub Port [23:16].
    dma.write32(islot, (1 << 27) | (speed << 20));
    dma.write32(islot + 4, port << 16);

    // EP0 Context (Control): CErr=3 [2:1], EP Type=4 [5:3], Max Packet [31:16];
    // TR Dequeue Ptr | DCS; Average TRB Length.
    let ep0_tr = dma.phys_at(EP0_TR_OFF);
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);

    // DCBAA[slot] = device context physical base.
    dma.write64(DCBAA_OFF + slot as usize * 8, dma.phys_at(DEVICE_CTX_OFF));

    // Address Device command (input context ptr, slot id).
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + cmd_idx * TRB_SIZE;
    let (comp, _) = match run_command(
        &ctx, &dma, &mmio, dboff, ir0, cmd_off,
        in_phys as u32, (in_phys >> 32) as u32, 0,
        (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1, &mut ev_idx, &mut ev_cycle,
    ) {
        Some(r) => r,
        None => {
            ctx.log("xhci: Address Device — no completion");
            idle(&ctx);
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Address Device failed (completion={})", comp));
        idle(&ctx);
    }
    ctx.log_fmt(format_args!(
        "xhci: Address Device OK — device on port {} addressed (slot {})",
        port, slot
    ));

    // --- Stage 3c: Get Device Descriptor over EP0 (control transfer) ---
    // Three TRBs on the EP0 transfer ring: Setup (immediate GET_DESCRIPTOR),
    // Data (IN, 18 bytes), Status (OUT, IOC). Then ring the slot's EP0 doorbell.
    let data_phys = dma.phys_at(DATA_BUF_OFF);
    dma.write32(EP0_TR_OFF, 0x80 | (6 << 8) | (0x0100 << 16)); // bmReqType|bRequest|wValue
    dma.write32(EP0_TR_OFF + 4, 18 << 16); // wIndex=0 | wLength=18
    dma.write32(EP0_TR_OFF + 8, 8); // setup data length
    dma.write32(EP0_TR_OFF + 12, 1 | (1 << 6) | (TRB_SETUP_STAGE << 10) | (3 << 16)); // cyc|IDT|type|TRT=IN
    dma.write32(EP0_TR_OFF + 16, data_phys as u32);
    dma.write32(EP0_TR_OFF + 20, (data_phys >> 32) as u32);
    dma.write32(EP0_TR_OFF + 24, 18);
    dma.write32(EP0_TR_OFF + 28, 1 | (TRB_DATA_STAGE << 10) | (1 << 16)); // cyc|type|DIR=IN
    dma.write32(EP0_TR_OFF + 32, 0);
    dma.write32(EP0_TR_OFF + 36, 0);
    dma.write32(EP0_TR_OFF + 40, 0);
    dma.write32(EP0_TR_OFF + 44, 1 | (1 << 5) | (TRB_STATUS_STAGE << 10)); // cyc|IOC|type
    mmio.write32(dboff + slot as usize * 4, 1); // EP0 doorbell (DCI 1)

    let mut ok = false;
    for _ in 0..8 {
        match next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle) {
            Some((TRB_TRANSFER_EVENT, c, _)) => {
                ctx.log_fmt(format_args!("xhci: control transfer completion={}", c));
                ok = c == 1 || c == 13; // success or short-packet
                break;
            }
            Some((t, _, _)) => ctx.log_fmt(format_args!("xhci: (event type {})", t)),
            None => break,
        }
    }
    if !ok {
        ctx.log("xhci: Get Device Descriptor failed");
        idle(&ctx);
    }
    let d0 = dma.read32(DATA_BUF_OFF);
    let ids = dma.read32(DATA_BUF_OFF + 8);
    ctx.log_fmt(format_args!(
        "xhci: DEVICE DESCRIPTOR bLength={} type={} VID={:#06x} PID={:#06x}",
        d0 & 0xFF,
        (d0 >> 8) & 0xFF,
        ids & 0xFFFF,
        (ids >> 16) & 0xFFFF
    ));

    // --- Stage 4a: Get Configuration Descriptor; find the interrupt-IN endpoint ---
    let cfg_phys = dma.phys_at(CONFIG_BUF_OFF);
    let tr = EP0_TR_OFF + 48; // next 3 TRBs on the EP0 transfer ring
    dma.write32(tr, 0x80 | (6 << 8) | (0x0200 << 16)); // GET_DESCRIPTOR Config(2), idx 0
    dma.write32(tr + 4, 64 << 16); // wIndex=0 | wLength=64
    dma.write32(tr + 8, 8);
    dma.write32(tr + 12, 1 | (1 << 6) | (TRB_SETUP_STAGE << 10) | (3 << 16));
    dma.write32(tr + 16, cfg_phys as u32);
    dma.write32(tr + 20, (cfg_phys >> 32) as u32);
    dma.write32(tr + 24, 64);
    dma.write32(tr + 28, 1 | (TRB_DATA_STAGE << 10) | (1 << 16));
    dma.write32(tr + 32, 0);
    dma.write32(tr + 36, 0);
    dma.write32(tr + 40, 0);
    dma.write32(tr + 44, 1 | (1 << 5) | (TRB_STATUS_STAGE << 10));
    mmio.write32(dboff + slot as usize * 4, 1);
    let mut cfg_ok = false;
    for _ in 0..8 {
        match next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle) {
            Some((TRB_TRANSFER_EVENT, c, _)) => {
                cfg_ok = c == 1 || c == 13;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    if !cfg_ok {
        ctx.log("xhci: Get Config Descriptor failed");
        idle(&ctx);
    }

    // Walk the descriptors: config (bConfigurationValue), interface (HID protocol),
    // endpoint (the interrupt-IN endpoint we'll poll for key reports).
    let total = ((dma.read32(CONFIG_BUF_OFF) >> 16) & 0xFFFF) as usize;
    let mut i = 0usize;
    let mut ep_addr = 0u8;
    let mut ep_mps = 0u16;
    let mut ep_interval = 0u8;
    let mut cfg_val = 0u8;
    let mut hid_proto = 0u8;
    while i + 2 <= total && i < 200 {
        let blen = dma.read8(CONFIG_BUF_OFF + i) as usize;
        let dtype = dma.read8(CONFIG_BUF_OFF + i + 1);
        if blen == 0 {
            break;
        }
        match dtype {
            2 => cfg_val = dma.read8(CONFIG_BUF_OFF + i + 5),
            4 => hid_proto = dma.read8(CONFIG_BUF_OFF + i + 7),
            5 => {
                let addr = dma.read8(CONFIG_BUF_OFF + i + 2);
                let attr = dma.read8(CONFIG_BUF_OFF + i + 3);
                if attr & 0x3 == 0x3 && addr & 0x80 != 0 {
                    ep_addr = addr;
                    ep_mps = dma.read16(CONFIG_BUF_OFF + i + 4);
                    ep_interval = dma.read8(CONFIG_BUF_OFF + i + 6);
                }
            }
            _ => {}
        }
        i += blen;
    }
    if ep_addr != 0 {
        let ep_num = (ep_addr & 0x0F) as u32;
        let dci = ep_num * 2 + 1; // interrupt-IN endpoint
        ctx.log_fmt(format_args!(
            "xhci: HID int-IN endpoint {} (DCI {}) mps={} interval={} cfg_val={} proto={}",
            ep_num, dci, ep_mps, ep_interval, cfg_val, hid_proto
        ));
    } else {
        ctx.log("xhci: no interrupt-IN endpoint found");
    }

    idle(&ctx);
}
