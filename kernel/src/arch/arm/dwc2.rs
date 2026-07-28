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

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};

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
const GNPTXSTS: usize = 0x02C; // non-periodic TX FIFO/queue status (low 16 = words free): did the SSPLIT drain?
const GRXFSIZ:  usize = 0x024; // receive FIFO size
const GNPTXFSIZ:usize = 0x028; // non-periodic transmit FIFO size
const GSNPSID:  usize = 0x040; // Synopsys core ID ("OT2" + release, e.g. 0x4F54_294A)
const GHWCFG2:  usize = 0x048; // hardware config 2 (architecture, HS PHY type)
const GHWCFG3:  usize = 0x04C; // hardware config 3 (bits 31:16 = total DFIFO depth in 32-bit words)
const HPTXFSIZ: usize = 0x100; // host periodic transmit FIFO size
// --- Host-mode registers ---
const HCFG:     usize = 0x400; // host config (PHY clock select)
const HPRT:     usize = 0x440; // host port control + status (root port)
const HFIR:     usize = 0x404; // host frame interval (Circle writes 48000 for a full-speed host)
const HFNUM:    usize = 0x408; // host frame number (low 16) + frame remaining (high 16)
// HCCHAR bit 29: schedule the transaction in an ODD (micro)frame. Circle sets this from the current
// frame number on every channel start; some DWC2 cores gate a channel's dispatch on the parity match,
// so a fixed value can leave the channel armed (ChEna set) but never executed - the DMA master idle.
const HCCHAR_ODDFRM: u32 = 1 << 29;
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
/// Per-channel register base: the DWC2 lays its host channels out at 0x500 + ch*0x20, and this board
/// reports **8** of them (GHWCFG2[17:14]+1, read at init - where we already halt every one).
///
/// We used to program every transfer through channel 0. Keyboard polls, mass-storage bulk and ethernet
/// frames shared one set of registers, which is why an interrupt-split abandoned mid-flight by the ISR
/// budget corrupted the next block transfer: not because they ran at the same time (they cannot - the
/// poll runs in the timer ISR, block I/O in an IRQ-masked syscall) but because the abandoned transfer
/// LEFT ITS STATE on the channel the next user inherited.
///
/// Linux allocates a channel per active transfer from a free list (`free_hc_list`,
/// `dwc2_assign_and_init_hc`). It needs a pool because it juggles arbitrary concurrent URBs; we have
/// three statically-known streams, so a pool would be speculative generality (§26.2). One channel per
/// stream buys the isolation the pool exists to provide, without the bookkeeping.
#[inline] fn hcchar_at(ch: u32)   -> usize { 0x500 + (ch as usize) * 0x20 }
#[inline] fn hcsplt_at(ch: u32)   -> usize { 0x504 + (ch as usize) * 0x20 }
#[inline] fn hcint_at(ch: u32)    -> usize { 0x508 + (ch as usize) * 0x20 }
#[inline] fn hcintmsk_at(ch: u32) -> usize { 0x50C + (ch as usize) * 0x20 }
#[inline] fn hctsiz_at(ch: u32)   -> usize { 0x510 + (ch as usize) * 0x20 }
#[inline] fn hcdma_at(ch: u32)    -> usize { 0x514 + (ch as usize) * 0x20 }

/// Bulk + control + enumeration. Boot or syscall context, IRQs masked.
const CH_BULK: u32 = 0;
/// The keyboard's periodic poll. Runs in the core-0 timer ISR and is the ONE transfer deliberately
/// abandoned when its budget expires - so it must not share a channel with anything.
const CH_KBD:  u32 = 1;
/// USB-ethernet frames.
const CH_NET:  u32 = 2;

/// Return a channel to a clean state, as `dwc2_hc_cleanup` does: mask its interrupts and clear every
/// latched status bit. An abandoned transfer leaves both set, and the next user of that channel would
/// otherwise read a previous transfer's completion as its own.
#[allow(dead_code)]
fn chan_release(ch: u32) {
    wr(hcintmsk_at(ch), 0);
    wr(hcint_at(ch), 0xFFFF_FFFF);
}

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

// u-boot's host init clears GOTGCTL.HstSetHNPEn (a host must not have it set).
const GOTGCTL_HSTSETHNPEN: u32 = 1 << 10;

// GAHBCFG bits. u-boot's rpi value is `DMAEN | (INCR4 = 3<<1) | GLBLINTRMSK`.
const GAHBCFG_GLBLINTRMSK: u32 = 1 << 0; // global interrupt enable
const GAHBCFG_DMAEN:       u32 = 1 << 5; // DMA mode enable

// GUSBCFG bits. The PHY-interface + OTG bits below configure the real BCM2836 UTMI+ PHY (QEMU ignores
// them). u-boot/Circle select the UTMI+ 8-bit interface and disable OTG HNP/SRP for a pure host.
const GUSBCFG_PHYIF:         u32 = 1 << 3;  // UTMI+ data width: 0 = 8-bit (Pi), 1 = 16-bit
const GUSBCFG_ULPI_UTMI_SEL: u32 = 1 << 4;  // PHY interface: 0 = UTMI+ (Pi), 1 = ULPI
const GUSBCFG_PHYSEL:     u32 = 1 << 6;  // 1 = full-speed serial PHY, 0 = USB 2.0 HS PHY (UTMI+)
const GUSBCFG_SRP_CAPABLE:   u32 = 1 << 8;  // OTG SRP - off for a pure host
const GUSBCFG_HNP_CAPABLE:   u32 = 1 << 9;  // OTG HNP - off for a pure host
const GUSBCFG_ULPI_EXT_VBUS: u32 = 1 << 20; // drive VBUS externally (ULPI) - off
const GUSBCFG_TERM_SEL_DL:   u32 = 1 << 22; // TermSel DLine pulsing - off
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

    // 2b. (Pi) Before the reset, clear the ULPI external-VBUS-drive and TermSel-DLine-pulse bits, matching
    //     Circle's working BCM2836 init. Harmless on the UTMI+ PHY; QEMU ignores them.
    wr(GUSBCFG, rd(GUSBCFG) & !(GUSBCFG_ULPI_EXT_VBUS | GUSBCFG_TERM_SEL_DL));

    // 3. Core soft reset: sets defaults and clears the FIFOs. Self-clears when done.
    wr(GRSTCTL, rd(GRSTCTL) | GRSTCTL_CSFTRST);
    let mut waited = 0u32;
    while rd(GRSTCTL) & GRSTCTL_CSFTRST != 0 {
        waited += 1;
        if waited > 1_000_000 { pl011_write(b"dwc2: WARN core soft reset did not clear\r\n"); break; }
    }
    // Let the PHY settle after reset.
    spin(200_000);

    // 4. Select the PHY interface + force HOST mode (Circle's working Pi 2 sequence). On the real BCM2836
    //    the UTMI+ 8-bit interface MUST be selected (clear ULPI_UTMI_SEL + PHYIF) and OTG HNP/SRP disabled,
    //    or the PHY never clocks a transaction (the channel arms, ChEna stays set, and the DMA master never
    //    starts - AHBIdle stuck 1, HW-diagnosed on the Pi 2). QEMU ignores all of this, so it only matters
    //    on silicon. The core samples ForceHstMode ~25 ms after the write, so wait for CurMode=host.
    let mut cfg = rd(GUSBCFG);
    cfg &= !(GUSBCFG_ULPI_UTMI_SEL | GUSBCFG_PHYIF | GUSBCFG_SRP_CAPABLE | GUSBCFG_HNP_CAPABLE);
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
    // 5a. (u-boot host_init) Clear GOTGCTL.HstSetHNPEn - a pure host must not have host-set-HNP enabled.
    wr(GOTGCTL, rd(GOTGCTL) & !GOTGCTL_HSTSETHNPEN);

    // 5b. Size the FIFOs (values are 32-bit words) to the Linux BCM2835 host-mode layout, which is the
    //     authoritative Pi value (`params_bcm2835` in `drivers/usb/dwc2/params.c`): RX 774, non-periodic
    //     TX 256, periodic TX 512. GNPTXFSIZ/HPTXFSIZ pack (depth << 16) | start_address, laid end to end
    //     (RX @0, NPTX @774, PTX @1030). Total 1542 words, well under this core's DFIFO depth.
    //
    //     The previous values - RX 256, NPTX 128, PTX 128, "ample for a single keyboard's tiny transfers"
    //     - were sized before this driver grew a mass-storage backend. A high-speed bulk packet is 512
    //     bytes = 128 words, so the whole non-periodic TX FIFO held exactly ONE packet and the RX FIFO
    //     had almost no headroom for the DMA engine's drain latency. Under SUSTAINED bulk I/O (a `drives
    //     check` reading the whole tree) that starves: the RX FIFO cannot buffer the next packet while
    //     the DMA drains the last, the host channel wedges, and the device appears to "stop answering
    //     EP0" - the ~once-per-run drop-off the recovery machinery has been catching. A keyboard never
    //     hit it because its reports are 8 bytes. Right-sizing removes the starvation at the source.
    //
    //     Read the core's total DFIFO depth (GHWCFG3[31:16]) and refuse to program a layout that would
    //     not fit, loudly (invariant 12) - a silently-truncated FIFO boundary is the exact stale-pointer
    //     class the flush below exists to prevent.
    const RX_WORDS:   u32 = 774;
    const NPTX_WORDS: u32 = 256;
    const PTX_WORDS:  u32 = 512;
    let dfifo_depth = rd(GHWCFG3) >> 16;
    pl011_write(b"dwc2: DFIFO depth "); write_hex32(dfifo_depth);
    pl011_write(b" words; sizing RX/NPTX/PTX 774/256/512 (Linux bcm2835)\r\n");
    if dfifo_depth != 0 && dfifo_depth < RX_WORDS + NPTX_WORDS + PTX_WORDS {
        pl011_write(b"dwc2: WARN DFIFO too small for the bcm2835 layout - USB may be unstable under load\r\n");
    }
    wr(GRXFSIZ, RX_WORDS);
    wr(GNPTXFSIZ, (NPTX_WORDS << 16) | RX_WORDS);
    wr(HPTXFSIZ, (PTX_WORDS << 16) | (RX_WORDS + NPTX_WORDS));
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
    // 5c. GAHBCFG = internal-DMA enable + INCR4 AHB burst + global-interrupt enable. u-boot's EXACT value
    //     for the rpi (`DWC2_GAHBCFG_DMA_EN | HBSTLEN_INCR4 | GLBLINTRMSK`, = 0x27). The transfer is DMA:
    //     the core moves each packet to/from the buffer HCDMA points at (we still POLL HCINT for
    //     completion). Same on QEMU and HW - u-boot drives DMA on the real Pi 2, so DMA is the faithful
    //     transcription's mode. (NOT Circle's WAIT_AXI_WRITES, which was a wrong turn; INCR4 is u-boot's.)
    wr(GAHBCFG, GAHBCFG_DMAEN | (3 << 1) | GAHBCFG_GLBLINTRMSK);
    // 5d. Host-channel interrupt masks. Linux dwc2 sets HCINTMSK + HAINTMSK + GINTMSK before enabling ANY
    //     channel (byte-level diff, 2026-07-24). The direct DMA path worked here WITHOUT them (u-boot omits
    //     them) - but u-boot does NOT do low-speed SPLITs, and Linux does. On the v2.80a the core advances
    //     the split state machine / registers the hub's SSPLIT ACK through the channel-interrupt path, so a
    //     split run with the masks OFF transmits the SSPLIT but never sees the ACK -> XactErr every
    //     microframe (exactly our HW data). So set the masks unconditionally, matching Linux. QEMU already
    //     needed them; this makes HW match. No USB IRQ is wired on ARM - the interrupts pend unserviced,
    //     which is fine: we poll HCINT; the masks only gate the core's own state-machine advancement.
    // Unmask the interrupt sources on every channel we use. This armed channel 0 only, from when
    // channel 0 was the only one.
    for c in [CH_BULK, CH_KBD, CH_NET] { wr(hcintmsk_at(c), 0x7FF); }
    // HAINTMSK gates which channels' interrupts reach the top-level Hchint (and thus the CPU, now that
    // the USB IRQ is routed). It does NOT gate a channel's own state-machine advancement - that is
    // HCINTMSK, set to 0x7FF per channel above, and left on for every channel so splits still advance.
    // Stage 0 of the interrupt-driven conversion drives NO channel from the ISR yet (the transfers are
    // still polled), so no channel may assert Hchint or the level-triggered line would storm between
    // poll cycles. Gate them all off here; each stage that moves a channel to interrupt service unmasks
    // exactly that channel's bit. Port interrupts (Prtint) are not gated by HAINTMSK and are the ISR's
    // only live source for now - enough to prove the route end to end.
    wr(HAINTMSK, 0x0000);
    // Hchint (25) + Prtint (24), plus a TEMPORARY stage-0 delivery probe: SOF (bit 3), which the core
    // raises every microframe while the port is enabled. A port change is a one-shot event already
    // consumed by the boot reset, so it cannot prove the route; a SOF fires continuously, so the very
    // first one delivered proves the whole legacy-controller -> GPU-funnel -> core path works (and, on
    // QEMU, whether its model raises USB interrupts at all). `on_usb_irq` disables SOF the instant it
    // sees one, so it proves delivery without storming. Removed once a channel drives the ISR for real.
    wr(GINTMSK, (1 << 25) | (1 << 24) | (1 << 3));
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

    // Stage 0 of the interrupt-driven conversion: route the USB IRQ to core 0 now that the controller
    // is up and every channel is gated off Hchint (HAINTMSK=0). From here the port interrupt reaches
    // the CPU; the transfer path is unchanged (still polled). This proves the whole route - legacy
    // controller -> GPU funnel -> core IRQ -> dispatcher -> on_usb_irq - end to end, which is the
    // go/no-go for the rest of the conversion (and, under QEMU, whether its DWC2 model delivers it).
    super::irq::route_usb_irq_to_core0();
    pl011_write(b"dwc2: USB interrupt routed to core 0 (port events now interrupt-driven)\r\n");
}

/// Count of USB interrupts serviced, for the boot proof and later diagnostics.
static USB_IRQ_COUNT: AtomicU32 = AtomicU32::new(0);
static USB_IRQ_PROVEN: AtomicBool = AtomicBool::new(false);
pub fn usb_irq_count() -> u32 { USB_IRQ_COUNT.load(Ordering::Relaxed) }

/// The USB interrupt service routine, reached from `arm_irq_dispatch` (core 0) when the GPU funnel
/// shows the USB line pending. Runs with IRQs masked (IRQ-mode entry).
///
/// Stage 0 handles ONLY the port interrupt (Prtint): a connect/enable/overcurrent change. Channel
/// interrupts are gated off at HAINTMSK, so Hchint cannot reach here yet - the transfers are still
/// polled. The one job is to clear the condition so the level-triggered line deasserts; a port change
/// is acknowledged by writing 1 to the set W1C change bits in HPRT (which also clears the derived,
/// read-only GINTSTS.Prtint). PrtEna is masked off the write so acknowledging a change cannot disable
/// the port (the W1C trap the rest of this driver already guards).
pub fn on_usb_irq() {
    let n = USB_IRQ_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let g = rd(GINTSTS);
    if g & (1 << 3) != 0 {           // SOF: the stage-0 delivery probe (see init)
        // Disable SOF immediately so it does not storm, clear it (W1C), and announce ONCE that the
        // interrupt route works end to end. This whole branch is temporary scaffolding.
        wr(GINTMSK, rd(GINTMSK) & !(1 << 3));
        wr(GINTSTS, 1 << 3);
        if !USB_IRQ_PROVEN.swap(true, Ordering::Relaxed) {
            pl011_write(b"dwc2: USB IRQ DELIVERY CONFIRMED (SOF) - the interrupt route works end to end\r\n");
        }
    }
    if g & (1 << 24) != 0 {          // Prtint: a port change is latched in HPRT's W1C bits
        let hprt = rd(HPRT);
        let changes = hprt & HPRT_WC_BITS;
        // Write the change bits back (set = clear), keeping PrtPwr, dropping PrtEna so we cannot
        // disable the port by acknowledging.
        wr(HPRT, (hprt & !(HPRT_PRTENA | HPRT_WC_BITS)) | changes);
        if n <= 4 {
            pl011_write(b"dwc2: USB IRQ #"); super::timer::write_dec_pub(n);
            pl011_write(b" port change HPRT="); write_hex32(hprt);
            pl011_write(b"\r\n");
        }
    }
    if g & (1 << 25) != 0 {          // Hchint: a host channel halted (only gated channels reach here)
        let haint = rd(HAINT);
        // CH_BULK is the only interrupt-driven channel today (async storage, stage 2); the keyboard and
        // net channels stay polled and are not gated into HAINTMSK, so they never reach this path.
        if haint & (1 << CH_BULK) != 0 { async_bulk_isr(); }
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
    super::timer::delay_us(10_000); // TRSTRCY: 10 ms reset recovery on the real 1 MHz clock (not the freq-dependent spin)

    let mut waited = 0u32;
    while rd(HPRT) & HPRT_PRTENA == 0 {
        waited += 1;
        if waited > 2_000_000 { pl011_write(b"dwc2: WARN port did not enable after reset\r\n"); return; }
    }
    let hprt = rd(HPRT);
    pl011_write(b"dwc2: root port enabled after reset, ");
    pl011_write(speed_name(hprt).as_bytes());
    pl011_write(b"\r\n");
    // (Pi) A full/low-speed host needs the frame interval set explicitly (Circle: HFIR=48000 for a
    //  full-speed port). The DWC2 request scheduler times transactions off HFIR; on the Pi the on-board
    //  hub enumerates full-speed, and leaving HFIR at its power-on default can leave non-periodic
    //  transfers undispatched (the SETUP channel arms but the DMA master never starts). QEMU ignores it.
    let spd = (hprt & HPRT_PRTSPD_MASK) >> HPRT_PRTSPD_SHIFT;
    if spd == 1 || spd == 2 { wr(HFIR, 48000); }
    // Clear the connect/enable change flags now that we have acted on them (W1C: write 1s back). Mask
    // PrtEna off (HPRT_RMW_CLEAR) so writing these change bits does not also disable the port.
    wr(HPRT, (rd(HPRT) & !HPRT_RMW_CLEAR) | HPRT_PRTCONNDET | HPRT_PRTENCHNG);
    // Enumerate synchronously in slave/PIO mode. Enumeration is a one-time bounded boot cost, and slave
    // mode needs prompt FIFO servicing (a tick-spaced poll would under/overrun the FIFO), so a bounded
    // busy-poll here is the right shape. The DWC2's internal DMA master never initiated a transfer on this
    // board (AHBIdle stayed 1 across a dozen HW tests), so PIO is the working path.
    LOW_SPEED.store((hprt & HPRT_PRTSPD_MASK) >> HPRT_PRTSPD_SHIFT == 2, Ordering::Relaxed);
    // NOTE: a ~1 s host-mode settle before enumeration (u-boot's `dwc2_init_common` mdelay(1000)) was tried
    // on HW and REMOVED - even at an accurate full second it did NOT dispatch the SETUP, and it FROZE the
    // frame counter (HFNUM stopped advancing = SOF gated off during the long idle), so the long idle lets
    // the port stop framing. The v2.80a "channel arms but the master won't dispatch channel 0" wall stays
    // unresolved (DMA and PIO alike); see the git log + docs/arm32-status.md. Enumerate now (SOF is running).
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
const HCINT_NAK: u32 = 1 << 4;   // the device positively answered "nothing new"
const HCINT_CHHLTD:    u32 = 1 << 1;
const HCINT_STALL:     u32 = 1 << 3;   // the endpoint is halted - a HARD failure, never retried
const HCINT_NYET:      u32 = 1 << 6;   // split: the TT has not finished yet - retry the CSPLIT
const HCINT_XACTERR:   u32 = 1 << 7;   // a real transaction error (CRC, timeout, bit-stuff, toggle)

/// How many genuine TRANSACTION errors a transfer tolerates before it fails. Matches Linux, which
/// fails a QTD at `error_count >= 3` in `dwc2_release_channel`.
const XACT_ERR_MAX: u32 = 3;

/// How long a single transfer attempt may HOLD THE CORE while the device says "busy, ask again".
///
/// This is not a failure deadline, and the difference matters. Linux never gives up on a NAK: it is
/// interrupt-driven, so the channel halts, the QTD is re-queued, and the transfer resumes whenever the
/// device is ready - there is no wall clock anywhere in that path. Commandment VIII says the same
/// thing from the other direction: wait on TRUTH (the transfer completed), never on time.
///
/// The previous 250 ms value was a clock standing in for that truth, and the measurement says so
/// plainly: 42 of 42 transfer failures across nine runs were this timeout expiring. Zero XACTERR, zero
/// STALL. The device was never failing - it was busy, and we walked away from work that was going to
/// succeed, then escalated into resetting a healthy device.
///
/// We cannot simply wait longer: a block transfer runs in a syscall with IRQs masked, so waiting is
/// paid for by the timer tick not running (kernel-audit K7-1). So the attempt is bounded by how long
/// we may hold the CORE, and a device still busy at that point yields `busy` rather than `failed` -
/// the caller re-asks with interrupts enabled in between, which is where the real waiting belongs and
/// costs nothing. Bounded (§26.6) and truth-based (VIII), instead of trading one against the other.
const CORE_HOLD_US: u32 = 5_000;

/// A temporarily-raised NAK budget for the boot/probe path, in microseconds; 0 = use `CORE_HOLD_US`.
///
/// The steady-state budget is deliberately tiny (5 ms) because a normal transfer runs in a syscall with
/// interrupts masked, and the userspace caller re-asks a busy device with interrupts ON in between (the
/// real waiting belongs there). But the PROBE read runs at boot, before any userspace exists to re-ask,
/// and one SanDisk needs far longer than 5 ms to produce its first block: it NAKs the data phase, we
/// abandon it after 5 ms, and re-issuing the command only wedges it (it is still busy with the read we
/// walked away from - even a Mass Storage Reset then NAKs). The cure is the opposite of retrying: give a
/// SINGLE read one long, uninterrupted poll so the device can finish and hand the data over on the same
/// transfer. This raises the budget only around that one probe read, then restores it.
static IO_NAK_BUDGET_US: AtomicU32 = AtomicU32::new(0);
#[inline]
fn nak_budget_us() -> u32 {
    let p = IO_NAK_BUDGET_US.load(Ordering::Relaxed);
    if p > 0 { p } else { CORE_HOLD_US }
}

/// A block read/write's patient NAK backstop, in microseconds. A real transfer ends the wait early by
/// completing (`XferCompl`) or failing (STALL/XactErr) - this only bounds how long a device that keeps
/// NAKing (busy, still working) is waited on before we call it stuck (§26.6). Larger than the tiny
/// steady-state `CORE_HOLD_US` because the alternative - abandon after 5 ms and let userspace re-issue
/// the whole command - WEDGES a slow stick: it is still busy with the transfer we walked away from, so
/// the re-issued command NAKs too, forever. A slow SanDisk needs its READ(10)/WRITE(10) waited on, not
/// re-issued; this lets ONE command finish. It is IRQs-masked core-hold, so it is a backstop, not a
/// target - a healthy device is in and out in well under a millisecond.
const IO_BUDGET_US: u32 = 500_000;

/// RAII: raise the NAK backstop for a block I/O command, and restore it on ANY exit path (Drop). Using
/// a guard rather than manual store/restore means an early return cannot leave the backstop raised for
/// the next, unrelated transfer.
struct NakBudget;
impl NakBudget {
    fn raised(us: u32) -> Self { IO_NAK_BUDGET_US.store(us, Ordering::Relaxed); NakBudget }
}
impl Drop for NakBudget {
    fn drop(&mut self) { IO_NAK_BUDGET_US.store(0, Ordering::Relaxed); }
}

/// Why the last `chan_dma` gave up. A transfer can fail for reasons that need OPPOSITE responses - a
/// device rejecting us (STALL), a genuinely broken link (XACTERR), or one that stayed busy longer than
/// we can afford to wait (NAK timeout) - and `bot CBW-out failed` said none of them. Guessing between
/// them is what produced three wrong diagnoses of the user-task faults before the fault reporter was
/// taught to name things. Same fix, same reason.
static LAST_FAIL: AtomicU32 = AtomicU32::new(0);
const FAIL_STALL: u32 = 1;
const FAIL_XACT:  u32 = 2;
const FAIL_NAK_TIMEOUT: u32 = 3;

/// Was the last transfer merely BUSY (the device asked us to come back) rather than failed? The
/// caller re-asks; nothing is wrong.
pub fn msc_last_was_busy() -> bool { LAST_FAIL.load(Ordering::Relaxed) == FAIL_NAK_TIMEOUT }

/// One word for why the last transfer failed (see `LAST_FAIL`).
fn last_fail_str() -> &'static str {
    match LAST_FAIL.load(Ordering::Relaxed) {
        FAIL_STALL => "STALL (endpoint halted - device refused)",
        FAIL_XACT  => "3x XACTERR (real transaction errors - link/CRC/timeout)",
        FAIL_NAK_TIMEOUT => "device busy (NAK) - handing the core back, the caller re-asks",
        _ => "unknown",
    }
}

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
// Max packet size of the CURRENTLY SELECTED endpoint. **u16, not u8**: a high-speed bulk endpoint is
// 512 bytes, which a u8 cannot represent at all - and `wMaxPacketSize` is a 16-bit descriptor field
// whose LOW byte is 0 for exactly that size, so a byte-wide parse silently read 512 as "0" and fell
// back to 64. That mismatch is invisible for short replies and breaks every full-size transfer.
static MPS0:      AtomicU16  = AtomicU16::new(8);      // EP0 max packet size (8 until GET_DESCRIPTOR)

// --- boot-keyboard poll state (set once enumeration finds a keyboard behind the hub) ---
static KBD_READY:  AtomicBool = AtomicBool::new(false); // a boot keyboard is configured + pollable
static KBD_ADDR:   AtomicU8   = AtomicU8::new(0);       // its assigned USB address
static KBD_EP:     AtomicU8   = AtomicU8::new(0);       // its interrupt IN endpoint number
static KBD_MPS:    AtomicU8   = AtomicU8::new(8);       // its interrupt endpoint max-packet
static KBD_LOW:    AtomicBool = AtomicBool::new(false); // whether it is a low-speed device
static KBD_TOGGLE: AtomicBool = AtomicBool::new(false); // DATA0/DATA1 toggle for the interrupt endpoint
static KBD_HUB_PORT: AtomicU8 = AtomicU8::new(0);      // hub port the keyboard is on (for split; 0 = direct)

/// Hub port (1-based) the CURRENTLY-selected device sits on when it is low/full-speed behind the high-speed
/// LAN9514 hub - such a device is only reachable via SPLIT transactions (the hub does the low-speed transfer;
/// the host talks to the hub at high speed). 0 = a direct (high-speed or root) device, no split. Set by the
/// enumeration/poll paths after `select_device`, which clears it. The hub itself is always device address 1.
static SPLIT_PORT: AtomicU8 = AtomicU8::new(0);

/// Point channel 0 at a specific device before a transaction. With more than one device behind the hub
/// (a keyboard AND the ethernet), the single host channel is time-shared: each transfer path selects its
/// device's address / EP0-or-endpoint max-packet / speed into the globals `chan_program` reads.
fn select_device(addr: u8, mps: u16, low: bool) {
    DEV_ADDR.store(addr, Ordering::Relaxed);
    MPS0.store(mps as u16, Ordering::Relaxed);
    LOW_SPEED.store(low, Ordering::Relaxed);
    SPLIT_PORT.store(0, Ordering::Relaxed);                     // direct by default; split set explicitly after
}

/// Build the HCSPLT value for the currently-selected device: 0 for a direct device, or a Start-Split
/// descriptor when `SPLIT_PORT` is set. HCSPLT layout (DWC2 databook / Circle `dwhci.h`):
///   PrtAddr [6:0]  = the hub PORT the device is on,
///   HubAddr [13:7] = the hub's DEVICE address (1),
///   XactPos [15:14] = ALL (3), CompSplit [16] (set by the caller for the CSPLIT phase), SplEna [31].
/// NOTE: these two fields were SWAPPED for the entire debug saga (hub address written into the port field
/// and vice-versa), so every SSPLIT was addressed to hub-address = port -> no hub answered -> XactErr in
/// every microframe. Correct order below.
fn hcsplt_for_current() -> u32 {
    let port = SPLIT_PORT.load(Ordering::Relaxed) as u32;
    if port == 0 { 0 } else { (port & 0x7F) | (1 << 7) | (0b11 << 14) | (1 << 31) } // PrtAddr=port, HubAddr=1, XactPos=ALL, SplEna
}

/// Program + enable channel 0 for one transaction. `ep`/`ep_type` select the endpoint (0/control for the
/// enumeration path, the keyboard's IN endpoint / interrupt=3 for polling); device address, EP0 max-packet
/// and speed come from the globals the enumeration steps set. `hcsplt` is the split-transaction descriptor
/// (0 for a direct device). The DWC2 DMA master moves the data itself.
fn chan_program(ch: u32, dir_in: bool, pid: u32, len: u32, buf_phys: u32, ep: u32, ep_type: u32, hcsplt: u32) {
    let mps = MPS0.load(Ordering::Relaxed) as u32;
    let dev_addr = DEV_ADDR.load(Ordering::Relaxed) as u32;
    let low_speed = LOW_SPEED.load(Ordering::Relaxed) as u32;
    let pkts = if len == 0 { 1 } else { (len + mps - 1) / mps };
    // Channel-reuse hygiene: if a prior transaction left the channel ENABLED (a timeout that never truly
    // halted, or a split phase re-arm), disable it cleanly before reprogramming - never reuse a half-live
    // channel (fresh-eyes checklist: stale ChEna/ChDis before reuse).
    if rd(hcchar_at(ch)) & (1 << 31) != 0 {
        wr(hcchar_at(ch), (rd(hcchar_at(ch)) & !(1 << 31)) | (1 << 30));    // ChDis: clear ChEna, set ChDis
        let mut t = 0u32; while rd(hcchar_at(ch)) & (1 << 31) != 0 { t += 1; if t > 100_000 { break; } }
    }
    wr(hcint_at(ch), 0xFFFF_FFFF);                                     // clear stale channel interrupts
    wr(hctsiz_at(ch), (len & 0x7_FFFF) | ((pkts & 0x3ff) << 19) | (pid << 29));  // size, packet count (10-bit field), starting PID
    // The HCDMA address is a *bus* address as the DWC2 master sees memory (see DMA_BUS_ALIAS).
    wr(hcdma_at(ch), buf_phys | DMA_BUS_ALIAS);
    wr(hcsplt_at(ch), hcsplt);                                         // split descriptor - LAST before HCCHAR (Linux order: HCTSIZ -> HCSPLT -> HCCHAR)
    // Odd-frame scheduling applies to PERIODIC transfers AND SPLIT transactions (a split's SSPLIT/CSPLIT
    // are microframe-scheduled by the hub's TT - fresh-eyes checklist: derive OddFrm from the target frame
    // including control splits). Target the NEXT microframe: OddFrm set when the current one is even (so the
    // token lands in the next, odd, one). A direct non-periodic transfer keeps OddFrm = 0 (setting it there
    // makes the v2.80a core defer the token and strand the bytes - HW-diagnosed).
    let oddfrm = if (ep_type == 3 || hcsplt != 0) && (rd(HFNUM) & 1) == 0 { HCCHAR_ODDFRM } else { 0 };
    let chan = (mps & 0x7FF)
        | ((ep & 0xF) << 11)               // endpoint number
        | ((dir_in as u32) << 15)
        | (low_speed << 17)                // low-speed device - Linux sets LSpdDev even for a low-speed split
        | ((ep_type & 0x3) << 18)          // 0=control, 2=bulk, 3=interrupt
        | (1 << 20)                        // multi-count = 1 (Linux: ec_mc = 1 for control/bulk, incl. split)
        | ((dev_addr & 0x7F) << 22)        // the low-speed DEVICE address (the hub address rides HCSPLT)
        | oddfrm                           // odd/even frame parity (Circle sets this per start)
        | (1 << 31);                       // channel enable
    wr(hcchar_at(ch), chan);
}

static DUMPED: AtomicBool = AtomicBool::new(false);

/// A tighter-bounded halt wait for the STEADY-STATE keyboard poll, which runs inside the core-0 timer ISR
/// (SEC-32): a wedged/hostile controller must not tax the ISR the full enumeration budget every tick. A
/// normal interrupt-IN halts (complete or NAK) in microseconds, so this budget never affects the working
/// path; it only bounds the pathological per-tick cost (the keyboard auto-recovers if the wedge clears).
///
/// The bound is by REAL TIME (the 1 MHz System Timer), not a spin count: a spin count's duration depends
/// on the peripheral-bus MMIO latency, and the old 500k-spin cap could reach tens of ms - longer than the
/// 10 ms scheduler tick - so a keyboard whose split does not halt promptly starved the scheduler and wedged
/// the boot (HW-observed on a Logitech low-speed keyboard behind the LAN9514 hub). `POLL_HALT_BUDGET_US`
/// keeps a single wait well under one tick.
fn poll_wait_halt(ch: u32) -> u32 {
    let start = super::timer::systimer_us();
    loop {
        let ci = rd(hcint_at(ch));
        if ci & HCINT_CHHLTD != 0 { return ci; }
        if super::timer::systimer_us().wrapping_sub(start) > POLL_HALT_BUDGET_US {
            return ci | HCINT_CHHLTD; // treat as halted-without-complete -> the poll gives up this tick
        }
    }
}
/// Per in-ISR halt wait. Generous enough not to ABANDON a transfer that is about to complete: if this
/// expires just as the device's data lands, the controller has already ACKed it - the device considers
/// the report delivered and will not resend - so we would silently LOSE that report. Losing a key-RELEASE
/// report is what left a key armed and auto-repeating until the next keypress (`appearddddddddd` on real
/// hardware). Worst case per poll is one start-split plus three complete-splits = ~4 ms, still under half
/// the 10 ms scheduler tick, so the ISR still cannot starve the scheduler.
const POLL_HALT_BUDGET_US: u32 = 1000;

/// One-shot diagnostic dump of the channel + core-config state on a stalled transfer (`wait_halt` calls it
/// on its bounded timeout); `DUMPED` gates it to the FIRST stall so the log is never flooded.
fn stall_dump(ch: u32) {
    if DUMPED.swap(true, Ordering::Relaxed) { return; }
    // Did the core CONSUME the pushed bytes (NPTxFSpcAvail back to full) or are they stuck in the FIFO?
    pl011_write(b"dwc2: STALL HCCHAR="); write_hex32(rd(hcchar_at(ch)));
    pl011_write(b" HCTSIZ="); write_hex32(rd(hctsiz_at(ch)));
    pl011_write(b" GNPTXSTS="); write_hex32(rd(0x02C));
    pl011_write(b" GINTSTS="); write_hex32(rd(GINTSTS));
    pl011_write(b" HPRT="); write_hex32(rd(HPRT));
    pl011_write(b" HAINT="); write_hex32(rd(HAINT));
    pl011_write(b" GRSTCTL="); write_hex32(rd(GRSTCTL));
    let f1 = rd(0x408); spin(300_000); let f2 = rd(0x408);
    pl011_write(b" HFNUM1="); write_hex32(f1);
    pl011_write(b" HFNUM2="); write_hex32(f2);
    // Second line: did our config writes actually stick? (GAHBCFG DMAEN/slave, force-host + PHY in
    // GUSBCFG, FSLSPClkSel in HCFG, the full-speed HFIR, the interrupt unmask.)
    pl011_write(b"\r\ndwc2: cfg GAHBCFG="); write_hex32(rd(GAHBCFG));
    pl011_write(b" GUSBCFG="); write_hex32(rd(GUSBCFG));
    pl011_write(b" HCFG="); write_hex32(rd(HCFG));
    pl011_write(b" HFIR="); write_hex32(rd(HFIR));
    pl011_write(b" GINTMSK="); write_hex32(rd(GINTMSK));
    pl011_write(b"\r\n");
}

/// Wait for the channel to halt (complete or error), bounded by REAL TIME so a wedged controller reports
/// rather than hanging.
///
/// **The bound MUST be time, not a spin count, and it MUST be short.** This runs in two contexts that
/// both hold interrupts off: the boot path, and - critically - the runtime net/bulk path, which the SVC
/// entry enters with IRQs MASKED (see the DMA soundness invariant below: the net path must not block, so
/// it keeps IRQs masked start to finish). A spin count of 4,000,000 MMIO reads on the slow peripheral bus
/// is tens to hundreds of milliseconds with the timer ISR unable to fire - so a single stalled ethernet
/// frame read froze the whole machine: no keyboard poll (the ISR never ran), no preemption (the shell
/// took 49 s to reach ready). HW-observed on the Pi 2 once the board-MAC fix brought the NIC up and
/// net-stack began polling frames in earnest.
///
/// A healthy USB transaction halts in microseconds, so `HALT_BUDGET_US` is enormously generous for the
/// working path while capping the pathological one at a fraction of a scheduler tick.
fn wait_halt(ch: u32) -> u32 {
    let start = super::timer::systimer_us();
    loop {
        let ci = rd(hcint_at(ch));
        if ci & HCINT_CHHLTD != 0 { return ci; }
        if super::timer::systimer_us().wrapping_sub(start) > HALT_BUDGET_US {
            stall_dump(ch);
            return ci | HCINT_CHHLTD; // treat as halted-without-complete -> failure
        }
    }
}
/// Per-transaction halt budget. 2 ms is ~1000x a healthy transaction and still well under the 10 ms
/// scheduler tick, so even a fully wedged device cannot starve the timer ISR or the scheduler.
const HALT_BUDGET_US: u32 = 2000;

/// DMA scratch buffer. Static so it lives in identity-mapped RAM (VA == PA); the DMA engine reads/writes
/// it via the bus alias (`chan_program`). 64-byte aligned, and `setup` is padded to a full 64 bytes so
/// `data` starts on its own cache line (the clean/invalidate bracket never straddles setup + data). The
/// `data` region holds a full disk block (512) or ethernet frame (~1514) for bulk transfers.
///
/// SOUNDNESS INVARIANT (the `&mut *addr_of_mut!(DMA)` in `ctrl_xfer`/`bulk_xfer` must never overlap):
/// every DMA access is **core-0 only** and the accessors are **mutually exclusive in time**. The
/// keyboard `poll()` uses its OWN buffer (`KBD_DMA`), so it no longer aliases `DMA` at all - the
/// prerequisite for driving it from an interrupt (where it *would* overlap a `DMA` transfer in time).
/// This rests on two properties that any future edit MUST preserve:
///   1. `poll()` runs only from the core-0 timer tick, and `net_frame_tx/rx` only from a syscall guarded
///      by `on_core0()` - so no cross-core and no off-core access.
///   2. `net_frame_tx/rx` (and everything they call) **never block** - no `yield`/`recv`/`enable_interrupts`.
///      The SVC entry masks IRQs, so a non-blocking syscall keeps them masked start-to-finish; the timer
///      cannot fire and `poll()` cannot interleave. Adding a blocking call to the net path would re-enable
///      IRQs mid-transfer and let `poll()` alias this buffer - a data race. Keep the net path synchronous.
#[repr(C, align(64))]
struct DmaBuf { setup: [u8; 64], data: [u8; 2048] }
static mut DMA: DmaBuf = DmaBuf { setup: [0; 64], data: [0; 2048] };

/// The keyboard poll's OWN DMA buffer, separate from `DMA` above. An 8-byte boot report, but a full
/// 64-byte cache line, `align(64)`: once the keyboard completes via **interrupt** (Stage 1c) its ISR can
/// touch this buffer while a storage/net transfer is mid-flight in `DMA` - so the two must not share a
/// cache line, or one transfer's `flush_dcache` clobbers the other's. A dedicated aligned line makes the
/// keyboard's cache maintenance (invalidate before the device writes, invalidate after, over exactly
/// these 8 bytes) touch nothing else. Today (Stage 1a) the keyboard is still polled from the tick and
/// still never overlaps `DMA` in time; separating the buffer is the prerequisite the interrupt path needs.
#[repr(C, align(64))]
struct KbdDma { report: [u8; 64] }
static mut KBD_DMA: KbdDma = KbdDma { report: [0; 64] };

/// Storage's OWN DMA buffer (CH_BULK), separate from the shared `DMA` that networking (CH_NET) and
/// enumeration (control) use. `bulk_xfer` used one buffer for both storage and net; their safety rested
/// on never overlapping in time. Driving storage from an **interrupt** (stage 2) breaks that: a storage
/// transfer parks with IRQs enabled, so the nic-driver task can run a net transfer through the shared
/// buffer while storage's DMA is mid-flight. A dedicated buffer removes that aliasing. 512 bytes holds
/// the largest storage transfer (one block); CBW/CSW/SENSE/CAPACITY are all smaller. `align(64)` so its
/// cache-maintenance bracket (SEC-28) touches no neighbour. Today (1b) storage is still spin-polled and
/// still never overlaps `DMA`; separating the buffer is the prerequisite the async path needs.
#[repr(C, align(64))]
struct MscDma { data: [u8; 512] }
static mut MSC_DMA: MscDma = MscDma { data: [0; 512] };

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
/// the data itself. Retries on NAK / transaction-error up to `tries` times; STALL or exhausted retries is
/// a hard failure. `tries == 1` (no backoff) is the fast path for polling an endpoint that legitimately
/// NAKs when idle (a bulk IN with no frame queued), so an empty poll returns immediately.
fn chan_dma(ch: u32, dir_in: bool, pid: u32, buf_phys: u32, len: u32, ep: u32, ep_type: u32, can_block: bool) -> bool {
    let hcsplt = hcsplt_for_current();
    if hcsplt != 0 {
        // SPLIT path, ONE low/full-speed packet per split transaction. The DWC2 does not auto-continue a
        // multi-packet split in buffer-DMA mode - it halts XferCompl after the FIRST packet - so software
        // must sequence each mps-sized packet itself, advancing the buffer and toggling the data PID.
        // HW-proven (Pi 2 / LAN9514): an 18-byte device descriptor read whole came back as 8 correct bytes
        // + 10 stale, because only packet 1 was ever retrieved. A single-packet transfer (the 8-byte SETUP,
        // a boot-report IN) is just one iteration; a zero-length STATUS is one iteration with chunk 0.
        let mps = MPS0.load(Ordering::Relaxed).max(1) as u32;
        let mut off = 0u32;
        let mut cur_pid = pid;
        loop {
            let chunk = (len - off).min(mps);
            let mut ok = false;
            let mut xact_errs = 0u32;
            let started = super::timer::systimer_us();
            loop {
                let ci = split_txn(ch, dir_in, cur_pid, chunk, buf_phys + off, ep, ep_type, hcsplt, false);
                if ci & HCINT_XFERCOMPL != 0 { ok = true; break; }
                if ci & HCINT_STALL != 0 { LAST_FAIL.store(FAIL_STALL, Ordering::Relaxed); return false; }
                // A NAK or NYET is FLOW CONTROL, not an error: the device (or the hub's TT) is telling
                // us to ask again. It must not spend the error budget - that is the whole point.
                if ci & (HCINT_XACTERR) != 0 {
                    xact_errs += 1;
                    if xact_errs >= XACT_ERR_MAX { LAST_FAIL.store(FAIL_XACT, Ordering::Relaxed); return false; }
                }
                if super::timer::systimer_us().wrapping_sub(started) > nak_budget_us() {
                    LAST_FAIL.store(FAIL_NAK_TIMEOUT, Ordering::Relaxed); return false;
                }
                super::uart_rx_drain_now();   // keep serial input alive during a long IRQs-masked wait
                spin(5_000);
            }
            if !ok { return false; }
            off += chunk;
            if off >= len || chunk < mps {                  // whole transfer done (len==0 here), or a short packet ended it
                // Next data PID = flip of the last packet's PID; bulk_xfer reads this to keep the toggle in sync.
                NEXT_BULK_PID_DATA1.store(cur_pid == PID_DATA0, Ordering::Relaxed);
                return true;
            }
            cur_pid = if cur_pid == PID_DATA1 { PID_DATA0 } else { PID_DATA1 }; // control/bulk data toggle
        }
    }
    // INTERRUPT-DRIVEN direct path (async block I/O, stage 2): arm the channel, park the calling task,
    // and let the channel-halt ISR re-arm on NAK / wake on completion - so the core is free (the keyboard
    // tick, other tasks) instead of spinning IRQs-masked. Only the runtime storage path passes
    // can_block=true; boot enumeration (no task to park) and, until stage 2b, writes keep the spin path.
    if can_block {
        return chan_dma_async(ch, dir_in, pid, len, buf_phys, ep, ep_type);
    }
    // DIRECT (high-speed) path: the core handles multi-packet framing + the data toggle itself.
    let mut xact_errs = 0u32;
    let started = super::timer::systimer_us();
    loop {
        chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, 0);
        let ci = wait_halt(ch);
        if ci & HCINT_XFERCOMPL != 0 {
            // The core advances HCTSIZ.PID [30:29] to the next data PID (parity- and ZLP-correct); bulk_xfer reads it.
            NEXT_BULK_PID_DATA1.store((rd(hctsiz_at(ch)) >> 29) & 0x3 == PID_DATA1, Ordering::Relaxed);
            return true;
        }
        if ci & HCINT_STALL != 0 { LAST_FAIL.store(FAIL_STALL, Ordering::Relaxed); return false; }
        // NAK/NYET = "busy, ask again" and costs nothing. Only a real TRANSACTION error counts, and
        // three of them fail the transfer - the same threshold Linux applies in `dwc2_release_channel`.
        if ci & HCINT_XACTERR != 0 {
            xact_errs += 1;
            if xact_errs >= XACT_ERR_MAX { LAST_FAIL.store(FAIL_XACT, Ordering::Relaxed); return false; }
        }
        if super::timer::systimer_us().wrapping_sub(started) > nak_budget_us() {
            LAST_FAIL.store(FAIL_NAK_TIMEOUT, Ordering::Relaxed); return false;
        }
        super::uart_rx_drain_now();   // keep serial input alive during a long IRQs-masked wait
        spin(5_000);
    }
}

// --- Interrupt-driven storage: async block I/O on CH_BULK (stage 2) --------------------------------
// The block-driver is single-threaded, so at most ONE storage transfer is ever in flight - a single
// waiter holds its parked task and the registers to replay on a NAK re-arm. Written IRQ-masked at arm
// (the storage syscall) and read/updated only by the USB channel-halt ISR and the tick watchdog, which
// are also IRQ-masked on core 0 - so it is core-0 exclusive in fact, like KBD_STATE/DMA.
struct AsyncBulk {
    active: bool,
    slot: u32,                 // parked task's scheduler slot (u32::MAX = none)
    // Captured channel registers, REPLAYED verbatim to re-arm on a NAK. Replaying (not re-calling
    // chan_program) is what makes the re-arm immune to the concurrent keyboard poll, which changes the
    // shared MPS0/DEV_ADDR/LOW_SPEED that chan_program would otherwise read.
    hcchar: u32, hctsiz: u32, hcdma: u32, hcsplt: u32,
    started_us: u32,           // NAK-budget origin
    budget_us: u32,            // the budget captured at arm (same value the spin path uses)
    xact_errs: u32,
}
static mut ASYNC_BULK: AsyncBulk = AsyncBulk {
    active: false, slot: u32::MAX, hcchar: 0, hctsiz: 0, hcdma: 0, hcsplt: 0,
    started_us: 0, budget_us: 0, xact_errs: 0,
};
// Wake result carried through park_current(): a real HCINT (its bits are all in the low 8) for a halt the
// ISR saw, or this sentinel when the budget expired with no terminal halt. Negative so it can never be
// mistaken for an HCINT value.
const ASYNC_TIMEOUT: i64 = -0x0100_0000;
// The tick watchdog fires this far PAST the budget, so the ISR's own budget check (which runs the instant
// a NAK halts the channel) handles the normal slow-stick timeout; the watchdog only catches a device that
// never halts at all - no channel interrupt ever arrives - which the ISR cannot see.
const ASYNC_WATCHDOG_MARGIN_US: u32 = 200_000;

/// Arm a direct (high-speed) transfer on CH_BULK and PARK the calling task until the channel-halt ISR
/// wakes it. Returns the same verdicts the spin path returns (true on XferCompl; false on STALL /
/// repeated XactErr / NAK-budget timeout), so callers are unchanged. Preconditions: IRQs masked (syscall
/// context), hcsplt == 0 (direct - storage is high-speed). Reached only with can_block = true.
fn chan_dma_async(ch: u32, dir_in: bool, pid: u32, len: u32, buf_phys: u32, ep: u32, ep_type: u32) -> bool {
    let slot = crate::task::scheduler::current_task_slot();
    // No task to park (boot/idle context, or a mis-set flag): fall back to one bounded synchronous
    // attempt so the degradation is correct-but-slow, never a hang.
    if slot >= crate::task::scheduler::MAX_TASKS {
        chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, 0);
        let ci = wait_halt(ch);
        if ci & HCINT_XFERCOMPL != 0 {
            NEXT_BULK_PID_DATA1.store((rd(hctsiz_at(ch)) >> 29) & 0x3 == PID_DATA1, Ordering::Relaxed);
            return true;
        }
        LAST_FAIL.store(if ci & HCINT_STALL != 0 { FAIL_STALL } else { FAIL_XACT }, Ordering::Relaxed);
        return false;
    }
    // Gate CH_BULK's halt to the CPU: only CHHLTD (a terminal halt) reaches HAINT, and only CH_BULK
    // reaches Hchint. Toggled per transfer so the SPIN path (boot enumeration, writes) is never exposed
    // to the ISR clearing HCINT under its poll.
    wr(hcintmsk_at(CH_BULK), HCINT_CHHLTD);
    wr(HAINTMSK, rd(HAINTMSK) | (1 << CH_BULK));
    chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, 0);
    // SAFETY: core-0 exclusive, IRQ-masked; the ISR/watchdog cannot run until park_current() switches
    // away (which enables IRQs on the next task), so the arm + capture completes before any reader.
    let result = unsafe {
        let x = &mut *core::ptr::addr_of_mut!(ASYNC_BULK);
        x.hcchar = rd(hcchar_at(ch));
        x.hctsiz = rd(hctsiz_at(ch));
        x.hcdma  = rd(hcdma_at(ch));
        x.hcsplt = rd(hcsplt_at(ch));
        x.started_us = super::timer::systimer_us();
        x.budget_us  = nak_budget_us();
        x.xact_errs  = 0;
        x.slot   = slot as u32;
        x.active = true;
        // Park: CAS Running->Blocked, switch away, and return the value wake_by_slot delivered on resume.
        crate::task::scheduler::park_current()
    };
    // Resumed (IRQ-masked). Ungate CH_BULK so the next spin-path transfer is not disturbed by the ISR.
    wr(HAINTMSK, rd(HAINTMSK) & !(1 << CH_BULK));
    if result == ASYNC_TIMEOUT {
        LAST_FAIL.store(FAIL_NAK_TIMEOUT, Ordering::Relaxed);
        return false;
    }
    let ci = result as u32;
    if ci & HCINT_XFERCOMPL != 0 {
        NEXT_BULK_PID_DATA1.store((rd(hctsiz_at(ch)) >> 29) & 0x3 == PID_DATA1, Ordering::Relaxed);
        return true;
    }
    LAST_FAIL.store(if ci & HCINT_STALL != 0 { FAIL_STALL } else { FAIL_XACT }, Ordering::Relaxed);
    false
}

/// Re-arm CH_BULK by REPLAYING the captured registers - immune to the concurrent keyboard poll, which
/// changes the shared MPS0/DEV_ADDR/LOW_SPEED that a fresh chan_program would read.
fn async_bulk_rearm(x: &AsyncBulk) {
    let ch = CH_BULK;
    wr(hcint_at(ch), 0xFFFF_FFFF);            // clear the halt we just handled
    wr(hctsiz_at(ch), x.hctsiz);
    wr(hcdma_at(ch), x.hcdma);
    wr(hcsplt_at(ch), x.hcsplt);
    wr(hcchar_at(ch), x.hcchar | (1 << 31));  // re-enable the channel
}

/// Deliver a wake to the parked storage task and disarm the waiter.
fn async_bulk_wake(x: &mut AsyncBulk, result: i64) {
    let slot = x.slot;
    x.active = false;
    x.slot = u32::MAX;
    if slot != u32::MAX {
        crate::task::scheduler::wake_by_slot(slot as usize, result);
    }
}

/// CH_BULK channel-halt handler, called from `on_usb_irq` when Hchint names CH_BULK. This IS the body of
/// the spin loop, triggered by the halt interrupt instead of by polling: XferCompl -> wake ok; STALL /
/// repeated XactErr -> wake fail; NAK/NYET -> re-arm unless the budget is spent (then wake timeout).
fn async_bulk_isr() {
    let hcint = rd(hcint_at(CH_BULK));
    wr(hcint_at(CH_BULK), hcint);             // W1C: deassert this channel's HAINT/Hchint
    // SAFETY: core-0 exclusive, IRQ-masked (interrupt context).
    unsafe {
        let x = &mut *core::ptr::addr_of_mut!(ASYNC_BULK);
        if !x.active { return; }              // no parked transfer (should not happen while CH_BULK gated)
        if hcint & HCINT_XFERCOMPL != 0 {
            async_bulk_wake(x, hcint as i64);
        } else if hcint & HCINT_STALL != 0 {
            async_bulk_wake(x, hcint as i64);
        } else if hcint & HCINT_XACTERR != 0 {
            x.xact_errs += 1;
            if x.xact_errs >= XACT_ERR_MAX { async_bulk_wake(x, hcint as i64); }
            else { async_bulk_rearm(x); }
        } else {
            // NAK / NYET / halted-without-data: flow control - retry until the budget is spent.
            if super::timer::systimer_us().wrapping_sub(x.started_us) > x.budget_us {
                async_bulk_wake(x, ASYNC_TIMEOUT);
            } else {
                async_bulk_rearm(x);
            }
        }
    }
}

/// Tick watchdog (core-0 timer): force-wake a parked storage transfer whose device NEVER halted, so no
/// channel interrupt ever arrived for the ISR to time out. Replaces `wait_halt`'s internal timeout for
/// the async path - a wedged device stays bounded rather than parking the task forever.
pub fn async_bulk_watchdog() {
    // SAFETY: core-0 exclusive, IRQ-masked (timer ISR).
    unsafe {
        let x = &mut *core::ptr::addr_of_mut!(ASYNC_BULK);
        if x.active
            && super::timer::systimer_us().wrapping_sub(x.started_us)
                > x.budget_us.saturating_add(ASYNC_WATCHDOG_MARGIN_US)
        {
            // Disable the stuck channel so the next transfer starts clean, ungate it, then wake timeout.
            let ch = CH_BULK;
            if rd(hcchar_at(ch)) & (1 << 31) != 0 {
                wr(hcchar_at(ch), (rd(hcchar_at(ch)) & !(1 << 31)) | (1 << 30)); // ChDis
            }
            wr(HAINTMSK, rd(HAINTMSK) & !(1 << CH_BULK));
            async_bulk_wake(x, ASYNC_TIMEOUT);
        }
    }
}

/// One SPLIT transaction to a low/full-speed device behind the high-speed LAN9514 hub: a **Start-Split**
/// (the hub captures the token and runs it at the device's low/full speed), then **Complete-Splits** polled
/// until the hub returns the result (NYET/NAK = "not ready yet", retry). Returns the final HCINT. `bounded`
/// picks the tight ISR wait (the keyboard poll) over the generous one-shot enumeration wait. HCINT bits used:
/// XferCompl0, STALL3, NAK4, ACK5, NYET6.
fn split_txn(ch: u32, dir_in: bool, pid: u32, len: u32, buf_phys: u32, ep: u32, ep_type: u32, hcsplt: u32, bounded: bool) -> u32 {
    // The keyboard interrupt IN (bounded = the ISR poll) is a PERIODIC split - frame-scheduled with an
    // ACK-required start-split (Circle). The enumeration control/bulk splits (not bounded) use the
    // microframe-sweep loop below, which brute-forces a landing microframe for a non-periodic split.
    if bounded {
        return split_txn_periodic(ch, dir_in, pid, len, buf_phys, ep, ep_type, hcsplt);
    }
    let mut last = 0u32;
    // A split transaction that transaction-errors is retried whole (USB 2.0 11.17.5 / 11.20). The hub's
    // transaction translator (TT) legitimately XactErr/NAKs while busy; the host re-issues the start-split.
    // Non-periodic (control/bulk) split - the ISR keyboard poll (`bounded`) took the periodic path above.
    // Brute-force a landing microframe by SWEEPING 0..7 across retries (waiting on the HFNUM truth), since a
    // non-periodic split has no fixed schedule; the patient `wait_halt` is fine off the ISR (enumeration).
    let ss_tries = 24u32;
    for attempt in 0..ss_tries {
        wait_for_uframe((attempt % 8) as u32);
        // STATE 1 - issue the Start-Split (CompleteSplit = 0); capture the microframe it goes out in.
        let hf0 = rd(HFNUM);
        chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, hcsplt);
        let ss = wait_halt(ch);
        trace_split(PH_SSPLIT, hf0, rd(HFNUM), ss);
        last = ss;
        if ss & (1 << 3) != 0 { break; }                        // STALL - real failure
        if ss & HCINT_XFERCOMPL != 0 { return ss; }             // (rare) already complete
        if ss & (1 << 5) == 0 { continue; }                     // no ACK -> retry (the microframe sweep above spaces it)
        // STATE 2 - poll the Complete-Split (CompleteSplit = 1) for the low/full-speed result.
        let mut nyet = 0u32;
        loop {
            let hf1 = rd(HFNUM);
            chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, hcsplt | (1 << 16));
            let cs = wait_halt(ch);
            trace_split(PH_CSPLIT, hf1, rd(HFNUM), cs);
            last = cs;
            if cs & HCINT_XFERCOMPL != 0 { return cs; }         // the transfer completed
            if cs & (1 << 3) != 0 { return cs; }                // STALL - real failure
            if cs & (1 << 6) != 0 {                             // NYET: TT not finished, keep polling the CSPLIT
                nyet += 1;
                if nyet > 500 { break; }                        // bounded; fall out to a fresh start-split
                super::timer::delay_us(125);                    // ONE microframe on the real 1 MHz clock (Commandment VIII)
                continue;
            }
            break;                                              // NAK (4) / XactErr (7): re-issue the start-split
        }
    }
    // Enumeration only, one-shot: dump HFIR + the captured microframe trace (logging INLINE would take far
    // longer than a 125 us microframe and destroy the timing we are measuring, so we captured it silently).
    if !bounded && !SPLIT_DUMPED.swap(true, Ordering::Relaxed) {
        pl011_write(b"dwc2: split fail last_hcint="); write_hex32(last);
        pl011_write(b" HCSPLT="); write_hex32(hcsplt);
        pl011_write(b" HCCHAR="); write_hex32(rd(hcchar_at(ch)));
        pl011_write(b" GINTMSK="); write_hex32(rd(GINTMSK));
        pl011_write(b" HFIR="); write_hex32(rd(HFIR));
        pl011_write(b"\r\ndwc2: split trace [phase issue.uf -> halt.uf hcint]:\r\n");
        let n = SPLIT_TRACE_N.load(Ordering::Relaxed).min(SPLIT_TRACE_MAX);
        for i in 0..n {
            // SAFETY: read-only, single-threaded, i < n <= SPLIT_TRACE_MAX (array bound).
            let (ph, hi, hh, ci, nptx, gints) = unsafe { (*core::ptr::addr_of!(SPLIT_TRACE))[i as usize] };
            pl011_write(b"  "); pl011_write(&[ph]); pl011_write(b" ");
            write_hfnum(hi); pl011_write(b" -> "); write_hfnum(hh);
            pl011_write(b" hcint="); write_hex32(ci);
            pl011_write(b" nptx="); write_hex32(nptx);
            pl011_write(b" gints="); write_hex32(gints); pl011_write(b"\r\n");
        }
    }
    last
}

static SPLIT_DUMPED: AtomicBool = AtomicBool::new(false);

// --- split microframe trace ------------------------------------------------------------------------
// Capture (phase, HFNUM-at-issue, HFNUM-at-halt, HCINT) per SSPLIT/CSPLIT into a fixed buffer, dumped
// ONCE after the first failing enumeration split. Never log inline in the split path: a pl011_write is
// far slower than a 125 us microframe and would perturb the very scheduling we are measuring.
const PH_SSPLIT: u8 = b'S';
const PH_CSPLIT: u8 = b'C';
const SPLIT_TRACE_MAX: u32 = 40;
// (phase, HFNUM-issue, HFNUM-halt, HCINT, GNPTXSTS, GINTSTS) per SSPLIT/CSPLIT.
static mut SPLIT_TRACE: [(u8, u32, u32, u32, u32, u32); 40] = [(0, 0, 0, 0, 0, 0); 40];
static SPLIT_TRACE_N: AtomicU32 = AtomicU32::new(0);

fn trace_split(phase: u8, hf_issue: u32, hf_halt: u32, hcint: u32) {
    if SPLIT_DUMPED.load(Ordering::Relaxed) { return; }         // stop capturing once the one-shot dump ran
    let n = SPLIT_TRACE_N.load(Ordering::Relaxed);
    if n < SPLIT_TRACE_MAX {
        // Also snapshot GNPTXSTS (did the SSPLIT's 8 SETUP bytes leave the NP TX FIFO = the core actually
        // TRANSMITTED the split, vs stuck = a core-internal reject) and GINTSTS (global state), read now at
        // the halt. This is THE discriminator: on-the-wire XactErr vs internal reject.
        // SAFETY: single-threaded (core-0 only) capture into a fixed array, index n < SPLIT_TRACE_MAX.
        unsafe { (*core::ptr::addr_of_mut!(SPLIT_TRACE))[n as usize] = (phase, hf_issue, hf_halt, hcint, rd(GNPTXSTS), rd(GINTSTS)); }
        SPLIT_TRACE_N.store(n + 1, Ordering::Relaxed);
    }
}

/// Decode + print HFNUM as `frame.uframe`: FrNum is bits [13:0]; the low 3 bits are the microframe (0..7)
/// in high-speed mode, the rest the frame. This is the scheduling axis a split transaction lives on.
fn write_hfnum(hf: u32) {
    let frnum = hf & 0x3FFF;
    write_hex32(frnum >> 3); pl011_write(b"."); write_hex32(frnum & 0x7);
}

/// Spin until the host frame counter reaches microframe `target` (0..7) - waiting on the HFNUM scheduling
/// TRUTH, not a guessed delay (Commandment VIII). Bounded backstop so a stuck counter cannot hang the boot.
fn wait_for_uframe(target: u32) {
    // Bound by REAL time (1 MHz System Timer): reaching a target microframe takes at most one ~1 ms frame,
    // so 1.5 ms always reaches it while keeping the in-ISR periodic-split poll bounded. (A spin-count bound
    // is MMIO-latency-dependent and was the cause of the scheduler-starving hang; use real time here too.)
    let start = super::timer::systimer_us();
    while (rd(HFNUM) & 0x7) != (target & 0x7) {
        if super::timer::systimer_us().wrapping_sub(start) > 1500 { break; }
    }
}

/// A PERIODIC (interrupt) IN split, frame-scheduled per Circle's CDWHCIFrameSchedulerPeriodic (proven on
/// this exact Pi 2 + LAN9514 hardware). The keyboard poll's start-split was being NYET'd because it fired
/// at an arbitrary microframe; a periodic split MUST be scheduled:
///   1. START-SPLIT in microframe (current+1)&7, SKIPPING uframe 6 (too few left before the frame boundary
///      for the complete-split at +2). ODDFRM must match that microframe's parity - `chan_program` derives
///      it from HFNUM, which now reads the scheduled microframe because we enable the channel only after
///      `wait_for_uframe` reaches it. The start-split MUST be ACK'd (a NYET/NAK means the TT refused it).
///   2. COMPLETE-SPLIT at +2 (COMPSPLT set, SplitEnable kept), retrying NYET in the following microframes
///      (Circle: 3 tries), ODDFRM re-set each time.
/// ONE attempt per call: the keyboard poll runs every tick, so a NYET/NAK on the start-split simply
/// reschedules a fresh start-split next tick. Every wait is bounded (time-bounded `wait_for_uframe` +
/// `poll_wait_halt`), so the whole poll is ~1-3 ms, well under the 10 ms scheduler tick.
fn split_txn_periodic(ch: u32, dir_in: bool, pid: u32, len: u32, buf_phys: u32, ep: u32, ep_type: u32, hcsplt: u32) -> u32 {
    let mut ssf = (rd(HFNUM).wrapping_add(1)) & 0x7;  // start-split microframe = current + 1
    if ssf == 6 { ssf = 7; }                          // skip uframe 6 (Circle WaitForFrame)
    wait_for_uframe(ssf);
    let hf0 = rd(HFNUM);
    chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, hcsplt);   // CompleteSplit = 0
    let ss = poll_wait_halt(ch);
    trace_split(PH_SSPLIT, hf0, rd(HFNUM), ss);
    if ss & (1 << 3) != 0 { return ss; }                             // STALL
    if ss & HCINT_XFERCOMPL != 0 { return ss; }                      // (rare) already complete
    if ss & (1 << 5) == 0 { return ss; }                            // no ACK on the start-split -> reschedule next tick
    // Complete-split at ssf+2, retry NYET in the following microframes (COMPSPLT set, ODDFRM re-set each).
    let mut csf = (ssf + 2) & 0x7;
    let mut last = ss;
    for _ in 0..3 {
        wait_for_uframe(csf);
        let hf1 = rd(HFNUM);
        chan_program(ch, dir_in, pid, len, buf_phys, ep, ep_type, hcsplt | (1 << 16)); // CompleteSplit = 1
        let cs = poll_wait_halt(ch);
        trace_split(PH_CSPLIT, hf1, rd(HFNUM), cs);
        last = cs;
        if cs & HCINT_XFERCOMPL != 0 { return cs; }                 // data arrived - the report
        if cs & (1 << 3) != 0 { return cs; }                        // STALL
        if cs & (1 << 4) != 0 { return cs; }                        // NAK - no data this period (idle keyboard)
        if cs & (1 << 6) != 0 { csf = (csf + 1) & 0x7; continue; }  // NYET - retry the complete-split next microframe
        return cs;                                                   // XactErr / other - reschedule next tick
    }
    last
}

/// A single control-endpoint DMA transaction (ep 0, type control). Thin wrapper so ctrl_xfer reads clean.
fn ctrl_dma(ch: u32, dir_in: bool, pid: u32, buf_phys: u32, len: u32) -> bool {
    chan_dma(ch, dir_in, pid, buf_phys, len, 0, 0, false)   // control/enum: boot context, never parks
}

/// A full control transfer via DMA: SETUP -> (DATA) -> STATUS, through the `DMA` scratch buffer. `data_in`
/// / `dlen` describe the DATA stage; the STATUS stage runs in the opposite direction with zero length.
fn ctrl_xfer(setup: &[u8; 8], data: &mut [u8], data_in: bool, dlen: usize) -> bool {
    // Control traffic (enumeration, SET_ADDRESS, clear-halt) belongs to the bulk stream: it runs in
    // boot or syscall context, never from the timer ISR, so it shares CH_BULK with storage rather than
    // competing with the keyboard's periodic poll.
    let ch = CH_BULK;
    // SAFETY: DMA is a static touched only here on core 0; `addr_of` yields its identity-mapped physical
    // address. The buffer is filled + cache-flushed while no channel is running, so the DMA engine never
    // reads a half-written buffer.
    unsafe {
        let d = &mut *core::ptr::addr_of_mut!(DMA);
        let setup_phys = core::ptr::addr_of!(d.setup) as u32;
        let data_phys = core::ptr::addr_of!(d.data) as u32;

        d.setup[..8].copy_from_slice(setup);
        flush_dcache(setup_phys, 8);
        if !ctrl_dma(ch, false, PID_SETUP, setup_phys, 8) { pl011_write(b"dwc2: SETUP failed\r\n"); return false; }

        if dlen > 0 {
            if data_in {
                // Never let the device DMA past the scratch buffer (clamp the programmed length, not just
                // the copy-out). All current callers pass dlen <= ~160, but defend the buffer regardless.
                let want = dlen.min(d.data.len());
                flush_dcache(data_phys, want as u32); // invalidate the line before the device writes it
                if !ctrl_dma(ch, true, PID_DATA1, data_phys, want as u32) { pl011_write(b"dwc2: DATA failed\r\n"); return false; }
                flush_dcache(data_phys, want as u32); // invalidate after -> the CPU reads device-written bytes
                let n = want.min(data.len());
                data[..n].copy_from_slice(&d.data[..n]);
            } else {
                // Send only what fits in BOTH the scratch buffer and the source slice - so a future caller
                // with dlen > data.len() can neither panic the `&data[..n]` copy nor DMA past the buffer.
                let n = dlen.min(d.data.len()).min(data.len());
                d.data[..n].copy_from_slice(&data[..n]);
                flush_dcache(data_phys, n as u32);
                if !ctrl_dma(ch, false, PID_DATA1, data_phys, n as u32) { pl011_write(b"dwc2: DATA failed\r\n"); return false; }
            }
        }

        // STATUS: opposite direction, zero length, DATA1 (uses the setup buffer as a dummy DMA target).
        let ok = if data_in {
            ctrl_dma(ch, false, PID_DATA1, setup_phys, 0)
        } else {
            flush_dcache(data_phys, 4);
            ctrl_dma(ch, true, PID_DATA1, data_phys, 0)
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
    let mps = if buf[7] == 0 { 8u16 } else { buf[7] as u16 };
    MPS0.store(mps, Ordering::Relaxed);
    pl011_write(b"dwc2: desc8 mps0="); write_hex32(mps as u32); pl011_write(b"\r\n");

    // SET_ADDRESS(1).
    if !control_out(0x00, 0x05, 1, 0) {
        pl011_write(b"dwc2: SET_ADDRESS failed - USB unavailable\r\n"); return;
    }
    DEV_ADDR.store(1, Ordering::Relaxed);
    super::timer::delay_us(2000); // TDSETADDR: 2 ms SET_ADDRESS recovery on the real 1 MHz clock

    // GET_DESCRIPTOR(device, 18) at address 1 -> VID/PID/class.
    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 18) {
        pl011_write(b"dwc2: GET_DESC(18) failed - USB unavailable\r\n"); return;
    }
    let vid = (buf[8] as u32) | ((buf[9] as u32) << 8);
    let pid = (buf[10] as u32) | ((buf[11] as u32) << 8);
    let class = buf[4];
    let protocol = buf[6];             // hub bDeviceProtocol: 1 = single-TT, 2 = multi-TT
    pl011_write(b"dwc2: enumerated device VID:PID=");
    write_hex32((vid << 16) | pid);
    pl011_write(b" class="); write_hex32(class as u32);
    pl011_write(b" proto="); write_hex32(protocol as u32); pl011_write(b"\r\n");

    if class == 0x09 {
        enumerate_hub(protocol);       // keyboard is behind the hub (LAN9514 on real Pi 2, NEC hub in QEMU)
    } else if class == 0x00 || class == 0x03 {
        configure_keyboard();          // keyboard plugged straight into the root port
    }
}

/// Walk the hub at address 1: configure it, power every port, then for each connected port reset it and
/// enumerate the downstream device, stopping at the first keyboard. Every wait is bounded.
fn enumerate_hub(protocol: u8) {
    let ch = CH_BULK;
    let hub_mps = MPS0.load(Ordering::Relaxed);          // hub EP0 max-packet (set during root enumeration)
    if !control_out(0x00, 0x09, 1, 0) { pl011_write(b"dwc2: hub SET_CONFIG failed\r\n"); return; }
    // A MULTI-TT hub (bDeviceProtocol 2) needs SET_INTERFACE(alt 1) to activate its per-port transaction
    // translators before ANY split is accepted - a USBCORE hub-driver step the dwc2 register layer does
    // not perform (fresh-eyes lead #1 for the low-speed keyboard split XactErr). Harmless on a single-TT
    // hub (it STALLs alt 1; we log and continue). SET_INTERFACE = bmReqType 0x01, bReq 0x0B, wValue=alt.
    if protocol == 2 {
        if control_out(0x01, 0x0B, 1, 0) { pl011_write(b"dwc2: hub is multi-TT: SET_INTERFACE(1) ok\r\n"); }
        else { pl011_write(b"dwc2: hub is multi-TT: SET_INTERFACE(1) refused\r\n"); }
    }

    // Hub descriptor (class GET_DESCRIPTOR, type 0x29) -> bNbrPorts at byte 2.
    let mut hd = [0u8; 16];
    if !get_descriptor(0xA0, 0x29, 0x00, 0, &mut hd, 16) {
        pl011_write(b"dwc2: hub descriptor failed\r\n"); return;
    }
    let nports = hd[2];
    pl011_write(b"dwc2: hub ports="); write_hex32(nports as u32); pl011_write(b"\r\n");

    for port in 1..=nports { control_out(0x23, 0x03, PORT_POWER, port as u16); } // SET_FEATURE(PORT_POWER)
    // Power-on-to-power-good + device-connect settle. bPwrOn2PwrGood (hd[5], in 2 ms units) is how long the
    // hub says to wait after PORT_POWER before a port reads valid; wait that plus a generous margin so a
    // just-powered device - or the LAN9514's internal ethernet port - has time to show connected. The old
    // spin(1M) was ~tens of ms, too short (every port read disconnected). Accurate delay via the 1 MHz timer.
    super::timer::delay_us((hd[5] as u32).saturating_mul(2000).max(300_000));

    // Walk EVERY connected port, assigning each device a distinct USB address (2, 3, ...) and configuring
    // it (keyboard AND ethernet can coexist behind the one hub - the Pi 2's LAN9514 topology). The single
    // host channel is time-shared: each device's transfer path re-selects it (`select_device`).
    let mut next_addr = 2u8;
    for port in 1..=nports {
        // Re-select the hub's own control endpoint: a prior downstream enumeration left DEV_ADDR/MPS0
        // pointing at that device, so every hub request below would otherwise go to the wrong address.
        select_device(1, hub_mps, false);

        let st = hub_get_port_status(port);
        pl011_write(b"dwc2: hub port "); write_hex32(port as u32);
        pl011_write(b" status="); write_hex32(st); pl011_write(b"\r\n");
        if st & 1 == 0 { continue; }                                            // no device on this port
        control_out(0x23, 0x01, C_PORT_CONNECTION, port as u16);                // CLEAR_FEATURE(C_CONNECTION)
        control_out(0x23, 0x03, PORT_RESET, port as u16);                       // SET_FEATURE(PORT_RESET)
        // Wait on the hub's TRUTH that the reset finished - PORT_RESET (wPortStatus bit 4) clears when the
        // hub has driven the ~10-20 ms reset - not a nop-spin guess (Commandment VIII). Bounded on the REAL
        // 1 MHz clock so a dead port cannot hang the boot; then the USB-spec reset-recovery on the real clock.
        let mut st2 = hub_get_port_status(port);
        let mut waited_ms = 0u32;
        while st2 & (1 << 4) != 0 && waited_ms < 60 {                            // bit4 = PORT_RESET still asserted
            super::timer::delay_us(1_000);
            waited_ms += 1;
            st2 = hub_get_port_status(port);
        }
        control_out(0x23, 0x01, C_PORT_RESET, port as u16);                     // CLEAR_FEATURE(C_RESET)
        super::timer::delay_us(10_000);                                         // reset-recovery (real clock, not spin)
        let low = (st2 >> 9) & 1 == 1;                                          // wPortStatus low-speed bit
        // A device that is NOT high-speed (bit 10 clear) behind this high-speed hub is reachable ONLY via
        // SPLIT transactions through this hub port; a high-speed device (ethernet/wifi) is direct.
        let split_port = if (st2 >> 10) & 1 == 0 { port } else { 0 };
        pl011_write(b"dwc2: port "); write_hex32(port as u32);
        pl011_write(b" device status="); write_hex32(st2); pl011_write(b"\r\n");
        enumerate_downstream(low, next_addr, split_port);                       // configure whatever it is
        next_addr += 1;                                                         // consume the address regardless
        if next_addr > 120 { break; }                                          // bounded (address space guard)
    }
    if !KBD_READY.load(Ordering::Relaxed) && !NET_READY.load(Ordering::Relaxed) {
        pl011_write(b"dwc2: no keyboard or network device found behind hub\r\n");
    }
    // A one-shot mass-storage probe during this walk may have advanced the shared bulk toggle; reset it so
    // the net device's first ongoing frame op starts from DATA0 (its config already set it, but a later
    // storage probe on another port could have moved it).
    if NET_READY.load(Ordering::Relaxed) {
        BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
        BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
    }
}

/// A freshly-reset downstream device answers at address 0. Learn its EP0 max-packet, move it to `addr`,
/// then dispatch by function: a boot keyboard (HID), the ethernet (CDC-ECM), or a mass-storage device.
/// Returns true if it was one we brought up.
fn enumerate_downstream(low: bool, addr: u8, split_port: u8) -> bool {
    select_device(0, 8, low);
    // A low/full-speed device behind the high-speed hub needs SPLIT for EVERY transfer (select_device
    // above cleared it); a high-speed device gets split_port = 0 = direct. This persists through the
    // SET_ADDRESS + descriptor reads + configure_* below (none of which re-select the device).
    SPLIT_PORT.store(split_port, Ordering::Relaxed);
    let mut buf = [0u8; 64];

    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 8) {
        pl011_write(b"dwc2: downstream desc8 failed\r\n"); return false;
    }
    MPS0.store(if buf[7] == 0 { 8 } else { buf[7] as u16 }, Ordering::Relaxed);

    if !control_out(0x00, 0x05, addr as u16, 0) { pl011_write(b"dwc2: downstream SET_ADDRESS failed\r\n"); return false; }
    DEV_ADDR.store(addr, Ordering::Relaxed);
    super::timer::delay_us(2000); // TDSETADDR: 2 ms SET_ADDRESS recovery on the real 1 MHz clock

    if !get_descriptor(0x80, 0x01, 0x00, 0, &mut buf, 18) {
        pl011_write(b"dwc2: downstream desc18 failed\r\n"); return false;
    }
    let vid = (buf[8] as u32) | ((buf[9] as u32) << 8);
    let pid = (buf[10] as u32) | ((buf[11] as u32) << 8);
    pl011_write(b"dwc2: downstream VID:PID="); write_hex32((vid << 16) | pid);
    pl011_write(b" class="); write_hex32(buf[4] as u32); pl011_write(b"\r\n");
    // DIAGNOSTIC: raw 18-byte device descriptor. A low-speed device (mps 8) returns it over 3 IN packets
    // via split; if bytes 8+ (VID/PID/class detail) are stale or duplicated while bytes 0-7 are right,
    // the multi-packet split IN is mis-toggling. bLength(00)=0x12, bDescType(01)=0x01 for a real one.
    pl011_write(b"dwc2: desc18 raw=");
    for i in 0..18usize {
        let b = buf[i];
        let hi = b >> 4; let lo = b & 0xF;
        pl011_write(&[if hi < 10 { b'0' + hi } else { b'a' + hi - 10 },
                      if lo < 10 { b'0' + lo } else { b'a' + lo - 10 }, b' ']);
    }
    pl011_write(b"\r\n");

    // The Pi 2's onboard LAN9514 is a vendor-specific smsc95xx (class 0xFF, VID 0x0424 SMSC). Bring it up
    // as the network device (HW-blind - QEMU never takes this branch).
    if buf[4] == 0xFF && vid == 0x0424 && configure_smsc95xx() { return true; }

    // A CDC device (class 0x02 at the device level) is a USB-Ethernet gadget: QEMU's usb-net, and real
    // CDC-ECM dongles.
    if buf[4] == 0x02 && configure_cdc_ecm(buf[17]) { return true; }

    // Both HID and mass storage define their class at the interface level, so each probe reads the
    // config descriptor itself. A boot keyboard is the goal; mass storage exercises the bulk path.
    if configure_keyboard() { return true; }
    // Record the hub port BEFORE probing: a stick behind the hub needs split transfers on every later
    // block I/O, and `SPLIT_PORT` is re-selected per transfer (the channel is shared with the keyboard).
    MSC_HUB_PORT.store(split_port, Ordering::Relaxed);
    if probe_mass_storage() { return true; }
    false
}

// --- CDC-ECM USB-Ethernet: raw ethernet frames over the bulk endpoints, no per-packet framing ---
static NET_READY:  AtomicBool = AtomicBool::new(false);
static NET_ADDR:   AtomicU8   = AtomicU8::new(0);   // the net device's assigned USB address
static NET_LOW:    AtomicBool = AtomicBool::new(false); // whether it is a low-speed device (it is not)
static NET_EP_IN:  AtomicU8   = AtomicU8::new(0);   // bulk IN endpoint (device -> host frames)
static NET_EP_OUT: AtomicU8   = AtomicU8::new(0);   // bulk OUT endpoint (host -> device frames)
static mut NET_MAC: [u8; 6] = [0; 6];               // our station MAC (the future net-stack bridge needs it)
// How the net device frames ethernet on its bulk endpoints. CDC-ECM carries raw frames; smsc95xx (the real
// Pi 2 LAN9514) prepends an 8-byte TX command / 4-byte RX status word, so tx/rx branch on this.
const NET_KIND_CDC:   u8 = 1;
const NET_KIND_SMSC:  u8 = 2;
static NET_KIND: AtomicU8 = AtomicU8::new(0);

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
        let mut bulk_mps = 64u16;
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
                let raw = (cfg[i + 4] as u16) | ((cfg[i + 5] as u16) << 8);
                bulk_mps = match raw & 0x07FF { 0 => 64, v => v }; // [10:0] = size; [12:11] = HS mult
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
        // SET_ETHERNET_PACKET_FILTER (CDC class, req 0x43) on the control interface. 0x0E =
        // DIRECTED(0x04) | BROADCAST(0x08) | ALL_MULTICAST(0x02) - a superset of Linux's DIRECTED|BROADCAST.
        control_out(0x21, 0x43, 0x000E, ctrl_iface as u16);

        BULK_MPS.store(bulk_mps, Ordering::Relaxed);
        BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
        BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
        NET_ADDR.store(DEV_ADDR.load(Ordering::Relaxed), Ordering::Relaxed);
        NET_LOW.store(LOW_SPEED.load(Ordering::Relaxed), Ordering::Relaxed);
        NET_EP_IN.store(ep_in, Ordering::Relaxed);
        NET_EP_OUT.store(ep_out, Ordering::Relaxed);
        NET_KIND.store(NET_KIND_CDC, Ordering::Relaxed);
        // SAFETY: NET_MAC is written only here, during core-0 enumeration.
        unsafe { (*core::ptr::addr_of_mut!(NET_MAC)).copy_from_slice(&mac); }
        NET_READY.store(true, Ordering::Release);

        pl011_write(b"dwc2: CDC-ECM up: in ep="); write_hex32(ep_in as u32);
        pl011_write(b" out ep="); write_hex32(ep_out as u32);
        pl011_write(b" mac="); write_hex32(u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]));
        write_hex32(((mac[4] as u32) << 8) | mac[5] as u32);
        pl011_write(b"\r\n");
        return true;
    }
    false
}

// --- smsc95xx (Raspberry Pi 2 LAN9514) USB-Ethernet ------------------------------------------------------
// The Pi 2's onboard NIC. Vendor-specific (class 0xFF, VID 0x0424 SMSC), NOT CDC-ECM, and NOT emulated by
// QEMU - so this path is HW-BLIND (written from the working u-boot/Linux `smsc95xx` reference, per the
// driver doctrine in arch/CLAUDE.md; behaviour is cited, code is a clean reimplementation). It differs from
// CDC-ECM in two ways: (1) all chip config is register R/W over VENDOR control requests (bRequest 0xA0 write
// / 0xA1 read, the register offset in wIndex, a 4-byte value in the data stage); (2) each TX frame is
// prefixed with an 8-byte TX command word and each RX frame with a 4-byte RX status word (handled in
// net_frame_tx/rx, branched on NET_KIND). Every hardware wait is bounded so a wrong assumption can't hang
// the boot - it just leaves the device unconfigured and net-stack degrades (invariant 12).

const SMSC_HW_CFG: u16 = 0x14;
const SMSC_HW_CFG_LRST: u32 = 0x0000_0008;      // Lite reset
const SMSC_HW_CFG_BIR:  u32 = 0x0000_1000;      // Bulk-IN empty response (0-length packet, not a NAK storm)
const SMSC_PM_CTRL: u16 = 0x20;
const SMSC_PM_CTRL_PHY_RST: u32 = 0x0000_0010;
const SMSC_AFC_CFG:  u16 = 0x2C;
const SMSC_BURST_CAP: u16 = 0x38;
const SMSC_BULK_IN_DLY: u16 = 0x6C;
const SMSC_MAC_CR: u16 = 0x100;
const SMSC_MAC_CR_TXEN: u32 = 0x0000_0008;
const SMSC_MAC_CR_RXEN: u32 = 0x0000_0004;
const SMSC_MAC_CR_FDPX: u32 = 0x0010_0000;      // full-duplex (Linux MAC_CR_FDPX_)
const SMSC_ADDRH: u16 = 0x104;
const SMSC_ADDRL: u16 = 0x108;
const SMSC_TX_CFG: u16 = 0x10;
const SMSC_TX_CFG_ON: u32 = 0x0000_0004;
const SMSC_MII_ADDR: u16 = 0x114;
const SMSC_MII_DATA: u16 = 0x118;
const SMSC_PHY_ID: u32 = 1;                     // the internal PHY is at MII address 1
const SMSC_MII_BMCR: u32 = 0;                   // basic mode control register
const SMSC_MII_ADVERTISE: u32 = 4;

/// Write a 4-byte smsc95xx register via a vendor control OUT (bRequest 0xA0; offset in wIndex).
fn smsc_write_reg(index: u16, value: u32) -> bool {
    let setup = [0x40, 0xA0, 0x00, 0x00, index as u8, (index >> 8) as u8, 4, 0x00];
    let mut data = value.to_le_bytes();
    ctrl_xfer(&setup, &mut data, false, 4)
}

/// Read a 4-byte smsc95xx register via a vendor control IN (bRequest 0xA1; offset in wIndex).
fn smsc_read_reg(index: u16) -> u32 {
    let setup = [0xC0, 0xA1, 0x00, 0x00, index as u8, (index >> 8) as u8, 4, 0x00];
    let mut data = [0u8; 4];
    if !ctrl_xfer(&setup, &mut data, true, 4) { return 0; }
    u32::from_le_bytes(data)
}

/// Wait (bounded) for the MII/MDIO engine to go not-busy (MII_ADDR bit 0).
fn smsc_mii_wait() {
    let mut n = 0u32;
    while smsc_read_reg(SMSC_MII_ADDR) & 1 != 0 { n += 1; if n > 100_000 { break; } }
}

fn smsc_mii_read(reg: u32) -> u16 {
    smsc_mii_wait();
    smsc_write_reg(SMSC_MII_ADDR, (SMSC_PHY_ID << 11) | (reg << 6) | 1); // BUSY, read (WRITE bit clear)
    smsc_mii_wait();
    (smsc_read_reg(SMSC_MII_DATA) & 0xFFFF) as u16
}

fn smsc_mii_write(reg: u32, val: u16) {
    smsc_mii_wait();
    smsc_write_reg(SMSC_MII_DATA, val as u32);
    smsc_write_reg(SMSC_MII_ADDR, (SMSC_PHY_ID << 11) | (reg << 6) | 0x02 | 1); // WRITE | BUSY
    smsc_mii_wait();
}

/// Bring up the LAN9514: select its config, reset the chip + PHY, program the MAC, enable TX/RX, kick the
/// PHY into auto-negotiation. HW-blind (see the section header); every wait is bounded.
fn configure_smsc95xx() -> bool {
    // Find the bulk endpoints + select the (single) configuration.
    let mut cfg = [0u8; 64];
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, 9) { return false; }
    let total = (((cfg[2] as usize) | ((cfg[3] as usize) << 8)).max(9)).min(cfg.len());
    if !get_descriptor(0x80, 0x02, 0x00, 0, &mut cfg, total) { return false; }
    let cfg_val = cfg[5];
    let mut i = 0usize;
    let mut ep_in = 0u8;
    let mut ep_out = 0u8;
    let mut bulk_mps = 64u16;
    while i + 2 <= total {
        let blen = cfg[i] as usize;
        if blen == 0 { break; }
        if cfg[i + 1] == 0x05 && i + 7 <= total && cfg[i + 3] & 0x03 == 0x02 {   // bulk endpoint
            let raw = (cfg[i + 4] as u16) | ((cfg[i + 5] as u16) << 8);
            bulk_mps = match raw & 0x07FF { 0 => 64, v => v }; // [10:0] = size; [12:11] = HS mult
            if cfg[i + 2] & 0x80 != 0 { ep_in = cfg[i + 2] & 0x0F; } else { ep_out = cfg[i + 2] & 0x0F; }
        }
        i += blen;
    }
    if ep_in == 0 || ep_out == 0 { pl011_write(b"dwc2: smsc no bulk endpoints\r\n"); return false; }
    if !control_out(0x00, 0x09, cfg_val as u16, 0) { pl011_write(b"dwc2: smsc SET_CONFIG failed\r\n"); return false; }

    // Lite reset the chip, then reset the PHY.
    smsc_write_reg(SMSC_HW_CFG, smsc_read_reg(SMSC_HW_CFG) | SMSC_HW_CFG_LRST);
    let mut n = 0u32;
    while smsc_read_reg(SMSC_HW_CFG) & SMSC_HW_CFG_LRST != 0 { n += 1; if n > 100_000 { break; } }
    smsc_write_reg(SMSC_PM_CTRL, smsc_read_reg(SMSC_PM_CTRL) | SMSC_PM_CTRL_PHY_RST);
    n = 0;
    while smsc_read_reg(SMSC_PM_CTRL) & SMSC_PM_CTRL_PHY_RST != 0 { n += 1; if n > 100_000 { break; } }

    // MAC: prefer the real board MAC (b8:27:eb:..) read from the VideoCore mailbox at boot; else read
    // whatever the firmware left in the chip; else fall back to a locally-administered address.
    let mac = super::video::board_mac().unwrap_or_else(|| {
        let lo = smsc_read_reg(SMSC_ADDRL);
        let hi = smsc_read_reg(SMSC_ADDRH);
        let m = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
        if m == [0u8; 6] || m == [0xFFu8; 6] {
            [0x02, 0x00, 0x00, 0x12, 0x34, 0x56]               // locally-administered (bit 1 of byte 0 set)
        } else {
            m
        }
    });
    smsc_write_reg(SMSC_ADDRL, (mac[0] as u32) | ((mac[1] as u32) << 8) | ((mac[2] as u32) << 16) | ((mac[3] as u32) << 24));
    smsc_write_reg(SMSC_ADDRH, (mac[4] as u32) | ((mac[5] as u32) << 8));

    // Read-modify-write (Linux does): a bare write would clear HW_CFG's non-zero power-on default bits.
    smsc_write_reg(SMSC_HW_CFG, smsc_read_reg(SMSC_HW_CFG) | SMSC_HW_CFG_BIR); // empty bulk-IN -> 0-length packet, not a NAK
    smsc_write_reg(SMSC_BURST_CAP, 0);                          // one frame per transfer (our simple model)
    smsc_write_reg(SMSC_BULK_IN_DLY, 0x2000);                   // smsc95xx default
    smsc_write_reg(SMSC_AFC_CFG, 0x00F8_30A1);                  // flow-control thresholds (smsc95xx default)

    // PHY: reset, advertise 10/100, restart auto-negotiation. We do NOT block on link (net-stack retries +
    // self-configures when the link comes up).
    smsc_mii_write(SMSC_MII_BMCR, 0x8000);                     // PHY reset
    n = 0;
    while smsc_mii_read(SMSC_MII_BMCR) & 0x8000 != 0 { n += 1; if n > 1000 { break; } }
    smsc_mii_write(SMSC_MII_ADVERTISE, 0x01E1);               // 100/10 full+half, 802.3
    smsc_mii_write(SMSC_MII_BMCR, 0x1200);                    // ANENABLE | ANRESTART

    // Enable TX + RX.
    // FDPX: the Pi's internal PHY negotiates full duplex; without setting it the MAC runs half-duplex on
    // a full-duplex link (late collisions / drops). We do not watch link status, so set it as the default.
    smsc_write_reg(SMSC_MAC_CR, smsc_read_reg(SMSC_MAC_CR) | SMSC_MAC_CR_TXEN | SMSC_MAC_CR_RXEN | SMSC_MAC_CR_FDPX);
    smsc_write_reg(SMSC_TX_CFG, SMSC_TX_CFG_ON);

    BULK_MPS.store(bulk_mps, Ordering::Relaxed);
    BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
    BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
    NET_ADDR.store(DEV_ADDR.load(Ordering::Relaxed), Ordering::Relaxed);
    NET_LOW.store(false, Ordering::Relaxed);
    NET_EP_IN.store(ep_in, Ordering::Relaxed);
    NET_EP_OUT.store(ep_out, Ordering::Relaxed);
    // SAFETY: NET_MAC is written only during core-0 enumeration.
    unsafe { (*core::ptr::addr_of_mut!(NET_MAC)).copy_from_slice(&mac); }
    NET_KIND.store(NET_KIND_SMSC, Ordering::Relaxed);
    NET_READY.store(true, Ordering::Release);
    pl011_write(b"dwc2: smsc95xx (LAN9514) up: in ep="); write_hex32(ep_in as u32);
    pl011_write(b" out ep="); write_hex32(ep_out as u32);
    pl011_write(b" mac="); write_hex32(u32::from_be_bytes([mac[0], mac[1], mac[2], mac[3]]));
    write_hex32(((mac[4] as u32) << 8) | mac[5] as u32);
    pl011_write(b" (HW-UNVERIFIED)\r\n");
    true
}

// --- USB-net bridge: the mechanism the userspace ARM `nic-driver` calls (via syscalls) to move ethernet
// frames to/from the net device. net-stack owns all protocol (ARP/IP/DHCP); this is pure transport. ---

const NET_FRAME_MAX: usize = 1600;                  // matches nic-driver's FRAME_MAX

/// True on the boot processor. The DWC2 has one host channel + one DMA buffer, driven only from core 0;
/// the bridge functions guard on this so a misplaced `nic-driver` can never corrupt that shared state.
fn on_core0() -> bool {
    let mpidr: u32;
    // SAFETY: reading MPIDR (`c0, c0, 5`) is a side-effect-free PL1 register read.
    unsafe { core::arch::asm!("mrc p15, 0, {m}, c0, c0, 5", m = out(reg) mpidr, options(nomem, nostack)); }
    mpidr & 3 == 0
}

/// Transmit one ethernet frame (bulk OUT). Returns true if it was handed to the device.
pub fn net_frame_tx(frame: &[u8]) -> bool {
    if !NET_READY.load(Ordering::Acquire) || !on_core0() { return false; }
    // Point the shared channel at the net device (the keyboard poll may have selected itself last).
    select_device(NET_ADDR.load(Ordering::Relaxed), BULK_MPS.load(Ordering::Relaxed),
                  NET_LOW.load(Ordering::Relaxed));
    let ep_out = NET_EP_OUT.load(Ordering::Relaxed) as u32;
    let n = frame.len().min(NET_FRAME_MAX);
    let mut buf = [0u8; NET_FRAME_MAX + 8];                     // room for the smsc95xx 8-byte TX command
    let total = if NET_KIND.load(Ordering::Relaxed) == NET_KIND_SMSC {
        // smsc95xx TX command: TX_CMD_A = len | FIRST_SEG(0x2000) | LAST_SEG(0x1000); TX_CMD_B = len.
        let a = (n as u32) | 0x0000_2000 | 0x0000_1000;
        buf[0..4].copy_from_slice(&a.to_le_bytes());
        buf[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        buf[8..8 + n].copy_from_slice(&frame[..n]);
        n + 8
    } else {
        buf[..n].copy_from_slice(&frame[..n]);                 // CDC-ECM: raw frame
        n
    };
    if bulk_xfer(CH_NET, false, ep_out, &mut buf, total, false) < 0 { return false; }
    // CDC-ECM delimits a datagram with a short packet; a frame that is an exact multiple of the bulk max
    // packet size needs a trailing zero-length packet, or the device won't see the frame boundary. (smsc95xx
    // carries an explicit length in its TX command, so it needs no ZLP.)
    if NET_KIND.load(Ordering::Relaxed) != NET_KIND_SMSC {
        let mps = BULK_MPS.load(Ordering::Relaxed) as usize;
        if total != 0 && mps != 0 && total % mps == 0 {
            let mut zlp = [0u8; 1];
            let _ = bulk_xfer(CH_NET, false, ep_out, &mut zlp, 0, false);
        }
    }
    true
}

/// Receive one ethernet frame (a single bulk IN attempt), copied into `dst`. Returns the frame length, or
/// 0 if none is available (the device NAKs an empty bulk IN).
pub fn net_frame_rx(dst: &mut [u8]) -> usize {
    if !NET_READY.load(Ordering::Acquire) || !on_core0() { return 0; }
    // Point the shared channel at the net device (the keyboard poll may have selected itself last).
    select_device(NET_ADDR.load(Ordering::Relaxed), BULK_MPS.load(Ordering::Relaxed),
                  NET_LOW.load(Ordering::Relaxed));
    let ep_in = NET_EP_IN.load(Ordering::Relaxed) as u32;
    // A single bulk-IN attempt: a NAK means "no frame queued now" and must return fast (net-stack
    // re-polls under its own deadline), so no retry/backoff here.
    if NET_KIND.load(Ordering::Relaxed) == NET_KIND_SMSC {
        // smsc95xx prefixes each frame with a 4-byte RX status word: bit 15 = error summary, bits[30:16] =
        // frame length (INCLUDING the 4-byte FCS, which we strip). The frame follows the status word.
        let mut buf = [0u8; NET_FRAME_MAX + 4];
        let got = bulk_xfer(CH_NET, true, ep_in, &mut buf, NET_FRAME_MAX + 4, false);
        if got < 4 { return 0; }
        let status = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if status & 0x0000_8000 != 0 { return 0; }             // RX error summary - drop
        let flen = ((status >> 16) & 0x3FFF) as usize;
        if flen < 4 || 4 + flen > got as usize { return 0; }
        let payload = flen - 4;                                 // strip the trailing FCS
        let m = payload.min(dst.len());
        dst[..m].copy_from_slice(&buf[4..4 + m]);
        m
    } else {
        let cap = dst.len().min(NET_FRAME_MAX);                 // CDC-ECM: raw frame, no header
        let got = bulk_xfer(CH_NET, true, ep_in, dst, cap, false);
        if got > 0 { got as usize } else { 0 }
    }
}

/// The USB-net device's MAC + link state, or None if no net device is up. CDC-ECM's link is up once the
/// interface is configured (QEMU slirp is always up); a real driver would read a link-status notification.
pub fn net_info() -> Option<([u8; 6], bool)> {
    if !NET_READY.load(Ordering::Acquire) { return None; }
    // SAFETY: NET_MAC is written once at enumeration; read-only here.
    let mac = unsafe { *core::ptr::addr_of!(NET_MAC) };
    Some((mac, true))
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
    let mut ep_mps = 8u16;
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
                let raw = (cfg[i + 4] as u16) | ((cfg[i + 5] as u16) << 8);
                ep_mps = match raw & 0x07FF { 0 => 8, v => v };
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
    KBD_MPS.store(ep_mps.min(255) as u8, Ordering::Relaxed);                      // interrupt-endpoint packet size for the poll
    KBD_LOW.store(LOW_SPEED.load(Ordering::Relaxed), Ordering::Relaxed);
    KBD_HUB_PORT.store(SPLIT_PORT.load(Ordering::Relaxed), Ordering::Relaxed); // remember the split path for poll()
    KBD_TOGGLE.store(false, Ordering::Relaxed);
    KBD_READY.store(true, Ordering::Release);
    pl011_write(b"dwc2: boot keyboard ready on ep="); write_hex32(ep as u32); pl011_write(b"\r\n");
    true
}

// --- bulk transfers (the shared foundation for USB mass storage and, later, USB-Ethernet) ---
// A bulk endpoint keeps its own DATA0/DATA1 toggle per direction, advanced only on a completed packet.
static BULK_TOGGLE_IN:  AtomicBool = AtomicBool::new(false);
static BULK_TOGGLE_OUT: AtomicBool = AtomicBool::new(false);
/// The next data PID (true = DATA1) after the most recent chan_dma, computed from the ACTUAL packet count
/// (split path) or the HCTSIZ.PID readback (direct path). bulk_xfer stores this into the endpoint toggle so
/// an even-packet-count transfer does not desync - a blind flip is only correct for odd packet counts.
static NEXT_BULK_PID_DATA1: AtomicBool = AtomicBool::new(false);
static BULK_MPS:        AtomicU16  = AtomicU16::new(64);  // bulk endpoint max-packet (set at config time)

/// One bulk transfer on endpoint `ep`, through the `DMA.data` buffer, with cache maintenance for the A7's
/// non-coherent DMA. Uses the bulk endpoint's max-packet (`BULK_MPS`) for the packet count and maintains
/// the per-direction data toggle. Returns the number of bytes transferred (for IN, the device may send a
/// short packet, so this can be < `len`), or -1 on failure / no data.
fn bulk_xfer(ch: u32, dir_in: bool, ep: u32, data: &mut [u8], len: usize, can_block: bool) -> i32 {
    MPS0.store(BULK_MPS.load(Ordering::Relaxed), Ordering::Relaxed); // chan_program uses MPS0 for pktcnt
    if ch == CH_BULK && can_block {
        // ASYNC storage parks between the three BOT transfers (CBW/data/CSW), and the keyboard tick runs
        // in that gap - re-pointing the SHARED device selection (DEV_ADDR/LOW_SPEED/SPLIT_PORT) at the
        // keyboard. Re-establish storage's selection before this transfer, here in the IRQs-masked window
        // (before chan_dma reads SPLIT_PORT and chan_program reads DEV_ADDR/LOW_SPEED), so it is atomic
        // against the keyboard tick - which needs IRQs to run. MPS0 is already BULK_MPS above; the ISR
        // re-arm is immune via register replay, so this covers only the FRESH chan_program of each transfer.
        DEV_ADDR.store(MSC_ADDR.load(Ordering::Relaxed), Ordering::Relaxed);
        LOW_SPEED.store(false, Ordering::Relaxed);
        SPLIT_PORT.store(MSC_HUB_PORT.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    let toggle = if dir_in { &BULK_TOGGLE_IN } else { &BULK_TOGGLE_OUT };
    let pid = if toggle.load(Ordering::Relaxed) { PID_DATA1 } else { PID_DATA0 };
    // SAFETY: the DMA buffers are touched only on core 0; addr_of gives their identity-mapped physical
    // address. Storage (CH_BULK) uses its OWN buffer (`MSC_DMA`) so a parked async storage transfer never
    // aliases a concurrent net transfer through the shared `DMA`; everything else uses `DMA`.
    let got = unsafe {
        let (bufp, cap): (*mut u8, usize) = if ch == CH_BULK {
            (core::ptr::addr_of_mut!((*core::ptr::addr_of_mut!(MSC_DMA)).data) as *mut u8, 512)
        } else {
            (core::ptr::addr_of_mut!((*core::ptr::addr_of_mut!(DMA)).data) as *mut u8, 2048)
        };
        let buf: &mut [u8] = core::slice::from_raw_parts_mut(bufp, cap);
        let data_phys = buf.as_ptr() as u32;
        let n = len.min(buf.len());
        if dir_in {
            flush_dcache(data_phys, n as u32);                     // invalidate before the device writes
            if !chan_dma(ch, true, pid, data_phys, n as u32, ep, 2, can_block) { -1i32 }
            else {
                flush_dcache(data_phys, n as u32);                 // invalidate after -> read device bytes
                // HCTSIZ.xfersize counts DOWN as bytes arrive, so received = requested - remaining.
                let remaining = (rd(hctsiz_at(ch)) & 0x7_FFFF) as usize;
                let recv = n.saturating_sub(remaining);
                let m = recv.min(data.len());
                data[..m].copy_from_slice(&buf[..m]);
                recv as i32
            }
        } else {
            // Send exactly what we filled. `n` is the caller's requested length clamped to the buffer,
            // but the caller's own slice may be shorter still - and transmitting `n` after filling only
            // `m` would push (n - m) bytes of the PREVIOUS transfer's leftovers to the device. On a disk
            // that is silent corruption written to the medium, which is the one failure this driver must
            // never produce. No current caller passes a short slice (a block write is 512/512), so this
            // costs nothing today and removes the trap for the next one.
            let m = n.min(data.len());
            buf[..m].copy_from_slice(&data[..m]);
            flush_dcache(data_phys, m as u32);
            if chan_dma(ch, false, pid, data_phys, m as u32, ep, 2, can_block) { m as i32 } else { -1i32 }
        }
    };
    // Advance the endpoint data toggle to the parity-correct next PID that chan_dma computed (from the actual
    // packet count / HCTSIZ.PID) - NOT a blind flip, which desyncs on an even-packet-count multi-packet transfer.
    if got >= 0 { toggle.store(NEXT_BULK_PID_DATA1.load(Ordering::Relaxed), Ordering::Relaxed); }
    got
}

// --- USB Mass Storage (Bulk-Only Transport) - a QEMU-verifiable exerciser of the bulk path ---
// BOT wraps each SCSI command in a 31-byte CBW (bulk OUT), an optional data stage, and a 13-byte CSW
// (bulk IN). Signatures: CBW "USBC" (0x43425355), CSW "USBS" (0x53425355).

/// Attempts per bulk transfer inside a BOT command. A real stick NAKs while its flash is busy - an
/// erase or an internal remap can outlast a single attempt - so give it several rather than declaring
/// the device broken on the first one. Each attempt is separately time-bounded, so this lengthens the
/// Retry policy now lives in `chan_dma`, expressed the way the protocol actually distinguishes cases:
/// three genuine TRANSACTION errors fail a transfer (`XACT_ERR_MAX`, matching Linux), while a NAK -
/// the device saying "busy, ask again" while its flash is occupied - never counts and is bounded only
/// by wall-clock (`CORE_HOLD_US`). A single attempt count could not express that difference, which is
/// why a busy stick used to be declared broken.

/// Recover the Bulk-Only endpoints after a failed command.
///
/// **Why a failure must not merely be reported.** A BOT command is three transfers (CBW, data, CSW). If
/// one fails the device is left mid-command: its endpoint may be HALTED and our data toggle no longer
/// matches its. Every later command then fails too - which is exactly what hardware showed, one bad
/// transfer turning into `drives check`, `drives scrub` and `fcap` all failing in a row. Clearing the
/// halt on both endpoints and resetting the toggles puts the pair back in a known state, so a transient
/// glitch costs one command instead of every command after it.
///
/// USB 2.0 CLEAR_FEATURE(ENDPOINT_HALT): bmRequestType 0x02 (host-to-device, endpoint recipient),
/// bRequest 0x01, wValue 0, wIndex = the endpoint address with bit 7 set for an IN endpoint. Clearing
/// the halt also resets the DEVICE's toggle to DATA0, which is why ours is reset to match.
fn bot_recover(ep_in: u32, ep_out: u32) -> bool {
    // Re-select the device with its CONTROL packet size. `msc_select` leaves MPS0 set to the BULK
    // endpoint's size (512 on a high-speed stick), and a control transfer framed with that instead of
    // EP0's 64 is malformed - the clear-halt then fails and disturbs the device further, turning a
    // recovery into a second fault. (Measured: doing this wrong took a selfcheck run from 16 failures
    // to 70.) Restore the bulk selection afterwards so the caller's next transfer is framed correctly.
    select_device(MSC_ADDR.load(Ordering::Relaxed), MSC_EP0_MPS.load(Ordering::Relaxed), false);
    SPLIT_PORT.store(MSC_HUB_PORT.load(Ordering::Relaxed), Ordering::Relaxed);
    // Step 1 of the Bulk-Only Transport reset recovery, which was missing: the class-specific
    // **Bulk-Only Mass Storage Reset** (bmRequestType 0x21, bRequest 0xFF, wIndex = interface). The
    // spec orders it FIRST, before the clear-halts, because it is the step that resynchronises the
    // device's own CBW/CSW state machine - clear-halt only unsticks the pipes. Without it a device
    // that lost framing stayed lost: we would clear both halts, it would still be waiting for the
    // rest of a transfer we had abandoned, and every following command failed. Interface 0 (this
    // driver binds the first mass-storage interface; `probe_mass_storage` does not look past it).
    let reset_ok = control_out(0x21, 0xFF, 0x0000, 0);
    let reset_fail = last_fail_str();       // capture before the next transfer overwrites it
    let in_ok  = control_out(0x02, 0x01, 0x0000, (ep_in | 0x80) as u16); // CLEAR_FEATURE(ENDPOINT_HALT)
    let in_fail = last_fail_str();
    let out_ok = control_out(0x02, 0x01, 0x0000, ep_out as u16);
    let out_fail = last_fail_str();
    BULK_TOGGLE_IN.store(false, Ordering::Relaxed);
    BULK_TOGGLE_OUT.store(false, Ordering::Relaxed);
    msc_select();
    // A recovery that itself failed is still a failure, and saying so is the whole point (§26.7).
    // These results used to be discarded with `let _ =`, so a recovery that achieved nothing looked
    // exactly like one that worked - and the caller went on to fail forever with no idea why. Name
    // WHICH of the three control transfers failed and WHY (NAK-timeout = the device is busy and this
    // is retryable; XACT/STALL = a transport wedge), because "not answering on EP0" is three different
    // failures and they need different fixes - the whole point of the drop-off investigation.
    let ok = reset_ok && in_ok && out_ok;
    if !ok {
        pl011_write(b"dwc2: bot RESET RECOVERY FAILED on EP0 - MSReset ");
        pl011_write(if reset_ok { b"ok" } else { reset_fail.as_bytes() });
        pl011_write(b", clr-IN "); pl011_write(if in_ok { b"ok" } else { in_fail.as_bytes() });
        pl011_write(b", clr-OUT "); pl011_write(if out_ok { b"ok" } else { out_fail.as_bytes() });
        pl011_write(b"\r\n");
    }
    ok
}
/// The mass-storage device's endpoint-0 max packet size (see `bot_recover`).
static MSC_EP0_MPS: AtomicU16 = AtomicU16::new(64);


/// Set while a command we are ALLOWED to be refused is in flight (see the CSW-bad branch).
static BOT_PROBE: AtomicBool = AtomicBool::new(false);

/// How many times a vanished device may be revived per boot, and whether a revival is already running.
static MSC_REVIVE_TRIES: AtomicU32 = AtomicU32::new(0);
static MSC_REVIVING: AtomicBool = AtomicBool::new(false);
const MSC_REVIVE_MAX: u32 = 3;
/// Consecutive failed BOT commands. Cleared by any success - see `recover_or_revive` for why a
/// clear-halt that reports OK is not enough on its own to call the device healthy.
static MSC_FAIL_STREAK: AtomicU32 = AtomicU32::new(0);
const MSC_FAIL_STREAK_MAX: u32 = 4;

/// Consecutive BUSY hand-backs with no successful command in between, and how many of them mean the
/// endpoint is STUCK rather than occupied.
///
/// Busy was made exempt from recovery because recovering on every busy issued a Mass Storage Reset for
/// ordinary flow control - 564 of them in one selfcheck. That was right, and then it was applied too
/// absolutely: recovery went from "always" to "never", which removed the only thing that clears a
/// desynchronised endpoint. Hardware then showed exactly that state - `write lba 2 gave up after 6000
/// busy retries`, a solid **30 seconds** of NAK on a single write, after which `fs` degraded the mount.
/// No device is occupied for 30 seconds; a device that never once pauses is not busy, it is stuck (the
/// classic cause being a CSW we never collected, which the device waits on before accepting any new CBW).
///
/// The two are told apart by DURATION, which is the only thing that distinguishes them: a run of busies
/// unbroken by a single success. Linux draws the same line - `usb-storage` lets a command NAK freely and
/// then, when it exceeds the command timeout, runs the very same BOT reset recovery.
///
/// 200 consecutive hand-backs is about a second. Recovery then fires once per further 200, so a device
/// stuck for the full 30-second budget gets ~30 reset attempts rather than 6000 (bounded, §26.6), and a
/// device that is merely slow gets none at all - one success resets the run.
static MSC_BUSY_RUN: AtomicU32 = AtomicU32::new(0);
const MSC_BUSY_RUN_MAX: u32 = 200;

/// Record a BUSY hand-back, and repair the endpoint if they stop looking like flow control.
///
/// The repair is chosen by what the device ANSWERS, not by assuming what busy means. The first attempt
/// here asserted that "busy is positive evidence the device is present and answering, so resynchronise
/// the transport rather than declaring it gone" - and hardware disproved that sentence directly: the
/// endpoint kept NAKing while `bot_recover` reported `device is not answering on EP0`, fourteen times in
/// a row, storage never returning. A halted bulk endpoint still answers control transfers, so an EP0
/// that does not answer means the device itself has stopped responding - it is absent, not confused, and
/// no amount of endpoint recovery reaches it. That is precisely the signal `recover_or_revive` escalates
/// on (port reset + fresh enumeration, bounded to `MSC_REVIVE_MAX`), and withholding it here left the one
/// layer that could have recovered this unreachable from the busy path.
///
/// So: try the cheap repair, and let its verdict pick the next one. Escalation stays bounded and stays
/// in one place - this decides only *whether* the device looks gone, never what to do about it.
fn note_busy(ep_in: u32, ep_out: u32) {
    let run = MSC_BUSY_RUN.fetch_add(1, Ordering::Relaxed) + 1;
    if run % MSC_BUSY_RUN_MAX != 0 { return; }
    pl011_write(b"dwc2: device busy with no pause for ");
    super::timer::write_dec_pub(run);
    pl011_write(b" commands - treating the endpoint as stuck, BOT reset recovery\r\n");
    // EP0 answered: the device is present and merely out of step, and the clear-halt is the whole fix.
    // EP0 did not: it is gone, and only a port reset + re-enumeration brings it back.
    if !bot_recover(ep_in, ep_out) { revive_if_needed(false, false, ep_in, ep_out); }
}

/// Recover from a failed BOT command, escalating to a full re-enumeration if the device has GONE.
///
/// `bot_recover` clears the endpoint halts, which fixes a device that is confused. It cannot fix one
/// that is absent - and the two are distinguishable: a halted bulk endpoint still answers control
/// transfers, so a `bot_recover` that fails on EP0 means the device itself stopped responding. That
/// happened on real hardware after ~27 s of sustained I/O, and because nothing escalated, the FIRST
/// such failure killed storage until reboot: every later command failed against a device that was no
/// longer there.
///
/// The USB-level answer to a device that has gone is a port reset and a fresh enumeration, which is
/// exactly what boot does. The machinery already existed and was simply never reached after boot.
fn recover_or_revive(ep_in: u32, ep_out: u32) {
    // Escalate on EITHER signal, because "recovery succeeded" is not the same as "the device works".
    //
    // Observed: 15 consecutive command failures during `drives check` with `bot_recover` returning
    // TRUE every single time and no revival ever attempted. EP0 answered, the halts cleared, the reset
    // was accepted - and the very next CBW failed again, all the way down. Gating escalation purely on
    // the recovery's own verdict trusts the wrong thing: what matters is whether COMMANDS start
    // working again, not whether the repair reported OK.
    //
    // So a run of failures escalates on its own. The streak is cleared by any successful command
    // (`bot_command`), which is the only evidence that actually settles it.
    // `bot_recover` runs even during a revival, and that is deliberate. It re-points the bus using the
    // MSC_* coordinates - which `probe_mass_storage` now publishes BEFORE issuing any command, so they
    // describe the device actually present. Suppressing recovery here instead (the first attempt at
    // this) starved the probe of the endpoint recovery it DEPENDS on: its very first command is
    // expected to fail on the power-on UNIT ATTENTION, and without a clear-halt every revival
    // enumerated the stick perfectly and then failed every command after it - 0 revivals out of 3,
    // against 2 of 3 before. Fixing the stale coordinates removes the reason to suppress it.
    let streak = MSC_FAIL_STREAK.fetch_add(1, Ordering::Relaxed) + 1;
    let recovered = bot_recover(ep_in, ep_out);
    revive_if_needed(recovered, streak >= MSC_FAIL_STREAK_MAX, ep_in, ep_out);
}

/// Escalate to a port reset + re-enumeration, given a repair verdict somebody else already obtained.
///
/// Split out so a caller that has ALREADY run `bot_recover` can act on its answer without running it a
/// second time (`note_busy` does exactly this). The escalation policy stays here, in one place - callers
/// supply evidence, never a decision.
fn revive_if_needed(recovered: bool, streak_exhausted: bool, ep_in: u32, ep_out: u32) {
    if recovered && !streak_exhausted { return; }
    if !recovered {
        // EP0 is not answering: the device is gone rather than confused (see `bot_recover`).
    } else {
        pl011_write(b"dwc2: recovery keeps succeeding but commands keep failing - escalating\r\n");
    }
    if MSC_REVIVING.swap(true, Ordering::Relaxed) { return; } // re-entered from the probe below
    let tries = MSC_REVIVE_TRIES.load(Ordering::Relaxed);
    if tries >= MSC_REVIVE_MAX {
        MSC_REVIVING.store(false, Ordering::Relaxed);
        return;                                  // bounded: stop trying, stay loudly unavailable
    }
    MSC_REVIVE_TRIES.store(tries + 1, Ordering::Relaxed);
    // Capture the controller state at the drop-off, so a hardware log says WHICH failure this is rather
    // than only that recovery ran. HPRT.PrtConnSts (bit 0) is the deciding bit: SET = the device is
    // still electrically present and it is the host CHANNEL that wedged (the FIFO-starvation case the
    // resize targets); CLEAR = the device really left the bus (an electrical/power event no software
    // fix reaches). GINTSTS shows any pending global condition; the bulk channel's HCINT shows how the
    // last transfer ended.
    pl011_write(b"dwc2: drop-off state: HPRT="); write_hex32(rd(HPRT));
    pl011_write(b" GINTSTS="); write_hex32(rd(GINTSTS));
    pl011_write(b" HCINT[bulk]="); write_hex32(rd(hcint_at(CH_BULK)));
    pl011_write(b" (HPRT bit0 set = device present, channel wedged; clear = device left the bus)\r\n");
    pl011_write(b"dwc2: device stopped answering EP0 - port reset + re-enumerate, attempt ");
    super::timer::write_dec_pub(tries + 1);
    pl011_write(b" of 3\r\n");
    // Stop serving only WHILE the bus is rebuilt, so nothing issues commands mid-reset.
    MSC_READY.store(false, Ordering::Release);
    // Put OUR side of the bus back to the default state before resetting the port, because that is the
    // state the DEVICES come back in. A port reset returns every device to address 0, EP0 max-packet 8,
    // and no split routing - while `DEV_ADDR`, `MPS0` and `SPLIT_PORT` still described the mass-storage
    // device that just died (address N, mps 512). Enumeration then addressed a device that no longer
    // existed and failed at its very first transfer:
    //
    //     dwc2: root port enabled after reset, high-speed (480 Mbps)   <- the reset worked
    //     dwc2: SETUP failed
    //     dwc2: GET_DESC(8) failed - USB unavailable                   <- talking to the wrong address
    //
    // At boot these statics are already at exactly these values, which is why enumeration only ever
    // worked the first time. Nothing reset them afterwards because nothing had ever re-enumerated.
    DEV_ADDR.store(0, Ordering::Relaxed);
    MPS0.store(8, Ordering::Relaxed);
    SPLIT_PORT.store(0, Ordering::Relaxed);
    KBD_READY.store(false, Ordering::Relaxed);
    NET_READY.store(false, Ordering::Relaxed);
    select_device(0, 8, false);
    // `reset_port` ENUMERATES as its last step. Calling `enumerate_sync` after it ran the whole thing
    // a second time against a bus that had just been addressed - the doubled "GET_DESC(8) failed" pair
    // in the log is that second pass, failing for a different reason than the first.
    reset_port();
    MSC_REVIVING.store(false, Ordering::Relaxed);
    if MSC_READY.load(Ordering::Acquire) {
        // Say what the revival COST, not just that it worked. A port reset clears the device's
        // volatile write cache, and this device accepts no SYNCHRONIZE CACHE - so any write it had
        // acknowledged but not yet committed is gone. Availability is bought with durability here, and
        // that is a real trade rather than a free recovery: observed as a file written moments before
        // a revival reading back EMPTY afterwards, which looks like corruption until you know why.
        pl011_write(b"dwc2: device came back - storage restored (writes it had BUFFERED are LOST: a port \
reset clears the device cache and this device accepts no flush)\r\n");
        return;
    }
    // The revival did not take. RESTORE the serving flag anyway - do NOT leave storage switched off.
    //
    // Clearing it permanently was meant to stop the driver "looping on a corpse". What it actually did
    // was end the boot: every `msc_*` entry point returns early on `!MSC_READY`, so no command ever
    // reaches `bot_command`, so `recover_or_revive` is never called again - which makes the 3-attempt
    // budget unreachable and the FIRST failed attempt final. Measured: one failed revival, then 126
    // lost block operations and 84 test failures, where the previous retry-forever behaviour had
    // recovered on its own more than once.
    //
    // Retrying a dead device costs bounded, loud failures. Refusing to retry costs the machine's
    // storage until reboot. That is not a close call - and it is the second time on this branch that a
    // recovery path did more damage than the fault it was written for.
    MSC_READY.store(true, Ordering::Release);
    // Restore the OTHER devices' flags too. Clearing them before the port reset is right - the reset
    // genuinely drops the keyboard and the NIC - but leaving them false when enumeration fails is the
    // same defect as the `MSC_READY` one fixed a commit ago, applied to the two flags that were not
    // audited then. `usb_poll` returns early on `!KBD_READY`, and `reset_port` has exactly two callers
    // (boot, and here), so nothing would ever set them again: a STORAGE fault would take the keyboard
    // and the network down permanently, on a board where the keyboard IS the console. Storage retries
    // can reach this path again; a dead keyboard flag cannot reach anything.
    KBD_READY.store(true, Ordering::Relaxed);
    NET_READY.store(true, Ordering::Relaxed);
    pl011_write(b"dwc2: device did not come back - still retrying commands (revival attempt ");
    super::timer::write_dec_pub(tries + 1);
    pl011_write(b" of 3 spent)\r\n");
}

/// Run one SCSI command via BOT. `cdb` is the SCSI command block; `data`/`dlen` is the data stage
/// (`data_in` selects direction). Returns true iff the command completed with CSW status = passed.
fn bot_command(ep_in: u32, ep_out: u32, cdb: &[u8], data_in: bool, data: &mut [u8], dlen: usize, can_block: bool) -> bool {
    let mut cbw = [0u8; 31];
    cbw[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());     // dCBWSignature "USBC"
    cbw[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());     // dCBWTag
    cbw[8..12].copy_from_slice(&(dlen as u32).to_le_bytes());     // dCBWDataTransferLength
    cbw[12] = if data_in { 0x80 } else { 0x00 };                 // bmCBWFlags (bit7 = data-IN)
    cbw[13] = 0;                                                  // bCBWLUN
    cbw[14] = cdb.len() as u8;                                    // bCBWCBLength
    let n = cdb.len().min(16);
    cbw[15..15 + n].copy_from_slice(&cdb[..n]);

    if bulk_xfer(CH_BULK, false, ep_out, &mut cbw, 31, can_block) < 0 {
        // A BUSY hand-back is not a failure - the caller re-asks and it usually succeeds. Logging
        // it printed 564 lines in ONE selfcheck for entirely normal flow control, which is the
        // same "report a characteristic as a fault" mistake already fixed once this session:
        // loud is a budget, and spending it on the expected case is how a real line gets ignored.
        if !msc_last_was_busy() {
            pl011_write(b"dwc2: bot CBW-out failed - "); pl011_write(last_fail_str().as_bytes());
            pl011_write(b"\r\n");
        }
        // A BUSY device needs no recovery - it is working, just occupied, and the caller re-asks.
        // Recovering here meant every busy hand-back issued a Mass Storage Reset plus two clear-halts
        // and bumped the failure streak: 564 of them in one selfcheck, which is what produced 58
        // escalations and 3 port resets of a stick that was never broken. Recovery is for faults.
        if msc_last_was_busy() { note_busy(ep_in, ep_out); } else { recover_or_revive(ep_in, ep_out); }
        return false;
    }
    // Keep what the data stage actually moved, so the CSW check below can compare the device's own
    // residue against it rather than trusting a "passed" verdict on a transfer that fell short.
    let mut moved = 0usize;
    if dlen > 0 {
        let n = bulk_xfer(CH_BULK, data_in, if data_in { ep_in } else { ep_out }, data, dlen, can_block);
        if n < 0 {
            // A BUSY hand-back is not a failure - the caller re-asks and it usually succeeds. Logging
            // it printed 564 lines in ONE selfcheck for entirely normal flow control, which is the
            // same "report a characteristic as a fault" mistake already fixed once this session:
            // loud is a budget, and spending it on the expected case is how a real line gets ignored.
            if !msc_last_was_busy() {
                pl011_write(b"dwc2: bot data-stage failed - "); pl011_write(last_fail_str().as_bytes());
                pl011_write(b"\r\n");
            }
            // Escalates like its CBW-out and CSW-in siblings, and must. This site used to run a bare
            // `bot_recover` and DISCARD its verdict, so a data-stage failure never bumped the failure
            // streak and could therefore never reach `revive_if_needed` - the entire revival machinery
            // was unreachable from the likeliest place for a block transfer to die (the 512-byte data
            // phase, eight split chunks wide on this board). A stick that dies mid-READ(10) would retry
            // into this branch forever, storage staying dead while the log said nothing.
            if msc_last_was_busy() { note_busy(ep_in, ep_out); } else { recover_or_revive(ep_in, ep_out); }
            return false;
        }
        moved = n as usize;
    }

    let mut csw = [0u8; 13];
    if bulk_xfer(CH_BULK, true, ep_in, &mut csw, 13, can_block) < 0 {
        // A BUSY hand-back is not a failure - the caller re-asks and it usually succeeds. Logging
        // it printed 564 lines in ONE selfcheck for entirely normal flow control, which is the
        // same "report a characteristic as a fault" mistake already fixed once this session:
        // loud is a budget, and spending it on the expected case is how a real line gets ignored.
        if !msc_last_was_busy() {
            pl011_write(b"dwc2: bot CSW-in failed - "); pl011_write(last_fail_str().as_bytes());
            pl011_write(b"\r\n");
        }
        // A BUSY device needs no recovery - it is working, just occupied, and the caller re-asks.
        // Recovering here meant every busy hand-back issued a Mass Storage Reset plus two clear-halts
        // and bumped the failure streak: 564 of them in one selfcheck, which is what produced 58
        // escalations and 3 port resets of a stick that was never broken. Recovery is for faults.
        if msc_last_was_busy() { note_busy(ep_in, ep_out); } else { recover_or_revive(ep_in, ep_out); }
        return false;
    }
    let sig = u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]);
    let tag = u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]);
    let residue = u32::from_le_bytes([csw[8], csw[9], csw[10], csw[11]]);
    // "USBS", tag echoed, status passed - AND the device moved every byte it was asked to.
    //
    // `dResidue` is how many bytes of the data stage the device did NOT deliver, and it used to be
    // decoded for the error message only, never checked. A device is entitled to return a SHORT data
    // phase with status 0 (BOT case 5/6, and the ordinary degraded behaviour of a flaky stick): 100
    // bytes of a 512-byte read, residue 412, verdict "passed". That was accepted as a good block, and
    // `fs` received 412 bytes of stale zeros PRESENTED AS DATA - silent corruption arriving through
    // the device's verdict instead of the DMA buffer, which is the one thing this driver must never
    // do. A short transfer is now a failed transfer, and a residue mismatch means we are out of step
    // with the device's framing, so it recovers like any other transport fault.
    let short = residue != 0 || moved != dlen;
    let ok = sig == 0x5342_5355 && tag == 0x1234_5678 && csw[12] == 0 && !short;
    // A command that actually worked is the ONLY thing that clears the failure streak.
    // A command that worked is also the only evidence that the device is pausing between requests,
    // so it is what makes a later run of busies mean "stuck" rather than "still the same slow write".
    if ok { MSC_FAIL_STREAK.store(0, Ordering::Relaxed); MSC_BUSY_RUN.store(0, Ordering::Relaxed); }
    if !ok {
        // The transfers all succeeded but the device's verdict did not: report WHAT it said. Signature
        // wrong = we are out of sync with the device's framing; status 1 = the SCSI command itself
        // failed; a nonzero residue says how many bytes of the data stage it did NOT deliver.
        // A PROBE is a command we are allowed to be told "no" to - today, SYNCHRONIZE CACHE on a
        // device that does not implement it. That is a fact about the device, not an anomaly, and
        // dumping six registers for it put a wall of hex in front of the operator's first `ls`. The
        // conclusion is still reported (once, by the caller); only the post-mortem is suppressed.
        if !BOT_PROBE.load(Ordering::Relaxed) {
        pl011_write(b"dwc2: bot CSW bad - sig="); write_hex32(sig);
        pl011_write(b" tag="); write_hex32(tag);
        pl011_write(b" residue="); write_hex32(residue);
        pl011_write(b" status="); write_hex32(csw[12] as u32);
        pl011_write(b"\r\n");
        // Recover ONLY from a transport-level problem: a wrong signature or tag means we are out of
        // sync with the device's framing, and status 2 is a phase error, which explicitly asks for a
        // reset. Status 1 is the device cleanly REFUSING this command (a power-on UNIT ATTENTION, a
        // bad LBA) - the endpoints are fine, so clearing halts there is pointless control traffic that
        // only risks upsetting a healthy device.
        }
        if sig != 0x5342_5355 || tag != 0x1234_5678 || csw[12] >= 2 {
            bot_recover(ep_in, ep_out);
        }
    }
    ok
}

/// Detect a Bulk-Only mass-storage device on the current address, select its config, and prove the bulk
/// path by reading its capacity and block 0 (READ CAPACITY(10) + READ(10)). Returns true if it was one.
fn probe_mass_storage() -> bool {
    // The device's CONTROL (endpoint 0) max-packet, as selected right now by enumeration. It must be
    // captured before the bulk size replaces it: a later control transfer to this device (the endpoint
    // recovery below) has to be framed with EP0's packet size, not the bulk endpoint's.
    MSC_EP0_MPS.store(MPS0.load(Ordering::Relaxed), Ordering::Relaxed);
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
    let mut bulk_mps = 64u16;
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
                let raw = (cfg[i + 4] as u16) | ((cfg[i + 5] as u16) << 8);
                bulk_mps = match raw & 0x07FF { 0 => 64, v => v }; // [10:0] = size; [12:11] = HS mult
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
    pl011_write(b" out ep="); write_hex32(ep_out as u32);
    // The bulk max-packet size and whether this device needs SPLIT transfers decide how a 512-byte
    // block moves: one packet on a high-speed stick (mps 512, direct), or EIGHT split packets on a
    // full-speed one (mps 64) - and a small command reply can succeed while the block read fails.
    pl011_write(b" mps="); write_hex32(bulk_mps as u32);
    pl011_write(b" split_port="); write_hex32(SPLIT_PORT.load(Ordering::Relaxed) as u32);
    pl011_write(b"\r\n");

    // Publish this device's coordinates NOW, before a single command is issued against it.
    //
    // They used to be stored only after the probe fully succeeded, so throughout the probe
    // `MSC_ADDR`/`MSC_EP_*`/`MSC_MPS` still described the PREVIOUS device. `bot_recover` re-points the
    // bus using exactly those, so recovering a probe-time failure aimed at a device that no longer
    // existed - which is why recovery had to be suppressed during a revival. Suppressing it then
    // starved the probe of the recovery it DEPENDS on (its first command is expected to fail on the
    // UNIT ATTENTION below), and every revival enumerated the stick perfectly and then failed every
    // command after it. Setting them here fixes the cause, so recovery is safe to leave enabled.
    MSC_ADDR.store(DEV_ADDR.load(Ordering::Relaxed), Ordering::Relaxed);
    MSC_EP_IN.store(ep_in, Ordering::Relaxed);
    MSC_EP_OUT.store(ep_out, Ordering::Relaxed);
    MSC_MPS.store(bulk_mps, Ordering::Relaxed);

    // Clear the power-on UNIT ATTENTION: a freshly-attached device rejects its first command with CHECK
    // CONDITION until its sense data is drained. Loop TEST UNIT READY / REQUEST SENSE a bounded few times.
    //
    // Marked a PROBE, because being refused here is the expected path, not a fault. Without that this
    // printed a six-register CSW post-mortem on every single boot - a normal protocol handshake dressed
    // up as an anomaly. Noise like that is not free: it teaches a reader that a `CSW bad` line is
    // background, which is exactly the wrong lesson the day one means something.
    BOT_PROBE.store(true, Ordering::Relaxed);
    let ei = ep_in as u32;
    let eo = ep_out as u32;
    for _ in 0..8 {
        if bot_command(ei, eo, &[0u8; 6], false, &mut [], 0, false) { break; } // TEST UNIT READY (0x00)
        let mut sense = [0u8; 18];
        let _ = bot_command(ei, eo, &[0x03, 0, 0, 0, 18, 0], true, &mut sense, 18, false); // REQUEST SENSE clears it
    }

    // READ CAPACITY(10): 8-byte reply = last LBA (BE) + block size (BE).
    let cap_cdb = [0x25u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut cap = [0u8; 8];
    BOT_PROBE.store(false, Ordering::Relaxed);
    if !bot_command(ep_in as u32, ep_out as u32, &cap_cdb, true, &mut cap, 8, false) {
        pl011_write(b"dwc2: msc READ CAPACITY failed\r\n"); return true;
    }
    let last_lba = u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]);
    let bsize = u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]);
    pl011_write(b"dwc2: msc capacity last_lba="); write_hex32(last_lba);
    pl011_write(b" block_size="); write_hex32(bsize); pl011_write(b"\r\n");

    // READ(10) block 0: proves a bulk IN moves real data - and the read a 16 GB SanDisk WEDGES on. It
    // answers READ CAPACITY (an 8-byte IN) fine, then NAKs the 512-byte data IN forever, and then NAKs
    // even a REQUEST SENSE. That last part is the tell: the device is STUCK MID-TRANSPORT, not merely
    // slow - a bare REQUEST SENSE will not clear it. A device left mid-command is resynchronised by a
    // BOT reset (Bulk-Only Mass Storage Reset + clear-halt), which `bot_recover` does, and - the reason
    // it is the right tool here - it also RESETS THE BULK DATA TOGGLES, the prime suspect for why a
    // 512-byte read wedges while an 8-byte one does not (a toggle that is correct for the small reply
    // and wrong for the block). So each failed attempt runs a full recovery, then retries. Bounded
    // (§26.6, 8 attempts). On final failure it reports the channel state so the cause is unambiguous:
    // HCINT (NAK/XactErr/XferCompl) and HCTSIZ.remaining - `remaining == 512` means no data ever moved
    // (the device never sent), `< 512` means a partial/toggle transfer.
    let rd_cdb = [0x28u8, 0, 0, 0, 0, 0, 0, 0, 1, 0];             // READ(10), LBA 0, 1 block
    let mut blk = [0u8; 512];
    BOT_PROBE.store(true, Ordering::Relaxed);
    // ONE read, and wait on the TRUTH (Commandment VIII): the device delivering block 0. A NAK is not a
    // failure - it is the device saying "alive, still fetching, ask again" - so `chan_dma` keeps
    // re-polling the SAME data phase through every NAK and returns the instant it sees XferCompl (data
    // arrived) or a real error (STALL/XactErr = the device refusing or the link breaking). Those are the
    // truths that end the wait; nothing here counts a clock down to a verdict.
    //
    // Do NOT abandon-and-retry: this SanDisk NAKs while it fetches the first block, and walking away
    // after the tiny steady-state budget only leaves it busy with a read we no longer collect (after
    // which it NAKs even a reset). So the ONLY thing raised here is the BACKSTOP that stops the wait
    // being unbounded (§26.6) - lifted to 1.5 s just for this one probe read, for a device that goes
    // truly silent, then restored. A device that is working ends the wait early by delivering; the
    // backstop only catches one that never will.
    IO_NAK_BUDGET_US.store(1_500_000, Ordering::Relaxed);
    let read_ok = bot_command(ep_in as u32, ep_out as u32, &rd_cdb, true, &mut blk, 512, false);
    let last_cause = last_fail_str();
    IO_NAK_BUDGET_US.store(0, Ordering::Relaxed);
    if !read_ok {
        BOT_PROBE.store(false, Ordering::Relaxed);
        pl011_write(b"dwc2: msc READ(10) - device never delivered block 0 (backstop reached); transport: ");
        pl011_write(last_cause.as_bytes());
        pl011_write(b"\r\n");
        return true;
    }
    BOT_PROBE.store(false, Ordering::Relaxed);
    pl011_write(b"dwc2: msc read block0 first4=");
    write_hex32(u32::from_be_bytes([blk[0], blk[1], blk[2], blk[3]]));
    pl011_write(b"\r\n");

    // KEEP the device: everything above proved the path works, and the same coordinates are what a block
    // driver needs to serve real I/O. Only 512-byte-sector media is accepted - the whole storage stack
    // (GSFS, the block IPC protocol) is 512-byte blocks, and silently serving a 4K-sector device as if it
    // were 512 would corrupt it. A device with any other sector size is reported and left unclaimed.
    if bsize != 512 {
        pl011_write(b"dwc2: msc UNSUPPORTED sector size (only 512 is served)\r\n");
        return true;
    }
    // (coordinates were published above, before the first command)
    MSC_SECTORS.store(last_lba as u64 + 1, Ordering::Relaxed); // READ CAPACITY reports the LAST LBA
    // Forget what the PREVIOUS device answered. `MSC_NO_FLUSH` and `MSC_NO_FUA` cache one specific
    // device's "no", and this is the point a different device arrives - so they are facts about a
    // thing that is no longer attached. Left set, a stick that refuses SYNCHRONIZE CACHE would hand
    // its refusal to whatever replaced it, silently denying a capable device the durability it does
    // offer, with the one-shot warning already spent so nothing would ever say so. A derived view
    // must not outlive the source it was derived from (Commandment III).
    // A device that enumerated is evidence of health, so replenish BOTH counters. The revive budget
    // exists to stop a FUTILE loop; spending it on attempts that each produced a working device turns
    // it into a lifetime cap. This device dies roughly every 27 s of sustained I/O, so three successful
    // revivals - about a minute and a half - would otherwise exhaust it and leave storage dead for the
    // rest of the boot despite every recovery having worked. The streak counter already followed this
    // rule ("only a command that worked clears it"); the budget did not.
    MSC_REVIVE_TRIES.store(0, Ordering::Relaxed);
    MSC_FAIL_STREAK.store(0, Ordering::Relaxed);
    MSC_NO_FLUSH.store(false, Ordering::Relaxed);
    MSC_NO_FUA.store(false, Ordering::Relaxed);
    MSC_FUA_LOGGED.store(false, Ordering::Relaxed);
    MSC_READY.store(true, Ordering::Release);
    // Settle durability HERE, at bring-up, rather than on whichever command happens to be the first
    // metadata write. It is a property of the device, so it belongs in the boot log next to its
    // capacity - not interleaved with an operator's `ls`, which is exactly where it was landing.
    let _ = msc_sync_cache();
    pl011_write(b"dwc2: mass storage READY - ");
    super::timer::write_dec_pub(((last_lba as u64 + 1) / 2048) as u32);
    pl011_write(b" MiB, serving block I/O\r\n");
    true
}

// --- USB mass storage as a block device -------------------------------------------------------------
// The probe above leaves the device configured and its coordinates recorded here, so the userspace
// `block-driver` can serve real block I/O from it through the gated syscalls (the same shape as the
// USB-net bridge: the kernel owns the controller, a userspace driver owns the protocol above it).
static MSC_READY:   AtomicBool = AtomicBool::new(false);
static MSC_ADDR:    AtomicU8   = AtomicU8::new(0);   // the device's USB address
static MSC_EP_IN:   AtomicU8   = AtomicU8::new(0);   // bulk IN endpoint
static MSC_EP_OUT:  AtomicU8   = AtomicU8::new(0);   // bulk OUT endpoint
static MSC_MPS:     AtomicU16  = AtomicU16::new(64); // bulk max packet size
static MSC_SECTORS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);

/// Storage and the keyboard no longer share a host channel, so the time-based standoff that used to
/// live here is gone. It existed because both drove channel 0: a keyboard split abandoned when its
/// in-ISR budget expired left that channel dirty, and the next block command failed on its very first
/// transfer. Measured at the time - keyboard attached: 83 selfcheck failures, 235 USB error lines;
/// unplugged: 1 failure, zero errors, same build and same disk. The remedy was to keep the keyboard
/// OFF the channel for 10 ms after every storage command: a timer standing in for isolation the
/// hardware already provides. This controller has 8 host channels and we were using one. CH_BULK and
/// CH_KBD are separate now, so an abandoned poll damages only its own channel and nothing takes turns.



/// Capacity of the attached USB mass-storage device in 512-byte sectors (0 = none attached).
pub fn msc_sectors() -> u64 {
    if !MSC_READY.load(Ordering::Acquire) { return 0; }
    MSC_SECTORS.load(Ordering::Relaxed)
}

/// Point the shared host channel at the mass-storage device. The single DWC2 channel is time-shared by
/// every device (keyboard, net, storage), so each path re-selects its own before transacting.
fn msc_select() {
    select_device(MSC_ADDR.load(Ordering::Relaxed), MSC_MPS.load(Ordering::Relaxed), false);
    SPLIT_PORT.store(MSC_HUB_PORT.load(Ordering::Relaxed), Ordering::Relaxed);
}
static MSC_HUB_PORT: AtomicU8 = AtomicU8::new(0); // hub port for split transfers (0 = direct/high-speed)

/// Name a refusal that used to be silent.
///
/// The block entry points below refuse a request for four different reasons (not ready, wrong core,
/// short buffer, LBA past the end) through one indistinguishable `false` - and a hardware run produced
/// exactly that signature: `fs` reporting I/O errors at specific LBAs while dwc2, which logs every
/// TRANSPORT failure, said nothing at all, because the failure was a refusal that never reached the
/// transport. Ruling out each silent gate took a log-deduction session that one printed word would have
/// avoided. A refusal is a defined answer, but an unnameable failure is still a silent one
/// (Invariant 12) - so each gate now says which it was. Rate-limited per reason (first, then every
/// 64th) so a client that retries in a loop cannot flood the console.
fn msc_refuse(reason: &str, counter: &AtomicU32, lba: u64) -> bool {
    let n = counter.fetch_add(1, Ordering::Relaxed);
    if n % 64 == 0 {
        pl011_write(b"dwc2: block request refused - ");
        pl011_write(reason.as_bytes());
        pl011_write(b" (lba ");
        super::timer::write_dec_pub(lba as u32);
        pl011_write(b", occurrence ");
        super::timer::write_dec_pub(n + 1);
        pl011_write(b")
");
    }
    false
}
static MSC_REFUSE_NOT_READY: AtomicU32 = AtomicU32::new(0);
static MSC_REFUSE_WRONG_CORE: AtomicU32 = AtomicU32::new(0);
static MSC_REFUSE_RANGE: AtomicU32 = AtomicU32::new(0);

/// Read one 512-byte block from the USB mass-storage device into `dst`. Returns false if there is no
/// device, the LBA is past the end, or the transfer failed. Core-0 only (the single DWC2 poller), and
/// non-blocking like the net path - see the DMA soundness invariant on `DMA`.
pub fn msc_read_block(lba: u64, dst: &mut [u8]) -> bool {
    if !MSC_READY.load(Ordering::Acquire) { return msc_refuse("not ready (mid-revival?)", &MSC_REFUSE_NOT_READY, lba); }
    if !on_core0() { return msc_refuse("wrong core", &MSC_REFUSE_WRONG_CORE, lba); }
    if dst.len() < 512 { return false; }
    if lba >= MSC_SECTORS.load(Ordering::Relaxed) { return msc_refuse("LBA past capacity", &MSC_REFUSE_RANGE, lba); }
    msc_select();
    // Wait on the truth (the transfer completing), not a 5 ms clock: a slow stick NAKs while it fetches,
    // and abandoning + re-issuing wedges it (see IO_BUDGET_US). Auto-restored on return.
    let _budget = NakBudget::raised(IO_BUDGET_US);
    let l = lba as u32;
    // READ(10): opcode 0x28, LBA big-endian at [2..6], transfer length big-endian at [7..9].
    let cdb = [0x28u8, 0, (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8, 0, 0, 1, 0];
    let ok = bot_command(MSC_EP_IN.load(Ordering::Relaxed) as u32, MSC_EP_OUT.load(Ordering::Relaxed) as u32,
                         &cdb, true, &mut dst[..512], 512, true);  // async read (stage 2a): park, ISR wakes
    ok
}

/// Whether to ask for FUA at all. **Off**, and the reason is measured, not assumed.
///
/// The drive honours the bit - that was never in doubt after it accepted it and kept working for 31
/// seconds. The problem is what honouring it COSTS: a write that waits for the medium leaves the drive
/// busy afterwards, and it NAKs the next command while it finishes programming. A NAK is normal USB
/// flow control meaning "busy, ask again", but this driver gives a command only `BOT_TRIES` x
/// `HALT_BUDGET_US` = 20 ms before calling it an error, and a flash program outlasts that. The result
/// was 17 `bot CBW-out failed` and 18 failed tests, against 1 with the bit off.
///
/// Those budgets are not arbitrary either: a block write runs in a syscall with IRQs masked, so the
/// time it spends waiting is time the timer tick does not run. Buying FUA by simply waiting longer
/// trades data durability for scheduler responsiveness, which is a real trade and not one to make
/// silently. The honest resolution is to treat NAK as "busy, retry" rather than an error and to wait
/// for it WITHOUT holding the core - which is an async block path, not a constant.
///
/// **ON again (2026-07-27), because that prerequisite was BUILT.** A NAKed command now comes back as
/// BUSY (`USB_DISK_BUSY`, distinct from failure), the kernel holds the core only `CORE_HOLD_US` per
/// attempt, and `block-driver` re-asks from userspace, yielding between attempts - the wait costs
/// latency, not the tick. So the measured cost that turned FUA off (a post-write NAK burning the 20 ms
/// command budget into an error) no longer exists; a drive programming flash is simply busy for a
/// while, which is now a first-class answer. What re-enabling BUYS was demonstrated the same day this
/// flag flipped: a 20-round carnage storm forced two port resets, each printing "writes it had
/// BUFFERED are LOST: a port reset clears the device cache", and the second lost the ROOT record -
/// tree unreadable, reformat required. This device refuses SYNCHRONIZE CACHE (`MSC_NO_FLUSH`), so FUA
/// is its ONLY durability mechanism: with it, every acknowledged write is on the medium before the
/// ack, and a revival's port reset has nothing buffered to lose. `MSC_NO_FUA` still guards the device
/// that rejects the bit - such a device falls back to plain writes, loudly, once.
const USE_FUA: bool = true;

/// Set once the device has rejected a WRITE(10) carrying FUA, after which writes go out plain.
/// See `msc_write_block` - this is the fallback that keeps a device writable when it will not take
/// the bit, rather than leaving the filesystem unable to write at all.
static MSC_NO_FUA: AtomicBool = AtomicBool::new(false);
/// One-shot latch so the FUA-accepted line is stated once, not per write.
static MSC_FUA_LOGGED: AtomicBool = AtomicBool::new(false);

/// Write one 512-byte block to the USB mass-storage device. Same constraints as `msc_read_block`.
///
/// **FUA is currently OFF** (`USE_FUA`, above, records the measurement that turned it off), so the
/// FUA branch and its fallback below are inert. What follows describes what the bit is FOR, and is
/// written in the present tense deliberately - it is the standing rationale for re-enabling, not a
/// description of today's behaviour.
///
/// Issued with **FUA** (Force Unit Access) where the device accepts it, which asks the drive to put
/// this block on the medium before reporting completion instead of parking it in a volatile buffer.
/// That matters because this stick refuses SYNCHRONIZE CACHE, leaving no barrier at all: a redo
/// journal is nothing but ordering (staged blocks durable before the commit record, that record
/// durable before any home block moves), and without one the device may land the journal's
/// invalidation before the checkpoint - losing the data AND the means to replay it. Observed as a
/// root directory that failed its CRC after every power cycle. FUA per write restores the ordering
/// from below: if each write is durable when acknowledged, the sequence is durable in order.
///
/// **Acceptance is not proof.** Some devices take the bit and ignore it, and nothing on the host can
/// tell. So this claims only what it knows - the device did not reject it - and the real evidence is
/// a power cycle with the tree still readable afterwards.
pub fn msc_write_block(lba: u64, src: &[u8]) -> bool {
    if !MSC_READY.load(Ordering::Acquire) { return msc_refuse("not ready (mid-revival?)", &MSC_REFUSE_NOT_READY, lba); }
    if !on_core0() { return msc_refuse("wrong core", &MSC_REFUSE_WRONG_CORE, lba); }
    if src.len() < 512 { return false; }
    if lba >= MSC_SECTORS.load(Ordering::Relaxed) { return msc_refuse("LBA past capacity", &MSC_REFUSE_RANGE, lba); }
    msc_select();
    // Same patience as the read: let ONE write finish (the device NAKs its CSW while programming flash,
    // more so under FUA) rather than abandoning it after 5 ms and re-issuing, which wedges a slow stick
    // and used to trip the false "endpoint stuck" reset that failed the format. Auto-restored on return.
    let _budget = NakBudget::raised(IO_BUDGET_US);
    let l = lba as u32;
    let mut buf = [0u8; 512];
    let fua = USE_FUA && !MSC_NO_FUA.load(Ordering::Relaxed);
    // WRITE(10): opcode 0x2A, same big-endian LBA/length layout as READ(10). Byte 1 bit 3 = FUA.
    let cdb = |fua: bool| [0x2Au8, if fua { 0x08 } else { 0 },
                           (l >> 24) as u8, (l >> 16) as u8, (l >> 8) as u8, l as u8, 0, 0, 1, 0];
    buf.copy_from_slice(&src[..512]);
    let ep_in = MSC_EP_IN.load(Ordering::Relaxed) as u32;
    let ep_out = MSC_EP_OUT.load(Ordering::Relaxed) as u32;
    let mut ok = bot_command(ep_in, ep_out, &cdb(fua), false, &mut buf, 512, false);
    if ok && fua && !MSC_FUA_LOGGED.swap(true, Ordering::Relaxed) {
        // State the regime once, positively. Inferring it from the ABSENCE of a rejection message
        // makes a log reader guess, and the whole point of this experiment is to know which of the
        // two durability regimes the machine is actually running in.
        pl011_write(b"dwc2: device accepted FUA writes - each write reported durable on completion\r\n");
    }
    if !ok && fua {
        // A failed FUA write is NOT evidence the device rejects the bit - and assuming it was cost the
        // machine its durability. The first version latched `MSC_NO_FUA` on ANY failure here, with a
        // comment admitting "we cannot tell from a CSW status alone"; hardware then showed the latch
        // flipping at the exact onset of a busy-stuck episode (`busy with no pause for 200` on the very
        // next log line). The device had never objected to FUA - a transport wobble was misread as a
        // rejection, durability silently downgraded to plain writes, and the NEXT revival's port reset
        // lost the buffered root record: the precise corruption FUA had just been enabled to prevent.
        // The same shape as every regression this branch has recorded: a capability withheld on an
        // assumption instead of a verdict.
        //
        // We CAN tell - by asking. Two verdicts, each from evidence:
        // - BUSY: flow control, nothing to diagnose. Return busy; the caller re-asks, FUA stays on.
        // - Otherwise, REQUEST SENSE: the device's own stated reason for the CHECK CONDITION. Sense
        //   key 5 (ILLEGAL REQUEST) is "I do not take that CDB" - THAT is a FUA rejection, and only
        //   that latches the plain-write fallback (loudly: the machine changes durability regime).
        //   Any other sense key is an ordinary I/O problem on a FUA-capable device: report the
        //   failure, let the normal retry/recovery machinery act, and keep FUA armed. This is the
        //   discrimination Linux's usb-storage makes (auto-sense, then a decision keyed on what the
        //   device SAID), reimplemented for this driver.
        if msc_last_was_busy() { return false; }
        let mut sense = [0u8; 18];
        let sensed = bot_command(ep_in, ep_out, &[0x03, 0, 0, 0, 18, 0], true, &mut sense, 18, false);
        if sensed && (sense[2] & 0x0F) == 0x05 {
            MSC_NO_FUA.store(true, Ordering::Relaxed);
            pl011_write(b"dwc2: device rejected FUA (ILLEGAL REQUEST) - falling back to plain writes (not durable on ack)\r\n");
            buf.copy_from_slice(&src[..512]);
            ok = bot_command(ep_in, ep_out, &cdb(false), false, &mut buf, 512, false);
        }
        // Sense unavailable or a non-ILLEGAL key: a real I/O failure, already reported by bot_command's
        // own paths. FUA stays on; the retry that follows recovery re-asks with the bit set.
    }
    ok
}

/// Are writes going out with FUA (so each is durable when acknowledged)? Used by `msc_sync_cache` to
/// answer a durability request truthfully without a bus round-trip.
pub fn msc_writes_are_fua() -> bool {
    USE_FUA && MSC_READY.load(Ordering::Acquire) && !MSC_NO_FUA.load(Ordering::Relaxed)
}

/// Flush the device's internal write cache to the medium (SCSI SYNCHRONIZE CACHE (10), opcode 0x35).
///
/// A USB mass-storage device completes a WRITE(10) as soon as it has the data in its own buffer - the
/// bytes need not be on flash yet. Every write this driver issues was therefore only *acknowledged*,
/// not *durable*, and a reset or power cut before the device flushed lost it. That is not theoretical:
/// it destroyed the root directory block of a freshly formatted disk twice, because `format` writes the
/// root last, so it was still the most likely thing sitting in the device's buffer when the Pi was
/// power-cycled seconds later. The superblock, written first, always survived - which is exactly the
/// signature of a lost tail of writes rather than a bad block.
///
/// It also decides whether the crash-consistency journal means anything on this medium. A redo journal
/// rests on the commit record reaching the disk BEFORE the blocks it authorises; a device free to hold
/// and reorder both in a volatile cache voids "replayed or discarded, never torn". Durability has to be
/// asked for explicitly, so callers ask - `fs` at the points where it promises it (§26.5).
///
/// No data phase. Bounded like every other transfer; a device that refuses it reports `false` rather
/// than silently pretending the data is safe (invariant 12).
pub fn msc_sync_cache() -> bool {
    if !MSC_READY.load(Ordering::Acquire) || !on_core0() { return false; }
    // Ask a device at most once. This stick answers SYNCHRONIZE CACHE with CHECK CONDITION - it does
    // not implement the command - and a device that refuses once will refuse forever, so continuing to
    // send it bought nothing and cost a full bulk round-trip per journal barrier: 166 of them in a
    // single selfcheck. Latching the refusal keeps the bus for work that can succeed. The caller still
    // gets `false` and still reports the lost guarantee once (§26.7) - we stop retrying, not telling.
    if MSC_NO_FLUSH.load(Ordering::Relaxed) { return false; }
    // If every write went out with FUA it was already on the medium when it completed, so "make prior
    // writes durable" is satisfied before it is asked and costs no round-trip. This is why the caller
    // stops warning once FUA is in use: the guarantee is being met by a different mechanism, not
    // quietly dropped. (Bounded by what the device honours - see `msc_write_block`.)
    if msc_writes_are_fua() { return true; }
    msc_select();
    // SYNCHRONIZE CACHE (10): opcode 0x35; LBA 0 + length 0 means "the whole medium".
    let cdb = [0x35u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut none = [0u8; 0];
    BOT_PROBE.store(true, Ordering::Relaxed);
    let ok = bot_command(MSC_EP_IN.load(Ordering::Relaxed) as u32, MSC_EP_OUT.load(Ordering::Relaxed) as u32,
                         &cdb, false, &mut none, 0, false);
    BOT_PROBE.store(false, Ordering::Relaxed);
    if !ok {
        MSC_NO_FLUSH.store(true, Ordering::Relaxed);
        pl011_write(b"dwc2: device does not support SYNCHRONIZE CACHE - writes cannot be confirmed durable\r\n");
    }
    ok
}

/// Set once the device has refused SYNCHRONIZE CACHE, so it is never asked again (see `msc_sync_cache`).
/// **Cleared on enumeration** (`probe_mass_storage`), because it is a fact about ONE device: a later
/// stick may well honour the command. An earlier version of this comment justified never clearing it
/// with "a fresh device re-enumerates through `msc_select`" - which was simply wrong, `msc_select` only
/// re-points the channel and clears nothing.
static MSC_NO_FLUSH: AtomicBool = AtomicBool::new(false);

/// The keyboard's host-side decode state: the previous report's keycodes (for N-key edge detection),
/// the Caps Lock latch (the HID modifier byte never carries it), and the typematic auto-repeat timer.
/// One keyboard, one owner: touched only by `poll()` on core 0 (the single DWC2 poller).
struct KbdState {
    last: [u8; 6],
    caps: bool,
    rep: super::hid::KeyRepeat,
    /// Consecutive polls that did not reach the keyboard (timeout / transaction error, as opposed to a
    /// clean NAK). While this is nonzero our view of which keys are down is stale, so auto-repeat is
    /// held back; past a few in a row it is disarmed outright.
    unhealthy: u8,
}
static mut KBD_STATE: KbdState =
    KbdState { last: [0; 6], caps: false, rep: super::hid::KeyRepeat::new(), unhealthy: 0 };

/// Called from the Core-0 timer tick. Once a keyboard is configured, run one interrupt IN transaction; on
/// a completed transfer decode the boot report into console bytes. A NAK (no key change) returns quietly.
pub fn poll() {
    // The keyboard's periodic poll gets its OWN channel. It is the one transfer deliberately abandoned
    // when its in-ISR budget expires, and on a shared channel that abandonment was inherited by
    // whatever ran next.
    let ch = CH_KBD;
    if !KBD_READY.load(Ordering::Acquire) { return; }
    // Stand off the shared host channel while storage is using it (see `MSC_LAST_USE_US`). Polling an
    // idle keyboard means starting a SPLIT through the hub's transaction translator and abandoning it
    // when the in-ISR budget expires - harmless on its own, but it leaves the TT holding a transaction
    // and the next block command fails on its first transfer. A skipped poll costs nothing: the device
    // queues its reports, so the keystroke arrives on the next one.
    // Point the shared channel at the keyboard (the net device may have selected itself last).
    select_device(KBD_ADDR.load(Ordering::Relaxed), KBD_MPS.load(Ordering::Relaxed) as u16,
                  KBD_LOW.load(Ordering::Relaxed));
    // A low/full-speed keyboard behind the high-speed hub is reached only via SPLIT (like enumeration).
    SPLIT_PORT.store(KBD_HUB_PORT.load(Ordering::Relaxed), Ordering::Relaxed);
    let ep = KBD_EP.load(Ordering::Relaxed) as u32;
    let toggle = KBD_TOGGLE.load(Ordering::Relaxed);
    let pid = if toggle { PID_DATA1 } else { PID_DATA0 };
    // SAFETY: KBD_DMA is touched only on core 0; addr_of gives its identity-mapped physical address.
    // The keyboard has its OWN buffer (not the shared `DMA`) so its transfer never aliases storage/net.
    unsafe {
        let kd = &mut *core::ptr::addr_of_mut!(KBD_DMA);
        let data_phys = core::ptr::addr_of!(kd.report) as u32;
        // One interrupt IN, up to 8 bytes. Tight bound: this runs in the core-0 timer ISR.
        flush_dcache(data_phys, 8);                          // invalidate before the device writes
        let hcsplt = hcsplt_for_current();
        let ci = if hcsplt != 0 {
            split_txn(ch, true, pid, 8, data_phys, ep, 3, hcsplt, true) // split IN, tight ISR bound
        } else {
            chan_program(ch, true, pid, 8, data_phys, ep, 3, 0);
            poll_wait_halt(ch)
        };
        // SAFETY: KBD_STATE is touched only here, only on core 0 (the single DWC2 poller); addr_of
        // avoids taking a reference to the mutable static.
        let ks = &mut *core::ptr::addr_of_mut!(KBD_STATE);
        if ci & HCINT_XFERCOMPL == 0 {
            // No new report. A HELD key sends NOTHING (a boot keyboard reports only on change), so
            // typematic auto-repeat MUST be driven from here - the tick where nothing arrived.
            //
            // But ONLY on a clean NAK, which is the device positively saying "nothing has changed".
            // A timeout or transaction error means we did not reach the keyboard at all, so we do not
            // know whether the key is still down - and if the report we missed was the key's RELEASE,
            // repeating would spew characters the user never typed until the next keypress. That is
            // exactly what appeared once a USB stick joined the bus: block I/O shares this single host
            // channel, keyboard polls started failing, and held keys ran on. Several unreachable polls
            // in a row mean we have lost track of the keyboard entirely - disarm rather than guess.
            if ci & HCINT_NAK != 0 {
                ks.unhealthy = 0;
                ks.rep.poll(super::console_push_byte);
            } else {
                ks.unhealthy = ks.unhealthy.saturating_add(1);
                if ks.unhealthy > 3 { ks.rep.disarm(); }
            }
            return;
        }
        ks.unhealthy = 0;
        flush_dcache(data_phys, 8);                          // invalidate after -> read device bytes
        let mut report = [0u8; 8];
        report.copy_from_slice(&kd.report[..8]);
        KBD_TOGGLE.store(!toggle, Ordering::Relaxed);            // advance the data toggle on a real packet
        // Ctrl+Alt+Del: SIGNAL it on the console stream; the shell (which holds REBOOT) decides (§6.4).
        if super::hid::is_ctrl_alt_del(&report) {
            super::console_push_byte(super::hid::CTRL_ALT_DEL_SIGNAL);
            return;
        }
        super::hid::decode_keyboard(&report, &mut ks.last, &mut ks.rep, &mut ks.caps,
                                    super::console_push_byte);
        // Also service repeat on a report tick, so a held key keeps repeating while another is tapped.
        ks.rep.poll(super::console_push_byte);
    }
}
