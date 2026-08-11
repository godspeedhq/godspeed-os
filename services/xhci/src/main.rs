// SPDX-License-Identifier: GPL-2.0-only
//! `xhci` - USB host-controller driver (§12). Multi-HID: enumerates EVERY
//! connected port and binds up to `MAX_HID` boot-protocol HID devices (a
//! keyboard AND a mouse) on the SAME controller at once, then polls all of them
//! from one loop, demultiplexing transfer events by slot id. All hardware access
//! is via the SDK's audited Mmio / Dma wrappers (§18); no `unsafe` here.

#![no_std]
#![no_main]

use godspeed_sdk::{Dma, Mmio, ServiceContext};

/// USB mass storage (Bulk-Only Transport + SCSI). Split out because it is a self-contained protocol
/// on top of the bulk endpoints this file configures - and because it is the capability whose absence
/// kept a USB stack in the kernel.
mod msc;

/// Shadow topology model - observation only, see docs/xhci-topology.md.
mod topo;

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
const IMAN_IE: u32 = 1 << 1; // Interrupter 0 Management: Interrupt Enable
const IMAN_IP: u32 = 1 << 0; // Interrupter 0 Management: Interrupt Pending (write 1 to clear)
const CMD_HCRST: u32 = 1 << 1;
const STS_HCH: u32 = 1 << 0; // USBSTS: Host Controller Halted
const STS_HSE: u32 = 1 << 2; // USBSTS: Host System Error (a DMA/system error halted the HC)
const STS_CNR: u32 = 1 << 11;
const STS_HCE: u32 = 1 << 12; // USBSTS: Host Controller Error (internal fatal error)
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
const INPUT_CTX_OFF: usize = 0x4000; // transient: built per device for Address/Configure
const DATA_BUF_OFF: usize = 0x5000; // transient: control-transfer data during enumeration
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
const MAX_SLICES: usize = 6; // per-device DMA slices (bound HIDs + transient hub/enum)
const SCRATCHPAD_SBA_OFF: usize = 0x1F000; // = DEV_BASE + MAX_SLICES*DEV_STRIDE (0x7000 + 6*0x4000)
const SCRATCHPAD_BUF_BASE: usize = 0x20000; // = SCRATCHPAD_SBA_OFF + 0x1000
const MAX_SCRATCHPAD: usize = 256; // arena room = XHCI_DMA_PAGES (288) - 32

/// Maximum HID devices bound on one controller at once (keyboard + mouse).
const MAX_HID: usize = 2;

// Typematic auto-repeat delays are CALIBRATED per-machine from the TSC rate at the keyboard-poll
// setup (`KeyRepeat::new_calibrated(ctx.tsc_ticks_per_10ms())`), not hardcoded here: assuming ~2 GHz
// made one keypress repeat into `qqqqq` on the differently-clocked Goldmont+ Wyse. read_tsc is
// hardware-proven to advance (perf §22); tsc_ticks_per_10ms is the kernel's PIT-calibrated rate.
// The four timing budgets that used to live here are now MILLISECONDS, next to `spin` below, because
// the cycle counts they were could not survive leaving x86. See `RESET_RECOVERY_MS` and friends.
const DEV_BASE: usize = 0x7000;
const DEV_STRIDE: usize = 0x4000; // 4 pages: device ctx, EP0 ring, int ring, report
fn device_ctx_off(i: usize) -> usize {
    DEV_BASE + i * DEV_STRIDE
}
fn ep0_tr_off(i: usize) -> usize {
    DEV_BASE + i * DEV_STRIDE + 0x1000
}
fn int_tr_off(i: usize) -> usize {
    DEV_BASE + i * DEV_STRIDE + 0x2000
}
fn report_off(i: usize) -> usize {
    DEV_BASE + i * DEV_STRIDE + 0x3000
}

/// Fixed pool of the MAX_SLICES per-device DMA slices (§26.6, no heap - a bitmap). A bound HID and an
/// in-use hub HOLD their slice for the enumeration pass (a downstream device's routing depends on its
/// hub's device context staying put); a transient probe - a non-HID device - frees it. Each hot-plug
/// pass re-inits the controller and starts a fresh allocator, so nothing leaks across passes.
struct SliceAlloc {
    used: [bool; MAX_SLICES],
}
impl SliceAlloc {
    fn new() -> Self {
        Self {
            used: [false; MAX_SLICES],
        }
    }
    fn alloc(&mut self) -> Option<usize> {
        (0..MAX_SLICES)
            .find(|&i| !self.used[i])
            .inspect(|&i| self.used[i] = true)
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    slot: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) {
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let _ = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        0,
        0,
        0,
        (TRB_DISABLE_SLOT << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
    );
}

/// Decode ONE completed HID report for device `d` and push it to the console.
///
/// Extracted so that BOTH paths that observe a completed report can deliver it. The poll loop always
/// did. The other path - a hub port probe that consumes a keystroke completion while waiting for its
/// own event - re-armed the endpoint and threw the report away, with a comment calling it "a rare
/// dropped keystroke". It stopped being rare: probes run every `HUB_POLL_MS` across every hub port,
/// and a FAILING probe holds its window open for the whole `PROBE_ANSWER_MS`, so a real amount of
/// typing landed in those windows and was discarded. That is the dropped keys and the lag - a key that
/// never arrives is also a key that seems to arrive late, when the next one finally re-triggers.
///
/// The report was never lost, only unread: the transfer completed into the device's DMA buffer, so
/// this reads exactly what the poll loop would have read.
#[allow(clippy::too_many_arguments)]
fn deliver_hid_report(
    ctx: &ServiceContext,
    dma: &Dma,
    d: usize,
    devs: &[Hid; MAX_HID],
    kb_last: &mut [[u8; 6]; MAX_HID],
    kb_rep: &mut [godspeed_sdk::hid::KeyRepeat; MAX_HID],
    kb_caps: &mut [bool; MAX_HID],
    mouse: &mut [godspeed_sdk::hid::MouseTracker; MAX_HID],
) {
    let dev = devs[d].idx;
    let mut rep = [0u8; 8];
    for (j, b) in rep.iter_mut().enumerate() {
        *b = dma.read8(report_off(dev) + j);
    }
    // An all-0xff report is a failed/stale DMA read from a device that vanished mid-transaction;
    // decoding it would push 0xff "keystrokes". The real disconnect is caught by the CCS check.
    if !godspeed_sdk::hid::report_is_valid(&rep) {
        return;
    }
    if devs[d].is_mouse {
        mouse[d].feed(&rep, |_mask, _down| {}, |_dx, _dy| {});
    } else if godspeed_sdk::hid::is_ctrl_alt_del(&rep) {
        // SEC-2: this driver holds no REBOOT. It SIGNALS the chord; the shell decides (§6.4).
        ctx.console_push(godspeed_sdk::hid::CTRL_ALT_DEL_SIGNAL);
    } else {
        godspeed_sdk::hid::decode_keyboard(
            &rep,
            &mut kb_last[d],
            &mut kb_rep[d],
            &mut kb_caps[d],
            ctx.read_tsc(),
            |ch| ctx.console_push(ch),
            |code| {
                ctx.log_fmt(format_args!(
                    "xhci: unmapped HID key usage {:#04x} (add to sdk hid_to_ascii)", code))
            },
        );
    }
}

/// Clear a HALTED endpoint and tell the controller where to resume: Reset Endpoint, then Set TR
/// Dequeue Pointer (xHCI 4.6.8 + 4.6.10). Returns true if both commands succeeded.
///
/// This is the repair for the state the hardware kept reaching: an errored transfer leaves the
/// endpoint HALTED, the controller stops executing its ring, and every later probe just adds TRBs
/// nothing will run - `cur` climbing while `ev_idx` stays frozen, which is what the log showed. The
/// endpoint never recovers on its own, so the keyboard behind it stayed dead until a reboot.
///
/// Both commands are required and in this order. Reset Endpoint clears the halt but leaves the
/// dequeue pointer wherever it stopped, which is mid-ring and pointing at TRBs the controller already
/// skipped; Set TR Dequeue is what makes the ring coherent again. The caller must reset ITS cursor to
/// match the pointer set here, or the two disagree about where the ring starts and it wedges again.
#[allow(clippy::too_many_arguments)]
fn reset_endpoint(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    slot: u32,
    dci: u32,
    ring_off: usize,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> bool {
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let cc = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off, 0, 0, 0,
        (TRB_RESET_ENDPOINT << 10) | (slot << 24) | (dci << 16) | 1,
        ev_idx, ev_cycle,
    );
    // `run_command` yields (completion_code, slot); 1 = Success.
    if !matches!(cc, Some((1, _))) {
        ctx.log_fmt(format_args!(
            "xhci: Reset Endpoint slot {} dci {} FAILED (cc {:?}) - endpoint stays halted",
            slot, dci, cc));
        return false;
    }
    // Resume at the ring BASE with Dequeue Cycle State = 1, the state a fresh ring is in. The caller
    // resets its producer cursor to the same place, so both sides agree again.
    let bp = dma.phys_at(ring_off);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let cc = run_command(
        ctx, dma, mmio, dboff, ir0, cmd_off,
        (bp as u32) | 1,          // low 32 bits | DCS=1
        (bp >> 32) as u32,
        0,
        (TRB_SET_TR_DEQUEUE << 10) | (slot << 24) | (dci << 16) | 1,
        ev_idx, ev_cycle,
    );
    if !matches!(cc, Some((1, _))) {
        ctx.log_fmt(format_args!(
            "xhci: Set TR Dequeue slot {} dci {} FAILED (cc {:?}) - ring left incoherent",
            slot, dci, cc));
        return false;
    }
    ctx.log_fmt(format_args!(
        "xhci: endpoint slot {} dci {} RESET and dequeue re-pointed to the ring base - recovered",
        slot, dci));
    true
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
    hub_nports: u32, // parent hub's downstream port count (0 = root device); for the new-device re-scan
}

/// A position signature unique per PHYSICAL device location, for telling a genuinely new plug from a
/// survivor the re-init merely re-bound. A root device keys on its root port; a device behind a hub also
/// keys on (hub slot, hub downstream port), so two devices behind the SAME hub (a keyboard on hub port 3,
/// a mouse on hub port 4) are DISTINCT - and a replug of one is "new" even though the other keeps the
/// shared root port bound. Keying on the root port alone (the old prev_ports bitmask) suppressed the
/// "connected" notice for every behind-hub replug, since the hub's root port never leaves.
fn dev_sig(d: &Hid) -> u32 {
    ((d.hub_slot & 0xFF) << 16) | ((d.hub_port & 0xFF) << 8) | (d.port & 0xFF)
}

const EVENT_RING_TRBS: usize = 16;
pub(crate) const TRB_SIZE: usize = 16;

pub(crate) const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
pub(crate) const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
/// Reset Endpoint (xHCI 4.6.8) - clears the HALTED state an errored transfer left on an endpoint.
/// Consecutive hub-probe failures across ALL ports of a hub. A halt is a property of the shared EP0
/// endpoint, so every port's probe fails together - which makes any of them evidence, and makes the
/// count meaningful only when unbroken (a single success resets it).
static PROBE_FAILS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
const TRB_RESET_ENDPOINT: u32 = 14;
/// Set TR Dequeue Pointer (xHCI 4.6.10) - tells the controller where to resume on that endpoint.
const TRB_SET_TR_DEQUEUE: u32 = 16;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
pub(crate) const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

/// Wait for a controller register to reach a state, with a bound and a NAME.
///
/// Two things were wrong with the bare `while !cond() && n < 5_000_000` this replaces, and only one
/// of them was a portability problem.
///
/// The portability one: **a count is not a duration** (§26.6). Five million iterations of an MMIO
/// read is a few hundred milliseconds on an x86 machine whose uncached reads are ~50 ns, and several
/// SECONDS on a board reaching a PCIe endpoint across a root complex. The same literal cannot mean
/// both, so the bound is expressed in milliseconds and converted by the machine's own calibration.
///
/// The other one applies everywhere and was always a bug: expiry was **silent**. A controller that
/// never cleared `CNR` produced no line at all, and the next step ran anyway on a device that was
/// not ready - a failure discovered several registers later, as nonsense. §26.7 asks for the
/// opposite, so expiry now says which wait gave up. `what` is the register condition in words; a
/// caller passing something vague is passing up the whole point.
fn spin<F: Fn() -> bool>(ctx: &ServiceContext, what: &str, ms: u64, cond: F) -> bool {
    let budget = ctx.duration_cycles(ms);
    let t0 = ctx.read_tsc();
    while !cond() {
        if ctx.read_tsc().wrapping_sub(t0) >= budget {
            ctx.log_fmt(format_args!(
                "xhci: TIMEOUT after ~{} ms waiting for {} - the controller did not answer",
                ms, what
            ));
            return false;
        }
    }
    true
}

// Timing budgets, in milliseconds, converted per machine at the point of use.
//
// These were raw TSC-cycle literals chosen for a ~2 GHz x86 (`100_000_000` meaning "~50 ms"). A
// cycle is not a portable unit - the AArch64 generic timer runs at 54 MHz on a Pi 4, where that same
// literal asks for nearly two seconds and `HUB_RESCAN` asks for the better part of a minute. So the
// numbers below are DURATIONS and `ctx.duration_cycles` does the conversion through the kernel's own
// calibration, which is the portable path the SDK already documents for exactly this mistake.
//
/// Recovery hold after a root-port reset before addressing the device. USB 2.0 requires a
/// reset-recovery interval (TRSTRCY >= 10 ms) before a device can accept transactions; without it a
/// high-speed root-port device NAKs the Address Device SET_ADDRESS and returns a Transaction Error.
/// Matches the behind-a-hub reset hold.
const RESET_RECOVERY_MS: u64 = 55;
/// How often the poll loop GET_STATUSes a hub's downstream port to notice a device unplugged from
/// behind it (no root PORTSC reflects that). Responsive enough for a "keyboard disconnected" notice,
/// infrequent enough not to load the hub or eat keystrokes off the shared event ring - the check runs
/// a control transfer, and between checks the keyboard endpoint has the ring to itself.
/// How long to hold a hub's downstream port in reset. USB 2.0 asks for at least 10 ms; hubs and
/// devices vary, and being generous here costs one enumeration, not a running system.
const PORT_RESET_HOLD_MS: u64 = 60;
/// How long to let a hub's port power settle before resetting anything on it.
const PORT_POWER_SETTLE_MS: u64 = 200;
/// TRSTRCY - the recovery interval a device is owed AFTER its port reset completes, before it can be
/// addressed (USB 2.0 §7.1.7.5 gives 10 ms). 20 for margin: the cost is 20 ms per downstream port at
/// enumeration, and the alternative is a device that never enumerates at all.
const PORT_RECOVERY_MS: u64 = 20;

const HUB_POLL_MS: u64 = 500;
/// How often the driver says it is still alive. See the heartbeat's comment in the poll loop: this
/// exists because a STOPPED loop is otherwise indistinguishable from a quiet one, and every failure
/// detector here counts failures that a stopped loop never produces.
const HEARTBEAT_MS: u64 = 60_000;
/// How long the "a hub is present but nothing usable is behind it" wait sleeps before re-walking the
/// hub. A device replugged BEHIND a hub changes no root PORTSC, so the root-port wait would miss it.
/// Only runs while NO HID is bound.
const HUB_RESCAN_MS: u64 = 1_500;

/// How long a hub gets to answer a port-status probe - a REAL DURATION (see `hub_port_status`).
///
/// A hub answers GET_STATUS in about a millisecond. This is the bound for one that does not, and it
/// is deliberately small: the probe runs per port per pass, so this is a direct input-latency cost
/// whenever a hub is unresponsive.
const PROBE_ANSWER_MS: u64 = 50;

// 50, not 5, and the reason is scheduling rather than the device.
//
// 5 ms was chosen while this driver was IN THE KERNEL, where the only thing between posting a
// transfer and seeing its completion was the controller. As a userspace SERVICE it runs on a 10 ms
// quantum (§9.1), so a 5 ms wall-clock deadline can elapse ENTIRELY while the service is descheduled.
// It was measuring our scheduling, not the hardware: 832 failed probes in one idle session.
//
// That is why this matters beyond noise. A failed probe no longer invents a removal (correct), so a
// probe that cannot complete makes a REAL removal invisible - the reported "unplugged the stick, no
// INFO; unplugged the keyboard, no INFO, and it did not rebind". The absence rule was fixed; the
// thing it depends on was still broken.
//
// 50 ms is five quanta, so the answer survives being preempted a few times, and it is still a real
// duration that means the same on any board. The cost is bounded and paid only when a hub does not
// answer, since a healthy probe returns as soon as the event lands.

/// Idle-wait pacing for the paths that have NO device to service (`idle`, `wait_for_port`, the hub
/// re-walk). These used `yield_cpu`, which does not sleep - it pegs the core at ~100% forever, which
/// is exactly what showed up as ~85k scheduler quanta/s on the T630 (its keyboard is on ehci, so
/// xhci sits here permanently). `sleep` PARKS the task instead: the core can halt, and with the
/// Phase 2a idle-tick slowdown it can also stretch its timer.
///
/// This deliberately keeps the SELF-DRIVEN poll these loops were built around - we still `try_recv`
/// on our own schedule and need no cross-core wake, so the flood-storm drain property the previous
/// comments relied on is preserved (a deeply-blocked `recv` on an AP was the unreliable part, and we
/// still never do that). Granularity is one scheduler quantum, so this value only sets a floor.
const IDLE_WAIT_MS: u64 = 5;

/// Wait until a port reports a *newly* connected device, then return so the caller
/// re-scans. Snapshots the ports already connected on entry (e.g. the USB boot
/// drive, which is always present and is not a HID) and only returns when a port
/// that was NOT connected becomes connected - otherwise an always-present non-HID
/// device would make the hot-plug loop spin (re-scan → not a keyboard → wait →
/// still connected → re-scan …).
fn wait_for_port(ctx: &ServiceContext, mmio: &Mmio, op: usize, max_ports: u32) {
    let connected =
        |p: u32| mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0;
    let mut base = 0u32;
    for p in 1..=max_ports {
        if connected(p) {
            base |= 1 << p;
        }
    }
    loop {
        for p in 1..=max_ports {
            let c = connected(p);
            if c && base & (1 << p) == 0 {
                return; // a new device appeared on port p
            }
            if !c {
                base &= !(1 << p);
            } // a known device left; its port can be reused
        }
        // Drain our IPC endpoint while we idle here. This wait - NOT the 'poll loop - is where a driver
        // with no HID attached lives, and 'poll is the only other place we drain. Without this, a chaos
        // flood-storm (or any stray send) fills our 16-deep queue and it sits at 16/16 FOREVER, exactly
        // the logger stub bug in another guise. try_recv is non-blocking, so the port poll is unaffected.
        while ctx.try_recv().is_some() {}
        ctx.sleep(ctx.duration_cycles(IDLE_WAIT_MS));
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
    loop {
        while ctx.try_recv().is_some() {}
        ctx.sleep(ctx.duration_cycles(IDLE_WAIT_MS));
    }
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
pub(crate) fn next_event(
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
        wr64(
            mmio,
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
    mmio.write32(dboff, 0); // command doorbell: DB Target must be 0 (the command ring), NOT a DCI like the slot doorbells (dboff + slot*4, target = endpoint DCI) elsewhere in this file

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

/// Clear the whole Input Context before building one.
///
/// `INPUT_CTX_OFF` is a SINGLE buffer reused for every device and every command. Each path writes
/// only the dwords it cares about, so everything it does not write keeps whatever the PREVIOUS
/// device left there - `islot+12`, the endpoint contexts' dword0, the tail of the Input Control
/// Context. The xHCI spec requires unused fields to be zero and the xHC VALIDATES them, so a stale
/// bit is not a harmless leftover: it is a Parameter Error (completion 17) attributed to the device
/// that inherited it.
///
/// The Pi 4 made this visible. A low-speed keyboard behind the internal hub failed Address Device
/// with completion 17 while a high-speed stick on the same hub succeeded - and the keyboard is
/// addressed AFTER the stick and the hub, so it is the one that inherits the most debris.
///
/// One Input Control Context + 32 endpoint contexts is the most any command here uses.
fn clear_input_ctx(dma: &Dma, ctx_size: usize) {
    for off in (0..(33 * ctx_size)).step_by(4) {
        dma.write32(INPUT_CTX_OFF + off, 0);
    }
}

/// EP0's Max Packet Size for a device's link speed, as the USB and xHCI specs REQUIRE it.
///
/// These are not preferences the driver gets to choose. USB 2.0 mandates 64 bytes for a high-speed
/// control endpoint and 8 for low-speed; SuperSpeed uses 512. The xHC VALIDATES the value in an
/// Address Device input context and answers an illegal one with **Parameter Error (completion 17)**.
///
/// **Full-speed starts at 8, not 64.** For FS the real value is 8, 16, 32 or 64 and is only
/// discoverable by READING the device descriptor, so xHCI 4.3.3 has software address the device with
/// 8 first and correct it afterwards. Putting 64 there is not a generous guess - it is an illegal
/// initial value the xHC answers with Parameter Error, the same completion 17 that a high-speed
/// device got from an EP0 of 8. Low-speed is always 8, high-speed always 64, SuperSpeed always 512;
/// full-speed is the one that is not simply a property of the link.
///
/// This exists as ONE function because it was three copies, and one of them had drifted: the
/// downstream (behind-a-hub) path said `_ => 8`, so every high-speed device behind the Pi 4's
/// internal VL817 hub - which is where its keyboard and its USB stick actually live - was addressed
/// with an illegal 8-byte EP0 and refused. The two root-port copies were correct, which is exactly
/// why it survived: the hub itself enumerated perfectly and only the devices behind it failed.
///
/// Commandment III: one truth, derived views. Three hand-maintained copies of a spec constant is
/// three chances to be wrong, and being wrong here is not a subtle degradation - it is a hard refusal
/// on the only devices a user actually plugs in.
fn ep0_max_packet(speed: u32) -> u32 {
    match speed {
        1 => 8,   // FULL-speed: see below - 8 INITIALLY, not 64
        2 => 8,   // low-speed: always 8
        4 => 512, // super-speed: always 512
        3 => 64,  // high-speed: always 64, mandated by USB 2.0
        _ => 8,   // unknown speed: the conservative value, never the largest
    }
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    hub_slot: u32,
    hub_dev: usize,
    hub_port: u32,
    cur: &mut usize,
    pcs: &mut u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    eaten: &mut u32,
    // Set when the probe gave up its own transfer to deliver someone else's completion. NOT a
    // failure: it is evidence of nothing, and must never feed an absence or wedge counter.
    abandoned: &mut bool,
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
        // A hub port-status probe that will FAIL should fail fast.
        //
        // This ran to 5_000_000 poll iterations, eight times over - and a probe against a port whose
        // device has been pulled fails EVERY time, every hub poll, on the loop that also polls the
        // keyboard. So an unplugged stick did not merely stop working, it made typing worse for as
        // long as it stayed unplugged, which is exactly the "keyboard gets laggy after a while" this
        // driver kept being reported for.
        //
        // 64k iterations is ample for a hub that is going to answer - it answers in the first few -
        // and cheap for one that is not. The removal is still detected; it just costs a fraction of
        // the time to conclude.
        // Wait on the CLOCK, not on an iteration count.
        //
        // This read `next_event(.., 65_536)`, and the comment above defended it: "64k iterations is
        // ample for a hub that is going to answer." That is the count-is-not-a-duration error, for
        // the seventh time on this port. 64k reads of DMA memory take however long 64k reads happen
        // to take - and that shrinks exactly when the keyboard's interrupt traffic is competing for
        // the same event ring. So the probe gave up BEFORE a transfer that was still in flight.
        //
        // The evidence is decisive: the boot with the stick already attached logged ZERO probe
        // failures, and the loaded run logged 1530. Same code, same board, different amount of
        // competing traffic. It also explains why `chaos max-carnage` fixed it - a re-init quiets
        // the ring long enough for 64k spins to once again outlast a 1 ms transfer.
        //
        // 5 ms is generous for a hub that answers in about one, and it is the same 5 ms on a fast
        // board and a slow one. It stays SHORT deliberately: this runs per port per pass, and a long
        // wait on a hub that will never answer is what made typing lag while the stick was out.
        let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(PROBE_ANSWER_MS));
        let mut ev;
        loop {
            ev = next_event(dma, mmio, ir0, ev_idx, ev_cycle, 4_096);
            if ev.is_some() || ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
                break;
            }
            // SLEEP between polls - not yield, and not spin.
            //
            // Three versions of this loop, and the middle one was mine and the worst:
            //   spin        -> 50 ms of this task burning a core          (~15%)
            //   yield_cpu   -> 50 ms of SCHEDULER THRASH                  (~70%)
            //   sleep       -> the task is not runnable at all            (this)
            //
            // `yield_cpu` hands the core back but leaves the task READY, so the scheduler picks it
            // straight back up and it yields again - same wall time, now with a scheduler round trip
            // per iteration, and the task charged for every tick it is scheduled. It measured WORSE
            // than the busy-wait it replaced, which is the honest reason this comment exists.
            //
            // `sleep` blocks on the timed wake until the deadline, so the core is genuinely free.
            // The floor is one 10 ms tick (`cycles_to_ticks` clamps sub-quantum requests), which
            // against a 50 ms budget is about five polls - ample, because a probe that is going to
            // answer answers in about a millisecond, and one that is not was going to burn the whole
            // budget either way.
            //
            // Measured, after four measurements that each refuted something else: of 10064 ms of work
            // in 60 s, the hub segment held 10061 - 99.97% - at ~60 ms per pass, which is
            // PROBE_ANSWER_MS plus overhead. So a probe that does not get an immediate answer spins
            // its ENTIRE 50 ms budget at full tilt, on nearly every pass. That is the 13-16% CPU, and
            // it is why adjusting wake rates never moved it: the cost was never how OFTEN the loop
            // ran, it was one busy-wait inside it.
            //
            // Yielding keeps the deadline exactly as it was - the wait is still bounded by the clock
            // and still returns the same answers (Commandment VIII) - while letting the core run
            // something else, or idle. The same fix this driver already received once, on the Wyse,
            // where a busy-spin held a core at 100%.
            ctx.sleep(ctx.duration_cycles(1));
        }
        match ev {
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
            Some((TRB_TRANSFER_EVENT, _, sid)) => {
                if sid < 32 {
                    *eaten |= 1 << sid;
                }
                // ABANDON THE PROBE and let the caller deliver the keystroke NOW.
                //
                // This used to keep waiting for its own event, so a key pressed during a probe sat
                // undelivered until the probe finished - up to PROBE_ANSWER_MS (50 ms) later. That is
                // the stutter felt while typing continuously: a 13 ms poll cadence with occasional
                // 50 ms hitches on top.
                //
                // The report is not lost either way (the caller delivers every `eaten` slot before
                // re-arming, 6ab4a926); the difference is WHEN. Returning immediately puts it on
                // screen this pass instead of after the wait.
                //
                // Cost: the probe is abandoned, so a hub port is not checked this cycle. That is
                // cheap - probes run every HUB_POLL_MS and a `None` concludes NOTHING (a failed
                // question is not an answer, 9af9ab4b), so the next cycle simply asks again. Input
                // is the interactive path; a removal noticed 500 ms later is not felt.
                //
                // FLAGGED as abandoned, because the caller cannot tell otherwise and the guard I
                // first wrote (`None if eaten != 0`) did not work: `eaten` is re-zeroed every pass
                // (see its declaration), so a probe abandoned on pass N looked like a genuine failure
                // on pass N+1. The counters then walked to 200, declared a HALTED endpoint that was
                // running perfectly well, and the Reset Endpoint came back Context State Error - the
                // controller telling us the endpoint was never halted. The disk was dropped and the
                // whole controller re-initialised, every ~195 s. That was this change's doing.
                *abandoned = true;
                return None;
            }
            Some(_) => {} // a non-transfer event (port change, command) - ignore; keep waiting
            None => {
                // A probe that got no completion. NOT logged per occurrence any more.
                //
                // This printed cursor/cycle state on every failure while the dequeue desync was being
                // hunted, and it earned that: the frozen `ev_idx` against a climbing `cur` is what
                // identified a halted endpoint. But a transient failure is now harmless and handled,
                // so the line had no consumer left - 550 of them in one session, 4% of the serial log,
                // emitted from the loop that also polls the keyboard.
                //
                // The escalations still report themselves, which is where the information belongs:
                // `unreachable 20x` when it starts looking persistent, and `unreachable 200x -
                // resetting it` when the endpoint is repaired. Silence here means "transient, and
                // something is counting".
                return None;
            }
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    ctx_size: usize,
    slot: u32,
    dev_idx: usize,
    speed: u32,
    route: u32,
    root_port: u32,
    nports: u8,
    mtt: bool,
    ttt: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> bool {
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    clear_input_ctx(dma, ctx_size);
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 0b11); // Add: slot + EP0
                                          // Slot dword0: Context Entries=1, Hub=1 (bit 26), MTT (bit 25), Speed (bits 23:20), Route (19:0).
    dma.write32(
        islot,
        (1 << 27) | (1 << 26) | (if mtt { 1 << 25 } else { 0 }) | (speed << 20) | (route & 0xFFFFF),
    );
    // Slot dword1: Number of Ports [31:24], Root Hub Port Number [23:16].
    dma.write32(islot + 4, ((nports as u32) << 24) | (root_port << 16));
    // Slot dword2: TT Think Time [17:16].
    dma.write32(islot + 8, (ttt & 0x3) << 16);
    // Re-specify EP0 (Add A1 set) so the command carries a valid endpoint-0 context.
    let ep0_tr = dma.phys_at(ep0_tr_off(dev_idx));
    let max_packet = ep0_max_packet(speed);
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let ce = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    ctx_size: usize,
    dev_idx: usize,
    route: u32,
    root_port: u32,
    speed: u32,
    parent_slot: u32,
    parent_port: u32,
    ttt: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> Option<(u32, u16, u16, u8)> {
    // Enable Slot.
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, slot) = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        0,
        0,
        0,
        (TRB_ENABLE_SLOT << 10) | 1,
        ev_idx,
        ev_cycle,
    )?;
    if comp != 1 {
        return None;
    }
    // Input context: Add slot + EP0.
    let islot = INPUT_CTX_OFF + ctx_size;
    let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
    clear_input_ctx(dma, ctx_size);
    dma.write32(INPUT_CTX_OFF, 0);
    dma.write32(INPUT_CTX_OFF + 4, 0b11);
    // Slot dword0: Context Entries=1, Speed [23:20], Route String [19:0]. (Not a hub, no MTT.)
    dma.write32(islot, (1 << 27) | (speed << 20) | (route & 0xFFFFF));
    // Slot dword1: Root Hub Port Number [23:16] (the root port the whole chain hangs off).
    dma.write32(islot + 4, root_port << 16);
    // Slot dword2: TT fields for a low/full-speed device (speed 1 or 2) behind a high-speed hub -
    // TT Hub Slot ID [7:0], TT Port Number [15:8], TT Think Time [17:16].
    // TT Hub Slot ID [7:0] and TT Port Number [15:8] name the high-speed hub that translates for this
    // low/full-speed device. TT Think Time [17:16] is deliberately NOT set here.
    //
    // xHCI 6.2.2 is explicit: TTT "shall be '0' if this device is not a High-speed hub". It is a
    // property of the HUB, and it belongs in the HUB's own slot context - which `configure_as_hub`
    // already sets. Stamping it into the DEVICE's slot context is an illegal field value, answered
    // with Parameter Error (completion 17).
    //
    // That is exactly the shape the Pi 4 showed: the high-speed stick on the same hub enumerated
    // fine because its whole TT block is zero, while the low-speed keyboard - the only device that
    // populates these fields at all - was refused. The dump proved it, after two fixes aimed at EP0
    // from reasoning alone had missed:
    //
    //     slot 0x08200004 0x00010000 0x00030401   <- dword2 bits 17:16 = 3, and must be 0
    //
    // Every other field in that dump was already correct.
    let tt = if speed == 1 || speed == 2 {
        let _ = ttt; // the hub's think time; NOT this device's business (see above)
        (parent_slot & 0xFF) | ((parent_port & 0xFF) << 8)
    } else {
        0
    };
    dma.write32(islot + 8, tt);
    // EP0 context.
    let ep0_tr = dma.phys_at(ep0_tr_off(dev_idx));
    let max_packet = ep0_max_packet(speed);
    dma.write32(iep0 + 4, (3 << 1) | (4 << 3) | (max_packet << 16));
    dma.write32(iep0 + 8, (ep0_tr as u32 & !0xF) | 1);
    dma.write32(iep0 + 12, (ep0_tr >> 32) as u32);
    dma.write32(iep0 + 16, 8);
    dma.write64(
        DCBAA_OFF + slot as usize * 8,
        dma.phys_at(device_ctx_off(dev_idx)),
    );
    // Address Device.
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, _) = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
    )?;
    if comp != 1 {
        ctx.log_fmt(format_args!(
            "xhci: downstream Address Device failed (completion={}, route={:#x}, speed={}, ep0_mps={})",
            comp, route, speed, ep0_max_packet(speed)
        ));
        // Completion 17 is Parameter Error: the xHC rejected a FIELD, and it does not say which. Dump
        // the context we built so the answer is read off the log rather than reasoned toward. The slot
        // dwords carry route/speed/context-entries, the root port, and the TT triple that only a
        // low/full-speed device behind a high-speed hub uses - which is precisely the case that fails
        // here while a high-speed device on the same hub succeeds.
        let islot = INPUT_CTX_OFF + ctx_size;
        let iep0 = INPUT_CTX_OFF + 2 * ctx_size;
        ctx.log_fmt(format_args!(
            "xhci:   input ctrl add={:#010x} drop={:#010x} | slot {:#010x} {:#010x} {:#010x} {:#010x}",
            dma.read32(INPUT_CTX_OFF + 4), dma.read32(INPUT_CTX_OFF),
            dma.read32(islot), dma.read32(islot + 4), dma.read32(islot + 8), dma.read32(islot + 12)));
        ctx.log_fmt(format_args!(
            // Print what was WRITTEN, decoded from the slot context - not the arguments that went
            // in. The first version printed `ttt` the parameter beside the dumped dwords, and I read
            // it as the field's value: it showed 3 while the context correctly held 0, and I spent a
            // night believing a fix had not applied when it had. A diagnostic that mixes inputs with
            // observations is worse than one that omits them.
            "xhci:   ep0 {:#010x} {:#010x} {:#010x} {:#010x} {:#010x} | written tt_slot={} tt_port={} ttt={} (hub's own ttt={})",
            dma.read32(iep0), dma.read32(iep0 + 4), dma.read32(iep0 + 8),
            dma.read32(iep0 + 12), dma.read32(iep0 + 16),
            dma.read32(islot + 8) & 0xFF,
            (dma.read32(islot + 8) >> 8) & 0xFF,
            (dma.read32(islot + 8) >> 16) & 0x3,
            ttt));
        return None;
    }
    // Read the device descriptor over the downstream slice's EP0 ring (offset 0).
    if !control(
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        0,
        ev_idx,
        ev_cycle,
        0x80,
        6,
        0x0100,
        0,
        18,
        DATA_BUF_OFF,
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
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    ctx_size: usize,
    slot: u32,
    dev_idx: usize,
    speed: u32,
    port: u32,
    route: u32,
    root_port: u32,
    parent_slot: u32,
    parent_port: u32,
    ttt: u32,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> (Option<Hid>, Option<msc::Disk>, u8) {
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
        return (None, None, 0);
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
        if blen == 0 {
            break;
        }
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
        // No boot-HID interrupt-IN endpoint. Before giving up on the device, check whether it is a
        // MASS-STORAGE one - that is the capability whose absence kept a USB stack in the kernel, so
        // "not a keyboard" must no longer mean "not ours".
        if let Some(m) = msc::parse_msc(dma, CONFIG_BUF_OFF, 64) {
            let disk = bind_msc(
                ctx,
                dma,
                mmio,
                dboff,
                ir0,
                ctx_size,
                slot,
                dev_idx,
                speed,
                port,
                route,
                root_port,
                parent_slot,
                parent_port,
                ttt,
                &m,
                ev_idx,
                ev_cycle,
                cmd_idx,
            );
            return (None, disk, cfg_val);
        }
        // A hub (the caller walks it with cfg_val) or a device this driver does not speak for.
        return (None, None, cfg_val);
    }
    let is_mouse = hid_proto == 2;
    let ep_num = (ep_addr & 0x0F) as u32;
    let dci = ep_num * 2 + 1;
    ctx.log_fmt(format_args!(
        "xhci: {} found on port {} (slot {}, DCI {}, mps={} interval={} cfg_val={})",
        if is_mouse { "mouse" } else { "keyboard" },
        port,
        slot,
        dci,
        ep_mps,
        ep_interval,
        cfg_val
    ));

    // Configure Endpoint (add the interrupt-IN endpoint). The slot context is re-supplied with the
    // updated Context Entries AND this device's route/speed/root-port/TT, so a downstream device keeps
    // routing (a root-port device passes route=0 / parent_*=0, reducing to the plain form).
    let int_tr = dma.phys_at(int_tr_off(dev_idx));
    let islot = INPUT_CTX_OFF + ctx_size;
    clear_input_ctx(dma, ctx_size);
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 1 | (1 << dci)); // Add: slot + interrupt endpoint
    dma.write32(islot, (dci << 27) | (speed << 20) | (route & 0xFFFFF)); // Context Entries=dci, speed, route
    dma.write32(islot + 4, root_port << 16);
    // TT Hub Slot ID [7:0] and TT Port Number [15:8] name the high-speed hub that translates for this
    // low/full-speed device. TT Think Time [17:16] is deliberately NOT set here.
    //
    // xHCI 6.2.2 is explicit: TTT "shall be '0' if this device is not a High-speed hub". It is a
    // property of the HUB, and it belongs in the HUB's own slot context - which `configure_as_hub`
    // already sets. Stamping it into the DEVICE's slot context is an illegal field value, answered
    // with Parameter Error (completion 17).
    //
    // That is exactly the shape the Pi 4 showed: the high-speed stick on the same hub enumerated
    // fine because its whole TT block is zero, while the low-speed keyboard - the only device that
    // populates these fields at all - was refused. The dump proved it, after two fixes aimed at EP0
    // from reasoning alone had missed:
    //
    //     slot 0x08200004 0x00010000 0x00030401   <- dword2 bits 17:16 = 3, and must be 0
    //
    // Every other field in that dump was already correct.
    let tt = if speed == 1 || speed == 2 {
        let _ = ttt; // the hub's think time; NOT this device's business (see above)
        (parent_slot & 0xFF) | ((parent_port & 0xFF) << 8)
    } else {
        0
    };
    dma.write32(islot + 8, tt);
    let iep = INPUT_CTX_OFF + (1 + dci as usize) * ctx_size;
    // xHCI Endpoint Context Interval encoding is speed-dependent (xHCI 6.2.3.6).
    let xhci_interval = match speed {
        1 | 2 => {
            let bi = if ep_interval == 0 {
                1
            } else {
                ep_interval as u32
            };
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
    dma.write32(iep, xhci_interval << 16);
    dma.write32(iep + 4, (3 << 1) | (7 << 3) | ((ep_mps as u32) << 16));
    dma.write32(iep + 8, (int_tr as u32 & !0xF) | 1);
    dma.write32(iep + 12, (int_tr >> 32) as u32);
    dma.write32(iep + 16, ep_mps as u32 | ((ep_mps as u32) << 16));
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let ce = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
    )
    .map(|(c, _)| c)
    .unwrap_or(0);
    ctx.log_fmt(format_args!("xhci: Configure Endpoint completion={}", ce));

    // Set Configuration, then Set Protocol (boot) on EP0 (offsets 96, 128 - contiguous after the
    // config-descriptor read that ended at 96).
    if control(
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        96,
        ev_idx,
        ev_cycle,
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
    // SET_PROTOCOL: boot (wValue=0) on the HID interface. Best-effort (most keyboards default to boot
    // mode), but log the outcome - a keyboard that needs it and didn't get it is otherwise an
    // undiagnosable dead keyboard.
    if control(
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        128,
        ev_idx,
        ev_cycle,
        0x21,
        0x0B,
        0,
        kbd_iface as u32,
        0,
        0,
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
    (
        Some(Hid {
            slot,
            dci,
            port,
            idx: dev_idx,
            is_mouse,
            hub_slot: 0,
            hub_dev: 0,
            hub_port: 0,
            hub_off: 0,
            hub_nports: 0,
        }),
        None,
        cfg_val,
    )
}

/// Answer a received message IF it is a block-IPC request; otherwise let it go.
///
/// The discriminator is the **reply cap**, not the payload. An interrupt wakeup from the kernel
/// carries no reply cap and nothing that needs answering, so it is discarded exactly as before; a
/// `request_with_reply` from `block-driver` carries one, and something is blocked awaiting the
/// answer. Guessing from the payload instead would mean an interrupt wakeup whose first byte
/// happened to be 1 got treated as a sector read.
///
/// NOTE on the shared event ring: serving a request consumes transfer events, and a HID completion
/// that lands in that window is recorded as "eaten" by `await_on_slot` - the same rare dropped
/// keystroke the hub port-status poll can already cause. It is a lost report, not a stalled
/// endpoint, and it is the honest cost of one event ring shared by input and storage.
#[allow(clippy::too_many_arguments)]
fn serve_if_block(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    disk: &mut Option<msc::Disk>,
    msg: &godspeed_sdk::Message,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    // HID slots whose transfer events this disk operation consumed. The caller MUST re-arm them.
    //
    // This was a local variable, thrown away - and the comment above it called the loss "a rare
    // dropped keystroke, not a stalled endpoint". That was wrong, and the Pi 4 proved it: the
    // keyboard typed a few characters and then died. An interrupt endpoint's completion is what
    // retires its in-flight TRB, so whoever consumes that event owes the re-arm. Swallowing it here
    // meant the endpoint was never queued again and the keyboard was gone for good - not a lost
    // report, a lost DEVICE.
    eaten: &mut u32,
    // `false` = the disk stopped answering a DATA operation, so the caller should drop it and
    // re-scan. Returning this rather than swallowing it is what turns an unplugged stick from "the
    // machine hangs" into "the disk went away".
) -> bool {
    // A message ARRIVED. Logged (bounded, first few only) because the Pi 4 showed block requests
    // getting no reply at all while NEITHER failure path inside `serve_block` logged - which means
    // it was never reached, and the two ways that can happen need different fixes: the poll loop is
    // not running (no message ever arrives here), or a message arrives WITHOUT a reply cap and this
    // function returns silently. Guessing between them has already cost two boots.
    // An ATOMIC, not a `static mut`. The first draft of this counter used `static mut` + `unsafe`,
    // and `scripts/unsafe_check.py` rejected it on the spot - correctly: §18.2 forbids `unsafe` in a
    // service outright, and "it is only a diagnostic" is exactly the reasoning the rule exists to
    // refuse. A relaxed atomic costs nothing and needs no exemption.
    static SEEN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    // Counted but not read: the arrival trace it fed was removed (f67f5c15) and the no-reply-cap
    // warning now has its own counter. Kept because the count itself is the cheap part and a future
    // diagnostic will want it; named `_n` so the compiler does not have to warn about our intent.
    let _n = SEEN.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
    // The "block-path message #N arrived" trace is GONE, and the refusal below now speaks ONCE.
    //
    // Both were bounded per instance (`n <= 8`), which looked fine and was not: a 10-hour soak logged
    // 477 of them in a seven-minute window, because chaos restarted services 501 times there and every
    // fresh instance gets its own first-8. A per-instance bound is no bound at all on a machine whose
    // whole purpose is restarting services.
    //
    // The arrival trace existed to prove the block path worked at all; it does, and has for a day of
    // hardware testing. The refusal is worth keeping - a request with no reply cap leaves its caller
    // waiting - but once per instance says everything a hundred repeats do, and this is the loop that
    // also polls the keyboard.
    let Some(reply) = ctx.take_pending_cap() else {
        // Counted SEPARATELY from `n`, which counts every block-path message. Gating on `n == 1`
        // could only ever fire if the very FIRST message a fresh instance saw was the malformed one -
        // a guard whose trigger cannot occur in the failing case, which is the eighth instance of that
        // shape this cycle and one I introduced while fixing the log spam an hour earlier.
        static NO_CAP: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if NO_CAP.fetch_add(1, core::sync::atomic::Ordering::Relaxed) == 0 {
            ctx.log("xhci: block request had NO reply cap - dropping it (the caller will block; further occurrences silent)");
        }
        return true;
    };
    let mut out = [0u8; 520];
    let n = msc::serve_block(
        ctx,
        dma, mmio, dboff, ir0, disk, msg.payload_bytes(), &mut out, ev_idx, ev_cycle, eaten,
    );
    // A9-1: TRY_send the reply, and do NOT discard the verdict.
    //
    // This was a blocking `send` into a queue the caller cannot drain: `block-driver` is parked in
    // `request_with_reply` waiting for exactly this reply, so if its 16-deep queue is full, it waits
    // for us and we wait for it. §8.9 is explicit - where two services send to each other, at least
    // one direction MUST be non-blocking - and neither was. It is reachable: `chaos` floods every
    // service, and the one that wedges here owns the KEYBOARD.
    //
    // A dropped reply is recoverable and a deadlock is not: the caller's own deadline fires, it
    // reacquires and retries (§14.3). So a full queue costs one retry instead of the machine.
    if let Err(e) = ctx.try_send_by_handle(reply, &godspeed_sdk::Message::from_bytes(&out[..n])) {
        // Reported, never swallowed (§26.7). The caller will time out and retry; this line is how an
        // operator knows WHY a block request went unanswered rather than inferring it.
        ctx.log_fmt(format_args!(
            "xhci: block reply not delivered ({:?}) - caller's queue full or gone; it will retry", e));
    }
    ctx.remove_cap(reply);
    // A data operation that FAILED means the device stopped answering - which on this board is
    // usually that it was unplugged. Report it so the caller can re-enumerate rather than answer
    // errors forever against a device that is no longer there: an unplugged stick left the disk
    // "bound", every request paying the full transfer timeout before failing, which reads as the
    // whole machine hanging rather than as a removed disk.
    // OP_CAPACITY is NOT one: it does no device I/O (see `serve_block`), so it cannot report that
    // the device stopped answering, and treating its failure that way dropped a live disk.
    let was_data_op = matches!(msg.payload_bytes().first(),
        Some(&msc::OP_READ_BLOCK) | Some(&msc::OP_WRITE_BLOCK) | Some(&msc::OP_WRITE_ZEROS));
    // ... AND only while a disk is still bound.
    //
    // Once it has been dropped, `serve_block` answers STATUS_ERR for the honest reason "no disk" -
    // and reading THAT as "the disk just stopped answering" restarts the whole enumeration again.
    // `fs` retries, so every retry triggered another re-init: a storm every ~450 ms that tore down
    // and rebound the KEYBOARD each time, which is how an unplugged stick presented as typing going
    // laggy and then dead.
    //
    // A device can only stop answering once. The second report is not news, it is an echo.
    !(was_data_op && out[0] == msc::STATUS_ERR && disk.is_some())
}

/// Configure an addressed mass-storage device's two BULK endpoints and read its geometry.
///
/// The shape mirrors the HID path exactly - one Configure Endpoint carrying the slot context plus the
/// endpoints being added, then Set Configuration - with two differences that matter:
///
/// * **Two endpoints in one command, not one.** Bulk transport needs both directions, and adding them
///   in a single Configure Endpoint is what keeps the device's context consistent; adding them in two
///   commands would leave a window where the device is half-configured.
/// * **Endpoint type is directional.** Bulk OUT is type 2 and bulk IN is type 6 (xHCI 6.2.3). Getting
///   this wrong does not fail loudly - it configures a pipe that accepts TRBs and never completes them.
///
/// Returns the bound `Disk` only if it also answered READ CAPACITY, because a disk whose size is
/// unknown is not usable and reporting one as present would push the failure to the first read.
#[allow(clippy::too_many_arguments)]
fn bind_msc(
    ctx: &ServiceContext,
    dma: &Dma,
    mmio: &Mmio,
    dboff: usize,
    ir0: usize,
    ctx_size: usize,
    slot: u32,
    dev_idx: usize,
    speed: u32,
    port: u32,
    route: u32,
    root_port: u32,
    parent_slot: u32,
    parent_port: u32,
    ttt: u32,
    m: &msc::MscInfo,
    ev_idx: &mut usize,
    ev_cycle: &mut u32,
    cmd_idx: &mut usize,
) -> Option<msc::Disk> {
    let mut disk = msc::Disk::new(slot, m.out_ep, m.in_ep, port);
    let (out_dci, in_dci) = (disk.out_dci(), disk.in_dci());
    let max_dci = out_dci.max(in_dci);
    ctx.log_fmt(format_args!(
        "xhci: USB mass storage on port {} (slot {}, bulk OUT DCI {} / IN DCI {}, mps={})",
        port, slot, out_dci, in_dci, m.mps
    ));

    // Input context: add the slot plus BOTH bulk endpoints.
    let islot = INPUT_CTX_OFF + ctx_size;
    clear_input_ctx(dma, ctx_size);
    dma.write32(INPUT_CTX_OFF, 0); // Drop flags
    dma.write32(INPUT_CTX_OFF + 4, 1 | (1 << out_dci) | (1 << in_dci));
    dma.write32(islot, (max_dci << 27) | (speed << 20) | (route & 0xFFFFF));
    dma.write32(islot + 4, root_port << 16);
    // TT Hub Slot ID [7:0] and TT Port Number [15:8] name the high-speed hub that translates for this
    // low/full-speed device. TT Think Time [17:16] is deliberately NOT set here.
    //
    // xHCI 6.2.2 is explicit: TTT "shall be '0' if this device is not a High-speed hub". It is a
    // property of the HUB, and it belongs in the HUB's own slot context - which `configure_as_hub`
    // already sets. Stamping it into the DEVICE's slot context is an illegal field value, answered
    // with Parameter Error (completion 17).
    //
    // That is exactly the shape the Pi 4 showed: the high-speed stick on the same hub enumerated
    // fine because its whole TT block is zero, while the low-speed keyboard - the only device that
    // populates these fields at all - was refused. The dump proved it, after two fixes aimed at EP0
    // from reasoning alone had missed:
    //
    //     slot 0x08200004 0x00010000 0x00030401   <- dword2 bits 17:16 = 3, and must be 0
    //
    // Every other field in that dump was already correct.
    let tt = if speed == 1 || speed == 2 {
        let _ = ttt; // the hub's think time; NOT this device's business (see above)
        (parent_slot & 0xFF) | ((parent_port & 0xFF) << 8)
    } else {
        0
    };
    dma.write32(islot + 8, tt);

    // Endpoint contexts. Interval is 0 for bulk (it is not a periodic endpoint); CErr = 3 is the
    // standard retry count; Average TRB Length is the max packet size, which is what BOT transfers
    // are built from.
    for (dci, ep_type) in [(out_dci, 2u32), (in_dci, 6u32)] {
        let ring = if ep_type == 2 {
            disk.out_ring_phys(dma)
        } else {
            disk.in_ring_phys(dma)
        };
        let iep = INPUT_CTX_OFF + (1 + dci as usize) * ctx_size;
        dma.write32(iep, 0);
        dma.write32(iep + 4, (3 << 1) | (ep_type << 3) | ((m.mps as u32) << 16));
        // Bit 0 is the Dequeue Cycle State, which must match the ring's initial producer cycle (1).
        dma.write32(iep + 8, (ring as u32 & !0xF) | 1);
        dma.write32(iep + 12, (ring >> 32) as u32);
        dma.write32(iep + 16, m.mps as u32);
    }

    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let ce = run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_CONFIGURE_ENDPOINT << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
    )
    .map(|(c, _)| c)
    .unwrap_or(0);
    if ce != 1 {
        ctx.log_fmt(format_args!(
            "xhci: mass storage Configure Endpoint failed (completion={}) - disk not bound",
            ce
        ));
        return None;
    }

    // Set Configuration at EP0 ring offset 96, past the 3-TRB config-descriptor read at 48.
    if !control(
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        96,
        ev_idx,
        ev_cycle,
        0x00,
        9,
        m.cfg_value as u32,
        0,
        0,
        0,
    ) {
        ctx.log("xhci: mass storage Set Configuration failed - disk not bound");
        return None;
    }

    // TEST UNIT READY is allowed to fail a few times: a stick that has just been configured is often
    // still spinning up its controller and answers NOT READY until it is. Bounded, and the bound is a
    // COUNT of attempts here only because each attempt already carries its own generous transfer
    // budget - the loop cannot outlive the device's own answer.
    let mut eaten = 0u32;
    let mut ready = false;
    // A9-5: bounded by the CLOCK as well as the count.
    //
    // The comment above argued the count was safe because "each attempt carries its own generous
    // transfer budget". That is exactly what makes it unsafe: the budget is 30 s, so 16 attempts is
    // up to EIGHT MINUTES of a driver that answers nothing - and this driver owns the keyboard. A
    // per-attempt bound multiplied by a retry count is a total, and the total is what the user waits.
    // 20 s covers a stick that is genuinely still spinning up; past that it is not coming.
    let spinup_deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(20_000));
    for _ in 0..16 {
        if ctx.read_tsc().wrapping_sub(spinup_deadline) < (1u64 << 63) {
            ctx.log("xhci: mass storage still not ready after 20s - giving up on this bind");
            break;
        }
        if msc::test_unit_ready(
            ctx,
        dma, mmio, dboff, ir0, &mut disk, ev_idx, ev_cycle, &mut eaten,
        ) {
            ready = true;
            break;
        }
    }
    if !ready {
        ctx.log("xhci: mass storage never reported ready - disk not bound");
        return None;
    }

    match msc::read_capacity(
        ctx,
        dma, mmio, dboff, ir0, &mut disk, ev_idx, ev_cycle, &mut eaten,
    ) {
        Some(n) => {
            disk.sectors = n;
            ctx.log_fmt(format_args!(
                "xhci: USB disk ready - {} sectors of {} B ({} MiB)",
                n,
                msc::SECTOR,
                (n * msc::SECTOR as u64) / (1024 * 1024)
            ));
            // NOT announced here. This runs on the boot pass too, and the boot devices are
            // deliberately silent - a keyboard present at power-on does not announce itself either
            // (`announce` stays false for the first pass). Saying "storage connected" before the
            // prompt reports a plug event that did not happen. The announce moved to the enumeration
            // caller, which knows whether this pass is a boot or a hot-plug.
            Some(disk)
        }
        None => {
            // Either the command failed or the device reported a sector size this driver does not
            // speak. Both are "unusable", and saying so beats binding a disk whose geometry is a guess.
            ctx.log_fmt(format_args!(
                "xhci: USB disk READ CAPACITY failed or block size is not {} - disk not bound",
                msc::SECTOR
            ));
            None
        }
    }
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
/// Write a 64-bit xHCI register as TWO 32-BIT WRITES, low half first.
///
/// The SDK's `Mmio::write64` emits a single 64-bit store. The in-kernel driver this replaced (deleted 2026-08-09; read it in git history) - the one that had
/// always driven this exact VL805 on the Pi 4 - has never done that: its `wr64` is two 32-bit writes,
/// and the difference had never been tested because QEMU accepts either.
///
/// It matters because these registers live behind the BCM2711's PCIe bridge, not in the SoC's own
/// peripheral space, and a 64-bit store to a device BAR is not guaranteed to cross a bridge intact.
/// The xHCI spec's own guidance is that software may only use a single 64-bit access where the
/// controller declares support for it; two 32-bit writes are always legal.
///
/// Low half first is required, not stylistic: several of these registers (CRCR especially) latch on
/// the write of the HIGH half, so writing high-then-low latches a half-updated pointer.
fn wr64(mmio: &Mmio, off: usize, val: u64) {
    mmio.write32(off, val as u32);
    mmio.write32(off + 4, (val >> 32) as u32);
}

/// One-line reminder of what the dump below distinguishes, so a log reader does not have to hold the
/// xHCI spec in their head. Kept as a constant so it cannot drift from `dump_ring_state`.
const DIAG_HINT: &str = "command diagnosis - CRR=0 means the controller never started the command ring; TRB readback wrong means our write is not reaching RAM the device sees; EVT cycle unchanged means it never posted a completion";

/// Dump the DMA-side state a "no completion" failure turns on, because USBSTS alone cannot tell the
/// three causes apart and they need completely different fixes:
///
/// * **CRCR.CRR = 0** - the controller is not running the command ring at all. Our CRCR programming
///   or the ring's physical address is wrong.
/// * **The command TRB does not read back as written** - our stores are not landing in memory the
///   device can see. On AArch64 that is the arena's cache attributes (it must be Device/nC), which
///   is exactly the class that makes MMIO look perfect while every DMA silently fails.
/// * **Both fine, event ring untouched** - the controller consumed the command and never posted a
///   completion, or posted one we cannot see because the event ring or its cycle bit is misprogrammed.
///
/// This exists because the Pi 4's first userspace-USB boot failed at the very first Enable Slot with
/// a HEALTHY controller (HCH=0 HSE=0 HCE=0), and no amount of reasoning from that line alone can
/// choose between the three. Measure, do not guess.
#[allow(clippy::too_many_arguments)]
fn dump_ring_state(
    ctx: &ServiceContext, dma: &Dma, mmio: &Mmio, op: usize, ir0: usize,
    cmd_off: usize, ev_idx: usize, ev_cycle: u32,
) {
    let crcr_lo = mmio.read32(op + OP_CRCR);
    let dcbaap_lo = mmio.read32(op + OP_DCBAAP);
    ctx.log_fmt(format_args!(
        "xhci: CRCR={:#010x} (CRR={}) DCBAAP={:#010x} cmd_ring_phys={:#x}",
        crcr_lo, (crcr_lo & (1 << 3) != 0) as u8, dcbaap_lo, dma.phys_at(CMD_RING_OFF)));
    ctx.log_fmt(format_args!(
        "xhci: cmd TRB @{:#x} readback {:#010x} {:#010x} {:#010x} {:#010x}",
        cmd_off,
        dma.read32(cmd_off), dma.read32(cmd_off + 4),
        dma.read32(cmd_off + 8), dma.read32(cmd_off + 12)));
    ctx.log_fmt(format_args!(
        "xhci: ERSTSZ={} ERSTBA={:#010x} erst_phys={:#x} erst[0]={:#010x} size={}",
        mmio.read32(ir0 + 0x08), mmio.read32(ir0 + 0x10), dma.phys_at(ERST_OFF),
        dma.read32(ERST_OFF), dma.read32(ERST_OFF + 8)));
    let ev = EVENT_RING_OFF + ev_idx * TRB_SIZE;
    ctx.log_fmt(format_args!(
        "xhci: EVT[{}] @{:#x} ctrl={:#010x} (want cycle {}) ERDP={:#010x} ev_ring_phys={:#x}",
        ev_idx, ev, dma.read32(ev + 12), ev_cycle,
        mmio.read32(ir0 + 0x18), dma.phys_at(EVENT_RING_OFF)));
}

/// commands. Pure diagnosis when it returns false (e.g. a device-level Transaction Error with the HC
/// still running); the log is the breadcrumb that tells us, on the Wyse, which case a port hit.
fn hc_wedged_now(ctx: &ServiceContext, mmio: &Mmio, op: usize) -> bool {
    let sts = mmio.read32(op + OP_USBSTS);
    let wedged = sts & STS_WEDGED != 0;
    ctx.log_fmt(format_args!("xhci: {}", DIAG_HINT));
    ctx.log_fmt(format_args!(
        "xhci: post-command USBSTS={:#010x} (HCH={} HSE={} HCE={} CNR={}){}",
        sts,
        (sts & STS_HCH != 0) as u8,
        (sts & STS_HSE != 0) as u8,
        (sts & STS_HCE != 0) as u8,
        (sts & STS_CNR != 0) as u8,
        if wedged {
            " - HC WEDGED, re-initialising"
        } else {
            ""
        },
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
        if next == 0 {
            break;
        }
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
    // The bound mass-storage device, if this port produced one. An out-param rather than a return
    // value because a port can yield a HID *and* (behind a hub) a disk - one pass, two results.
    disk: &mut Option<msc::Disk>,
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
        spin(ctx, "PORTSC.PED after a root-port reset", 250, || {
            mmio.read32(portsc_off) & PORT_PED != 0
        });
        // Reset-recovery hold before we address the device (Fix 3): PED asserting does not mean the
        // device is ready for the SET_ADDRESS of Address Device. A high-speed root-port device (the
        // Wyse's port 6) returns a Transaction Error (completion=4) without this; the behind-a-hub path
        // already holds here. Bounded, TSC-paced.
        let t0 = ctx.read_tsc();
        let hold = ctx.duration_cycles(RESET_RECOVERY_MS);
        while ctx.read_tsc().wrapping_sub(t0) < hold {}
    }
    let psc = mmio.read32(portsc_off);
    let speed = (psc >> 10) & 0xF;
    let max_packet = ep0_max_packet(speed);
    ctx.log_fmt(format_args!(
        "xhci: port {} ready; PORTSC={:#010x} speed={} max_packet={}",
        port, psc, speed, max_packet
    ));

    // Enable Slot.
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, slot) = match run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        0,
        0,
        0,
        (TRB_ENABLE_SLOT << 10) | 1,
        ev_idx,
        ev_cycle,
    ) {
        Some(r) => r,
        None => {
            ctx.log("xhci: Enable Slot - no completion");
            // The FIRST command on a fresh controller. If this one gets no completion, nothing about
            // the DMA side has ever been proven, so dump it rather than guess (see `dump_ring_state`).
            dump_ring_state(ctx, dma, mmio, op, ir0, cmd_off, *ev_idx, *ev_cycle);
            *hc_wedged = hc_wedged_now(ctx, mmio, op);
            sa.free(dev_idx);
            return;
        }
    };
    if comp != 1 {
        ctx.log_fmt(format_args!(
            "xhci: Enable Slot failed (completion={})",
            comp
        ));
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
    dma.write64(
        DCBAA_OFF + slot as usize * 8,
        dma.phys_at(device_ctx_off(dev_idx)),
    );
    let in_phys = dma.phys_at(INPUT_CTX_OFF);
    let cmd_off = CMD_RING_OFF + *cmd_idx * TRB_SIZE;
    *cmd_idx += 1;
    let (comp, _) = match run_command(
        ctx,
        dma,
        mmio,
        dboff,
        ir0,
        cmd_off,
        in_phys as u32,
        (in_phys >> 32) as u32,
        0,
        (TRB_ADDRESS_DEVICE << 10) | (slot << 24) | 1,
        ev_idx,
        ev_cycle,
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
        ctx.log_fmt(format_args!(
            "xhci: Address Device failed (completion={})",
            comp
        ));
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
            Some((TRB_TRANSFER_EVENT, c, _)) => {
                ok = c == 1 || c == 13;
                break;
            }
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
        dclass,
        ids & 0xFFFF,
        (ids >> 16) & 0xFFFF
    ));

    // Read the config descriptor and bind if it's a boot HID (root device: route=0, parent_*=0).
    let (bound, found_disk, cfg_val) = read_config_and_bind(
        ctx, dma, mmio, dboff, ir0, ctx_size, slot, dev_idx, speed, port, 0, port, 0, 0, 0, ev_idx,
        ev_cycle, cmd_idx,
    );
    // First disk wins. A second one is left unbound rather than silently replacing the first, which
    // would swap the filesystem's device out from under it.
    if disk.is_none() {
        *disk = found_disk;
    }
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
    // A device this pass bound AS THE DISK keeps its slice and its slot. Everything below tears down
    // a device the driver has no use for, and until mass storage existed that included every disk -
    // the old comment here said so outright ("e.g. the mass-storage boot drive"). Releasing the slot
    // now would disable the endpoints that were just configured, so the disk would report its
    // capacity and then answer nothing, which reads as a broken device rather than a driver that
    // threw it away.
    if disk.as_ref().is_some_and(|d| d.slot == slot) {
        return;
    }
    if dclass != 0x09 {
        // Not a HID, not a hub, and not a disk - release the slice + slot so neither leaks.
        ctx.log_fmt(format_args!(
            "xhci: port {} device is not a keyboard, mouse, hub or disk - releasing it",
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
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        96,
        ev_idx,
        ev_cycle,
        0x00,
        9,
        cfg_val as u32,
        0,
        0,
        0,
    );
    let hub_ok = control(
        dma,
        mmio,
        dboff,
        ir0,
        slot,
        dev_idx,
        128,
        ev_idx,
        ev_cycle,
        0xA0,
        6,
        0x29 << 8,
        0,
        8,
        DATA_BUF_OFF,
    );
    let mut nports = if hub_ok { dma.read8(DATA_BUF_OFF + 2) } else { 0 };
    // A SUPERSPEED hub answers descriptor type 0x2A, not 0x29, and returns nothing for the 0x29 we
    // just asked for. The Pi 4's USB-A sockets are the SuperSpeed side of its internal hub, so
    // refusing to walk one meant nothing plugged into a blue port was ever seen - not a regression,
    // a capability this driver never had, hidden for as long as everything landed on the USB2 side.
    //
    // The two descriptors agree on the fields used here: bNbrPorts at offset 2, wHubCharacteristics
    // at 3. Only the TYPE differs, so this is a second request, not a second parser.
    //
    // The retry consumes another 3 TRBs of the EP0 ring, which is why `hoff` starts past it below.
    let mut ss_hub = false;
    if nports == 0 {
        let ok2 = control(
            dma, mmio, dboff, ir0, slot, dev_idx, 176,
            ev_idx, ev_cycle, 0xA0, 6, 0x2A << 8, 0, 12, DATA_BUF_OFF,
        );
        if ok2 {
            nports = dma.read8(DATA_BUF_OFF + 2);
            ss_hub = nports != 0;
        }
    }
    let whubchar = dma.read16(DATA_BUF_OFF + 3); // wHubCharacteristics
    // A SuperSpeed hub has NO transaction translator - a TT exists to carry low/full-speed traffic
    // across a high-speed link, and there is no such thing below a SuperSpeed one. Reporting a TT it
    // does not have is the same class of illegal field that made the low-speed keyboard fail
    // Address Device, so both are forced to zero rather than parsed out of wHubCharacteristics.
    let ttt = if ss_hub { 0 } else { ((whubchar >> 5) & 0x3) as u32 };
    let mtt = !ss_hub && dproto == 2; // bDeviceProtocol 2 = multi-TT hub
    ctx.log_fmt(format_args!(
        "xhci: USB{} hub on port {} (slot {}, {} downstream ports, mtt={}, ttt={})",
        if ss_hub { "3 SuperSpeed" } else { "2" }, port, slot, nports, mtt, ttt
    ));
    // Neither descriptor produced a port count. Now it really is unusable.
    if nports == 0 {
        ctx.log("xhci: hub reports 0 ports on both descriptor 0x29 and 0x2A - not walking");
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
    // Past the hub-descriptor read, and past the SuperSpeed retry if one was issued - each is 3 TRBs
    // of 16 bytes. Overlapping them would rewrite a TRB the controller may not have consumed.
    let mut hoff = if ss_hub { 224usize } else { 176usize };
    for dp in 1..=nports {
        if hoff + 32 > 0xF00 {
            break;
        }
        let _ = control(
            dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle, 0x23, 3, 8, dp as u32, 0,
            0,
        ); // Set_Feature(PORT_POWER = 8)
        hoff += 32;
    }
    // Let power settle before touching any port.
    //
    // The in-kernel driver waited 200 ms here and mine waited none. A port is powered, not ready: a
    // device has to see VBUS, pull its speed-signalling resistor up, and be DETECTED by the hub
    // before a reset can enable it. Reset it too early and the port answers "connected, powered, not
    // enabled" - status 0x0301, exactly what the Pi 4 reported seven times over sixteen seconds
    // before the device finally came up on a later re-scan.
    //
    // bPwrOn2PwrGood in the hub descriptor gives this in 2 ms units and would be the precise answer;
    // 200 ms is the reference driver's figure and covers every hub it has met. Paid once per hub at
    // enumeration.
    ctx.sleep(ctx.duration_cycles(PORT_POWER_SETTLE_MS));
    // For each CONNECTED downstream port: reset it, read its speed, Address Device it with a route
    // string (this hub port, tier 1) + parent-TT into its OWN slice, then read its config and bind it
    // if it's a boot HID - exactly like a root-port device (read_config_and_bind). This is what makes
    // the back-port keyboard work.
    let ndev_before = *ndev;
    // Whether a disk was already bound BEFORE walking this hub. A disk found behind it counts as
    // "something usable is down there" exactly as a HID does - see the release check below.
    let disk_before = disk.is_some();
    for dp in 1..=nports {
        if *ndev >= MAX_HID {
            break;
        }
        if hoff + 48 > 0xD00 {
            break;
        }
        let ok = control(
            dma,
            mmio,
            dboff,
            ir0,
            slot,
            dev_idx,
            hoff,
            ev_idx,
            ev_cycle,
            0xA3,
            0,
            0,
            dp as u32,
            4,
            DATA_BUF_OFF,
        ); // Get_Status(port)
        hoff += 48;
        let st = if ok { dma.read16(DATA_BUF_OFF) } else { 0 };
        if st & 1 == 0 {
            continue; // nothing connected on this downstream port
        }
        // Reset the port, hold, clear the reset-change, re-read status for the device speed.
        let _ = control(
            dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle, 0x23, 3, 4, dp as u32, 0,
            0,
        ); // Set_Feature(PORT_RESET = 4)
        hoff += 32;
        // POLL the port until it reports ENABLED, rather than holding for a fixed time and hoping.
        //
        // Taken from the in-kernel driver, since deleted (`arch/aarch64/xhci.rs`, in git history), which had
        // driven this exact hub for weeks. I had been re-deriving this sequence from the spec and
        // getting it subtly wrong; the working version was sitting in the repo the whole time.
        //
        // A fixed hold is wrong in both directions: too short and the device is addressed before it
        // is ready (completion 4, which is what the Pi 4 showed), too long and every enumeration
        // pays for the slowest device on the bus. The port itself says when it is ready.
        // FEWER, LONGER polls - because each one costs EP0 RING, not just time.
        //
        // Every status poll is a 3-TRB control transfer that advances `hoff`, and the ring is one
        // page. At 100 polls x 48 bytes a single slow port consumed 4800 bytes of it, so later ports
        // hit the `hoff` guard and were silently skipped. On the Pi 4 the disk sits on an early hub
        // port and the keyboard on a later one, which is exactly why plugging the stick back in made
        // the KEYBOARD disappear: the pass found the disk, ran out of ring, and never walked far
        // enough to reach the keyboard - logging "0 HID device(s) bound" as though none were there.
        //
        // 12 polls at 20 ms is the same ~240 ms of patience for a port that is genuinely slow, at a
        // twelfth of the ring cost. The budget was never the scarce resource; the ring was.
        let mut pstatus = 0u16;
        for _ in 0..12 {
            ctx.sleep(ctx.duration_cycles(20));
            let ok = control(
                dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle, 0xA3, 0, 0,
                dp as u32, 4, DATA_BUF_OFF,
            );
            hoff += 48;
            // Ring exhausted. Say so - this used to break out silently, and a port skipped for lack
            // of ring is indistinguishable in the log from a port with nothing plugged into it.
            if hoff + 64 > 0xF00 {
                ctx.log_fmt(format_args!(
                    "xhci: EP0 ring exhausted walking hub port {} - remaining ports not scanned this pass", dp));
                break;
            }
            if !ok { break; }
            pstatus = dma.read16(DATA_BUF_OFF);
            if pstatus & 0x2 != 0 { break; } // PORT_ENABLE
        }
        let _ = control(
            dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle, 0x23, 1, 0x14, dp as u32,
            0, 0,
        ); // Clear_Feature(C_PORT_RESET = 20)
        hoff += 32;
        // TRSTRCY: a device is NOT addressable the instant its port reset completes. USB 2.0 §7.1.7.5
        // gives it 10 ms to recover before it will answer on address 0, and this code went straight
        // from clearing C_PORT_RESET into Address Device.
        //
        // That is what a completion 4 (Transaction Error) means here: the input context was ACCEPTED
        // and the device did not answer - not an illegal field, not a flaky link. It also explains
        // why retrying immediately could not help: three attempts 15 ms apart all land inside the
        // recovery window, which is exactly what the Pi 4 showed (completion=4 three times in 45 ms,
        // the code never changing).
        // Acknowledge C_PORT_CONNECTION too. The in-kernel driver cleared BOTH change bits with the
        // comment "or the hub keeps reporting the same event forever" - and mine cleared only
        // C_PORT_RESET. An unacknowledged connection-change is a hub that keeps announcing the same
        // arrival, which is exactly the re-enumeration behaviour seen on this board.
        let _ = control(
            dma, mmio, dboff, ir0, slot, dev_idx, hoff, ev_idx, ev_cycle, 0x23, 1, 0x10, dp as u32,
            0, 0,
        ); // Clear_Feature(C_PORT_CONNECTION = 16)
        hoff += 32;
        // If the port never enabled, addressing it cannot work - say so instead of trying.
        if pstatus & 0x2 == 0 {
            ctx.log_fmt(format_args!(
                "xhci: hub port {} did not enable after reset (status {:#06x}) - skipping it",
                dp, pstatus));
            continue; // no slice allocated yet at this point - nothing to release
        }
        ctx.sleep(ctx.duration_cycles(PORT_RECOVERY_MS));
        let _ = control(
            dma,
            mmio,
            dboff,
            ir0,
            slot,
            dev_idx,
            hoff,
            ev_idx,
            ev_cycle,
            0xA3,
            0,
            0,
            dp as u32,
            4,
            DATA_BUF_OFF,
        ); // Get_Status again (post-reset, for speed)
        hoff += 48;
        let pst = dma.read16(DATA_BUF_OFF);
        // Port-status speed bits: bit 9 = low-speed, bit 10 = high-speed; neither = full-speed.
        // Map to the xHCI slot-context speed value (1=Full, 2=Low, 3=High).
        let dspeed = if pst & (1 << 9) != 0 {
            2
        } else if pst & (1 << 10) != 0 {
            3
        } else {
            1
        };
        // The downstream device gets its OWN slice.
        let d_idx = match sa.alloc() {
            Some(i) => i,
            None => {
                ctx.log("xhci: out of DMA slices for a downstream device - stopping hub walk");
                break;
            }
        };
        // Address Device is RETRIED, because completion 4 (Transaction Error) means the context was
        // ACCEPTED and the controller then failed to talk to the device over the wire - no response
        // or a corrupt one. That is transient by definition, unlike completion 17 (Parameter Error),
        // which means a field is illegal and will be illegal every time.
        //
        // The Pi 4's blue sockets produce exactly this: a low-speed keyboard there answers on the
        // first attempt only sometimes. A low/full-speed device behind a high-speed hub reaches the
        // controller through the hub's transaction translator, and the first transaction after a
        // port reset is the one most likely to be missed.
        //
        // Bounded at 3. Every attempt logs its own failure - deliberately noisier than reporting
        // only the last, because the interesting question for a flaky port is whether the completion
        // code CHANGES between attempts (a device that answers on attempt 2 is a different problem
        // from one that never answers). A retry that ultimately fails stays as loud as the original
        // failure either way (§26.7).
        let mut attempt = 0;
        let addressed = loop {
            attempt += 1;
            let r = address_downstream(
            ctx,
            dma,
            mmio,
            dboff,
            ir0,
            ctx_size,
            d_idx,
            dp as u32 & 0xF,
            port,
            dspeed,
            slot,
            dp as u32,
            ttt,
            ev_idx,
            ev_cycle,
            cmd_idx,
            );
            // Retry only while there is an attempt left; the callee reports the last failure.
            if r.is_some() || attempt >= 3 {
                break r;
            }
        };
        match addressed {
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
                let (dbound, d_disk, _) = read_config_and_bind(
                    ctx,
                    dma,
                    mmio,
                    dboff,
                    ir0,
                    ctx_size,
                    dslot,
                    d_idx,
                    dspeed,
                    port,
                    dp as u32 & 0xF,
                    port,
                    slot,
                    dp as u32,
                    ttt,
                    ev_idx,
                    ev_cycle,
                    cmd_idx,
                );
                if disk.is_none() {
                    // Record WHERE it is, not just that it exists: the hot-plug scan needs the hub
                    // coordinates to stop reading this disk as a newly-arrived device every pass.
                    *disk = d_disk.map(|mut dk| {
                        dk.hub_slot = slot;
                        dk.hub_port = dp as u32;
                        dk.hub_dev = dev_idx as u32;
                        dk.hub_nports = nports as u32;
                        dk.hub_off = hoff;
                        dk
                    });
                }
                match dbound {
                    Some(mut hid) => {
                        ctx.log_fmt(format_args!(
                            "xhci: hub port {} HID bound (slot {}) - back-port device now live",
                            dp, dslot
                        ));
                        // Record the parent hub so the poll loop can GET_STATUS this hub port to notice
                        // the device unplugged (no root PORTSC reflects a device leaving behind a hub).
                        hid.hub_slot = slot;
                        hid.hub_dev = dev_idx as u32;
                        hid.hub_port = dp as u32;
                        hid.hub_nports = nports as u32; // for the poll loop's new-device re-scan
                        devs[*ndev] = hid; // room checked at loop top
                        *ndev += 1;
                    }
                    None if disk.as_ref().is_some_and(|d| d.slot == dslot) => {
                        // It IS the disk. "Not a HID" is not "not usable", and tearing this down
                        // would disable the endpoints just configured - which is exactly what
                        // happened on the Pi 4: the stick reported 15267 MiB and then its first
                        // sector read failed, because this arm had already disabled its slot
                        // between those two lines without logging a word.
                        ctx.log_fmt(format_args!(
                            "xhci: hub port {} is the USB disk (slot {}) - keeping it bound", dp, dslot));
                    }
                    None => {
                        // Connected but not a boot HID or a disk (or bind failed) - release its
                        // slice + slot so neither leaks.
                        sa.free(d_idx);
                        disable_slot(ctx, dma, mmio, dboff, ir0, dslot, ev_idx, ev_cycle, cmd_idx);
                    }
                }
            }
            None => {
                ctx.log_fmt(format_args!(
                    "xhci: hub port {} connected but downstream Address Device FAILED (route/TT)",
                    dp
                ));
                sa.free(d_idx);
            }
        }
    }
    // "Nothing usable" has to mean nothing usable - a HID *or* a disk. Counting only HIDs released
    // the hub while the Pi 4's USB stick was hanging off it, and disabling a hub's slot tears down
    // the routing for everything behind it. The disk had already reported its geometry by then, so
    // the failure surfaced one line later as a sector read that "failed", pointing at the disk
    // rather than at the hub that had just been pulled out from under it.
    // THE HUB IS KEPT, always - even with nothing behind it.
    //
    // It used to be released whenever nothing was bound, to return its slice to the pool. On this
    // board that is always wrong: every USB socket is behind this hub, so releasing it disables the
    // one control endpoint through which an arrival could ever be noticed. Boot with no stick, and
    // the hub goes; plug something in afterwards and nothing sees it - no INFO, `drives` stale, a
    // keyboard that never rebinds. That is the entire session's report, from this one branch.
    //
    // The in-kernel driver reached the same conclusion and said so at the equivalent point: "Keep the
    // hub. Everything on this board is behind it, so a keyboard unplugged once would stay dead until
    // reboot without something still holding its control endpoint." I released it anyway.
    //
    // It also explains why `chaos max-carnage` FIXED everything: carnage restarts services, forcing a
    // fresh enumeration that re-acquires the hub. The steady state was stuck; a re-init cleared it.
    //
    // The slice cost is one, bounded and constant - a hub is infrastructure, not a device that comes
    // and goes, and the pool exists to bound growth rather than to reclaim the machine's own wiring.
    if *ndev == ndev_before && disk.is_some() == disk_before {
        ctx.log_fmt(format_args!(
            "xhci: hub on port {} has nothing behind it yet - KEEPING it so an arrival can be seen",
            port
        ));
        // Seed its poll cursor exactly as the bound case below does: the probes that watch for an
        // arrival run on this hub's EP0 ring and must resume where enumeration left the controller's
        // dequeue, not where it started.
        for d in 0..*ndev {
            if devs[d].hub_slot == slot {
                devs[d].hub_off = hoff;
            }
        }
    } else {
        // A device was bound behind this hub; the hub's slice is KEPT. Seed each such device's poll
        // cursor to just past all the enumeration TRBs we wrote on the hub's EP0 ring (`hoff`), so the
        // poll loop's downstream-port GET_STATUS resumes exactly where the controller's dequeue sits.
        for d in 0..*ndev {
            if devs[d].hub_slot == slot {
                devs[d].hub_off = hoff;
            }
        }
        // The DISK's cursor too, and for the same reason - it was captured INSIDE the per-port loop
        // (at the moment the disk was bound), so it recorded `hoff` before the remaining ports had
        // been walked. Every later port-power and status transfer advanced the ring past it, leaving
        // the disk's probe cursor pointing at TRBs the controller had already consumed. A probe
        // written behind the dequeue pointer is never looked at again: it is posted, the doorbell is
        // rung, and no completion ever comes.
        //
        // That is the shape the `[probe]` diagnostic showed on hardware - `cur` climbing 0x30 per
        // probe while `ev_idx` stood still, 296 times.
        if let Some(dk) = disk.as_mut() {
            if dk.hub_slot == slot {
                dk.hub_off = hoff;
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
    // Max Scratchpad Buffers: **Hi is bits [25:21], Lo is bits [31:27]** (xHCI 1.1 Table 5-11),
    // value = (Hi << 5) | Lo.
    //
    // These two were SWAPPED here, and the Pi 4 caught it: the VL805 reported `max_scratch=992` to
    // this service while the in-kernel driver - which had the fields the right way round - refused
    // loudly above 512 and has always driven this board fine. Decoding 992 backwards (31<<5 | 0)
    // gives Hi=0, Lo=31: the controller wants **31** buffers, not 992.
    //
    // On THIS board the mistake over-provisions and is survivable. It is still a real bug, because
    // the error is not symmetric: a controller with Hi != 0 (say Hi=1, Lo=0, wanting 32) would be
    // read as wanting 1, and UNDER-providing scratchpad is a controller that DMAs into memory it
    // was never given - the failure the paragraph below is about.
    let max_scratch = (((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F);
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
        "xhci: USB3 (SuperSpeed) companion root-port mask={:#x} - USB2 ports enumerated first, then these",
        usb3_ports
    ));

    // Hot-plug state that persists across passes.
    let mut announce = false; // suppress the connect line for the boot device
    // Whether a disk was bound at the END of the previous enumeration pass, so "storage connected"
    // can be gated on an actual not-bound -> bound transition (see its use below).
    let mut disk_was_bound = false;
    let mut signaled = false; // signal_input_ready (boot-screen clear) exactly once
    let mut prev_sigs: [u32; MAX_HID] = [u32::MAX; MAX_HID]; // per-device position sigs bound last pass (u32::MAX = empty)
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
    // Consecutive probes reporting the disk's port absent or unreachable. ABOVE the enumeration loop
    // on purpose: it was declared inside it and therefore reset to zero on EVERY pass, so it could
    // only reach its threshold if twenty probes ran with no re-enumeration in between. Anything that
    // triggers a pass - a keyboard replug, a hub rescan - put it back to zero, so the disk was never
    // dropped, no "storage disconnected" was ever shown, and the port kept being probed forever.
    //
    // Evidence of absence must ACCUMULATE across passes, because absence is a property of the device,
    // not of the pass that happened to look. Reset only when the port answers "present" (below) or a
    // disk is bound.
    let mut disk_absent_seen: u32 = 0;
    // Shadow model (docs/xhci-topology.md step 1). Fed from observations the driver already makes;
    // it decides nothing. Above the loop because a topology is a fact about the MACHINE, not about
    // the pass that happened to look - the same scope error that stopped the absence counter ever
    // reaching its threshold.
    let mut topo = topo::Topo::new();
    // Heartbeat state - ABOVE `'reenum` so a re-enumeration cannot reset it (see its comment below).
    let mut last_beat = ctx.read_tsc();
    let mut passes: u64 = 0;
    // Interrupts actually delivered, as distinct from messages received. Reported in the heartbeat so
    // "are we using interrupts?" is answered by a number instead of by a log line that could not tell.
    let mut msi_count: u64 = 0;
    let mut msg_count: u64 = 0;
    let mut work_cycles: u64 = 0;
    let mut work_t0: u64 = 0;
    // Where the per-pass time actually goes, split three ways: serving (block requests + HID
    // re-arm), draining the event ring, and the hub/PORTSC scan. 4.7 ms per pass is far more than a
    // loop that usually finds nothing should cost, and counting passes cannot say which part owns it.
    let mut seg_serve: u64 = 0;
    let mut seg_drain: u64 = 0;
    let mut seg_hub:   u64 = 0;
    let mut seg_mark:  u64 = 0;
    let mut fast_waits: u64 = 0;
    let mut idle_waits: u64 = 0;
    // The vector the KERNEL programmed this controller's MSI to deliver on; an interrupt notification
    // arrives as exactly this one byte (kernel/src/ipc/message.rs), which is what makes a real IRQ
    // distinguishable from a block request on the same endpoint.
    const MSI_VECTOR: u8 = 0x28;
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
        ctx.log_fmt(format_args!(
            "xhci: reset: entering (USBCMD={:#010x} USBSTS={:#010x})",
            cmd, sts0
        ));
        mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
        spin(&ctx, "USBSTS.HCH (controller to halt)", 250, || {
            mmio.read32(op + OP_USBSTS) & STS_HCH != 0
        });
        ctx.log_fmt(format_args!(
            "xhci: reset: halted (USBSTS={:#010x})",
            mmio.read32(op + OP_USBSTS)
        ));
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
        // A REAL duration. This was `< 2_000_000` raw cycles commented "~1-2 ms" - true only on the
        // machine it was measured on, and the sixth instance of that class found on this port.
        while ctx.read_tsc().wrapping_sub(t0) < ctx.duration_cycles(2) {}
        spin(&ctx, "USBSTS.CNR to clear after HCRST", 500, || {
            mmio.read32(op + OP_USBSTS) & STS_CNR == 0
        });
        spin(&ctx, "USBCMD.HCRST to clear", 250, || {
            mmio.read32(op + OP_USBCMD) & CMD_HCRST == 0
        });
        ctx.log_fmt(format_args!(
            "xhci: reset: done (USBCMD={:#010x} USBSTS={:#010x})",
            mmio.read32(op + OP_USBCMD),
            mmio.read32(op + OP_USBSTS)
        ));
        // Rebuild DMA structures + run.
        dma.zero();
        // Scratchpad: build the SBA (N pointers to page-aligned buffers) and point
        // DCBAA[0] at it, so the controller has the runtime workspace it requires
        // (MaxScratchpadBufs); without it real xHCI drops devices after binding.
        // A controller that asks for more scratchpad than we can provide must be REFUSED, not
        // quietly short-changed: it will DMA into the buffers it thinks it owns, and the ones past
        // our cap are memory it was never granted. Silently capping turns "unsupported controller"
        // into "random corruption" (§26.7).
        if max_scratch as usize > MAX_SCRATCHPAD {
            ctx.log_fmt(format_args!(
                "xhci: controller wants {} scratchpad buffers, this arena holds {} - REFUSING to run it rather than short-change its DMA",
                max_scratch, MAX_SCRATCHPAD));
            idle(&ctx);
        }
        let n_scratch = max_scratch as usize;
        if n_scratch > 0 {
            for i in 0..n_scratch {
                dma.write64(
                    SCRATCHPAD_SBA_OFF + i * 8,
                    dma.phys_at(SCRATCHPAD_BUF_BASE + i * 0x1000),
                );
            }
            dma.write64(DCBAA_OFF, dma.phys_at(SCRATCHPAD_SBA_OFF));
        }
        wr64(&mmio, op + OP_DCBAAP, dma.phys_at(DCBAA_OFF));
        wr64(&mmio, op + OP_CRCR, dma.phys_at(CMD_RING_OFF) | 1);
        dma.write64(ERST_OFF, dma.phys_at(EVENT_RING_OFF));
        dma.write32(ERST_OFF + 8, EVENT_RING_TRBS as u32);
        mmio.write32(ir0 + 0x08, 1);
        wr64(&mmio, ir0 + 0x10, dma.phys_at(ERST_OFF));
        wr64(&mmio, ir0 + 0x18, dma.phys_at(EVENT_RING_OFF));
        mmio.write32(op + OP_CONFIG, max_slots);
        // P2 (interrupt-driven, §12): enable the interrupter so the controller raises its
        // MSI-X (kernel-programmed to vector 0x28) when it posts an event. IMAN: IE on, write
        // 1 to IP to clear any stale pending; USBCMD.INTE gates interrupts globally. The poll
        // loop still runs and acks (clears IMAN.IP) - belt-and-suspenders until P4.
        mmio.write32(ir0 + 0x00, IMAN_IE | IMAN_IP);
        let c = mmio.read32(op + OP_USBCMD);
        mmio.write32(op + OP_USBCMD, c | CMD_RS | CMD_INTE);
        spin(&ctx, "USBSTS.HCH to clear (controller to run)", 250, || {
            mmio.read32(op + OP_USBSTS) & STS_HCH == 0
        });

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
            topo.note(&ctx, 0, p, Some(psc & PORT_CCS != 0));
            ctx.log_fmt(format_args!(
                "xhci: port census {}/{}: PORTSC={:#010x} connected={} enabled={} speed={}",
                p,
                max_ports,
                psc,
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
            slot: 0,
            dci: 0,
            port: 0,
            idx: 0,
            is_mouse: false,
            hub_slot: 0,
            hub_dev: 0,
            hub_port: 0,
            hub_off: 0,
            hub_nports: 0,
        }; MAX_HID];
        let mut ndev = 0usize;
        let mut sa = SliceAlloc::new();
        let mut saw_hub = false;
        // Re-declared per enumeration pass, alongside the HID array and the slice allocator, for the
        // same reason they are: a pass re-inits the controller, so every slot, ring and context from
        // the previous one is gone. Carrying a Disk across would leave it pointing at a slot the
        // controller no longer has.
        let mut disk: Option<msc::Disk> = None;
        let mut hc_wedged = false;
        // TWO SWEEPS: every USB2 root port first, then every USB3 (SuperSpeed) one.
        //
        // USB3 ports used to be skipped outright. That was right while this driver only bound boot
        // keyboards - those are always reached through the USB2 ports, and the SuperSpeed Address
        // Device path did not complete on the Wyse, so enumerating them only issued doomed commands
        // and churned the shared event ring. Storage breaks that assumption: a USB3 stick sits on a
        // SuperSpeed port, and the Pi 4's VL805 is a USB3 controller, so "skip them" means "never
        // find the disk".
        //
        // The ORDER is what keeps the old benefit. A keyboard on a USB2 port is found and bound
        // before a single SuperSpeed command is issued, so a controller whose SS path does not
        // complete costs some log noise at the END of a pass rather than delaying input. A USB3 port
        // that fails to enumerate is handled exactly as any other failing port - `enumerate_one`
        // returns having bound nothing - so this is strictly more capable, not more fragile.
        for sweep in 0..2 {
            for p in 1..=max_ports {
                let is_usb3 = p < 64 && usb3_ports & (1u64 << p) != 0;
                if (sweep == 0) == is_usb3 {
                    continue;
                }
                // Stop only when there is nothing left to find: both HID slots full AND a disk bound.
                // The old `ndev >= MAX_HID` break ended the whole scan on a keyboard and a mouse, which
                // would have hidden a disk on any later port.
                if ndev >= MAX_HID && disk.is_some() {
                    break;
                }
                // Skip a port that WEDGED the HC on a previous pass (Item 3, Fix 1). Enumerating it again
                // would just re-halt the controller and take the keyboard down with it; the working devices
                // (e.g. the keyboard's hub on a lower port) enumerate first and stay bound. Bounded: at most
                // one re-init per poisoning port, then the pass runs to completion with them skipped.
                if p < 64 && poisoned & (1u64 << p) != 0 {
                    continue;
                }
                enumerate_one(
                    &ctx,
                    &dma,
                    &mmio,
                    dboff,
                    ir0,
                    op,
                    ctx_size,
                    p,
                    &mut sa,
                    &mut devs,
                    &mut ndev,
                    &mut saw_hub,
                    &mut disk,
                    &mut ev_idx,
                    &mut ev_cycle,
                    &mut cmd_idx,
                    &mut hc_wedged,
                );
                if hc_wedged {
                    ctx.log_fmt(format_args!(
                    "xhci: port {} wedged the HC - poisoning it and re-initialising the controller", p
                ));
                    if p < 64 {
                        poisoned |= 1u64 << p;
                    }
                    continue 'reenum;
                }
            }
        }

        // If this pass bound a disk, prove the bulk path end to end before anything depends on it: a
        // real READ(10) of sector 0, reported with what came back. A driver that reports a disk it
        // has never successfully read from pushes the first failure into the filesystem, where it
        // reads as corruption rather than as a driver that does not work (§26.7).
        // A newly-arrived disk announces itself, a boot disk does not - the same rule the HID
        // announce follows two blocks down, and for the same reason: `announce` is false on the boot
        // pass, so only a genuine plug reaches the screen.
        // Announce a TRANSITION, not a pass.
        //
        // `announce` only means "this is not the boot pass". It is set by ANY re-enumeration - and
        // unplugging the KEYBOARD causes one - so a disk that never moved got re-announced as
        // "storage connected" while the user was pulling a different device out. The console stated a
        // plug event that did not happen.
        //
        // Comparing against the previous pass makes the line mean what it says: it appears when the
        // disk goes from not-bound to bound, and stays silent when a re-enumeration merely rebinds
        // something that was already there.
        if announce && disk.is_some() && !disk_was_bound {
            notify(&ctx, "storage connected (xhci)");
        }
        disk_was_bound = disk.is_some();
        if let Some(d) = disk.as_mut() {
            let mut eaten = 0u32;
            if msc::read10(
                &ctx,
                &dma,
                &mmio,
                dboff,
                ir0,
                d,
                0,
                1,
                &mut ev_idx,
                &mut ev_cycle,
                &mut eaten,
            ) {
                // 0x55AA at offset 510 is the boot signature. Its ABSENCE is not an error - a raw or
                // GSFS-formatted stick has no MBR - so it is reported as an observation, not a verdict.
                let sig = (msc::data_read8(&dma, 510) as u16)
                    | ((msc::data_read8(&dma, 511) as u16) << 8);
                ctx.log_fmt(format_args!(
                    "xhci: USB disk sector 0 read OK - first bytes {:02x} {:02x} {:02x} {:02x}, sig={:#06x}{}",
                    msc::data_read8(&dma, 0), msc::data_read8(&dma, 1),
                    msc::data_read8(&dma, 2), msc::data_read8(&dma, 3),
                    sig,
                    if sig == 0xAA55 { " (MBR)" } else { " (no MBR - raw or GSFS)" }
                ));
            } else {
                ctx.log("xhci: USB disk sector 0 read FAILED - the disk is bound but not usable");
            }
        }

        // A DISK counts as something worth staying up for, even with no HID.
        //
        // Below this point is the "nothing is attached" path, whose job is to keep re-initialising
        // the controller and re-walking the ports until something appears. That is right when the
        // driver has found nothing - and catastrophic when it has found a disk: the Pi 4 bound its
        // stick, read GSFS off sector 0, and then re-init tore the whole thing down and started
        // over, forever. The block server never got to answer a single request.
        //
        // Guarding on `ndev` alone was the same "usable means HID" assumption that cost the two
        // fixes before this one, at the third and last level it appears. With a disk bound we fall
        // through to the poll loop, which is where block requests are served.
        if ndev == 0 && disk.is_none() {
            // Nothing usable attached. Still report input-ready once so the shell's
            // boot-screen clear fires (the keyboard may be on the other controller).
            if !signaled {
                ctx.signal_input_ready();
                signaled = true;
            }
            // Nothing is bound, so forget the previous pass's bound devices: whatever binds next is a
            // genuine new plug and must announce.
            prev_sigs = [u32::MAX; MAX_HID];
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
                        let c = mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10)
                            & PORT_CCS
                            != 0;
                        if c && base_ports & (1 << p) == 0 {
                            new_root = true;
                            break;
                        }
                        if !c {
                            base_ports &= !(1 << p);
                        }
                    }
                    if new_root {
                        break;
                    } // a front/root-port device appeared - re-walk now
                    if ctx.read_tsc().wrapping_sub(t0) >= ctx.duration_cycles(HUB_RESCAN_MS) {
                        break;
                    } // periodic re-walk
                    ctx.sleep(ctx.duration_cycles(IDLE_WAIT_MS));
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
        if !signaled {
            ctx.signal_input_ready();
            signaled = true;
        } // boot-screen clear, once
          // Announce only devices that weren't already bound on the previous pass. A
          // hot-plug re-initializes the whole controller and re-binds EVERY surviving
          // device, but a device whose port was bound last pass wasn't physically
          // touched - announcing it again ("keyboard connected" when only the mouse
          // was unplugged) would be misleading. `announce` stays false for the boot
          // pass, so the initial devices are silent regardless.
        if announce {
            for d in &devs[..ndev] {
                if !prev_sigs.contains(&dev_sig(d)) {
                    notify(
                        &ctx,
                        if d.is_mouse {
                            "mouse connected (xhci)"
                        } else {
                            "keyboard connected (xhci)"
                        },
                    );
                }
            }
        }
        // Remember which ports are bound so the next pass can tell a genuinely new
        // plug from a survivor the re-init merely re-bound.
        prev_sigs = [u32::MAX; MAX_HID];
        for (i, d) in devs[..ndev].iter().enumerate() {
            prev_sigs[i] = dev_sig(d);
        }

        // --- Poll every bound device's interrupt endpoint from one loop ---
        // The event ring is shared; transfer events are demultiplexed by slot id.
        // Each device has its own ring cursor (int_idx/int_cycle), re-arm flag, and
        // decode state (keyboard rollover buffer or mouse tracker).
        // Says the poll loop was REACHED, and with what. If block requests go unanswered and this
        // line is absent, the loop is not running and nothing inside it can be at fault.
        ctx.log_fmt(format_args!(
            "xhci: entering poll loop - {} HID(s), disk {}",
            ndev, if disk.is_some() { "BOUND" } else { "none" }));
        // Consecutive waits that ended in a TIMEOUT rather than a delivered event. It is how the
        // driver notices nothing is waking it - see the adaptive deadline below.
        // EP0 ring cursor for the DISK's hub, used only when no HID is bound (see the scan below).
        // Persistent across passes for the same reason the HID hub cursors are: the ring's producer
        // position and cycle must continue where the controller's dequeue actually sits.
        // Consecutive probes that reported the disk's port disconnected. Persists across passes -
        // a counter reset every pass could never reach two.
        // Hub ports we have already enumerated and did NOT end up binding anything from.
        //
        // "Connected" is not "newly arrived". A device we cannot bind - not a HID, not a disk, or a
        // bind that failed - leaves its port connected forever, and a scan that equates the two
        // re-enumerates on it every pass. That is the ~8-second loop on the Pi 4: hub port 4 held a
        // keyboard that did not bind, so the driver rediscovered it, re-initialised the controller,
        // failed to bind it again, and repeated - tearing down every other device each time.
        //
        // Root ports already had this concept (`poisoned`); hub ports did not. A bit is cleared when
        // its port reports DISCONNECTED, so a genuine unplug-replug is still seen as new.
        let mut hub_tried: u64 = 0;
        let mut disk_hub_cur = 0usize;
        let mut disk_hub_pcs = 1u32;
        let mut quiet_waits: u32 = 0;
        // One-shot latches so the mode is stated once each way, not on every pass.
        let mut poll_noted = false;
        let mut irq_noted = false;
        // Set only when a wait actually ended in a delivered event.
        let mut irq_seen = false;
        let mut int_idx = [0usize; MAX_HID];
        let mut int_cycle = [1u32; MAX_HID];
        let mut need_queue = [true; MAX_HID];
        // Per-device producer cursor into its parent HUB's EP0 ring, for the throttled downstream-port
        // GET_STATUS that detects a device unplugged behind a hub. Seeded past the enumeration TRBs
        // (hub_off); pcs starts at 1 (enumeration never wrapped the ring). Unused for root devices.
        let mut hub_cur = [0usize; MAX_HID];
        let mut hub_pcs = [1u32; MAX_HID];
        for d in 0..ndev {
            hub_cur[d] = devs[d].hub_off;
        }
        // Two HIDs behind the SAME hub (a keyboard AND a mouse on one back-port hub) share that hub's
        // ONE EP0 control ring, so their downstream GET_STATUS polls MUST advance ONE monotonic cursor -
        // not a per-device cursor each. The controller has a single dequeue pointer per ring; two
        // independent cursors desync it, and the second device's polls never complete -> None (the mouse
        // "port 4 status probe -> None" spam). cursor_owner[d] = the first device on d's hub; devices on
        // the same hub share its cursor, seeded to the FURTHEST enumeration point (the last device to
        // enumerate on that shared ring advanced it most). Root devices (hub_slot == 0) own their own.
        let mut cursor_owner = [0usize; MAX_HID];
        for d in 0..ndev {
            cursor_owner[d] = d;
            for e in 0..d {
                if devs[d].hub_slot != 0 && devs[e].hub_slot == devs[d].hub_slot {
                    cursor_owner[d] = e;
                    if hub_cur[d] > hub_cur[e] {
                        hub_cur[e] = hub_cur[d];
                        hub_pcs[e] = hub_pcs[d];
                    }
                    break;
                }
            }
        }
        let mut last_hub_poll = ctx.read_tsc();
        // LIVENESS HEARTBEAT. The driver must be able to say "I am still running".
        //
        // On hardware this driver went completely silent for two minutes - keyboard dead, hot-plug
        // ignored - and every detector reported healthy, because they all count FAILING probes and a
        // loop that has STOPPED produces no failures to count. The wedge repair added earlier today
        // could not fire for the same reason. "The loop stopped" was undetectable by construction.
        //
        // A heartbeat is the one signal that distinguishes "nothing to report" from "no longer
        // reporting". Rate-limited to once a minute so it cannot become the log noise the per-probe
        // diagnostic became (that one was 4% of a session and sat on the same loop that polls the
        // keyboard); over an overnight soak this is a few hundred lines and worth every one.
        //
        // It carries the pass count deliberately: a heartbeat whose counter has not moved says the
        // loop is being ENTERED but not progressing, which is a different fault from silence.
        //
        // Its state is declared ABOVE `'reenum` (with `topo` and `disk_absent_seen`, hoisted for this
        // same reason): declared here it was re-initialised by every re-enumeration, so under chaos -
        // which re-enumerates far more often than once a minute - the timer never reached its
        // interval and the heartbeat NEVER FIRED. 239,794 log lines with zero beats. A liveness
        // signal that a busy system resets is not a liveness signal, and it is the sixth time this
        // session that a mechanism sat behind a condition that could not occur when it was needed.
        let mut hub_probe_logged = false; // log the first downstream-status probe per session (diagnostic)
        let mut hub_none_logged = [false; MAX_HID]; // an inconclusive None logs at most ONCE per device (no spam)
        let mut kb_last = [[0u8; 6]; MAX_HID];
        // Auto-repeat delays calibrated to THIS machine's TSC rate (0 under QEMU -> ~2 GHz fallback),
        // so the repeat feels the same on any CPU instead of assuming ~2 GHz (the Goldmont+ Wyse ran
        // the old hardcoded delays too fast - one keypress became `qqqqq`).
        let rep_ticks = ctx.tsc_ticks_per_10ms();
        let mut kb_rep = [
            godspeed_sdk::hid::KeyRepeat::new_calibrated(rep_ticks),
            godspeed_sdk::hid::KeyRepeat::new_calibrated(rep_ticks),
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
            // Observe every root port FIRST, before anything in this pass can break out of the loop.
            //
            // The first placement was near the end, after four `break 'poll` sites, so a pass that
            // exited early never recorded anything - and the model stayed silent. Observation belongs
            // at the top for the same reason it is not gated on binding: it must happen on every
            // pass, unconditionally, or the model is describing the passes that finished rather than
            // the machine.
            for rp in 1..=max_ports {
                let c = mmio.read32(op + OP_PORTSC_BASE + (rp as usize - 1) * 0x10) & PORT_CCS != 0;
                topo.note(&ctx, 0, rp, Some(c));
            }
            // (Re-)arm each device's interrupt ring as needed, BEFORE blocking - so a fresh
            // HID report can post a transfer event (→ MSI-X) that wakes us.
            for d in 0..ndev {
                if !need_queue[d] {
                    continue;
                }
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

            // INTERRUPT-DRIVEN (§12, docs/power.md). Block until the controller's next MSI-X (a
            // device event, e.g. a keypress) or a deadline, instead of busy-yielding, so the core
            // can `hlt` between events and drops to ~0% CPU at rest. The wake is now LOCAL: the
            // xHCI MSI is co-located to this driver's OWN core (task::XHCI_CORE + pci.rs), so a
            // keypress wakes this core directly out of idle rather than paging a halted AP across
            // cores - the destination/placement drift that made the earlier attempt lag
            // (docs/power.md §11). A held key emits no new USB reports, so while one is armed we
            // wake briskly (~20 ms) to synthesise typematic auto-repeat below; when idle we sleep
            // ~250 ms as the hot-plug watchdog. Never pass 0 (recv_timeout(0) blocks FOREVER).
            let base = rep_ticks.max(1);
            // HID slots whose transfer events something else consumed this pass. BOTH the block
            // server below and the hub status checks further down can swallow a keyboard completion,
            // and either one owes the endpoint a re-arm - so the set spans them.
            let mut eaten = 0u32;
            // Set by the hub scan when the disk's own port reports DISCONNECTED. Acted on AFTER the
            // scan rather than inside it, so the scan's borrow of `disk` and the clearing of it do
            // not overlap.
            let mut disk_gone = false;
            // How long to wait before looking at the event ring ourselves.
            //
            // Where the controller's MSI reaches us, `recv_timeout` returns EARLY on the interrupt
            // and this is only a lost-wake safety net, so 250 ms costs nothing. On the Pi 4 nothing
            // programs the VL805's MSI yet (`pci::program_xhci_msi` WAS a stub; it is now programmed in arch/aarch64/pcie.rs), so this timeout IS
            // the polling interval - and 250 ms per keystroke is a quarter-second of lag on every
            // character typed.
            //
            // Rather than shorten it everywhere, which would burn ~100 wakeups/second on boards that
            // are already interrupt-driven and undo the power work, it ADAPTS: if several waits in a
            // row have timed out while a HID is bound, nothing is waking us and we must wake
            // ourselves. A single real wake-up restores the long interval.
            //
            // This is a workaround and is labelled as one. The fix is MSI - the interrupt path
            // itself now exists on this port, only the VL805's MSI programming is missing.
            // A BOUND HID means the short deadline, full stop - interrupts or not.
            //
            // This used to require `quiet_waits >= 4`, i.e. "only poll fast once we have proven
            // nothing is waking us". On the Pi 4 that produced the worst of both worlds: MSI wakes
            // arrived often enough to keep `quiet_waits` at 0, so the fallback never engaged, while
            // keyboard completions evidently did NOT each raise one - so every keystroke waited out
            // the 250 ms idle deadline. Typing was slower WITH interrupts working than without, and
            // the "quiet - polling" line never printed to say so.
            //
            // An interrupt that arrives makes `recv_timeout` return early regardless, so the short
            // deadline costs nothing where MSI is reliable and rescues latency where it is not. The
            // 250 ms deadline is now only for a controller with NO input device bound, where there is
            // nothing whose latency a human can feel.
            let polling = ndev > 0;
            // Say WHICH mode this driver is in, once each way. Otherwise "interrupts are enabled" is
            // a claim about configuration, not about behaviour - and the two differ exactly when the
            // MSI is programmed but never actually delivered, which is the failure this whole change
            // could plausibly have. The fallback works either way, so without a line here a silent
            // regression to polling is invisible.
            if polling && !poll_noted {
                // NOT necessarily a fault. With no USB activity there is nothing to interrupt
                // about, so an idle machine reaches this legitimately - the first wording called
                // that "MSI not reaching us" and made normal idle read as a broken feature.
                // What matters is whether the interrupt line ever worked, which the companion
                // message states from an observed delivery.
                ctx.log("xhci: polling at the 10ms tick alongside interrupts (input latency floor)");
                poll_noted = true;
            }
            // Announced only from an OBSERVED delivery (`irq_seen`), never from the initial state.
            // The first version tested `quiet_waits == 0`, which is true before anything has happened
            // at all - so it declared interrupt mode on the first pass of a driver that had never
            // received an interrupt, and the later fallback line read as "MSI worked then stopped".
            // It had never started. A diagnostic that reports an ASSUMPTION as an observation is
            // worse than none: it cost a whole debugging round pointed at the wrong mechanism.
            if irq_seen && !irq_noted {
                ctx.log("xhci: waking on interrupts (MSI) - not polling");
                irq_noted = true;
            }
            // ONE tick while a HID is bound. No `any_held` doubling, and no sub-tick pretence.
            //
            // Two errors were here at once, and hardware exposed both. The `any_held` branch asked for
            // `base * 2` (20 ms) and was tested FIRST - and `armed()` is true from key-down until the
            // release report - so the SLOWEST branch was selected exactly while the user was typing.
            //
            // And `base / 4` (2.5 ms) never existed: `scheduler::cycles_to_ticks` floors to whole
            // 10 ms BSP ticks and clamps to >= 1, so it was byte-identical to `base`. Commit 464d8fed
            // changed nothing, and its log line advertised a 2.5 ms floor the kernel cannot deliver.
            // A constant is only as fine-grained as the clock that implements it - tuning below the
            // quantum is tuning nothing.
            //
            // So: one tick, always, whenever a HID is bound. That halves the worst case while typing
            // and removes the held/not-held alternation, which is the IRREGULAR part and therefore the
            // part that reads as stutter rather than lag. Auto-repeat is over-served either way - its
            // interval is ~50 ms and `kb_rep.poll()` runs every pass.
            //
            // The 10 ms floor itself is the kernel's tick granularity, not a constant here; going
            // below it needs a sub-tick timed wake or a genuinely core-local interrupt (the aarch64
            // GIC currently targets every SPI at core 0 while this service is pinned to core 2).
            // Fast ONLY while a key is actually held. At rest, the hub-poll cadence.
            //
            // The 10 ms pace existed to catch HID reports that interrupts were not delivering - and
            // they were not, because the interrupter was never acked (EHB, fixed this commit's
            // parent). With interrupts genuinely arriving (218 MSI in the first 61 s on hardware),
            // a report WAKES us; nothing has to be caught by polling.
            //
            // Three things still need a timed wake, which is why this is not simply removed:
            //   - auto-repeat is SYNTHESISED locally, so a held key produces no further USB traffic
            //     and no interrupt can drive the repeat;
            //   - root-port hot-plug is read from PORTSC rather than from Port Status Change Events;
            //   - hub downstream status is polled every HUB_POLL_MS.
            // Only the first needs to be fast, and only while a key is down.
            //
            // So: `base` (one tick) while any keyboard has a repeat armed, `HUB_POLL_MS` otherwise.
            // At rest that is ~2 passes/sec instead of ~85, which is where the service's ~23% CPU
            // goes. Moving the other two onto events would let the timed wake go entirely.
            let repeat_armed = (0..ndev).any(|d| !devs[d].is_mouse && kb_rep[d].armed());
            // Which branch was actually taken, counted - because the observed pace (36 passes/sec,
            // ~28 ms per wait) matches NEITHER the 10 ms repeat branch (100/sec) nor the 500 ms idle
            // branch (2/sec), and only 5.6 wakes/sec come from messages. Reading the code cannot say
            // which; counting can.
            if repeat_armed { fast_waits = fast_waits.saturating_add(1); }
            else { idle_waits = idle_waits.saturating_add(1); }
            let deadline = if repeat_armed {
                base
            } else if polling {
                ctx.duration_cycles(HUB_POLL_MS)
            } else {
                base.saturating_mul(25)
            };
            // This is the driver's idle point, and therefore where block requests are answered.
            // Both the timed wait and the drain below used to DISCARD whatever arrived, which was
            // right when every message was an interrupt wakeup carrying no information. A block
            // request is not that: it carries a reply cap, and dropping it would hang the caller
            // forever on a disk that was working. `serve_if_block` tells them apart by that cap.
            // Close out the PREVIOUS pass's work before waiting again - this is the whole body,
            // wait excluded, which is the quantity `observe` charges us for and the one number this
            // investigation never had.
            if work_t0 != 0 {
                let now = ctx.read_tsc();
                work_cycles = work_cycles.wrapping_add(now.wrapping_sub(work_t0));
                // Whatever is left after the ack: the PORTSC sweep and the hub scan, which is where
                // the probe spin lives and therefore the first place to look for the 4.7 ms.
                seg_hub = seg_hub.wrapping_add(now.wrapping_sub(seg_mark));
            }
            let woke = ctx.recv_timeout(deadline);
            work_t0 = ctx.read_tsc();
            // Delivered event = something is waking us. Timeout = it is not.
            // An IRQ notification is a ONE-BYTE payload equal to the vector; a block request is not.
            //
            // `irq_seen` used to be set by ANY message, and this service receives block requests from
            // `block-driver` on the same endpoint - so with a disk attached it was set by disk I/O and
            // the "waking on interrupts (MSI)" line proved only that something had arrived. I read
            // that line off a hardware log and told the user interrupts were working. It was never
            // evidence. Now it means what it says.
            let is_irq = woke.as_ref().is_some_and(|m| m.payload_bytes() == [MSI_VECTOR]);
            if is_irq { msi_count = msi_count.saturating_add(1); }
            // Woken by a message that is NOT an interrupt - i.e. a block request, or anything else
            // addressed to this endpoint. Counted because the arithmetic says something is: the idle
            // deadline is HUB_POLL_MS (2 wakes/sec) and MSI runs ~5/sec, yet the loop turns ~39
            // times/sec. Roughly 32 wakes/sec are unaccounted for, and at ~3.6 ms of work each that
            // IS the service's remaining CPU. Guessing which sender it is has been the expensive move
            // this week; this makes the log say it.
            if let Some(m) = woke.as_ref() {
                if !is_irq {
                    msg_count = msg_count.saturating_add(1);
                    // Say WHAT it is, once. Either something really is sending ~32 messages/sec, or
                    // `is_irq` is wrong and these ARE interrupts miscounted - the kernel's
                    // notification carries the IRQ number, and if that is not 0x28 on this board the
                    // test above silently fails. One line settles which, and a wrong diagnostic that
                    // ends an investigation is worse than none (see the "waking on interrupts" line
                    // that cost this session a day).
                    if msg_count == 1 {
                        let p = m.payload_bytes();
                        ctx.log_fmt(format_args!(
                            "xhci: [wake] first non-IRQ message: len={} first={:#04x}",
                            p.len(), p.first().copied().unwrap_or(0)));
                    }
                }
            }
            if woke.is_some() { quiet_waits = 0; } else { quiet_waits = quiet_waits.saturating_add(1); }
            if is_irq { irq_seen = true; }
            if let Some(m) = woke {
                if !serve_if_block(&ctx, &dma, &mmio, dboff, ir0, &mut disk, &m, &mut ev_idx, &mut ev_cycle, &mut eaten) {
                    ctx.log("xhci: the USB disk stopped answering - dropping it and re-scanning (unplugged?)");
                    notify(&ctx, "storage disconnected (xhci)");
                    disk = None;
                    continue 'reenum;
                }
            }
            // Drain any further queued interrupt-event IPCs (an MSI-X mid-processing must not pile up).
            // BOUNDED, for the same reason the event drain is.
            //
            // "Drain until the queue is empty" is not a bound when a peer can refill it as fast as we
            // empty it. With the stick unplugged, `fs` retries block reads continuously; each retry
            // is a message, so this loop never returned and the KEYBOARD was never polled again. On
            // the Pi 4 that presented as typing working for a while after an unplug and then dying,
            // with the driver still visibly alive and answering.
            //
            // 64 messages per pass is far more than a settled system produces and still leaves the
            // input poll below its own latency budget. Anything left over is served next pass -
            // nothing is dropped, the service just stops starving one client to feed another.
            let mut disk_alive = true;
            // FOUR per pass, not 64.
            //
            // 64 was chosen to stop a retrying `fs` starving the input poll, and it did - but it is
            // still 64 BOT commands, each three awaited transfers, before the keyboard is looked at
            // again. That is invisible while the disk is idle and very visible during file I/O,
            // which is exactly where the user found it: typing stayed responsive through unplugs and
            // replugs, then went laggy during a `read`.
            //
            // The input poll and the block server share one loop, so the queue bound IS the input
            // latency bound. Four keeps a read's worth of work moving while capping the gap between
            // keyboard polls at a few commands rather than dozens. Throughput costs a pass; latency
            // is what a human notices.
            // ONE block request per pass.
            //
            // Not another tuning step - this is the split's GUARANTEE reached by serialisation
            // instead of preemption. The input drain runs immediately after this block, so a budget
            // of one means the gap between keystroke polls is bounded by a SINGLE BOT command rather
            // than by however many `fs` had queued. The in-kernel driver got that bound from the timer
            // interrupt; here it comes from refusing to batch.
            //
            // Throughput cost is real and accepted: a multi-sector read now takes one pass per
            // sector. Latency is what a human notices, throughput is what a progress bar notices, and
            // only one of those has been complaining.
            //
            // This is the INTERIM of docs/xhci-split.md, not its conclusion. The full fix delivers
            // reports from inside the disk wait, where they are currently seen and discarded - that
            // removes the coupling rather than bounding it, and needs the poll-loop state gathered
            // into one struct first.
            let mut served = 0u32;
            while served < 1 {
                let Some(m) = ctx.try_recv() else { break };
                served += 1;
                disk_alive &= serve_if_block(&ctx, &dma, &mmio, dboff, ir0, &mut disk, &m, &mut ev_idx, &mut ev_cycle, &mut eaten);
            }
            if !disk_alive {
                ctx.log("xhci: the USB disk stopped answering - dropping it and re-scanning (unplugged?)");
                disk = None;
                continue 'reenum;
            }
            // Ack the interrupter (clear IP, keep IE) BEFORE draining the ring, so an event
            // arriving mid-drain re-sets IP and re-arms a fresh MSI-X (no missed events).
            mmio.write32(ir0 + 0x00, IMAN_IE | IMAN_IP);

            // Drain ALL pending events. Transfer events → decode HID; other events (port
            // status change, etc.) are dequeued and ignored (hot-plug is handled by the
            // PORTSC checks below). next_event advances ERDP, which clears EHB.
            // BOUNDED. The drain exits when the ring runs dry, which is the common case - but a
            // device posting events as fast as we retire them never lets it run dry, and then this
            // loop never returns. The keyboard stops being polled, block requests stop being served,
            // and the service is alive but useless.
            //
            // Third occurrence of this class in this repo (arm A6-1 `net_rx_isr`, the aarch64 timer
            // tick, now here). Every device loop is bounded (§26.6); "it stops when the hardware
            // stops" is not a bound, because the hardware is the thing that might not stop.
            //
            // 4096 is far above any real burst - a full event ring is 256 TRBs - so reaching it means
            // a storm, not a busy moment. The next pass drains the rest; nothing is lost.
            let seg_a = ctx.read_tsc();          // block serving + HID re-arm, before the drain
            seg_serve = seg_serve.wrapping_add(seg_a.wrapping_sub(work_t0));
            let mut drained = 0u32;
            loop {
                drained += 1;
                if drained > 4096 {
                    ctx.log("xhci: event drain hit its bound - the controller is posting faster than we retire (storm?)");
                    break;
                }
                match next_event(&dma, &mmio, ir0, &mut ev_idx, &mut ev_cycle, 1) {
                    Some((TRB_TRANSFER_EVENT, _, slot_id)) => {
                        if let Some(d) = devs[..ndev].iter().position(|h| h.slot == slot_id) {
                            deliver_hid_report(&ctx, &dma, d, &devs, &mut kb_last,
                                               &mut kb_rep, &mut kb_caps, &mut mouse);
                            need_queue[d] = true;
                        }
                    }
                    Some(_) => {} // non-transfer event (port change, command, etc.) - drained
                    None => break,
                }
            }

            let seg_b = ctx.read_tsc();
            seg_drain = seg_drain.wrapping_add(seg_b.wrapping_sub(seg_a));
            seg_mark = seg_b;

            // ACK THE INTERRUPTER UNCONDITIONALLY - including when the drain consumed NOTHING.
            //
            // This is why interrupts "did not reliably" fire, and why this driver polls at all.
            //
            // The xHC sets EHB (Event Handler Busy, ERDP bit 3) at the same moment it sets IP and
            // asserts the interrupt, and it will NOT assert again for this interrupter until software
            // clears EHB by writing 1 to that bit. The only ERDP write was inside `next_event`, on the
            // path where a TRB was actually consumed - so a zero-event drain left EHB set forever.
            //
            // And a zero-event drain is the NORMAL case here, not an exotic one: enumeration consumes
            // its events through `run_command`, `control`, `msc::*` and `hub_port_status`, none of
            // which ack the interrupter. The controller asserts during those ~600 ms, nobody clears
            // EHB, and the first poll pass then finds an already-drained ring. From that point the
            // interrupter is wedged until the next full HCRST - exactly "one interrupt, then never
            // again".
            //
            // Linux ends its handler with this same unconditional write, on every path, including the
            // one where it processed no events. IP is cleared BEFORE the drain (above) and EHB AFTER
            // it, which is the required order: ack the assertion, service the ring, then release the
            // handler-busy interlock.
            wr64(&mmio, ir0 + 0x18,
                 dma.phys_at(EVENT_RING_OFF + ev_idx * TRB_SIZE) | (1 << 3));

            // Unplug detection. A device DIRECTLY on a root port is gone when its root-port CCS drops
            // (cheap MMIO read, every pass). A device BEHIND a hub changes no root PORTSC when it
            // leaves - its root port is the hub's, and the hub stays put - so it is instead detected by
            // GET_STATUSing the hub's downstream port, throttled (a control transfer, not free). Either
            // way: notify and break to fully re-initialize, re-binding whatever remains next pass.
            passes = passes.wrapping_add(1);
            // TIME the work half of the pass, not just count passes.
            //
            // Every measurement in this investigation has counted EVENTS - wakes, MSI, messages,
            // branches - and none has measured DURATION. That gap is why the two instruments cannot
            // be reconciled: `observe` charges xhci ~15% while it sits in BlockRecv, and the
            // heartbeat says it wakes 2.85 times a second. Both are consistent only if a wake costs
            // ~50 ms, which is exactly `PROBE_ANSWER_MS` - the hub probe budget, which this driver
            // SPINS on rather than blocking. If that is where the time goes, the fix is to stop
            // busy-waiting, and no amount of adjusting wake rates would ever have found it.
            if ctx.read_tsc().wrapping_sub(last_beat) > ctx.duration_cycles(HEARTBEAT_MS) {
                last_beat = ctx.read_tsc();
                // Carries the DEVICE's own elapsed seconds, so the beat can be checked against
                // itself rather than against host timestamps.
                //
                // I read a swing of 35.8 / 55.1 / 91.3 s between beats off the serial log and
                // concluded the time base was broken. It is not: the beat goes out through a BLOCKING
                // UART, so a host timestamp records when a line ARRIVED, not when it was produced,
                // and backpressure or host-side buffering can defer it by tens of seconds. The
                // aggregate was accurate to ~1% the whole time, which a wrong frequency could not be.
                //
                // With `t` on the line the reader never has to trust the host again: consecutive
                // beats must differ by 60 +/- 1. If they do, any swing in the timestamps is in the
                // OUTPUT path. If they do not, the counter genuinely is not tracking time, and the
                // next step is comparing CNTPCT deltas against BSP tick counts - two independent
                // clocks. Either way the log answers it without a rebuild.
                ctx.log_fmt(format_args!(
                    "xhci: alive - t={}s, {} passes ({} fast/{} idle), work {}ms (serve {} drain {} hub {}), {} MSI, {} msg, {} HID, disk {}",
                    ctx.epoch_secs_monotonic(), passes, fast_waits, idle_waits,
                    work_cycles / ctx.duration_cycles(1).max(1),
                    seg_serve / ctx.duration_cycles(1).max(1),
                    seg_drain / ctx.duration_cycles(1).max(1),
                    seg_hub   / ctx.duration_cycles(1).max(1),
                    msi_count, msg_count, ndev,
                    if disk.is_some() { "yes" } else { "no" }));
            }
            let hub_due =
                ctx.read_tsc().wrapping_sub(last_hub_poll) > ctx.duration_cycles(HUB_POLL_MS);
            // (declared above, before the block-serving calls - a DISK transfer can consume a HID
            // completion just as a hub check can, and both owe the same re-arm)
            for d in 0..ndev {
                let gone = if devs[d].hub_slot == 0 {
                    let portsc_off = op + OP_PORTSC_BASE + (devs[d].port as usize - 1) * 0x10;
                    mmio.read32(portsc_off) & PORT_CCS == 0
                } else if hub_due {
                    // Some(false) = the hub says its port is empty now; Some(true)/None = still there
                    // or an inconclusive read (do not false-notify on a transient control failure).
                    let owner = cursor_owner[d];
                    let (mut cur, mut pcs) = (hub_cur[owner], hub_pcs[owner]);
                    let mut abandoned = false;
                    let st = hub_port_status(
                        &ctx,
                        &dma,
                        &mmio,
                        dboff,
                        ir0,
                        devs[d].hub_slot,
                        devs[d].hub_dev as usize,
                        devs[d].hub_port,
                        &mut cur,
                        &mut pcs,
                        &mut ev_idx,
                        &mut ev_cycle,
                        &mut eaten,
                        &mut abandoned,
                    );
                    topo.note(&ctx, devs[d].hub_slot, devs[d].hub_port, st);
                    hub_cur[owner] = cur;
                    hub_pcs[owner] = pcs;
                    // Log the first probe of the session, and an inconclusive None at most ONCE per device
                    // (a None would silently disable this detection, so surface it - but never spam it).
                    if !hub_probe_logged || (st.is_none() && !hub_none_logged[d]) {
                        ctx.log_fmt(format_args!(
                            "xhci: hub slot {} port {} status probe -> {:?}",
                            devs[d].hub_slot, devs[d].hub_port, st
                        ));
                        hub_probe_logged = true;
                        if st.is_none() {
                            hub_none_logged[d] = true;
                        }
                    }
                    matches!(st, Some(false))
                } else {
                    false
                };
                if gone {
                    notify(
                        &ctx,
                        if devs[d].is_mouse {
                            "mouse disconnected (xhci)"
                        } else {
                            "keyboard disconnected (xhci)"
                        },
                    );
                    announce = true;
                    break 'poll;
                }
            }
            // New device BEHIND A HUB. A device (re)plugged into a hub's downstream port changes no root
            // PORTSC, so the root-port scan below misses it - it was only picked up on the next unrelated
            // disconnect (the "keyboard came back after a while" latency). While there is room and on the
            // throttled hub-poll tick, GET_STATUS the hub's UNBOUND downstream ports; a connected one is a
            // fresh plug, so break to re-enumerate and bind it alongside the survivor(s). Same shared
            // per-hub EP0 cursor as the disconnect check; any report it consumes is re-armed just below.
            // A9-2: this scan must run to WATCH, not only to BIND.
            //
            // It was gated on `ndev < MAX_HID` because a full HID table cannot bind an arrival. But
            // the same loop carries the disk-removal watch AND the only increment of `PROBE_FAILS`,
            // which is what triggers the halted-endpoint repair. With `MAX_HID = 2`, a keyboard plus
            // a mouse satisfies neither this nor the `ndev == 0` fallback below, so BOTH went dead:
            // an unplugged stick was never noticed and a wedged endpoint was never repaired. Hardware
            // testing used a keyboard alone (`ndev == 1`), which is exactly why it looked healthy.
            //
            // Fifth instance today of a mechanism guarded by a condition that cannot be true in the
            // failing case. The rule this keeps teaching: a guard belongs on the ACTION it protects,
            // not on the observation that feeds it. So the scan runs whenever there is something to
            // watch, and the bind-an-arrival arm carries the `ndev < MAX_HID` check itself.
            // A10-7: `hub_due` ALONE. The `(ndev < MAX_HID || disk.is_some())` qualifier still left the
            // scan dead in one case - a full HID table with NO disk - and that case carries the only
            // increment of `PROBE_FAILS`, so the halted-endpoint repair could not fire there either.
            // A9-2 fixed the disk half of exactly this and I left the other half standing.
            //
            // There is nothing to gate: the scan's cost is one control transfer per hub port per
            // `HUB_POLL_MS`, and its two jobs - watch for departures, and count failures so a wedged
            // endpoint gets repaired - are wanted whenever a hub exists, regardless of what is bound
            // behind it. The bind-an-arrival arm carries its own `ndev < MAX_HID` check, which is the
            // only thing that ever needed one.
            if hub_due {
                let mut scanned_hubs = 0u32; // hub slots already scanned this tick (scan each hub once)
                for d in 0..ndev {
                    let hub_slot = devs[d].hub_slot;
                    if hub_slot == 0 || scanned_hubs & (1 << (hub_slot & 31)) != 0 {
                        continue;
                    }
                    scanned_hubs |= 1 << (hub_slot & 31);
                    let hub_dev = devs[d].hub_dev as usize;
                    let nports = devs[d].hub_nports;
                    let owner = cursor_owner[d];
                    for hp in 1..=nports {
                        if devs[..ndev]
                            .iter()
                            .any(|h| h.hub_slot == hub_slot && h.hub_port == hp)
                        {
                            continue; // already bound on this hub port
                        }
                        // The DISK is bound on one of these ports, and it is not in `devs` - that
                        // list is HIDs only. It must not be treated as a new arrival (that
                        // re-enumerated forever), but it DOES have to be watched for LEAVING.
                        //
                        // Until now an unplug was only noticed when a block request failed, so with
                        // no I/O in flight the disk could be pulled and nothing would say so - and
                        // the next `drives` then blocked on a device that was no longer there.
                        // Absence is a fact about the hardware, not a consequence of asking.
                        if disk.as_ref().is_some_and(|dk| dk.hub_slot == hub_slot && dk.hub_port == hp) {
                            // (A `[diag] probing/probed` bracket lived here while the stall was being
                            // located. It was gated on `disk_absent_seen < 4`, which is ZERO whenever
                            // the disk is healthy - so it logged on every hub poll forever, ~4 serial
                            // lines a second at roughly a millisecond each, on the same loop that
                            // polls the keyboard. It became 20% of the serial log and a permanent
                            // latency tax. Removed once it had answered its question.
                            //
                            // The lesson is the same one this driver keeps teaching: a diagnostic on
                            // a hot path is a feature with a cost, and "only while things are going
                            // wrong" has to be a condition that is actually FALSE when they are not.
                            let (mut c2, mut p2) = (hub_cur[owner], hub_pcs[owner]);
                            let mut abandoned = false;
                            let st = hub_port_status(
                                &ctx,
                                &dma, &mmio, dboff, ir0, hub_slot, hub_dev, hp,
                                &mut c2, &mut p2, &mut ev_idx, &mut ev_cycle, &mut eaten, &mut abandoned,
                            );
                            topo.note(&ctx, hub_slot, hp, st);
                            hub_cur[owner] = c2;
                            hub_pcs[owner] = p2;
                            // Only a definite `Some(false)` counts. `None` is a FAILED probe -
                            // "unknown", not "gone" - and dropping a working disk on an unknown
                            // would unmount a filesystem over a transient.
                            // TWO consecutive disconnected reads, not one.
                            //
                            // A single `Some(false)` was enough, and it fired repeatedly on a disk
                            // that had already been dropped - each false positive forcing a full
                            // re-enumeration, which tears the KEYBOARD down and rebinds it. The
                            // symptom was not "the disk flaps", it was "the keyboard stops working",
                            // several layers from the cause.
                            //
                            // This probe is a control transfer on a ring shared with the HID status
                            // checks, so a read can come back wrong without the device having moved.
                            // A disconnect is not urgent - the port is not going to reconnect itself
                            // between two passes - so confirming it costs one poll interval and buys
                            // immunity to a single bad read.
                            // A FAILED probe counts too, once it KEEPS failing.
                            //
                            // `None` was treated as "unknown, not gone" and nothing more. Right for
                            // one bad read, wrong as a permanent rule: pulling the stick makes this
                            // transfer FAIL rather than report disconnected, so the disk was never
                            // dropped, no notification fired, and the port was probed forever. The Pi
                            // 4 log ends on exactly that line - "status probe -> None" - with the
                            // disk still bound and the keyboard dead.
                            //
                            // A port that cannot be probed REPEATEDLY is not unknown, it is
                            // unreachable. Three failures, a stronger bar than the two a clean
                            // `Some(false)` needs, because a failed transfer is weaker evidence.
                            let gone_now = match st {
                                Some(false) => {
                                    disk_absent_seen = disk_absent_seen.saturating_add(1);
                                    ctx.log_fmt(format_args!(
                                        "xhci: disk port {} reports DISCONNECTED ({}/2)", hp, disk_absent_seen));
                                    disk_absent_seen >= 2
                                }
                                // A FAILED QUESTION IS NOT AN ANSWER OF ABSENT. Ever.
                                //
                                // This counted unreachable probes and declared the disk removed at
                                // 20 of them. It was 3 before that, and raising it only moved the
                                // threshold - the hardware then logged phantom disconnect/connect
                                // pairs every 30 seconds for a stick nobody touched, each one forcing
                                // a controller reset and a re-enumeration that rebinds the KEYBOARD.
                                //
                                // The rule was wrong in kind, not in degree. `None` means WE COULD
                                // NOT ASK - the probe shares rings with HID polling and block I/O and
                                // times out when the controller is busy. That says something about
                                // our timing, nothing about the device. `topo.rs` already states this
                                // exact principle for the same reason, and this code contradicted it.
                                //
                                // A REAL removal answers: the hub is still there and reports connect
                                // = 0, which is `Some(false)` and has its own 2-strike rule. Every
                                // genuine unplug in the log took that path ("reports disconnected");
                                // every phantom took this one ("unreachable"). So refusing to
                                // conclude here loses no removal detection at all.
                                //
                                // Still LOUD, once, so a persistent inability to ask is visible
                                // rather than silently ignored (§26.7) - it just no longer invents a
                                // removal to explain itself.
                                // An abandoned probe (we returned early to deliver a keystroke) is
                                // NOT a failed one - counting it would let fast typing walk the
                                // counter toward the wedge threshold and reset a healthy endpoint.
                                // Abandoned, not failed. `eaten` alone could not express this - it is
                                // re-zeroed every pass, so a probe abandoned on one pass looked like a
                                // genuine failure on the next, and that is what walked this counter to
                                // 200 and re-initialised the controller every ~195 s.
                                None if abandoned || eaten != 0 => false,
                                None => {
                                    disk_absent_seen = disk_absent_seen.saturating_add(1);
                                    if disk_absent_seen == 20 {
                                        ctx.log_fmt(format_args!(
                                            "xhci: disk port {} unreachable 20x - NOT concluding removal (a failed probe is not an answer); check probe timing",
                                            hp));
                                    }
                                    // AT 200 THE RING IS WEDGED, NOT BUSY - recover instead of sitting dead.
                                    //
                                    // Signature from hardware: `cur` climbs 0x30 per probe while `ev_idx`
                                    // stays FROZEN - we keep posting transfers and the controller executes
                                    // none of them. That is a halted endpoint (an earlier transfer error
                                    // stalls it until a Reset Endpoint + Set TR Dequeue), not contention,
                                    // and it never clears itself: the keyboard stayed dead until reboot.
                                    //
                                    // Proper repair is Reset Endpoint + Set TR Dequeue on that endpoint.
                                    // Until that exists, a re-enumeration rebuilds the rings and gets the
                                    // machine back - the same recovery a user performs by replugging.
                                    // Nothing above the kernel may leave the machine dead, and a driver
                                    // that can see it is wedged and does nothing is exactly that.
                                    //
                                    // 200, not 20: 20 consecutive failures happen under load (172 to 832
                                    // per session were measured), and re-enumerating on those would bring
                                    // back the phantom flapping just fixed. 200 unbroken failures is a
                                    // ring that has stopped, not one that is busy.
                                    //
                                    // This does NOT claim the device is gone - the caller only announces a
                                    // disconnect when the hub actually SAID so (`Some(false)`).
                                    if disk_absent_seen >= 200 {
                                        // REPAIR THE ENDPOINT rather than re-enumerate the world.
                                        //
                                        // The previous version tore everything down and rebuilt it -
                                        // effective, but it rebinds the KEYBOARD too, which is a
                                        // visible stall for a fault that is confined to one endpoint.
                                        // Reset Endpoint + Set TR Dequeue is the operation the
                                        // hardware actually defines for this state (xHCI 4.6.8 /
                                        // 4.6.10): clear the halt, then re-point the dequeue.
                                        ctx.log_fmt(format_args!(
                                            "xhci: port {} unreachable 200x - endpoint looks HALTED (cursor advancing, no completions); resetting it",
                                            hp));
                                        disk_absent_seen = 0;
                                        let ok = reset_endpoint(
                                            &ctx, &dma, &mmio, dboff, ir0, hub_slot, 1,
                                            ep0_tr_off(hub_dev), &mut ev_idx, &mut ev_cycle,
                                            &mut cmd_idx,
                                        );
                                        // Our producer cursor MUST match the dequeue we just set, or
                                        // the two disagree about where the ring begins and it wedges
                                        // again on the next probe. Base, cycle 1 - a fresh ring.
                                        hub_cur[owner] = 0;
                                        hub_pcs[owner] = 1;
                                        // If the reset itself failed, fall back to the bigger hammer:
                                        // a failed recovery must not be quietly swallowed (§26.7), and
                                        // leaving the machine wedged is not an option.
                                        !ok
                                    } else {
                                        false
                                    }
                                }
                                Some(true) => {
                                    if disk_absent_seen != 0 {
                                        ctx.log_fmt(format_args!(
                                            "xhci: disk port {} present again after {} bad probe(s) - counter reset",
                                            hp, disk_absent_seen));
                                    }
                                    disk_absent_seen = 0;
                                    false
                                }
                            };
                            if gone_now {
                                ctx.log_fmt(format_args!(
                                    "xhci: the USB disk is gone (port {} {}) - dropping it",
                                    hp, if st.is_none() { "unreachable" } else { "reports disconnected" }));
                                // Announce a disconnect ONLY when the hub actually said so.
                                //
                                // The other way in here is now the wedged-ring recovery, where the hub
                                // never answered at all. Telling the user "storage disconnected" for a
                                // stick that is still plugged in is a false statement on the console -
                                // and re-announcing "connected" a second later is the phantom flapping
                                // this driver was just cured of. The recovery still happens and is still
                                // logged loudly by the branch that triggers it; it just does not claim a
                                // removal it did not observe.
                                if st.is_some() {
                                    notify(&ctx, "storage disconnected (xhci)");
                                }
                                disk_gone = true;
                                break;
                            }
                            continue;
                        }
                        let (mut cur, mut pcs) = (hub_cur[owner], hub_pcs[owner]);
                        // Only the `Some(..)` arms below act, so an abandoned probe (which yields
                        // `None`) is already ignored here.
                        let mut abandoned = false;
                        let _ = &mut abandoned;
                        let st = hub_port_status(
                            &ctx,
                            &dma,
                            &mmio,
                            dboff,
                            ir0,
                            hub_slot,
                            hub_dev,
                            hp,
                            &mut cur,
                            &mut pcs,
                            &mut ev_idx,
                            &mut ev_cycle,
                            &mut eaten,
                            &mut abandoned,
                        );
                        topo.note(&ctx, hub_slot, hp, st);
                        hub_cur[owner] = cur;
                        hub_pcs[owner] = pcs;
                        // WEDGE DETECTION BELONGS HERE TOO - this is where the failures actually are.
                        //
                        // The first version of this counted only in the DISK's probe path, so on a
                        // machine whose wedged endpoint is the keyboard's hub the counter never moved:
                        // 508 failed probes in a session and the reset fired ZERO times. The detector
                        // was correct and looking in the wrong place, which is indistinguishable from
                        // no detector at all.
                        //
                        // Counting consecutive failures ACROSS ports of this hub, because the halt is a
                        // property of the shared EP0 endpoint, not of one port: every port's probe
                        // rides the same ring, so they all fail together and any of them is evidence.
                        // `eaten != 0` means the probe was ABANDONED to deliver a keystroke, not that
                        // it failed. Counting it would let fast typing walk this counter to 200 and
                        // reset a perfectly healthy endpoint - a wedge repair firing because the user
                        // typed. The wedge case is unmistakable: nothing arrives at all, so `eaten`
                        // stays 0 and the count climbs as it should.
                        // Same rule for the wedge counter: an abandoned probe is evidence of
                        // nothing, so it must not push this toward the endpoint reset.
                        if st.is_none() && !abandoned && eaten == 0 {
                            let n = PROBE_FAILS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1;
                            if n >= 200 {
                                ctx.log_fmt(format_args!(
                                    "xhci: hub slot {} unreachable {}x - endpoint looks HALTED (cursor advancing, no completions); resetting it",
                                    hub_slot, n));
                                PROBE_FAILS.store(0, core::sync::atomic::Ordering::Relaxed);
                                let ok = reset_endpoint(
                                    &ctx, &dma, &mmio, dboff, ir0, hub_slot, 1,
                                    ep0_tr_off(hub_dev), &mut ev_idx, &mut ev_cycle, &mut cmd_idx,
                                );
                                // The producer cursor must match the dequeue just set, or the two
                                // disagree about where the ring starts and it wedges again at once.
                                hub_cur[owner] = 0;
                                hub_pcs[owner] = 1;
                                if !ok {
                                    ctx.log("xhci: endpoint reset FAILED - falling back to a full re-enumeration");
                                    announce = false;
                                    continue 'reenum;
                                }
                            }
                        } else {
                            PROBE_FAILS.store(0, core::sync::atomic::Ordering::Relaxed);
                        }
                        // Same rule as the disk-hub scan: connected is not newly-arrived. Without
                        // the retry mask, a port holding something we cannot bind re-enumerates the
                        // whole controller on every pass.
                        match st {
                            // The bind guard lives HERE now (see the scan gate above): only a free
                            // HID slice makes an arrival actionable. A full table still watches.
                            Some(true) if hp < 64 && ndev < MAX_HID && hub_tried & (1u64 << hp) == 0 => {
                                hub_tried |= 1u64 << hp;
                                ctx.log_fmt(format_args!(
                                    "xhci: new device on hub slot {} port {} - re-enumerating",
                                    hub_slot, hp
                                ));
                                announce = true;
                                break 'poll;
                            }
                            Some(false) if hp < 64 => hub_tried &= !(1u64 << hp),
                            _ => {}
                        }
                    }
                }
            }
            // Scan the DISK's hub too, when no HID is bound to scan it for us.
            //
            // The loop above iterates bound HIDs, so with `ndev == 0` it scans nothing - and a device
            // arriving behind a hub changes no root PORTSC, so the root-port check below cannot see it
            // either. On the Pi 4 that meant unplugging the keyboard was noticed and plugging it back
            // in was invisible: the disk was the only bound device, and nothing was watching its hub.
            //
            // Deliberately only when `ndev == 0`. With a HID bound the loop above already covers this
            // hub (they share it), and scanning it twice would consume the same events twice.
            if hub_due && ndev == 0 {
                if let Some((hs, hd, hn, hoff)) = disk
                    .as_ref()
                    .filter(|dk| dk.hub_slot != 0)
                    .map(|dk| (dk.hub_slot, dk.hub_dev as usize, dk.hub_nports, dk.hub_off))
                {
                    let (mut cur, mut pcs) = (disk_hub_cur.max(hoff), disk_hub_pcs);
                    for hp in 1..=hn {
                        // THE DISK'S OWN PORT IS PROBED HERE, not skipped.
                        //
                        // It used to `continue` past it, reasoning that the disk is "present by
                        // definition". That is true right up until it is unplugged, which is the one
                        // thing this scan exists to notice - and this is the ONLY scan that runs when
                        // no HID is bound. So: unplug the keyboard, then unplug the stick, and nothing
                        // saw it go. Exactly the reported sequence, and it explains why removal worked
                        // perfectly while the keyboard was in (the HID-driven scan covers this hub)
                        // and never worked once it was out.
                        //
                        // The concern behind the skip was real but belongs to the ARRIVAL arm: a
                        // present disk would re-trigger "device arrived" every pass. So the port is
                        // probed, and only the DISCONNECT arm acts on it - handled below.
                        let is_disk_port = disk.as_ref().is_some_and(|dk| dk.hub_port == hp);
                        // Only the `Some(..)` arms below act, so an abandoned probe (which yields
                        // `None`) is already ignored here.
                        let mut abandoned = false;
                        let _ = &mut abandoned;
                        let st = hub_port_status(
                            &ctx,
                            &dma, &mmio, dboff, ir0, hs, hd, hp,
                            &mut cur, &mut pcs, &mut ev_idx, &mut ev_cycle, &mut eaten, &mut abandoned,
                        );
                        topo.note(&ctx, hs, hp, st);
                        match st {
                            // The disk's own port answering "disconnected" is a REMOVAL. Two
                            // consecutive reads, the same two-strike rule the HID-driven scan uses,
                            // because a single bad read on a shared control ring is not evidence.
                            Some(false) if is_disk_port => {
                                disk_absent_seen = disk_absent_seen.saturating_add(1);
                                ctx.log_fmt(format_args!(
                                    "xhci: disk port {} reports DISCONNECTED ({}/2)", hp, disk_absent_seen));
                                if disk_absent_seen >= 2 {
                                    ctx.log_fmt(format_args!(
                                        "xhci: the USB disk is gone (port {} reports disconnected) - dropping it", hp));
                                    notify(&ctx, "storage disconnected (xhci)");
                                    disk_absent_seen = 0;
                                    disk_hub_cur = cur;
                                    disk_hub_pcs = pcs;
                                    disk_gone = true;
                                    break;
                                }
                            }
                            // Still there: the count must be UNBROKEN to mean anything.
                            Some(true) if is_disk_port => {
                                disk_absent_seen = 0;
                            }
                            // Connected AND not already tried: a real arrival.
                            Some(true) if hp < 64 && hub_tried & (1u64 << hp) == 0 => {
                                hub_tried |= 1u64 << hp;
                                ctx.log_fmt(format_args!(
                                    "xhci: device arrived on hub slot {} port {} - re-enumerating", hs, hp));
                                announce = true;
                                disk_hub_cur = cur;
                                disk_hub_pcs = pcs;
                                break 'poll;
                            }
                            // Gone: forget that we tried, so a replug counts as new again.
                            Some(false) if hp < 64 => hub_tried &= !(1u64 << hp),
                            _ => {}
                        }
                    }
                    disk_hub_cur = cur;
                    disk_hub_pcs = pcs;
                }
            }
            if disk_gone {
                // Drop it and re-scan. The filesystem above sees its next request fail and
                // reacquires when the stick returns (§14.3) - the same recovery a service restart
                // already asks of it.
                disk = None;
                announce = true;
                continue 'reenum;
            }
            if hub_due {
                last_hub_poll = ctx.read_tsc();
            }
            // DELIVER the report a hub check consumed, then re-arm.
            //
            // This used to re-arm and drop it ("the report is discarded; the next keystroke lands on
            // the fresh TRB"), which is exactly the dropped keys and input lag reported on hardware.
            // The keystroke was never lost - it completed into the device's DMA buffer and simply went
            // unread, so the same read the poll loop does recovers it.
            if eaten != 0 {
                for k in 0..ndev {
                    if devs[k].slot < 32 && eaten & (1 << devs[k].slot) != 0 {
                        deliver_hid_report(&ctx, &dma, k, &devs, &mut kb_last,
                                           &mut kb_rep, &mut kb_caps, &mut mouse);
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
                    let c =
                        mmio.read32(op + OP_PORTSC_BASE + (p as usize - 1) * 0x10) & PORT_CCS != 0;
                    if c && present & (1 << p) == 0 {
                        ctx.log_fmt(format_args!(
                            "xhci: new device on port {} - re-enumerating",
                            p
                        ));
                        announce = true;
                        break 'poll;
                    }
                    if !c {
                        present &= !(1 << p);
                    }
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
