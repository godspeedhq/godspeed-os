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

const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

const DATA_BUF_OFF: usize = 0x9000; // control-transfer data buffer (page 9)
const CONFIG_BUF_OFF: usize = 0xA000; // config-descriptor buffer (page 10)
const INT_TR_OFF: usize = 0xB000; // interrupt-endpoint transfer ring (page 11)
const REPORT_OFF: usize = 0xC000; // HID report buffer (page 12)

fn spin<F: Fn() -> bool>(cond: F) {
    let mut n = 0u32;
    while !cond() && n < 5_000_000 {
        n += 1;
    }
}

/// Wait until a port reports a *newly* connected device, then return so the caller
/// re-scans. Snapshots the ports already connected on entry (e.g. the USB boot
/// drive, which is always present and is not a HID) and only returns when a port
/// that was NOT connected becomes connected — otherwise an always-present non-HID
/// device would make the hot-plug loop spin (re-scan → not a keyboard → wait →
/// still connected → re-scan …).
fn wait_for_port(ctx: &ServiceContext, mmio: &Mmio, op: usize, max_ports: u32) {
    let connected = |p: u32| {
        mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0
    };
    let mut base = 0u32;
    for p in 1..=max_ports {
        if connected(p) { base |= 1 << p; }
    }
    loop {
        for p in 1..=max_ports {
            let c = connected(p);
            if c && base & (1 << p) == 0 {
                return; // a new device appeared on port p
            }
            if !c { base &= !(1 << p); } // a known device left; its port can be reused
        }
        ctx.yield_cpu();
    }
}

/// Print a hot-plug notice on the console, then nudge the shell to redraw its
/// prompt. The notice is asynchronous output that lands wherever the cursor was,
/// leaving the prompt scrolled up; injecting a newline into the input ring (which
/// this driver already feeds) makes the shell print a fresh `gs> `. The leading
/// "\n" starts the notice on its own line; the injected newline supplies the
/// terminating line break, so there is no blank line.
fn notify(ctx: &ServiceContext, msg: &str) {
    // Leading "\n " — the space is sacrificial: the framebuffer drops the first
    // glyph drawn on a freshly-scrolled line, so we let it eat a space, not the
    // 'U' of "USB:". (Serial is unaffected.)
    ctx.console_write("\n USB: ");
    ctx.console_write(msg);
    ctx.console_push(b'\n');
}

fn idle(ctx: &ServiceContext) -> ! {
    // Degraded terminal path (no controller / no DMA / no keyboard). Still report
    // input-ready so the shell's boot-screen auto-clear fires — boot is "done" as
    // far as the input subsystem is concerned, even if there's no usable keyboard.
    ctx.signal_input_ready();
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
        mmio.write64(
            ir0 + 0x18,
            dma.phys_at(EVENT_RING_OFF + *ev_idx * TRB_SIZE) | (1 << 3),
        );
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

/// Issue a control transfer on EP0 at `ep0_off` in the EP0 transfer ring
/// (Setup, optional IN Data, Status). `wlen == 0` means a no-data transfer.
/// Returns true on success/short-packet completion.
#[allow(clippy::too_many_arguments)]
fn control(
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    slot: u32,
    ep0_off: usize,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    bmreq: u32,
    breq: u32,
    wval: u32,
    widx: u32,
    wlen: u32,
    data_off: usize,
) -> bool {
    let tr = EP0_TR_OFF + ep0_off;
    dma.write32(tr, bmreq | (breq << 8) | (wval << 16));
    dma.write32(tr + 4, widx | (wlen << 16));
    dma.write32(tr + 8, 8);
    let trt = if wlen > 0 { 3 } else { 0 }; // 3 = IN data stage, 0 = no data
    dma.write32(
        tr + 12,
        1 | (1 << 6) | (TRB_SETUP_STAGE << 10) | (trt << 16),
    );
    let mut off = tr + 16;
    if wlen > 0 {
        let dp = dma.phys_at(data_off);
        dma.write32(off, dp as u32);
        dma.write32(off + 4, (dp >> 32) as u32);
        dma.write32(off + 8, wlen);
        dma.write32(off + 12, 1 | (TRB_DATA_STAGE << 10) | (1 << 16)); // DIR=IN
        off += 16;
    }
    let sdir = if wlen > 0 { 0 } else { 1 }; // status dir opposite of data; no-data → IN
    dma.write32(off, 0);
    dma.write32(off + 4, 0);
    dma.write32(off + 8, 0);
    dma.write32(
        off + 12,
        1 | (1 << 5) | (TRB_STATUS_STAGE << 10) | (sdir << 16),
    );
    mmio.write32(dboff + slot as usize * 4, 1);
    for _ in 0..8 {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle) {
            Some((TRB_TRANSFER_EVENT, c, _)) => return c == 1 || c == 13,
            Some(_) => {}
            None => return false,
        }
    }
    false
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

    // Hot-plug state that persists across passes.
    let mut announce = false; // suppress the connect line for the boot device
    let mut signaled = false; // signal_input_ready (boot-screen clear) exactly once

    // Hot-plug loop. Each pass FULLY re-initializes the controller (stop, reset,
    // rebuild the command/event rings + DCBAA, run) so every (re)enumeration starts
    // from pristine state — no stale completion events or slots can survive an
    // unplug/replug to desync the rings. Then it (re)scans ports, binds a HID
    // device, and polls until it is unplugged (root-port CCS drops); on a drop it
    // announces and loops. With nothing attached it waits on the ports. Per-pass
    // re-init is heavy, but hot-plug is infrequent and it keeps the ring bookkeeping
    // trivially correct (§26.12: correctness over cleverness).
    'reenum: loop {
        // Stop + reset the controller.
        let cmd = mmio.read32(op + OP_USBCMD);
        mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
        spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH != 0);
        mmio.write32(op + OP_USBCMD, CMD_HCRST);
        spin(|| {
            mmio.read32(op + OP_USBCMD) & CMD_HCRST == 0 && mmio.read32(op + OP_USBSTS) & STS_CNR == 0
        });
        // Rebuild DMA structures + run.
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

        // Fresh ring bookkeeping for this pass.
        let mut ev_idx = 0usize;
        let mut ev_cycle = 1u32;
        let mut cmd_idx = 0usize;

    // Enumerate EVERY connected port and bind to the first device that is a HID
    // keyboard or mouse — i.e. one that exposes an interrupt-IN endpoint
    // (Linux-style, class-based binding). The mass-storage boot drive has no
    // interrupt endpoint and is skipped, so the device is found wherever it sits.
    let mut found = false;
    let mut port = 0u32;
    let mut slot = 0u32;
    let mut speed = 0u32;
    let mut max_packet = 64u32;
    let mut ep_addr = 0u8;
    let mut ep_mps = 0u16;
    let mut ep_interval = 0u8;
    let mut cfg_val = 0u8;
    let mut kbd_iface = 0u8;   // bInterfaceNumber of the bound HID interface
    let mut bound_proto = 0u8; // HID protocol of the bound device (1=keyboard, 2=mouse)

    // --- Port census (back-USB diagnostic) ---
    // Log EVERY root-hub port's PORTSC, connected or not, BEFORE we start binding.
    // This tells us exactly which xHCI ports are live when a keyboard is plugged
    // into a back socket: if a back-port keyboard shows connected=1 here, it's an
    // xHCI port we can enumerate (a driver fix); if NO xHCI port reacts to the
    // back socket, that connector hangs off the EHCI controller (00:12.0), which
    // this driver does not drive — a much bigger piece of work. CCS=bit0,
    // PED=bit1, speed=bits10-13.
    for p in 1..=max_ports {
        let psc = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10);
        ctx.log_fmt(format_args!(
            "xhci: port census {}/{}: PORTSC={:#010x} connected={} enabled={} speed={}",
            p, max_ports, psc,
            (psc & PORT_CCS != 0) as u8,
            (psc & (1 << 1) != 0) as u8,
            (psc >> 10) & 0xF,
        ));
    }

    'ports: for p in 1..=max_ports {
        let portsc_off = op + OP_PORTSC_BASE + (p as usize - 1) * 0x10;
        let psc = mmio.read32(portsc_off);
        if psc & PORT_CCS == 0 {
            continue; // nothing connected on this port
        }
        port = p; // root-hub port number used by Address Device below
        ctx.log_fmt(format_args!(
            "xhci: enumerating port {} PORTSC={:#010x}",
            p, psc
        ));

        // Enable the port. USB3 (SuperSpeed) ports auto-train and are already
        // enabled (PED=1) — issuing the USB2 port-reset (PR) bit *disables* them
        // (PORTSC speed→0, link→Disabled). So only reset a not-yet-enabled (USB2)
        // port; an already-enabled port is used as-is.
        if psc & PORT_PED == 0 {
            mmio.write32(portsc_off, (psc & !PORT_RW1C) | PORT_PR);
            spin(|| mmio.read32(portsc_off) & PORT_PED != 0);
        }
        let psc = mmio.read32(portsc_off);
        speed = (psc >> 10) & 0xF;
        max_packet = match speed {
            2 => 8,   // low-speed
            4 => 512, // super-speed
            _ => 64,  // full / high-speed
        };
        ctx.log_fmt(format_args!(
            "xhci: port {} ready; PORTSC={:#010x} speed={} max_packet={}",
            p, psc, speed, max_packet
        ));

        // Enable Slot.
        let cmd_off = CMD_RING_OFF + cmd_idx * TRB_SIZE;
        cmd_idx += 1;
        let (comp, got_slot) = match run_command(
            &ctx,
            &dma,
            &mmio,
            dboff,
            ir0,
            cmd_off,
            0,
            0,
            0,
            (TRB_ENABLE_SLOT << 10) | 1,
            &mut ev_idx,
            &mut ev_cycle,
        ) {
            Some(r) => r,
            None => {
                ctx.log("xhci: Enable Slot — no completion; next port");
                continue 'ports;
            }
        };
        if comp != 1 {
            ctx.log_fmt(format_args!(
                "xhci: Enable Slot failed (completion={}); next port",
                comp
            ));
            continue 'ports;
        }
        slot = got_slot;
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
        cmd_idx += 1;
        let (comp, _) = match run_command(
            &ctx,
            &dma,
            &mmio,
            dboff,
            ir0,
            cmd_off,
            in_phys as u32,
            (in_phys >> 32) as u32,
            0,
            (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1,
            &mut ev_idx,
            &mut ev_cycle,
        ) {
            Some(r) => r,
            None => {
                ctx.log("xhci: Address Device — no completion; next port");
                continue 'ports;
            }
        };
        if comp != 1 {
            ctx.log_fmt(format_args!(
                "xhci: Address Device failed (completion={}); next port",
                comp
            ));
            continue 'ports;
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
        dma.write32(
            EP0_TR_OFF + 12,
            1 | (1 << 6) | (TRB_SETUP_STAGE << 10) | (3 << 16),
        ); // cyc|IDT|type|TRT=IN
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
            ctx.log("xhci: Get Device Descriptor failed; next port");
            continue 'ports;
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
            ctx.log("xhci: Get Config Descriptor failed; next port");
            continue 'ports;
        }

        // Walk the descriptors: config (bConfigurationValue), interface (HID protocol),
        // endpoint (the interrupt-IN endpoint we'll poll for key reports).
        let total = ((dma.read32(CONFIG_BUF_OFF) >> 16) & 0xFFFF) as usize;
        let mut i = 0usize;
        ep_addr = 0;
        ep_mps = 0;
        ep_interval = 0;
        cfg_val = 0;
        let mut hid_proto = 0u8;
        let mut cur_hid = false; // are we inside a HID boot keyboard/mouse interface?
        while i + 2 <= total && i < 200 {
            let blen = dma.read8(CONFIG_BUF_OFF + i) as usize;
            let dtype = dma.read8(CONFIG_BUF_OFF + i + 1);
            if blen == 0 {
                break;
            }
            match dtype {
                2 => cfg_val = dma.read8(CONFIG_BUF_OFF + i + 5),
                4 => {
                    // Interface descriptor: class[+5], protocol[+7], number[+2].
                    // Bind a HID boot keyboard (class 3, protocol 1) OR mouse
                    // (protocol 2). A composite device may expose extra interfaces
                    // (e.g. media keys, protocol 0) with their OWN interrupt-IN
                    // endpoints — bind the boot keyboard/mouse one, not whichever
                    // endpoint happens to come last.
                    let iclass = dma.read8(CONFIG_BUF_OFF + i + 5);
                    let iproto = dma.read8(CONFIG_BUF_OFF + i + 7);
                    cur_hid = iclass == 3 && (iproto == 1 || iproto == 2);
                    if cur_hid {
                        hid_proto = iproto;
                        kbd_iface = dma.read8(CONFIG_BUF_OFF + i + 2);
                    }
                }
                5 => {
                    let addr = dma.read8(CONFIG_BUF_OFF + i + 2);
                    let attr = dma.read8(CONFIG_BUF_OFF + i + 3);
                    // First interrupt-IN endpoint of the bound HID interface.
                    if cur_hid && attr & 0x3 == 0x3 && addr & 0x80 != 0 && ep_addr == 0 {
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
            // This device is a HID boot keyboard or mouse — bind it and stop.
            ctx.log_fmt(format_args!(
                "xhci: {} found on port {} (slot {}, proto={})",
                if hid_proto == 2 { "mouse" } else { "keyboard" }, port, slot, hid_proto
            ));
            bound_proto = hid_proto;
            found = true;
            break 'ports;
        }
        ctx.log_fmt(format_args!(
            "xhci: port {} device has no interrupt-IN endpoint (not a keyboard/mouse); next port",
            port
        ));
    } // end 'ports — scan of every connected port

    if !found {
        // Nothing usable attached. Still report input-ready once so the shell's
        // boot-screen clear fires (the keyboard may be on the other controller),
        // then wait for a port connection and re-scan.
        if !signaled { ctx.signal_input_ready(); signaled = true; }
        ctx.log("xhci: no HID keyboard/mouse on any port — waiting for a connection");
        wait_for_port(&ctx, &mmio, op, max_ports);
        announce = true; // whatever connects now is a real plug event
        continue 'reenum;
    }
    let is_mouse = bound_proto == 2;
    let ep_num = (ep_addr & 0x0F) as u32;
    let dci = ep_num * 2 + 1;
    ctx.log_fmt(format_args!(
        "xhci: HID int-IN endpoint {} (DCI {}) mps={} interval={} cfg_val={}",
        ep_num, dci, ep_mps, ep_interval, cfg_val
    ));

    // --- Stage 4b: Configure Endpoint (add the interrupt-IN endpoint) ---
    let islot = INPUT_CTX_OFF + ctx_size; // Slot Context within the input context
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 1 | (1 << dci)); // Add: slot + interrupt endpoint
    dma.write32(islot, (dci << 27) | (speed << 20)); // Context Entries = dci
    dma.write32(islot + 4, port << 16);
    let iep = INPUT_CTX_OFF + (1 + dci as usize) * ctx_size;
    let int_tr = dma.phys_at(INT_TR_OFF);
    // xHCI Endpoint Context Interval encoding is speed-dependent (xHCI §6.2.3.6):
    //   Low/Full speed (PSI 1,2): bInterval is in 1 ms frames → 3 + floor(log2(bInterval)),
    //                             clamped to [3, 10].
    //   High/Super speed (PSI 3,4): bInterval is already a 2^(n-1) exponent → bInterval - 1.
    // Writing the raw frame count (e.g. 32 → 31) overflows the field → Parameter Error (17).
    let xhci_interval = match speed {
        1 | 2 => {
            let bi = if ep_interval == 0 { 1 } else { ep_interval as u32 };
            (3 + (31 - bi.leading_zeros())).clamp(3, 10)
        }
        _ => {
            if ep_interval > 1 {
                (ep_interval - 1) as u32
            } else {
                0
            }
        }
    };
    dma.write32(iep, xhci_interval << 16); // Interval [23:16]
    dma.write32(iep + 4, (3 << 1) | (7 << 3) | ((ep_mps as u32) << 16)); // CErr|Int-IN(7)|mps
    dma.write32(iep + 8, (int_tr as u32 & !0xF) | 1); // TR Dequeue | DCS
    dma.write32(iep + 12, (int_tr >> 32) as u32);
    dma.write32(iep + 16, ep_mps as u32 | ((ep_mps as u32) << 16)); // Avg TRB | Max ESIT
    // Command ring must stay contiguous (no gaps): use cmd_idx, then advance.
    let cmd_off = CMD_RING_OFF + cmd_idx * TRB_SIZE;
    cmd_idx += 1;
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let ce = run_command(
        &ctx,
        &dma,
        &mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        &mut ev_idx,
        &mut ev_cycle,
    )
    .map(|(c, _)| c)
    .unwrap_or(0);
    ctx.log_fmt(format_args!("xhci: Configure Endpoint completion={}", ce));

    // Set Configuration, then Set Protocol (boot) on EP0.
    if control(
        &dma,
        &mmio,
        dboff,
        ir0,
        slot,
        96,
        &mut ev_idx,
        &mut ev_cycle,
        0x00,
        9,
        cfg_val as u32,
        0,
        0,
        0,
    ) {
        ctx.log("xhci: Set Configuration OK");
    } else {
        ctx.log("xhci: Set Configuration failed");
    }
    let _ = control(
        &dma,
        &mmio,
        dboff,
        ir0,
        slot,
        128,
        &mut ev_idx,
        &mut ev_cycle,
        0x21,
        0x0B,
        0,
        kbd_iface as u32,
        0,
        0,
    ); // SET_PROTOCOL: boot (wValue=0) on the keyboard's interface

    // --- Stage 4c: poll the interrupt endpoint for HID key reports ---
    let report_phys = dma.phys_at(REPORT_OFF);
    let ring_phys = dma.phys_at(INT_TR_OFF);
    let link = INT_TR_OFF + 15 * 16; // Link TRB closes the 16-entry ring
    dma.write32(link, ring_phys as u32);
    dma.write32(link + 4, (ring_phys >> 32) as u32);
    dma.write32(link + 8, 0);
    dma.write32(link + 12, (TRB_LINK << 10) | (1 << 1) | 1); // Link | Toggle Cycle | cycle

    ctx.log_fmt(format_args!("xhci: {} ready", if is_mouse { "mouse" } else { "keyboard" }));
    if !signaled { ctx.signal_input_ready(); signaled = true; } // boot-screen clear, once
    if announce {
        notify(&ctx, if is_mouse { "mouse connected (xhci)" } else { "keyboard connected (xhci)" });
    }
    let mut int_idx = 0usize;
    let mut int_cycle = 1u32;
    let mut need_queue = true;
    let mut kb_last = [0u8; 6];                            // keyboard edge-detection state
    let mut mouse = godspeed_sdk::hid::MouseTracker::new(); // mouse button/motion state
    let portsc_off = op + OP_PORTSC_BASE + (port as usize - 1) * 0x10;
    'poll: loop {
        if need_queue {
            let t = INT_TR_OFF + int_idx * 16;
            dma.write32(t, report_phys as u32);
            dma.write32(t + 4, (report_phys >> 32) as u32);
            dma.write32(t + 8, 8);
            dma.write32(t + 12, int_cycle | (1 << 5) | (TRB_NORMAL << 10)); // cycle|IOC|Normal
            int_idx += 1;
            if int_idx == 15 {
                dma.write32(link + 12, (TRB_LINK << 10) | (1 << 1) | int_cycle);
                int_idx = 0;
                int_cycle ^= 1;
            }
            mmio.write32(dboff + slot as usize * 4, dci); // ring the endpoint doorbell
            need_queue = false;
        }
        if let Some((TRB_TRANSFER_EVENT, _, _)) =
            next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle)
        {
            // Copy the 8-byte boot report out of DMA, then decode it with the
            // controller-agnostic shared HID logic (§26.2). Keyboard reports push
            // characters into the console; mouse reports log to serial (no cursor
            // in a text console — that belongs to a future display server).
            let mut rep = [0u8; 8];
            for j in 0..8 { rep[j] = dma.read8(REPORT_OFF + j); }
            if is_mouse {
                mouse.feed(
                    &rep,
                    |mask, down| ctx.log_fmt(format_args!(
                        "xhci: mouse {} {}",
                        godspeed_sdk::hid::button_name(mask), if down { "down" } else { "up" })),
                    |dx, dy| ctx.log_fmt(format_args!("xhci: mouse moved dx={} dy={}", dx, dy)),
                );
            } else {
                godspeed_sdk::hid::decode_keyboard(&rep, &mut kb_last, |ch| ctx.console_push(ch));
            }
            need_queue = true;
        }
        // Unplug detection: the bound root port's Current Connect Status drops.
        // A plain MMIO read — it doesn't disturb the transfer rings.
        if mmio.read32(portsc_off) & PORT_CCS == 0 {
            break 'poll;
        }
        ctx.yield_cpu();
    }

    // Device unplugged. Announce it and loop: the controller is fully
    // re-initialized at the top of the next pass, which frees the slot and clears
    // all device state, so we just await the reconnect.
    notify(&ctx, if is_mouse { "mouse disconnected (xhci)" } else { "keyboard disconnected (xhci)" });
    announce = true;
    } // end 'reenum loop
}
