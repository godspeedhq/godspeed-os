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

    // E2 will reset + run the controller from here. For now, the controller is
    // only observed; park so the core can idle.
    ctx.park();
}
