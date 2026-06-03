//! `xhci` — USB host-controller driver (§12). Stage 3: build the command +
//! event rings and the device-context array in DMA memory, run the controller,
//! and issue an Enable Slot command — proving the full command-ring → doorbell
//! → event-ring round trip. No `unsafe` here: all hardware access goes through
//! the SDK's audited Mmio / Dma wrappers (§18).

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// Capability registers (BAR + 0).
const CAP_CAPLEN_VERSION: usize = 0x00; // CAPLENGTH[7:0], HCIVERSION[31:16]
const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_DBOFF: usize = 0x14; // doorbell array offset
const CAP_RTSOFF: usize = 0x18; // runtime register space offset

// Operational registers (BAR + CAPLENGTH).
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18; // command ring control (64-bit)
const OP_DCBAAP: usize = 0x30; // device-context base array ptr (64-bit)
const OP_CONFIG: usize = 0x38;

const CMD_RS: u32 = 1 << 0;
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

// DMA arena layout (within the 64 KiB granted region).
const DCBAA_OFF: usize = 0x0000; // device-context base array (page 0)
const CMD_RING_OFF: usize = 0x1000; // command ring (page 1)
const EVENT_RING_OFF: usize = 0x2000; // event ring (page 2)
const ERST_OFF: usize = 0x3000; // event-ring segment table (page 3)
const SCRATCH_ARR_OFF: usize = 0x4000; // scratchpad buffer array (page 4)
const SCRATCH_BUF_OFF: usize = 0x5000; // scratchpad buffers (pages 5..)
const MAX_SCRATCH_PAGES: usize = 11; // pages 5..16 of the arena

const EVENT_RING_TRBS: usize = 16;
const TRB_SIZE: usize = 16;

const TRB_ENABLE_SLOT: u32 = 9;
const TRB_CMD_COMPLETION: u32 = 33;

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

    // --- Capability registers ---
    let cap_version = mmio.read32(CAP_CAPLEN_VERSION);
    let caplen = (cap_version & 0xFF) as usize;
    let version = (cap_version >> 16) as u16;
    let hcs1 = mmio.read32(CAP_HCSPARAMS1);
    let hcs2 = mmio.read32(CAP_HCSPARAMS2);
    let max_slots = hcs1 & 0xFF;
    let max_ports = (hcs1 >> 24) & 0xFF;
    let max_scratch = ((((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F)) as usize;
    let dboff = (mmio.read32(CAP_DBOFF) & !0x3) as usize;
    let rtsoff = (mmio.read32(CAP_RTSOFF) & !0x1F) as usize;
    let op = caplen;
    let ir0 = rtsoff + 0x20; // interrupter 0 register set

    ctx.log_fmt(format_args!(
        "xhci: v{:#06x} slots={} ports={} scratch={} caplen={:#x} dboff={:#x} rtsoff={:#x}",
        version, max_slots, max_ports, max_scratch, caplen, dboff, rtsoff
    ));

    // --- Reset: halt, then HCRST, wait for ready ---
    let cmd = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
    spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH != 0);
    mmio.write32(op + OP_USBCMD, CMD_HCRST);
    spin(|| {
        mmio.read32(op + OP_USBCMD) & CMD_HCRST == 0 && mmio.read32(op + OP_USBSTS) & STS_CNR == 0
    });
    ctx.log("xhci: controller reset");

    // --- Build DMA structures ---
    dma.zero();

    // Scratchpad: the controller may require N scratchpad pages. DCBAA[0] points
    // to an array of their physical addresses.
    let scratch = max_scratch.min(MAX_SCRATCH_PAGES);
    if scratch < max_scratch {
        ctx.log("xhci: WARNING — scratchpad need exceeds arena; capping");
    }
    if scratch > 0 {
        for i in 0..scratch {
            let buf = dma.phys_at(SCRATCH_BUF_OFF + i * 0x1000);
            dma.write64(SCRATCH_ARR_OFF + i * 8, buf);
        }
        dma.write64(DCBAA_OFF, dma.phys_at(SCRATCH_ARR_OFF));
    }

    // DCBAAP = device-context base array physical base.
    mmio.write64(op + OP_DCBAAP, dma.phys_at(DCBAA_OFF));

    // Command ring: CRCR = ring phys | RCS (cycle = 1).
    mmio.write64(op + OP_CRCR, dma.phys_at(CMD_RING_OFF) | 1);

    // Event ring segment table: one segment of EVENT_RING_TRBS entries.
    dma.write64(ERST_OFF, dma.phys_at(EVENT_RING_OFF)); // ring segment base
    dma.write32(ERST_OFF + 8, EVENT_RING_TRBS as u32); // ring segment size
    mmio.write32(ir0 + 0x08, 1); // ERSTSZ = 1 segment
    mmio.write64(ir0 + 0x10, dma.phys_at(ERST_OFF)); // ERSTBA
    mmio.write64(ir0 + 0x18, dma.phys_at(EVENT_RING_OFF)); // ERDP

    // CONFIG.MaxSlotsEn
    mmio.write32(op + OP_CONFIG, max_slots);

    // --- Run ---
    let c = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, c | CMD_RS);
    spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH == 0);
    ctx.log_fmt(format_args!(
        "xhci: running (USBSTS={:#x})",
        mmio.read32(op + OP_USBSTS)
    ));

    // --- Issue Enable Slot on the command ring (TRB 0), ring doorbell 0 ---
    dma.write32(CMD_RING_OFF, 0);
    dma.write32(CMD_RING_OFF + 4, 0);
    dma.write32(CMD_RING_OFF + 8, 0);
    dma.write32(CMD_RING_OFF + 12, (TRB_ENABLE_SLOT << 10) | 1); // type + cycle
    mmio.write32(dboff, 0); // doorbell 0, target 0 = command ring

    // --- Poll the event ring for the Command Completion Event ---
    let mut ev_idx = 0usize;
    let mut ev_cycle = 1u32;
    let mut found = false;
    let mut tries = 0u32;
    while tries < 10_000_000 && !found {
        tries += 1;
        let off = EVENT_RING_OFF + ev_idx * TRB_SIZE;
        let ctrl = dma.read32(off + 12);
        if (ctrl & 1) != ev_cycle {
            continue; // no new event yet
        }
        let trb_type = (ctrl >> 10) & 0x3F;
        let completion = dma.read32(off + 8) >> 24;
        let slot_id = (ctrl >> 24) & 0xFF;
        ctx.log_fmt(format_args!(
            "xhci: event type={} completion={} slot={}",
            trb_type, completion, slot_id
        ));
        if trb_type == TRB_CMD_COMPLETION {
            if completion == 1 {
                ctx.log_fmt(format_args!(
                    "xhci: Enable Slot OK — slot {} assigned; command ring works!",
                    slot_id
                ));
            } else {
                ctx.log_fmt(format_args!(
                    "xhci: Enable Slot completion={} (not success)",
                    completion
                ));
            }
            found = true;
        }
        // Advance the event-ring dequeue pointer (wrap + flip cycle at the end).
        ev_idx += 1;
        if ev_idx == EVENT_RING_TRBS {
            ev_idx = 0;
            ev_cycle ^= 1;
        }
        mmio.write64(ir0 + 0x18, dma.phys_at(EVENT_RING_OFF + ev_idx * TRB_SIZE) | (1 << 3));
    }
    if !found {
        ctx.log("xhci: no command-completion event (ring/doorbell issue?)");
    }

    idle(&ctx);
}
