// SPDX-License-Identifier: GPL-2.0-only
//! `ehci` - userspace USB 2.0 (EHCI) host-controller driver (§12).
//!
//! The T630's back USB sockets are wired to the EHCI controller (PCI 00:12.0),
//! not the xHCI that the [`xhci`] driver handles - so a keyboard in the back is
//! invisible to us. This service drives the EHCI to reach it, in the same spirit
//! as the xHCI driver: a **userspace** service holding the controller's MMIO (and
//! later DMA) capability, with the kernel only discovering the controller and
//! granting access. The kernel stays small (§4.4); all USB 2.0 protocol lives
//! here, `unsafe`-free behind the SDK's audited `Mmio`/`Dma` wrappers (§18.1).
//!
//! Staged build (mirrors how xHCI was grown):
//!   E1  read capability registers (THIS stage)
//!   E2  DMA arena + reset + run
//!   E3  root ports → rate-matching hub enumeration
//!   E4  address the keyboard via split transactions
//!   E5  poll the interrupt endpoint, decode HID, push keystrokes

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

// EHCI capability registers (at the MMIO base; EHCI spec §2.2).
const CAP_CAPLENGTH:  usize = 0x00; // u8  - bytes from base to the operational regs
const CAP_HCIVERSION: usize = 0x02; // u16 - BCD interface version
const CAP_HCSPARAMS:  usize = 0x04; // u32 - structural parameters
const CAP_HCCPARAMS:  usize = 0x08; // u32 - capability parameters

/// Idle forever by DRAINING our IPC endpoint, never `ctx.park()`. A registered driver that parks
/// never recv's, so a flood-storm (or any stray send) fills its 16-deep queue and it sits at 16/16
/// FOREVER - the logger stub bug in another guise. recv() parks the task between messages, so the
/// core still idles; it just no longer clogs. Used at every dead end where ehci has nothing left to
/// do (no controller MMIO, reset failed, or no high-speed device present at boot).
fn idle_draining(ctx: &ServiceContext) -> ! {
    // Drain by POLLING (try_recv), not a blocking recv: a cross-core flood that must WAKE a deeply-blocked
    // recv on an AP is unreliable under QEMU TCG (the drain flaked in the flood-storm pin); the self-driven
    // poll drains every quantum with no wake needed. Busy-yield is fine for this rare no-controller path.
    loop { while ctx.try_recv().is_some() {} ctx.yield_cpu(); }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("ehci: driver starting");

    let mmio = match ctx.ehci_mmio() {
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
    if !wait(&mmio, op + OP_USBSTS, STS_HCHALTED, true) {
        ctx.log("ehci: WARN - controller did not halt (BIOS may still own it; E2b handoff needed)");
    }

    // Reset: set HCRESET and wait for the controller to clear it.
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_HCRESET);
    if !wait(&mmio, op + OP_USBCMD, CMD_HCRESET, false) {
        ctx.log("ehci: WARN - HCRESET did not complete (E2b handoff needed); idling");
        idle_draining(&ctx);
    }
    ctx.log("ehci: controller reset");

    // Route all ports to the EHCI (not to companion controllers) and run.
    mmio.write32(op + OP_CONFIGFLAG, 1);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_RS);
    if wait(&mmio, op + OP_USBSTS, STS_HCHALTED, false) {
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
    let mut first_connected: Option<usize> = None;
    for p in 0..n_ports as usize {
        let psc = mmio.read32(op + OP_PORTSC0 + p * 4);
        ctx.log_fmt(format_args!(
            "ehci: port {}/{}: PORTSC={:#010x} connected={} enabled={} owner={} (1=companion)",
            p + 1, n_ports, psc,
            (psc & PORTSC_CCS != 0) as u8,
            (psc & PORTSC_PED != 0) as u8,
            (psc & PORTSC_OWNER != 0) as u8,
        ));
        if psc & PORTSC_CCS != 0 && first_connected.is_none() {
            first_connected = Some(p);
        }
    }

    // ---------------------------------------------------------------------------
    // E3a - reset the first connected port and see if it enables. EHCI only
    // enables a port for a HIGH-SPEED device; if PED comes up, the thing on that
    // port is high-speed - for the back keyboard (low-speed) that means a
    // high-speed hub (the integrated rate-matching hub), with the keyboard behind
    // it. If PED stays 0, the device is low/full-speed directly on the port.
    // ---------------------------------------------------------------------------
    if let Some(p) = first_connected {
        let off = op + OP_PORTSC0 + p * 4;
        // Begin reset: set Port Reset, clear Port Enable, preserve the other RW
        // bits, and write the write-1-to-clear change bits as 0 so we don't clear
        // them by accident.
        let keep = mmio.read32(off) & !PORTSC_W1C & !PORTSC_PED;
        mmio.write32(off, keep | PORTSC_RESET);
        delay_cycles(&ctx, RESET_HOLD_CYCLES); // hold reset >= 50 ms (USB 2.0 §7.1.7.5)
        // End reset.
        mmio.write32(off, mmio.read32(off) & !PORTSC_W1C & !PORTSC_RESET);
        wait(&mmio, off, PORTSC_RESET, false); // controller finishes (~2 ms)
        delay_cycles(&ctx, RECOVERY_CYCLES);   // reset-recovery settle (~10 ms)

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

        // E3b/E3c - the device on the port (the high-speed hub) is now enabled at
        // the default address 0. Build the async schedule, address it, configure
        // it, and read its hub descriptor.
        if enabled {
            enumerate_hub(&ctx, &mmio, op);
        }
    }

    idle_draining(&ctx);
}

// --- EHCI async-schedule registers + DMA layout for the control transfer ---
const OP_CTRLDSSEGMENT: usize = 0x10; // high 32 bits of 64-bit data-structure addrs
const OP_ASYNCLISTADDR: usize = 0x18; // physical base of the async (QH) list
const CMD_ASE:          u32   = 1 << 5;  // USBCMD: Async Schedule Enable
const CMD_IAAD:         u32   = 1 << 6;  // USBCMD: Interrupt on Async Advance Doorbell
const STS_ASS:          u32   = 1 << 15; // USBSTS: Async Schedule Status
const STS_IAA:          u32   = 1 << 5;  // USBSTS: Interrupt on Async Advance

// Offsets within the granted DMA arena (32-byte aligned where a QH/qTD lives).
const QH_OFF:     usize = 0x000; // Queue Head (48 bytes)
const QTD_SETUP:  usize = 0x040; // SETUP-stage qTD
const QTD_DATA:   usize = 0x060; // DATA-stage qTD
const QTD_STATUS: usize = 0x080; // STATUS-stage qTD
const SETUP_PKT:  usize = 0x100; // 8-byte USB setup packet
const DATA_BUF:   usize = 0x200; // control-transfer data buffer

// qTD token bits.
const QTD_ACTIVE: u32 = 1 << 7;
const QTD_HALTED: u32 = 1 << 6;
const QTD_ERRMASK: u32 = (1 << 3) | (1 << 4) | (1 << 5); // XactErr | Babble | BufErr

/// EP0 endpoint addressing for a control transfer. For the high-speed hub we talk
/// directly (`hs`); for the low-speed keyboard behind it we go through the hub's
/// transaction translator via split transactions (`low`, carrying the hub address
/// and downstream port).
struct Ep { addr: u8, max_packet: u32, speed: u32, hub_addr: u8, port: u8 }
impl Ep {
    /// High-speed device addressed directly (speed=2, no split).
    fn hs(addr: u8, max_packet: u32) -> Ep {
        Ep { addr, max_packet, speed: 2, hub_addr: 0, port: 0 }
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
/// apart, starting at `POLL_BASE` - clear of the config-phase region (QH_OFF 0x0 …
/// DATA_BUF 0x200) so enumeration and polling never alias.
const POLL_BASE:   usize = 0x400;
const POLL_STRIDE: usize = 0x100; // QH @ +0x00, qTD @ +0x40, report buf @ +0x80

/// Run one control transfer on EP0 of `ep`. For a low-speed `ep` the controller
/// wraps each transaction as a split through the hub TT (hub addr + port in the
/// QH). `setup` is the 8-byte setup packet; for an IN transfer up to `data_len`
/// bytes land in DATA_BUF. Returns the byte count transferred, or None on
/// error/timeout (with the qTD tokens logged). The async schedule stays enabled
/// across calls; the single QH is idle between them.
fn control(
    _ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, ep: &Ep, setup: &[u8; 8], data_len: usize, in_dir: bool,
) -> Option<usize> {
    dma.zero();

    let qh_phys     = dma.phys_at(QH_OFF) as u32;
    let setup_phys  = dma.phys_at(QTD_SETUP) as u32;
    let data_phys   = dma.phys_at(QTD_DATA) as u32;
    let status_phys = dma.phys_at(QTD_STATUS) as u32;
    let pkt_phys    = dma.phys_at(SETUP_PKT) as u32;
    let buf_phys    = dma.phys_at(DATA_BUF) as u32;

    // QH for EP0 @ ep.addr. Control Endpoint flag (C, bit 27) is set for non-HS
    // endpoints behind a TT; the hub address + port (dword 2) drive the split.
    let c = if ep.speed != 2 { 1u32 << 27 } else { 0 };
    dma.write32(QH_OFF + 0x00, (qh_phys & !0x1F) | (1 << 1));
    dma.write32(QH_OFF + 0x04,
        (ep.addr as u32 & 0x7F) | (ep.speed << 12) | (1 << 14) | (1 << 15)
            | (ep.max_packet << 16) | c);
    dma.write32(QH_OFF + 0x08,
        (1 << 30) | ((ep.hub_addr as u32 & 0x7F) << 16) | ((ep.port as u32 & 0x7F) << 22));
    dma.write32(QH_OFF + 0x0C, 0);
    dma.write32(QH_OFF + 0x10, setup_phys & !0x1F);
    dma.write32(QH_OFF + 0x14, 1);
    dma.write32(QH_OFF + 0x18, 0);

    // 8-byte setup packet → two little-endian dwords.
    let s0 = setup[0] as u32 | (setup[1] as u32) << 8 | (setup[2] as u32) << 16 | (setup[3] as u32) << 24;
    let s1 = setup[4] as u32 | (setup[5] as u32) << 8 | (setup[6] as u32) << 16 | (setup[7] as u32) << 24;
    dma.write32(SETUP_PKT + 0x00, s0);
    dma.write32(SETUP_PKT + 0x04, s1);

    // SETUP qTD (DATA0). Chains to DATA if there is a data stage, else STATUS.
    let after_setup = if data_len > 0 { data_phys } else { status_phys };
    dma.write32(QTD_SETUP + 0x00, after_setup & !0x1F);
    dma.write32(QTD_SETUP + 0x04, 1);
    dma.write32(QTD_SETUP + 0x08, QTD_ACTIVE | (2 << 8) | (3 << 10) | (8 << 16));
    dma.write32(QTD_SETUP + 0x0C, pkt_phys);

    // DATA qTD (DATA1), if any. PID IN=01 / OUT=00.
    if data_len > 0 {
        let pid = if in_dir { 1u32 } else { 0u32 };
        dma.write32(QTD_DATA + 0x00, status_phys & !0x1F);
        dma.write32(QTD_DATA + 0x04, 1);
        dma.write32(QTD_DATA + 0x08,
            QTD_ACTIVE | (pid << 8) | (3 << 10) | ((data_len as u32) << 16) | (1 << 31));
        dma.write32(QTD_DATA + 0x0C, buf_phys);
    }

    // STATUS qTD (DATA1, IOC). Opposite direction: IN unless the data stage was IN.
    let status_pid = if in_dir && data_len > 0 { 0u32 } else { 1u32 };
    dma.write32(QTD_STATUS + 0x00, 1);
    dma.write32(QTD_STATUS + 0x04, 1);
    dma.write32(QTD_STATUS + 0x08,
        QTD_ACTIVE | (status_pid << 8) | (3 << 10) | (1 << 15) | (1 << 31));
    dma.write32(QTD_STATUS + 0x0C, 0);

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
        wait(mmio, op + OP_USBSTS, STS_ASS, false);
    }
    mmio.write32(op + OP_ASYNCLISTADDR, qh_phys & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait(mmio, op + OP_USBSTS, STS_ASS, true);
    // Doorbell: set IAAD, wait for the controller to set IAA (bounded - if the
    // controller is wedged it simply won't fire and we proceed), then clear it.
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_IAAD);
    wait(mmio, op + OP_USBSTS, STS_IAA, true);
    mmio.write32(op + OP_USBSTS, STS_IAA); // RW1C: acknowledge the advance

    // Wait for the STATUS qTD to retire.
    let mut done = false;
    for _ in 0..10_000_000u32 {
        if dma.read32(QTD_STATUS + 0x08) & QTD_ACTIVE == 0 { done = true; break; }
    }
    let t_setup  = dma.read32(QTD_SETUP + 0x08);
    let t_data   = if data_len > 0 { dma.read32(QTD_DATA + 0x08) } else { 0 };
    let t_status = dma.read32(QTD_STATUS + 0x08);
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
        data_len - ((dma.read32(QTD_DATA + 0x08) >> 16) & 0x7FFF) as usize
    } else { 0 };
    Some(moved)
}

/// E3b/E3c - address and configure the high-speed hub on the port, then read its
/// hub descriptor (downstream port count). The keyboard is on one of those ports;
/// E3c-2 will power them and find it.
fn enumerate_hub(ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, op: usize) {
    let dma = match ctx.dma_region() {
        Some(d) => d,
        None => { ctx.log("ehci: no DMA arena granted - cannot enumerate; idling"); return; }
    };

    // E3b: device descriptor at the default address 0.
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
    if control(ctx, mmio, &dma, op, &Ep::hs(0, 64), &setup, 18, true).is_none() { return; }
    let class = dma.read8(DATA_BUF + 4);
    let proto = dma.read8(DATA_BUF + 6); // hub: 0/1 = single-TT, 2 = multi-TT
    let mps0  = dma.read8(DATA_BUF + 7) as u32;
    let vid   = dma.read16(DATA_BUF + 8);
    let pid   = dma.read16(DATA_BUF + 10);
    ctx.log_fmt(format_args!(
        "ehci: DEVICE DESCRIPTOR class={:#04x} proto={} (TT type) mps0={} VID={:#06x} PID={:#06x}",
        class, proto, mps0, vid, pid));

    // E3c: assign address 1.
    let setup = [0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]; // Set_Address(1)
    if control(ctx, mmio, &dma, op, &Ep::hs(0, mps0), &setup, 0, false).is_none() {
        ctx.log("ehci: Set_Address failed"); return;
    }
    delay_cycles(ctx, RECOVERY_CYCLES); // SetAddress recovery (>= 2 ms)
    ctx.log("ehci: hub addressed (1)");

    // Get the configuration descriptor (first 9 bytes) for bConfigurationValue.
    let setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x09, 0x00]; // Get_Descriptor(Config), 9
    if control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 9, true).is_none() {
        ctx.log("ehci: Get_Config failed"); return;
    }
    let cfgval = dma.read8(DATA_BUF + 5);

    // Set configuration.
    let setup = [0x00, 0x09, cfgval, 0x00, 0x00, 0x00, 0x00, 0x00]; // Set_Configuration
    if control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 0, false).is_none() {
        ctx.log("ehci: Set_Configuration failed"); return;
    }
    ctx.log_fmt(format_args!("ehci: hub configured (cfg={})", cfgval));

    // Hub class descriptor → number of downstream ports.
    let setup = [0xA0, 0x06, 0x00, 0x29, 0x00, 0x00, 0x40, 0x00]; // Get_Descriptor(Hub 0x29), 64
    let n = match control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 64, true) {
        Some(n) => n,
        None => { ctx.log("ehci: Get_Hub_Descriptor failed"); return; }
    };
    if n < 4 { ctx.log("ehci: short hub descriptor"); return; }
    let nports  = dma.read8(DATA_BUF + 2);
    let hubchar = dma.read16(DATA_BUF + 3);
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
    loop {
        let (devs, ndev) = scan_devices(ctx, mmio, &dma, op, mps0, nports);
        if ndev == 0 {
            ctx.log("ehci: no boot keyboard/mouse attached - waiting for a connection");
            wait_for_connection(ctx, mmio, &dma, op, mps0, nports);
            announce = true; // whatever connects after a wait is a real plug event
            continue;
        }
        if announce {
            for d in &devs[..ndev] {
                notify(ctx, if d.is_mouse { "mouse connected (ehci)" } else { "keyboard connected (ehci)" });
            }
        }
        let gone = poll_devices(ctx, mmio, &dma, op, &devs[..ndev]);
        notify(ctx, if devs[gone].is_mouse { "mouse disconnected (ehci)" } else { "keyboard disconnected (ehci)" });
        announce = true; // the next connect (after re-scan) is a real plug event
        delay_cycles(ctx, DEBOUNCE_CYCLES); // let the port status settle before re-scan
    }
}

/// Power the hub's downstream ports, then reset + enumerate every connected one,
/// configuring each boot keyboard/mouse at a unique address. Returns the device
/// table and how many were configured. Called once per (re)scan in the hot-plug
/// loop - addressing each device (Set_Address moves it off the default address 0)
/// before resetting the next port is what lets two devices coexist.
fn scan_devices(
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, mps0: u32, nports: u8,
) -> ([HidDev; MAX_HID], usize) {
    // Power every downstream port (the hub does individual power switching), let
    // power settle, then enumerate each connected port.
    for port in 1..=nports {
        let setup = [0x23, 0x03, 0x08, 0x00, port, 0x00, 0x00, 0x00]; // Set_Feature(PORT_POWER)
        let _ = control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &setup, 0, false);
    }
    delay_cycles(ctx, RESET_HOLD_CYCLES); // power-on-to-power-good settle (generous)

    let mut devs = [HidDev { addr: 0, port: 0, ep_num: 0, is_mouse: false }; MAX_HID];
    let mut ndev = 0usize;
    let mut next_addr = 2u8; // 1 is the hub; devices start at 2
    let mut any_failed = false; // a connected port we couldn't enumerate (faulty port)
    for port in 1..=nports {
        // Status before reset.
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
        if control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &setup, 4, true).is_none() { continue; }
        let status = dma.read16(DATA_BUF + 0);
        ctx.log_fmt(format_args!(
            "ehci: hub port {}: status={:#06x} connected={} low_speed={}",
            port, status, (status & 1) as u8, ((status >> 9) & 1) as u8));
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
        let kep = Ep::low(0, 8, 1, port);
        let dd  = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
        let mut got = false;
        let mut high_speed = false; // device enabled at high speed after reset
        for attempt in 1..=3u32 {
            delay_cycles(ctx, DEBOUNCE_CYCLES); // connect-debounce before reset
            let s = [0x23, 0x03, 0x04, 0x00, port, 0x00, 0x00, 0x00]; // Set_Feature(PORT_RESET)
            let _ = control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &s, 0, false);
            delay_cycles(ctx, RESET_HOLD_CYCLES);
            let s = [0x23, 0x01, 0x14, 0x00, port, 0x00, 0x00, 0x00]; // Clear_Feature(C_PORT_RESET)
            let _ = control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &s, 0, false);
            delay_cycles(ctx, RESET_HOLD_CYCLES); // generous post-reset recovery

            let s = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
            let _ = control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &s, 4, true);
            let pstat = dma.read16(DATA_BUF + 0);

            // A device that enables at high speed (status bit 10) after reset is
            // mass storage or a nested hub, not a boot HID. The speed bits aren't
            // valid until the reset completes (a thumbdrive reads 0x0101 before
            // reset, 0x0503 after), so this is where we can finally tell. Abandon it
            // without retrying or marking the port faulty - it isn't.
            if (pstat >> 10) & 1 != 0 {
                ctx.log_fmt(format_args!(
                    "ehci: hub port {} enabled high-speed (mass storage / hub) - not a HID, skipping",
                    port));
                high_speed = true;
                break;
            }

            // Retry the descriptor read several times within this reset before
            // re-resetting: this keyboard's FIRST split SETUP after a reset
            // XactErrs, but a settle-and-retry on the same reset often succeeds.
            if control_retry(ctx, mmio, dma, op, &kep, &dd, 18, true, 6).is_some() { got = true; break; }
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
        let vid = dma.read16(DATA_BUF + 8);
        let pid = dma.read16(DATA_BUF + 10);
        ctx.log_fmt(format_args!(
            "ehci: SPLIT device (hub port {}): VID={:#06x} PID={:#06x}", port, vid, pid));

        // Read the config descriptor to identify the device: configuration value,
        // HID interface number/class/protocol (1=keyboard, 2=mouse) and the
        // interrupt-IN endpoint.
        let setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x40, 0x00]; // Get_Descriptor(Config), 64
        if control(ctx, mmio, dma, op, &kep, &setup, 64, true).is_none() { continue; }
        let cfg_val = dma.read8(DATA_BUF + 5);
        let mut o = 0usize;
        let (mut iface, mut iclass, mut iproto, mut ep_addr, mut ep_int) = (0u8, 0u8, 0u8, 0u8, 0u8);
        while o + 2 <= 64 {
            let blen = dma.read8(DATA_BUF + o) as usize;
            if blen == 0 { break; }
            match dma.read8(DATA_BUF + o + 1) {
                0x04 if o + 8 <= 64 && iclass == 0 => {       // interface
                    iface  = dma.read8(DATA_BUF + o + 2);
                    iclass = dma.read8(DATA_BUF + o + 5);
                    iproto = dma.read8(DATA_BUF + o + 7);
                }
                0x05 if o + 7 <= 64 && ep_addr == 0 => {      // endpoint
                    if dma.read8(DATA_BUF + o + 3) & 0x03 == 0x03 { // interrupt
                        ep_addr = dma.read8(DATA_BUF + o + 2);
                        ep_int  = dma.read8(DATA_BUF + o + 6);
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
            ctx.log_fmt(format_args!("ehci: hub port {} is not a boot keyboard/mouse - skipping", port));
            continue;
        }
        if ndev >= MAX_HID {
            ctx.log("ehci: more HID devices than supported - ignoring the extra one");
            continue;
        }
        ctx.log_fmt(format_args!(
            "ehci: *** boot {} on hub port {} ***", if is_mouse { "MOUSE" } else { "KEYBOARD" }, port));
        if setup_hid(ctx, mmio, dma, op, port, next_addr, cfg_val, iface, is_mouse) {
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
    (devs, ndev)
}

/// Poll the hub's downstream ports until one reports a *newly* connected device,
/// then return so the caller re-scans. Snapshots the ports already connected on
/// entry (e.g. a device sitting on the dead port that never enumerates) and only
/// returns on a port that was NOT connected - otherwise a connected-but-unusable
/// device would make the hot-plug loop spin (re-scan → fails → wait → still
/// connected → re-scan …). The async schedule is free for these hub control
/// transfers while nothing usable is attached.
fn wait_for_connection(
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, mps0: u32, nports: u8,
) {
    // Returns Some(true/false) on a successful status read, None if the read
    // itself failed. Distinguishing "read failed (state unknown)" from "read OK,
    // not connected" matters: a device whose split status reads are intermittently
    // flaky (e.g. the boot thumbdrive on a hub TT) would otherwise flap
    // connected→disconnected→connected and trigger a spurious "new device" return,
    // re-scanning forever. On a failed read we leave the snapshot untouched.
    let status = |port: u8| -> Option<bool> {
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
        if control(ctx, mmio, dma, op, &Ep::hs(1, mps0), &setup, 4, true).is_some() {
            Some(dma.read16(DATA_BUF + 0) & 1 != 0)
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
                Some(false) => base &= !(1 << port), // a known device left; reuse its port
                _ => {}                              // unchanged, or unknown (read failed)
            }
        }
        delay_cycles(ctx, DEBOUNCE_CYCLES); // ~50 ms between polls
        // Drain our IPC endpoint while we idle here with no HID attached (the active path drains in
        // poll_devices). Without it a flood-storm clogs our 16-deep queue permanently - see xhci.
        while ctx.try_recv().is_some() {}
        ctx.yield_cpu();
    }
}

/// Run a control transfer, retrying up to `tries` times. The Logitech keyboard's
/// control endpoint over the hub TT intermittently XactErrs/times out a split
/// SETUP; control() rebuilds the QH/qTDs fresh each call, so a plain retry (with a
/// short settle between) reliably gets it through. Used for the configuration
/// transfers, which must all succeed before the keyboard is usable.
fn control_retry(
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, ep: &Ep, setup: &[u8; 8], data_len: usize, in_dir: bool, tries: u32,
) -> Option<usize> {
    for _ in 0..tries {
        if let Some(n) = control(ctx, mmio, dma, op, ep, setup, data_len, in_dir) {
            return Some(n);
        }
        delay_cycles(ctx, RECOVERY_CYCLES); // let the TT settle before retrying
    }
    None
}

/// E5a - give a low-speed HID device (currently at the default address 0 behind
/// hub `port`) a unique `addr` and configure it in HID boot protocol so its
/// reports are the fixed 8-byte format. Each transfer retries (control_retry)
/// because this hub's split control endpoint is intermittently flaky - one failed
/// SETUP must not abandon the device.
fn setup_hid(
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, port: u8, addr: u8, cfg_val: u8, iface: u8, is_mouse: bool,
) -> bool {
    let what = if is_mouse { "mouse" } else { "keyboard" };
    // Set_Address(addr) - issued while still at address 0.
    let s = [0x00, 0x05, addr, 0x00, 0x00, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, &Ep::low(0, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Address failed (5 tries)", what)); return false;
    }
    delay_cycles(ctx, RECOVERY_CYCLES); // SetAddress recovery
    // Set_Configuration (at the new address).
    let s = [0x00, 0x09, cfg_val, 0x00, 0x00, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, &Ep::low(addr, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Configuration failed (5 tries)", what)); return false;
    }
    // HID Set_Protocol(boot=0) on the interface (bmRequestType 0x21, bRequest 0x0B).
    let s = [0x21, 0x0B, 0x00, 0x00, iface, 0x00, 0x00, 0x00];
    if control_retry(ctx, mmio, dma, op, &Ep::low(addr, 8, 1, port), &s, 0, false, 5).is_none() {
        ctx.log_fmt(format_args!("ehci: {} Set_Protocol(boot) failed (5 tries)", what)); return false;
    }
    ctx.log_fmt(format_args!("ehci: {} configured (addr {}, cfg {}, boot protocol)", what, addr, cfg_val));
    true
}

/// Arm an interrupt IN qTD (8-byte report, IOC, given data toggle) for the QH at
/// DMA offset `qh`, and point the QH overlay at it so the controller re-fetches.
/// The qTD lives at `qh + 0x40`, its report buffer at `qh + 0x80`.
fn arm_int(dma: &godspeed_sdk::Dma, qh: usize, toggle: u32) {
    let qtd = qh + 0x40;
    let qtd_phys = dma.phys_at(qtd) as u32;
    let buf_phys = dma.phys_at(qh + 0x80) as u32;
    dma.write32(qtd + 0x00, 1);                          // next qTD = T (terminate)
    dma.write32(qtd + 0x04, 1);                          // alt next  = T
    dma.write32(qtd + 0x08,
        QTD_ACTIVE | (1 << 8) | (3 << 10) | (1 << 15) | (8 << 16) | (toggle << 31));
    dma.write32(qtd + 0x0C, buf_phys);                   // report buffer
    dma.write32(qh + 0x10, qtd_phys & !0x1F);            // QH overlay → this qTD
    dma.write32(qh + 0x14, 1);
    dma.write32(qh + 0x18, 0);                           // clear overlay token
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
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
    op: usize, devs: &[HidDev],
) -> usize {
    let n = devs.len();
    ctx.log_fmt(format_args!(
        "ehci: polling {} device(s) - type at the gsh> prompt; mouse events log here", n));

    // Build a ring of one QH per device, each with an interrupt-IN qTD armed.
    for i in 0..n {
        let qh = POLL_BASE + i * POLL_STRIDE;
        let next_qh = POLL_BASE + ((i + 1) % n) * POLL_STRIDE;
        let next_phys = dma.phys_at(next_qh) as u32;
        let h = if i == 0 { 1u32 << 15 } else { 0 }; // sole head of reclamation list
        dma.write32(qh + 0x00, (next_phys & !0x1F) | (1 << 1)); // horiz link → next QH, typ=QH
        dma.write32(qh + 0x04,
            (devs[i].addr as u32 & 0x7F) | ((devs[i].ep_num as u32) << 8)
                | (1 << 12) | (1 << 14) | h | (8 << 16)); // low speed, DTC, [head], mps 8, C=0
        dma.write32(qh + 0x08,
            (1 << 30) | ((1u32 & 0x7F) << 16) | ((devs[i].port as u32 & 0x7F) << 22)); // Mult, hub 1, port
        dma.write32(qh + 0x0C, 0);
        arm_int(dma, qh, 0);
    }

    // Repoint the async list at our ring with the schedule stopped (the spec wants
    // ASYNCLISTADDR changed only while the async schedule is disabled), then run it.
    if mmio.read32(op + OP_USBSTS) & STS_ASS != 0 {
        mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) & !CMD_ASE);
        wait(mmio, op + OP_USBSTS, STS_ASS, false);
    }
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    mmio.write32(op + OP_ASYNCLISTADDR, dma.phys_at(POLL_BASE) as u32 & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait(mmio, op + OP_USBSTS, STS_ASS, true);

    // E2 (interrupt-driven, §12): enable the controller's interrupts. The interrupt qTDs
    // already carry IOC, so a completed report sets USBSTS.USBINT and the controller asserts
    // its (level) INTx, which the kernel routes via the IOAPIC to EHCI_INT_VECTOR. Clear any
    // stale status first, then enable USB-interrupt + port-change. The poll loop still
    // processes reports (belt-and-suspenders); it also acks (clears USBSTS) + unmasks.
    mmio.write32(op + OP_USBSTS, STS_INT_BITS);
    mmio.write32(op + OP_USBINTR, INT_USB | INT_PCD);

    let mut toggle = [0u32; MAX_HID];
    let mut err = [0u32; MAX_HID];                        // consecutive errored completions
    let mut kb_last = [0u8; 6];                           // keyboard edge-detection state
    let mut sts_logged = false;                          // log when USBSTS first shows USBINT
    // Typematic auto-repeat, timed in TSC cycles (read_tsc is hardware-proven to advance,
    // unlike the coarse kernel tick): ~300 ms before the first repeat, then ~50 ms apart
    // at ~2 GHz. The spread across 1.5-3 GHz CPUs just shifts the feel slightly.
    let mut kb_rep = godspeed_sdk::hid::KeyRepeat::new(REPEAT_INITIAL_CYCLES, REPEAT_INTERVAL_CYCLES);
    let mut kb_caps = false; // Caps Lock latch (host-tracked toggle)
    let mut mouse = godspeed_sdk::hid::MouseTracker::new(); // mouse button/motion state
    loop {
        for i in 0..n {
            let qh = POLL_BASE + i * POLL_STRIDE;
            let qtd = qh + 0x40;
            let buf = qh + 0x80;
            if dma.read32(qtd + 0x08) & QTD_ACTIVE != 0 { continue; } // still NAK'ing
            let tok = dma.read32(qtd + 0x08);
            if tok & (QTD_HALTED | QTD_ERRMASK) == 0 {
                let mut rep = [0u8; 8];
                for j in 0..8 { rep[j] = dma.read8(buf + j); }
                if devs[i].is_mouse {
                    mouse.feed(
                        &rep,
                        |mask, down| ctx.log_fmt(format_args!(
                            "ehci: mouse {} {}",
                            godspeed_sdk::hid::button_name(mask), if down { "down" } else { "up" })),
                        |dx, dy| ctx.log_fmt(format_args!("ehci: mouse moved dx={} dy={}", dx, dy)),
                    );
                } else {
                    // Ctrl+Alt+Del = secure-attention reboot, from any context. Checked only for
                    // keyboard reports (a mouse button byte can alias the modifier bits).
                    // reboot() does not return.
                    if godspeed_sdk::hid::is_ctrl_alt_del(&rep) {
                        ctx.log("ehci: Ctrl+Alt+Del - rebooting");
                        ctx.reboot();
                    }
                    godspeed_sdk::hid::decode_keyboard(
                        &rep, &mut kb_last, &mut kb_rep, &mut kb_caps, ctx.read_tsc(),
                        |ch| ctx.console_push(ch),
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
        kb_rep.poll(ctx.read_tsc(), |ch| ctx.console_push(ch));
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
        // (task/mod.rs) so the system core and the interrupt-driven xHCI's core stay idle.
        // (USBINTR + the drain/unmask below are belt-and-suspenders: if an INTx ever does post an
        // IPC, we drain + ack it so it can't storm; the qTD scan above is what actually reads
        // the keyboard.)
        while ctx.try_recv().is_some() {
            let sts = mmio.read32(op + OP_USBSTS);
            if sts & STS_INT_BITS != 0 {
                mmio.write32(op + OP_USBSTS, sts & STS_INT_BITS); // ack: clear W1C status bits
            }
            ctx.irq_unmask(EHCI_INT_VECTOR);
        }
        ctx.yield_cpu();
    }
}

/// Consecutive errored interrupt completions before a device is declared
/// unplugged. A present device never errors (it NAKs while idle), so this only
/// trips on a real disconnect; the bound rides out the odd transient glitch. At
/// roughly a few milliseconds per errored split this is well under a second.
const DISCONNECT_ERR_THRESHOLD: u32 = 250;

/// Print a hot-plug notice on the console, then nudge the shell to redraw its
/// prompt by injecting a newline into the input ring (which this driver already
/// feeds). The notice is async output that would otherwise leave the prompt
/// scrolled up; the leading "\n" starts it on its own line and the injected
/// newline supplies the terminating break + triggers a fresh `gsh> `.
fn notify(ctx: &ServiceContext, msg: &str) {
    // Leading "\n " - the space is sacrificial: the framebuffer drops the first
    // glyph drawn on a freshly-scrolled line, so we let it eat a space, not the
    // 'U' of "USB:". (Serial is unaffected.)
    ctx.console_write("\n USB: ");
    ctx.console_write(msg);
    ctx.console_push(b'\n');
}

/// Busy-wait roughly `cycles` TSC ticks. Used for the millisecond-scale USB reset
/// timings. Overestimated against the T630's ~2 GHz so the >= 50 ms reset hold is
/// always satisfied even if the TSC runs faster.
fn delay_cycles(ctx: &ServiceContext, cycles: u64) {
    let start = ctx.read_tsc();
    while ctx.read_tsc().wrapping_sub(start) < cycles {}
}
/// ~100 ms at 2 GHz - comfortably over the 50 ms minimum reset hold.
const RESET_HOLD_CYCLES: u64 = 200_000_000;
/// ~20 ms at 2 GHz - reset-recovery settle before reading the port.
const RECOVERY_CYCLES:   u64 = 40_000_000;
/// ~50 ms at 2 GHz - connect-debounce before resetting a hub downstream port.
const DEBOUNCE_CYCLES:   u64 = 100_000_000;

// --- EHCI operational registers (offsets from base + CAPLENGTH) + bit fields ---
const OP_USBCMD:     usize = 0x00;
const OP_USBSTS:     usize = 0x04;
const OP_USBINTR:    usize = 0x08; // Interrupt Enable register
const OP_CONFIGFLAG: usize = 0x40;
// USBINTR / USBSTS interrupt bits (E2, interrupt-driven §12). The interrupt qTDs already set
// IOC, so a completed report sets USBSTS.USBINT; enabling it raises the controller's (level)
// INTx, which the kernel routes via the IOAPIC to EHCI_INT_VECTOR. USBSTS bits are W1C.
const INT_USB:        u32 = 1 << 0; // USB Interrupt (a transfer with IOC completed)
const INT_PCD:        u32 = 1 << 2; // Port Change Detect (hot-plug)
const STS_INT_BITS:   u32 = 0x3F;   // the six W1C interrupt-status bits (0..5)
const EHCI_INT_VECTOR: u8 = 0x29;   // matches kernel interrupts::EHCI_MSI_VECTOR

// Typematic auto-repeat timings, in TSC cycles (~2 GHz on the T630). The driver busy-polls, so
// these only pace the synthesised key-repeat (a held key sends no further reports); kb_rep emits
// a repeat off its own read_tsc clock. ~300 ms to first repeat, then ~50 ms apart.
const REPEAT_INITIAL_CYCLES:  u64 = 600_000_000;   // ~300 ms before the first repeat
const REPEAT_INTERVAL_CYCLES: u64 = 100_000_000;   // ~50 ms between repeats
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
/// (`false`), bounded. Returns true if the condition was met, false on timeout.
fn wait(mmio: &godspeed_sdk::Mmio, off: usize, mask: u32, want_set: bool) -> bool {
    const MAX: u32 = 2_000_000;
    let mut i = 0u32;
    while i < MAX {
        let set = mmio.read32(off) & mask != 0;
        if set == want_set {
            return true;
        }
        i += 1;
    }
    false
}
