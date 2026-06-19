// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `xhci` — USB host-controller driver (§12). Multi-HID: enumerates EVERY
//! connected port and binds up to `MAX_HID` boot-protocol HID devices (a
//! keyboard AND a mouse) on the SAME controller at once, then polls all of them
//! from one loop, demultiplexing transfer events by slot id. All hardware access
//! is via the SDK's audited Mmio / Dma wrappers (§18); no `unsafe` here.

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
// Interrupter enable (P2, interrupt-driven USB §12). The kernel programmed the controller's
// MSI-X to deliver to vector 0x28; these turn the controller's interrupt generation on.
const CMD_INTE: u32 = 1 << 2; // USBCMD: global interrupter enable
const IMAN_IE: u32 = 1 << 1;  // Interrupter 0 Management: Interrupt Enable
const IMAN_IP: u32 = 1 << 0;  // Interrupter 0 Management: Interrupt Pending (write 1 to clear)
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0;
const STS_CNR: u32 = 1 << 11;

const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_RW1C: u32 = 0x00FE_0000; // change bits 17..23 (write 0 to preserve)

// DMA arena layout (64 KiB). Shared controller structures up front, then a
// per-device 4-page slice (device context + EP0 ring + interrupt ring + report
// buffer) for each HID device we bind — so a keyboard AND a mouse can run on the
// same controller at once. Device i occupies [DEV_BASE + i*DEV_STRIDE, +STRIDE).
const DCBAA_OFF: usize = 0x0000;
const CMD_RING_OFF: usize = 0x1000;
const EVENT_RING_OFF: usize = 0x2000;
const ERST_OFF: usize = 0x3000;
const INPUT_CTX_OFF: usize = 0x4000;  // transient: built per device for Address/Configure
const DATA_BUF_OFF: usize = 0x5000;   // transient: control-transfer data during enumeration
const CONFIG_BUF_OFF: usize = 0x6000; // transient: config descriptor during enumeration

// Scratchpad: the controller's own runtime DMA workspace. DCBAA[0] points at the
// Scratchpad Buffer Array (SBA) — an array of physical pointers to N page-aligned
// scratchpad buffers, where N = HCSPARAMS2.MaxScratchpadBufs. Real AMD xHCI needs
// 256 of them and malfunctions (devices drop, re-enumerate) without them. The SBA
// lives at arena page 15; the buffers occupy pages 16.. (the arena's tail, sized
// for this in the kernel's XHCI_DMA_PAGES).
const SCRATCHPAD_SBA_OFF: usize = 0xF000;
const SCRATCHPAD_BUF_BASE: usize = 0x10000;
const MAX_SCRATCHPAD: usize = 256; // arena room = XHCI_DMA_PAGES (272) - 16

/// Maximum HID devices bound on one controller at once (keyboard + mouse).
const MAX_HID: usize = 2;

/// Typematic auto-repeat delays, in TSC cycles (`ctx.read_tsc()` units). Sized for a
/// ~2 GHz CPU (the T630): ~300 ms before the first repeat, then ~50 ms apart (~20/s).
/// Auto-repeat is forgiving, so a 1.5–3 GHz spread just shifts the feel a little; no
/// per-machine calibration needed. read_tsc is hardware-proven to advance (perf §22).
const REPEAT_INITIAL_CYCLES: u64 = 600_000_000;
const REPEAT_INTERVAL_CYCLES: u64 = 100_000_000;
const DEV_BASE: usize = 0x7000;
const DEV_STRIDE: usize = 0x4000; // 4 pages: device ctx, EP0 ring, int ring, report
fn device_ctx_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE }
fn ep0_tr_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x1000 }
fn int_tr_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x2000 }
fn report_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x3000 }

/// A bound HID device: its slot, interrupt-endpoint DCI, root-hub port (for
/// disconnect detection), per-device DMA slice index, and whether it's a mouse.
#[derive(Clone, Copy)]
struct Hid { slot: u32, dci: u32, port: u32, idx: usize, is_mouse: bool }

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
/// this driver already feeds) makes the shell print a fresh `gsh> `. The leading
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
/// slot_id) and advances the dequeue pointer, or None.
///
/// Drain one event from the event ring. `max_tries` bounds how long to wait for an
/// event whose cycle bit has flipped: the command path passes a large budget (it just
/// rang a doorbell and expects a completion imminently); the **poll loop passes 1** so
/// it is fully non-blocking — otherwise, while a key is held (no new transfer events),
/// this would busy-spin millions of times before returning `None`, starving the
/// typematic auto-repeat poll at the bottom of the loop.
fn next_event(
    dma: &Dma,
    mmio: &Mmio,
    ir0: usize,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    max_tries: u32,
) -> Option<(u32, u32, u32)> {
    let mut tries = 0u32;
    while tries < max_tries {
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
#[allow(clippy::too_many_arguments)]
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
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, 10_000_000) {
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

/// Issue a control transfer on EP0 at `ep0_off` in device `dev`'s EP0 transfer
/// ring (Setup, optional IN Data, Status). `wlen == 0` means a no-data transfer.
/// Returns true on success/short-packet completion.
#[allow(clippy::too_many_arguments)]
fn control(
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    slot: u32,
    dev: usize,
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
    let tr = ep0_tr_off(dev) + ep0_off;
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
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, 10_000_000) {
            Some((TRB_TRANSFER_EVENT, c, _)) => return c == 1 || c == 13,
            Some(_) => {}
            None => return false,
        }
    }
    false
}

/// Fully enumerate the device on root-hub `port` into per-device DMA slice
/// `dev_idx`: enable the port, Enable Slot, Address Device, read the device and
/// configuration descriptors, find a boot-protocol HID interrupt-IN endpoint,
/// Configure Endpoint, Set Configuration, Set Protocol(boot), and arm the
/// interrupt transfer ring. Returns `Some(Hid)` if the device is a boot keyboard
/// or mouse and is now ready to poll; `None` for any non-HID device (e.g. the
/// mass-storage boot drive) or on any enumeration failure. Shares the command and
/// event rings via the mutable bookkeeping refs.
#[allow(clippy::too_many_arguments)]
fn enumerate_one(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    op: usize,
    ctx_size: usize,
    port: u32,
    dev_idx: usize,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> Option<Hid> {
    let portsc_off = op + OP_PORTSC_BASE + (port as usize - 1) * 0x10;
    let psc = mmio.read32(portsc_off);
    if psc & PORT_CCS == 0 {
        return None; // nothing connected on this port
    }
    ctx.log_fmt(format_args!(
        "xhci: enumerating port {} PORTSC={:#010x} into dev slice {}",
        port, psc, dev_idx
    ));

    // Enable the port. USB3 (SuperSpeed) ports auto-train and are already enabled
    // (PED=1) — issuing the USB2 port-reset (PR) bit *disables* them. So only reset
    // a not-yet-enabled (USB2) port; an already-enabled port is used as-is.
    if psc & PORT_PED == 0 {
        mmio.write32(portsc_off, (psc & !PORT_RW1C) | PORT_PR);
        spin(|| mmio.read32(portsc_off) & PORT_PED != 0);
    }
    let psc = mmio.read32(portsc_off);
    let speed = (psc >> 10) & 0xF;
    let max_packet = match speed {
        2 => 8,   // low-speed
        4 => 512, // super-speed
        _ => 64,  // full / high-speed
    };
    ctx.log_fmt(format_args!(
        "xhci: port {} ready; PORTSC={:#010x} speed={} max_packet={}",
        port, psc, speed, max_packet
    ));

    // Enable Slot.
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, slot) = match run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off,
        0, 0, 0, (TRB_ENABLE_SLOT << 10) | 1,
        ev_idx, ev_cycle,
    ) {
        Some(r) => r,
        None => {
            ctx.log("xhci: Enable Slot — no completion");
            return None;
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Enable Slot failed (completion={})", comp));
        return None;
    }
    ctx.log_fmt(format_args!("xhci: slot {} enabled", slot));

    // Build the Input Context for Address Device.
    //   +0:            Input Control Context — Add flags A0(slot)|A1(ep0)
    //   +ctx_size:     Slot Context
    //   +2*ctx_size:   Endpoint 0 Context
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    dma.write32(INPUT_CTX_OFF + 4, 0b11); // Add Context flags: slot + EP0
    dma.write32(islot, (1 << 27) | (speed << 20));
    dma.write32(islot + 4, port << 16);
    let ep0_tr = dma.phys_at(ep0_tr_off(dev_idx));
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);

    // DCBAA[slot] = device context physical base.
    dma.write64(DCBAA_OFF + slot as usize * 8, dma.phys_at(device_ctx_off(dev_idx)));

    // Address Device command (input context ptr, slot id).
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, _) = match run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off,
        in_phys as u32, (in_phys >> 32) as u32, 0,
        (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1,
        ev_idx, ev_cycle,
    ) {
        Some(r) => r,
        None => {
            ctx.log("xhci: Address Device — no completion");
            return None;
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Address Device failed (completion={})", comp));
        return None;
    }
    ctx.log_fmt(format_args!(
        "xhci: Address Device OK — device on port {} addressed (slot {})",
        port, slot
    ));

    // Get Device Descriptor over EP0 (control transfer): Setup (immediate
    // GET_DESCRIPTOR), Data (IN, 18 bytes), Status (OUT, IOC).
    let data_phys = dma.phys_at(DATA_BUF_OFF);
    let tr0 = ep0_tr_off(dev_idx);
    dma.write32(tr0, 0x80 | (6 << 8) | (0x0100 << 16));
    dma.write32(tr0 + 4, 18 << 16);
    dma.write32(tr0 + 8, 8);
    dma.write32(tr0 + 12, 1 | (1 << 6) | (TRB_SETUP_STAGE << 10) | (3 << 16));
    dma.write32(tr0 + 16, data_phys as u32);
    dma.write32(tr0 + 20, (data_phys >> 32) as u32);
    dma.write32(tr0 + 24, 18);
    dma.write32(tr0 + 28, 1 | (TRB_DATA_STAGE << 10) | (1 << 16));
    dma.write32(tr0 + 32, 0);
    dma.write32(tr0 + 36, 0);
    dma.write32(tr0 + 40, 0);
    dma.write32(tr0 + 44, 1 | (1 << 5) | (TRB_STATUS_STAGE << 10));
    mmio.write32(dboff + slot as usize * 4, 1);
    let mut ok = false;
    for _ in 0..8 {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, 10_000_000) {
            Some((TRB_TRANSFER_EVENT, c, _)) => { ok = c == 1 || c == 13; break; }
            Some(_) => {}
            None => break,
        }
    }
    if !ok {
        ctx.log("xhci: Get Device Descriptor failed");
        return None;
    }
    let d0 = dma.read32(DATA_BUF_OFF);
    let ids = dma.read32(DATA_BUF_OFF + 8);
    ctx.log_fmt(format_args!(
        "xhci: DEVICE DESCRIPTOR bLength={} type={} VID={:#06x} PID={:#06x}",
        d0 & 0xFF, (d0 >> 8) & 0xFF, ids & 0xFFFF, (ids >> 16) & 0xFFFF
    ));

    // Get Configuration Descriptor (64 bytes); walk it for the boot-HID
    // interrupt-IN endpoint.
    let cfg_phys = dma.phys_at(CONFIG_BUF_OFF);
    let tr = ep0_tr_off(dev_idx) + 48;
    dma.write32(tr, 0x80 | (6 << 8) | (0x0200 << 16));
    dma.write32(tr + 4, 64 << 16);
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
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, 10_000_000) {
            Some((TRB_TRANSFER_EVENT, c, _)) => { cfg_ok = c == 1 || c == 13; break; }
            Some(_) => {}
            None => break,
        }
    }
    if !cfg_ok {
        ctx.log("xhci: Get Config Descriptor failed");
        return None;
    }

    // Walk the descriptors: config (bConfigurationValue), interface (HID protocol),
    // endpoint (the interrupt-IN endpoint we'll poll for reports). A composite
    // device may expose extra interfaces with their own interrupt-IN endpoints —
    // bind the boot keyboard (class 3, proto 1) or mouse (proto 2) interface, not
    // whichever endpoint happens to come last.
    let total = ((dma.read32(CONFIG_BUF_OFF) >> 16) & 0xFFFF) as usize;
    let mut i = 0usize;
    let mut ep_addr = 0u8;
    let mut ep_mps = 0u16;
    let mut ep_interval = 0u8;
    let mut cfg_val = 0u8;
    let mut hid_proto = 0u8;
    let mut kbd_iface = 0u8;
    let mut cur_hid = false;
    while i + 2 <= total && i < 200 {
        let blen = dma.read8(CONFIG_BUF_OFF + i) as usize;
        let dtype = dma.read8(CONFIG_BUF_OFF + i + 1);
        if blen == 0 { break; }
        match dtype {
            2 => cfg_val = dma.read8(CONFIG_BUF_OFF + i + 5),
            4 => {
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
    if ep_addr == 0 {
        ctx.log_fmt(format_args!(
            "xhci: port {} device has no interrupt-IN endpoint (not a keyboard/mouse)",
            port
        ));
        return None;
    }
    let is_mouse = hid_proto == 2;
    let ep_num = (ep_addr & 0x0F) as u32;
    let dci = ep_num * 2 + 1;
    ctx.log_fmt(format_args!(
        "xhci: {} found on port {} (slot {}, DCI {}, mps={} interval={} cfg_val={})",
        if is_mouse { "mouse" } else { "keyboard" },
        port, slot, dci, ep_mps, ep_interval, cfg_val
    ));

    // Configure Endpoint (add the interrupt-IN endpoint).
    let int_tr = dma.phys_at(int_tr_off(dev_idx));
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 1 | (1 << dci)); // Add: slot + interrupt endpoint
    dma.write32(islot, (dci << 27) | (speed << 20)); // Context Entries = dci
    dma.write32(islot + 4, port << 16);
    let iep = INPUT_CTX_OFF + (1 + dci as usize) * ctx_size;
    // xHCI Endpoint Context Interval encoding is speed-dependent (xHCI §6.2.3.6).
    let xhci_interval = match speed {
        1 | 2 => {
            let bi = if ep_interval == 0 { 1 } else { ep_interval as u32 };
            (3 + (31 - bi.leading_zeros())).clamp(3, 10)
        }
        _ => {
            if ep_interval > 1 { (ep_interval - 1) as u32 } else { 0 }
        }
    };
    dma.write32(iep, xhci_interval << 16);
    dma.write32(iep + 4, (3 << 1) | (7 << 3) | ((ep_mps as u32) << 16));
    dma.write32(iep + 8, (int_tr as u32 & !0xF) | 1);
    dma.write32(iep + 12, (int_tr >> 32) as u32);
    dma.write32(iep + 16, ep_mps as u32 | ((ep_mps as u32) << 16));
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let ce = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off,
        in_phys as u32, (in_phys >> 32) as u32, 0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        ev_idx, ev_cycle,
    )
    .map(|(c, _)| c)
    .unwrap_or(0);
    ctx.log_fmt(format_args!("xhci: Configure Endpoint completion={}", ce));

    // Set Configuration, then Set Protocol (boot) on EP0.
    if control(
        dma, mmio, dboff, ir0, slot, dev_idx, 96,
        ev_idx, ev_cycle, 0x00, 9, cfg_val as u32, 0, 0, 0,
    ) {
        ctx.log("xhci: Set Configuration OK");
    } else {
        ctx.log("xhci: Set Configuration failed");
    }
    let _ = control(
        dma, mmio, dboff, ir0, slot, dev_idx, 128,
        ev_idx, ev_cycle, 0x21, 0x0B, 0, kbd_iface as u32, 0, 0,
    ); // SET_PROTOCOL: boot (wValue=0) on the HID interface

    // Arm the interrupt transfer ring: the Link TRB closes the 16-entry ring.
    let ring_phys = dma.phys_at(int_tr_off(dev_idx));
    let link = int_tr_off(dev_idx) + 15 * 16;
    dma.write32(link, ring_phys as u32);
    dma.write32(link + 4, (ring_phys >> 32) as u32);
    dma.write32(link + 8, 0);
    dma.write32(link + 12, (TRB_LINK << 10) | (1 << 1) | 1);

    Some(Hid { slot, dci, port, idx: dev_idx, is_mouse })
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
    let hcs2 = mmio.read32(CAP_HCSPARAMS2);
    // Max Scratchpad Buffers: HCSPARAMS2 bits [31:27] (hi) and [25:21] (lo). Real
    // controllers need these pages allocated and DCBAA[0] pointed at the buffer
    // array before they run; if non-zero we must set them up (§ scratchpad).
    let max_scratch = (((hcs2 >> 27) & 0x1F) << 5) | ((hcs2 >> 21) & 0x1F);
    let ctx_size = if hcc1 & (1 << 2) != 0 { 64 } else { 32 }; // CSZ
    let dboff = (mmio.read32(CAP_DBOFF) & !0x3) as usize;
    let rtsoff = (mmio.read32(CAP_RTSOFF) & !0x1F) as usize;
    let op = caplen;
    let ir0 = rtsoff + 0x20;

    ctx.log_fmt(format_args!(
        "xhci: v{:#06x} slots={} ports={} ctx_size={} dboff={:#x} rtsoff={:#x} max_scratch={}",
        version, max_slots, max_ports, ctx_size, dboff, rtsoff, max_scratch
    ));

    // Hot-plug state that persists across passes.
    let mut announce = false;    // suppress the connect line for the boot device
    let mut signaled = false;    // signal_input_ready (boot-screen clear) exactly once
    let mut prev_ports = 0u32;   // root-hub ports bound on the previous pass

    // Hot-plug loop. Each pass FULLY re-initializes the controller (stop, reset,
    // rebuild the command/event rings + DCBAA, run) so every (re)enumeration starts
    // from pristine state — no stale completion events or slots can survive an
    // unplug/replug to desync the rings. Then it (re)scans every port, binds up to
    // MAX_HID HID devices (keyboard + mouse), and polls all of them until ANY of
    // them is unplugged (root-port CCS drops); on a drop it announces and loops,
    // re-binding whatever remains. Per-pass re-init is heavy, but hot-plug is
    // infrequent and it keeps the ring bookkeeping trivially correct (§26.12).
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
        // Scratchpad: build the SBA (N pointers to page-aligned buffers) and point
        // DCBAA[0] at it, so the controller has the runtime workspace it requires
        // (MaxScratchpadBufs); without it real xHCI drops devices after binding.
        let n_scratch = (max_scratch as usize).min(MAX_SCRATCHPAD);
        if n_scratch > 0 {
            for i in 0..n_scratch {
                dma.write64(
                    SCRATCHPAD_SBA_OFF + i * 8,
                    dma.phys_at(SCRATCHPAD_BUF_BASE + i * 0x1000),
                );
            }
            dma.write64(DCBAA_OFF, dma.phys_at(SCRATCHPAD_SBA_OFF));
        }
        mmio.write64(op + OP_DCBAAP, dma.phys_at(DCBAA_OFF));
        mmio.write64(op + OP_CRCR, dma.phys_at(CMD_RING_OFF) | 1);
        dma.write64(ERST_OFF, dma.phys_at(EVENT_RING_OFF));
        dma.write32(ERST_OFF + 8, EVENT_RING_TRBS as u32);
        mmio.write32(ir0 + 0x08, 1);
        mmio.write64(ir0 + 0x10, dma.phys_at(ERST_OFF));
        mmio.write64(ir0 + 0x18, dma.phys_at(EVENT_RING_OFF));
        mmio.write32(op + OP_CONFIG, max_slots);
        // P2 (interrupt-driven, §12): enable the interrupter so the controller raises its
        // MSI-X (kernel-programmed to vector 0x28) when it posts an event. IMAN: IE on, write
        // 1 to IP to clear any stale pending; USBCMD.INTE gates interrupts globally. The poll
        // loop still runs and acks (clears IMAN.IP) — belt-and-suspenders until P4.
        mmio.write32(ir0 + 0x00, IMAN_IE | IMAN_IP);
        let c = mmio.read32(op + OP_USBCMD);
        mmio.write32(op + OP_USBCMD, c | CMD_RS | CMD_INTE);
        spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH == 0);

        // Fresh ring bookkeeping for this pass.
        let mut ev_idx = 0usize;
        let mut ev_cycle = 1u32;
        let mut cmd_idx = 0usize;

        // --- Port census (diagnostic) ---
        // Log EVERY root-hub port's PORTSC, connected or not, before binding. This
        // tells us which xHCI ports are live; a device on a port absent here hangs
        // off the EHCI controller, which this driver does not drive.
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

        // Enumerate EVERY connected port, binding up to MAX_HID boot HID devices
        // (keyboard + mouse) into successive per-device DMA slices. Non-HID devices
        // (the mass-storage boot drive) return None and are skipped, so the HID
        // devices are found wherever they sit.
        let mut devs = [Hid { slot: 0, dci: 0, port: 0, idx: 0, is_mouse: false }; MAX_HID];
        let mut ndev = 0usize;
        for p in 1..=max_ports {
            if ndev >= MAX_HID { break; }
            if let Some(hid) = enumerate_one(
                &ctx, &dma, &mmio, dboff, ir0, op, ctx_size,
                p, ndev, &mut ev_idx, &mut ev_cycle, &mut cmd_idx,
            ) {
                devs[ndev] = hid;
                ndev += 1;
            }
        }

        if ndev == 0 {
            // Nothing usable attached. Still report input-ready once so the shell's
            // boot-screen clear fires (the keyboard may be on the other controller),
            // then wait for a port connection and re-scan.
            if !signaled { ctx.signal_input_ready(); signaled = true; }
            ctx.log("xhci: no HID keyboard/mouse on any port — waiting for a connection");
            wait_for_port(&ctx, &mmio, op, max_ports);
            announce = true; // whatever connects now is a real plug event
            continue 'reenum;
        }

        ctx.log_fmt(format_args!("xhci: {} HID device(s) bound", ndev));
        if !signaled { ctx.signal_input_ready(); signaled = true; } // boot-screen clear, once
        // Announce only devices that weren't already bound on the previous pass. A
        // hot-plug re-initializes the whole controller and re-binds EVERY surviving
        // device, but a device whose port was bound last pass wasn't physically
        // touched — announcing it again ("keyboard connected" when only the mouse
        // was unplugged) would be misleading. `announce` stays false for the boot
        // pass, so the initial devices are silent regardless.
        if announce {
            for d in &devs[..ndev] {
                if prev_ports & (1 << d.port) == 0 {
                    notify(&ctx, if d.is_mouse {
                        "mouse connected (xhci)"
                    } else {
                        "keyboard connected (xhci)"
                    });
                }
            }
        }
        // Remember which ports are bound so the next pass can tell a genuinely new
        // plug from a survivor the re-init merely re-bound.
        prev_ports = 0;
        for d in &devs[..ndev] { prev_ports |= 1 << d.port; }

        // --- Poll every bound device's interrupt endpoint from one loop ---
        // The event ring is shared; transfer events are demultiplexed by slot id.
        // Each device has its own ring cursor (int_idx/int_cycle), re-arm flag, and
        // decode state (keyboard rollover buffer or mouse tracker).
        let mut int_idx = [0usize; MAX_HID];
        let mut int_cycle = [1u32; MAX_HID];
        let mut need_queue = [true; MAX_HID];
        let mut kb_last = [[0u8; 6]; MAX_HID];
        let mut kb_rep = [
            godspeed_sdk::hid::KeyRepeat::new(REPEAT_INITIAL_CYCLES, REPEAT_INTERVAL_CYCLES),
            godspeed_sdk::hid::KeyRepeat::new(REPEAT_INITIAL_CYCLES, REPEAT_INTERVAL_CYCLES),
        ];
        let mut mouse = [
            godspeed_sdk::hid::MouseTracker::new(),
            godspeed_sdk::hid::MouseTracker::new(),
        ];
        // Snapshot every connected root-hub port at poll start: the bound HID
        // devices, plus any non-HID device (e.g. a thumbdrive). A genuinely NEW
        // connection — a port NOT in this set becoming connected — triggers a
        // re-enumeration, so a keyboard added while the mouse stays plugged is
        // noticed. Without this the poll loop only ever reacts to disconnects, so a
        // second device added later would stay invisible until everything is
        // unplugged. Ports already present (including a device that failed to
        // enumerate) never re-trigger: `present` is recomputed each pass and
        // includes them. A port whose device leaves has its bit cleared below, so
        // re-plugging into the same port counts as new.
        let mut present = 0u32;
        for p in 1..=max_ports {
            if mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0 {
                present |= 1 << p;
            }
        }
        let mut int_logged = false; // log the first MSI-X interrupt once (P2 proof)
        'poll: loop {
            // (Re-)arm each device's interrupt ring as needed.
            for d in 0..ndev {
                if !need_queue[d] { continue; }
                let dev = devs[d].idx;
                let report_phys = dma.phys_at(report_off(dev));
                let link = int_tr_off(dev) + 15 * 16;
                let t = int_tr_off(dev) + int_idx[d] * 16;
                dma.write32(t, report_phys as u32);
                dma.write32(t + 4, (report_phys >> 32) as u32);
                dma.write32(t + 8, 8);
                dma.write32(t + 12, int_cycle[d] | (1 << 5) | (TRB_NORMAL << 10));
                int_idx[d] += 1;
                if int_idx[d] == 15 {
                    dma.write32(link + 12, (TRB_LINK << 10) | (1 << 1) | int_cycle[d]);
                    int_idx[d] = 0;
                    int_cycle[d] ^= 1;
                }
                mmio.write32(dboff + devs[d].slot as usize * 4, devs[d].dci);
                need_queue[d] = false;
            }

            // Drain one event (non-blocking: max_tries=1) so a held key — which produces
            // no new events — doesn't trap us in next_event's spin and starve the auto-
            // repeat poll below. Any pending event is still processed one per iteration.
            if let Some((TRB_TRANSFER_EVENT, _, slot_id)) =
                next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle, 1)
            {
                if let Some(d) = devs[..ndev].iter().position(|h| h.slot == slot_id) {
                    let dev = devs[d].idx;
                    let mut rep = [0u8; 8];
                    for (j, b) in rep.iter_mut().enumerate() {
                        *b = dma.read8(report_off(dev) + j);
                    }
                    if devs[d].is_mouse {
                        mouse[d].feed(
                            &rep,
                            |mask, down| ctx.log_fmt(format_args!(
                                "xhci: mouse {} {}",
                                godspeed_sdk::hid::button_name(mask),
                                if down { "down" } else { "up" })),
                            |dx, dy| ctx.log_fmt(format_args!(
                                "xhci: mouse moved dx={} dy={}", dx, dy)),
                        );
                    } else {
                        godspeed_sdk::hid::decode_keyboard(
                            &rep, &mut kb_last[d], &mut kb_rep[d], ctx.read_tsc(),
                            |ch| ctx.console_push(ch),
                            |code| ctx.log_fmt(format_args!(
                                "xhci: unmapped HID key usage {:#04x} (add to sdk hid_to_ascii)", code)));
                    }
                    need_queue[d] = true;
                }
            }

            // Unplug detection: if ANY bound device's root-port CCS drops, break and
            // fully re-initialize — re-binding whatever remains on the next pass.
            for d in 0..ndev {
                let portsc_off = op + OP_PORTSC_BASE + (devs[d].port as usize - 1) * 0x10;
                if mmio.read32(portsc_off) & PORT_CCS == 0 {
                    notify(&ctx, if devs[d].is_mouse {
                        "mouse disconnected (xhci)"
                    } else {
                        "keyboard disconnected (xhci)"
                    });
                    announce = true;
                    break 'poll;
                }
            }
            // New-device detection: while we still have a free device slice, a port
            // that was NOT connected at poll start becoming connected is a fresh
            // plug — break and re-enumerate to bind it alongside the existing
            // device(s). Tracks port leaves so a re-plug into the same port counts.
            if ndev < MAX_HID {
                for p in 1..=max_ports {
                    let c = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0;
                    if c && present & (1 << p) == 0 {
                        ctx.log_fmt(format_args!("xhci: new device on port {} — re-enumerating", p));
                        announce = true;
                        break 'poll;
                    }
                    if !c { present &= !(1 << p); }
                }
            }
            // Typematic auto-repeat: a held key sends no further USB reports, so
            // synthesise repeats from the TSC cycle counter while the key stays down.
            let now = ctx.read_tsc();
            for d in 0..ndev {
                if !devs[d].is_mouse {
                    kb_rep[d].poll(now, |ch| ctx.console_push(ch));
                }
            }
            // P2 (interrupt-driven, §12): drain any interrupt events the kernel routed to us
            // (MSI-X → vector 0x28 → IPC). Proves the interrupt fires and reaches the driver.
            // Ack by clearing IMAN.IP (keep IE) so the next event re-arms. The poll path above
            // still processes the event ring and manages ERDP/EHB — belt-and-suspenders.
            while ctx.try_recv().is_some() {
                mmio.write32(ir0 + 0x00, IMAN_IE | IMAN_IP);
                if !int_logged {
                    ctx.log("xhci: MSI-X interrupt received (P2: interrupt path live)");
                    int_logged = true;
                }
            }
            ctx.yield_cpu();
        }
    } // end 'reenum loop
}
