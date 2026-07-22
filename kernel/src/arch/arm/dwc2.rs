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

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

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
    // 5c. Enable internal buffer DMA + a 16-beat AHB burst, so a transfer is "point HCDMA at a buffer,
    //     start the channel, wait" rather than hand-copying the FIFO. Re-enable the global interrupt bit
    //     too (we still poll, but some cores gate DMA completion on it).
    wr(GAHBCFG, (rd(GAHBCFG) & !0x1E) | GAHBCFG_DMAEN | (0x7 << 1) | GAHBCFG_GLBLINTRMSK);
    // 5d. Enable the channel + aggregate host-channel interrupts. We poll HCINT, but some DWC2 cores
    //     (and QEMU's model) only advance a channel's transaction when its interrupt is unmasked. These
    //     never reach the CPU (the BCM2836 USB IRQ line is not wired), they just gate the state machine.
    wr(HCINTMSK0, 0x7FF);   // all channel-0 interrupt sources
    wr(HAINTMSK, 0xFFFF);   // all channels
    wr(GINTMSK, (1 << 25) | (1 << 24)); // Hchint (host channel) + Prtint (port)
    // 5e. Host PHY clock select: for a full/low-speed device the PHY runs at 48 MHz (FSLSPClkSel=1);
    //     leaving it 0 (30/60 MHz HS clock) makes the SOF/transaction timing wrong for an FS keyboard.
    wr(HCFG, (rd(HCFG) & !0b11) | 1);
    // Ack any pending core interrupts (a stuck SOF/port flag can stall the emulated frame machine).
    wr(GINTSTS, 0xFFFF_FFFF);

    // 6. Power the root port. Preserve the W1C change bits (mask them off so we do not clear pending
    //    connect/enable-change flags), then set PrtPwr.
    let hprt = rd(HPRT) & !HPRT_WC_BITS;
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
    let base = rd(HPRT) & !HPRT_WC_BITS;
    wr(HPRT, base | HPRT_PRTRST);
    spin(3_000_000); // ~50 ms of USB reset (generous; bounded)
    let base = rd(HPRT) & !HPRT_WC_BITS;
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
    // Clear the connect/enable change flags now that we have acted on them (W1C: write 1s back).
    wr(HPRT, (rd(HPRT) & !HPRT_WC_BITS) | HPRT_PRTCONNDET | HPRT_PRTENCHNG);
    // Arm tick-driven enumeration (increment 2). We do NOT enumerate synchronously here: a control
    // transfer must let the controller's transactions run between our polls, which a boot-time busy-spin
    // never allows (QEMU advances the emulated core only on its event loop; hardware completes DMA in
    // silicon but our poll would still hog the CPU). `poll()`, called from the timer tick, drives the
    // state machine one transaction per tick, so the idle WFI between ticks gives the controller time.
    LOW_SPEED.store((hprt & HPRT_PRTSPD_MASK) >> HPRT_PRTSPD_SHIFT == 2, Ordering::Relaxed);
    SM_ACTIVE.store(true, Ordering::Release);
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

/// DMA scratch buffers. Static, so they live in identity-mapped kernel RAM (VA == PA - the DWC2 DMA
/// engine takes a physical address, and the kernel identity map makes the static's address usable
/// directly). Cacheable, so `flush_dcache` cleans+invalidates around every transfer: the A7's DMA is NOT
/// cache-coherent, so without this the device would read stale bytes (OUT) or the CPU would read a stale
/// cache line instead of what the device just wrote (IN).
#[repr(C, align(64))]
struct DmaBuf { setup: [u8; 8], data: [u8; 256] }
static mut DMA: DmaBuf = DmaBuf { setup: [0; 8], data: [0; 256] };

/// Clean+invalidate a cache-line range to the PoC (DCCIMVAC) - the DMA-coherency bracket. Clean pushes
/// any dirty CPU write out to RAM (so the device sees it); invalidate drops the line (so a later CPU read
/// re-fetches what the device wrote). Correct for both directions, so used before AND after DMA.
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

// --- State machine state (core 0 only touches these) ---
static SM_ACTIVE:  AtomicBool = AtomicBool::new(false); // a device is present + enumeration is armed
static LOW_SPEED:  AtomicBool = AtomicBool::new(false); // attached device is low-speed
static SM_STEP:    AtomicU8   = AtomicU8::new(0);       // enumeration step (see step_setup)
static SM_STAGE:   AtomicU8   = AtomicU8::new(0);       // 0=SETUP, 1=DATA, 2=STATUS
static SM_RUNNING: AtomicBool = AtomicBool::new(false); // a channel transaction is in flight
static SM_TICKS:   AtomicU32  = AtomicU32::new(0);      // watchdog: ticks the current stage has waited
static DEV_ADDR:   AtomicU8   = AtomicU8::new(0);       // 0 until SET_ADDRESS assigns 1
static MPS0:       AtomicU8   = AtomicU8::new(8);       // EP0 max packet size (8 until GET_DESCRIPTOR)
static SM_RETRY:   AtomicU8   = AtomicU8::new(0);       // NAK/xact-error retries for the current stage

// Enumeration steps. `done` marks enumeration complete; 255 marks a failure. Increment 2 stops after
// reading the device descriptor (proof the transfers work); increment 3 adds SET_CONFIGURATION +
// SET_PROTOCOL, increment 4 the interrupt-endpoint HID poll.
const STEP_GET_DESC8:  u8 = 0; // GET_DESCRIPTOR(device, 8) -> learn EP0 max packet size
const STEP_SET_ADDR:   u8 = 1; // SET_ADDRESS(1)
const STEP_GET_DESC18: u8 = 2; // GET_DESCRIPTOR(device, 18) @ addr 1 -> VID/PID
const STEP_DONE:       u8 = 3;
const STEP_FAILED:     u8 = 255;

/// The SETUP packet + data direction + data length for a step. `data_in`/`dlen` describe the DATA stage
/// (dlen 0 = no DATA stage - the STATUS goes IN).
fn step_setup(step: u8) -> ([u8; 8], bool, usize) {
    match step {
        STEP_GET_DESC8  => ([0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 8, 0x00], true, 8),
        STEP_SET_ADDR   => ([0x00, 0x05, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00], false, 0),
        STEP_GET_DESC18 => ([0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00], true, 18),
        _               => ([0; 8], false, 0),
    }
}

/// Start one channel-0 transaction (non-blocking - just enables the channel; completion is polled).
fn channel_start(dir_in: bool, pid: u32, buf_phys: u32, len: u32) {
    let mps = MPS0.load(Ordering::Relaxed) as u32;
    let dev_addr = DEV_ADDR.load(Ordering::Relaxed) as u32;
    let low_speed = LOW_SPEED.load(Ordering::Relaxed) as u32;
    let pkts = if len == 0 { 1 } else { (len + mps - 1) / mps };
    wr(HCINT0, 0xFFFF_FFFF);                                    // clear stale channel interrupts
    wr(HCTSIZ0, (len & 0x7_FFFF) | (pkts << 19) | (pid << 29)); // size, packet count, PID
    wr(HCDMA0, buf_phys);
    wr(HCCHAR0, (mps & 0x7FF)
        | ((dir_in as u32) << 15)
        | (low_speed << 17)
        | (1 << 20)                        // multi-count = 1; endpoint 0 + control type are the zero fields
        | ((dev_addr & 0x7F) << 22)
        | (1 << 31));                      // channel enable
}

/// Poll the current stage: start it if idle, else check the channel. Returns true while still busy with
/// this step, false when the step is complete (or on failure, which sets SM_STEP = STEP_FAILED).
fn poll_stage(setup_phys: u32, data_phys: u32, data_in: bool, dlen: usize) -> bool {
    if !SM_RUNNING.load(Ordering::Relaxed) {
        match SM_STAGE.load(Ordering::Relaxed) {
            0 => channel_start(false, PID_SETUP, setup_phys, 8),            // SETUP (always 8, OUT)
            1 => channel_start(data_in, PID_DATA1, data_phys, dlen as u32), // DATA (OUT already flushed)
            _ => channel_start(!data_in, PID_DATA1, data_phys, 0),          // STATUS (opposite dir, 0 len)
        }
        SM_RUNNING.store(true, Ordering::Relaxed);
        SM_TICKS.store(0, Ordering::Relaxed);
        return true;
    }
    let i = rd(HCINT0);
    if i & HCINT_CHHLTD == 0 {
        // Still in flight. Give the controller more ticks; fail loudly if it never completes.
        if SM_TICKS.fetch_add(1, Ordering::Relaxed) > 1000 {
            pl011_write(b"dwc2: enumeration stalled (channel never halted) - USB unavailable\r\n");
            SM_STEP.store(STEP_FAILED, Ordering::Relaxed);
        }
        return true;
    }
    SM_RUNNING.store(false, Ordering::Relaxed);
    if i & HCINT_XFERCOMPL == 0 {
        // The channel halted without completing. A device fresh out of reset routinely NAKs early
        // control transfers (bit 4) and can hit transaction errors (bit 7); both mean "not ready, try
        // again", so re-issue the SAME stage a bounded number of times rather than giving up. A STALL
        // (bit 3) or exhausted retries is a real failure.
        let nak = i & (1 << 4) != 0;
        let xacterr = i & (1 << 7) != 0;
        if (nak || xacterr) && SM_RETRY.fetch_add(1, Ordering::Relaxed) < 200 {
            return true; // SM_RUNNING is false + stage unchanged, so next poll restarts this transaction
        }
        pl011_write(b"dwc2: control transfer error HCINT=");
        write_hex32(i);
        pl011_write(b" step=");
        write_hex32(SM_STEP.load(Ordering::Relaxed) as u32);
        pl011_write(b" stage=");
        write_hex32(SM_STAGE.load(Ordering::Relaxed) as u32);
        pl011_write(b" - USB unavailable\r\n");
        SM_STEP.store(STEP_FAILED, Ordering::Relaxed);
        return false;
    }
    SM_RETRY.store(0, Ordering::Relaxed);
    match SM_STAGE.load(Ordering::Relaxed) {
        0 => { SM_STAGE.store(if dlen > 0 { 1 } else { 2 }, Ordering::Relaxed); true }
        1 => {
            if data_in { flush_dcache(data_phys, dlen as u32); } // publish device-written bytes to the CPU
            SM_STAGE.store(2, Ordering::Relaxed);
            true
        }
        _ => { SM_STAGE.store(0, Ordering::Relaxed); false } // STATUS done -> step complete
    }
}

static POLL_BUSY: AtomicBool = AtomicBool::new(false);

/// Advance the enumeration by (at most) one transaction. Called from the Core-0 timer tick AND the Core-0
/// idle loop; a re-entry guard stops the tick (which can preempt the idle mid-poll) from racing the
/// state machine on the same core.
pub fn poll() {
    if POLL_BUSY.swap(true, Ordering::Acquire) { return; }
    poll_inner();
    POLL_BUSY.store(false, Ordering::Release);
}

/// Between calls the core idles (WFI), which is what lets the controller run its transactions.
fn poll_inner() {
    if !SM_ACTIVE.load(Ordering::Acquire) { return; }
    let step = SM_STEP.load(Ordering::Relaxed);
    if step >= STEP_DONE { return; }

    let (setup, data_in, dlen) = step_setup(step);
    // SAFETY: DMA is a static touched only here on core 0; `addr_of` yields its identity-mapped physical
    // address. The SETUP / OUT-DATA buffer is filled + flushed only while no channel is running
    // (SM_RUNNING gates that), so the DMA engine never reads a half-written buffer.
    let still_busy = unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DMA);
        let setup_phys = core::ptr::addr_of!(d.setup) as u32;
        let data_phys = core::ptr::addr_of!(d.data) as u32;
        if !SM_RUNNING.load(Ordering::Relaxed) {
            match SM_STAGE.load(Ordering::Relaxed) {
                0 => { d.setup.copy_from_slice(&setup); flush_dcache(setup_phys, 8); }
                1 if !data_in => flush_dcache(data_phys, dlen as u32), // (OUT payload would be filled here)
                _ => {}
            }
        }
        poll_stage(setup_phys, data_phys, data_in, dlen)
    };
    if still_busy { return; }

    // Step just completed - consume its result and advance.
    match step {
        STEP_GET_DESC8 => {
            // SAFETY: read the invalidated device-descriptor bytes the DMA delivered.
            let d = unsafe { (*core::ptr::addr_of!(DMA)).data };
            // Diagnostic: first 8 descriptor bytes - bLength(0x12) bDescType(0x01) bcdUSB.. bMaxPacketSize0.
            pl011_write(b"dwc2: desc8=");
            write_hex32((d[0] as u32) << 24 | (d[1] as u32) << 16 | (d[2] as u32) << 8 | d[3] as u32);
            write_hex32((d[4] as u32) << 24 | (d[5] as u32) << 16 | (d[6] as u32) << 8 | d[7] as u32);
            pl011_write(b"\r\n");
            MPS0.store(if d[7] == 0 { 8 } else { d[7] }, Ordering::Relaxed);
        }
        STEP_SET_ADDR => DEV_ADDR.store(1, Ordering::Relaxed),
        STEP_GET_DESC18 => {
            // SAFETY: read the invalidated 18-byte device descriptor.
            let d = unsafe { (*core::ptr::addr_of!(DMA)).data };
            let vid = (d[8] as u32) | ((d[9] as u32) << 8);
            let pid = (d[10] as u32) | ((d[11] as u32) << 8);
            pl011_write(b"dwc2: enumerated device VID:PID=");
            write_hex32((vid << 16) | pid);
            pl011_write(b" class=");
            write_hex32(d[4] as u32);
            pl011_write(b" mps0=");
            write_hex32(MPS0.load(Ordering::Relaxed) as u32);
            pl011_write(b" - control transfers work (tick-driven)\r\n");
        }
        _ => {}
    }
    SM_STEP.store(step + 1, Ordering::Relaxed);
}
