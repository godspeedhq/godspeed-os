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
    ///
    /// **Deliberately not `write_bytes`.** That lowers to `memset`, and a `memset` is free to reach
    /// for whatever the ISA makes fastest - unaligned stores, cache-line zeroing hints (AArch64's
    /// `DC ZVA`), non-temporal stores. Every one of those is legal on the Normal cacheable memory
    /// x86 maps this arena as, and every one of them is a **fault** on the Device memory a
    /// non-coherent port maps it as instead (`DMA_ARENA_UNCACHED`, the SEC-28 answer): Device-nGnRnE
    /// forbids unaligned access outright and cache maintenance by address on it is not meaningful.
    ///
    /// So the clear is an explicit aligned 64-bit volatile loop, which is the one shape that is
    /// correct on both. The arena is page-based (a whole number of 4 KiB frames at a page-aligned
    /// VA), so the fast path always applies; the byte tail exists only so this function is total
    /// rather than silently leaving a remainder, and it cannot run on a real grant.
    ///
    /// `volatile` matters as much as the alignment: an ordinary store loop over Device memory is
    /// something the optimiser may merge back into a `memset` call, which would reintroduce exactly
    /// what this avoids.
    pub fn zero(&self) {
        let words = self.len / 8;
        for i in 0..words {
            // SAFETY: base..base+len is the kernel-granted mapped arena (Dma is only constructed
            // from one) and is page-aligned, so `base + i*8` is 8-byte aligned and in bounds for
            // every i < len/8.
            unsafe { core::ptr::write_volatile(self.base.add(i * 8) as *mut u64, 0) }
        }
        for off in words * 8..self.len {
            // SAFETY: off < len, so this is in bounds; a byte store needs no alignment.
            unsafe { core::ptr::write_volatile(self.base.add(off), 0) }
        }
    }

    // ---------------------------------------------------------------------------------------------
    // ALIGNMENT
    //
    // The multi-byte accessors below compose from BYTE accesses whenever the offset is not naturally
    // aligned. That is not defensiveness; on a non-coherent arch this arena is mapped **Device**
    // memory (AArch64 Device-nGnRnE, §SEC-28 - the mapping is what removes the need for cache
    // maintenance), and Device memory does not permit unaligned access AT ALL. An unaligned 16- or
    // 32-bit load there is not slow, it is an ALIGNMENT FAULT.
    //
    // The doc comments used to say "2-byte aligned" / "4-byte aligned" and leave it at that, which
    // put the obligation on every caller. Drivers cannot honour it: parsing a USB configuration
    // descriptor means walking to offsets the DEVICE chose, so `wMaxPacketSize` lands at an odd
    // address whenever the descriptors before it summed to an odd length. The Pi 4 found this the
    // first time the xhci service reached a real hub:
    //
    //     ESR_EL1 = 0x92000021 (data abort, DFSC 0b100001 = alignment fault)
    //     FAR_EL1 = 0x200005003          <- arena base + 0x5003, an ODD offset
    //     task    = xhci
    //
    // x86 hides this completely: unaligned loads just work there, so every such call site is correct
    // on the arch it was written for and faults on the arch it was ported to. Putting the fix HERE
    // rather than at the call sites means it cannot be forgotten by the next driver.
    //
    // Aligned offsets keep the single wide volatile access, so nothing on the fast path pays for it.

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
        if off & 1 != 0 {
            return u16::from_le_bytes([self.read8(off), self.read8(off + 1)]);
        }
        // SAFETY: as read8; aligned 16-bit access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }

    /// Read a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn read32(&self, off: usize) -> u32 {
        self.check(off, 4);
        if off & 3 != 0 {
            return u32::from_le_bytes([
                self.read8(off), self.read8(off + 1), self.read8(off + 2), self.read8(off + 3),
            ]);
        }
        // SAFETY: base is a valid kernel-granted mapping; check() bounded off in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }

    /// Write a 32-bit value at byte offset `off` (4-byte aligned, `off < len`).
    #[inline]
    pub fn write32(&self, off: usize, val: u32) {
        self.check(off, 4);
        if off & 3 != 0 {
            let b = val.to_le_bytes();
            for (k, v) in b.iter().enumerate() { self.write8(off + k, *v); }
            return;
        }
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
        if off & 1 != 0 {
            let b = val.to_le_bytes();
            self.write8(off, b[0]);
            self.write8(off + 1, b[1]);
            return;
        }
        // SAFETY: as read16; volatile 16-bit write in range.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }

    /// Read a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn read64(&self, off: usize) -> u64 {
        self.check(off, 8);
        if off & 7 != 0 {
            return (self.read32(off) as u64) | ((self.read32(off + 4) as u64) << 32);
        }
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u64) }
    }

    /// Write a 64-bit value at byte offset `off` (8-byte aligned, `off < len`).
    #[inline]
    pub fn write64(&self, off: usize, val: u64) {
        self.check(off, 8);
        if off & 7 != 0 {
            self.write32(off, val as u32);
            self.write32(off + 4, (val >> 32) as u32);
            return;
        }
        // SAFETY: as read32; 64-bit aligned access in range.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u64, val) }
    }
}
