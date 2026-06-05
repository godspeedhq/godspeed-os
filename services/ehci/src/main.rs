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

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // E1a scaffold: prove the service builds, spawns, and runs. The kernel-granted
    // MMIO capability is wired in E1b; until then there is nothing to drive.
    ctx.log("ehci: driver starting (E1a scaffold — awaiting MMIO grant)");
    ctx.park();
}
