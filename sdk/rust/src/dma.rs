// SPDX-License-Identifier: Apache-2.0
//! DMA arena access for userspace driver services (§12, §18).
//!
//! Part of the SDK's audited hardware/ABI layer (§18.1), alongside `mmio.rs` and
//! the syscall ABI. A [`Dma`] wraps a kernel-granted, physically-contiguous
//! arena: the driver builds device structures in it (via the read/write
//! helpers) and hands the controller physical addresses (via [`Dma::phys_at`]).
//! Driver services use this safe wrapper and never write `unsafe` themselves;
//! `Dma` is only constructable inside this crate, from a kernel-granted region.

/// A physically-contiguous DMA arena granted to a driver (e.g. via
/// [`crate::ServiceContext::dma_region`]). The CPU accesses it through `base`
/// (a normal cacheable mapping - x86 DMA is cache-coherent); the device through
/// `phys`. Both views cover the same `len` bytes one-to-one.
///
/// SEC-28 (SMP-port contract, `kernel/src/arch/CLAUDE.md`): this cacheable, no-maintenance mapping
/// assumes x86 DMA coherence. On a non-coherent arch (AArch64) a port must add cache maintenance here
/// (clean before a device read of a CPU-written buffer; invalidate before a CPU read of a device-written
/// one) or map the arena non-cacheable - else the CPU and the device can see stale copies.
#[derive(Clone, Copy)]
pub struct Dma {
    base: *mut u8,
    phys: u64,
    len: usize,
}

impl Dma {
    /// Crate-internal: only the SDK constructs a `Dma`, from a kernel-granted
    /// region, which is what makes the volatile accesses below sound.
    pub(crate) fn new(base: *mut u8, phys: u64, len: usize) -> Self {
        Self { base, phys, len }
    }

    /// Physical base address - program this (plus offsets) into the controller.
    #[inline]
    pub fn phys_base(&self) -> u64 {
        self.phys
    }

    /// Length of the arena in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Bounds-check an `off..off+size` access against the arena length (SEC-4). A driver bug that
    /// lets a device-supplied value drive `off` past the arena is caught here as a loud panic
    /// (killing only the one driver) instead of a silent out-of-arena CPU access - which, for an
    /// IOMMU-passthrough driver, is a write ANYWHERE in RAM (§26.7). `checked_add` so a wrapping
    /// `off` cannot slip past. The `unsafe` accessors below are only actually memory-safe with this.
    #[inline]
    fn check(&self, off: usize, size: usize) {
        assert!(
            off.checked_add(size).map_or(false, |end| end <= self.len),
            "Dma access out of arena bounds",
        );
    }

    /// Physical address of byte offset `off` within the arena.
    #[inline]
    pub fn phys_at(&self, off: usize) -> u64 {
        self.check(off, 0);
        self.phys + off as u64
    }

    /// Zero the whole arena.
    pub fn zero(&self) {
        // SAFETY: base..base+len is the kernel-granted mapped arena (Dma is only
        // constructed from one); zeroing across it is in-bounds.
        unsafe { core::ptr::write_bytes(self.base, 0, self.len) }
    }

    /// Read an 8-bit value at byte offset `off` (`off < len`).
    #[inline]
    pub fn read8(&self, off: usize) -> u8 {
        self.check(off, 1);
        // SAFETY: base is a valid kernel-granted mapping; check() bounded off in range.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }

    /// Read a 16-bit value at byte offset `off` (2-byte aligned, `off < len`).
    #[inline]
    pub fn read16(&self, off: usize) -> u16 {
        self.check(off, 2);
        // SAFETY: as read8; aligned 16-bit access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }

    /// Read a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        self.check(off, 4);
        // SAFETY: base is a valid kernel-granted mapping; check() bounded off in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }

    /// Write a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn write32(&self, off: usize, val: u32) {
        self.check(off, 4);
        // SAFETY: as read32; volatile so the device observes ordered writes.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }

    /// Write an 8-bit value at byte offset `off` (`off < len`). For byte-granular
    /// device structures (e.g. an e1000 TX descriptor's command byte, or frame bytes).
    #[inline]
    pub fn write8(&self, off: usize, val: u8) {
        self.check(off, 1);
        // SAFETY: as read8; volatile so the device observes ordered writes.
        unsafe { core::ptr::write_volatile(self.base.add(off), val) }
    }

    /// Write a 16-bit value at byte offset `off` (2-byte aligned, `off < len`).
    #[inline]
    pub fn write16(&self, off: usize, val: u16) {
        self.check(off, 2);
        // SAFETY: as read16; volatile 16-bit write in range.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }

    /// Read a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        self.check(off, 8);
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u64) }
    }

    /// Write a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn write64(&self, off: usize, val: u64) {
        self.check(off, 8);
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u64, val) }
    }
}
