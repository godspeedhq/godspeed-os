// SPDX-License-Identifier: Apache-2.0
//! A driver's REGISTERS: the window onto its device that the kernel granted at spawn.
//!
//! # Why this exists
//!
//! A driver must name its register window and rely on it, so the window is part of how a Godspeed
//! driver is written, and comes through `gs` (`docs/driver-library.md`, step 3). Until 2026-10-19 every
//! driver reached past the standard library for it, with the SDK's `ctx.mmio()`; this is the same grant,
//! in the shape the promise covers.
//!
//! # What it is, and is not
//!
//! The registers of the device the spawn request named, and nothing else: the kernel resolved the device
//! by its class and mapped exactly its window (CLAUDE.md 12.3, 14.1). [`registers`] answers `None` when no
//! window was granted - a service that is not a driver, or a device that is not there - and a driver says
//! so and parks rather than guessing.
//!
//! Every access is checked: an offset past the window, or one not aligned to the access's size, is
//! refused loudly and ends the driver alone (the SDK's audited layer does the access, CLAUDE.md 18.1). A
//! register's MEANING - its offset, its bits, the order a device wants them written in - is the
//! device's, and stays in its driver.
//!
//! ```ignore
//! let Some(regs) = gs::driver::registers(&ctx) else {
//!     ctx.log("mydev: no device was granted - nothing to drive");
//!     gs::ipc::park(&ctx)
//! };
//! let id = regs.read32(0x00);
//! regs.write32(0x04, 1);
//! ```

#[cfg(not(test))]
use godspeed_sdk::mmio::Mmio;
#[cfg(not(test))]
use godspeed_sdk::service_context::ServiceContext;

/// The register window the kernel granted this driver. Copy it freely: it is a view of the one window,
/// and every copy reaches the same registers.
#[cfg(not(test))]
#[derive(Clone, Copy)]
pub struct Registers {
    mmio: Mmio,
}

/// This driver's register window, or `None` when the spawn granted none.
#[cfg(not(test))]
pub fn registers(ctx: &ServiceContext) -> Option<Registers> {
    ctx.mmio().map(|mmio| Registers { mmio })
}

#[cfg(not(test))]
impl Registers {
    /// The window's length in bytes: the last register is below it.
    pub fn len(&self) -> usize {
        self.mmio.len()
    }

    /// Read the 8-bit register at `off` bytes into the window.
    pub fn read8(&self, off: usize) -> u8 {
        self.mmio.read8(off)
    }

    /// Read the 16-bit register at `off`, which must be 2-byte aligned.
    pub fn read16(&self, off: usize) -> u16 {
        self.mmio.read16(off)
    }

    /// Read the 32-bit register at `off`, which must be 4-byte aligned.
    pub fn read32(&self, off: usize) -> u32 {
        self.mmio.read32(off)
    }

    /// Read the 64-bit register at `off`, which must be 8-byte aligned.
    pub fn read64(&self, off: usize) -> u64 {
        self.mmio.read64(off)
    }

    /// Write the 8-bit register at `off`.
    pub fn write8(&self, off: usize, val: u8) {
        self.mmio.write8(off, val)
    }

    /// Write the 16-bit register at `off`, which must be 2-byte aligned.
    pub fn write16(&self, off: usize, val: u16) {
        self.mmio.write16(off, val)
    }

    /// Write the 32-bit register at `off`, which must be 4-byte aligned.
    pub fn write32(&self, off: usize, val: u32) {
        self.mmio.write32(off, val)
    }

    /// Write the 64-bit register at `off`, which must be 8-byte aligned.
    pub fn write64(&self, off: usize, val: u64) {
        self.mmio.write64(off, val)
    }
}
