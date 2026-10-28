// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `ehci` - userspace USB 2.0 (EHCI) host-controller driver (§12).
//!
//! The T630's back USB sockets are wired to the EHCI controller (PCI 00:12.0),
//! not the xHCI that the [`xhci`] driver handles - so a keyboard in the back is
//! invisible to us. This service drives the EHCI to reach it, in the same spirit
//! as the xHCI driver: a **userspace** service holding the controller's MMIO and
//! DMA capabilities, with the kernel only discovering the controller and
//! granting access. The kernel stays small (§4.4); all USB 2.0 protocol lives
//! here, `unsafe`-free behind `gs::driver`'s `Registers` and `Arena` (§18.1).
//!
//! Staged build (mirrors how xHCI was grown; every stage below is built, and E6 adds the
//! hot-plug loop):
//!   E1  read capability registers
//!   E2  DMA arena + reset + run
//!   E3  root ports → rate-matching hub enumeration
//!   E4  address the keyboard via split transactions
//!   E5  poll the interrupt endpoint, decode HID, push keystrokes
//!   B1  bind one vendor-specific device for an installed driver package (`bound.rs`, 2026-10-23)

#![no_std]
#![no_main]

mod bound;

use godspeed as gs;
use godspeed::driver::{delay, Part};
use godspeed::driver::irq::Woke;
use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::ServiceContext;
use core::cell::Cell;

// EHCI capability registers (at the MMIO base; EHCI spec §2.2).
const CAP_CAPLENGTH:  usize = 0x00; // u8  - bytes from base to the operational regs
const CAP_HCIVERSION: usize = 0x02; // u16 - BCD interface version
const CAP_HCSPARAMS:  usize = 0x04; // u32 - structural parameters
const CAP_HCCPARAMS:  usize = 0x08; // u32 - capability parameters

/// Idle forever by DRAINING our IPC endpoint, never `ctx.park()`. A registered driver that parks
/// never recv's, so a flood-storm (or any stray send) fills its 16-deep queue and it sits at 16/16
/// FOREVER - `events` stub bug in another guise. The drain parks between looks, so the core still
/// idles; it just no longer clogs. Used at every dead end where ehci has nothing left to do (no
/// controller MMIO, reset failed, no DMA arena, a hub that would not come up, or no high-speed device
/// present at boot). What it drains is ANSWERED since 2026-10-23 (`bound::idle`, which does the
/// polling and says why it polls): the supervisor's ask with "nothing bound", and a driver's request
/// with `ST_NO_DEVICE`, so neither waits out its deadline against a host that is here.
fn idle_draining(ctx: &ServiceContext) -> ! {
    let mut host = bound::Host::new(ctx);
    bound::idle(ctx, &mut host)
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // DECLARE THIS SERVICE'S NAME, once. Identity is not ambient - a service cannot ask what it is
    // called - so a traced service says. Without it every event reads `?` in the caller column, and
    // worse, every METRIC published lands under a BLANK owner: the metric key is (owner, name), so
    // ten unnamed services all collide into one row and their counters interleave. Observed as a
    // single `msgs.received 1920` belonging to nobody.
    gs::trace::as_name(&ctx, "ehci");
    // Which life of this host this is: the high half of every binding gen it gives (backlog/84 C2).
    ctx.log_fmt(format_args!("ehci: driver starting (instance {})", ctx.instance()));

    let mmio = match godspeed::driver::registers(&ctx) {
        Some(m) => m,
        None => {
            ctx.log("ehci: no controller MMIO granted - idling");
            idle_draining(&ctx);
        }
    };

    // E1: read the read-only capability registers and report what we found. This
    // is the "hello, controller" proof that the userspace driver can see the EHCI
    // hardware the kernel mapped for it.
    let caplength  = mmio.read8(CAP_CAPLENGTH);
    let hciversion = mmio.read16(CAP_HCIVERSION);
    let hcsparams  = mmio.read32(CAP_HCSPARAMS);
    let hccparams  = mmio.read32(CAP_HCCPARAMS);

    let n_ports    = hcsparams & 0xF;          // number of root-hub ports
    let ppc        = (hcsparams >> 4) & 0x1;   // port power control
    let n_cc       = (hcsparams >> 12) & 0xF;  // number of companion controllers
    let n_pcc      = (hcsparams >> 8) & 0xF;   // ports per companion controller
    let addr64     = hccparams & 0x1;          // 64-bit addressing capable
    let eecp       = (hccparams >> 8) & 0xFF;  // extended-capabilities pointer (BIOS handoff)

    ctx.log_fmt(format_args!(
        "ehci: CAPLENGTH={:#x} HCIVERSION={:#06x} ports={} ppc={} companions={} ports/cc={}",
        caplength, hciversion, n_ports, ppc, n_cc, n_pcc
    ));
    ctx.log_fmt(format_args!(
        "ehci: HCCPARAMS={:#010x} addr64={} eecp={:#x} (op regs at base+{:#x})",
        hccparams, addr64, eecp, caplength
    ));
    let _ = (ppc, n_cc, n_pcc, addr64, eecp);

    // ---------------------------------------------------------------------------
    // E2a - reset the controller and run it, then read the port status. No BIOS
    // handoff yet (E2b adds it if the firmware fights us); every wait is bounded so
    // a firmware tug-of-war times out and reports rather than hanging.
    // ---------------------------------------------------------------------------
    let op = caplength as usize; // operational registers begin at base + CAPLENGTH

    // Stop the controller if the BIOS left it running, then wait for it to halt.
    let cmd = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
    if !wait(&ctx, &mmio, op + OP_USBSTS, STS_HCHALTED, true) {
        ctx.log("ehci: WARN - controller did not halt (BIOS may still own it; E2b handoff needed)");
    }

    // Reset: set HCRESET and wait for the controller to clear it.
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_HCRESET);
    if !wait(&ctx, &mmio, op + OP_USBCMD, CMD_HCRESET, false) {
        ctx.log("ehci: WARN - HCRESET did not complete (E2b handoff needed); idling");
        idle_draining(&ctx);
    }
    ctx.log("ehci: controller reset");

    // Route all ports to the EHCI (not to companion controllers) and run.
    mmio.write32(op + OP_CONFIGFLAG, 1);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_RS);
    if wait(&ctx, &mmio, op + OP_USBSTS, STS_HCHALTED, false) {
        ctx.log("ehci: controller running");
    } else {
        ctx.log("ehci: WARN - controller did not leave halted state after run");
    }
    // Diagnostic (H1): dump the schedule registers after reset+run. If the IOMMU
    // is faulting on a garbage 0xffffffc0 pointer, one of these (or the schedule
    // they point at) is the source - e.g. an uninitialised PERIODICLISTBASE the
    // controller is walking, or a stale ASYNCLISTADDR.
    ctx.log_fmt(format_args!(
        "ehci: sched regs USBCMD={:#010x} USBSTS={:#010x} ASYNCLIST={:#010x} PERIODICLIST={:#010x}",
        mmio.read32(op + OP_USBCMD), mmio.read32(op + OP_USBSTS),
        mmio.read32(op + 0x18), mmio.read32(op + 0x14)
    ));

    // Port census: with the controller running and CONFIGFLAG set, a device on a
    // back socket should now read connected=1. PORTSC is one 32-bit reg per port.
    for p in 0..n_ports as usize {
        let psc = mmio.read32(op + OP_PORTSC0 + p * 4);
        ctx.log_fmt(format_args!(
            "ehci: port {}/{}: PORTSC={:#010x} connected={} enabled={} owner={} (1=companion)",
            p + 1, n_ports, psc,
            (psc & PORTSC_CCS != 0) as u8,
            (psc & PORTSC_PED != 0) as u8,
            (psc & PORTSC_OWNER != 0) as u8,
        ));
    }

    let dma = match godspeed::driver::arena(&ctx) {
        Some(a) => match lay_out(&ctx, a) { Some(d) => d, None => idle_draining(&ctx) },
        None => { ctx.log("ehci: no DMA arena granted - cannot enumerate; idling"); idle_draining(&ctx) }
    };
    // Whether the control-transfer timeout has been said. Owned here, by the function that runs for the driver's
    // whole life, rather than as a static (invariant 9, backlog/fixed/80 D9): a respawn is a fresh instance and says
    // it again, which it should.
    let said = Cell::new(false);
    let mut host = bound::Host::new(&ctx);
    root_ports(&ctx, &mmio, op, &dma, &said, (n_ports as usize).min(PORTS_MAX), &mut host)
}

/// The most root ports watched: HCSPARAMS' field is four bits.
const PORTS_MAX: usize = 15;

/// E3a - every root port, for the driver's life: the first with a high-speed device on it is taken (`root_port`), and
/// when that device leaves, every port is watched again. EHCI enables a port only for a HIGH-SPEED device; for the back
/// keyboard (low-speed) that means a high-speed hub - the T630's integrated rate-matching hub - with the keyboard
/// behind it. A port whose device is not high-speed stays disabled, and is passed over until that device leaves.
///
/// A hub is enumerated and its ports scanned for the rest of the driver's life (`enumerate_hub` does not return while
/// the hub works), so once one is found no other root port is watched; the T630's back sockets are all behind its one.
fn root_ports(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, op: usize, dma: &Parts, said: &Cell<bool>,
    n_ports: usize, host: &mut bound::Host,
) -> ! {
    let hw = bound::Hw { mmio, dma, op, said };
    let irq = gs::driver::irq::Irq::granted(ctx);
    // Ports passed over (not high-speed, a hub that did not come up, a device not bound), each until it leaves.
    let mut passed = 0u32;
    // The first look has no debounce: a device connected at boot has been there since power-on.
    let mut booting = true;
    loop {
        let p = 'found: loop {
            for q in 0..n_ports {
                if mmio.read32(op + OP_PORTSC0 + q * 4) & PORTSC_CCS == 0 {
                    passed &= !(1 << q);
                    continue;
                }
                if passed & (1 << q) != 0 {
                    continue;
                }
                if !booting {
                    delay::hold_parked(ctx, DEBOUNCE); // connect-debounce before reset
                }
                if reset_root_port(ctx, mmio, op, q) {
                    break 'found q;
                }
                passed |= 1 << q;
            }
            if booting {
                booting = false;
                ctx.log("ehci: no high-speed device on a root port - watching every port for one");
            }
            // Nothing to serve but answers: say so once (a report only when it differs), then wait for a plug.
            host.report_if_changed(ctx);
            serve_wake(ctx, mmio, op, &irq, &hw, host, None);
        };
        booting = false;
        if !root_port(ctx, mmio, op, dma, said, p, &irq, &hw, host) {
            passed |= 1 << p;
        }
    }
}

/// Reset root port `p` (from 0) and say what came up. `true` when the port enabled - which EHCI does only for a
/// HIGH-SPEED device: for the back keyboard (low-speed) that means a high-speed hub (the integrated rate-matching
/// hub) with the keyboard behind it. A low- or full-speed device straight on the port stays disabled.
fn reset_root_port(ctx: &ServiceContext, mmio: &godspeed::driver::Registers, op: usize, p: usize) -> bool {
    let off = op + OP_PORTSC0 + p * 4;
    // Begin reset: set Port Reset, clear Port Enable, preserve the other RW
    // bits, and write the write-1-to-clear change bits as 0 so we don't clear
    // them by accident.
    let keep = mmio.read32(off) & !PORTSC_W1C & !PORTSC_PED;
    mmio.write32(off, keep | PORTSC_RESET);
    delay::hold_parked(ctx, RESET_HOLD); // hold reset >= 50 ms (USB 2.0 §7.1.7.5)
    // End reset.
    mmio.write32(off, mmio.read32(off) & !PORTSC_W1C & !PORTSC_RESET);
    wait(ctx, mmio, off, PORTSC_RESET, false); // controller finishes (~2 ms)
    delay::hold_parked(ctx, RECOVERY);   // reset-recovery settle (RECOVERY, 20 ms)

    let psc = mmio.read32(off);
    let enabled = psc & PORTSC_PED != 0;
    ctx.log_fmt(format_args!(
        "ehci: port {} after reset: PORTSC={:#010x} enabled={} -> {}",
        p + 1, psc, enabled as u8,
        if enabled {
            "HIGH-SPEED (hub or HS device on EHCI; keyboard is behind a hub) -> E3b enumerates it"
        } else {
            "not high-speed (low/full-speed direct on the port)"
        }
    ));
    enabled
}

/// Root port `p`, enabled, its device at address 0: a hub is enumerated (`enumerate_hub`, which does not return
/// while the hub works); a device on its own is bound if it can be and SERVED until the port says it left. `true`
/// when the port is empty again, `false` when its device is to be passed over until it leaves: a hub whose own
/// bring-up failed, or a device not bound (it did not answer, or is not one this host binds).
#[allow(clippy::too_many_arguments)]
fn root_port(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, op: usize, dma: &Parts, said: &Cell<bool>, p: usize,
    irq: &gs::driver::irq::Irq, hw: &bound::Hw, host: &mut bound::Host,
) -> bool {
    let off = op + OP_PORTSC0 + p * 4;
    // What is on the port: its device descriptor's class, at the default address.
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
    if control_retry(ctx, mmio, dma, op, said, &Ep::hs(0, 64), &setup, 18, true, 3).is_none() {
        ctx.log_fmt(format_args!("ehci: the device on root port {} did not answer its descriptor", p + 1));
    } else if dma.ctl.read8(DATA_BUF + 4) == 0x09 {
        enumerate_hub(ctx, mmio, op, dma, said, host);
        return false;
    } else {
        let gen = host.next_gen();
        host.dev = bound::try_bind(ctx, hw, Ep::hs(0, 64), p as u8 + 1, 0, gen);
    }
    host.report_if_changed(ctx);
    if host.dev.is_none() {
        // Nothing bound: the other ports are watched while this one's device stays, passed over.
        return false;
    }
    // Serve the bound device's driver - or answer that nothing is bound - until the port is empty. The port is read
    // on every wake; a request wakes at once, and the deadline bounds how long an unplug goes unseen.
    while mmio.read32(off) & PORTSC_CCS != 0 {
        serve_wake(ctx, mmio, op, irq, hw, host, None);
    }
    host.unbind(ctx);
    host.report_if_changed(ctx);
    ctx.log_fmt(format_args!("ehci: root port {} is empty - watching every root port for a device", p + 1));
    true
}

/// One wait of `ROOT_WATCH_MS` at most: a message is served (`bound::Host::serve`), an interrupt acknowledged.
fn serve_wake(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, op: usize, irq: &gs::driver::irq::Irq, hw: &bound::Hw,
    host: &mut bound::Host, ring: Option<u32>,
) {
    match irq.wait(ctx, Budget::ms(ROOT_WATCH_MS)) {
        Woke::Request(m) => host.serve(ctx, Some(hw), ring, &m),
        Woke::Interrupt => {
            let sts = mmio.read32(op + OP_USBSTS);
            if sts & STS_INT_BITS != 0 {
                mmio.write32(op + OP_USBSTS, sts & STS_INT_BITS);
            }
            irq.rearm(ctx);
        }
        Woke::Timeout => {}
    }
}

/// How long a root-port watch waits before it reads the port again: how long an unplug or a plug goes unseen
/// with no request arriving to wake it sooner. A watchdog, as `POLL_DEADLINE_MS` is.
const ROOT_WATCH_MS: u64 = 250;

// --- EHCI async-schedule registers + DMA layout for the control transfer ---
const OP_CTRLDSSEGMENT: usize = 0x10; // high 32 bits of 64-bit data-structure addrs
const OP_ASYNCLISTADDR: usize = 0x18; // physical base of the async (QH) list
const CMD_ASE:          u32   = 1 << 5;  // USBCMD: Async Schedule Enable
const CMD_IAAD:         u32   = 1 << 6;  // USBCMD: Interrupt on Async Advance Doorbell
const STS_ASS:          u32   = 1 << 15; // USBSTS: Async Schedule Status
const STS_IAA:          u32   = 1 << 5;  // USBSTS: Interrupt on Async Advance

// Offsets within the CONTROL part of the arena (`Parts`), 32-byte aligned where a QH/qTD lives.
const QH_OFF:     usize = 0x000; // Queue Head (48 bytes)
const QTD_SETUP:  usize = 0x040; // SETUP-stage qTD
const QTD_DATA:   usize = 0x060; // DATA-stage qTD
const QTD_STATUS: usize = 0x080; // STATUS-stage qTD
const SETUP_PKT:  usize = 0x100; // 8-byte USB setup packet
const DATA_BUF:   usize = 0x200; // control-transfer data buffer, to the part's end
/// The control part: one transfer's QH, qTDs, setup packet and data buffer.
const CONTROL_BYTES: usize = 0x400;

/// C8-1: how long a control transfer may take before we stop waiting. A DURATION, not a read count.
///
/// It was `2_000_000_000` raw counter cycles - a duration only on the board it was worked out on, ~1 s
/// at the T630's ~2 GHz. One second, now on every board (`gs::driver::wait`).
const CTRL_XFER_WAIT: Budget = Budget::ms(1_000);
// qTD token bits.
const QTD_ACTIVE: u32 = 1 << 7;
const QTD_HALTED: u32 = 1 << 6;
const QTD_ERRMASK: u32 = (1 << 3) | (1 << 4) | (1 << 5); // XactErr | Babble | BufErr

/// EP0 endpoint addressing for a control transfer. For the high-speed hub we talk
/// directly (`hs`); for the low-speed keyboard behind it we go through the hub's
/// transaction translator via split transactions (`low`, carrying the hub address
/// and downstream port).
#[derive(Clone, Copy)]
struct Ep { addr: u8, max_packet: u32, speed: u32, hub_addr: u8, port: u8 }
impl Ep {
    /// High-speed device addressed directly (speed=2, no split).
    fn hs(addr: u8, max_packet: u32) -> Ep {
        Ep { addr, max_packet, speed: 2, hub_addr: 0, port: 0 }
    }
    /// Full-speed device behind a hub TT (speed=0, split via hub/port).
    fn full(addr: u8, max_packet: u32, hub_addr: u8, port: u8) -> Ep {
        Ep { addr, max_packet, speed: 0, hub_addr, port }
    }
    /// Low-speed device behind a hub TT (speed=1, split via hub/port).
    fn low(addr: u8, max_packet: u32, hub_addr: u8, port: u8) -> Ep {
        Ep { addr, max_packet, speed: 1, hub_addr, port }
    }
}

/// A configured low-speed HID boot device behind the hub: its assigned address,
/// the hub downstream port it sits on, its interrupt-IN endpoint number, and
/// whether it's a mouse (vs a keyboard). Filled in during enumeration; the poll
/// loop drives one QH per entry.
#[derive(Clone, Copy)]
struct HidDev { addr: u8, port: u8, ep_num: u8, is_mouse: bool }

/// Maximum HID devices polled together (keyboard + mouse covers the cases we have).
const MAX_HID: usize = 2;
/// Poll-phase DMA layout: one QH + qTD + report buffer per device, `POLL_STRIDE`
/// apart, in the POLL part (`Parts`) - its own part, after the control part, so
/// enumeration and polling never alias.
const POLL_STRIDE: usize = 0x100; // QH @ +0x00, qTD @ +0x40, report buf @ +0x80

/// The arena in its three parts, taken by name through `gs::driver::arena` (`docs/driver-library.md`
/// step 3): the control transfer's structures at 0x0 and the poll slots at 0x400, where the offset
/// constants put them, and the bound device's bulk transfers after them (`bound.rs`, `BULK_BYTES`). An
/// access past a part ends the driver naming it.
#[derive(Clone, Copy)]
struct Parts {
    ctl: Part,
    poll: Part,
    /// The bound device's bulk transfers (`bound.rs`): their queue head, descriptor and buffers.
    bulk: Part,
}

/// Lay the arena out, said once; `None`, said with the part that did not fit, is no arena.
fn lay_out(ctx: &ServiceContext, a: godspeed::driver::Arena) -> Option<Parts> {
    let mut l = a.layout();
    let laid = (|| Ok::<_, godspeed::driver::arena::NoRoom>(Parts {
        ctl: l.take("control transfer", CONTROL_BYTES, 4096)?,
        poll: l.take("poll slots", MAX_HID * POLL_STRIDE, POLL_STRIDE)?,
        bulk: l.take("bulk transfers", bound::BULK_BYTES, 4096)?,
    }))();
    match laid {
        Ok(p) => {
            ctx.log_fmt(format_args!(
                "ehci: arena laid out - control transfer at {:#x}, poll slots at {:#x}, bulk transfers at {:#x} ({} of {} bytes)",
                p.ctl.offset(), p.poll.offset(), p.bulk.offset(), l.used(), l.len()));
            Some(p)
        }
        Err(e) => {
            ctx.log_fmt(format_args!("ehci: {} - cannot enumerate; idling", e));
            None
        }
    }
}

/// Run one control transfer on EP0 of `ep`. For a low-speed `ep` the controller
/// wraps each transaction as a split through the hub TT (hub addr + port in the
/// QH). `setup` is the 8-byte setup packet; for an IN transfer up to `data_len`
/// bytes land in DATA_BUF. Returns the byte count transferred, or None on
/// error/timeout (with the qTD tokens logged). The async schedule stays enabled
/// across calls; the single QH is idle between them.
fn control(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, ep: &Ep, setup: &[u8; 8], data_len: usize, in_dir: bool,
) -> Option<usize> {
    control_data(ctx, mmio, dma, op, said, ep, setup, data_len, in_dir, &[])
}

/// `control`, with `out` as an OUT transfer's data stage (`data_len` bytes of it), for a driver's request to the
/// device this host bound for it (`bound.rs`). Only the control part is zeroed: the keyboard ring in the poll part
/// may be live, and the bound device's bulk part is its own.
fn control_data(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, ep: &Ep, setup: &[u8; 8], data_len: usize, in_dir: bool, out: &[u8],
) -> Option<usize> {
    dma.ctl.zero();
    for (i, &b) in out.iter().take(data_len).enumerate() {
        dma.ctl.write8(DATA_BUF + i, b);
    }

    let qh_phys     = dma.ctl.phys_at(QH_OFF) as u32;
    let setup_phys  = dma.ctl.phys_at(QTD_SETUP) as u32;
    let data_phys   = dma.ctl.phys_at(QTD_DATA) as u32;
    let status_phys = dma.ctl.phys_at(QTD_STATUS) as u32;
    let pkt_phys    = dma.ctl.phys_at(SETUP_PKT) as u32;
    let buf_phys    = dma.ctl.phys_at(DATA_BUF) as u32;

    // QH for EP0 @ ep.addr. Control Endpoint flag (C, bit 27) is set for non-HS
    // endpoints behind a TT; the hub address + port (dword 2) drive the split.
    let c = if ep.speed != 2 { 1u32 << 27 } else { 0 };
    dma.ctl.write32(QH_OFF + 0x00, (qh_phys & !0x1F) | (1 << 1));
    dma.ctl.write32(QH_OFF + 0x04,
        (ep.addr as u32 & 0x7F) | (ep.speed << 12) | (1 << 14) | (1 << 15)
            | (ep.max_packet << 16) | c);
    // Endpoint capabilities: Mult [31:30], Port Number [29:23], Hub Address [22:16] (EHCI 1.0
    // 3.6.2; Linux's ehci.h masks QH_HUBPORT 0x3f800000 and QH_HUBADDR 0x007f0000, and its
    // qh_make writes ttport << 23). This was `<< 22` until 2026-10-10, which put an odd port's low bit into the
    // hub address's top bit and read every port one low (backlog/fixed/80 D1).
    dma.ctl.write32(QH_OFF + 0x08,
        (1 << 30) | ((ep.hub_addr as u32 & 0x7F) << 16) | ((ep.port as u32 & 0x7F) << 23));
    dma.ctl.write32(QH_OFF + 0x0C, 0);
    dma.ctl.write32(QH_OFF + 0x10, setup_phys & !0x1F);
    dma.ctl.write32(QH_OFF + 0x14, 1);
    dma.ctl.write32(QH_OFF + 0x18, 0);

    // 8-byte setup packet → two little-endian dwords.
    let s0 = setup[0] as u32 | (setup[1] as u32) << 8 | (setup[2] as u32) << 16 | (setup[3] as u32) << 24;
    let s1 = setup[4] as u32 | (setup[5] as u32) << 8 | (setup[6] as u32) << 16 | (setup[7] as u32) << 24;
    dma.ctl.write32(SETUP_PKT + 0x00, s0);
    dma.ctl.write32(SETUP_PKT + 0x04, s1);

    // SETUP qTD (DATA0). Chains to DATA if there is a data stage, else STATUS.
    let after_setup = if data_len > 0 { data_phys } else { status_phys };
    dma.ctl.write32(QTD_SETUP + 0x00, after_setup & !0x1F);
    dma.ctl.write32(QTD_SETUP + 0x04, 1);
    dma.ctl.write32(QTD_SETUP + 0x08, QTD_ACTIVE | (2 << 8) | (3 << 10) | (8 << 16));
    dma.ctl.write32(QTD_SETUP + 0x0C, pkt_phys);

    // DATA qTD (DATA1), if any. PID IN=01 / OUT=00.
    if data_len > 0 {
        let pid = if in_dir { 1u32 } else { 0u32 };
        dma.ctl.write32(QTD_DATA + 0x00, status_phys & !0x1F);
        dma.ctl.write32(QTD_DATA + 0x04, 1);
        dma.ctl.write32(QTD_DATA + 0x08,
            QTD_ACTIVE | (pid << 8) | (3 << 10) | ((data_len as u32) << 16) | (1 << 31));
        dma.ctl.write32(QTD_DATA + 0x0C, buf_phys);
    }

    // STATUS qTD (DATA1, IOC). Opposite direction: IN unless the data stage was IN.
    let status_pid = if in_dir && data_len > 0 { 0u32 } else { 1u32 };
    dma.ctl.write32(QTD_STATUS + 0x00, 1);
    dma.ctl.write32(QTD_STATUS + 0x04, 1);
    dma.ctl.write32(QTD_STATUS + 0x08,
        QTD_ACTIVE | (status_pid << 8) | (3 << 10) | (1 << 15) | (1 << 31));
    dma.ctl.write32(QTD_STATUS + 0x0C, 0);

    // Refresh the async schedule cleanly and flush the controller's internal
    // pointer cache. On `main` the firmware's ongoing activity keeps the EHCI
    // controller coherent; once we confine xHCI the firmware abandons EHCI and a
    // stale firmware-era internal DMA pointer (read-faults at 0xffffffc0) that
    // HCRESET never scrubbed surfaces. The EHCI spec's tool for this is the
    // Interrupt-on-Async-Advance doorbell: it forces the controller to advance
    // the async schedule to a safe point and acknowledge, re-reading it from
    // ASYNCLISTADDR. We first cycle ASE off→on (ASYNCLISTADDR may only change
    // while ASE=0) to point it at our QH, then ring the doorbell.
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    if mmio.read32(op + OP_USBSTS) & STS_ASS != 0 {
        mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) & !CMD_ASE);
        wait(ctx, mmio, op + OP_USBSTS, STS_ASS, false);
    }
    mmio.write32(op + OP_ASYNCLISTADDR, qh_phys & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait(ctx, mmio, op + OP_USBSTS, STS_ASS, true);
    // Doorbell: set IAAD, wait for the controller to set IAA (bounded - if the
    // controller is wedged it simply won't fire and we proceed), then clear it.
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_IAAD);
    wait(ctx, mmio, op + OP_USBSTS, STS_IAA, true);
    mmio.write32(op + OP_USBSTS, STS_IAA); // RW1C: acknowledge the advance

    // Wait for the STATUS qTD to retire.
    // C8-1: this was `for _ in 0..10_000_000u32`. Ten million DMA reads is not a duration - it is a
    // different wall-clock wait on every machine, and nobody picked it against the device's timing.
    // The truth is still the ACTIVE bit going clear; the clock only bounds how long we keep believing
    // it might.
    // YIELD BRIEFLY, THEN PARK - the transfer completes in DMA memory, so waiting costs nothing but
    // the core, and this loop had no yield and no sleep at all.
    //
    // `CTRL_XFER_WAIT` is one SECOND, and it is spent in full whenever the transfer does not
    // complete. That is the unplugged case exactly: `wait_for_connection` asks each hub port for its
    // status every 50 ms, every one of those control transfers runs the whole budget, and the driver
    // holds the core continuously. `observe` reads `ehci` at 100% unplugged and 0% plugged in - the
    // cable is the switch. Two earlier fixes (the reset-timing delay, then `wait`) were the same shape one
    // and two layers further out, and neither was on this path: a control transfer completes on a
    // qTD bit in DMA, so it never reaches `wait`, which polls MMIO.
    //
    // A transfer that is going to succeed completes in about a millisecond, so the first 2 ms
    // (`AWAIT_SPIN`) are yielded rather than parked and the fast path is unchanged. Past that it is
    // either slow or never coming, and 10 ms of park costs nothing that matters against a one-second
    // budget.
    let done = await_hw(ctx, CTRL_XFER_WAIT, || dma.ctl.read32(QTD_STATUS + 0x08) & QTD_ACTIVE == 0);
    // Say so ONCE. A transfer that burns the full budget is the difference between a driver that is
    // waiting and one that is eating the machine, and nothing in the log distinguished them - which
    // is why this took three attempts to place. Bounded to one line so an unplugged hub cannot
    // flood.
    if !done && !said.replace(true) {
        ctx.log("ehci: a control transfer ran out its full budget (device gone?) - parking between polls");
    }
    let t_setup  = dma.ctl.read32(QTD_SETUP + 0x08);
    let t_data   = if data_len > 0 { dma.ctl.read32(QTD_DATA + 0x08) } else { 0 };
    let t_status = dma.ctl.read32(QTD_STATUS + 0x08);
    let toks = t_setup | t_data | t_status;
    if !done || toks & (QTD_HALTED | QTD_ERRMASK) != 0 {
        // A failed control transfer is normal on a faulty/dead low-speed port; the
        // caller retries and logs one summary, so don't spam a line per attempt.
        // (The per-stage tokens are kept here only as a comment trail for future
        // debugging: setup={t_setup} data={t_data} status={t_status}.)
        let _ = (t_setup, t_data, t_status);
        return None;
    }
    // Bytes actually moved = requested - (DATA token's remaining-bytes field).
    let moved = if data_len > 0 {
        data_len - ((dma.ctl.read32(QTD_DATA + 0x08) >> 16) & 0x7FFF) as usize
    } else { 0 };
    Some(moved)
}

/// E3b/E3c - address and configure the high-speed hub on the port, read its hub
/// descriptor (downstream port count), then run the E6 hot-plug loop over its ports
/// for the rest of the driver's life. Returns only when a step of the hub's own
/// bring-up fails.
fn enumerate_hub(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, op: usize, dma: &Parts, said: &Cell<bool>,
    host: &mut bound::Host,
) {
    let dma = *dma;

    // E3b: device descriptor at the default address 0.
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
    if control(ctx, mmio, &dma, op, said, &Ep::hs(0, 64), &setup, 18, true).is_none() { return; }
    let class = dma.ctl.read8(DATA_BUF + 4);
    let proto = dma.ctl.read8(DATA_BUF + 6); // hub: 0/1 = single-TT, 2 = multi-TT
    let mps0  = dma.ctl.read8(DATA_BUF + 7) as u32;
    let vid   = dma.ctl.read16(DATA_BUF + 8);
    let pid   = dma.ctl.read16(DATA_BUF + 10);
    ctx.log_fmt(format_args!(
        "ehci: DEVICE DESCRIPTOR class={:#04x} proto={} (TT type) mps0={} VID={:#06x} PID={:#06x}",
        class, proto, mps0, vid, pid));

    // E3c: assign address 1.
    let setup = [0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]; // Set_Address(1)
    if control(ctx, mmio, &dma, op, said, &Ep::hs(0, mps0), &setup, 0, false).is_none() {
        ctx.log("ehci: Set_Address failed"); return;
    }
    delay::hold_parked(ctx, RECOVERY); // SetAddress recovery (>= 2 ms)
    ctx.log("ehci: hub addressed (1)");

    // Get the configuration descriptor (first 9 bytes) for bConfigurationValue.
    let setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x09, 0x00]; // Get_Descriptor(Config), 9
    if control(ctx, mmio, &dma, op, said, &Ep::hs(1, mps0), &setup, 9, true).is_none() {
        ctx.log("ehci: Get_Config failed"); return;
    }
    let cfgval = dma.ctl.read8(DATA_BUF + 5);

    // Set configuration.
    let setup = [0x00, 0x09, cfgval, 0x00, 0x00, 0x00, 0x00, 0x00]; // Set_Configuration
    if control(ctx, mmio, &dma, op, said, &Ep::hs(1, mps0), &setup, 0, false).is_none() {
        ctx.log("ehci: Set_Configuration failed"); return;
    }
    ctx.log_fmt(format_args!("ehci: hub configured (cfg={})", cfgval));

    // Hub class descriptor → number of downstream ports.
    let setup = [0xA0, 0x06, 0x00, 0x29, 0x00, 0x00, 0x40, 0x00]; // Get_Descriptor(Hub 0x29), 64
    let n = match control(ctx, mmio, &dma, op, said, &Ep::hs(1, mps0), &setup, 64, true) {
        Some(n) => n,
        None => { ctx.log("ehci: Get_Hub_Descriptor failed"); return; }
    };
    if n < 4 { ctx.log("ehci: short hub descriptor"); return; }
    let nports  = dma.ctl.read8(DATA_BUF + 2);
    let hubchar = dma.ctl.read16(DATA_BUF + 3);
    ctx.log_fmt(format_args!(
        "ehci: HUB DESCRIPTOR ports={} characteristics={:#06x}", nports, hubchar));

    // E6 - hot-plug loop. The hub stays addressed/configured for the driver's
    // lifetime; only the downstream devices come and go. Each pass: (re)scan the
    // ports for boot HID devices, announce what connected, poll until one drops,
    // announce the drop, and loop. With nothing attached we wait on the hub's port
    // status for a new connection rather than busy-rescanning.
    // The device present at boot is not a plug event, so its "connected" line is
    // suppressed (keeps the boot screen clean); every later connect/disconnect is
    // announced on the console.
    let mut announce = false;
    // Re-scans spent on a device that IS connected and did not come up. See the bounded-retry note
    // below; reset every time a scan succeeds, so it bounds one failure episode and not the driver's
    // lifetime.
    let mut failed_rescans = 0u32;
    const MAX_FAILED_RESCANS: u32 = 3;
    loop {
        let (devs, ndev, any_failed) = scan_devices(ctx, mmio, &dma, op, said, mps0, nports, host);
        host.report_if_changed(ctx);
        if ndev == 0 {
            // A BOUND, where this used to be a binary choice between two wrong answers.
            //
            // `wait_for_connection` deliberately ignores ports that were ALREADY connected - its own
            // comment explains why, and the reason is sound: otherwise a connected-but-unusable device
            // makes this loop spin (re-scan -> fails -> wait -> still connected -> re-scan ...) and
            // burns a core. But parking loses a device that is sitting right there, which is what the
            // T630 did: the keyboard was found, failed one transfer, and was then waited past forever.
            //
            // Neither answer is 26.6's: the choice was between UNBOUNDED retrying and none. So a
            // connected device that did not come up gets a few more whole-enumeration attempts with a
            // settle between, and then the park happens exactly as before. With the config-descriptor
            // retry added above this should rarely be reached - it is here for a transfer that is
            // flaky rather than broken, which is what this hub's TT demonstrably is.
            if any_failed && failed_rescans < MAX_FAILED_RESCANS {
                failed_rescans += 1;
                ctx.log_fmt(format_args!(
                    "ehci: a connected device did not come up - re-scanning ({} of {})",
                    failed_rescans, MAX_FAILED_RESCANS));
                delay::hold_parked(ctx, DEBOUNCE);
                continue;
            }
            ctx.log("ehci: no boot keyboard/mouse attached - waiting for a connection");
            wait_for_connection(ctx, mmio, &dma, op, said, mps0, nports, host);
            announce = true; // whatever connects after a wait is a real plug event
            failed_rescans = 0; // a real plug event starts a fresh episode
            continue;
        }
        failed_rescans = 0; // something came up: this episode is over
        if announce {
            for d in &devs[..ndev] {
                notify(ctx, if d.is_mouse { "mouse connected (ehci)" } else { "keyboard connected (ehci)" });
            }
        }
        let gone = poll_devices(ctx, mmio, &dma, op, said, mps0, &devs[..ndev], host);
        notify(ctx, if devs[gone].is_mouse { "mouse disconnected (ehci)" } else { "keyboard disconnected (ehci)" });
        announce = true; // the next connect (after re-scan) is a real plug event
        delay::hold_parked(ctx, DEBOUNCE); // let the port status settle before re-scan
    }
}

/// Power the hub's downstream ports, then reset + enumerate every connected one,
/// configuring each boot keyboard/mouse at a unique address. Returns the device
/// table and how many were configured. Called once per (re)scan in the hot-plug
/// loop - addressing each device (Set_Address moves it off the default address 0)
/// before resetting the next port is what lets two devices coexist.
fn scan_devices(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, mps0: u32, nports: u8, host: &mut bound::Host,
) -> ([HidDev; MAX_HID], usize, bool) {
    let hw = bound::Hw { mmio, dma, op, said };
    // Power every downstream port (the hub does individual power switching), let
    // power settle, then enumerate each connected port.
    for port in 1..=nports {
        let setup = [0x23, 0x03, 0x08, 0x00, port, 0x00, 0x00, 0x00]; // Set_Feature(PORT_POWER)
        let _ = control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &setup, 0, false);
    }
    delay::hold_parked(ctx, RESET_HOLD); // power-on-to-power-good settle (generous)

    let mut devs = [HidDev { addr: 0, port: 0, ep_num: 0, is_mouse: false }; MAX_HID];
    let mut ndev = 0usize;
    let mut next_addr = 2u8; // 1 is the hub; devices start at 2
    let mut any_failed = false; // a connected port we couldn't enumerate (faulty port)
    for port in 1..=nports {
        // Status before reset.
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
        if control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &setup, 4, true).is_none() { continue; }
        let status = dma.ctl.read16(DATA_BUF + 0);
        ctx.log_fmt(format_args!(
            "ehci: hub port {}: status={:#06x} connected={} low_speed={}",
            port, status, (status & 1) as u8, ((status >> 9) & 1) as u8));
        // The device bound for a driver of its own is left alone while it is there: re-resetting its port would
        // reset it under its driver every time a keyboard came or went.
        if host.dev.is_some_and(|b| b.hub_port == port) {
            if status & 1 != 0 { continue; }
            host.unbind(ctx);
        }
        if status & 1 == 0 { continue; } // nothing connected

        // A high-speed device on a hub port (status bit 10) is mass storage or a
        // nested hub - never a boot keyboard/mouse, which are low/full speed. Skip
        // it: a low-speed split descriptor read to a high-speed device fails and
        // would look like a faulty port, which it is not. This is how the USB boot
        // thumbdrive, when it sits on an EHCI port, is correctly ignored (rather
        // than nagging "faulty port - try another").
        if (status >> 10) & 1 != 0 {
            ctx.log_fmt(format_args!(
                "ehci: hub port {} has a high-speed device (mass storage / hub) - not a HID, skipping",
                port));
            continue;
        }

        // Reset the port and read the device descriptor over SPLIT, retrying with a
        // FULL re-reset (+ connect-debounce + longer recovery) each attempt - the
        // Logitech keyboard's first SETUP after a single reset XactErrs, the
        // Microsoft mouse needs only one.
        let mut kep = Ep::low(0, 8, 1, port);
        let dd  = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
        let mut got = false;
        let mut high_speed = false; // device enabled at high speed after reset
        for attempt in 1..=3u32 {
            delay::hold_parked(ctx, DEBOUNCE); // connect-debounce before reset
            let s = [0x23, 0x03, 0x04, 0x00, port, 0x00, 0x00, 0x00]; // Set_Feature(PORT_RESET)
            let _ = control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &s, 0, false);
            delay::hold_parked(ctx, RESET_HOLD);
            let s = [0x23, 0x01, 0x14, 0x00, port, 0x00, 0x00, 0x00]; // Clear_Feature(C_PORT_RESET)
            let _ = control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &s, 0, false);
            delay::hold_parked(ctx, RESET_HOLD); // generous post-reset recovery

            let s = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
            let _ = control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &s, 4, true);
            let pstat = dma.ctl.read16(DATA_BUF + 0);

            // A device that enables at high speed (status bit 10) after reset is
            // mass storage or a nested hub, not a boot HID. The speed bits aren't
            // valid until the reset completes (a thumbdrive reads 0x0101 before
            // reset, 0x0503 after), so this is where we can finally tell. Abandon it
            // without retrying or marking the port faulty - it isn't.
            if (pstat >> 10) & 1 != 0 {
                // Unless it is a device this host binds for a driver of its own (`bound.rs`): read at high speed,
                // straight through the hub, with no translator.
                if host.dev.is_none() {
                    let gen = host.next_gen();
                    host.dev = bound::try_bind(ctx, &hw, Ep::hs(0, 64), 1, port, gen);
                }
                if host.dev.is_none() {
                    ctx.log_fmt(format_args!(
                        "ehci: hub port {} enabled high-speed (mass storage / hub) - not a HID, skipping",
                        port));
                }
                high_speed = true;
                break;
            }
            // FULL speed (neither the low- nor the high-speed bit): the translator is asked for full-speed
            // transactions, and the default endpoint's packet size is read first, since a full-speed device's
            // may be up to 64 and an 18-byte answer in one packet would overrun the 8 asked for. Low speed is
            // always 8, as before.
            if (pstat >> 9) & 1 == 0 {
                let d8 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00]; // Get_Descriptor(Device), 8
                let fs = Ep::full(0, 8, 1, port);
                if control_retry(ctx, mmio, dma, op, said, &fs, &d8, 8, true, 6).is_some() {
                    kep = Ep::full(0, (dma.ctl.read8(DATA_BUF + 7) as u32).max(8), 1, port);
                }
            }

            // Retry the descriptor read several times within this reset before
            // re-resetting: this keyboard's FIRST split SETUP after a reset
            // XactErrs, but a settle-and-retry on the same reset often succeeds.
            if control_retry(ctx, mmio, dma, op, said, &kep, &dd, 18, true, 6).is_some() { got = true; break; }
            ctx.log_fmt(format_args!(
                "ehci: hub port {} attempt {} failed (post-reset status={:#06x}) - re-resetting",
                port, attempt, pstat));
        }
        if high_speed { continue; } // not a HID, and not faulty - skip quietly
        if !got {
            ctx.log_fmt(format_args!("ehci: split descriptor on hub port {} failed after 3 resets", port));
            any_failed = true;
            continue;
        }
        let vid = dma.ctl.read16(DATA_BUF + 8);
        let pid = dma.ctl.read16(DATA_BUF + 10);
        ctx.log_fmt(format_args!(
            "ehci: SPLIT device (hub port {}): VID={:#06x} PID={:#06x}", port, vid, pid));

        // Read the config descriptor to identify the device: configuration value,
        // HID interface number/class/protocol (1=keyboard, 2=mouse) and the
        // interrupt-IN endpoint.
        let setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x40, 0x00]; // Get_Descriptor(Config), 64
        // RETRY, like every other transfer in this sequence. This is 64 bytes over an 8-byte low-speed
        // control endpoint - EIGHT split transactions, where the device descriptor above was two or
        // three - so it is the transfer most likely to lose one, and it was the only one calling bare
        // `control`. A single failure then `continue`d past the device, which is exactly what
        // `control_retry`'s own comment forbids: "one failed SETUP must not abandon the device".
        //
        // Observed on the HP T630 (2026-09-27): a Logitech 046d:c30a behind the AMD hub `0438:7900`
        // had its device descriptor read fine and then vanished here, and the operator had to move the
        // keyboard to a front xHCI port. Same five tries as `setup_hid` uses, for the same reason.
        //
        // WHY `xhci` NEEDS NO EQUIVALENT, so nobody adds one and nobody removes this for symmetry: every
        // endpoint context that driver programs carries CErr = 3 (`(3 << 1)` in dword 1, at every site - seven today),
        // so the xHCI CONTROLLER retries a transaction three times in hardware before it reports a
        // Transaction Error at all. The failure this retry exists for never reaches its `control()`. The
        // rule is "retry where the controller does not", NOT "every USB driver retries" - and the
        // 2026-09-27 evidence is that the same WiFi dongle enumerated cleanly on xhci twice while dwc2
        // needed a retry for it.
        if control_retry(ctx, mmio, dma, op, said, &kep, &setup, 64, true, 5).is_none() {
            ctx.log_fmt(format_args!(
                "ehci: hub port {} config descriptor failed after 5 tries - skipping this device", port));
            any_failed = true;
            continue;
        }
        let cfg_val = dma.ctl.read8(DATA_BUF + 5);
        let mut o = 0usize;
        let (mut iface, mut iclass, mut iproto, mut ep_addr, mut ep_int) = (0u8, 0u8, 0u8, 0u8, 0u8);
        while o + 2 <= 64 {
            let blen = dma.ctl.read8(DATA_BUF + o) as usize;
            if blen == 0 { break; }
            match dma.ctl.read8(DATA_BUF + o + 1) {
                0x04 if o + 8 <= 64 && iclass == 0 => {       // interface
                    iface  = dma.ctl.read8(DATA_BUF + o + 2);
                    iclass = dma.ctl.read8(DATA_BUF + o + 5);
                    iproto = dma.ctl.read8(DATA_BUF + o + 7);
                }
                0x05 if o + 7 <= 64 && ep_addr == 0 => {      // endpoint
                    if dma.ctl.read8(DATA_BUF + o + 3) & 0x03 == 0x03 { // interrupt
                        ep_addr = dma.ctl.read8(DATA_BUF + o + 2);
                        ep_int  = dma.ctl.read8(DATA_BUF + o + 6);
                    }
                }
                _ => {}
            }
            o += blen;
        }
        ctx.log_fmt(format_args!(
            "ehci: port {} HID iface={} class={:#x} protocol={} (1=kbd 2=mouse) int_ep={:#04x} interval={}",
            port, iface, iclass, iproto, ep_addr, ep_int));

        // Boot keyboard (proto 1) or mouse (proto 2): address + configure it (boot
        // protocol) and record it. Set_Address moves it off address 0 before the
        // next port is reset.
        let is_kbd   = iclass == 0x03 && iproto == 1;
        let is_mouse = iclass == 0x03 && iproto == 2;
        if !(is_kbd || is_mouse) {
            if host.dev.is_none() && bound::bindable(iclass) {
                let gen = host.next_gen();
                host.dev = bound::try_bind(ctx, &hw, kep, 1, port, gen);
                if host.dev.is_some() { continue; }
            }
            ctx.log_fmt(format_args!("ehci: hub port {} is not a boot keyboard/mouse - skipping", port));
            continue;
        }
        if ndev >= MAX_HID {
            ctx.log("ehci: more HID devices than supported - ignoring the extra one");
            continue;
        }
        ctx.log_fmt(format_args!(
            "ehci: *** boot {} on hub port {} ***", if is_mouse { "MOUSE" } else { "KEYBOARD" }, port));
        if setup_hid(ctx, mmio, dma, op, said, port, next_addr, cfg_val, iface, is_mouse) {
            devs[ndev] = HidDev { addr: next_addr, port, ep_num: ep_addr & 0x0F, is_mouse };
            ndev += 1;
            next_addr += 1;
        }
    }
    // A connected device we couldn't bring up means a faulty back port (the T630
    // hub has dead low-speed ports). Tell the user once so a dead-port plug isn't
    // silent - but only when nothing else came up, so a working device alongside
    // a dead one doesn't nag.
    if any_failed && ndev == 0 {
        notify(ctx, "a back-port device didn't enumerate (faulty port - try another)");
    }
    // `any_failed` is returned now so the hot-plug loop can tell "nothing is plugged in" from "something
    // is plugged in and would not come up". Those need opposite responses and used to get the same one.
    (devs, ndev, any_failed)
}

/// Poll the hub's downstream ports until one reports a *newly* connected device,
/// then return so the caller re-scans. Snapshots the ports already connected on
/// entry (e.g. a device sitting on the dead port that never enumerates) and only
/// returns on a port that was NOT connected - otherwise a connected-but-unusable
/// device would make the hot-plug loop spin (re-scan → fails → wait → still
/// connected → re-scan …). The async schedule is free for these hub control
/// transfers while nothing usable is attached.
fn wait_for_connection(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, mps0: u32, nports: u8, host: &mut bound::Host,
) {
    let hw = bound::Hw { mmio, dma, op, said };
    // Returns Some(true/false) on a successful status read, None if the read
    // itself failed. Distinguishing "read failed (state unknown)" from "read OK,
    // not connected" matters: a device whose split status reads are intermittently
    // flaky (e.g. the boot thumbdrive on a hub TT) would otherwise flap
    // connected→disconnected→connected and trigger a spurious "new device" return,
    // re-scanning forever. On a failed read we leave the snapshot untouched.
    let status = |port: u8| -> Option<bool> {
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
        if control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &setup, 4, true).is_some() {
            Some(dma.ctl.read16(DATA_BUF + 0) & 1 != 0)
        } else {
            None
        }
    };
    let mut base = 0u32;
    for port in 1..=nports {
        if status(port) == Some(true) { base |= 1 << port; }
    }
    loop {
        for port in 1..=nports {
            match status(port) {
                Some(true) if base & (1 << port) == 0 => return, // a new device appeared
                Some(false) => {
                    base &= !(1 << port); // a known device left; reuse its port
                    if host.dev.is_some_and(|b| b.hub_port == port) {
                        host.unbind(ctx);
                        host.report_if_changed(ctx);
                    }
                }
                _ => {}                              // unchanged, or unknown (read failed)
            }
        }
        // SLEEP THE POLL INTERVAL, DO NOT SPIN IT.
        //
        // This paced with the 50 ms debounce delay - a hard `while read_tsc() ...` spin, 50 ms
        // of it - and then slept one 10 ms quantum below. Fifty spinning against ten sleeping is about
        // 83% of a core, held for as long as nothing is plugged in. Observed on hardware: move the
        // keyboard off EHCI and `observe` shows ehci at 100% with the port empty - directly above a
        // line claiming "park, do not spin".
        //
        // The 50 ms is PACING, not hardware timing; the comment said as much ("~50 ms between
        // polls"). Nothing in the silicon needs the core held while it elapses, so sleeping satisfies
        // it exactly and costs nothing. The parked hold stays where it IS hardware timing - the reset
        // hold, the recovery settle, the connect-debounce before a port reset - which are one-shot
        // steps rather than a loop.
        // Drain our IPC endpoint while we idle here with no HID attached (the active path drains in
        // poll_devices). Without it a flood-storm clogs our 16-deep queue permanently - see xhci. What is drained
        // is served: the supervisor's ask, and the bound device's driver (`bound.rs`).
        while let Some(m) = gs::ipc::try_recv(ctx) {
            host.serve(ctx, Some(&hw), None, &m);
        }
        gs::task::sleep_ms(ctx, HOTPLUG_POLL_MS); // no HID attached: park - and actually park
    }
}

/// Run a control transfer, retrying up to `tries` times. The Logitech keyboard's
/// control endpoint over the hub TT intermittently XactErrs/times out a split
/// SETUP; control() rebuilds the QH/qTDs fresh each call, so a plain retry (with a
/// short settle between) reliably gets it through. Used for the configuration
/// transfers, which must all succeed before the keyboard is usable.
fn control_retry(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, ep: &Ep, setup: &[u8; 8], data_len: usize, in_dir: bool, tries: u32,
) -> Option<usize> {
    for _ in 0..tries {
        if let Some(n) = control(ctx, mmio, dma, op, said, ep, setup, data_len, in_dir) {
            return Some(n);
        }
        delay::hold_parked(ctx, RECOVERY); // let the TT settle before retrying
    }
    None
}

/// E5a - give a low-speed HID device (currently at the default address 0 behind
/// hub `port`) a unique `addr` and configure it in HID boot protocol so its
/// reports are the fixed 8-byte format. Each transfer retries (control_retry)
/// because this hub's split control endpoint is intermittently flaky - one failed
/// SETUP must not abandon the device.
fn setup_hid(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, port: u8, addr: u8, cfg_val: u8, iface: u8, is_mouse: bool,
) -> bool {
    let what = if is_mouse { "mouse" } else { "keyboard" };
    // Set_Address(addr) - issued while still at address 0.
    let s = [0x00, 0x05, addr, 0x00, 0x00, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, said, &Ep::low(0, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Address failed (5 tries)", what)); return false;
    }
    delay::hold_parked(ctx, RECOVERY); // SetAddress recovery
    // Set_Configuration (at the new address).
    let s = [0x00, 0x09, cfg_val, 0x00, 0x00, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, said, &Ep::low(addr, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Configuration failed (5 tries)", what)); return false;
    }
    // HID Set_Protocol(boot=0) on the interface (bmRequestType 0x21, bRequest 0x0B).
    let s = [0x21, 0x0B, 0x00, 0x00, iface, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, said, &Ep::low(addr, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Protocol(boot) failed (5 tries)", what)); return false;
    }
    ctx.log_fmt(format_args!("ehci: {} configured (addr {}, cfg {}, boot protocol)", what, addr, cfg_val));
    true
}

/// Arm an interrupt IN qTD (8-byte report, IOC, given data toggle) for the QH at
/// offset `qh` within the poll part (`Parts::poll`), and point the QH overlay at it so the controller re-fetches.
/// The qTD lives at `qh + 0x40`, its report buffer at `qh + 0x80`.
fn arm_int(dma: &Parts, qh: usize, toggle: u32) {
    let qtd = qh + 0x40;
    let qtd_phys = dma.poll.phys_at(qtd) as u32;
    let buf_phys = dma.poll.phys_at(qh + 0x80) as u32;
    dma.poll.write32(qtd + 0x00, 1);                          // next qTD = T (terminate)
    dma.poll.write32(qtd + 0x04, 1);                          // alt next  = T
    dma.poll.write32(qtd + 0x08,
        QTD_ACTIVE | (1 << 8) | (3 << 10) | (1 << 15) | (8 << 16) | (toggle << 31));
    dma.poll.write32(qtd + 0x0C, buf_phys);                   // report buffer
    dma.poll.write32(qh + 0x10, qtd_phys & !0x1F);            // QH overlay → this qTD
    dma.poll.write32(qh + 0x14, 1);
    dma.poll.write32(qh + 0x18, 0);                           // clear overlay token
}

/// E5b/E6 - poll every configured HID device's interrupt-IN endpoint over split
/// transactions and act on the reports (keystrokes → console, mouse → log).
/// Returns the index of the first device that stops responding - when a device is
/// unplugged its split transactions error (the hub TT gets no downstream response)
/// instead of NAK'ing, so a run of errored completions means it's gone. The
/// hot-plug loop announces the drop and re-scans.
///
/// Each device gets its own QH; the QHs are linked into one async ring (so the
/// controller services them all every pass) with the first marked head of the
/// reclamation list. Each QH carries the hub TT info (split), low speed, DTC=1
/// (toggle from the qTD), and - unlike a control endpoint - `C = 0` (the Control
/// Endpoint flag is for low-speed *control* endpoints only). One IN qTD per device
/// is armed at a time; with NakCnt-reload = 0 the controller keeps retrying it
/// (the device NAKs between events) until a report arrives, then the qTD retires,
/// we act on it, and re-arm with the data toggle flipped.
fn poll_devices(
    ctx: &ServiceContext, mmio: &godspeed::driver::Registers, dma: &Parts,
    op: usize, said: &Cell<bool>, mps0: u32, devs: &[HidDev], host: &mut bound::Host,
) -> usize {
    let hw = bound::Hw { mmio, dma, op, said };
    let ring = dma.poll.phys() as u32;
    let mut bound_check = wait::Since::now(ctx);
    let n = devs.len();
    ctx.log_fmt(format_args!(
        "ehci: polling {} device(s) - type at the gsh> prompt; mouse events log here", n));

    // Build a ring of one QH per device, each with an interrupt-IN qTD armed.
    for i in 0..n {
        let qh = i * POLL_STRIDE;
        let next_qh = ((i + 1) % n) * POLL_STRIDE;
        let next_phys = dma.poll.phys_at(next_qh) as u32;
        let h = if i == 0 { 1u32 << 15 } else { 0 }; // sole head of reclamation list
        dma.poll.write32(qh + 0x00, (next_phys & !0x1F) | (1 << 1)); // horiz link → next QH, typ=QH
        dma.poll.write32(qh + 0x04,
            (devs[i].addr as u32 & 0x7F) | ((devs[i].ep_num as u32) << 8)
                | (1 << 12) | (1 << 14) | h | (8 << 16)); // low speed, DTC, [head], mps 8, C=0
        dma.poll.write32(qh + 0x08,
            (1 << 30) | ((1u32 & 0x7F) << 16) | ((devs[i].port as u32 & 0x7F) << 23)); // Mult, hub 1, port [29:23]
        dma.poll.write32(qh + 0x0C, 0);
        arm_int(dma, qh, 0);
    }

    // Repoint the async list at our ring with the schedule stopped (the spec wants
    // ASYNCLISTADDR changed only while the async schedule is disabled), then run it.
    if mmio.read32(op + OP_USBSTS) & STS_ASS != 0 {
        mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) & !CMD_ASE);
        wait(ctx, mmio, op + OP_USBSTS, STS_ASS, false);
    }
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    mmio.write32(op + OP_ASYNCLISTADDR, dma.poll.phys() as u32 & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait(ctx, mmio, op + OP_USBSTS, STS_ASS, true);

    // E2 (interrupt-driven, §12): enable the controller's interrupts. The interrupt qTDs
    // already carry IOC, so a completed report sets USBSTS.USBINT and the controller asserts
    // its (level) INTx, which the kernel routes via the IOAPIC to our vector. Clear any
    // stale status first, then enable USB-interrupt + port-change. The poll loop still
    // processes reports (belt-and-suspenders); it also acks (clears USBSTS) + unmasks.
    mmio.write32(op + OP_USBSTS, STS_INT_BITS);
    mmio.write32(op + OP_USBINTR, INT_USB | INT_PCD);

    let mut toggle = [0u32; MAX_HID];
    let mut err = [0u32; MAX_HID];                        // consecutive errored completions
    let mut kb_last = [0u8; 6];                           // keyboard edge-detection state
    // WORK TIME, not pass count - and it settled an argument that reasoning could not.
    //
    // `observe` reported this driver at 100%, and the theory offered for it was that its CPU figure
    // is SAMPLED on the scheduler tick, so a task waking on every tick is observed running every
    // time it is looked at. The poll floor is one tick, so that fits, and the history fits too: a
    // 10 ms pace read 100%, a 250 ms deadline read 0%, an accidental 1 ms deadline read 100% again.
    //
    // THE THEORY WAS WRONG, and this counter is what proved it. With a device attached the driver
    // measures ~35 ms of work per 60 s - 0.058% of a core - at 93 passes/sec, so the plugged-in case
    // really is idle. Unplug the device and `observe` reads 100% and STAYS there until it is plugged
    // back in; the cable is the switch, which no sampling artefact would care about. The cost is
    // real and it is on the unplug path (backlog/13).
    //
    // Kept, and kept prominent, because a number that is argued about is a number nobody trusts: two
    // characterisations of this symptom were published from readings that were never established to
    // be comparable, and only the measurement ended it. The gap that remains is that this counter
    // lives inside `poll_devices`, so the UNPLUGGED path - the one that misbehaves - still reports
    // nothing.
    let mut passes: u64 = 0;
    let mut work_cycles: u64 = 0;
    let mut last_beat = wait::Since::now(ctx);
    // Was the current pass woken by our INTx? Set from the wait, read where a report is harvested.
    let mut irq_this_pass = false;
    // Proof that interrupts are not covering HID here, so the tick floor must be polled for.
    let mut hid_needs_poll = false;
    let mut hid_poll_noted = false;
    let mut sts_logged = false;                          // log when USBSTS first shows USBINT
    // Typematic auto-repeat, timed in TSC cycles (read_tsc is hardware-proven to advance, unlike
    // the coarse kernel tick), CALIBRATED to this machine's TSC rate so ~300 ms initial / ~50 ms
    // interval holds on any CPU - hardcoding ~2 GHz made one keypress repeat into `qqqqq` on the
    // Goldmont+ Wyse. 0 (QEMU, no USB HID) falls back to the ~2 GHz cycle counts.
    let mut kb_rep = godspeed_sdk::hid::KeyRepeat::new_calibrated(wait::ticks_per_10ms(ctx));
    let mut kb_caps = false; // Caps Lock latch (host-tracked toggle)
    // The interrupt the kernel routed at spawn: `hwclass::EHCI` -> `EHCI_MSI_VECTOR` (0x29). Its `wait`
    // tells that vector's notice from any other message, and `rearm` unmasks it.
    let irq = gs::driver::irq::Irq::granted(ctx);
    let mut mouse = godspeed_sdk::hid::MouseTracker::new(); // mouse button/motion state
    loop {
        let work_t0 = wait::Since::now(ctx);
        for i in 0..n {
            let qh = i * POLL_STRIDE;
            let qtd = qh + 0x40;
            let buf = qh + 0x80;
            if dma.poll.read32(qtd + 0x08) & QTD_ACTIVE != 0 { continue; } // still NAK'ing
            let tok = dma.poll.read32(qtd + 0x08);
            if tok & (QTD_HALTED | QTD_ERRMASK) == 0 {
                let mut rep = [0u8; 8];
                for j in 0..8 { rep[j] = dma.poll.read8(buf + j); }
                if devs[i].is_mouse {
                    mouse.feed(
                        &rep,
                        |mask, down| ctx.log_fmt(format_args!(
                            "ehci: mouse {} {}",
                            godspeed_sdk::hid::button_name(mask), if down { "down" } else { "up" })),
                        |dx, dy| ctx.log_fmt(format_args!("ehci: mouse moved dx={} dy={}", dx, dy)),
                    );
                } else if godspeed_sdk::hid::is_ctrl_alt_del(&rep) {
                    // SEC-2 follow-up: this driver holds no REBOOT and never resets the machine
                    // itself. It only SIGNALS the chord on the console stream; the shell - which
                    // legitimately holds REBOOT - decides. That keeps what SEC-2 won (no direct
                    // reboot syscall from a driver, in any context) while restoring the UX, and
                    // grants the driver nothing new: a CONSOLE_PUSH holder can already type
                    // `reboot` (§6.4).
                    gs::driver::input::push(&ctx, godspeed_sdk::hid::CTRL_ALT_DEL_SIGNAL);
                } else {
                    if !irq_this_pass { hid_needs_poll = true; }
                    godspeed_sdk::hid::decode_keyboard(
                        &rep, &mut kb_last, &mut kb_rep, &mut kb_caps, wait::ticks(ctx),
                        |ch| gs::driver::input::push(&ctx, ch),
                        |code| ctx.log_fmt(format_args!(
                            "ehci: unmapped HID key usage {:#04x} (add to sdk hid_to_ascii)", code)),
                    );
                }
                toggle[i] ^= 1;            // successful IN flips the data toggle
                err[i] = 0;               // a good report clears the error run
            } else {
                // Errored completion. A present, idle device NAKs (the qTD stays
                // ACTIVE and we `continue` above) - it never completes with error.
                // A sustained run of errored completions therefore means the device
                // was unplugged: report it so the hot-plug loop re-scans.
                err[i] += 1;
                if err[i] >= DISCONNECT_ERR_THRESHOLD {
                    return i;
                }
            }
            arm_int(dma, qh, toggle[i]);  // re-arm (on success or error alike)
        }
        // Typematic auto-repeat: a held key sends no further reports, so synthesise
        // repeats from the monotonic tick while the key stays down.
        kb_rep.poll(wait::ticks(ctx), |ch| gs::driver::input::push(&ctx, ch));
        // Diagnostic (E2): does the controller actually ASSERT its interrupt? If USBSTS.USBINT
        // sets but no IPC arrives below, the controller is asserting INTx but the IOAPIC route
        // (GSI / destination) is wrong; if it never sets, the controller isn't completing
        // interrupt transfers. One-shot so it doesn't spam.
        if !sts_logged {
            let sts = mmio.read32(op + OP_USBSTS);
            if sts & INT_USB != 0 {
                ctx.log_fmt(format_args!(
                    "ehci: USBSTS.USBINT set (controller asserting INTx), USBSTS={:#010x}", sts));
                sts_logged = true;
            }
        }
        // BUSY-POLL (not interrupt-driven). The EHCI's legacy INTx never reaches the kernel in a
        // block-and-wake model on this hardware (the controller only asserts while its async
        // schedule is kept hot by continuous re-arming - proven across many T630 flashes: the
        // kernel deliver() diagnostic fired ZERO times once the driver blocked). So this driver
        // keeps the proven busy-poll: yield each pass (preemption still shares the core) and scan
        // again. The cost is its core runs hot - accepted, because it is the ONLY model in which
        // this controller's split-transaction keyboard works. It is pinned to its own core
        // (core 3, its spawn row in the supervisor) so the system core and the interrupt-driven
        // xHCI's core stay idle.
        // (USBINTR, and the ack and re-arm after the wait below, are belt-and-suspenders: an INTx is
        // acked so it cannot storm; the qTD scan above is what actually reads the keyboard.)
        // PACED POLL (was a bare `yield_cpu`). Everything above still holds: this controller's INTx
        // will not drive a block-and-wake loop, so the driver must keep its own self-driven re-arm
        // of the async schedule. But needing to POLL never meant needing to SPIN. `yield_cpu` does
        // not sleep - it pegs the core at ~100%, and this core measured ~216k scheduler quanta/s,
        // by far the hottest thing left in the system. `sleep` parks the task so the core can halt.
        //
        // Why this is safe NOW when the earlier sleep-poll attempt was not: `sleep` granularity is
        // one scheduler quantum, and that quantum used to be an uncalibrated ~1 s on this machine,
        // so the shortest possible sleep was a whole second - precisely the lag that got that
        // attempt reverted. The APIC timer is now PIT-calibrated to a true ~10 ms, and ~10 ms is
        // also the keyboard's bInterval, so the controller has at most one fresh report to hand us
        // per pass: we are pacing to the hardware's own rate, not outrunning it.
        //
        // Residual to watch: a boot-protocol report is a state SNAPSHOT, so a press+release
        // completed entirely inside one interval could be missed. Human presses last far longer
        // than 10 ms, but fast typing is the thing to check on hardware.
        //
        // BLOCK ON THE INTERRUPT, with the poll interval as the deadline rather than the pace.
        //
        // The paragraph above this one says the controller's INTx "will not drive a block-and-wake
        // loop, so the driver must keep its own self-driven re-arm". That was TRUE when it was
        // written and is not true now, and the difference is measured rather than argued: this
        // driver's INTx was routed to `bsp_lapic_id()`, which returned an unpublished 0 on every
        // boot, so on a machine whose BSP is LAPIC id 16 the interrupt was delivered nowhere. The
        // kernel now publishes core 0's real id and the T630's own log reports
        // `irq: FIRST delivery of vector 0x29 on core 0 (routed: yes)` - the interrupt arrives, and
        // it arrives at THIS endpoint.
        //
        // So the wait is no longer a pace to be kept. It returns EARLY on the interrupt
        // and otherwise at the deadline, which makes the deadline a hot-plug watchdog instead of a
        // 100-times-a-second heartbeat. Same worst-case latency, and the core can actually halt
        // between keystrokes - the property the `sleep` above was reaching for and could not have,
        // because a task that must re-arm itself is never absent from the run queue for long.
        //
        // The wait is `gs::driver::irq`'s (2026-10-22): it tells the interrupt from anything else that
        // arrives, so only the interrupt is acked and re-armed. A message that is not the interrupt is served
        // if it is the supervisor's ask or the bound device's driver (`bound::Host::serve`, below) and dropped
        // otherwise - one per wake, and each wake runs a pass, so a flood empties at the loop's own rate. The
        // work is still driven by reading the controller at the top of the pass: what wakes us changed,
        // not what we do when woken.
        // A report that turned up on a pass no interrupt woke means interrupts are NOT covering HID
        // completions on this controller, whatever the setup reported. The xHCI shipped a 500 ms idle
        // on precisely that assumption and cost half a second per keystroke on the machine where it
        // was false; the same mistake is not worth making twice in one day, in the same shape, in the
        // driver next door. Proof, not assumption: one such report and the deadline drops to the tick
        // floor for good.
        let deadline = if hid_needs_poll {
            POLL_FLOOR_MS
        } else {
            POLL_DEADLINE_MS
        };
        if hid_needs_poll && !hid_poll_noted {
            ctx.log("ehci: a HID report arrived with no interrupt - polling input at the 10ms tick");
            hid_poll_noted = true;
        }
        passes = passes.wrapping_add(1);
        work_cycles = work_cycles.wrapping_add(work_t0.elapsed_ticks(ctx));
        if last_beat.passed(ctx, Budget::ms(60_000)) {
            last_beat = wait::Since::now(ctx);
            ctx.log_fmt(format_args!(
                "ehci: alive - {} passes, work {}ms, polling {}",
                passes, work_cycles / (wait::ticks_per_10ms(ctx) / 10).max(1),
                if hid_needs_poll { "yes (interrupt does not cover HID)" } else { "no (interrupt-driven)" }));
        }
        // Was this pass woken by our INTx, or by something else, or did the deadline simply expire?
        irq_this_pass = match irq.wait(ctx, Budget::ms(deadline)) {
            Woke::Interrupt => {
                let sts = mmio.read32(op + OP_USBSTS);
                if sts & STS_INT_BITS != 0 {
                    mmio.write32(op + OP_USBSTS, sts & STS_INT_BITS); // ack: clear W1C status bits
                }
                irq.rearm(ctx);
                true
            }
            // The supervisor's ask, or the bound device's driver: served, and the schedule pointed back at the
            // keyboard ring after any transfer (`bound.rs`). Anything else is dropped, as before.
            Woke::Request(m) => {
                host.serve(ctx, Some(&hw), Some(ring), &m);
                false
            }
            Woke::Timeout => false,
        };
        // Is the bound device still on its hub port? Asked of the hub at most once a second, as an unplug of it
        // raises nothing this loop sees - its driver's transfers would only start failing.
        if let Some(b) = host.dev {
            if bound_check.passed(ctx, Budget::ms(BOUND_CHECK_MS)) {
                bound_check = wait::Since::now(ctx);
                let setup = [0xA3, 0x00, 0x00, 0x00, b.hub_port, 0x00, 0x04, 0x00]; // Get_Status
                let gone = control(ctx, mmio, dma, op, said, &Ep::hs(1, mps0), &setup, 4, true).is_some()
                    && dma.ctl.read16(DATA_BUF) & 1 == 0;
                bound::point_async(ctx, mmio, op, ring);
                if gone {
                    host.unbind(ctx);
                    host.report_if_changed(ctx);
                }
            }
        }
    }
}

/// How often the keyboard loop asks the hub whether the bound device is still on its port.
const BOUND_CHECK_MS: u64 = 1_000;

/// Consecutive errored interrupt completions before a device is declared
/// unplugged. A present device never errors (it NAKs while idle), so this only
/// trips on a real disconnect; the bound rides out the odd transient glitch. At
/// roughly a few milliseconds per errored split this is well under a second.
const DISCONNECT_ERR_THRESHOLD: u32 = 250;

/// Print a hot-plug notice on the console, then nudge the shell to redraw its
/// prompt by pushing `NOTICE_SIGNAL` into the input ring (which this driver already
/// feeds). The notice is async output that would otherwise leave the prompt
/// scrolled up; the leading "\n" starts it on its own line, and the shell ends it
/// and draws a fresh `gsh> ` with the half-typed line. It pushed a real newline
/// until 2026-10-24, which answered any `[y/N]` question waiting on the console.
fn notify(ctx: &ServiceContext, msg: &str) {
    // Leading "\n " - the space is sacrificial: the framebuffer drops the first
    // glyph drawn on a freshly-scrolled line, so we let it eat a space, not the
    // 'U' of "USB:". (Serial is unaffected.)
    gs::io::print(ctx, "\n USB: ");
    gs::io::print(ctx, msg);
    gs::driver::input::notice_shown(&ctx);
}

// THE USB RESET TIMINGS are held PARKED by `gs::driver::delay::hold_parked`, which is this driver's
// own reset-timing delay moved into the library (`docs/driver-library.md` 1l) - the reason it parks is
// below, and is why it is a separate call from `delay::hold`, which spins. The three durations were raw
// counter cycles, right only on the T630's ~2 GHz counter; they are the times they were meant to be.
//
// They are PARKED rather than spun because of what spinning cost. The delay this replaced was once a
// bare `while read_tsc() < deadline {}` for the whole span. It hid while a device was attached - the
// driver's own heartbeat measured 35 ms of work per 60 s there - and showed the moment one was
// unplugged: the rescan loop spends `DEBOUNCE` (50 ms) plus `RESET_HOLD` (100 ms) on every turn, and
// `observe` read `ehci` at 100% with nothing plugged in and 0% when it came back. Both halves were
// observed on the T630, and the second is what ruled out a measurement artefact.
//
// How `hold_parked` still keeps the MINIMUM USB 2.0 7.1.7.5 asks for - on a calibrated clock and an
// uncalibrated one - is in its own documentation.
/// 100 ms - comfortably over the 50 ms minimum reset hold.
const RESET_HOLD: Budget = Budget::ms(100);
/// 20 ms - reset-recovery settle before reading the port.
const RECOVERY:   Budget = Budget::ms(20);
/// 50 ms - connect-debounce before resetting a hub downstream port.
const DEBOUNCE:   Budget = Budget::ms(50);

// --- EHCI operational registers (offsets from base + CAPLENGTH) + bit fields ---
const OP_USBCMD:     usize = 0x00;
const OP_USBSTS:     usize = 0x04;
const OP_USBINTR:    usize = 0x08; // Interrupt Enable register
const OP_CONFIGFLAG: usize = 0x40;
// USBINTR / USBSTS interrupt bits (E2, interrupt-driven §12). The interrupt qTDs already set
// IOC, so a completed report sets USBSTS.USBINT; enabling it raises the controller's (level)
// INTx, which the kernel routes via the IOAPIC to the vector `Irq::granted` reads. USBSTS bits are W1C.
const INT_USB:        u32 = 1 << 0; // USB Interrupt (a transfer with IOC completed)
const INT_PCD:        u32 = 1 << 2; // Port Change Detect (hot-plug)
const STS_INT_BITS:   u32 = 0x3F;   // the six W1C interrupt-status bits (0..5)
// Pacing for the idle drain (`idle_draining`) is `gs::task::sleep_quantum`. It PARKS the task, so the
// core can halt between passes; `yield_now` does not sleep at all - it spins the core at ~100%. One
// scheduler quantum is ~10 ms now that the APIC timer is PIT-calibrated, which is also the keyboard's
// own bInterval. (This was a constant, `POLL_SLEEP_CYCLES = 1`, passed to the raw cycle-count sleep.)
/// Input polling floor in MILLISECONDS, for when the interrupt is proven not to cover HID.
///
/// SEPARATE FROM the one-quantum idle pace BECAUSE THE UNITS WERE DIFFERENT, and nothing but hardware
/// could tell them apart: the raw sleep took CYCLES and the raw deadline conversion took MILLISECONDS,
/// both as a bare `u64`. Passing the cycle constant to the millisecond function asked for a 1 ms deadline
/// instead of the 10 ms tick floor this comment claimed - about a thousand wakes a second, which
/// `observe` reported as `ehci` at 100% of the core while the log looked entirely healthy.
///
/// 10 ms because that is both the scheduler's quantum and the keyboard's bInterval: the controller
/// has at most one fresh report to hand over per pass, so this paces to the hardware's own rate
/// rather than outrunning it. Asking for less does not produce more reports, only more wakes.
const POLL_FLOOR_MS: u64 = 10;
/// Deadline for the interrupt-driven main wait, in milliseconds.
///
/// This is a WATCHDOG, not a pace. The controller's INTx wakes the loop when a transfer completes,
/// so this bounds only how long the driver can go without noticing something an interrupt cannot
/// report - a device appearing or vanishing on a root port, which is read from PORTSC rather than
/// signalled. It is deliberately far longer than the old one-tick pace: 100 wakes a second was the
/// cost of having no wake source at all, and there is one now.
const POLL_DEADLINE_MS: u64 = 250;

/// How often to re-read the hub ports while waiting for something to be plugged in.
///
/// The pacing the hot-plug loop always intended ("~50 ms between polls") - the difference is that it
/// is now SLEPT rather than spun. Hot-plug latency is unchanged; a core is simply not held to get it.
const HOTPLUG_POLL_MS: u64 = 50;


// Typematic auto-repeat is CALIBRATED per-machine from the TSC rate at keyboard-poll setup
// (`KeyRepeat::new_calibrated(wait::ticks_per_10ms(ctx))`); a held key sends no further reports, so
// kb_rep synthesises the repeat off its own read_tsc clock. Hardcoding ~2 GHz repeated too fast on
// the Goldmont+ Wyse (one keypress -> `qqqqq`).
const OP_PORTSC0:    usize = 0x44; // PORTSC[0]; +4 bytes per additional port

const CMD_RS:        u32 = 1 << 0;  // Run/Stop
const CMD_HCRESET:   u32 = 1 << 1;  // Host Controller Reset
const STS_HCHALTED:  u32 = 1 << 12; // HCHalted
const PORTSC_CCS:    u32 = 1 << 0;  // Current Connect Status
const PORTSC_PED:    u32 = 1 << 2;  // Port Enabled/Disabled
const PORTSC_RESET:  u32 = 1 << 8;  // Port Reset
const PORTSC_OWNER:  u32 = 1 << 13; // Port Owner (1 = handed to a companion)
/// Write-1-to-clear change bits (CSC, PEDC, OCC) - mask off when writing PORTSC
/// so a read-modify-write does not clear them unintentionally.
const PORTSC_W1C:    u32 = (1 << 1) | (1 << 3) | (1 << 5);

/// Poll a 32-bit register until `mask` is set (`want_set=true`) or clear
/// (`false`), bounded BY TIME. Returns true if the condition was met, false on timeout.
///
/// **A COUNT IS NOT A DURATION.** This was `while i < 2_000_000` with no yield: an iteration bound,
/// which bounds nothing in wall-clock and means a different timeout on every machine - each turn is
/// an UNCACHED MMIO read across PCIe, so the same 2,000,000 is milliseconds on one box and most of a
/// second on another. Commandment VIII asks for a wait on truth with a real deadline, and 26.6 asks
/// every wait to be able to state its own bound; a loop counter can state neither.
///
/// It also never yielded. On a MULTI-CORE machine that is merely rude - this driver has a core to
/// itself and the rest of the system runs elsewhere. On a SINGLE-CORE machine it is one service
/// spinning on the only core the machine has, for however long 2,000,000 PCIe reads happen to take,
/// while every other service waits. The 10 ms quantum still preempts it, so this is not a hang - but
/// "the scheduler will eventually take it away" is not a substitute for a driver that hands the core
/// back when it has nothing to do.
fn wait(ctx: &ServiceContext, mmio: &godspeed::driver::Registers, off: usize, mask: u32, want_set: bool) -> bool {
    // Generous, because a controller coming out of BIOS ownership can be slow, and a timeout that
    // fires early turns a working controller into a reported fault.
    const TIMEOUT_MS: u64 = 250;
    await_hw(ctx, Budget::ms(TIMEOUT_MS), || (mmio.read32(off) & mask != 0) == want_set)
}

/// Look until `cond` holds or `budget` runs out: yield for the first `AWAIT_SPIN`, then park between
/// looks (a 1 ms sleep, which floors to one scheduler quantum). Both of this driver's condition waits -
/// a register (`wait`) and a control transfer's qTD - have this shape, and the reason is below.
///
/// `gs::driver::wait`'s deadlines. The loops this replaced compared `read_tsc() >= end`, which a counter
/// wrapping mid-wait ends at once.
///
/// BOTH deadlines are paced, and the spin phase's has to be. On an uncalibrated machine a paced
/// deadline's bound is the paces that fit its budget, and every look counts against it. A polling spin
/// phase never expires there (200,000 looks), so every look would be a yield and the 250 or 1,000
/// looks of the main deadline would be spent yielding - ending the wait far short of its budget, the
/// defect this library exists to remove. Paced, the spin phase ends after two looks and the rest park,
/// so the wait errs long instead. On a calibrated machine both are measured by the counter and nothing
/// changes.
fn await_hw(ctx: &ServiceContext, budget: Budget, mut cond: impl FnMut() -> bool) -> bool {
    let mut deadline = wait::Deadline::paced(ctx, budget, Budget::ms(1));
    let mut spin = wait::Deadline::paced(ctx, AWAIT_SPIN, Budget::ms(1));
    loop {
        if cond() {
            return true;
        }
        if deadline.expired() {
            return false;
        }
        // YIELD BRIEFLY, THEN PARK. The register is changed by HARDWARE, not by anything this service
        // could compute, so spinning on it buys nothing - but `yield_cpu` does not stop consuming the
        // core either. It leaves the task RUNNABLE, so a single-core scheduler hands it straight back
        // and the loop runs flat out for the whole timeout.
        //
        // That is invisible while the hardware answers quickly, which is every path this function was
        // written for. It stops being invisible when the hardware does NOT answer: pull the device and
        // the rescan loop asks each hub port for its status, every one of those waits runs out its
        // full budget, and the driver holds the core continuously. `observe` shows
        // `ehci` at 100% unplugged and 0% the moment it is plugged back in - the cable is the switch,
        // which is what says the cost is here and not in the paced poll.
        //
        // So: keep yielding for the first few milliseconds, because a controller bit that is about to
        // flip usually flips within one or two, and sleeping through that would slow every
        // enumeration. After that the answer is evidently not imminent, and a 10 ms park costs at
        // most one quantum of extra latency on an operation already measured in tens of milliseconds
        // - while cutting thousands of yields down to about two dozen polls.
        if spin.expired() {
            deadline.pause(); // a millisecond, which floors to one scheduler quantum
        } else {
            gs::task::yield_now(ctx);
        }
    }
}

/// How long `await_hw` busy-yields before parking between polls. Covers the common case (hardware
/// answers in a millisecond or two) without holding the core through a timeout that is not going to end.
const AWAIT_SPIN: Budget = Budget::ms(2);
