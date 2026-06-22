// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Memory-mapped I/O access for userspace driver services (§12, §18).
//!
//! This is the SDK's audited hardware-access layer - one of the two places
//! outside the kernel where `unsafe` is permitted (the other being the syscall
//! ABI, `raw_syscall`). Driver services use the safe [`Mmio`] wrapper and never
//! write `unsafe` themselves; every volatile access below carries a SAFETY
//! argument, and `Mmio` is only constructable inside this crate (from a
//! kernel-granted mapping), so its base pointer is always valid by construction.

/// A mapped MMIO region granted to a driver (e.g. via
/// [`crate::ServiceContext::xhci_mmio`]). Read/write device registers by byte
/// offset. All accesses are volatile - never reordered or elided - and target
/// the uncached device registers directly, with no kernel mediation (§12).
#[derive(Clone, Copy)]
pub struct Mmio {
    base: *mut u8,
}

impl Mmio {
    /// Wrap a kernel-granted MMIO base virtual address. Crate-internal: only the
    /// SDK constructs an `Mmio`, and only from a VA the kernel mapped for this
    /// driver, which is what makes the volatile accesses below sound.
    pub(crate) fn new(base: *mut u8) -> Self {
        Self { base }
    }

    /// Read an 8-bit register at `off` bytes from the base.
    #[inline]
    pub fn read8(&self, off: usize) -> u8 {
        // SAFETY: `base` is a valid kernel-granted MMIO mapping (Mmio is only
        // constructed from one); `off` is within the driver's mapped window.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }

    /// Read a 16-bit register at `off` (must be 2-byte aligned).
    #[inline]
    pub fn read16(&self, off: usize) -> u16 {
        // SAFETY: as `read8`; aligned 16-bit access within the mapped region.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }

    /// Read a 32-bit register at `off` (must be 4-byte aligned).
    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        // SAFETY: as `read8`; aligned 32-bit access within the mapped region.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }

    /// Write an 8-bit register at `off`.
    #[inline]
    pub fn write8(&self, off: usize, val: u8) {
        // SAFETY: as `read8`; volatile device-register write.
        unsafe { core::ptr::write_volatile(self.base.add(off), val) }
    }

    /// Write a 16-bit register at `off` (must be 2-byte aligned).
    #[inline]
    pub fn write16(&self, off: usize, val: u16) {
        // SAFETY: as `read8`; aligned 16-bit write.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }

    /// Write a 32-bit register at `off` (must be 4-byte aligned).
    #[inline]
    pub fn write32(&self, off: usize, val: u32) {
        // SAFETY: as `read8`; aligned 32-bit write.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }

    /// Read a 64-bit register at `off` (must be 8-byte aligned). For the
    /// controller's 64-bit registers (DCBAAP, CRCR, ERSTBA, ERDP).
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        // SAFETY: as `read8`; aligned 64-bit access within the mapped region.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u64) }
    }

    /// Write a 64-bit register at `off` (must be 8-byte aligned).
    #[inline]
    pub fn write64(&self, off: usize, val: u64) {
        // SAFETY: as `read8`; aligned 64-bit write.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u64, val) }
    }
}
