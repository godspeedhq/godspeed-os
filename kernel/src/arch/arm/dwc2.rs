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
// What the DWC2 DMA master OR's into a physical buffer address to reach RAM.
//   Real Pi 2 (BCM2836): the VideoCore uncached bus alias 0xC000_0000 | phys (Circle's BUS_ADDRESS,
//     u-boot's `dev->dma`). The peripherals see ARM RAM at 0xC000_0000, not at 0.
//   QEMU raspi2b: the emulated DWC2 DMA reads/writes the ARM *system* address space directly, so the
//     alias points at unmapped memory - the device would then DMA a garbage SETUP (which USB still ACKs)
//     and STALL the DATA stage. Emulation therefore wants 0 (identity).
// Gated on the `qemu` build feature so the same source serves both; HW build keeps the alias.
#[cfg(feature = "qemu")]
const DMA_BUS_ALIAS: u32 = 0x0000_0000;
#[cfg(not(feature = "qemu"))]
const DMA_BUS_ALIAS: u32 = 0xC000_0000;
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
    // 5c. Internal buffer DMA + INCR4 AHB burst (u-boot's value) + the global-int bit. We POLL HCINT for
    //     completion, but the transfer itself is DMA: the core moves each packet to/from the buffer HCDMA
    //     points at, so no FIFO push/pop. This is what QEMU's DWC2 model actually emulates (slave/PIO is
    //     an unimplemented TODO there), and the earlier "DMA master never initiated" symptom was a channel
    //     left in an undefined state - the halt-all-channels init (5f) is exactly the fix for that, so DMA
    //     is retried WITH it in place. INCR4 = HBstLen field (bits [4:1]) = 3.
    wr(GAHBCFG, GAHBCFG_DMAEN | (3 << 1) | GAHBCFG_GLBLINTRMSK);
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
const PID_DATA0: u32 = 0;
const PID_DATA1: u32 = 2;
const PID_SETUP: u32 = 3;
// HCINT bits
const HCINT_XFERCOMPL: u32 = 1 << 0;
const HCINT_CHHLTD:    u32 = 1 << 1;

// --- Internal DMA mode ------------------------------------------------------
// The DWC2's own bus-mastering DMA moves the data: we point HCDMA at a physically-contiguous buffer
// (`DMA`), arm the channel, and wait for the halt - no FIFO push/pop from the CPU. QEMU's DWC2 model
// only implements this DMA path (not slave/PIO), and it is also how u-boot/Linux drive the Pi 2 core.
// The A7's DMA is not cache-coherent, so every transfer is bracketed with cache maintenance
// (`flush_dcache`, DCCIMVAC) and the buffer is reached through the VideoCore bus alias on real hardware
// (`DMA_BUS_ALIAS`). Enumeration is synchronous (a one-time bounded boot cost); the keyboard interrupt
// endpoint is polled from the timer tick (`poll`).

static LOW_SPEED: AtomicBool = AtomicBool::new(false); // attached device is low-speed
static DEV_ADDR:  AtomicU8   = AtomicU8::new(0);       // 0 until SET_ADDRESS assigns 1
static MPS0:      AtomicU8   = AtomicU8::new(8);       // EP0 max packet size (8 until GET_DESCRIPTOR)

// --- boot-keyboard poll state (set once enumeration finds a keyboard behind the hub) ---
static KBD_READY:  AtomicBool = AtomicBool::new(false); // a boot keyboard is configured + pollable
static KBD_ADDR:   AtomicU8   = AtomicU8::new(0);       // its assigned USB address
static KBD_EP:     AtomicU8   = AtomicU8::new(0);       // its interrupt IN endpoint number
static KBD_TOGGLE: AtomicBool = AtomicBool::new(false); // DATA0/DATA1 toggle for the interrupt endpoint

/// Program + enable channel 0 for one transaction. `ep`/`ep_type` select the endpoint (0/control for the
/// enumeration path, the keyboard's IN endpoint / interrupt=3 for polling); device address, EP0 max-packet
/// and speed come from the globals the enumeration steps set. The DWC2 DMA master moves the data itself.
fn chan_program(dir_in: bool, pid: u32, len: u32, buf_phys: u32, ep: u32, ep_type: u32) {
    let mps = MPS0.load(Ordering::Relaxed) as u32;
    let dev_addr = DEV_ADDR.load(Ordering::Relaxed) as u32;
    let low_speed = LOW_SPEED.load(Ordering::Relaxed) as u32;
    let pkts = if len == 0 { 1 } else { (len + mps - 1) / mps };
    wr(HCINT0, 0xFFFF_FFFF);                                     // clear stale channel interrupts
    wr(HCSPLT0, 0);                                              // no split transaction (full-speed chain)
    wr(HCTSIZ0, (len & 0x7_FFFF) | (pkts << 19) | (pid << 29));  // size, packet count, starting PID
    // The HCDMA address is a *bus* address as the DWC2 master sees memory (see DMA_BUS_ALIAS).
    wr(HCDMA0, buf_phys | DMA_BUS_ALIAS);
    let chan = (mps & 0x7FF)
        | ((ep & 0xF) << 11)               // endpoint number
        | ((dir_in as u32) << 15)
        | (low_speed << 17)
        | ((ep_type & 0x3) << 18)          // 0=control, 2=bulk, 3=interrupt
        | (1 << 20)                        // multi-count = 1
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

/// DMA scratch buffer. Static so it lives in identity-mapped RAM (VA == PA); the DMA engine reads/writes
/// it via the bus alias (`chan_program`). 64-byte aligned, and `setup` is padded to a full 64 bytes so
/// `data` starts on its own cache line (the clean/invalidate bracket never straddles setup + data). The
/// `data` region holds a full disk block (512) or ethernet frame (~1514) for bulk transfers.
#[repr(C, align(64))]
struct DmaBuf { setup: [u8; 64], data: [u8; 2048] }
static mut DMA: DmaBuf = DmaBuf { setup: [0; 64], data: [0; 2048] };

/// Clean+invalidate a cache-line range to the PoC (DCCIMVAC) - the DMA-coherency bracket. The A7's DMA
/// is not cache-coherent: clean pushes CPU writes to RAM before the device reads (OUT); invalidate drops
/// the line so a later CPU read re-fetches what the device wrote (IN). A no-op under QEMU (no caches).
fn flush_dcache(addr: u32, len: u32) {
    let mut p = addr & !31;
    let end = addr.wrapping_add(len);
    while p < end {
        // SAFETY: DCCIMVAC (`c7, c14, 1`) cleans+invalidates one line by MVA; no memory is modified.
        unsafe { core::arch::asm!("mcr p15, 0, {a}, c7, c14, 1", a = in(reg) p, options(nostack)); }
        p = p.wrapping_add(32);
    }
    // SAFETY: `dsb` orders the maintenance before the DMA (or the following CPU read) observes memory.
    unsafe { core::arch::asm!("dsb", options(nostack)); }
}

/// One DMA transaction: point HCDMA at `buf_phys`, enable the channel, wait for the halt. The core moves
/// the data itself. Retries on NAK / transaction-error; STALL or exhausted retries is a hard failure.
fn chan_dma(dir_in: bool, pid: u32, buf_phys: u32, len: u32, ep: u32, ep_type: u32) -> bool {
    for _attempt in 0..3 {
        chan_program(dir_in, pid, len, buf_phys, ep, ep_type);
        let ci = wait_halt();
        if ci & HCINT_XFERCOMPL != 0 { return true; }
        if ci & (1 << 3) != 0 { return false; }         // STALL - hard failure
        spin(5_000);                                    // NAK / XactErr - brief backoff, then retry
    }
    false
}

/// A single control-endpoint DMA transaction (ep 0, type control). Thin wrapper so ctrl_xfer reads clean.
fn ctrl_dma(dir_in: bool, pid: u32, buf_phys: u32, len: u32) -> bool {
    chan_dma(dir_in, pid, buf_phys, len, 0, 0)
}

/// A full control transfer via DMA: SETUP -> (DATA) -> STATUS, through the `DMA` scratch buffer. `data_in`
/// / `dlen` describe the DATA stage; the STATUS stage runs in the opposite direction with zero length.
fn ctrl_xfer(setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize) -> bool {
    // SAFETY: DMA is a static touched only here on core 0; `addr_of` yields its identity-mapped physical
    // address. The buffer is filled + cache-flushed while no channel is running, so the DMA engine never
    // reads a half-written buffer.
    unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DMA);
        let setup_phys = core::ptr::addr_of!(d.setup) as u32;
        let data_phys = core::ptr::addr_of!(d.data) as u32;

        d.setup[..8].copy_from_slice(setup);
        flush_dcache(setup_phys, 8);
        if !ctrl_dma(false, PID_SETUP, setup_phys, 8) { pl011_write(b"dwc2: SETUP failed\r\n"); return false; }

        if dlen > 0 {
            if data_in {
                flush_dcache(data_phys, dlen as u32); // invalidate the line before the device writes it
                if !ctrl_dma(true, PID_DATA1, data_phys, dlen as u32) { pl011_write(b"dwc2: DATA failed\r\n"); return false; }
                flush_dcache(data_phys, dlen as u32); // invalidate after -> the CPU reads device-written bytes
                let n = dlen.min(d.data.len()).min(data.len());
                data[..n].copy_from_slice(&d.data[..n]);
            } else {
                let n = dlen.min(d.data.len());
                d.data[..n].copy_from_slice(&data[..n]);
                flush_dcache(data_phys, dlen as u32);
                if !ctrl_dma(false, PID_DATA1, data_phys, dlen as u32) { pl011_write(b"dwc2: DATA failed\r\n"); return false; }
            }
        }

        // STATUS: opposite direction, zero length, DATA1 (uses the setup buffer as a dummy DMA target).
        let ok = if data_in {
            ctrl_dma(false, PID_DATA1, setup_phys, 0)
        } else {
            flush_dcache(data_phys, 4);
            ctrl_dma(true, PID_DATA1, data_phys, 0)
        };
        if !ok { pl011_write(b"dwc2: STATUS failed\r\n"); return false; }
    }
    true
}

// --- small control-transfer helpers (built on ctrl_xfer) ---

/// GET_DESCRIPTOR: `dtype`/`dindex` select the descriptor; up to `len` bytes land in `buf`.
fn get_descriptor(rtype: u8, dtype: u8, dindex: u8, windex: u16, buf: &mut [u8], len: usize) -> bool {
    let setup = [rtype, 0x06, dindex, dtype, windex as u8, (windex >> 8) as u8, len as u8, (len >> 8) as u8];
    ctrl_xfer(&setup, buf, true, len)
}

/// A no-data control OUT (SET_ADDRESS / SET_CONFIGURATION / a class request). `rtype`/`req`/`value`/`index`
/// are the bmRequestType / bRequest / wValue / wIndex fields.
fn control_out(rtype: u8, req: u8, value: u16, index: u16) -> bool {
    let setup = [rtype, req, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, 0, 0];
    let mut z = [0u8; 1];
    ctrl_xfer(&setup, &mut z, false, 0)
}

// USB hub port features (USB 2.0 §11.24.2) and wPortStatus bits.
const PORT_RESET: u16 = 4;
const PORT_POWER: u16 = 8;
const C_PORT_CONNECTION: u16 = 16;
const C_PORT_RESET: u16 = 20;

/// GET_STATUS of a hub port -> wPortStatus (low 16) | wPortChange (high 16). 0 on failure.
fn hub_get_port_status(port: u8) -> u32 {
    let setup = [0xA3, 0x00, 0x00, 0x00, port, 0x00, 4, 0x00];
    let mut b = [0u8; 4];
    if !ctrl_xfer(&setup, &mut b, true, 4) { return 0; }
    (b[0] as u32) | ((b[1] as u32) << 8) | ((b[2] as u32) << 16) | ((b[3] as u32) << 24)
}

/// Enumerate the device on the root port synchronously: read 8 bytes of the device descriptor to learn
/// EP0's max packet size, assign address 1, read the full 18-byte descriptor for VID/PID/class. If the
/// device is a hub (class 0x09) - the Pi 2's onboard LAN9514 topology, and QEMU's model - walk it to find
/// a keyboard. Called once from `reset_port` at boot.
fn enumerate_sync() {
    let mut buf = [0u8; 64];

    // GET_DESCRIPTOR(device, 8) -> bMaxPacketSize0 at byte 7.
    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 8) {
        pl011_write(b"dwc2: GET_DESC(8) failed - USB unavailable\r\n"); return;
    }
    let mps = if buf[7] == 0 { 8 } else { buf[7] };
    MPS0.store(mps, Ordering::Relaxed);
    pl011_write(b"dwc2: desc8 mps0="); write_hex32(mps as u32); pl011_write(b"\r\n");

    // SET_ADDRESS(1).
    if !control_out(0x00, 0x05, 1, 0) {
        pl011_write(b"dwc2: SET_ADDRESS failed - USB unavailable\r\n"); return;
    }
    DEV_ADDR.store(1, Ordering::Relaxed);
    spin(300_000); // USB spec: 2 ms recovery before the device answers on its new address

    // GET_DESCRIPTOR(device, 18) at address 1 -> VID/PID/class.
    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 18) {
        pl011_write(b"dwc2: GET_DESC(18) failed - USB unavailable\r\n"); return;
    }
    let vid = (buf[8] as u32) | ((buf[9] as u32) << 8);
    let pid = (buf[10] as u32) | ((buf[11] as u32) << 8);
    let class = buf[4];
    pl011_write(b"dwc2: enumerated device VID:PID=");
    write_hex32((vid << 16) | pid);
    pl011_write(b" class="); write_hex32(class as u32); pl011_write(b"\r\n");

    if class == 0x09 {
        enumerate_hub();               // keyboard is behind the hub (LAN9514 on real Pi 2, NEC hub in QEMU)
    } else if class == 0x00 || class == 0x03 {
        configure_keyboard();          // keyboard plugged straight into the root port
    }
}

/// Walk the hub at address 1: configure it, power every port, then for each connected port reset it and
/// enumerate the downstream device, stopping at the first keyboard. Every wait is bounded.
fn enumerate_hub() {
    let hub_mps = MPS0.load(Ordering::Relaxed);          // hub EP0 max-packet (set during root enumeration)
    if !control_out(0x00, 0x09, 1, 0) { pl011_write(b"dwc2: hub SET_CONFIG failed\r\n"); return; }

    // Hub descriptor (class GET_DESCRIPTOR, type 0x29) -> bNbrPorts at byte 2.
    let mut hd = [0u8; 16];
    if !get_descriptor(0xA0, 0x29, 0x00, 0, &mut hd, 16) {
        pl011_write(b"dwc2: hub descriptor failed\r\n"); return;
    }
    let nports = hd[2];
    pl011_write(b"dwc2: hub ports="); write_hex32(nports as u32); pl011_write(b"\r\n");

    for port in 1..=nports { control_out(0x23, 0x03, PORT_POWER, port as u16); } // SET_FEATURE(PORT_POWER)
    spin(1_000_000);                                                             // ~bPwrOn2PwrGood, bounded

    for port in 1..=nports {
        // Re-select the hub's own control endpoint: a prior downstream enumeration left DEV_ADDR/MPS0
        // pointing at that device, so every hub request below would otherwise go to the wrong address.
        DEV_ADDR.store(1, Ordering::Relaxed);
        MPS0.store(hub_mps, Ordering::Relaxed);
        LOW_SPEED.store(false, Ordering::Relaxed);

        let st = hub_get_port_status(port);
        if st & 1 == 0 { continue; }                                            // no device on this port
        control_out(0x23, 0x01, C_PORT_CONNECTION, port as u16);                // CLEAR_FEATURE(C_CONNECTION)
        control_out(0x23, 0x03, PORT_RESET, port as u16);                       // SET_FEATURE(PORT_RESET)
        spin(6_000_000);                                                        // reset drive + recovery, bounded
        let st2 = hub_get_port_status(port);
        control_out(0x23, 0x01, C_PORT_RESET, port as u16);                     // CLEAR_FEATURE(C_RESET)
        let low = (st2 >> 9) & 1 == 1;                                          // wPortStatus low-speed bit
        pl011_write(b"dwc2: port "); write_hex32(port as u32);
        pl011_write(b" device status="); write_hex32(st2); pl011_write(b"\r\n");
        if enumerate_downstream(low) { return; }                               // recognised + brought up a device
    }
    pl011_write(b"dwc2: no keyboard found behind hub\r\n");
}

/// A freshly-reset downstream device answers at address 0. Learn its EP0 max-packet, move it to address 2,
/// then dispatch by function: a boot keyboard (HID) or - proving the bulk path - a mass-storage device.
/// Returns true if it was one we brought up.
fn enumerate_downstream(low: bool) -> bool {
    DEV_ADDR.store(0, Ordering::Relaxed);
    MPS0.store(8, Ordering::Relaxed);
    LOW_SPEED.store(low, Ordering::Relaxed);
    let mut buf = [0u8; 64];

    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 8) {
        pl011_write(b"dwc2: downstream desc8 failed\r\n"); return false;
    }
    MPS0.store(if buf[7] == 0 { 8 } else { buf[7] }, Ordering::Relaxed);

    if !control_out(0x00, 0x05, 2, 0) { pl011_write(b"dwc2: downstream SET_ADDRESS failed\r\n"); return false; }
    DEV_ADDR.store(2, Ordering::Relaxed);
    spin(300_000);

    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 18) {
        pl011_write(b"dwc2: downstream desc18 failed\r\n"); return false;
    }
    let vid = (buf[8] as u32) | ((buf[9] as u32) << 8);
    let pid = (buf[10] as u32) | ((buf[11] as u32) << 8);
    pl011_write(b"dwc2: downstream VID:PID="); write_hex32((vid << 16) | pid);
    pl011_write(b" class="); write_hex32(buf[4] as u32); pl011_write(b"\r\n");

    // A CDC device (class 0x02 at the device level) is a USB-Ethernet gadget. Try CDC-ECM (QEMU's usb-net,
    // and real CDC-ECM dongles). The Pi 2's own LAN9514 is vendor-specific (smsc95xx) - a later branch.
    if buf[4] == 0x02 && configure_cdc_ecm(buf[17]) { return true; }

    // Both HID and mass storage define their class at the interface level, so each probe reads the
    // config descriptor itself. A boot keyboard is the goal; mass storage exercises the bulk path.
    if configure_keyboard() { return true; }
    if probe_mass_storage() { return true; }
    false
}

// --- CDC-ECM USB-Ethernet: raw ethernet frames over the bulk endpoints, no per-packet framing ---
static NET_READY:  AtomicBool = AtomicBool::new(false);
static NET_EP_IN:  AtomicU8   = AtomicU8::new(0);   // bulk IN endpoint (device -> host frames)
static NET_EP_OUT: AtomicU8   = AtomicU8::new(0);   // bulk OUT endpoint (host -> device frames)
static mut NET_MAC: [u8; 6] = [0; 6];               // our station MAC (the future net-stack bridge needs it)

fn hex_val(c: u8) -> u8 {
    match c { b'0'..=b'9' => c - b'0', b'a'..=b'f' => c - b'a' + 10, b'A'..=b'F' => c - b'A' + 10, _ => 0 }
}

/// Read the ECM iMACAddress string descriptor (12 UTF-16LE hex chars) into a 6-byte MAC.
fn read_mac_string(idx: u8) -> [u8; 6] {
    let mut mac = [0u8; 6];
    if idx == 0 { return mac; }
    let mut s = [0u8; 40];
    if !get_descriptor(0x80, 0x03, idx, 0x0409, &mut s, 2) { return mac; }   // langid en-US; length first
    let len = (s[0] as usize).min(s.len());
    if len < 26 { return mac; }
    if !get_descriptor(0x80, 0x03, idx, 0x0409, &mut s, len) { return mac; }
    for b in 0..6 { mac[b] = (hex_val(s[2 + b * 4]) << 4) | hex_val(s[2 + b * 4 + 2]); }
    mac
}

/// Bring up a CDC-ECM USB-Ethernet interface: find the ECM config (control class 0x02/subclass 0x06 + a
/// data interface with bulk endpoints), select it, read the station MAC, activate the data interface's
/// bulk endpoints, enable the packet filter, then prove the frame path with an ARP round-trip.
fn configure_cdc_ecm(nconfigs: u8) -> bool {
    for ci in 0..nconfigs {
        let mut cfg = [0u8; 160];
        if !get_descriptor(0x80, 0x02, ci, 0, &mut cfg, 9) { continue; }
        let total = (((cfg[2] as usize) | ((cfg[3] as usize) << 8)).max(9)).min(cfg.len());
        if !get_descriptor(0x80, 0x02, ci, 0, &mut cfg, total) { continue; }
        let cfg_val = cfg[5];

        let mut i = 0usize;
        let mut is_ecm = false;
        let mut ctrl_iface = 0u8;
        let mut imac = 0u8;
        let mut cur_iface = 0u8;
        let mut cur_alt = 0u8;
        let mut cur_is_data = false;
        let mut data_iface = 0u8;
        let mut data_alt = 0u8;
        let mut ep_in = 0u8;
        let mut ep_out = 0u8;
        let mut bulk_mps = 64u8;
        while i + 2 <= total {
            let blen = cfg[i] as usize;
            let bt = cfg[i + 1];
            if blen == 0 { break; }
            if bt == 0x04 && i + 8 <= total {                          // interface descriptor
                cur_iface = cfg[i + 2];
                cur_alt = cfg[i + 3];
                cur_is_data = cfg[i + 5] == 0x0A;                      // CDC Data class
                if cfg[i + 5] == 0x02 && cfg[i + 6] == 0x06 { is_ecm = true; ctrl_iface = cur_iface; }
            } else if bt == 0x24 && i + 4 <= total && cfg[i + 2] == 0x0F {
                imac = cfg[i + 3];                                     // ECM functional: iMACAddress index
            } else if bt == 0x05 && cur_is_data && i + 7 <= total && cfg[i + 3] & 0x03 == 0x02 {
                bulk_mps = if cfg[i + 4] == 0 { 64 } else { cfg[i + 4] };
                data_iface = cur_iface;
                data_alt = cur_alt;                                    // the alt setting that carries the bulk eps
                if cfg[i + 2] & 0x80 != 0 { ep_in = cfg[i + 2] & 0x0F; } else { ep_out = cfg[i + 2] & 0x0F; }
            }
            i += blen;
        }
        if !is_ecm || ep_in == 0 || ep_out == 0 { continue; }

        if !control_out(0x00, 0x09, cfg_val as u16, 0) { pl011_write(b"dwc2: ecm SET_CONFIG failed\r\n"); return false; }
        let mac = read_mac_string(imac);
        // SET_INTERFACE(data_iface, data_alt): activate the alt setting that exposes the bulk endpoints.
        control_out(0x01, 0x0B, data_alt as u16, data_iface as u16);
        // SET_ETHERNET_PACKET_FILTER (CDC class, req 0x43) on the control interface: directed+broadcast+multicast.
        control_out(0x21, 0x43, 0x000E, ctrl_iface as u16);

        BULK_MPS.store(bulk_mps, Ordering::Relaxed);
        BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
        BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
        NET_EP_IN.store(ep_in, Ordering::Relaxed);
        NET_EP_OUT.store(ep_out, Ordering::Relaxed);
        // SAFETY: NET_MAC is written only here, during core-0 enumeration.
        unsafe { (*core::ptr::addr_of_mut!(NET_MAC)).copy_from_slice(&mac); }
        NET_READY.store(true, Ordering::Release);

        pl011_write(b"dwc2: CDC-ECM up: in ep="); write_hex32(ep_in as u32);
        pl011_write(b" out ep="); write_hex32(ep_out as u32);
        pl011_write(b" mac="); write_hex32(u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]));
        write_hex32(((mac[4] as u32) << 8) | mac[5] as u32);
        pl011_write(b"\r\n");
        net_verify_arp(&mac);
        return true;
    }
    false
}

/// Prove the USB-Ethernet frame path end to end: broadcast an ARP request for the QEMU user-net gateway
/// (10.0.2.2) and poll the bulk IN endpoint for the reply. A frame out + a frame in through a real network
/// stack is the verification the bulk-storage test is to mass storage.
fn net_verify_arp(mac: &[u8; 6]) {
    let ep_out = NET_EP_OUT.load(Ordering::Relaxed) as u32;
    let ep_in = NET_EP_IN.load(Ordering::Relaxed) as u32;
    let mut frame = [0u8; 64];
    for b in 0..6 { frame[b] = 0xFF; }                                 // dst = broadcast
    frame[6..12].copy_from_slice(mac);                                // src = our MAC
    frame[22..28].copy_from_slice(mac);                               // ARP sender hardware address
    frame[12] = 0x08; frame[13] = 0x06;                               // ethertype ARP
    frame[14] = 0x00; frame[15] = 0x01;                               // htype = Ethernet
    frame[16] = 0x08; frame[17] = 0x00;                               // ptype = IPv4
    frame[18] = 6; frame[19] = 4;                                     // hlen, plen
    frame[20] = 0x00; frame[21] = 0x01;                               // oper = request
    frame[28] = 10; frame[29] = 0; frame[30] = 2; frame[31] = 15;     // sender IP 10.0.2.15
    frame[38] = 10; frame[39] = 0; frame[40] = 2; frame[41] = 2;      // target IP 10.0.2.2 (gateway)

    if !bulk_xfer(false, ep_out, &mut frame, 42) { pl011_write(b"dwc2: net ARP send failed\r\n"); return; }
    pl011_write(b"dwc2: net ARP request sent\r\n");

    let mut rx = [0u8; 1536];
    for _ in 0..40 {
        if bulk_xfer(true, ep_in, &mut rx, 1536) && rx[12] == 0x08 && rx[13] == 0x06 && rx[21] == 0x02 {
            pl011_write(b"dwc2: net ARP reply, gateway MAC=");
            write_hex32(u32::from_be_bytes([rx[22], rx[23], rx[24], rx[25]]));
            write_hex32(((rx[26] as u32) << 8) | rx[27] as u32);
            pl011_write(b"\r\ndwc2: USB-ETHERNET FRAME TX/RX VERIFIED (CDC-ECM)\r\n");
            return;
        }
        spin(500_000);
    }
    pl011_write(b"dwc2: net no ARP reply\r\n");
}

/// Read the configuration descriptor of the current device (DEV_ADDR), find a boot-keyboard interface
/// (HID class 0x03, boot subclass, keyboard protocol) and its interrupt IN endpoint, select the config,
/// put it in boot protocol, and arm the poll. Returns true iff it is a boot keyboard.
fn configure_keyboard() -> bool {
    let mut cfg = [0u8; 64];
    // First 9 bytes for wTotalLength, then the whole thing (capped at our buffer).
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, 9) {
        pl011_write(b"dwc2: config desc(9) failed\r\n"); return false;
    }
    let total = (((cfg[2] as usize) | ((cfg[3] as usize) << 8)).max(9)).min(cfg.len());
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, total) {
        pl011_write(b"dwc2: config desc(full) failed\r\n"); return false;
    }
    let cfg_val = cfg[5];

    // Walk the packed interface/endpoint descriptors for a boot-keyboard interrupt IN endpoint.
    let mut i = 0usize;
    let mut iface = 0u8;
    let mut in_kbd_iface = false;
    let mut found_kbd = false;
    let mut ep = 0u8;
    let mut ep_mps = 8u8;
    while i + 2 <= total {
        let blen = cfg[i] as usize;
        let btype = cfg[i + 1];
        if blen == 0 { break; }
        if btype == 0x04 && i + 8 <= total {                       // interface descriptor
            iface = cfg[i + 2];
            in_kbd_iface = cfg[i + 5] == 0x03 && cfg[i + 7] == 0x01; // HID class, keyboard protocol
            if in_kbd_iface { found_kbd = true; }
        } else if btype == 0x05 && in_kbd_iface && i + 7 <= total { // endpoint descriptor
            let addr = cfg[i + 2];
            let attr = cfg[i + 3];
            if addr & 0x80 != 0 && attr & 0x03 == 0x03 {           // IN + interrupt
                ep = addr & 0x0F;
                ep_mps = if cfg[i + 4] == 0 { 8 } else { cfg[i + 4] };
            }
        }
        i += blen;
    }
    if !found_kbd || ep == 0 { pl011_write(b"dwc2: no boot-keyboard interface\r\n"); return false; }

    if !control_out(0x00, 0x09, cfg_val as u16, 0) { pl011_write(b"dwc2: kbd SET_CONFIG failed\r\n"); return false; }
    // SET_PROTOCOL(boot=0) and SET_IDLE(0) are HID class requests; some devices STALL them - not fatal.
    control_out(0x21, 0x0B, 0, iface as u16);                      // SET_PROTOCOL(boot)
    control_out(0x21, 0x0A, 0, iface as u16);                      // SET_IDLE(indefinite)

    KBD_ADDR.store(DEV_ADDR.load(Ordering::Relaxed), Ordering::Relaxed);
    KBD_EP.store(ep, Ordering::Relaxed);
    MPS0.store(ep_mps, Ordering::Relaxed);                         // interrupt-endpoint packet size for the poll
    KBD_TOGGLE.store(false, Ordering::Relaxed);
    KBD_READY.store(true, Ordering::Release);
    pl011_write(b"dwc2: boot keyboard ready on ep="); write_hex32(ep as u32); pl011_write(b"\r\n");
    true
}

// --- bulk transfers (the shared foundation for USB mass storage and, later, USB-Ethernet) ---
// A bulk endpoint keeps its own DATA0/DATA1 toggle per direction, advanced only on a completed packet.
static BULK_TOGGLE_IN:  AtomicBool = AtomicBool::new(false);
static BULK_TOGGLE_OUT: AtomicBool = AtomicBool::new(false);
static BULK_MPS:        AtomicU8   = AtomicU8::new(64);   // bulk endpoint max-packet (set at config time)

/// One bulk transfer of `len` bytes on endpoint `ep`, through the `DMA.data` buffer, with cache
/// maintenance for the A7's non-coherent DMA. Uses the bulk endpoint's max-packet (`BULK_MPS`) for the
/// packet count and maintains the per-direction data toggle. Returns true on completion.
fn bulk_xfer(dir_in: bool, ep: u32, data: &mut [u8], len: usize) -> bool {
    MPS0.store(BULK_MPS.load(Ordering::Relaxed), Ordering::Relaxed); // chan_program uses MPS0 for pktcnt
    let toggle = if dir_in { &BULK_TOGGLE_IN } else { &BULK_TOGGLE_OUT };
    let pid = if toggle.load(Ordering::Relaxed) { PID_DATA1 } else { PID_DATA0 };
    // SAFETY: DMA is touched only on core 0; addr_of gives its identity-mapped physical address.
    let ok = unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DMA);
        let data_phys = core::ptr::addr_of!(d.data) as u32;
        let n = len.min(d.data.len());
        if dir_in {
            flush_dcache(data_phys, n as u32);                     // invalidate before the device writes
            let ok = chan_dma(true, pid, data_phys, n as u32, ep, 2);
            if ok {
                flush_dcache(data_phys, n as u32);                 // invalidate after -> read device bytes
                let m = n.min(data.len());
                data[..m].copy_from_slice(&d.data[..m]);
            }
            ok
        } else {
            let m = n.min(data.len());
            d.data[..m].copy_from_slice(&data[..m]);
            flush_dcache(data_phys, n as u32);
            chan_dma(false, pid, data_phys, n as u32, ep, 2)
        }
    };
    if ok { toggle.store(!toggle.load(Ordering::Relaxed), Ordering::Relaxed); }
    ok
}

// --- USB Mass Storage (Bulk-Only Transport) - a QEMU-verifiable exerciser of the bulk path ---
// BOT wraps each SCSI command in a 31-byte CBW (bulk OUT), an optional data stage, and a 13-byte CSW
// (bulk IN). Signatures: CBW "USBC" (0x43425355), CSW "USBS" (0x53425355).

/// Run one SCSI command via BOT. `cdb` is the SCSI command block; `data`/`dlen` is the data stage
/// (`data_in` selects direction). Returns true iff the command completed with CSW status = passed.
fn bot_command(ep_in: u32, ep_out: u32, cdb: &[u8], data_in: bool, data: &mut [u8], dlen: usize) -> bool {
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());     // dCBWSignature "USBC"
    cbw[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());     // dCBWTag
    cbw[8..12].copy_from_slice(&(dlen as u32).to_le_bytes());     // dCBWDataTransferLength
    cbw[12] = if data_in { 0x80 } else { 0x00 };                 // bmCBWFlags (bit7 = data-IN)
    cbw[13] = 0;                                                  // bCBWLUN
    cbw[14] = cdb.len() as u8;                                    // bCBWCBLength
    let n = cdb.len().min(16);
    cbw[15..15 + n].copy_from_slice(&cdb[..n]);

    if !bulk_xfer(false, ep_out, &mut cbw, 31) { pl011_write(b"dwc2: bot CBW-out failed\r\n"); return false; }
    if dlen > 0 && !bulk_xfer(data_in, if data_in { ep_in } else { ep_out }, data, dlen) {
        pl011_write(b"dwc2: bot data-stage failed\r\n"); return false;
    }

    let mut csw = [0u8; 13];
    if !bulk_xfer(true, ep_in, &mut csw, 13) { pl011_write(b"dwc2: bot CSW-in failed\r\n"); return false; }
    let sig = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    sig == 0x5342_5355 && csw[12] == 0                            // "USBS" and bCSWStatus = passed
}

/// Detect a Bulk-Only mass-storage device on the current address, select its config, and prove the bulk
/// path by reading its capacity and block 0 (READ CAPACITY(10) + READ(10)). Returns true if it was one.
fn probe_mass_storage() -> bool {
    let mut cfg = [0u8; 64];
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, 9) { return false; }
    let total = (((cfg[2] as usize) | ((cfg[3] as usize) << 8)).max(9)).min(cfg.len());
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, total) { return false; }
    let cfg_val = cfg[5];

    // Walk for a mass-storage interface (class 0x08, Bulk-Only protocol 0x50) + its bulk IN/OUT endpoints.
    let mut i = 0usize;
    let mut in_ms = false;
    let mut is_ms = false;
    let mut ep_in = 0u8;
    let mut ep_out = 0u8;
    let mut bulk_mps = 64u8;
    while i + 2 <= total {
        let blen = cfg[i] as usize;
        let btype = cfg[i + 1];
        if blen == 0 { break; }
        if btype == 0x04 && i + 8 <= total {                       // interface descriptor
            in_ms = cfg[i + 5] == 0x08 && cfg[i + 7] == 0x50;      // Mass Storage class, Bulk-Only transport
            if in_ms { is_ms = true; }
        } else if btype == 0x05 && in_ms && i + 7 <= total {       // endpoint descriptor
            let addr = cfg[i + 2];
            if cfg[i + 3] & 0x03 == 0x02 {                         // bulk
                bulk_mps = if cfg[i + 4] == 0 { 64 } else { cfg[i + 4] };
                if addr & 0x80 != 0 { ep_in = addr & 0x0F; } else { ep_out = addr & 0x0F; }
            }
        }
        i += blen;
    }
    if !is_ms || ep_in == 0 || ep_out == 0 { return false; }

    if !control_out(0x00, 0x09, cfg_val as u16, 0) { pl011_write(b"dwc2: msc SET_CONFIG failed\r\n"); return true; }
    BULK_MPS.store(bulk_mps, Ordering::Relaxed);
    BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
    BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
    pl011_write(b"dwc2: mass storage: bulk in ep="); write_hex32(ep_in as u32);
    pl011_write(b" out ep="); write_hex32(ep_out as u32); pl011_write(b"\r\n");

    // Clear the power-on UNIT ATTENTION: a freshly-attached device rejects its first command with CHECK
    // CONDITION until its sense data is drained. Loop TEST UNIT READY / REQUEST SENSE a bounded few times.
    let ei = ep_in as u32;
    let eo = ep_out as u32;
    for _ in 0..8 {
        if bot_command(ei, eo, &[0u8; 6], false, &mut [], 0) { break; }        // TEST UNIT READY (0x00)
        let mut sense = [0u8; 18];
        let _ = bot_command(ei, eo, &[0x03, 0, 0, 0, 18, 0], true, &mut sense, 18); // REQUEST SENSE clears it
    }

    // READ CAPACITY(10): 8-byte reply = last LBA (BE) + block size (BE).
    let cap_cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut cap = [0u8; 8];
    if !bot_command(ep_in as u32, ep_out as u32, &cap_cdb, true, &mut cap, 8) {
        pl011_write(b"dwc2: msc READ CAPACITY failed\r\n"); return true;
    }
    let last_lba = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]);
    let bsize = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
    pl011_write(b"dwc2: msc capacity last_lba="); write_hex32(last_lba);
    pl011_write(b" block_size="); write_hex32(bsize); pl011_write(b"\r\n");

    // READ(10) block 0: proves a multi-packet bulk IN moves real data.
    let rd_cdb = [0x28u8, 0, 0, 0, 0, 0, 0, 0, 1, 0];             // READ(10), LBA 0, 1 block
    let mut blk = [0u8; 512];
    if !bot_command(ep_in as u32, ep_out as u32, &rd_cdb, true, &mut blk, 512) {
        pl011_write(b"dwc2: msc READ(10) failed\r\n"); return true;
    }
    pl011_write(b"dwc2: msc read block0 first4=");
    write_hex32(u32::from_be_bytes([blk[0], blk[1], blk[2], blk[3]]));
    pl011_write(b"\r\ndwc2: BULK TRANSFER VERIFIED (usb mass storage)\r\n");
    true
}

static mut PREV_KEYS: [u8; 6] = [0; 6];

/// Map a HID boot-keyboard usage code to an ASCII byte (US layout). Returns None for keys we do not feed
/// to the console (modifiers, F-keys, ...). `shift` selects the shifted glyph.
fn hid_to_ascii(k: u8, shift: bool) -> Option<u8> {
    let c = match k {
        0x04..=0x1D => {                                           // a-z
            let base = b'a' + (k - 0x04);
            if shift { base - 32 } else { base }
        }
        0x1E..=0x26 => {                                           // 1-9
            if shift { b"!@#$%^&*("[(k - 0x1E) as usize] } else { b'1' + (k - 0x1E) }
        }
        0x27 => if shift { b')' } else { b'0' },                   // 0
        0x28 => b'\r',                                             // Enter
        0x2A => 0x08,                                              // Backspace
        0x2B => b'\t',                                             // Tab
        0x2C => b' ',                                              // Space
        0x2D => if shift { b'_' } else { b'-' },
        0x2E => if shift { b'+' } else { b'=' },
        0x2F => if shift { b'{' } else { b'[' },
        0x30 => if shift { b'}' } else { b']' },
        0x31 => if shift { b'|' } else { b'\\' },
        0x33 => if shift { b':' } else { b';' },
        0x34 => if shift { b'"' } else { b'\'' },
        0x36 => if shift { b'<' } else { b',' },
        0x37 => if shift { b'>' } else { b'.' },
        0x38 => if shift { b'?' } else { b'/' },
        _ => return None,
    };
    Some(c)
}

/// Decode one 8-byte boot report `[modifiers, reserved, key0..key5]`, pushing a byte for each key that is
/// newly pressed this report (edge-triggered against the previous report - no auto-repeat).
fn decode_report(r: &[u8; 8]) {
    let shift = r[0] & 0x22 != 0;                                  // Left|Right Shift
    // SAFETY: PREV_KEYS is touched only here, only on core 0 (the single DWC2 poller); addr_of avoids a
    // reference to the mutable static.
    unsafe {
        let prev = &mut *core::ptr::addr_of_mut!(PREV_KEYS);
        for j in 2..8 {
            let k = r[j];
            if k == 0 { continue; }
            let mut was_down = false;
            for &p in prev.iter() { if p == k { was_down = true; break; } }
            if !was_down {
                if let Some(c) = hid_to_ascii(k, shift) { super::console_push_byte(c); }
            }
        }
        prev.copy_from_slice(&r[2..8]);
    }
}

/// Called from the Core-0 timer tick. Once a keyboard is configured, run one interrupt IN transaction; on
/// a completed transfer decode the boot report into console bytes. A NAK (no key change) returns quietly.
pub fn poll() {
    if !KBD_READY.load(Ordering::Acquire) { return; }
    let ep = KBD_EP.load(Ordering::Relaxed) as u32;
    let toggle = KBD_TOGGLE.load(Ordering::Relaxed);
    let pid = if toggle { PID_DATA1 } else { PID_DATA0 };
    // SAFETY: DMA is touched only on core 0; addr_of gives its identity-mapped physical address.
    unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DMA);
        let data_phys = core::ptr::addr_of!(d.data) as u32;
        flush_dcache(data_phys, 8);                               // invalidate before the device writes
        chan_program(true, pid, 8, data_phys, ep, 3);            // one interrupt IN, up to 8 bytes
        let ci = wait_halt();
        if ci & HCINT_XFERCOMPL == 0 { return; }                 // NAK / no new report
        flush_dcache(data_phys, 8);                               // invalidate after -> read device bytes
        let mut report = [0u8; 8];
        report.copy_from_slice(&d.data[..8]);
        KBD_TOGGLE.store(!toggle, Ordering::Relaxed);            // advance the data toggle on a real packet
        decode_report(&report);
    }
}
