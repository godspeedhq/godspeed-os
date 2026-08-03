// SPDX-License-Identifier: GPL-2.0-only
//! Per-task page tables for the Raspberry Pi 4.
//!
//! An address space here is a full L1 plus **its own copies** of the four kernel L2 tables, and that
//! copy is the whole design decision. The alternative - pointing each task's L1 at the *shared* kernel
//! L2s - is what the 32-bit ARM port does, and it costs less memory but buys two sharp edges:
//!
//! - A user page must land at 4 KiB granularity *inside* a 2 MiB kernel block, so the block has to be
//!   split. Split a **shared** table and every other address space sees it.
//! - Reclaim then has to distinguish "a table this task owns" from "a table it merely points at", and
//!   freeing the wrong one corrupts every other task. The 32-bit port hit exactly this.
//!
//! Copying costs 5 frames (20 KiB) per address space: one L1 and four L2s, plus an L3 per 2 MiB region
//! the task actually uses. In exchange **nothing is aliased**, so a split is local by construction and
//! reclaim can free everything the root reaches without a single ownership test. That is the trade this
//! port makes: a fixed, visible 20 KiB against a class of bug that is invisible until it corrupts an
//! unrelated task (§26.13 - boring and inspectable beats clever and cramped).
//!
//! The consequence to know: a change to the kernel map after a task is created does **not** propagate
//! to it. Nothing changes the kernel map after boot, so this is currently free - but it is a real
//! constraint and is written down rather than discovered later.
//!
//! ## Why the kernel is in here at all
//!
//! This port runs the kernel from **TTBR0**, identity-mapped in the low 4 GiB, because the image is
//! loaded and linked at `0x80000`. So every task's address space must also map the kernel: when a task
//! traps, the vectors, kernel code and kernel stack are reached through whatever `TTBR0_EL1` currently
//! holds, which is the task's table. The kernel entries are EL1-only (no `AP[1]`), so a task cannot
//! read them - present-but-privileged, which is the split a user address space needs.
//!
//! The architecturally cleaner shape is the kernel in **TTBR1** (high VA) with TTBR0 purely user, as
//! x86 does with its higher half. That needs the kernel relinked at a high address and a jump across
//! the transition, and it would delete this whole file's kernel-copying half. It is the right long-term
//! move and is deliberately not being made mid-port; recorded per §26.3 rather than silently foregone.

use crate::memory::allocator::{alloc_frame, free_frame};
use crate::memory::frame::{Frame, PhysAddr};

const ENTRIES: usize = 512;
const L1_KERNEL_ENTRIES: usize = 4; // the low 4 GiB the boot map covers

// Descriptor bits, matching `mmu.rs` (which builds the boot map these are copied from).
const DESC_TABLE: u64 = 0b11;
const DESC_BLOCK: u64 = 0b01;
const DESC_PAGE: u64 = 0b11; // at L3 a "page" is 0b11; a BLOCK at L2 is 0b01 - different encodings
const DESC_VALID: u64 = 0b01;
const DESC_AF: u64 = 1 << 10;
const DESC_SH_INNER: u64 = 0b11 << 8;
const DESC_AP_EL0: u64 = 1 << 6; // AP[1]: EL0 may access
const DESC_AP_RO: u64 = 1 << 7; // AP[2]: read-only
const DESC_PXN: u64 = 1 << 53;
const DESC_UXN: u64 = 1 << 54;
const ATTR_NORMAL: u64 = 1 << 2; // AttrIndx = 1 (MAIR slot 1), matching mmu.rs

const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Index of `va` at each level, for a 39-bit VA with a 4 KiB granule.
#[inline]
fn idx(va: u64, level: u8) -> usize {
    let shift = match level {
        1 => 30,
        2 => 21,
        _ => 12,
    };
    ((va >> shift) as usize) & (ENTRIES - 1)
}

/// Where the kernel can reach a physical address: through the `TTBR1` direct map.
///
/// **Not the physical address itself.** Table frames come from the allocator as physical addresses, and
/// while the kernel ran identity-mapped those doubled as pointers. Once TTBR0 belongs to a task, a
/// physical address is not addressable at all - the only way the kernel reaches a frame is the high
/// alias. Getting this wrong would work fine right up until the first real task, then fault inside the
/// page-table code with no obvious link to the address space that had just been installed.
#[inline]
fn phys_to_virt(pa: u64) -> u64 {
    super::mmu::KERNEL_VA_BASE + pa
}

/// Read/write a table entry, addressed through the kernel's high direct map.
///
/// # Safety
/// `table` must be a live, 4 KiB-aligned page-table frame (physical) and `i < 512`.
unsafe fn get(table: u64, i: usize) -> u64 {
    // SAFETY: caller's contract; the high half maps all physical RAM.
    unsafe { ((phys_to_virt(table) + (i as u64) * 8) as *const u64).read_volatile() }
}

/// # Safety
/// As [`get`].
unsafe fn set(table: u64, i: usize, v: u64) {
    // SAFETY: caller's contract; the high half maps all physical RAM.
    unsafe { ((phys_to_virt(table) + (i as u64) * 8) as *mut u64).write_volatile(v) }
}

/// Allocate a zeroed table frame.
fn alloc_table() -> Option<u64> {
    let f = alloc_frame()?;
    let pa = f.phys_addr().0;
    // SAFETY: a freshly allocated frame is ours alone, and identity-mapped RAM is writable. Zeroing
    // matters: a table with garbage in it has VALID bits set at random, and the walker will follow
    // them into whatever the bits happen to name.
    unsafe { core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, 4096) };
    Some(pa)
}

#[derive(Debug)]
pub enum MapError {
    FrameAllocFailed,
    AlreadyMapped,
    NotMapped,
}

/// One task's address space.
pub struct PageTable {
    root: u64,
}

impl PageTable {
    /// Build a fresh, **empty** address space.
    ///
    /// One frame. No kernel entries at all - which is the whole payoff of the TTBR1 split. Before it,
    /// every task table had to carry a copy of the kernel map (5 frames, 20 KiB), because the kernel
    /// was reached through `TTBR0` and a task running with its own table would otherwise have had no
    /// vectors, no kernel code and no kernel stack to trap into. Now the kernel is in `TTBR1`, which no
    /// `TTBR0` switch can disturb and no EL0 access can reach, so a task's table describes the task and
    /// nothing else.
    ///
    /// That is not only cheaper, it removes the failure mode: there is no kernel mapping in here to
    /// collide with a task's own pages, so a task may use any address the architecture gives it.
    pub fn new() -> Result<Self, MapError> {
        let root = alloc_table().ok_or(MapError::FrameAllocFailed)?;
        Ok(PageTable { root })
    }

    /// Map one 4 KiB page, splitting a 2 MiB block if the address lands inside one.
    pub fn map(&mut self, virt: u64, phys: u64, user: bool, writable: bool, exec: bool)
        -> Result<(), MapError>
    {
        let l1i = idx(virt, 1);
        // SAFETY: `self.root` is this table's live L1.
        let l1e = unsafe { get(self.root, l1i) };
        let l2 = if l1e & DESC_VALID == 0 {
            let t = alloc_table().ok_or(MapError::FrameAllocFailed)?;
            // SAFETY: as above.
            unsafe { set(self.root, l1i, t | DESC_TABLE) };
            t
        } else {
            l1e & ADDR_MASK
        };

        let l2i = idx(virt, 2);
        // SAFETY: `l2` is a live table frame.
        let l2e = unsafe { get(l2, l2i) };
        let l3 = if l2e & DESC_VALID == 0 {
            let t = alloc_table().ok_or(MapError::FrameAllocFailed)?;
            // SAFETY: as above.
            unsafe { set(l2, l2i, t | DESC_TABLE) };
            t
        } else if l2e & 0b11 == DESC_BLOCK {
            // A 2 MiB KERNEL block already covers this address, so mapping here would put a task page
            // on top of the kernel's identity mapping of that same range. Refused, loudly, because the
            // alternative is worse than a failed map: kernel code reaching a physical address through
            // its identity VA would find the task's frame instead. `USER_STACK_TOP` (0x8000_0000) is
            // squarely in this region on this board - it sits directly above the frame allocator's own
            // bitmap - so this is the normal case for a real user task, not a corner.
            //
            // The fix is not to split the block; it is to stop putting the kernel in TTBR0 at all. See
            // the module header: the kernel belongs in TTBR1 (high VA), leaving TTBR0 entirely to the
            // task, at which point this branch cannot arise because a task's table holds no kernel
            // entries to collide with.
            return Err(MapError::AlreadyMapped);
        } else {
            l2e & ADDR_MASK
        };

        let l3i = idx(virt, 3);
        // SAFETY: `l3` is a live table frame.
        if unsafe { get(l3, l3i) } & DESC_VALID != 0 {
            // Refuse rather than overwrite: silently replacing a mapping loses the frame that was
            // there, and the loss surfaces as corruption somewhere unrelated (§26.7).
            return Err(MapError::AlreadyMapped);
        }

        let mut e = (phys & ADDR_MASK) | DESC_PAGE | DESC_AF | DESC_SH_INNER | ATTR_NORMAL;
        if user {
            e |= DESC_AP_EL0;
            // An EL0-accessible region is forced PXN at EL1 anyway (the milestone-6 lesson); saying so
            // explicitly keeps the descriptor honest about what it grants.
            e |= DESC_PXN;
        } else {
            e |= DESC_UXN;
        }
        if !writable {
            e |= DESC_AP_RO;
        }
        if !exec {
            e |= DESC_UXN | DESC_PXN;
        }
        // SAFETY: `l3` is live and `l3i < 512`.
        unsafe { set(l3, l3i, e) };
        Ok(())
    }

    /// Remove a mapping and hand back the frame it pointed at.
    pub fn unmap(&mut self, virt: u64) -> Result<Frame, MapError> {
        // SAFETY: walking this table's own live frames.
        unsafe {
            let l1e = get(self.root, idx(virt, 1));
            if l1e & DESC_VALID == 0 { return Err(MapError::NotMapped); }
            let l2e = get(l1e & ADDR_MASK, idx(virt, 2));
            if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { return Err(MapError::NotMapped); }
            let l3 = l2e & ADDR_MASK;
            let l3i = idx(virt, 3);
            let l3e = get(l3, l3i);
            if l3e & DESC_VALID == 0 { return Err(MapError::NotMapped); }
            set(l3, l3i, 0);
            Ok(Frame::from_phys(PhysAddr(l3e & ADDR_MASK)))
        }
    }

    /// The value to install in `TTBR0_EL1` for this address space.
    pub fn ttbr(&self) -> u64 { self.root }

    /// Give up ownership of the root without freeing it - the scheduler stores the raw value in a
    /// task's context and frees it via `free_page_table_root` at death.
    pub fn into_root(self) -> u64 {
        let r = self.root;
        core::mem::forget(self);
        r
    }
}

/// Free every frame reachable from `root`: the L3s, the L2s, then the root.
///
/// Safe to do wholesale precisely because nothing is shared - see the module header. It frees only
/// TABLE frames, never the pages a table points at; the task's data frames are reclaimed separately by
/// the neutral kill path, which knows which of them it handed out.
///
/// # Safety
/// `root` must be an address space no core is executing under - the caller has already switched
/// `TTBR0_EL1` away and invalidated - and it must not be freed twice.
pub unsafe fn free_all(root: u64) {
    // SAFETY: caller's contract; the tables are identity-mapped RAM.
    unsafe {
        for i in 0..ENTRIES {
            let l1e = get(root, i);
            if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { continue; }
            let l2 = l1e & ADDR_MASK;
            for j in 0..ENTRIES {
                let l2e = get(l2, j);
                // Only a TABLE entry owns a frame below it; a BLOCK maps memory directly and has no
                // table to free.
                if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { continue; }
                free_frame(Frame::from_phys(PhysAddr(l2e & ADDR_MASK)));
            }
            free_frame(Frame::from_phys(PhysAddr(l2)));
        }
        free_frame(Frame::from_phys(PhysAddr(root)));
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        // SAFETY: a `PageTable` that reaches Drop was never installed (installing consumes it via
        // `into_root`), so no core is executing under it.
        unsafe { free_all(self.root) };
    }
}
