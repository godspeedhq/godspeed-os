//! `ehci` — userspace USB 2.0 (EHCI) host-controller driver (§12).
//!
//! The T630's back USB sockets are wired to the EHCI controller (PCI 00:12.0),
//! not the xHCI that the [`xhci`] driver handles — so a keyboard in the back is
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
const CAP_CAPLENGTH:  usize = 0x00; // u8  — bytes from base to the operational regs
const CAP_HCIVERSION: usize = 0x02; // u16 — BCD interface version
const CAP_HCSPARAMS:  usize = 0x04; // u32 — structural parameters
const CAP_HCCPARAMS:  usize = 0x08; // u32 — capability parameters

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("ehci: driver starting");

    let mmio = match ctx.ehci_mmio() {
        Some(m) => m,
        None => {
            ctx.log("ehci: no controller MMIO granted — idling");
            ctx.park();
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
    // E2a — reset the controller and run it, then read the port status. No BIOS
    // handoff yet (E2b adds it if the firmware fights us); every wait is bounded so
    // a firmware tug-of-war times out and reports rather than hanging.
    // ---------------------------------------------------------------------------
    let op = caplength as usize; // operational registers begin at base + CAPLENGTH

    // Stop the controller if the BIOS left it running, then wait for it to halt.
    let cmd = mmio.read32(op + OP_USBCMD);
    mmio.write32(op + OP_USBCMD, cmd & !CMD_RS);
    if !wait(&mmio, op + OP_USBSTS, STS_HCHALTED, true) {
        ctx.log("ehci: WARN — controller did not halt (BIOS may still own it; E2b handoff needed)");
    }

    // Reset: set HCRESET and wait for the controller to clear it.
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_HCRESET);
    if !wait(&mmio, op + OP_USBCMD, CMD_HCRESET, false) {
        ctx.log("ehci: WARN — HCRESET did not complete (E2b handoff needed); idling");
        ctx.park();
    }
    ctx.log("ehci: controller reset");

    // Route all ports to the EHCI (not to companion controllers) and run.
    mmio.write32(op + OP_CONFIGFLAG, 1);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_RS);
    if wait(&mmio, op + OP_USBSTS, STS_HCHALTED, false) {
        ctx.log("ehci: controller running");
    } else {
        ctx.log("ehci: WARN — controller did not leave halted state after run");
    }

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
    // E3a — reset the first connected port and see if it enables. EHCI only
    // enables a port for a HIGH-SPEED device; if PED comes up, the thing on that
    // port is high-speed — for the back keyboard (low-speed) that means a
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

        // E3b — the device on the port (the high-speed hub) is now enabled at the
        // default address 0. Build the async schedule and read its device
        // descriptor over a control transfer.
        if enabled {
            e3b_read_device_descriptor(&ctx, &mmio, op);
        }
    }

    ctx.park();
}

// --- EHCI async-schedule registers + DMA layout for the control transfer ---
const OP_CTRLDSSEGMENT: usize = 0x10; // high 32 bits of 64-bit data-structure addrs
const OP_ASYNCLISTADDR: usize = 0x18; // physical base of the async (QH) list
const CMD_ASE:          u32   = 1 << 5;  // USBCMD: Async Schedule Enable
const STS_ASS:          u32   = 1 << 15; // USBSTS: Async Schedule Status

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

/// E3b — read the enabled high-speed device's device descriptor over a control
/// transfer (Get_Descriptor(Device)) using a one-QH async schedule. The device is
/// at the default address 0. Confirms it's the rate-matching hub (bDeviceClass
/// 0x09) and reports VID/PID, before E3c does the hub-class enumeration.
fn e3b_read_device_descriptor(ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, op: usize) {
    let dma = match ctx.dma_region() {
        Some(d) => d,
        None => { ctx.log("ehci: no DMA arena granted — cannot enumerate; idling"); return; }
    };
    dma.zero();

    let qh_phys     = dma.phys_at(QH_OFF) as u32;
    let setup_phys  = dma.phys_at(QTD_SETUP) as u32;
    let data_phys   = dma.phys_at(QTD_DATA) as u32;
    let status_phys = dma.phys_at(QTD_STATUS) as u32;
    let pkt_phys    = dma.phys_at(SETUP_PKT) as u32;
    let buf_phys    = dma.phys_at(DATA_BUF) as u32;

    // Queue Head for EP0 of the high-speed device at address 0.
    dma.write32(QH_OFF + 0x00, (qh_phys & !0x1F) | (1 << 1)); // self-link, Typ=QH, T=0
    // EP chars: addr 0, EP 0, speed=HS(2<<12), DTC=1, H=1 (reclamation head), MaxPkt=64.
    dma.write32(QH_OFF + 0x04, (2 << 12) | (1 << 14) | (1 << 15) | (64 << 16));
    dma.write32(QH_OFF + 0x08, 1 << 30); // EP caps: Mult=1 (no split — talking to the HS hub)
    dma.write32(QH_OFF + 0x0C, 0);       // current qTD
    dma.write32(QH_OFF + 0x10, setup_phys & !0x1F); // overlay: next qTD = SETUP
    dma.write32(QH_OFF + 0x14, 1);       // alt next qTD = terminate
    dma.write32(QH_OFF + 0x18, 0);       // overlay token: Active clear → fetch next qTD

    // SETUP qTD: 8-byte setup packet, DATA0.
    dma.write32(QTD_SETUP + 0x00, data_phys & !0x1F);
    dma.write32(QTD_SETUP + 0x04, 1);
    dma.write32(QTD_SETUP + 0x08, QTD_ACTIVE | (2 << 8) | (3 << 10) | (8 << 16)); // PID=SETUP, CERR=3, 8 bytes
    dma.write32(QTD_SETUP + 0x0C, pkt_phys);

    // DATA qTD: IN, 18 bytes, DATA1.
    dma.write32(QTD_DATA + 0x00, status_phys & !0x1F);
    dma.write32(QTD_DATA + 0x04, 1);
    dma.write32(QTD_DATA + 0x08, QTD_ACTIVE | (1 << 8) | (3 << 10) | (18 << 16) | (1 << 31)); // PID=IN, 18 bytes, DT=1
    dma.write32(QTD_DATA + 0x0C, buf_phys);

    // STATUS qTD: OUT, 0 bytes, DATA1, interrupt-on-complete.
    dma.write32(QTD_STATUS + 0x00, 1); // terminate
    dma.write32(QTD_STATUS + 0x04, 1);
    dma.write32(QTD_STATUS + 0x08, QTD_ACTIVE | (0 << 8) | (3 << 10) | (1 << 15) | (1 << 31)); // PID=OUT, IOC, DT=1
    dma.write32(QTD_STATUS + 0x0C, 0);

    // Setup packet: Get_Descriptor(Device, index 0, length 18).
    dma.write32(SETUP_PKT + 0x00, 0x0100_0680); // bmRequestType=0x80 bRequest=0x06 wValue=0x0100
    dma.write32(SETUP_PKT + 0x04, 0x0012_0000); // wIndex=0 wLength=18

    // Point the async list at the QH and enable the async schedule.
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    mmio.write32(op + OP_ASYNCLISTADDR, qh_phys & !0x1F);
    mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
    wait(mmio, op + OP_USBSTS, STS_ASS, true);

    // Wait for the STATUS qTD to retire (Active clears) — the whole transfer done.
    let mut done = false;
    for _ in 0..10_000_000u32 {
        if dma.read32(QTD_STATUS + 0x08) & QTD_ACTIVE == 0 { done = true; break; }
    }
    let toks = dma.read32(QTD_SETUP + 0x08) | dma.read32(QTD_DATA + 0x08) | dma.read32(QTD_STATUS + 0x08);

    if !done {
        ctx.log_fmt(format_args!("ehci: control transfer timed out (tokens={:#010x})", toks));
    } else if toks & QTD_HALTED != 0 || toks & QTD_ERRMASK != 0 {
        ctx.log_fmt(format_args!("ehci: control transfer error (tokens={:#010x})", toks));
    } else {
        let blen   = dma.read8(DATA_BUF + 0);
        let dtype  = dma.read8(DATA_BUF + 1);
        let class  = dma.read8(DATA_BUF + 4);
        let mps0   = dma.read8(DATA_BUF + 7);
        let vid    = dma.read16(DATA_BUF + 8);
        let pid    = dma.read16(DATA_BUF + 10);
        ctx.log_fmt(format_args!(
            "ehci: DEVICE DESCRIPTOR bLength={} type={} class={:#04x} mps0={} VID={:#06x} PID={:#06x}",
            blen, dtype, class, mps0, vid, pid
        ));
        if class == 0x09 {
            ctx.log("ehci: device is a USB hub (the rate-matching hub) -> E3c enumerates its ports");
        }
    }
}

/// Busy-wait roughly `cycles` TSC ticks. Used for the millisecond-scale USB reset
/// timings. Overestimated against the T630's ~2 GHz so the >= 50 ms reset hold is
/// always satisfied even if the TSC runs faster.
fn delay_cycles(ctx: &ServiceContext, cycles: u64) {
    let start = ctx.read_tsc();
    while ctx.read_tsc().wrapping_sub(start) < cycles {}
}
/// ~100 ms at 2 GHz — comfortably over the 50 ms minimum reset hold.
const RESET_HOLD_CYCLES: u64 = 200_000_000;
/// ~20 ms at 2 GHz — reset-recovery settle before reading the port.
const RECOVERY_CYCLES:   u64 = 40_000_000;

// --- EHCI operational registers (offsets from base + CAPLENGTH) + bit fields ---
const OP_USBCMD:     usize = 0x00;
const OP_USBSTS:     usize = 0x04;
const OP_CONFIGFLAG: usize = 0x40;
const OP_PORTSC0:    usize = 0x44; // PORTSC[0]; +4 bytes per additional port

const CMD_RS:        u32 = 1 << 0;  // Run/Stop
const CMD_HCRESET:   u32 = 1 << 1;  // Host Controller Reset
const STS_HCHALTED:  u32 = 1 << 12; // HCHalted
const PORTSC_CCS:    u32 = 1 << 0;  // Current Connect Status
const PORTSC_PED:    u32 = 1 << 2;  // Port Enabled/Disabled
const PORTSC_RESET:  u32 = 1 << 8;  // Port Reset
const PORTSC_OWNER:  u32 = 1 << 13; // Port Owner (1 = handed to a companion)
/// Write-1-to-clear change bits (CSC, PEDC, OCC) — mask off when writing PORTSC
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
