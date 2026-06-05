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

        // E3b/E3c — the device on the port (the high-speed hub) is now enabled at
        // the default address 0. Build the async schedule, address it, configure
        // it, and read its hub descriptor.
        if enabled {
            enumerate_hub(&ctx, &mmio, op);
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

/// Run one control transfer on EP0 of `ep`. For a low-speed `ep` the controller
/// wraps each transaction as a split through the hub TT (hub addr + port in the
/// QH). `setup` is the 8-byte setup packet; for an IN transfer up to `data_len`
/// bytes land in DATA_BUF. Returns the byte count transferred, or None on
/// error/timeout (with the qTD tokens logged). The async schedule stays enabled
/// across calls; the single QH is idle between them.
fn control(
    ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, dma: &godspeed_sdk::Dma,
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

    // Point the async list at the QH; enable the schedule if it isn't already.
    mmio.write32(op + OP_CTRLDSSEGMENT, 0);
    mmio.write32(op + OP_ASYNCLISTADDR, qh_phys & !0x1F);
    if mmio.read32(op + OP_USBSTS) & STS_ASS == 0 {
        mmio.write32(op + OP_USBCMD, mmio.read32(op + OP_USBCMD) | CMD_ASE);
        wait(mmio, op + OP_USBSTS, STS_ASS, true);
    }

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
        ctx.log_fmt(format_args!(
            "ehci: control(req={:#04x}) FAILED done={} setup={:#010x} data={:#010x} status={:#010x}",
            setup[1], done as u8, t_setup, t_data, t_status));
        return None;
    }
    // Bytes actually moved = requested - (DATA token's remaining-bytes field).
    let moved = if data_len > 0 {
        data_len - ((dma.read32(QTD_DATA + 0x08) >> 16) & 0x7FFF) as usize
    } else { 0 };
    Some(moved)
}

/// E3b/E3c — address and configure the high-speed hub on the port, then read its
/// hub descriptor (downstream port count). The keyboard is on one of those ports;
/// E3c-2 will power them and find it.
fn enumerate_hub(ctx: &ServiceContext, mmio: &godspeed_sdk::Mmio, op: usize) {
    let dma = match ctx.dma_region() {
        Some(d) => d,
        None => { ctx.log("ehci: no DMA arena granted — cannot enumerate; idling"); return; }
    };

    // E3b: device descriptor at the default address 0.
    let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
    if control(ctx, mmio, &dma, op, &Ep::hs(0, 64), &setup, 18, true).is_none() { return; }
    let class = dma.read8(DATA_BUF + 4);
    let mps0  = dma.read8(DATA_BUF + 7) as u32;
    let vid   = dma.read16(DATA_BUF + 8);
    let pid   = dma.read16(DATA_BUF + 10);
    ctx.log_fmt(format_args!(
        "ehci: DEVICE DESCRIPTOR class={:#04x} mps0={} VID={:#06x} PID={:#06x}", class, mps0, vid, pid));

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

    // E3c-2 — power every downstream port (the hub does individual power
    // switching), let power settle, then read each port's status to find the
    // keyboard. A connected, low-speed port is our keyboard.
    for port in 1..=nports {
        // Set_Feature(PORT_POWER=8) on the hub, wIndex = port.
        let setup = [0x23, 0x03, 0x08, 0x00, port, 0x00, 0x00, 0x00];
        let _ = control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 0, false);
    }
    delay_cycles(ctx, RESET_HOLD_CYCLES); // power-on-to-power-good settle (generous)

    // E4 — for EACH connected downstream port, reset it (arms the hub TT) and try
    // reading its device descriptor over SPLIT transactions. Trying every
    // connected port finds the keyboard wherever it sits (ports 3 and 4 both
    // showed a low-speed device). Per-port reset status + per-stage qTD tokens are
    // logged so a split failure is precisely diagnosable.
    let mut found = false;
    for port in 1..=nports {
        // Status before reset.
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00]; // Get_Status
        if control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 4, true).is_none() { continue; }
        let status = dma.read16(DATA_BUF + 0);
        ctx.log_fmt(format_args!(
            "ehci: hub port {}: status={:#06x} connected={} low_speed={}",
            port, status, (status & 1) as u8, ((status >> 9) & 1) as u8));
        if status & 1 == 0 { continue; } // nothing connected

        // Reset the downstream port and clear the reset-change.
        let setup = [0x23, 0x03, 0x04, 0x00, port, 0x00, 0x00, 0x00]; // Set_Feature(PORT_RESET)
        let _ = control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 0, false);
        delay_cycles(ctx, RESET_HOLD_CYCLES);
        let setup = [0x23, 0x01, 0x14, 0x00, port, 0x00, 0x00, 0x00]; // Clear_Feature(C_PORT_RESET)
        let _ = control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 0, false);
        delay_cycles(ctx, RECOVERY_CYCLES);

        // Confirm the port enabled (bit 1) after reset.
        let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 0x04, 0x00];
        let _ = control(ctx, mmio, &dma, op, &Ep::hs(1, mps0), &setup, 4, true);
        let pstat = dma.read16(DATA_BUF + 0);
        ctx.log_fmt(format_args!(
            "ehci: hub port {} after reset: status={:#06x} enabled={}",
            port, pstat, ((pstat >> 1) & 1) as u8));

        // Read the low-speed device's descriptor over SPLIT (EP0 max packet 8),
        // retrying a couple times — the first transaction after reset can XactErr.
        let kep = Ep::low(0, 8, 1, port);
        let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00]; // Get_Descriptor(Device), 18
        let mut got = false;
        for _ in 0..3 {
            if control(ctx, mmio, &dma, op, &kep, &setup, 18, true).is_some() { got = true; break; }
            delay_cycles(ctx, RECOVERY_CYCLES);
        }
        if !got {
            ctx.log_fmt(format_args!("ehci: split descriptor on hub port {} failed (3 tries)", port));
            continue;
        }
        let vid = dma.read16(DATA_BUF + 8);
        let pid = dma.read16(DATA_BUF + 10);
        ctx.log_fmt(format_args!(
            "ehci: SPLIT device (hub port {}): VID={:#06x} PID={:#06x}", port, vid, pid));
        found = true;

        // Read the config descriptor to identify the device: HID interface
        // class/protocol (1=keyboard, 2=mouse) and the interrupt-IN endpoint.
        let setup = [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 0x40, 0x00]; // Get_Descriptor(Config), 64
        if control(ctx, mmio, &dma, op, &kep, &setup, 64, true).is_some() {
            let mut o = 0usize;
            let (mut iclass, mut iproto, mut ep_addr, mut ep_int) = (0u8, 0u8, 0u8, 0u8);
            while o + 2 <= 64 {
                let blen = dma.read8(DATA_BUF + o) as usize;
                if blen == 0 { break; }
                match dma.read8(DATA_BUF + o + 1) {
                    0x04 if o + 8 <= 64 && iclass == 0 => {       // interface
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
                "ehci: port {} HID class={:#x} protocol={} (1=kbd 2=mouse) int_ep={:#04x} interval={}",
                port, iclass, iproto, ep_addr, ep_int));
            if iclass == 0x03 && iproto == 1 {
                ctx.log_fmt(format_args!("ehci: *** boot KEYBOARD on hub port {} -> E4b/E5 ***", port));
            }
        }
    }
    if !found {
        ctx.log("ehci: no device descriptor read over split on any connected port");
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
