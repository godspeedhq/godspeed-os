// SPDX-License-Identifier: GPL-2.0-only
//! Per-task page tables for the Raspberry Pi 4.
//!
//! An address space here is **one frame**, holding the task and nothing else. No kernel entries, no
//! copies, no sharing - because the kernel lives in `TTBR1`, where no `TTBR0` switch can disturb it and
//! no EL0 access can reach it.
//!
//! That is worth stating plainly because it was not free. Before the TTBR1 split (milestone 11) the
//! kernel ran identity-mapped from `TTBR0`, so **every** task table had to carry a copy of the kernel
//! map - 5 frames, 20 KiB apiece - or a task would have had no vectors, no kernel code and no kernel
//! stack to trap into. Worse, a task page below 4 GiB then landed inside a 2 MiB kernel block and had
//! to shadow it, which is a collision the 32-bit port hit head-on. Moving the kernel to the high half
//! deleted the copy and the collision together.
//!
//! ## What a task may address
//!
//! Anything the architecture gives it. There is no kernel mapping in here to collide with, so the
//! usual x86-shaped layout (code low, stack at `USER_STACK_TOP`) works unchanged - which is the whole
//! point of matching x86's kernel-high/user-low arrangement rather than inventing a third one.
//!
//! ## Reclaim
//!
//! Nothing is aliased, so teardown needs no ownership test: [`free_all`] frees every table frame the
//! root reaches, and [`reclaim_pages`] frees the leaf pages. They are separate because the neutral kill
//! path reclaims a task's data and its page tables at different moments.

use super::page_tables::{MapError, PageFlags, VirtAddr};
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
const ATTR_DEVICE: u64 = 0; // AttrIndx = 0 (MAIR slot 0) = Device-nGnRnE, matching mmu.rs

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
    // REFUSE to follow an address that is not RAM, and say so.
    //
    // Every walk in this file dereferences a descriptor's output address. A stale or corrupt entry
    // therefore turns a reclaim - the recovery path - into a kernel data abort, which is the one thing
    // a recovery path must never become. x86 gained exactly this guard after a real `chaos max-carnage`
    // run followed a corrupt entry to physical ~68 GB (`arch/x86_64/page_tables.rs::walk`); the
    // semantic did not cross the seam with the signature (SEC-27).
    //
    // It bites harder here: the aarch64 high map covers only the low 4 GiB, so any descriptor naming
    // above that yields an unmapped VA and faults, where x86 would merely read rubbish. Returning 0
    // reads as "not valid" at every call site, so a corrupt subtree is skipped rather than followed,
    // and the frame is not then handed to `free_frame`.
    if !crate::memory::allocator::phys_in_ram(table) {
        crate::kprintln!("ptables: REFUSING a walk into {:#x} - not RAM; subtree skipped", table);
        return 0;
    }
    // SAFETY: caller's contract, and the address is now known to be RAM the high half maps.
    unsafe { ((phys_to_virt(table) + (i as u64) * 8) as *const u64).read_volatile() }
}

/// # Safety
/// As [`get`].
unsafe fn set(table: u64, i: usize, v: u64) {
    // Same guard as `get`, and more important: a write through a corrupt address does not merely fault,
    // it corrupts whatever it lands on if the address happens to be mapped.
    if !crate::memory::allocator::phys_in_ram(table) {
        crate::kprintln!("ptables: REFUSING a write into {:#x} - not RAM", table);
        return;
    }
    // SAFETY: caller's contract, and the address is now known to be RAM the high half maps.
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

    /// The neutral `arch::imp::page_tables::PageTable::map`, as `loader.rs` and `task/` call it.
    ///
    /// Translates the x86-shaped `PageFlags` the neutral layers speak into AArch64 descriptor bits.
    /// The interesting one is **`PCD | PWT`**: on x86 those disable caching for an MMIO mapping, and
    /// the faithful AArch64 equivalent is not "uncached Normal" but the **Device** memory attribute -
    /// which additionally forbids the reordering, merging and speculative repetition that make a
    /// wrongly-typed MMIO mapping misbehave in ways no fault points at.
    pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags)
        -> Result<(), MapError>
    {
        if !flags.contains(PageFlags::PRESENT) {
            // A mapping request that does not ask for PRESENT is a caller bug, not a way to remove a
            // mapping - `unmap` is that. Refusing is louder than installing an invalid descriptor.
            return Err(MapError::NotMapped);
        }
        let device = flags.contains(PageFlags::PCD) || flags.contains(PageFlags::PWT);
        self.map_raw(
            virt.0,
            phys.0,
            flags.contains(PageFlags::USER),
            flags.contains(PageFlags::WRITABLE),
            !flags.contains(PageFlags::NO_EXEC),
            device,
        )
    }

    /// Map one 4 KiB page. The arch-native form, also used directly by the EL0 bring-up path.
    pub fn map_raw(&mut self, virt: u64, phys: u64, user: bool, writable: bool, exec: bool, device: bool)
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

        // Device memory takes no shareability (the field is meaningless for it) and is never
        // executable; Normal memory is inner-shareable and cacheable.
        let mut e = if device {
            (phys & ADDR_MASK) | DESC_PAGE | DESC_AF | ATTR_DEVICE | DESC_UXN | DESC_PXN
        } else {
            (phys & ADDR_MASK) | DESC_PAGE | DESC_AF | DESC_SH_INNER | ATTR_NORMAL
        };
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
    pub fn unmap(&mut self, virt: VirtAddr) -> Result<Frame, MapError> {
        let virt = virt.0;
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
            // PUBLISH the clear, then INVALIDATE, before the frame is handed back.
            //
            // The caller receives an owned `Frame` and returns it to the allocator, where it is handed
            // to somebody else. If the translation is still in a TLB at that moment, the previous owner
            // can read and write a frame that now belongs to another address space - a use-after-free
            // the MMU actively enables, and one no fault reports.
            //
            // x86 does not need this HERE because its neutral kill path issues a shootdown, and that is
            // exactly why the omission survives review: the primitive looks complete against the arch
            // whose semantics were in mind when it was written. `arch/CLAUDE.md` (SEC-26/27) says a
            // primitive owes its SEMANTIC and not just its signature; this is that debt.
            //
            // Inner-shareable and by VA across all ASIDs: the entry may sit in any core's TLB, and the
            // task that owned it is not necessarily the one running this.
            core::arch::asm!(
                "dsb ishst",
                "tlbi vaae1is, {}",
                "dsb ish",
                "isb",
                in(reg) virt >> 12,
                options(nostack),
            );
            Ok(Frame::from_phys(PhysAddr(l3e & ADDR_MASK)))
        }
    }

    /// The value to install in `TTBR0_EL1` for this address space.
    ///
    /// Named `cr3_value` as well, because that is what the neutral scheduler calls it - the same
    /// naming compromise `TaskContext::cr3` makes, and for the same reason.
    pub fn ttbr(&self) -> u64 { self.root }
    pub fn cr3_value(&self) -> u64 { self.root }

    /// Give up ownership of the root without freeing it - the scheduler stores the raw value in a
    /// task's context and frees it via `free_page_table_root` at death.
    pub fn into_root(self) -> u64 {
        let r = self.root;
        core::mem::forget(self);
        r
    }
    /// The neutral name for [`into_root`].
    pub fn into_cr3(self) -> u64 { self.into_root() }
}

/// Map one page into an address space named only by its root, without owning it.
///
/// This is what the neutral `map_in_active_tables` needs: `AllocMem` grows the *running* task's heap,
/// and the running task's table is whatever `TTBR0_EL1` holds. A `PageTable` cannot be used directly
/// for that, because its `Drop` frees the tree - wrapping the live root in one and letting it fall out
/// of scope would free the address space of the task that is currently executing. `ManuallyDrop` says
/// exactly that: borrow the root, do not own it.
///
/// # Safety
/// `root` must be a live L1 built by this module (the value in `TTBR0_EL1`, or a table the caller
/// owns). The caller is responsible for any TLB maintenance a *re*mapping would need; this is used for
/// fresh mappings, where there is no stale entry to invalidate.
pub unsafe fn map_in_root(root: u64, virt: u64, phys: u64, flags: PageFlags) -> Result<(), MapError> {
    let mut t = core::mem::ManuallyDrop::new(PageTable { root });
    let r = t.map(VirtAddr(virt), PhysAddr(phys), flags);
    // Publish the descriptors before anything walks them.
    //
    // The doc above is right that a FRESH mapping needs no TLB invalidate - and that is a different
    // obligation from this one. The entries are ordinary stores, and `map_in_active_tables` calls this
    // against the tables THIS CORE IS EXECUTING UNDER, so the walker may reach for a descriptor that
    // has not become visible yet and fault on a page that is mapped. Same defect as the respawn fault,
    // in a tighter window: there, work happened between building a space and entering it; here the
    // next instruction can touch the page.
    //
    // SAFETY: a barrier; no memory is accessed.
    unsafe {
        core::arch::asm!("dsb ishst", options(nostack));
    }
    r
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

/// Make every page this address space maps safe to execute.
///
/// The kernel has just written a program's text into these frames through its own cacheable direct
/// map. The instruction and data caches are not coherent, so without this the core may fetch whatever
/// the I-cache still holds for those physical addresses - and when the allocator recycles frames a
/// previous task executed from, that is the previous task's code.
///
/// **This is the loader's version of the bug already fixed in the hand-written payload paths.** Fixing
/// those and not this one left the class open: the ELF loader writes far more code, into far more
/// recycled frames, and the failure it produced was a boot that silently re-entered itself until the
/// endpoint table filled. Applying the fix at the arch hook the neutral loader already calls covers
/// every service, not just the ones someone remembered.
///
/// Pages are synced through the kernel's direct map, which is where the writes went; cache maintenance
/// is by physical line, so the task's own virtual addresses do not need to be mapped here.
///
/// # Safety
/// `root` must be an address space this task owns and nothing is executing under yet.
pub unsafe fn sync_all_pages(root: u64) -> usize {
    let mut synced = 0usize;
    // SAFETY: caller's contract; tables reached through the kernel's direct map.
    unsafe {
        for i in 0..ENTRIES {
            let l1e = get(root, i);
            if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { continue; }
            let l2 = l1e & ADDR_MASK;
            for j in 0..ENTRIES {
                let l2e = get(l2, j);
                if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { continue; }
                let l3 = l2e & ADDR_MASK;
                for k in 0..ENTRIES {
                    let l3e = get(l3, k);
                    if l3e & DESC_VALID == 0 { continue; }
                    super::mmu::sync_instruction_cache(phys_to_virt(l3e & ADDR_MASK), 4096);
                    synced += 1;
                }
            }
        }
    }
    synced
}

/// Free every LEAF page the address space maps, returning how many.
///
/// Separate from [`free_all`], which frees the TABLE frames. The neutral kill path reclaims a task's
/// data pages and its page tables at different moments, so conflating them would free one of them
/// twice or not at all.
///
/// Every mapping in here belongs to the task by construction - the table holds nothing else - so there
/// is no "is this mine?" test to get wrong, which is the property the one-frame design buys.
///
/// # Safety
/// The address space must belong to a task already dead, with `TTBR0_EL1` switched away and
/// invalidated, so no walker can still reach it.
/// DIAGNOSTIC: make one already-mapped user page READ-ONLY, and invalidate its TLB entry.
///
/// The decisive experiment for the Pi 4 shell fault. Everything that can write that stack has been
/// eliminated by measurement - the shell (no `unsafe`, all buffers clamp), other tasks (no shared
/// frames, no aliasing), the kernel (zero writes overlapping the canary), and the DMA arenas (no
/// overlap). Yet a canary that cannot be optimised away is provably destroyed.
///
/// One candidate survives: a device writing to a physical address it was wrongly given. This board
/// has no IOMMU/SMMU (§6.4, `confine_device` returns false on aarch64), so a driver's DMA is
/// unconfined and invisible to every check above.
///
/// CPU writes and DMA differ in exactly one observable way: **DMA bypasses the MMU when there is no
/// IOMMU.** So making the page read-only separates them. If the canary is still destroyed and NO
/// permission fault is reported, the writer was not the CPU - which is positive proof of DMA rather
/// than another elimination. If a fault IS taken, the dump names the exact PC that wrote it.
///
/// Returns false if the VA is not mapped at L3.
///
/// # Safety
/// `root` must be a live page-table root; the caller must own the address space being modified.
pub unsafe fn set_page_ro_diag(root: u64, virt: u64) -> bool {
    // SAFETY: caller's contract. Read-modify-write of one L3 descriptor, followed by the TLB
    // maintenance the change requires - the same sequence `unmap` performs.
    unsafe {
        let l1e = get(root, idx(virt, 1));
        if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { return false; }
        let l2e = get(l1e & ADDR_MASK, idx(virt, 2));
        if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { return false; }
        let l3 = l2e & ADDR_MASK;
        let i = idx(virt, 3);
        let l3e = get(l3, i);
        if l3e & DESC_VALID == 0 { return false; }
        set(l3, i, l3e | DESC_AP_RO);
        core::arch::asm!(
            "dsb ishst",
            "tlbi vaae1is, {p}",
            "dsb ish",
            "isb",
            p = in(reg) (virt >> 12),
            options(nostack, preserves_flags)
        );
        true
    }
}

/// DIAGNOSTIC: translate one user VA through `root`, or `None` if it is not mapped.
///
/// Used by the fault dump to turn a faulting task's stack pages into PHYSICAL frames, so they can be
/// tested against the device DMA reservations (`allocator::phys_dma_proximity`). Read-only, and the
/// same descriptor walk `map_raw` performs.
///
/// # Safety
/// `root` must be a page-table root this walk may read; `get` already refuses non-RAM descriptors.
/// DIAGNOSTIC: find a VA in `root` that maps physical page `pa`, if any.
///
/// Walks the TREE rather than probing every VA. Probing the 2 GiB user range one page at a time is
/// half a million walks per address space, which on a machine with ~180 live tasks turns a fault dump
/// into a minutes-long hang - and a diagnostic that looks like a lock-up gets read as one. Descending
/// the tables instead skips whole unmapped subtrees, which is most of the space.
///
/// # Safety
/// `root` must be a live page-table root.
/// DIAGNOSTIC: collect EVERY VA in `root` that maps physical page `pa`. Returns how many were found
/// (which may exceed `out.len()`; only the first `out.len()` are written).
///
/// `find_pa_diag` returns the first match, which is the wrong question for the address space the fault
/// happened in. If ONE address space maps a frame twice, a write through the second VA reaches the
/// first VA's memory through a different descriptor - so a read-only mapping on one of them stops
/// nothing, no other task is involved, the allocator is perfectly consistent, and no device is needed.
/// That is the shape of every observation in this investigation, and the earlier scan could not see it
/// because it skipped the faulting task. Counting all matches is what turns that from a story into a
/// number.
///
/// # Safety
/// `root` must be a live page-table root.
pub unsafe fn find_all_pa_diag(root: u64, pa: u64, out: &mut [u64]) -> usize {
    let mut n = 0usize;
    // SAFETY: caller's contract; read-only walk with the audited `get` accessor.
    unsafe {
        for i in 0..ENTRIES {
            let l1e = get(root, i);
            if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { continue; }
            let l2 = l1e & ADDR_MASK;
            for j in 0..ENTRIES {
                let l2e = get(l2, j);
                if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { continue; }
                let l3 = l2e & ADDR_MASK;
                for k in 0..ENTRIES {
                    let l3e = get(l3, k);
                    if l3e & DESC_VALID == 0 { continue; }
                    if l3e & ADDR_MASK == pa {
                        if n < out.len() {
                            out[n] = ((i as u64) << 30) | ((j as u64) << 21) | ((k as u64) << 12);
                        }
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

pub unsafe fn find_pa_diag(root: u64, pa: u64) -> Option<u64> {
    // SAFETY: caller's contract; read-only walk using the same accessors as `map_raw`.
    unsafe {
        for i in 0..ENTRIES {
            let l1e = get(root, i);
            if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { continue; }
            let l2 = l1e & ADDR_MASK;
            for j in 0..ENTRIES {
                let l2e = get(l2, j);
                if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { continue; }
                let l3 = l2e & ADDR_MASK;
                for k in 0..ENTRIES {
                    let l3e = get(l3, k);
                    if l3e & DESC_VALID == 0 { continue; }
                    if l3e & ADDR_MASK == pa {
                        return Some(((i as u64) << 30) | ((j as u64) << 21) | ((k as u64) << 12));
                    }
                }
            }
        }
        None
    }
}

/// DIAGNOSTIC: return the RAW L3 descriptor for `virt`, not just the address it translates to.
///
/// The control the read-only canary experiment rests on. "No permission fault" only means the writer
/// bypassed the MMU if the page was still read-only WHEN it was written. Two things could fake that
/// result: a stale TLB entry that kept write permission alive, or something having rewritten the
/// descriptor afterwards. Reading the descriptor back at fault time distinguishes all three:
///   - AP[2] set, same PA  -> the mapping was read-only throughout. The writer was not the CPU.
///   - AP[2] clear         -> something rewrote this entry. A different bug, and a CPU-side one.
///   - a different PA      -> the page was remapped under a live task. Different bug again.
/// Without this, "no fault" is an assumption; with it, it is evidence.
///
/// # Safety
/// `root` must be a live page-table root.
pub unsafe fn descriptor_diag(root: u64, virt: u64) -> Option<u64> {
    // SAFETY: caller's contract; read-only descriptor walk, identical to `translate_diag` below.
    unsafe {
        let l1e = get(root, idx(virt, 1));
        if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { return None; }
        let l2e = get(l1e & ADDR_MASK, idx(virt, 2));
        if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { return None; }
        let l3e = get(l2e & ADDR_MASK, idx(virt, 3));
        if l3e & DESC_VALID == 0 { return None; }
        Some(l3e)
    }
}

pub unsafe fn translate_diag(root: u64, virt: u64) -> Option<u64> {
    // SAFETY: caller's contract; read-only descriptor walk, no dereference of a non-RAM address
    // (`get` rejects those itself).
    unsafe {
        let l1e = get(root, idx(virt, 1));
        if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { return None; }
        let l2e = get(l1e & ADDR_MASK, idx(virt, 2));
        if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { return None; }
        let l3e = get(l2e & ADDR_MASK, idx(virt, 3));
        if l3e & DESC_VALID == 0 { return None; }
        Some(l3e & ADDR_MASK)
    }
}

pub unsafe fn reclaim_pages(root: u64) -> usize {
    let mut freed = 0usize;
    // SAFETY: caller's contract; tables reached through the kernel's direct map.
    unsafe {
        for i in 0..ENTRIES {
            let l1e = get(root, i);
            if l1e & DESC_VALID == 0 || l1e & 0b11 != DESC_TABLE { continue; }
            let l2 = l1e & ADDR_MASK;
            for j in 0..ENTRIES {
                let l2e = get(l2, j);
                if l2e & DESC_VALID == 0 || l2e & 0b11 != DESC_TABLE { continue; }
                let l3 = l2e & ADDR_MASK;
                for k in 0..ENTRIES {
                    let l3e = get(l3, k);
                    if l3e & DESC_VALID == 0 { continue; }
                    set(l3, k, 0);
                    free_frame(Frame::from_phys(PhysAddr(l3e & ADDR_MASK)));
                    freed += 1;
                }
            }
        }
    }
    freed
}

impl Drop for PageTable {
    fn drop(&mut self) {
        // SAFETY: a `PageTable` that reaches Drop was never installed (installing consumes it via
        // `into_root`), so no core is executing under it.
        unsafe { free_all(self.root) };
    }
}
