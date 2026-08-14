//! Core bring-up: soft reset, host mode, FIFO sizing, root-port power and reset.
//!
//! Slice 1a of the port (`docs/arm32-usb-userspace.md`). This is `dwc2::init()`'s first half, moved
//! out of ring 0 and rewritten against the SDK's safe `Mmio` wrapper - so the service reaches its
//! registers with no `unsafe` anywhere in `services/` (§18.2), where the kernel version needed a
//! raw-pointer `read_volatile`/`write_volatile` pair.
//!
//! The sequence itself is NOT re-derived. It is the one that works on this silicon, and several of
//! its steps exist because of failures diagnosed on the board rather than anything a datasheet says.
//! Those comments travel with the code; a "cleaner" rewrite that dropped them would be a rewrite of
//! the hard-won part.

use godspeed_sdk::{Mmio, ServiceContext};

use crate::regs::*;

/// Wait for a register condition, bounded by the CLOCK.
///
/// Bounded by time rather than by an iteration count, because a count means a different duration on
/// every board and on this project it has been wrong seven times (`docs/xhci-completion-correlation.md`
/// records the last). The kernel version spins on iteration counts (`waited > 1_000_000`) - fine
/// there, where it ran once at boot on one board; not fine in a service that must behave the same
/// under load as idle.
///
/// Returns false on timeout, and the caller REPORTS it. A hardware wait that quietly gives up is the
/// silent-failure case invariant 12 exists to prevent.
fn wait_until(ctx: &ServiceContext, mmio: &Mmio, off: usize, mask: u32, want_set: bool, ms: u64) -> bool {
    let deadline = ctx.read_tsc().wrapping_add(ctx.duration_cycles(ms));
    loop {
        if ((mmio.read32(off) & mask) != 0) == want_set {
            return true;
        }
        if ctx.read_tsc().wrapping_sub(deadline) < (1u64 << 63) {
            return false;
        }
    }
}

/// Identify the controller. `None` if this is not a DesignWare OTG core.
///
/// Reported loudly and treated as fatal by the caller: a driver that cannot find its controller must
/// say so rather than proceed to program whatever is at that address (invariant 12).
pub fn identify(ctx: &ServiceContext, mmio: &Mmio) -> Option<u32> {
    let id = mmio.read32(GSNPSID);
    // The Synopsys OTG core IDs read "OT2"/"OT3" in the high half (0x4F54_xxxx).
    if (id & 0xFFFF_F000) != 0x4F54_2000 && (id & 0xFFFF_F000) != 0x4F54_3000 {
        ctx.log_fmt(format_args!(
            "dwc2-svc: no DesignWare core at the granted window (GSNPSID={:#010x}) - USB unavailable", id));
        return None;
    }
    ctx.log_fmt(format_args!("dwc2-svc: DesignWare USB 2.0 OTG core, GSNPSID={:#010x}", id));
    Some(id)
}

/// Reset the core and bring it up in host mode with the BCM2835 FIFO layout.
///
/// Returns false if any step timed out. Each timeout is reported individually, because "the core did
/// not reset" and "the core never entered host mode" are different faults with different causes, and
/// a single "init failed" would merge them.
pub fn reset_and_host_mode(ctx: &ServiceContext, mmio: &Mmio) -> bool {
    let mut ok = true;

    // 1. Mask + disable global interrupts while we reset.
    mmio.write32(GAHBCFG, mmio.read32(GAHBCFG) & !GAHBCFG_GLBLINTRMSK);
    mmio.write32(GINTMSK, 0);

    // 2. Wait for the AHB master to go idle before a core reset - resetting mid-transfer wedges the core.
    if !wait_until(ctx, mmio, GRSTCTL, GRSTCTL_AHBIDLE, true, 500) {
        ctx.log("dwc2-svc: WARN AHB not idle before reset");
        ok = false;
    }

    // 2b. (Pi) Before the reset, clear the ULPI external-VBUS-drive and TermSel-DLine-pulse bits,
    //     matching Circle's working BCM2836 init. Harmless on the UTMI+ PHY; QEMU ignores them.
    mmio.write32(GUSBCFG, mmio.read32(GUSBCFG) & !(GUSBCFG_ULPI_EXT_VBUS | GUSBCFG_TERM_SEL_DL));

    // 3. Core soft reset: sets defaults and clears the FIFOs. Self-clears when done.
    mmio.write32(GRSTCTL, mmio.read32(GRSTCTL) | GRSTCTL_CSFTRST);
    if !wait_until(ctx, mmio, GRSTCTL, GRSTCTL_CSFTRST, false, 500) {
        ctx.log("dwc2-svc: WARN core soft reset did not clear");
        ok = false;
    }
    // Let the PHY settle after reset.
    ctx.sleep(ctx.duration_cycles(10));

    // 4. Select the PHY interface + force HOST mode (Circle's working Pi 2 sequence).
    //
    //    On the real BCM2836 the UTMI+ 8-bit interface MUST be selected (clear ULPI_UTMI_SEL + PHYIF)
    //    and OTG HNP/SRP disabled, or the PHY never clocks a transaction: the channel arms, ChEna
    //    stays set, and the DMA master never starts (AHBIdle stuck at 1). That was diagnosed on
    //    silicon. QEMU ignores all of it, so it only matters on the real board - which is exactly the
    //    kind of step a rewrite drops because everything still passes in emulation.
    //
    //    The core samples ForceHstMode ~25 ms after the write, so wait for CurMode=host.
    let mut cfg = mmio.read32(GUSBCFG);
    cfg &= !(GUSBCFG_ULPI_UTMI_SEL | GUSBCFG_PHYIF | GUSBCFG_SRP_CAPABLE | GUSBCFG_HNP_CAPABLE);
    cfg &= !GUSBCFG_FRCDEVMODE;
    cfg |= GUSBCFG_FRCHSTMODE;
    mmio.write32(GUSBCFG, cfg);
    if !wait_until(ctx, mmio, GINTSTS, GINTSTS_CURMODE_HOST, true, 200) {
        ctx.log("dwc2-svc: WARN did not enter host mode");
        ok = false;
    }

    // 5. Ungate the PHY/port clocks (PCGCCTL=0 releases stop-pclk + gate-hclk).
    mmio.write32(PCGCCTL, 0);
    // 5a. (u-boot host_init) A pure host must not have host-set-HNP enabled.
    mmio.write32(GOTGCTL, mmio.read32(GOTGCTL) & !GOTGCTL_HSTSETHNPEN);

    // 5b. Size the FIFOs to the Linux BCM2835 host-mode layout (`params_bcm2835` in
    //     drivers/usb/dwc2/params.c): RX 774, non-periodic TX 256, periodic TX 512, laid end to end.
    //
    //     Not arbitrary, and not tunable by taste. The previous values (RX 256, NPTX 128, PTX 128)
    //     were sized for a keyboard's 8-byte reports. A high-speed bulk packet is 512 bytes = 128
    //     words, so the whole non-periodic TX FIFO held exactly ONE packet: under sustained bulk I/O
    //     the RX FIFO could not buffer the next packet while the DMA drained the last, the host
    //     channel wedged, and the device presented as "stopped answering EP0".
    const RX_WORDS: u32 = 774;
    const NPTX_WORDS: u32 = 256;
    const PTX_WORDS: u32 = 512;
    let dfifo_depth = mmio.read32(GHWCFG3) >> 16;
    ctx.log_fmt(format_args!(
        "dwc2-svc: DFIFO depth {} words; sizing RX/NPTX/PTX 774/256/512 (Linux bcm2835)", dfifo_depth));

    // What can this core SCHEDULE for itself?
    //
    // The keyboard costs real CPU because software sequences every split transaction by hand: spin to
    // a microframe boundary, arm the channel, spin until it halts, repeat for the complete-split. The
    // only way the controller can do that itself - and interrupt once, on completion - is DESCRIPTOR
    // (scatter/gather) DMA, where it walks a per-microframe descriptor list without software in the
    // loop. Whether this silicon has it is one bit, and the whole design of an interrupt-driven
    // periodic path rests on it, so read it and say so rather than assuming in either direction.
    //
    // The precedent says expect NO: the Raspberry Pi's own dwc_otg driver solves this exact problem
    // with an FIQ state machine (`dwc_otg.fiq_fsm_enable`), which is what you build when the core
    // cannot schedule splits by itself. If that is what this reports, an interrupt-driven periodic
    // path is not a matter of programming the controller differently - it needs a different mechanism.
    let hwcfg4 = mmio.read32(GHWCFG4);
    let arch = (mmio.read32(GHWCFG2) >> GHWCFG2_ARCH_SHIFT) & GHWCFG2_ARCH_MASK;
    ctx.log_fmt(format_args!(
        "dwc2-svc: GHWCFG2={:#010x} GHWCFG4={:#010x} - arch {} ({}), descriptor DMA {}{}",
        mmio.read32(GHWCFG2), hwcfg4, arch,
        match arch { 0 => "slave-only", 1 => "external DMA", 2 => "internal DMA", _ => "reserved" },
        if hwcfg4 & GHWCFG4_DESC_DMA != 0 { "SUPPORTED" } else { "NOT supported" },
        if hwcfg4 & GHWCFG4_DESC_DMA_DYN != 0 { " (dynamic)" } else { "" }));
    if dfifo_depth != 0 && dfifo_depth < RX_WORDS + NPTX_WORDS + PTX_WORDS {
        ctx.log("dwc2-svc: WARN DFIFO too small for the bcm2835 layout - USB may be unstable under load");
        ok = false;
    }
    mmio.write32(GRXFSIZ, RX_WORDS);
    mmio.write32(GNPTXFSIZ, (NPTX_WORDS << 16) | RX_WORDS);
    mmio.write32(HPTXFSIZ, (PTX_WORDS << 16) | (RX_WORDS + NPTX_WORDS));

    // 5b'. Flush every TX FIFO and the RX FIFO so their internal pointers match the boundaries just
    //      programmed. The soft reset set pointers for the DEFAULT layout, and resizing leaves them
    //      stale. In DMA mode the core DMAs the SETUP packet INTO the NP TX FIFO itself, so a stale
    //      pointer makes that write silently stall: the channel arms but never transacts (ChEna set,
    //      HCINT=0, zero bytes moved - diagnosed on the Pi 2). Flush only while the AHB master is idle.
    if !wait_until(ctx, mmio, GRSTCTL, GRSTCTL_AHBIDLE, true, 500) {
        ctx.log("dwc2-svc: WARN AHB not idle before FIFO flush");
        ok = false;
    }
    mmio.write32(GRSTCTL, GRSTCTL_TXFNUM_ALL | GRSTCTL_TXFFLSH);
    if !wait_until(ctx, mmio, GRSTCTL, GRSTCTL_TXFFLSH, false, 200) {
        ctx.log("dwc2-svc: WARN TX FIFO flush did not clear");
        ok = false;
    }
    mmio.write32(GRSTCTL, GRSTCTL_RXFFLSH);
    if !wait_until(ctx, mmio, GRSTCTL, GRSTCTL_RXFFLSH, false, 200) {
        ctx.log("dwc2-svc: WARN RX FIFO flush did not clear");
        ok = false;
    }

    // 6. DMA mode. The BCM2836 core is internal-DMA capable and this driver depends on it: the CPU
    //    never touches a FIFO, which is why only one page of registers is mapped and the data FIFO
    //    window at 0x1000 is deliberately absent from the grant.
    mmio.write32(GAHBCFG, mmio.read32(GAHBCFG) | GAHBCFG_DMAEN);

    ok
}

/// Power the root port and reset whatever is attached. Returns the settled HPRT, or `None` on timeout.
pub fn port_bring_up(ctx: &ServiceContext, mmio: &Mmio) -> Option<u32> {
    // HPRT has write-1-to-clear bits mixed in with control bits, so every write must mask the W1C
    // set off first or reading-modifying-writing silently acknowledges events. This is the register
    // that punishes a naive read-modify-write.
    let w1c = HPRT_PRTCONNDET | HPRT_PRTENA | HPRT_PRTENCHNG | HPRT_PRTOVRCHNG;

    let hprt = mmio.read32(HPRT);
    if hprt & HPRT_PRTPWR == 0 {
        mmio.write32(HPRT, (hprt & !w1c) | HPRT_PRTPWR);
        // The port needs time to settle after power before a device is detectable.
        ctx.sleep(ctx.duration_cycles(100));
    }

    let hprt = mmio.read32(HPRT);
    if hprt & HPRT_PRTCONNSTS == 0 {
        ctx.log("dwc2-svc: root port powered, nothing connected");
        return Some(hprt);
    }

    // Drive reset for 60 ms - USB 2.0 requires at least 10 ms, and the Pi's hub wants the longer end.
    mmio.write32(HPRT, (mmio.read32(HPRT) & !w1c) | HPRT_PRTRST);
    ctx.sleep(ctx.duration_cycles(60));
    mmio.write32(HPRT, mmio.read32(HPRT) & !w1c & !HPRT_PRTRST);
    // The core enables the port itself after reset; give it a moment to report the speed.
    ctx.sleep(ctx.duration_cycles(30));

    let hprt = mmio.read32(HPRT);
    if hprt & HPRT_PRTENA == 0 {
        ctx.log_fmt(format_args!(
            "dwc2-svc: port did not enable after reset (HPRT={:#010x})", hprt));
        return None;
    }
    Some(hprt)
}

/// Human-readable speed from HPRT, for the boot line.
pub fn speed_name(hprt: u32) -> &'static str {
    match (hprt >> HPRT_PRTSPD_SHIFT) & 3 {
        0 => "high",
        1 => "full",
        2 => "low",
        _ => "unknown",
    }
}
