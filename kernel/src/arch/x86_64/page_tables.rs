//! x86_64 four-level page table management — §10.
//!
//! Each task gets its own CR3. The kernel region is identity-mapped into every
//! address space so syscall entry/exit doesn't require a CR3 switch.
//! User regions are private per task.

use crate::memory::frame::{Frame, PhysAddr};

pub const PAGE_SIZE: usize = 4096;

/// A physical address holding the PML4 root of one address space.
pub struct PageTable {
    root: Frame,
}

impl PageTable {
    /// Allocate a new empty page table with the kernel region pre-mapped.
    pub fn new() -> Result<Self, MapError> {
        todo!("allocate PML4 frame, copy kernel mappings from the reference table")
    }

    /// Map `virt` → `phys` with the given flags in this address space.
    pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        todo!("walk/allocate PT levels, set entry")
    }

    /// Unmap `virt` and return the physical frame it pointed to.
    pub fn unmap(&mut self, virt: VirtAddr) -> Result<Frame, MapError> {
        todo!("clear PTE, return frame — caller issues TLB shootdown (§10.5)")
    }

    /// Physical address of PML4 root for loading into CR3.
    pub fn cr3_value(&self) -> u64 {
        self.root.phys_addr().0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(pub u64);

bitflags::bitflags! {
    pub struct PageFlags: u64 {
        const PRESENT   = 1 << 0;
        const WRITABLE  = 1 << 1;
        const USER      = 1 << 2;
        const NO_EXEC   = 1 << 63;
    }
}

#[derive(Debug)]
pub enum MapError {
    FrameAllocFailed,
    AlreadyMapped,
    NotMapped,
}
