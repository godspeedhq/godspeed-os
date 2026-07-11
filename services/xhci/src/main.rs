// SPDX-License-Identifier: GPL-2.0-only
//! `xhci` - USB host-controller driver (§12). Multi-HID: enumerates EVERY
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
const STS_HCH: u32 = 1 << 0;    // USBSTS: Host Controller Halted
const STS_HSE: u32 = 1 << 2;    // USBSTS: Host System Error (a DMA/system error halted the HC)
const STS_CNR: u32 = 1 << 11;
const STS_HCE: u32 = 1 << 12;   // USBSTS: Host Controller Error (internal fatal error)
/// USBSTS bits that mean the controller has stopped executing and only a reset recovers it. A
/// command that leaves any of these set has WEDGED the HC - and a halted HC also stops an
/// already-bound keyboard's transfers, so we must re-init rather than limp on (Item 3, Fix 1).
const STS_WEDGED: u32 = STS_HCH | STS_HSE | STS_HCE;

const PORT_CCS: u32 = 1 << 0;
const PORT_PED: u32 = 1 << 1;
const PORT_PR: u32 = 1 << 4;
const PORT_RW1C: u32 = 0x00FE_0000; // change bits 17..23 (write 0 to preserve)

// DMA arena layout (64 KiB). Shared controller structures up front, then a
// per-device 4-page slice (device context + EP0 ring + interrupt ring + report
// buffer) for each HID device we bind - so a keyboard AND a mouse can run on the
// same controller at once. Device i occupies [DEV_BASE + i*DEV_STRIDE, +STRIDE).
const DCBAA_OFF: usize = 0x0000;
const CMD_RING_OFF: usize = 0x1000;
const EVENT_RING_OFF: usize = 0x2000;
const ERST_OFF: usize = 0x3000;
const INPUT_CTX_OFF: usize = 0x4000;  // transient: built per device for Address/Configure
const DATA_BUF_OFF: usize = 0x5000;   // transient: control-transfer data during enumeration
const CONFIG_BUF_OFF: usize = 0x6000; // transient: config descriptor during enumeration

// Scratchpad: the controller's own runtime DMA workspace. DCBAA[0] points at the
// Scratchpad Buffer Array (SBA) - an array of physical pointers to N page-aligned
// scratchpad buffers, where N = HCSPARAMS2.MaxScratchpadBufs. Real AMD xHCI needs
// 256 of them and malfunctions (devices drop, re-enumerate) without them. The SBA
// lives at arena page 15; the buffers occupy pages 16.. (the arena's tail, sized
// for this in the kernel's XHCI_DMA_PAGES).
// Device slices sit first (DEV_BASE .. DEV_BASE + MAX_SLICES*DEV_STRIDE), then the SBA + scratchpad.
// Hub enumeration needs several slices live at once - the hub's own slice plus each downstream
// device's - so MAX_SLICES is larger than MAX_HID (docs/usb-hub.md). Keep these offsets in step with
// the kernel's XHCI_DMA_PAGES (32 + 256): control(7) + 6 slices*4 pages(24) + SBA(1) = 32, then 256.
const MAX_SLICES: usize = 6;                // per-device DMA slices (bound HIDs + transient hub/enum)
const SCRATCHPAD_SBA_OFF: usize = 0x1F000;  // = DEV_BASE + MAX_SLICES*DEV_STRIDE (0x7000 + 6*0x4000)
const SCRATCHPAD_BUF_BASE: usize = 0x20000; // = SCRATCHPAD_SBA_OFF + 0x1000
const MAX_SCRATCHPAD: usize = 256; // arena room = XHCI_DMA_PAGES (288) - 32

/// Maximum HID devices bound on one controller at once (keyboard + mouse).
const MAX_HID: usize = 2;

/// Typematic auto-repeat delays, in TSC cycles (`ctx.read_tsc()` units). Sized for a
/// ~2 GHz CPU (the T630): ~300 ms before the first repeat, then ~50 ms apart (~20/s).
/// Auto-repeat is forgiving, so a 1.5-3 GHz spread just shifts the feel a little; no
/// per-machine calibration needed. read_tsc is hardware-proven to advance (perf §22).
const REPEAT_INITIAL_CYCLES: u64 = 600_000_000;
const REPEAT_INTERVAL_CYCLES: u64 = 100_000_000;
/// How often the poll loop GET_STATUSes a hub's downstream port to notice a device unplugged from
/// behind it (no root PORTSC reflects that). ~0.5 s at ~1.5-2 GHz - responsive enough for a "keyboard
/// disconnected" notice, infrequent enough not to load the hub or eat keystrokes off the shared event
/// ring (the check runs a control transfer; between checks the keyboard endpoint has the ring to
/// itself).
const HUB_POLL_CYCLES: u64 = 1_000_000_000;
/// How long the "a hub is present but nothing usable is behind it" wait sleeps before re-walking the
/// hub. A device replugged BEHIND a hub changes no root PORTSC, so the root-port wait would miss it;
/// re-walking every ~1.5 s catches a back-port (re)connect. Only runs while NO HID is bound.
const HUB_RESCAN_CYCLES: u64 = 3_000_000_000;
const DEV_BASE: usize = 0x7000;
const DEV_STRIDE: usize = 0x4000; // 4 pages: device ctx, EP0 ring, int ring, report
fn device_ctx_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE }
fn ep0_tr_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x1000 }
fn int_tr_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x2000 }
fn report_off(i: usize) -> usize { DEV_BASE + i * DEV_STRIDE + 0x3000 }

/// Fixed pool of the MAX_SLICES per-device DMA slices (§26.6, no heap - a bitmap). A bound HID and an
/// in-use hub HOLD their slice for the enumeration pass (a downstream device's routing depends on its
/// hub's device context staying put); a transient probe - a non-HID device - frees it. Each hot-plug
/// pass re-inits the controller and starts a fresh allocator, so nothing leaks across passes.
struct SliceAlloc {
    used: [bool; MAX_SLICES],
}
impl SliceAlloc {
    fn new() -> Self {
        Self { used: [false; MAX_SLICES] }
    }
    fn alloc(&mut self) -> Option<usize> {
        (0..MAX_SLICES).find(|&i| !self.used[i]).inspect(|&i| self.used[i] = true)
    }
    fn free(&mut self, i: usize) {
        if i < MAX_SLICES {
            self.used[i] = false;
        }
    }
}

/// Free a slot back to the controller (Disable Slot) for a probed device we do not keep - a non-HID
/// downstream device - so controller slots do not leak across a hot-plug re-scan.
#[allow(clippy::too_many_arguments)]
fn disable_slot(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, slot: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) {
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let _ = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off, 0, 0, 0,
        (TRB_DISABLE_SLOT << 10) | (slot << 24) | 1, ev_idx, ev_cycle,
    );
}

/// A bound HID device: its slot, interrupt-endpoint DCI, root-hub port (for
/// disconnect detection), per-device DMA slice index, and whether it's a mouse.
///
/// For a device BEHIND a hub, `port` is the hub's root port (which never changes on the device's own
/// unplug), so the root-PORTSC disconnect check cannot see it leave. The `hub_*` fields carry the
/// parent hub's coordinates so the poll loop can instead GET_STATUS the hub's downstream port to notice
/// the unplug: `hub_slot` = the hub's xHC slot (0 = the device is on a root port, not behind a hub),
/// `hub_dev` = the hub's DMA slice (its EP0 ring), `hub_port` = the downstream port on the hub, and
/// `hub_off` = the EP0-ring byte offset just past enumeration where those status polls resume.
#[derive(Clone, Copy)]
struct Hid {
    slot: u32,
    dci: u32,
    port: u32,
    idx: usize,
    is_mouse: bool,
    hub_slot: u32,
    hub_dev: u32,
    hub_port: u32,
    hub_off: usize,
}

const EVENT_RING_TRBS: usize = 16;
const TRB_SIZE: usize = 16;

const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
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
/// that was NOT connected becomes connected - otherwise an always-present non-HID
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
        // Drain our IPC endpoint while we idle here. This wait - NOT the 'poll loop - is where a driver
        // with no HID attached lives, and 'poll is the only other place we drain. Without this, a chaos
        // flood-storm (or any stray send) fills our 16-deep queue and it sits at 16/16 FOREVER, exactly
        // the logger stub bug in another guise. try_recv is non-blocking, so the port poll is unaffected.
        while ctx.try_recv().is_some() {}
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
    // Leading "\n " - the space is sacrificial: the framebuffer drops the first
    // glyph drawn on a freshly-scrolled line, so we let it eat a space, not the
    // 'U' of "USB:". (Serial is unaffected.)
    ctx.console_write("\n USB: ");
    ctx.console_write(msg);
    ctx.console_push(b'\n');
}

fn idle(ctx: &ServiceContext) -> ! {
    // Degraded terminal path (no controller / no DMA / no keyboard). Still report
    // input-ready so the shell's boot-screen auto-clear fires - boot is "done" as
    // far as the input subsystem is concerned, even if there's no usable keyboard.
    ctx.signal_input_ready();
    // DRAIN our IPC endpoint forever, never just yield: a registered driver that idles here without
    // recv'ing lets a flood-storm (or any stray send) fill its 16-deep queue and sit at 16/16 FOREVER -
    // the logger stub bug, and the exact gap the flood-endpoint sweep missed for xhci's no-controller path.
    // We POLL (try_recv) rather than block on recv: a cross-core flood that must WAKE a deeply-blocked recv
    // on an AP is unreliable under QEMU TCG (the drain flaked in the flood-storm pin); the self-driven poll
    // drains every quantum with no wake needed (mirrors wait_for_port above + ehci idle_draining). Pinned by
    // the shell-test `chaos flood-storm xhci` step (xhci has no controller in QEMU, so it sits in this path).
    loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
}

/// Poll the event ring for the next event TRB. Returns (trb_type, completion,
/// slot_id) and advances the dequeue pointer, or None.
///
/// Drain one event from the event ring. `max_tries` bounds how long to wait for an
/// event whose cycle bit has flipped: the command path passes a large budget (it just
/// rang a doorbell and expects a completion imminently); the **poll loop passes 1** so
/// it is fully non-blocking - otherwise, while a key is held (no new transfer events),
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

/// GET_STATUS a hub's downstream port over the hub's EP0, to notice a device unplugged from behind the
/// hub (which changes no root PORTSC). Returns `Some(connected_bit)` (wPortStatus bit 0), or `None` on
/// a transfer failure (treated as "unknown", not a disconnect). The hub's EP0 ring is managed with a
/// persistent producer cursor `cur` + cycle `pcs` and a Link TRB at the ring tail, so the check can run
/// indefinitely without overrunning the one-page ring. Only the hub's own completion (slot_id ==
/// hub_slot) is accepted; a stray keyboard event landing in the tiny check window is skipped (a rare
/// dropped keystroke, not a misread status).
#[allow(clippy::too_many_arguments)]
fn hub_port_status(
    dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize,
    hub_slot: u32, hub_dev: usize, hub_port: u32,
    cur: &mut usize, pcs: &mut u32,
    ev_idx: &mut usize, ev_cycle: &mut u32,
    eaten: &mut u32,
) -> Option<bool> {
    const RING: usize = 0x1000;
    let base = ep0_tr_off(hub_dev);
    // If the 3-TRB GET_STATUS would not fit before the ring end, wrap: write a Link TRB AT the current
    // cursor (which is where the controller's dequeue sits - so no stale gap for it to stop on),
    // pointing back to base with Toggle Cycle, then reset the cursor + flip the producer cycle. The
    // `>=` keeps the cursor strictly below RING, so the Link (16 bytes) always fits at the tail.
    if *cur + 3 * 0x10 >= RING {
        let bp = dma.phys_at(base);
        dma.write32(base + *cur, bp as u32);
        dma.write32(base + *cur + 4, (bp >> 32) as u32);
        dma.write32(base + *cur + 8, 0);
        dma.write32(base + *cur + 12, (TRB_LINK << 10) | (1 << 1) | *pcs);
        *cur = 0;
        *pcs ^= 1;
    }
    let tr = base + *cur;
    let c = *pcs;
    // Setup: GET_STATUS(port) - bmRequestType 0xA3 (class, other, IN), bRequest 0, wValue 0,
    // wIndex = port, wLength 4.
    dma.write32(tr, 0xA3);
    dma.write32(tr + 4, hub_port | (4 << 16));
    dma.write32(tr + 8, 8);
    dma.write32(tr + 12, c | (1 << 6) | (TRB_SETUP_STAGE << 10) | (3 << 16)); // IDT, TRT=IN
    // Data: 4 bytes IN into DATA_BUF_OFF (unused by the poll loop, so safe to reuse here).
    let dp = dma.phys_at(DATA_BUF_OFF);
    dma.write32(tr + 16, dp as u32);
    dma.write32(tr + 20, (dp >> 32) as u32);
    dma.write32(tr + 24, 4);
    dma.write32(tr + 28, c | (TRB_DATA_STAGE << 10) | (1 << 16)); // DIR=IN
    // Status: OUT (opposite of the IN data stage), IOC.
    dma.write32(tr + 32, 0);
    dma.write32(tr + 36, 0);
    dma.write32(tr + 40, 0);
    dma.write32(tr + 44, c | (1 << 5) | (TRB_STATUS_STAGE << 10));
    *cur += 3 * 0x10;
    mmio.write32(dboff + hub_slot as usize * 4, 1); // ring the hub's EP0 doorbell (DCI 1)
    for _ in 0..8 {
        match next_event(dma, mmio, ir0, ev_idx, ev_cycle, 5_000_000) {
            Some((TRB_TRANSFER_EVENT, cc, sid)) if sid == hub_slot => {
                return if cc == 1 || cc == 13 {
                    Some(dma.read16(DATA_BUF_OFF) & 1 != 0) // wPortStatus bit0 = current connect
                } else {
                    None
                };
            }
            // A stray HID transfer event (a keystroke landing in this check window) - record its slot
            // so the caller re-arms that endpoint (its in-flight TRB is now spent). The report itself
            // is lost, a rare dropped keystroke, but the endpoint does not stall. Keep waiting for ours.
            Some((TRB_TRANSFER_EVENT, _, sid)) => { if sid < 32 { *eaten |= 1 << sid; } }
            Some(_) => {} // a non-transfer event (port change, command) - ignore; keep waiting
            None => return None,
        }
    }
    None
}

/// Configure an already-addressed device AS A HUB so the controller will route downstream traffic
/// through it: a Configure Endpoint command that sets the slot-context Hub bit, Number of Ports, MTT
/// (multi-TT), and TT Think Time (xHCI 4.6.5 / 6.2.2). Runs after the hub is Address'd +
/// Set_Configuration'd. `route` is 0 for a hub on a root port (recursion passes the parent's route).
#[allow(clippy::too_many_arguments)]
fn configure_as_hub(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, ctx_size: usize,
    slot: u32, dev_idx: usize, speed: u32, route: u32, root_port: u32, nports: u8, mtt: bool, ttt: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) -> bool {
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    dma.write32(INPUT_CTX_OFF, 0);        // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 0b11); // Add: slot + EP0
    // Slot dword0: Context Entries=1, Hub=1 (bit 26), MTT (bit 25), Speed (bits 23:20), Route (19:0).
    dma.write32(islot, (1 << 27) | (1 << 26) | (if mtt { 1 << 25 } else { 0 }) | (speed << 20) | (route & 0xFFFFF));
    // Slot dword1: Number of Ports [31:24], Root Hub Port Number [23:16].
    dma.write32(islot + 4, ((nports as u32) << 24) | (root_port << 16));
    // Slot dword2: TT Think Time [17:16].
    dma.write32(islot + 8, (ttt & 0x3) << 16);
    // Re-specify EP0 (Add A1 set) so the command carries a valid endpoint-0 context.
    let ep0_tr = dma.phys_at(ep0_tr_off(dev_idx));
    let max_packet = match speed { 2 => 8, 4 => 512, _ => 64 };
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let ce = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off,
        in_phys as u32, (in_phys >> 32) as u32, 0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        ev_idx, ev_cycle,
    )
    .map(|(c, _)| c)
    .unwrap_or(0);
    ctx.log_fmt(format_args!(
        "xhci: hub configure (Hub bit, {} ports, mtt={}, ttt={}) completion={}",
        nports, mtt, ttt, ce
    ));
    ce == 1
}

/// Address a device that sits BEHIND a hub, into per-device slice `dev_idx`, via a **route string**
/// (the path of hub-port numbers) and - for a low/full-speed device behind a high-speed hub - the
/// **parent-TT** fields (the hub's slot + port), so the controller runs split transactions for it
/// (xHCI 6.2.2). Enable Slot -> build the input slot context (route, speed, root port, TT) -> Address
/// Device -> read the device descriptor. Returns `(slot, vid, pid, class)` on success. This is the
/// downstream analogue of `enumerate_one`'s root-port addressing; the hard, fiddly part of hub support.
#[allow(clippy::too_many_arguments)]
fn address_downstream(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, ctx_size: usize,
    dev_idx: usize, route: u32, root_port: u32, speed: u32, parent_slot: u32, parent_port: u32, ttt: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) -> Option<(u32, u16, u16, u8)> {
    // Enable Slot.
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, slot) = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off, 0, 0, 0, (TRB_ENABLE_SLOT << 10) | 1, ev_idx, ev_cycle,
    )?;
    if comp != 1 {
        return None;
    }
    // Input context: Add slot + EP0.
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    dma.write32(INPUT_CTX_OFF, 0);
    dma.write32(INPUT_CTX_OFF + 4, 0b11);
    // Slot dword0: Context Entries=1, Speed [23:20], Route String [19:0]. (Not a hub, no MTT.)
    dma.write32(islot, (1 << 27) | (speed << 20) | (route & 0xFFFFF));
    // Slot dword1: Root Hub Port Number [23:16] (the root port the whole chain hangs off).
    dma.write32(islot + 4, root_port << 16);
    // Slot dword2: TT fields for a low/full-speed device (speed 1 or 2) behind a high-speed hub -
    // TT Hub Slot ID [7:0], TT Port Number [15:8], TT Think Time [17:16].
    let tt = if speed == 1 || speed == 2 {
        (parent_slot & 0xFF) | ((parent_port & 0xFF) << 8) | ((ttt & 0x3) << 16)
    } else {
        0
    };
    dma.write32(islot + 8, tt);
    // EP0 context.
    let ep0_tr = dma.phys_at(ep0_tr_off(dev_idx));
    let max_packet = match speed { 2 => 8, 4 => 512, _ => 8 };
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);
    dma.write64(DCBAA_OFF + slot as usize * 8, dma.phys_at(device_ctx_off(dev_idx)));
    // Address Device.
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, _) = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off, in_phys as u32, (in_phys >> 32) as u32, 0,
        (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1, ev_idx, ev_cycle,
    )?;
    if comp != 1 {
        ctx.log_fmt(format_args!(
            "xhci: downstream Address Device failed (completion={}, route={:#x})", comp, route
        ));
        return None;
    }
    // Read the device descriptor over the downstream slice's EP0 ring (offset 0).
    if !control(
        dma, mmio, dboff, ir0, slot, dev_idx, 0, ev_idx, ev_cycle, 0x80, 6, 0x0100, 0, 18, DATA_BUF_OFF,
    ) {
        return None;
    }
    let ids = dma.read32(DATA_BUF_OFF + 8);
    let class = dma.read8(DATA_BUF_OFF + 4);
    Some((slot, (ids & 0xFFFF) as u16, (ids >> 16) as u16, class))
}

/// Fully enumerate the device on root-hub `port` into per-device DMA slice
/// Given a device that already has an addressed slot and a working EP0 - either a root-port device
/// addressed by `enumerate_one` or a downstream device addressed by `address_downstream` - read its
/// configuration descriptor and, if it exposes a boot-protocol HID interrupt-IN endpoint, Configure
/// Endpoint + Set Configuration + Set Protocol(boot) + arm the interrupt ring. Returns
/// `(Some(Hid), cfg_val)` when a keyboard/mouse was bound, `(None, cfg_val)` when the device has no
/// boot-HID endpoint (a hub - the caller reuses `cfg_val` for its own Set_Configuration), or
/// `(None, 0)` if the config descriptor could not be read.
///
/// The slot-context fields (`route`, `root_port`, `parent_slot`, `parent_port`, `ttt`) are threaded
/// through because Configure Endpoint re-supplies the slot context (its Context Entries grows to cover
/// the new endpoint), and a downstream device's routing depends on that context still carrying its
/// route string + parent-TT. A root-port device passes route=0, root_port=port, parent_*=0, ttt=0, so
/// this reduces to the plain (no-route, no-TT) form.
#[allow(clippy::too_many_arguments)]
fn read_config_and_bind(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, dboff: usize, ir0: usize, ctx_size: usize,
    slot: u32, dev_idx: usize, speed: u32, port: u32,
    route: u32, root_port: u32, parent_slot: u32, parent_port: u32, ttt: u32,
    ev_idx: &mut usize, ev_cycle: &mut u32, cmd_idx: &mut usize,
) -> (Option<Hid>, u8) {
    // Get Configuration Descriptor (64 bytes) at EP0 ring offset 48 - contiguous after the 3-TRB
    // device-descriptor read at offset 0 - then walk it for the boot-HID interrupt-IN endpoint.
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
        return (None, 0);
    }
    // Walk config -> interface -> endpoint; bind the boot keyboard (class 3, proto 1) or mouse
    // (proto 2) interface's interrupt-IN endpoint, not whichever endpoint comes last.
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
        // No boot-HID interrupt-IN endpoint: a hub (caller walks it with cfg_val) or a non-HID device.
        return (None, cfg_val);
    }
    let is_mouse = hid_proto == 2;
    let ep_num = (ep_addr & 0x0F) as u32;
    let dci = ep_num * 2 + 1;
    ctx.log_fmt(format_args!(
        "xhci: {} found on port {} (slot {}, DCI {}, mps={} interval={} cfg_val={})",
        if is_mouse { "mouse" } else { "keyboard" },
        port, slot, dci, ep_mps, ep_interval, cfg_val
    ));

    // Configure Endpoint (add the interrupt-IN endpoint). The slot context is re-supplied with the
    // updated Context Entries AND this device's route/speed/root-port/TT, so a downstream device keeps
    // routing (a root-port device passes route=0 / parent_*=0, reducing to the plain form).
    let int_tr = dma.phys_at(int_tr_off(dev_idx));
    let islot = INPUT_CTX_OFF + ctx_size;
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 1 | (1 << dci)); // Add: slot + interrupt endpoint
    dma.write32(islot, (dci << 27) | (speed << 20) | (route & 0xFFFFF)); // Context Entries=dci, speed, route
    dma.write32(islot + 4, root_port << 16);
    let tt = if speed == 1 || speed == 2 {
        (parent_slot & 0xFF) | ((parent_port & 0xFF) << 8) | ((ttt & 0x3) << 16)
    } else {
        0
    };
    dma.write32(islot + 8, tt);
    let iep = INPUT_CTX_OFF + (1 + dci as usize) * ctx_size;
    // xHCI Endpoint Context Interval encoding is speed-dependent (xHCI 6.2.3.6).
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

    // Set Configuration, then Set Protocol (boot) on EP0 (offsets 96, 128 - contiguous after the
    // config-descriptor read that ended at 96).
    if control(
        dma, mmio, dboff, ir0, slot, dev_idx, 96,
        ev_idx, ev_cycle, 0x00, 9, cfg_val as u32, 0, 0, 0,
    ) {
        ctx.log("xhci: Set Configuration OK");
    } else {
        ctx.log("xhci: Set Configuration failed");
    }
    // SET_PROTOCOL: boot (wValue=0) on the HID interface. Best-effort (most keyboards default to boot
    // mode), but log the outcome - a keyboard that needs it and didn't get it is otherwise an
    // undiagnosable dead keyboard.
    if control(
        dma, mmio, dboff, ir0, slot, dev_idx, 128,
        ev_idx, ev_cycle, 0x21, 0x0B, 0, kbd_iface as u32, 0, 0,
    ) {
        ctx.log("xhci: Set Protocol (boot) OK");
    } else {
        ctx.log("xhci: Set Protocol (boot) failed - keyboard may report in non-boot mode");
    }

    // Arm the interrupt transfer ring: the Link TRB closes the 16-entry ring.
    let ring_phys = dma.phys_at(int_tr_off(dev_idx));
    let link = int_tr_off(dev_idx) + 15 * 16;
    dma.write32(link, ring_phys as u32);
    dma.write32(link + 4, (ring_phys >> 32) as u32);
    dma.write32(link + 8, 0);
    dma.write32(link + 12, (TRB_LINK << 10) | (1 << 1) | 1);

    // hub_* default to 0/direct; the downstream caller patches them for a device behind a hub.
    (Some(Hid { slot, dci, port, idx: dev_idx, is_mouse, hub_slot: 0, hub_dev: 0, hub_port: 0, hub_off: 0 }), cfg_val)
}

/// Enumerate whatever is attached to root-hub `port`, binding every boot HID it finds - directly on
/// the port, or behind a hub on the port - into `devs` (up to MAX_HID). A DMA slice is allocated per
/// device from `sa`; a bound HID and an active (HID-bearing) hub keep their slice for the pass, while a
/// transient probe - a non-HID device, or a hub with nothing usable behind it - frees its slice and
/// Disable-Slots its controller slot so neither leaks. A hub is configured AS a hub and its downstream
/// ports walked with route-string addressing + parent-TT, so a keyboard on a BACK port (behind the Wyse
/// 5070's internal Realtek hub) is reached and bound (docs/usb-hub.md). Shares the command and event
/// rings via the mutable bookkeeping refs.
#[allow(clippy::too_many_arguments)]
/// After a command failed or got no completion, read USBSTS and log it (HCH/HSE/HCE/CNR), returning
/// `true` if the controller has WEDGED (Item 3, Fix 1). A wedged HC does not just fail this one command
/// - it stops executing entirely, including an already-bound keyboard's interrupt transfers, so the
/// caller must poison the offending port and re-initialise the controller rather than issue more doomed
/// commands. Pure diagnosis when it returns false (e.g. a device-level Transaction Error with the HC
/// still running); the log is the breadcrumb that tells us, on the Wyse, which case a port hit.
fn hc_wedged_now(ctx: &ServiceContext, mmio: &Mmio, op: usize) -> bool {
    let sts = mmio.read32(op + OP_USBSTS);
    let wedged = sts & STS_WEDGED != 0;
    ctx.log_fmt(format_args!(
        "xhci: post-command USBSTS={:#010x} (HCH={} HSE={} HCE={} CNR={}){}",
        sts,
        (sts & STS_HCH != 0) as u8,
        (sts & STS_HSE != 0) as u8,
        (sts & STS_HCE != 0) as u8,
        (sts & STS_CNR != 0) as u8,
        if wedged { " - HC WEDGED, re-initialising" } else { "" },
    ));
    wedged
}

/// Walk the xHCI Extended Capabilities (xECP pointer in HCCPARAMS1[31:16]) and return a bitmask of the
/// root ports that belong to a USB 3.x (SuperSpeed) Supported Protocol Capability (Item 3, Fix 2).
///
/// On xHCI each physical USB3 connector is exposed as TWO logical root ports: a USB2 port and a USB3
/// (SuperSpeed) companion. Boot HID devices (keyboard/mouse) are always reached through the USB2 ports;
/// the SuperSpeed companions carry nothing the boot path needs, and the driver's SuperSpeed Address
/// Device path does not yet complete on the Wyse (it returns "no completion" while the HC stays healthy,
/// HCH=0 - see Fix 1's USBSTS log). Enumerating them only issues doomed commands and churns the shared
/// event ring, so the caller skips the ports this returns. Bit `p` = root port `p` (1-based, matching the
/// enumerate loop's `1..=max_ports`). Bounded walk: a malformed Next-pointer chain can neither loop
/// forever nor read far outside the cap region (§26.6). Returns 0 if there is no xECP list (all ports
/// enumerated as before).
fn usb3_port_mask(mmio: &Mmio, hcc1: u32, max_ports: u32) -> u64 {
    let mut mask = 0u64;
    let mut ptr = ((hcc1 >> 16) & 0xFFFF) as usize; // xECP, in dwords from the MMIO base (0 = none)
    let mut guard = 0u32;
    while ptr != 0 && guard < 64 && ptr * 4 + 8 < 0x1_0000 {
        guard += 1;
        let d0 = mmio.read32(ptr * 4);
        // Extended Capability: [7:0]=Cap ID, [15:8]=Next Cap Pointer (dwords, relative; 0 = end).
        if d0 & 0xFF == 2 {
            // Supported Protocol Capability: [31:24] of d0 = Major Revision (2 = USB2, 3 = USB3).
            // dword 2 (+8): [7:0]=Compatible Port Offset (1-based), [15:8]=Compatible Port Count.
            let major = (d0 >> 24) & 0xFF;
            let d2 = mmio.read32(ptr * 4 + 8);
            let port_off = d2 & 0xFF;
            let port_cnt = (d2 >> 8) & 0xFF;
            if major >= 3 {
                let mut p = port_off;
                while p < port_off.saturating_add(port_cnt) {
                    if p >= 1 && p <= max_ports && p < 64 {
                        mask |= 1u64 << p;
                    }
                    p += 1;
                }
            }
        }
        let next = (d0 >> 8) & 0xFF;
        if next == 0 { break; }
        ptr += next as usize;
    }
    mask
}

fn enumerate_one(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    op: usize,
    ctx_size: usize,
    port: u32,
    sa: &mut SliceAlloc,
    devs: &mut [Hid; MAX_HID],
    ndev: &mut usize,
    saw_hub: &mut bool,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
    hc_wedged: &mut bool,
) {
    *hc_wedged = false;
    let portsc_off = op + OP_PORTSC_BASE + (port as usize - 1) * 0x10;
    let psc = mmio.read32(portsc_off);
    if psc & PORT_CCS == 0 {
        return; // nothing connected on this port
    }
    // A per-device DMA slice for this root-port device (freed below unless it's a kept HID or a hub
    // with a HID behind it). Out of slices = stop cleanly (bounded arena, no heap - Commandment on
    // bounded behaviour, CLAUDE.md 26.6).
    let dev_idx = match sa.alloc() {
        Some(i) => i,
        None => {
            ctx.log("xhci: out of DMA slices - cannot enumerate more devices");
            return;
        }
    };
    ctx.log_fmt(format_args!(
        "xhci: enumerating port {} PORTSC={:#010x} into dev slice {}",
        port, psc, dev_idx
    ));

    // Enable the port. USB3 (SuperSpeed) ports auto-train and are already enabled (PED=1); issuing the
    // USB2 port-reset (PR) bit *disables* them. So only reset a not-yet-enabled (USB2) port.
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
            ctx.log("xhci: Enable Slot - no completion");
            *hc_wedged = hc_wedged_now(ctx, mmio, op);
            sa.free(dev_idx);
            return;
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Enable Slot failed (completion={})", comp));
        *hc_wedged = hc_wedged_now(ctx, mmio, op);
        sa.free(dev_idx);
        return;
    }
    ctx.log_fmt(format_args!("xhci: slot {} enabled", slot));

    // Build the Input Context and Address Device (root port, no route string).
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
    dma.write64(DCBAA_OFF + slot as usize * 8, dma.phys_at(device_ctx_off(dev_idx)));
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
            ctx.log("xhci: Address Device - no completion");
            let wedged = hc_wedged_now(ctx, mmio, op);
            sa.free(dev_idx);
            // Only try to disable the slot if the HC is still executing; on a wedged HC the command
            // would just be another doomed "no completion". The re-init reclaims the slot anyway.
            if !wedged {
                disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
            }
            *hc_wedged = wedged;
            return;
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!("xhci: Address Device failed (completion={})", comp));
        let wedged = hc_wedged_now(ctx, mmio, op);
        sa.free(dev_idx);
        if !wedged {
            disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
        }
        *hc_wedged = wedged;
        return;
    }
    ctx.log_fmt(format_args!(
        "xhci: Address Device OK - device on port {} addressed (slot {})",
        port, slot
    ));

    // Get Device Descriptor (18 bytes) over EP0 at ring offset 0 - for the device class (0x09 = hub).
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
        sa.free(dev_idx);
        disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
        return;
    }
    let ids = dma.read32(DATA_BUF_OFF + 8);
    let dclass = dma.read8(DATA_BUF_OFF + 4); // bDeviceClass: 0x09 = Hub
    let dproto = dma.read8(DATA_BUF_OFF + 6); // bDeviceProtocol: on a hub, 2 = multi-TT
    ctx.log_fmt(format_args!(
        "xhci: DEVICE DESCRIPTOR class={:#04x} VID={:#06x} PID={:#06x}",
        dclass, ids & 0xFFFF, (ids >> 16) & 0xFFFF
    ));

    // Read the config descriptor and bind if it's a boot HID (root device: route=0, parent_*=0).
    let (bound, cfg_val) = read_config_and_bind(
        ctx, dma, mmio, dboff, ir0, ctx_size, slot, dev_idx, speed, port,
        0, port, 0, 0, 0, ev_idx, ev_cycle, cmd_idx,
    );
    if let Some(hid) = bound {
        if *ndev < MAX_HID {
            devs[*ndev] = hid;
            *ndev += 1;
        } else {
            sa.free(dev_idx);
            disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
        }
        return;
    }
    if dclass != 0x09 {
        // Not a HID and not a hub (e.g. the mass-storage boot drive) - release the slice + slot.
        ctx.log_fmt(format_args!(
            "xhci: port {} device has no interrupt-IN endpoint (not a keyboard/mouse)",
            port
        ));
        sa.free(dev_idx);
        disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
        return;
    }

    // --- It's a USB hub. Read the hub descriptor, configure the device AS a hub, then walk its
    // downstream ports. Its slice is KEPT while we walk (downstream routing depends on the hub's
    // device context staying put); it is released at the end only if nothing usable was found behind
    // it. The Wyse 5070 routes its BACK ports through such a hub - this is how the back-port keyboard
    // is reached (docs/usb-hub.md). The EP0 ring must stay CONTIGUOUS: each transfer starts where the
    // previous ended. Config read ended at 96; Set_Configuration (no-data, 2 TRBs) ends at 128; the
    // hub descriptor starts at 128.
    let _ = control(
        dma, mmio, dboff, ir0, slot, dev_idx, 96,
        ev_idx, ev_cycle, 0x00, 9, cfg_val as u32, 0, 0, 0,
    );
    let hub_ok = control(
        dma, mmio, dboff, ir0, slot, dev_idx, 128,
        ev_idx, ev_cycle, 0xA0, 6, 0x29 << 8, 0, 8, DATA_BUF_OFF,
    );
    let nports = if hub_ok { dma.read8(DATA_BUF_OFF + 2) } else { 0 };
    let whubchar = dma.read16(DATA_BUF_OFF + 3); // wHubCharacteristics
    let ttt = ((whubchar >> 5) & 0x3) as u32;    // TT Think Time [6:5]
    let mtt = dproto == 2;                        // bDeviceProtocol 2 = multi-TT hub
    ctx.log_fmt(format_args!(
        "xhci: USB hub on port {} (slot {}, {} downstream ports, mtt={}, ttt={})",
        port, slot, nports, mtt, ttt
    ));
    // A SuperSpeed hub (the USB3 side of the Realtek hubs, 0x0411/0x0415) uses hub descriptor type
    // 0x2A, so the 0x29 read returns 0 ports - skip it (the low-speed keyboard is on the USB2 hub).
    if nports == 0 {
        ctx.log("xhci: hub reports 0 ports (SuperSpeed hub? needs descriptor 0x2A) - not walking");
        sa.free(dev_idx);
        disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
        return;
    }
    // A real hub is present. Record it so the "no HID bound" wait re-walks periodically (a device
    // replugged behind a hub changes no root PORTSC - see the reenum loop).
    *saw_hub = true;
    // Configure the device AS a hub so the controller routes downstream traffic through it.
    configure_as_hub(
        ctx, dma, mmio, dboff, ir0, ctx_size, slot, dev_idx, speed, 0, port, nports, mtt, ttt,
        ev_idx, ev_cycle, cmd_idx,
    );
    // POWER every downstream port. The EP0 cursor stays contiguous (hub descriptor ended at ~176);
    // bounded so a many-port hub cannot overrun the one-page ring.
    let mut hoff = 176usize;
    for dp in 1..=nports {
        if hoff + 32 > 0xF00 { break; }
        let _ = control(dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle,
            0x23, 3, 8, dp as u32, 0, 0); // Set_Feature(PORT_POWER = 8)
        hoff += 32;
    }
    // For each CONNECTED downstream port: reset it, read its speed, Address Device it with a route
    // string (this hub port, tier 1) + parent-TT into its OWN slice, then read its config and bind it
    // if it's a boot HID - exactly like a root-port device (read_config_and_bind). This is what makes
    // the back-port keyboard work.
    let ndev_before = *ndev;
    for dp in 1..=nports {
        if *ndev >= MAX_HID { break; }
        if hoff + 48 > 0xD00 { break; }
        let ok = control(dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle,
            0xA3, 0, 0, dp as u32, 4, DATA_BUF_OFF); // Get_Status(port)
        hoff += 48;
        let st = if ok { dma.read16(DATA_BUF_OFF) } else { 0 };
        if st & 1 == 0 {
            continue; // nothing connected on this downstream port
        }
        // Reset the port, hold, clear the reset-change, re-read status for the device speed.
        let _ = control(dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle,
            0x23, 3, 4, dp as u32, 0, 0); // Set_Feature(PORT_RESET = 4)
        hoff += 32;
        let t0 = ctx.read_tsc();
        while ctx.read_tsc().wrapping_sub(t0) < 100_000_000 {} // ~50-65 ms reset hold (bounded)
        let _ = control(dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle,
            0x23, 1, 0x14, dp as u32, 0, 0); // Clear_Feature(C_PORT_RESET = 20)
        hoff += 32;
        let _ = control(dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle,
            0xA3, 0, 0, dp as u32, 4, DATA_BUF_OFF); // Get_Status again (post-reset, for speed)
        hoff += 48;
        let pst = dma.read16(DATA_BUF_OFF);
        // Port-status speed bits: bit 9 = low-speed, bit 10 = high-speed; neither = full-speed.
        // Map to the xHCI slot-context speed value (1=Full, 2=Low, 3=High).
        let dspeed = if pst & (1 << 9) != 0 { 2 } else if pst & (1 << 10) != 0 { 3 } else { 1 };
        // The downstream device gets its OWN slice.
        let d_idx = match sa.alloc() {
            Some(i) => i,
            None => {
                ctx.log("xhci: out of DMA slices for a downstream device - stopping hub walk");
                break;
            }
        };
        match address_downstream(
            ctx, dma, mmio, dboff, ir0, ctx_size, d_idx, dp as u32 & 0xF, port, dspeed, slot,
            dp as u32, ttt, ev_idx, ev_cycle, cmd_idx,
        ) {
            Some((dslot, vid, pid, cls)) => {
                ctx.log_fmt(format_args!(
                    "xhci: hub port {} DEVICE: VID={:#06x} PID={:#06x} class={:#04x} speed={} (slot {})",
                    dp, vid, pid, cls, dspeed, dslot
                ));
                if cls == 0x09 {
                    // A hub behind a hub (tier 2). Single-tier support for now - release it loudly.
                    ctx.log("xhci: downstream device is a hub (tier 2) - not recursing");
                    sa.free(d_idx);
                    disable_slot(ctx, dma, mmio, dboff, ir0, dslot, ev_idx, ev_cycle, cmd_idx);
                    continue;
                }
                // Bind it exactly like a root-port HID, but with the route string + parent-TT so its
                // slot context keeps routing through the hub.
                let (dbound, _) = read_config_and_bind(
                    ctx, dma, mmio, dboff, ir0, ctx_size, dslot, d_idx, dspeed, port,
                    dp as u32 & 0xF, port, slot, dp as u32, ttt, ev_idx, ev_cycle, cmd_idx,
                );
                match dbound {
                    Some(mut hid) => {
                        ctx.log_fmt(format_args!(
                            "xhci: hub port {} HID bound (slot {}) - back-port device now live", dp, dslot
                        ));
                        // Record the parent hub so the poll loop can GET_STATUS this hub port to notice
                        // the device unplugged (no root PORTSC reflects a device leaving behind a hub).
                        hid.hub_slot = slot;
                        hid.hub_dev = dev_idx as u32;
                        hid.hub_port = dp as u32;
                        devs[*ndev] = hid; // room checked at loop top
                        *ndev += 1;
                    }
                    None => {
                        // Connected but not a boot HID (or bind failed) - release its slice + slot.
                        sa.free(d_idx);
                        disable_slot(ctx, dma, mmio, dboff, ir0, dslot, ev_idx, ev_cycle, cmd_idx);
                    }
                }
            }
            None => {
                ctx.log_fmt(format_args!(
                    "xhci: hub port {} connected but downstream Address Device FAILED (route/TT)", dp
                ));
                sa.free(d_idx);
            }
        }
    }
    if *ndev == ndev_before {
        // Nothing usable behind this hub - it need not stay configured. Free its slice + slot so a
        // later hub/device can use them (bounded slice pool).
        ctx.log_fmt(format_args!("xhci: hub on port {} had no bound HID behind it - releasing", port));
        sa.free(dev_idx);
        disable_slot(ctx, dma, mmio, dboff, ir0, slot, ev_idx, ev_cycle, cmd_idx);
    } else {
        // A device was bound behind this hub; the hub's slice is KEPT. Seed each such device's poll
        // cursor to just past all the enumeration TRBs we wrote on the hub's EP0 ring (`hoff`), so the
        // poll loop's downstream-port GET_STATUS resumes exactly where the controller's dequeue sits.
        for d in 0..*ndev {
            if devs[d].hub_slot == slot {
                devs[d].hub_off = hoff;
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("xhci: driver starting");

    let mmio = match ctx.xhci_mmio() {
        Some(m) => m,
        None => {
            ctx.log("xhci: no controller MMIO granted - idling");
            idle(&ctx);
        }
    };
    let dma = match ctx.dma_region() {
        Some(d) => d,
        None => {
            ctx.log("xhci: no DMA arena granted - idling");
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

    // Fix 2 (Item 3): learn which root ports are USB3 (SuperSpeed) companions from the Supported Protocol
    // Capability, and skip them during enumeration. Boot HID (keyboard/mouse) is reached through the USB2
    // ports; the SuperSpeed companions carry nothing the boot path needs and the SS Address Device path
    // does not yet complete (Fix 1's log showed "no completion" with the HC still healthy). Skipping them
    // removes those doomed commands and the event-ring churn around them. 0 = no xECP -> enumerate all
    // ports as before (e.g. an old controller or QEMU without a USB3 protocol range).
    let usb3_ports = usb3_port_mask(&mmio, hcc1, max_ports);
    ctx.log_fmt(format_args!(
        "xhci: USB3 (SuperSpeed) companion root-port mask={:#x} - enumerating USB2 ports only (boot HID is USB2)",
        usb3_ports
    ));

    // Hot-plug state that persists across passes.
    let mut announce = false;    // suppress the connect line for the boot device
    let mut signaled = false;    // signal_input_ready (boot-screen clear) exactly once
    let mut prev_ports = 0u32;   // root-hub ports bound on the previous pass
    let mut rescan_noted = false; // "periodic back-port re-scan" logged once per idle spell

    // Hot-plug loop. Each pass FULLY re-initializes the controller (stop, reset,
    // rebuild the command/event rings + DCBAA, run) so every (re)enumeration starts
    // from pristine state - no stale completion events or slots can survive an
    // unplug/replug to desync the rings. Then it (re)scans every port, binds up to
    // MAX_HID HID devices (keyboard + mouse), and polls all of them until ANY of
    // them is unplugged (root-port CCS drops); on a drop it announces and loops,
    // re-binding whatever remains. Per-pass re-init is heavy, but hot-plug is
    // infrequent and it keeps the ring bookkeeping trivially correct (§26.12).
    //
    // Ports that WEDGE the HC during enumeration (Item 3, Fix 1) are recorded here, OUTSIDE the loop so
    // the record survives the re-init: a wedged port is poisoned and skipped on every subsequent pass,
    // so one bad port (e.g. an unenumerable SuperSpeed companion) can neither halt the controller under
    // an already-bound keyboard nor livelock the re-init. One bit per root port (max_ports <= 63 on real
    // HW; a port >= 64 simply is not poison-tracked, never poison-halted).
    let mut poisoned: u64 = 0;
    'reenum: loop {
        // Stop + reset the controller. The Wyse `chaos max-carnage` all-core freeze lands
        // DETERMINISTICALLY in this sequence (the log dies right after the "v..." line above), so bracket
        // every step with a log: the last line printed before a freeze is then the exact MMIO that hung.
        // A controller left running by the kill (the kernel only clears bus-master, it does not halt the
        // HC) may be in a state where a register access stalls the PCI bus.
        let cmd = mmio.read32(op + OP_USBCMD);
        let sts0 = mmio.read32(op + OP_USBSTS);
        // A read of all-1s means the controller is not answering MMIO at all (master-abort / dead BAR) -
        // proceeding would only poke a dead device. Report it LOUD instead of silently spinning (§26.7).
        if cmd == 0xFFFF_FFFF || sts0 == 0xFFFF_FFFF {
            ctx.log_fmt(format_args!(
                "xhci: reset: CONTROLLER NOT RESPONDING (USBCMD={:#010x} USBSTS={:#010x}, all-1s = dead BAR)",
                cmd, sts0
            ));
        }
        ctx.log_fmt(format_args!("xhci: reset: entering (USBCMD={:#010x} USBSTS={:#010x})", cmd, sts0));
        mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
        spin(|| mmio.read32(op + OP_USBSTS) & STS_HCH != 0);
        ctx.log_fmt(format_args!("xhci: reset: halted (USBSTS={:#010x})", mmio.read32(op + OP_USBSTS)));
        mmio.write32(op + OP_USBCMD, CMD_HCRST);
        // xHCI 5.4.1/5.4.2: after HCRST the controller asserts CNR (Controller Not Ready) and software
        // must NOT access any Operational register - INCLUDING USBCMD - until CNR clears; only USBSTS is
        // safe to read while the HC is not ready. The old poll read USBCMD FIRST each iteration (an
        // Operational-register access mid-reset), which on the Wyse's Goldmont+ HC intermittently WEDGED
        // THE PCI BUS - freezing every core (the `chaos max-carnage` all-core hang, pinpointed to exactly
        // here: the log died between "halted" and "done"). A userspace driver must never be able to lock
        // the platform; touching a register the HC forbids mid-reset was ours to fix. So: settle briefly
        // (let CNR assert), wait for CNR to clear reading ONLY USBSTS, THEN read USBCMD to confirm HCRST
        // cleared - USBCMD is never touched while the controller is not ready.
        let t0 = ctx.read_tsc();
        while ctx.read_tsc().wrapping_sub(t0) < 2_000_000 {} // ~1-2 ms settle (bounded; TSC always runs)
        spin(|| mmio.read32(op + OP_USBSTS) & STS_CNR == 0);
        spin(|| mmio.read32(op + OP_USBCMD) & CMD_HCRST == 0);
        ctx.log_fmt(format_args!(
            "xhci: reset: done (USBCMD={:#010x} USBSTS={:#010x})",
            mmio.read32(op + OP_USBCMD), mmio.read32(op + OP_USBSTS)
        ));
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
        // loop still runs and acks (clears IMAN.IP) - belt-and-suspenders until P4.
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
        // (keyboard + mouse) into per-device DMA slices from a fresh allocator. Each
        // port's device is bound directly, or - if it is a hub - its downstream ports
        // are walked and any keyboard/mouse behind it bound (the Wyse back-port path).
        // Non-HID devices (the mass-storage boot drive) and empty hubs release their
        // slice + slot, so nothing leaks across this pass.
        let mut devs = [Hid {
            slot: 0, dci: 0, port: 0, idx: 0, is_mouse: false,
            hub_slot: 0, hub_dev: 0, hub_port: 0, hub_off: 0,
        }; MAX_HID];
        let mut ndev = 0usize;
        let mut sa = SliceAlloc::new();
        let mut saw_hub = false;
        let mut hc_wedged = false;
        for p in 1..=max_ports {
            if ndev >= MAX_HID { break; }
            // Skip USB3 (SuperSpeed) companion ports (Item 3, Fix 2): boot HID is reached through the USB2
            // ports, and the SS Address Device path does not yet complete - enumerating them only issues
            // doomed commands and churns the shared event ring.
            if p < 64 && usb3_ports & (1u64 << p) != 0 { continue; }
            // Skip a port that WEDGED the HC on a previous pass (Item 3, Fix 1). Enumerating it again
            // would just re-halt the controller and take the keyboard down with it; the working devices
            // (e.g. the keyboard's hub on a lower port) enumerate first and stay bound. Bounded: at most
            // one re-init per poisoning port, then the pass runs to completion with them skipped.
            if p < 64 && poisoned & (1u64 << p) != 0 { continue; }
            enumerate_one(
                &ctx, &dma, &mmio, dboff, ir0, op, ctx_size,
                p, &mut sa, &mut devs, &mut ndev, &mut saw_hub,
                &mut ev_idx, &mut ev_cycle, &mut cmd_idx, &mut hc_wedged,
            );
            if hc_wedged {
                ctx.log_fmt(format_args!(
                    "xhci: port {} wedged the HC - poisoning it and re-initialising the controller", p
                ));
                if p < 64 { poisoned |= 1u64 << p; }
                continue 'reenum;
            }
        }

        if ndev == 0 {
            // Nothing usable attached. Still report input-ready once so the shell's
            // boot-screen clear fires (the keyboard may be on the other controller).
            if !signaled { ctx.signal_input_ready(); signaled = true; }
            // Nothing is bound, so forget the previous pass's bound ports: whatever binds next is a
            // genuine new plug and must announce. Without this, a device BEHIND a hub keeps the hub's
            // (unchanging) root-port bit in prev_ports across its own unplug, so a replug re-binding on
            // that same root port looked "already present" and the "keyboard connected" notice was
            // suppressed (the root-port key can't tell one back-port device from another).
            prev_ports = 0;
            if saw_hub {
                // A hub is present but empty. A device (re)plugged BEHIND a hub changes no root PORTSC,
                // so a root-port wait would never see it - re-walk the hub after a bounded pause so a
                // back-port keyboard connect/reconnect is caught. But still break EARLY on a fresh
                // root-port device, so a FRONT-port plug is instant (the Wyse's internal hubs are always
                // present, so this branch is where the driver idles with no keyboard). Logged once so an
                // always-present hub does not spam while idle.
                if !rescan_noted {
                    ctx.log("xhci: hub present but no HID behind it - periodic back-port re-scan");
                    rescan_noted = true;
                }
                let mut base_ports = 0u32;
                for p in 1..=max_ports {
                    if mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0 {
                        base_ports |= 1 << p;
                    }
                }
                let t0 = ctx.read_tsc();
                loop {
                    while ctx.try_recv().is_some() {}
                    let mut new_root = false;
                    for p in 1..=max_ports {
                        let c = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0;
                        if c && base_ports & (1 << p) == 0 { new_root = true; break; }
                        if !c { base_ports &= !(1 << p); }
                    }
                    if new_root { break; } // a front/root-port device appeared - re-walk now
                    if ctx.read_tsc().wrapping_sub(t0) >= HUB_RESCAN_CYCLES { break; } // periodic re-walk
                    ctx.yield_cpu();
                }
                announce = true; // whatever we bind on the re-walk is a real plug event
                continue 'reenum;
            }
            ctx.log("xhci: no HID keyboard/mouse on any port - waiting for a connection");
            wait_for_port(&ctx, &mmio, op, max_ports);
            announce = true; // whatever connects now is a real plug event
            continue 'reenum;
        }
        rescan_noted = false; // a device is bound; re-arm the once-only re-scan log for next time

        ctx.log_fmt(format_args!("xhci: {} HID device(s) bound", ndev));
        if !signaled { ctx.signal_input_ready(); signaled = true; } // boot-screen clear, once
        // Announce only devices that weren't already bound on the previous pass. A
        // hot-plug re-initializes the whole controller and re-binds EVERY surviving
        // device, but a device whose port was bound last pass wasn't physically
        // touched - announcing it again ("keyboard connected" when only the mouse
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
        // Per-device producer cursor into its parent HUB's EP0 ring, for the throttled downstream-port
        // GET_STATUS that detects a device unplugged behind a hub. Seeded past the enumeration TRBs
        // (hub_off); pcs starts at 1 (enumeration never wrapped the ring). Unused for root devices.
        let mut hub_cur = [0usize; MAX_HID];
        let mut hub_pcs = [1u32; MAX_HID];
        for d in 0..ndev { hub_cur[d] = devs[d].hub_off; }
        let mut last_hub_poll = ctx.read_tsc();
        let mut hub_probe_logged = false; // log the first downstream-status probe per session (diagnostic)
        let mut kb_last = [[0u8; 6]; MAX_HID];
        let mut kb_rep = [
            godspeed_sdk::hid::KeyRepeat::new(REPEAT_INITIAL_CYCLES, REPEAT_INTERVAL_CYCLES),
            godspeed_sdk::hid::KeyRepeat::new(REPEAT_INITIAL_CYCLES, REPEAT_INTERVAL_CYCLES),
        ];
        let mut kb_caps = [false; MAX_HID]; // Caps Lock latch per keyboard (host-tracked toggle)
        let mut mouse = [
            godspeed_sdk::hid::MouseTracker::new(),
            godspeed_sdk::hid::MouseTracker::new(),
        ];
        // Snapshot every connected root-hub port at poll start: the bound HID
        // devices, plus any non-HID device (e.g. a thumbdrive). A genuinely NEW
        // connection - a port NOT in this set becoming connected - triggers a
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
        'poll: loop {
            // (Re-)arm each device's interrupt ring as needed, BEFORE blocking - so a fresh
            // HID report can post a transfer event (→ MSI-X) that wakes us.
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

            // BUSY-POLL (§12). The CPU-reduction experiment (block on recv_timeout to idle the
            // core, interrupt/timer to wake) introduced subtle quirks on this hardware - input
            // lag, sluggish auto-repeat, hot-plug wedges - so we scaled it back to the model that
            // worked flawlessly: yield each pass and re-scan. The MSI-X interrupt is still
            // enabled and drained below (belt-and-suspenders), it just doesn't gate the loop.
            // The core runs hot; reclaiming that idle cleanly is deferred (revisit later).
            ctx.yield_cpu();
            // Drain any queued interrupt-event IPCs (kept so an enabled MSI-X can't pile up).
            while ctx.try_recv().is_some() {}
            // Ack the interrupter (clear IP, keep IE) BEFORE draining the ring, so an event
            // arriving mid-drain re-sets IP and re-arms a fresh MSI-X (no missed events).
            mmio.write32(ir0 + 0x00, IMAN_IE | IMAN_IP);

            // Drain ALL pending events. Transfer events → decode HID; other events (port
            // status change, etc.) are dequeued and ignored (hot-plug is handled by the
            // PORTSC checks below). next_event advances ERDP, which clears EHB.
            loop {
                match next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle, 1) {
                    Some((TRB_TRANSFER_EVENT, _, slot_id)) => {
                        if let Some(d) = devs[..ndev].iter().position(|h| h.slot == slot_id) {
                            let dev = devs[d].idx;
                            let mut rep = [0u8; 8];
                            for (j, b) in rep.iter_mut().enumerate() {
                                *b = dma.read8(report_off(dev) + j);
                            }
                            // Skip an all-0xff report - a failed/stale DMA read from a device
                            // that vanished mid-transaction (e.g. a rapid unplug/replug). Decoding
                            // it would push 0xff "keystrokes" to the console; the real disconnect
                            // is caught by the PORTSC CCS check below, which re-initialises.
                            if !godspeed_sdk::hid::report_is_valid(&rep) {
                                need_queue[d] = true;
                                continue;
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
                                // Ctrl+Alt+Del = secure-attention reboot, from any context. Checked
                                // only for keyboard reports (a mouse button byte can alias the
                                // modifier bits). reboot() does not return.
                                if godspeed_sdk::hid::is_ctrl_alt_del(&rep) {
                                    ctx.log("xhci: Ctrl+Alt+Del - rebooting");
                                    ctx.reboot();
                                }
                                godspeed_sdk::hid::decode_keyboard(
                                    &rep, &mut kb_last[d], &mut kb_rep[d], &mut kb_caps[d], ctx.read_tsc(),
                                    |ch| ctx.console_push(ch),
                                    |code| ctx.log_fmt(format_args!(
                                        "xhci: unmapped HID key usage {:#04x} (add to sdk hid_to_ascii)", code)));
                            }
                            need_queue[d] = true;
                        }
                    }
                    Some(_) => {} // non-transfer event (port change, command, etc.) - drained
                    None => break,
                }
            }

            // Unplug detection. A device DIRECTLY on a root port is gone when its root-port CCS drops
            // (cheap MMIO read, every pass). A device BEHIND a hub changes no root PORTSC when it
            // leaves - its root port is the hub's, and the hub stays put - so it is instead detected by
            // GET_STATUSing the hub's downstream port, throttled (a control transfer, not free). Either
            // way: notify and break to fully re-initialize, re-binding whatever remains next pass.
            let hub_due = ctx.read_tsc().wrapping_sub(last_hub_poll) > HUB_POLL_CYCLES;
            let mut eaten = 0u32; // HID slots whose events a hub check consumed (re-arm them below)
            for d in 0..ndev {
                let gone = if devs[d].hub_slot == 0 {
                    let portsc_off = op + OP_PORTSC_BASE + (devs[d].port as usize - 1) * 0x10;
                    mmio.read32(portsc_off) & PORT_CCS == 0
                } else if hub_due {
                    // Some(false) = the hub says its port is empty now; Some(true)/None = still there
                    // or an inconclusive read (do not false-notify on a transient control failure).
                    let st = hub_port_status(
                        &dma, &mmio, dboff, ir0,
                        devs[d].hub_slot, devs[d].hub_dev as usize, devs[d].hub_port,
                        &mut hub_cur[d], &mut hub_pcs[d], &mut ev_idx, &mut ev_cycle, &mut eaten,
                    );
                    // Log the first probe of the session (and any inconclusive None, which would
                    // silently disable this detection) so the hub-poll path is diagnosable on hardware.
                    if !hub_probe_logged || st.is_none() {
                        ctx.log_fmt(format_args!(
                            "xhci: hub slot {} port {} status probe -> {:?}",
                            devs[d].hub_slot, devs[d].hub_port, st
                        ));
                        hub_probe_logged = true;
                    }
                    matches!(st, Some(false))
                } else {
                    false
                };
                if gone {
                    notify(&ctx, if devs[d].is_mouse {
                        "mouse disconnected (xhci)"
                    } else {
                        "keyboard disconnected (xhci)"
                    });
                    announce = true;
                    break 'poll;
                }
            }
            if hub_due { last_hub_poll = ctx.read_tsc(); }
            // Re-arm any HID endpoint whose in-flight report a hub check just consumed, so it does not
            // stall (the report is discarded; the next keystroke lands on the fresh TRB).
            if eaten != 0 {
                for k in 0..ndev {
                    if devs[k].slot < 32 && eaten & (1 << devs[k].slot) != 0 {
                        need_queue[k] = true;
                    }
                }
            }
            // New-device detection: while we still have a free device slice, a port
            // that was NOT connected at poll start becoming connected is a fresh
            // plug - break and re-enumerate to bind it alongside the existing
            // device(s). Tracks port leaves so a re-plug into the same port counts.
            if ndev < MAX_HID {
                for p in 1..=max_ports {
                    let c = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0;
                    if c && present & (1 << p) == 0 {
                        ctx.log_fmt(format_args!("xhci: new device on port {} - re-enumerating", p));
                        announce = true;
                        break 'poll;
                    }
                    if !c { present &= !(1 << p); }
                }
            }
            // Typematic auto-repeat: a held key sends no further USB reports, so synthesise
            // repeats from the TSC cycle counter. While a key is held we woke on the timer
            // (short timeout above), so this fires the repeats at ~the repeat interval.
            let now = ctx.read_tsc();
            for d in 0..ndev {
                if !devs[d].is_mouse {
                    kb_rep[d].poll(now, |ch| ctx.console_push(ch));
                }
            }
        }
    } // end 'reenum loop
}
