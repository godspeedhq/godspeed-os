// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Physical frame type and address arithmetic — §10.

/// A 4 KiB physical frame. Owning a `Frame` means the kernel has allocated
/// that physical page; dropping it without returning it to the allocator leaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Frame(PhysAddr);

/// A raw physical address (not necessarily page-aligned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysAddr(pub u64);

pub const FRAME_SIZE: u64 = 4096;

impl Frame {
    /// Construct a `Frame` from a page-aligned physical address.
    ///
    /// # Safety
    /// `addr` must be page-aligned and refer to physical memory that the
    /// allocator has granted to the caller.
    pub unsafe fn from_phys(addr: PhysAddr) -> Self {
        debug_assert!(addr.0 % FRAME_SIZE == 0, "frame address must be page-aligned");
        Self(addr)
    }

    pub fn phys_addr(self) -> PhysAddr {
        self.0
    }

    pub fn frame_number(self) -> u64 {
        self.0.0 / FRAME_SIZE
    }
}

impl PhysAddr {
    pub fn align_down(self) -> Self {
        Self(self.0 & !(FRAME_SIZE - 1))
    }

    pub fn is_aligned(self) -> bool {
        self.0 % FRAME_SIZE == 0
    }
}
