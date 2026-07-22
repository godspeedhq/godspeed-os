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
const GSNPSID:  usize = 0x040; // Synopsys core ID ("OT2" + release, e.g. 0x4F54_294A)
const GHWCFG2:  usize = 0x048; // hardware config 2 (architecture, HS PHY type)
// --- Host-mode registers ---
const HCFG:     usize = 0x400; // host config (PHY clock select)
const HPRT:     usize = 0x440; // host port control + status (root port)
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
    pl011_write(b" - ready for enumeration (increment 2)\r\n");
    // Clear the connect/enable change flags now that we have acted on them (W1C: write 1s back).
    wr(HPRT, (rd(HPRT) & !HPRT_WC_BITS) | HPRT_PRTCONNDET | HPRT_PRTENCHNG);
}
