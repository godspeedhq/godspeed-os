// SPDX-License-Identifier: GPL-2.0-only
//! DWC2 USB host controller (BCM2836 / Raspberry Pi 2) - Increment 1: core bring-up + port detect.
//!
//! The Pi 2's USB is a Synopsys DesignWare USB 2.0 OTG (DWC2) core, nothing like the x86 xHCI/EHCI
//! controllers - so this is a from-scratch driver. This first increment proves the controller is alive
//! and a device is attached: read the core's Synopsys ID, soft-reset the core, force it into HOST mode,
//! power the root port, and report whether a device connected and at what speed. No transfers yet (that
//! is increment 2: control transfers via host channels to enumerate the device).
//!
//! **In-kernel, not a userspace service (yet).** The x86 USB drivers are userspace services reached
//! through interrupt routing, which the ARM port does not wire for non-timer IRQs. For the first
//! keyboard we follow the PL011 console model instead: drive the controller from the kernel and, once
//! transfers land, poll the keyboard's interrupt endpoint from the timer tick and push decoded
//! keystrokes into the same console input ring the shell reads. Moving it to a userspace driver is
//! later work, once ARM routes device IRQs to userspace.
//!
//! On real hardware the single USB port sits behind the onboard LAN9514 hub, so a physical keyboard is
//! reached only after enumerating that hub (a later increment). Under QEMU (`-M raspi2b,usb=on -device
//! usb-kbd`) the keyboard attaches to the root port directly, which is what this increment detects.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use super::pl011_write;
use super::exceptions::write_hex32;

/// DWC2 register block: peripheral base + 0x980000 on the BCM2836. Device-mapped already by
/// `build_tables` (the whole `0x3F00_0000..0x4000_0000` peripheral window is Device memory), so no
/// extra mapping is needed - just volatile MMIO.
const DWC2_BASE: usize = super::PERIPHERAL_BASE + 0x98_0000;

// --- Global core registers (offsets from DWC2_BASE) ---
const GOTGCTL:  usize = 0x000; // OTG control + status
const GAHBCFG:  usize = 0x008; // AHB config (DMA enable, global int enable)
const GUSBCFG:  usize = 0x00C; // USB config (force host/device mode, PHY select)
const GRSTCTL:  usize = 0x010; // reset control (core soft reset, AHB idle)
const GINTSTS:  usize = 0x014; // core interrupt status
const GINTMSK:  usize = 0x018; // core interrupt mask
const GRXFSIZ:  usize = 0x024; // receive FIFO size
const GNPTXFSIZ:usize = 0x028; // non-periodic transmit FIFO size
const GSNPSID:  usize = 0x040; // Synopsys core ID ("OT2" + release, e.g. 0x4F54_294A)
const GHWCFG2:  usize = 0x048; // hardware config 2 (architecture, HS PHY type)
const HPTXFSIZ: usize = 0x100; // host periodic transmit FIFO size
// --- Host-mode registers ---
const HCFG:     usize = 0x400; // host config (PHY clock select)
const HPRT:     usize = 0x440; // host port control + status (root port)
// Host channel 0 register block (each channel is 0x20 apart from 0x500). We use only channel 0 - one
// transfer at a time is plenty for enumerating + polling a single keyboard.
const HCCHAR0:  usize = 0x500; // channel characteristics (ep, dir, addr, type, enable)
const HCSPLT0:  usize = 0x504; // channel split control (0 = no split transaction)
const HCINT0:   usize = 0x508; // channel interrupt status
const HCINTMSK0:usize = 0x50C; // channel interrupt mask
const HCTSIZ0:  usize = 0x510; // transfer size (bytes, packet count, PID)
const HCDMA0:   usize = 0x514; // channel DMA address (physical buffer)
const HAINT:    usize = 0x414; // host all-channels interrupt
const HAINTMSK: usize = 0x418; // host all-channels interrupt mask
// --- Power / clock gating ---
const PCGCCTL:  usize = 0xE00; // power + clock gating control

// GRSTCTL bits
const GRSTCTL_CSFTRST: u32 = 1 << 0;  // core soft reset (self-clearing)
const GRSTCTL_RXFFLSH: u32 = 1 << 4;  // RX FIFO flush (self-clearing)
const GRSTCTL_TXFFLSH: u32 = 1 << 5;  // TX FIFO flush (self-clearing)
const GRSTCTL_TXFNUM_ALL: u32 = 0x10 << 6; // TxFNum=0x10 flushes ALL TX FIFOs
const GRSTCTL_AHBIDLE: u32 = 1 << 31; // AHB master idle

// GAHBCFG bits
const GAHBCFG_GLBLINTRMSK: u32 = 1 << 0; // global interrupt enable
const GAHBCFG_DMAEN:       u32 = 1 << 5; // DMA mode enable

// GUSBCFG bits
const GUSBCFG_PHYSEL:     u32 = 1 << 6;  // 1 = full-speed serial PHY, 0 = USB 2.0 HS PHY (UTMI+)
const GUSBCFG_FRCHSTMODE: u32 = 1 << 29; // force host mode
const GUSBCFG_FRCDEVMODE: u32 = 1 << 30; // force device mode

// GINTSTS bit
const GINTSTS_CURMODE_HOST: u32 = 1 << 0; // current mode: 1 = host

// HPRT bits. NOTE: PrtConnDet/PrtEnChng/PrtOvrCurrChng are write-1-to-clear; a read-modify-write that
// sets PrtPwr/PrtRst must mask them off first or it clears pending change flags by accident.
const HPRT_PRTCONNSTS: u32 = 1 << 0;  // device connected
const HPRT_PRTCONNDET: u32 = 1 << 1;  // connect detected (W1C)
const HPRT_PRTENA:     u32 = 1 << 2;  // port enabled (set by hardware after reset)
const HPRT_PRTENCHNG:  u32 = 1 << 3;  // enable changed (W1C)
const HPRT_PRTOVRCURR: u32 = 1 << 4;  // overcurrent active
const HPRT_PRTOVRCHNG: u32 = 1 << 5;  // overcurrent changed (W1C)
const HPRT_PRTRST:     u32 = 1 << 8;  // port reset
const HPRT_PRTPWR:     u32 = 1 << 12; // port power
const HPRT_PRTSPD_SHIFT: u32 = 17;    // port speed (0=HS, 1=FS, 2=LS)
const HPRT_PRTSPD_MASK:  u32 = 0b11 << HPRT_PRTSPD_SHIFT;
/// The W1C change bits - preserve-by-masking-off on any HPRT write that is not clearing them.
const HPRT_WC_BITS: u32 = HPRT_PRTCONNDET | HPRT_PRTENCHNG | HPRT_PRTOVRCHNG;
/// Bits to mask OFF before ANY HPRT read-modify-write: the W1C change bits (above) AND PrtEna. PrtEna is
/// write-1-to-DISABLE, so an RMW that reads the hardware-set PrtEna=1 and writes it back would disable the
/// very port it just enabled (the SETUP then halts with ChHltd and zero bytes moved - HW-diagnosed on the
/// Pi 2). No RMW here ever intends to disable the port, so PrtEna is always zeroed on write.
const HPRT_RMW_CLEAR: u32 = HPRT_WC_BITS | HPRT_PRTENA;

#[inline]
fn rd(off: usize) -> u32 {
    // SAFETY: DWC2 MMIO is Device-mapped (peripheral window); a single 32-bit volatile read.
    unsafe { ((DWC2_BASE + off) as *const u32).read_volatile() }
}
#[inline]
fn wr(off: usize, v: u32) {
    // SAFETY: DWC2 MMIO is Device-mapped; a single 32-bit volatile write.
    unsafe { ((DWC2_BASE + off) as *mut u32).write_volatile(v) }
}

/// Bounded spin (~n loop iterations) - used instead of a real delay so bring-up never hangs the boot if
/// the hardware never sets a bit we wait on. The counts are generous; the callers all tolerate an early
/// timeout by reporting and moving on.
fn spin(n: u32) {
    for _ in 0..n {
        // SAFETY: `nop` has no operands or memory effect.
        unsafe { core::arch::asm!("nop", options(nomem, nostack)); }
    }
}

/// Speed decoded from HPRT.PrtSpd.
fn speed_name(hprt: u32) -> &'static str {
    match (hprt & HPRT_PRTSPD_MASK) >> HPRT_PRTSPD_SHIFT {
        0 => "high-speed (480 Mbps)",
        1 => "full-speed (12 Mbps)",
        2 => "low-speed (1.5 Mbps)",
        _ => "reserved-speed",
    }
}

/// Increment 1: bring the DWC2 core up in host mode, power the root port, and report what is attached.
/// Returns true if a device is connected on the root port (the QEMU `usb-kbd`, or the LAN9514 hub on
/// real hardware). Does no transfers - enumeration is the next increment.
pub fn init() {
    let id = rd(GSNPSID);
    // The Synopsys OTG core IDs read "OT2"/"OT3" in the high half (0x4F54_xxxx). If it does not, this is
    // not the DWC2 (or the region is unmapped) - report loudly and stop, per invariant 12.
    if (id & 0xFFFF_F000) != 0x4F54_2000 && (id & 0xFFFF_F000) != 0x4F54_3000 {
        pl011_write(b"dwc2: no DesignWare core at 0x3F980000 (GSNPSID=");
        write_hex32(id);
        pl011_write(b") - USB unavailable\r\n");
        return;
    }
    pl011_write(b"dwc2: DesignWare USB 2.0 OTG core, GSNPSID=");
    write_hex32(id);
    pl011_write(b"\r\n");

    // 1. Mask + disable global interrupts while we reset (we poll, so keep them off for now).
    wr(GAHBCFG, rd(GAHBCFG) & !GAHBCFG_GLBLINTRMSK);
    wr(GINTMSK, 0);

    // 2. Wait for the AHB master to go idle before a core reset (resetting mid-transfer wedges the core).
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_AHBIDLE == 0 {
        waited += 1;
        if waited > 100_000 { pl011_write(b"dwc2: WARN AHB not idle before reset\r\n"); break; }
    }

    // 3. Core soft reset: sets defaults and clears the FIFOs. Self-clears when done.
    wr(GRSTCTL, rd(GRSTCTL) | GRSTCTL_CSFTRST);
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_CSFTRST != 0 {
        waited += 1;
        if waited > 1_000_000 { pl011_write(b"dwc2: WARN core soft reset did not clear\r\n"); break; }
    }
    // Let the PHY settle after reset.
    spin(200_000);

    // 4. Force HOST mode (we are a host, not a device). The core samples this ~25 ms after the write, so
    //    wait for CurMode=host rather than assuming it took immediately.
    let mut cfg = rd(GUSBCFG);
    cfg &= !GUSBCFG_FRCDEVMODE;
    cfg |= GUSBCFG_FRCHSTMODE;
    wr(GUSBCFG, cfg);
    let mut waited = 0u32;
    while rd(GINTSTS) & GINTSTS_CURMODE_HOST == 0 {
        waited += 1;
        if waited > 2_000_000 { pl011_write(b"dwc2: WARN did not enter host mode\r\n"); break; }
    }

    // 5. Ungate the PHY/port clocks (PCGCCTL=0 releases stop-pclk + gate-hclk).
    wr(PCGCCTL, 0);

    // 5b. Size the FIFOs (values are 32-bit words): RX (256), non-periodic TX (128 @ 256), periodic TX
    //     (128 @ 384). Modest but ample for a single keyboard's tiny transfers. GNPTXFSIZ/HPTXFSIZ pack
    //     (depth << 16) | start_address.
    wr(GRXFSIZ, 0x100);
    wr(GNPTXFSIZ, (0x80 << 16) | 0x100);
    wr(HPTXFSIZ, (0x80 << 16) | 0x180);
    // 5b'. Flush every TX FIFO and the RX FIFO so their internal read/write pointers match the boundaries
    //      just programmed. The core soft reset set pointers for the DEFAULT layout; resizing the FIFOs
    //      leaves those pointers stale, and in DMA mode the core DMAs the SETUP packet INTO the NP TX FIFO
    //      itself - a stale pointer makes that write silently stall, so the channel arms but never
    //      transacts (HW-diagnosed on the Pi 2: ChEna set, HCINT=0, zero bytes moved). Flush only while the
    //      AHB master is idle; each flush bit self-clears.
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_AHBIDLE == 0 {
        waited += 1;
        if waited > 100_000 { pl011_write(b"dwc2: WARN AHB not idle before FIFO flush\r\n"); break; }
    }
    wr(GRSTCTL, GRSTCTL_TXFNUM_ALL | GRSTCTL_TXFFLSH);
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_TXFFLSH != 0 {
        waited += 1;
        if waited > 1_000_000 { pl011_write(b"dwc2: WARN TX FIFO flush did not clear\r\n"); break; }
    }
    wr(GRSTCTL, GRSTCTL_RXFFLSH);
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_RXFFLSH != 0 {
        waited += 1;
        if waited > 1_000_000 { pl011_write(b"dwc2: WARN RX FIFO flush did not clear\r\n"); break; }
    }
    // 5c. Slave / PIO mode: DMA DISABLED (DmaEn=0). The internal DMA master never initiated a transfer on
    //     this board (GRSTCTL.AHBIdle stayed 1 across a dozen HW tests despite correct config + framing),
    //     so we drive the FIFO from the CPU instead - the mode every working bare-metal Pi driver uses.
    //     Keep only GlblIntrMsk (harmless; we poll GINTSTS/HCINT directly, which update regardless).
    wr(GAHBCFG, GAHBCFG_GLBLINTRMSK);
    // 5d. Enable the channel + aggregate host-channel interrupts. We poll HCINT, but some DWC2 cores
    //     (and QEMU's model) only advance a channel's transaction when its interrupt is unmasked. These
    //     never reach the CPU (the BCM2836 USB IRQ line is not wired), they just gate the state machine.
    wr(HCINTMSK0, 0x7FF);   // all channel-0 interrupt sources
    wr(HAINTMSK, 0xFFFF);   // all channels
    wr(GINTMSK, (1 << 25) | (1 << 24)); // Hchint (host channel) + Prtint (port)
    // 5e. Host PHY clock select. CRITICAL for the Pi: with a HS UTMI+ PHY (GUSBCFG.PHYSel=0) driving a
    //     full/low-speed device, Linux's dwc2_init_fs_ls_pclk_sel() selects the 30/60 MHz HS-derived
    //     clock (FSLSPClkSel=0), NOT 48 MHz (which is for a dedicated FS serial PHY). With the wrong FS/LS
    //     clock the frame timer still ticks (SOFs advance) but the core cannot clock the actual FS token,
    //     so the channel arms and never transmits - the exact universal stall seen on this board in both
    //     DMA and PIO mode (SETUP bytes left unconsumed in the TX FIFO). Set it to 0 before the port reset.
    wr(HCFG, rd(HCFG) & !0b11);
    // Ack any pending core interrupts (a stuck SOF/port flag can stall the emulated frame machine).
    wr(GINTSTS, 0xFFFF_FFFF);

    // 5f. Halt every host channel into a clean, known state. A DWC2 channel can power up in an undefined
    //     state and will then NEVER dispatch a transfer (it arms - ChEna set - but the token never goes
    //     out, leaving the pushed bytes stuck in the FIFO), which is exactly the universal stall seen on
    //     this board in both DMA and PIO mode. u-boot/Linux do this dance before any transfer: for each
    //     channel, first assert ChDis (clearing ChEna), then set ChEna|ChDis together and wait for the
    //     hardware to clear ChEna (the channel halts cleanly). NumHstChnl is GHWCFG2[17:14] + 1.
    let num_ch = ((rd(GHWCFG2) >> 14) & 0xF) + 1;
    for i in 0..num_ch {
        let hcchar = 0x500 + (i as usize) * 0x20;
        wr(hcchar, (rd(hcchar) & !((1 << 31) | (1 << 15))) | (1 << 30)); // ChDis, clear ChEna+EPDir
    }
    for i in 0..num_ch {
        let hcchar = 0x500 + (i as usize) * 0x20;
        wr(hcchar, (rd(hcchar) & !(1 << 15)) | (1 << 31) | (1 << 30));   // ChEna|ChDis -> clean halt
        let mut t = 0u32;
        while rd(hcchar) & (1 << 31) != 0 {                             // wait for ChEna to clear
            t += 1;
            if t > 1_000_000 { break; }
        }
    }

    // 6. Power the root port. Preserve the W1C change bits (mask them off so we do not clear pending
    //    connect/enable-change flags), then set PrtPwr.
    let hprt = rd(HPRT) & !HPRT_RMW_CLEAR;
    if hprt & HPRT_PRTPWR == 0 {
        wr(HPRT, hprt | HPRT_PRTPWR);
    }
    // Give the port time to see a connect after power-on.
    spin(2_000_000);

    // 7. Report the root-port state.
    let hprt = rd(HPRT);
    if hprt & HPRT_PRTCONNSTS != 0 {
        pl011_write(b"dwc2: device connected on root port, ");
        pl011_write(speed_name(hprt).as_bytes());
        pl011_write(b"\r\n");
        reset_port();
    } else {
        pl011_write(b"dwc2: no device on root port (HPRT=");
        write_hex32(hprt);
        pl011_write(b") - on real hardware the LAN9514 hub should appear here\r\n");
    }
    if hprt & HPRT_PRTOVRCURR != 0 {
        pl011_write(b"dwc2: WARN port overcurrent\r\n");
    }
}

/// Drive a USB reset on the root port (required to move an attached device from Powered to Default so it
/// answers on address 0). Assert PrtRst, hold ~50 ms, deassert, then wait for the hardware to set
/// PrtEna. Reports the enabled speed - the handle a control transfer (increment 2) will use.
fn reset_port() {
    let base = rd(HPRT) & !HPRT_RMW_CLEAR;
    wr(HPRT, base | HPRT_PRTRST);
    spin(3_000_000); // ~50 ms of USB reset (generous; bounded)
    let base = rd(HPRT) & !HPRT_RMW_CLEAR;
    wr(HPRT, base & !HPRT_PRTRST);
    spin(1_000_000); // recovery time before the port enables

    let mut waited = 0u32;
    while rd(HPRT) & HPRT_PRTENA == 0 {
        waited += 1;
        if waited > 2_000_000 { pl011_write(b"dwc2: WARN port did not enable after reset\r\n"); return; }
    }
    let hprt = rd(HPRT);
    pl011_write(b"dwc2: root port enabled after reset, ");
    pl011_write(speed_name(hprt).as_bytes());
    pl011_write(b"\r\n");
    // Clear the connect/enable change flags now that we have acted on them (W1C: write 1s back). Mask
    // PrtEna off (HPRT_RMW_CLEAR) so writing these change bits does not also disable the port.
    wr(HPRT, (rd(HPRT) & !HPRT_RMW_CLEAR) | HPRT_PRTCONNDET | HPRT_PRTENCHNG);
    // Enumerate synchronously in slave/PIO mode. Enumeration is a one-time bounded boot cost, and slave
    // mode needs prompt FIFO servicing (a tick-spaced poll would under/overrun the FIFO), so a bounded
    // busy-poll here is the right shape. The DWC2's internal DMA master never initiated a transfer on this
    // board (AHBIdle stayed 1 across a dozen HW tests), so PIO is the working path.
    LOW_SPEED.store((hprt & HPRT_PRTSPD_MASK) >> HPRT_PRTSPD_SHIFT == 2, Ordering::Relaxed);
    enumerate_sync();
}

// ---------------------------------------------------------------------------
// Increment 2: tick-driven control-transfer state machine.
//
// A control transfer is SETUP -> (DATA) -> STATUS, each stage one host-channel transaction. Rather than
// busy-spin for each transaction to complete (which never yields to the emulated controller's event
// loop, and would hog the CPU on hardware too), `poll()` - called from the timer tick - advances ONE
// transaction per invocation: it starts a stage, then on later ticks checks whether the channel halted.
// The idle WFI between ticks lets the controller run. This is the in-kernel-polled design the module
// header promises.
// ---------------------------------------------------------------------------

// HCTSIZ PIDs
const PID_DATA1: u32 = 2;
const PID_SETUP: u32 = 3;
// HCINT bits
const HCINT_XFERCOMPL: u32 = 1 << 0;
const HCINT_CHHLTD:    u32 = 1 << 1;
// Max RX status entries drained in one IN transaction before declaring the core wedged. A real transfer
// yields a handful (data packets + completion); this is far above that but finite, so a core that keeps
// RxFLvl asserted forever cannot hang the kernel.
const RX_DRAIN_CAP: u32 = 100_000;

// --- Slave / PIO mode -------------------------------------------------------
// The DWC2's internal DMA master never initiates a transfer in our environment: across a dozen HW tests
// the channel arms (ChEna set), the host frames (HFNUM advances), every config register reads correct,
// yet GRSTCTL.AHBIdle stays 1 and HCDMA never advances. So we drive the FIFO from the CPU instead - the
// slave/PIO mode every working bare-metal Pi USB driver uses. No bus-mastering, no DMA buffers, no cache
// maintenance: OUT data is pushed word-by-word into the NP TX FIFO, IN data is popped word-by-word from
// the RX FIFO after reading GRXSTSP. Enumeration is synchronous (a one-time bounded boot cost); the
// keyboard interrupt-endpoint poll (increment 4) will reuse chan_in from the timer tick.

const DFIFO0:  usize = 0x1000; // host channel-0 data FIFO push/pop window (offset from DWC2_BASE)
const GRXSTSP: usize = 0x020;  // RX status pop (reading it dequeues one RX FIFO status word)
const GINTSTS_RXFLVL: u32 = 1 << 4; // RX FIFO non-empty (a packet status is waiting in GRXSTSP)

static LOW_SPEED: AtomicBool = AtomicBool::new(false); // attached device is low-speed
static DEV_ADDR:  AtomicU8   = AtomicU8::new(0);       // 0 until SET_ADDRESS assigns 1
static MPS0:      AtomicU8   = AtomicU8::new(8);       // EP0 max packet size (8 until GET_DESCRIPTOR)

/// Program + enable channel 0 for one transaction. In slave mode the enable just tells the core to run
/// the token; we then feed/drain the FIFO ourselves.
fn chan_program(dir_in: bool, pid: u32, len: u32) {
    let mps = MPS0.load(Ordering::Relaxed) as u32;
    let dev_addr = DEV_ADDR.load(Ordering::Relaxed) as u32;
    let low_speed = LOW_SPEED.load(Ordering::Relaxed) as u32;
    let pkts = if len == 0 { 1 } else { (len + mps - 1) / mps };
    wr(HCINT0, 0xFFFF_FFFF);                                     // clear stale channel interrupts
    wr(HCSPLT0, 0);                                              // no split transaction (device on root port)
    wr(HCTSIZ0, (len & 0x7_FFFF) | (pkts << 19) | (pid << 29));  // size, packet count, starting PID
    let chan = (mps & 0x7FF)
        | ((dir_in as u32) << 15)
        | (low_speed << 17)
        | (1 << 20)                        // multi-count = 1; endpoint 0 + control type are the zero fields
        | ((dev_addr & 0x7F) << 22)
        | (1 << 31);                       // channel enable
    wr(HCCHAR0, chan);
}

static DUMPED: AtomicBool = AtomicBool::new(false);

/// True while `HCINT` shows the channel neither completed nor errored. Bounded so a wedged controller
/// reports rather than hangs the boot.
fn wait_halt() -> u32 {
    let mut t = 0u32;
    loop {
        let ci = rd(HCINT0);
        if ci & HCINT_CHHLTD != 0 { return ci; }
        t += 1;
        if t > 4_000_000 {
            if !DUMPED.swap(true, Ordering::Relaxed) {
                // One-shot: did the core CONSUME the pushed SETUP bytes (NPTxFSpcAvail back to full) or
                // are they still stuck in the FIFO (core never started the token)?
                pl011_write(b"dwc2: STALL HCCHAR="); write_hex32(rd(HCCHAR0));
                pl011_write(b" HCTSIZ="); write_hex32(rd(HCTSIZ0));
                pl011_write(b" GNPTXSTS="); write_hex32(rd(0x02C));
                pl011_write(b" GINTSTS="); write_hex32(rd(GINTSTS));
                pl011_write(b" HPRT="); write_hex32(rd(HPRT));
                pl011_write(b" HAINT="); write_hex32(rd(HAINT));
                pl011_write(b" GRSTCTL="); write_hex32(rd(GRSTCTL));
                let f1 = rd(0x408); spin(300_000); let f2 = rd(0x408);
                pl011_write(b" HFNUM1="); write_hex32(f1);
                pl011_write(b" HFNUM2="); write_hex32(f2);
                pl011_write(b"\r\n");
            }
            return ci | HCINT_CHHLTD; // treat as halted-without-complete -> failure
        }
    }
}

/// One OUT transaction (SETUP or 0-length STATUS OUT): enable the channel, push `buf[..len]` into the NP
/// TX FIFO one word at a time, then wait for the halt. Retries on NAK/transaction-error (a device fresh
/// from reset can be briefly not-ready); STALL or exhausted retries is a hard failure. Returns true on
/// XferCompl.
fn chan_out(pid: u32, buf: &[u8], len: usize) -> bool {
    for _attempt in 0..2 {
        chan_program(false, pid, len as u32);
        let words = (len + 3) / 4;
        for i in 0..words {
            let mut t = 0u32;
            while (rd(0x02C) & 0xFFFF) == 0 {           // GNPTXSTS.NPTxFSpcAvail (words free)
                t += 1;
                if t > 1_000_000 { pl011_write(b"dwc2: TX FIFO space timeout\r\n"); return false; }
            }
            let mut w = 0u32;
            for b in 0..4 {
                let idx = i * 4 + b;
                if idx < len { w |= (buf[idx] as u32) << (b * 8); }
            }
            wr(DFIFO0, w);
        }
        let ci = wait_halt();
        if ci & HCINT_XFERCOMPL != 0 { return true; }
        if ci & (1 << 3) != 0 { return false; }         // STALL - hard failure
        spin(5_000);                                    // NAK / XactErr - brief backoff, then retry
    }
    false
}

/// One IN transaction (DATA IN or 0-length STATUS IN): enable the channel, drain the RX FIFO into `buf`
/// as packets arrive (GRXSTSP tells us the byte count), then wait for the halt. Same retry policy as
/// chan_out. Returns true on XferCompl.
fn chan_in(pid: u32, buf: &mut [u8], len: usize) -> bool {
    for _attempt in 0..2 {
        chan_program(true, pid, len as u32);
        let mut received = 0usize;
        let mut t = 0u32;
        let mut drained = 0u32;
        let ci = loop {
            // Drain every RX status currently queued before checking for the halt, so no received data
            // is left in the FIFO when the channel completes. This inner loop has its OWN bound: a
            // wedged core that keeps RxFLvl perpetually asserted would otherwise spin here forever, and
            // the `t = 0` progress-reset below means the outer 4M timeout would never fire. Any real IN
            // transfer produces a handful of RX status entries; RX_DRAIN_CAP is far above that but finite.
            while rd(GINTSTS) & GINTSTS_RXFLVL != 0 {
                let status = rd(GRXSTSP);
                let bcnt = ((status >> 4) & 0x7FF) as usize;
                let pktsts = (status >> 17) & 0xF;
                if pktsts == 2 && bcnt > 0 {            // IN data packet received
                    let words = (bcnt + 3) / 4;
                    let mut got = 0usize;
                    for _ in 0..words {
                        let w = rd(DFIFO0);
                        for b in 0..4 {
                            if got < bcnt {
                                if received < buf.len() { buf[received] = (w >> (b * 8)) as u8; received += 1; }
                                got += 1;
                            }
                        }
                    }
                }
                t = 0;
                drained += 1;
                if drained > RX_DRAIN_CAP {
                    pl011_write(b"dwc2: RX FIFO drain runaway - USB unavailable\r\n");
                    return false;
                }
            }
            let ci = rd(HCINT0);
            if ci & HCINT_CHHLTD != 0 { break ci; }
            t += 1;
            if t > 4_000_000 {
                pl011_write(b"dwc2: IN timeout HCINT="); write_hex32(rd(HCINT0)); pl011_write(b"\r\n");
                return false;
            }
        };
        let _ = received;
        if ci & HCINT_XFERCOMPL != 0 { return true; }
        if ci & (1 << 3) != 0 { return false; }         // STALL - hard failure
        spin(5_000);                                    // NAK / XactErr - brief backoff, then retry
    }
    false
}

/// A full control transfer: SETUP -> (DATA) -> STATUS. `data_in`/`dlen` describe the DATA stage; the
/// STATUS stage runs in the opposite direction with zero length. Returns true if every stage completed.
fn ctrl_xfer(setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize) -> bool {
    if !chan_out(PID_SETUP, setup, 8) { pl011_write(b"dwc2: SETUP failed\r\n"); return false; }
    if dlen > 0 {
        let ok = if data_in { chan_in(PID_DATA1, data, dlen) } else { chan_out(PID_DATA1, data, dlen) };
        if !ok { pl011_write(b"dwc2: DATA failed\r\n"); return false; }
    }
    // STATUS: opposite direction, zero length, DATA1.
    let ok = if data_in {
        chan_out(PID_DATA1, &[], 0)
    } else {
        let mut z = [0u8; 1];
        chan_in(PID_DATA1, &mut z, 0)
    };
    if !ok { pl011_write(b"dwc2: STATUS failed\r\n"); return false; }
    true
}

/// Enumerate the attached device synchronously: read 8 bytes of the device descriptor to learn EP0's max
/// packet size, assign address 1, then read the full 18-byte descriptor for VID/PID. Proof that control
/// transfers work end to end. Called once from `reset_port` at boot.
fn enumerate_sync() {
    let mut buf = [0u8; 64];

    // GET_DESCRIPTOR(device, 8) -> bMaxPacketSize0 at byte 7.
    let setup1 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00];
    if !ctrl_xfer(&setup1, &mut buf, true, 8) {
        pl011_write(b"dwc2: GET_DESC(8) failed - USB unavailable\r\n"); return;
    }
    let mps = if buf[7] == 0 { 8 } else { buf[7] };
    MPS0.store(mps, Ordering::Relaxed);
    pl011_write(b"dwc2: desc8 mps0="); write_hex32(mps as u32); pl011_write(b"\r\n");

    // SET_ADDRESS(1).
    let setup2 = [0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00];
    if !ctrl_xfer(&setup2, &mut buf, false, 0) {
        pl011_write(b"dwc2: SET_ADDRESS failed - USB unavailable\r\n"); return;
    }
    DEV_ADDR.store(1, Ordering::Relaxed);
    spin(300_000); // USB spec: 2 ms recovery before the device answers on its new address

    // GET_DESCRIPTOR(device, 18) at address 1 -> VID/PID/class.
    let setup3 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
    if !ctrl_xfer(&setup3, &mut buf, true, 18) {
        pl011_write(b"dwc2: GET_DESC(18) failed - USB unavailable\r\n"); return;
    }
    let vid = (buf[8] as u32) | ((buf[9] as u32) << 8);
    let pid = (buf[10] as u32) | ((buf[11] as u32) << 8);
    pl011_write(b"dwc2: enumerated device VID:PID=");
    write_hex32((vid << 16) | pid);
    pl011_write(b" class="); write_hex32(buf[4] as u32);
    pl011_write(b" mps0="); write_hex32(mps as u32);
    pl011_write(b" - control transfers work (slave/PIO)\r\n");
}

/// Called from the Core-0 idle loop. Enumeration is synchronous (done in `reset_port`); the keyboard
/// interrupt-endpoint poll will hook in here in increment 4.
pub fn poll() {}
