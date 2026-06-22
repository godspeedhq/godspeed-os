// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

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

    /// Physical address of byte offset `off` within the arena.
    #[inline]
    pub fn phys_at(&self, off: usize) -> u64 {
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
        // SAFETY: base is a valid kernel-granted mapping; caller keeps off in range.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }

    /// Read a 16-bit value at byte offset `off` (2-byte aligned, `off < len`).
    #[inline]
    pub fn read16(&self, off: usize) -> u16 {
        // SAFETY: as read8; aligned 16-bit access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }

    /// Read a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        // SAFETY: base is a valid kernel-granted mapping; caller keeps off in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }

    /// Write a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn write32(&self, off: usize, val: u32) {
        // SAFETY: as read32; volatile so the device observes ordered writes.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }

    /// Read a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u64) }
    }

    /// Write a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn write64(&self, off: usize, val: u64) {
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u64, val) }
    }
}
