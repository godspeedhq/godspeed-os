//! x86_64 four-level page table management — §10.
//!
//! Each task gets its own CR3.  The kernel region (PML4 entries 256–511) is
//! copied from the Limine-set-up PML4 into every new address space so that
//! syscall entry/exit never needs a CR3 switch.
//!
//! Physical frames are accessed via Limine's higher-half direct map (HHDM):
//!   virtual = HHDM_OFFSET + physical
//! `set_hhdm_offset` must be called during memory::init before any PageTable
//! is created.

use crate::memory::allocator::alloc_frame;
use crate::memory::frame::{Frame, PhysAddr};

pub const PAGE_SIZE: usize = 4096;

// ---------------------------------------------------------------------------
// HHDM offset — set once during memory::init, read-only after.
// ---------------------------------------------------------------------------

static mut HHDM_OFFSET: u64 = 0;

/// Read the HHDM offset set during memory init.
///
/// # Safety
/// Returns 0 if called before `set_hhdm_offset`.
#[inline]
pub fn get_hhdm_offset() -> u64 {
    // SAFETY: written once before any caller can observe it; read-only after.
    unsafe { HHDM_OFFSET }
}

/// Store the HHDM base address provided by Limine.
///
/// # Safety
/// Must be called exactly once, by the BSP, before any `PageTable` is created.
pub unsafe fn set_hhdm_offset(offset: u64) {
    // SAFETY: single-threaded init; no concurrent readers yet.
    unsafe { HHDM_OFFSET = offset };
}

/// Convert a physical address to a kernel-accessible virtual pointer.
///
/// # Safety
/// `phys` must be within the physical address range covered by the HHDM.
#[inline]
unsafe fn phys_to_virt(phys: u64) -> *mut u64 {
    // SAFETY: HHDM_OFFSET set during init; caller validates phys.
    (unsafe { HHDM_OFFSET } + phys) as *mut u64
}

// ---------------------------------------------------------------------------
// Page-table entry helpers.
// ---------------------------------------------------------------------------

/// Extract the physical address stored in a non-zero page-table entry.
#[inline]
fn entry_phys(entry: u64) -> u64 {
    entry & 0x000F_FFFF_FFFF_F000
}

#[inline]
fn entry_present(entry: u64) -> bool {
    entry & PageFlags::PRESENT.bits() != 0
}

/// Read the entry at `index` from the table whose root is at `table_phys`.
///
/// # Safety
/// `table_phys` must point to a valid, HHDM-accessible page-table page.
#[inline]
unsafe fn read_entry(table_phys: u64, index: usize) -> u64 {
    // SAFETY: caller guarantees table_phys is valid.
    unsafe { phys_to_virt(table_phys).add(index).read_volatile() }
}

/// Write `value` to `index` in the table at `table_phys`.
///
/// # Safety
/// `table_phys` must point to a valid, HHDM-accessible page-table page.
#[inline]
unsafe fn write_entry(table_phys: u64, index: usize, value: u64) {
    // SAFETY: caller guarantees table_phys is valid.
    unsafe { phys_to_virt(table_phys).add(index).write_volatile(value) }
}

// ---------------------------------------------------------------------------
// Virtual address index extraction.
// ---------------------------------------------------------------------------

#[inline] fn pml4_idx(va: u64) -> usize { ((va >> 39) & 0x1FF) as usize }
#[inline] fn pdpt_idx(va: u64) -> usize { ((va >> 30) & 0x1FF) as usize }
#[inline] fn pd_idx  (va: u64) -> usize { ((va >> 21) & 0x1FF) as usize }
#[inline] fn pt_idx  (va: u64) -> usize { ((va >> 12) & 0x1FF) as usize }

// ---------------------------------------------------------------------------
// PageTable.
// ---------------------------------------------------------------------------

/// A physical address holding the PML4 root of one address space.
pub struct PageTable {
    root: Frame,
}

impl PageTable {
    /// Allocate a new page table and copy the kernel region from the current
    /// address space so that kernel code remains reachable after a CR3 load.
    pub fn new() -> Result<Self, MapError> {
        let root = alloc_frame().ok_or(MapError::FrameAllocFailed)?;
        let root_phys = root.phys_addr().0;

        // Zero the PML4.
        // SAFETY: root_phys from allocator → valid frame; HHDM initialised.
        unsafe {
            let ptr = phys_to_virt(root_phys);
            for i in 0..512 {
                ptr.add(i).write_volatile(0);
            }
        }

        // Copy the kernel half (entries 256–511) from the active PML4 so the
        // new address space can run kernel code immediately.
        // SAFETY: CR3 always valid after boot; HHDM covers the active PML4.
        unsafe {
            let cr3: u64;
            core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
            let current_pml4 = cr3 & !0xFFF;

            for i in 256..512 {
                let entry = read_entry(current_pml4, i);
                write_entry(root_phys, i, entry);
            }
        }

        Ok(Self { root })
    }

    /// Map the 4 KiB page at `virt` to the physical frame at `phys`.
    pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        let va = virt.0;
        let root_phys = self.root.phys_addr().0;

        // Walk/allocate each level, returning the physical address of the next
        // table.  Intermediate entries are P+W (user flag set if VA is user).
        let user_bit = if va < (1u64 << 47) { PageFlags::USER.bits() } else { 0 };
        let inter_flags = PageFlags::PRESENT.bits() | PageFlags::WRITABLE.bits() | user_bit;

        let pdpt_phys = walk_or_alloc(root_phys, pml4_idx(va), inter_flags)?;
        let pd_phys   = walk_or_alloc(pdpt_phys, pdpt_idx(va), inter_flags)?;
        let pt_phys   = walk_or_alloc(pd_phys,   pd_idx(va),   inter_flags)?;

        let idx = pt_idx(va);
        // SAFETY: pt_phys from allocator/walk → valid; HHDM covers it.
        let existing = unsafe { read_entry(pt_phys, idx) };
        if entry_present(existing) {
            return Err(MapError::AlreadyMapped);
        }

        let pte = (phys.0 & !0xFFF) | flags.bits();
        // SAFETY: pt_phys valid, idx in 0..512.
        unsafe { write_entry(pt_phys, idx, pte) };
        Ok(())
    }

    /// Unmap the page at `virt` and return the physical frame it pointed to.
    /// Caller must issue a TLB shootdown before reusing the frame (§10.5).
    pub fn unmap(&mut self, virt: VirtAddr) -> Result<Frame, MapError> {
        let va = virt.0;
        let root_phys = self.root.phys_addr().0;

        let pdpt_phys = walk(root_phys, pml4_idx(va)).ok_or(MapError::NotMapped)?;
        let pd_phys   = walk(pdpt_phys, pdpt_idx(va)).ok_or(MapError::NotMapped)?;
        let pt_phys   = walk(pd_phys,   pd_idx(va))  .ok_or(MapError::NotMapped)?;

        let idx = pt_idx(va);
        // SAFETY: pt_phys valid, idx in 0..512.
        let pte = unsafe { read_entry(pt_phys, idx) };
        if !entry_present(pte) {
            return Err(MapError::NotMapped);
        }

        // SAFETY: clearing present bit; caller responsible for TLB shootdown.
        unsafe { write_entry(pt_phys, idx, 0) };

        let frame_phys = PhysAddr(entry_phys(pte));
        // SAFETY: frame was mapped → previously allocated from the allocator.
        Ok(unsafe { Frame::from_phys(frame_phys) })
    }

    /// Physical address of PML4 root for loading into CR3.
    pub fn cr3_value(&self) -> u64 {
        self.root.phys_addr().0
    }

    /// Consume this `PageTable` and return its raw CR3 value.
    ///
    /// The caller takes ownership of all allocated page-table frames and is
    /// responsible for freeing them at task death (§10.5).  `Frame` has no
    /// `Drop` impl, so the frames remain allocated after this call.
    pub fn into_cr3(self) -> u64 {
        let cr3 = self.root.phys_addr().0;
        // SAFETY: PageTable has no Drop impl and Frame has no Drop impl, so
        // forgetting self is a no-op at the allocator level.  The frames
        // remain allocated and are now owned by whoever loaded them into CR3.
        core::mem::forget(self);
        cr3
    }
}

/// Add a single 4 KiB mapping to the CURRENTLY ACTIVE page table (i.e. the
/// one pointed to by CR3), walking or allocating intermediate levels as needed.
///
/// Designed for boot-time kernel MMIO mappings (e.g. APIC) that must exist
/// in Limine's tables before our per-task `PageTable`s are created.  Because
/// `PageTable::new()` copies PML4 entries 256–511, any mapping added here at
/// a kernel virtual address automatically propagates to future address spaces.
///
/// If the target PTE is already present this is a no-op (returns `Ok`).
///
/// `flags` — raw PTE flag bits (e.g. PRESENT | WRITABLE | PCD | PWT for MMIO).
///
/// # Safety
/// Must be called after `set_hhdm_offset`; `virt` and `phys` must be
/// page-aligned; no TLB flush is issued (caller must invalidate if needed).
pub unsafe fn map_in_active_tables(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    let cr3: u64;
    // SAFETY: RDMSR of CR3 is always valid in ring 0.
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem)) };
    let pml4 = cr3 & !0xFFF;

    let user_bit = if virt < (1u64 << 47) { PageFlags::USER.bits() } else { 0 };
    let inter    = PageFlags::PRESENT.bits() | PageFlags::WRITABLE.bits() | user_bit;

    // SAFETY: pml4 is the live CR3-referenced table; HHDM lets us write entries.
    unsafe {
        let pdpt = walk_or_alloc(pml4,  pml4_idx(virt), inter)?;
        let pd   = walk_or_alloc(pdpt,  pdpt_idx(virt), inter)?;
        let pt   = walk_or_alloc(pd,    pd_idx(virt),   inter)?;
        let idx  = pt_idx(virt);

        let existing = read_entry(pt, idx);
        if !entry_present(existing) {
            write_entry(pt, idx, (phys & !0xFFF) | flags);
        }
        Ok(())
    }
}

/// Follow an existing entry in `table_phys` at `idx`.
/// Returns `Some(child_phys)` if present, `None` if absent.
fn walk(table_phys: u64, idx: usize) -> Option<u64> {
    // SAFETY: table_phys from a chain that started with a valid page table.
    let entry = unsafe { read_entry(table_phys, idx) };
    if entry_present(entry) { Some(entry_phys(entry)) } else { None }
}

/// Follow an existing entry or allocate a new child table.
fn walk_or_alloc(table_phys: u64, idx: usize, flags: u64) -> Result<u64, MapError> {
    // SAFETY: table_phys valid (caller chain guarantees it).
    let entry = unsafe { read_entry(table_phys, idx) };
    if entry_present(entry) {
        return Ok(entry_phys(entry));
    }

    let child = alloc_frame().ok_or(MapError::FrameAllocFailed)?;
    let child_phys = child.phys_addr().0;

    // Zero the new child table.
    // SAFETY: child_phys fresh from allocator; HHDM covers it.
    unsafe {
        let ptr = phys_to_virt(child_phys);
        for i in 0..512 {
            ptr.add(i).write_volatile(0);
        }
    }

    // Write the entry into the parent table.
    // SAFETY: table_phys valid, idx in 0..512.
    unsafe { write_entry(table_phys, idx, child_phys | flags) };

    // Leak the frame intentionally — page table frames are owned by the
    // PageTable and freed when the whole table is torn down (Milestone 5).
    core::mem::forget(child);
    Ok(child_phys)
}

// ---------------------------------------------------------------------------
// Supporting types.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(pub u64);

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq)]
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
