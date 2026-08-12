//! DWC2 register offsets and bit definitions, for the Raspberry Pi 2's USB host controller.
//!
//! Lifted VERBATIM from `kernel/src/arch/arm/dwc2.rs`, comments included. The comments are not
//! decoration: several of them record facts diagnosed on real silicon that no datasheet states
//! (which PHY bits the BCM2836 needs, why the FIFO layout is the Linux `bcm2835` one, which bits
//! QEMU ignores). Retyping these from a datasheet would lose exactly the knowledge that took the
//! longest to acquire, so they are copied rather than rewritten.

#![allow(dead_code)] // the full register map lands here; the slices consume it in order

// --- Global core registers (offsets from DWC2_BASE) ---
pub(crate) const GOTGCTL:  usize = 0x000; // OTG control + status
pub(crate) const GAHBCFG:  usize = 0x008; // AHB config (DMA enable, global int enable)
pub(crate) const GUSBCFG:  usize = 0x00C; // USB config (force host/device mode, PHY select)
pub(crate) const GRSTCTL:  usize = 0x010; // reset control (core soft reset, AHB idle)
pub(crate) const GINTSTS:  usize = 0x014; // core interrupt status
pub(crate) const GINTMSK:  usize = 0x018; // core interrupt mask
pub(crate) const GNPTXSTS: usize = 0x02C; // non-periodic TX FIFO/queue status (low 16 = words free): did the SSPLIT drain?
pub(crate) const GRXFSIZ:  usize = 0x024; // receive FIFO size
pub(crate) const GNPTXFSIZ:usize = 0x028; // non-periodic transmit FIFO size
pub(crate) const GSNPSID:  usize = 0x040; // Synopsys core ID ("OT2" + release, e.g. 0x4F54_294A)
pub(crate) const GHWCFG2:  usize = 0x048; // hardware config 2 (architecture, HS PHY type)
pub(crate) const GHWCFG3:  usize = 0x04C; // hardware config 3 (bits 31:16 = total DFIFO depth in 32-bit words)
pub(crate) const HPTXFSIZ: usize = 0x100; // host periodic transmit FIFO size
// --- Host-mode registers ---
pub(crate) const HCFG:     usize = 0x400; // host config (PHY clock select)
pub(crate) const HPRT:     usize = 0x440; // host port control + status (root port)
pub(crate) const HFIR:     usize = 0x404; // host frame interval (Circle writes 48000 for a full-speed host)
pub(crate) const HFNUM:    usize = 0x408; // host frame number (low 16) + frame remaining (high 16)
pub(crate) const HCCHAR_ODDFRM: u32 = 1 << 29;
pub(crate) const HCCHAR0:  usize = 0x500; // channel characteristics (ep, dir, addr, type, enable)
pub(crate) const HCSPLT0:  usize = 0x504; // channel split control (0 = no split transaction)
pub(crate) const HCINT0:   usize = 0x508; // channel interrupt status
pub(crate) const HCINTMSK0:usize = 0x50C; // channel interrupt mask
pub(crate) const HCTSIZ0:  usize = 0x510; // transfer size (bytes, packet count, PID)
pub(crate) const HCDMA0:   usize = 0x514; // channel DMA address (physical buffer)
// The cfg gate was lost in the mechanical lift and is restored here, because it is not a build
// convenience - it is the difference between working and stalling. QEMU's DWC2 takes a plain
// physical address; the real BCM2836 needs the VideoCore bus alias, and without it the DATA stage
// STALLs. Getting this wrong fails on exactly one of the two targets, which is the worst kind.
#[cfg(feature = "qemu")]
pub(crate) const DMA_BUS_ALIAS: u32 = 0x0000_0000;
#[cfg(not(feature = "qemu"))]
pub(crate) const DMA_BUS_ALIAS: u32 = 0xC000_0000;
pub(crate) const CH_BULK: u32 = 0;
pub(crate) const CH_KBD:  u32 = 1;
pub(crate) const CH_NET:  u32 = 2;
pub(crate) const CH_NET_RX: u32 = 3;
pub(crate) const HAINT:    usize = 0x414; // host all-channels interrupt
pub(crate) const HAINTMSK: usize = 0x418; // host all-channels interrupt mask
// --- Power / clock gating ---
pub(crate) const PCGCCTL:  usize = 0xE00; // power + clock gating control
pub(crate) const GRSTCTL_CSFTRST: u32 = 1 << 0;  // core soft reset (self-clearing)
pub(crate) const GRSTCTL_RXFFLSH: u32 = 1 << 4;  // RX FIFO flush (self-clearing)
pub(crate) const GRSTCTL_TXFFLSH: u32 = 1 << 5;  // TX FIFO flush (self-clearing)
pub(crate) const GRSTCTL_TXFNUM_ALL: u32 = 0x10 << 6; // TxFNum=0x10 flushes ALL TX FIFOs
pub(crate) const GRSTCTL_AHBIDLE: u32 = 1 << 31; // AHB master idle
pub(crate) const GOTGCTL_HSTSETHNPEN: u32 = 1 << 10;
pub(crate) const GAHBCFG_GLBLINTRMSK: u32 = 1 << 0; // global interrupt enable
pub(crate) const GAHBCFG_DMAEN:       u32 = 1 << 5; // DMA mode enable
pub(crate) const GUSBCFG_PHYIF:         u32 = 1 << 3;  // UTMI+ data width: 0 = 8-bit (Pi), 1 = 16-bit
pub(crate) const GUSBCFG_ULPI_UTMI_SEL: u32 = 1 << 4;  // PHY interface: 0 = UTMI+ (Pi), 1 = ULPI
pub(crate) const GUSBCFG_PHYSEL:     u32 = 1 << 6;  // 1 = full-speed serial PHY, 0 = USB 2.0 HS PHY (UTMI+)
pub(crate) const GUSBCFG_SRP_CAPABLE:   u32 = 1 << 8;  // OTG SRP - off for a pure host
pub(crate) const GUSBCFG_HNP_CAPABLE:   u32 = 1 << 9;  // OTG HNP - off for a pure host
pub(crate) const GUSBCFG_ULPI_EXT_VBUS: u32 = 1 << 20; // drive VBUS externally (ULPI) - off
pub(crate) const GUSBCFG_TERM_SEL_DL:   u32 = 1 << 22; // TermSel DLine pulsing - off
pub(crate) const GUSBCFG_FRCHSTMODE: u32 = 1 << 29; // force host mode
pub(crate) const GUSBCFG_FRCDEVMODE: u32 = 1 << 30; // force device mode
pub(crate) const GINTSTS_CURMODE_HOST: u32 = 1 << 0; // current mode: 1 = host
pub(crate) const HPRT_PRTCONNSTS: u32 = 1 << 0;  // device connected
pub(crate) const HPRT_PRTCONNDET: u32 = 1 << 1;  // connect detected (W1C)
pub(crate) const HPRT_PRTENA:     u32 = 1 << 2;  // port enabled (set by hardware after reset)
pub(crate) const HPRT_PRTENCHNG:  u32 = 1 << 3;  // enable changed (W1C)
pub(crate) const HPRT_PRTOVRCURR: u32 = 1 << 4;  // overcurrent active
pub(crate) const HPRT_PRTOVRCHNG: u32 = 1 << 5;  // overcurrent changed (W1C)
pub(crate) const HPRT_PRTRST:     u32 = 1 << 8;  // port reset
pub(crate) const HPRT_PRTPWR:     u32 = 1 << 12; // port power
pub(crate) const HPRT_PRTSPD_SHIFT: u32 = 17;    // port speed (0=HS, 1=FS, 2=LS)
pub(crate) const HPRT_PRTSPD_MASK:  u32 = 0b11 << HPRT_PRTSPD_SHIFT;
pub(crate) const HPRT_WC_BITS: u32 = HPRT_PRTCONNDET | HPRT_PRTENCHNG | HPRT_PRTOVRCHNG;
pub(crate) const HPRT_RMW_CLEAR: u32 = HPRT_WC_BITS | HPRT_PRTENA;
