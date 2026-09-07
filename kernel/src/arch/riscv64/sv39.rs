// SPDX-License-Identifier: GPL-2.0-only
//! Sv39 paging - three levels, 39-bit virtual addresses, 4 KiB pages.
//!
//! This is what invariant 2 rests on for this ISA: per-service address spaces. Until it exists the
//! neutral kernel can allocate frames and route IPC but cannot isolate anything, so
//! `spawn_supervisor` is unreachable.
//!
//! TWO RISC-V RULES THAT ARE EASY TO GET WRONG, and both fail as something else.
//!
//! **`W` without `R` is RESERVED.** A PTE with write set and read clear is not "write-only", it is
//! architecturally invalid, and the fault names the access rather than the mapping. So a writable
//! page is always mapped readable, done once in the flag translation rather than trusted to every
//! caller.
//!
//! **Accessed and Dirty may not be maintained by hardware.** QEMU `virt` implements `svadu` and sets
//! them for us; the JH7110 reports `Boot HART ISA Extensions : none` and does not, so a page mapped
//! with `A = 0` faults on its FIRST touch there while working perfectly in the emulator. Every leaf
//! written here carries `A` and `D` from the start. One line, and the difference between a port that
//! runs on hardware and one that only runs where it was written.
//!
//! Addresses are IDENTITY MAPPED for now: entry is in S-mode with `satp` zero, so VA equals PA, and
//! the kernel keeps it that way when it enables translation. That is why `PHYS_IS_IDENTITY` is true
//! and `hhdm_offset` is zero. A higher-half map is a later, deliberate change; doing it in the same
//! step as turning paging on would mean debugging two things at once.

use crate::memory::allocator::alloc_frame;
use crate::memory::frame::{Frame, PhysAddr};

/// PTE bits, from the privileged specification.
pub const PTE_V: u64 = 1 << 0; // valid
pub const PTE_R: u64 = 1 << 1; // readable
pub const PTE_W: u64 = 1 << 2; // writable
pub const PTE_X: u64 = 1 << 3; // executable
pub const PTE_U: u64 = 1 << 4; // reachable from user mode
pub const PTE_A: u64 = 1 << 6; // accessed
pub const PTE_D: u64 = 1 << 7; // dirty

#[derive(Debug, Clone, Copy)]
pub enum MapFail {
    NoFrame,
    AlreadyMapped,
    NotMapped,
    /// The address cannot exist in Sv39. See `va_is_canonical`.
    NotCanonical,
}

/// Can this virtual address exist at all under Sv39?
///
/// **Sv39 addresses are 39 bits SIGN-EXTENDED**, so bits 63:38 must all equal bit 38. The usable
/// space is therefore two halves, not one run: `0 .. 256 GiB` and `0xFFFF_FFC0_0000_0000 .. `. An
/// address in between is rejected by the hardware BEFORE translation, as a page fault, whatever the
/// tables say.
///
/// This is worth a check rather than a comment because the arithmetic lies to you. A root index is
/// nine bits, so index 256 is a perfectly good table slot and `map_page` will happily fill it - but
/// it is the first index of the HIGH half, reached at `0xFFFF_FFC0_0000_0000`, not at 256 GiB. Map
/// something there by computing "256 GiB" and the walk succeeds, the entry is correct, and the
/// access still faults, at an address that looks like the one you asked for. Refusing here turns
/// that into an error at the map, where the mistake is.
#[inline]
pub fn va_is_canonical(va: u64) -> bool {
    let top = va >> 38; // bits 63:38, twenty-six of them
    top == 0 || top == 0x3ff_ffff
}

/// A PTE is a POINTER to the next level when none of R, W or X is set, and a LEAF when any is. That
/// distinction is the whole shape of the walk, so it is named rather than open-coded.
#[inline]
pub fn pte_is_leaf(pte: u64) -> bool {
    pte & (PTE_R | PTE_W | PTE_X) != 0
}

#[inline]
pub fn pte_is_valid(pte: u64) -> bool {
    pte & PTE_V != 0
}

/// Physical address out of a PTE: the PPN sits in bits [53:10] and addresses a 4 KiB frame.
#[inline]
pub fn pte_phys(pte: u64) -> u64 {
    ((pte >> 10) & 0x0fff_ffff_ffff) << 12
}

/// Build a PTE pointing at `phys` with `bits` set.
#[inline]
pub fn pte_make(phys: u64, bits: u64) -> u64 {
    ((phys >> 12) << 10) | bits
}

/// `satp` for an Sv39 space rooted at `root_phys`. MODE 8 is Sv39; ASID stays 0 until this port has
/// anything to distinguish address spaces WITH, and a wrong ASID is worse than none.
#[inline]
pub fn satp_value(root_phys: u64) -> u64 {
    (8u64 << 60) | (root_phys >> 12)
}

/// Translate the neutral kernel's x86-shaped flags into Sv39 bits.
///
/// The neutral layers speak PRESENT / WRITABLE / USER / NO_EXEC because that vocabulary had to come
/// from somewhere and x86 was first. Each arch translates it; nothing above the seam learns RISC-V's
/// spelling, which is what the seam is for.
pub fn flags_to_pte_bits(flags: u64) -> u64 {
    const PRESENT: u64 = 1 << 0;
    const WRITABLE: u64 = 1 << 1;
    const USER: u64 = 1 << 2;
    const NO_EXEC: u64 = 1 << 63;

    if flags & PRESENT == 0 {
        return 0; // absent is an all-zero PTE, not a flags combination
    }
    // READ IS UNCONDITIONAL on a leaf we map: `W` without `R` is a reserved encoding, and a page
    // nobody may read is expressed by not mapping it rather than by an invalid PTE.
    let mut bits = PTE_V | PTE_R;
    if flags & WRITABLE != 0 {
        bits |= PTE_W;
    }
    if flags & NO_EXEC == 0 {
        bits |= PTE_X;
    }
    if flags & USER != 0 {
        bits |= PTE_U;
    }
    // Set by software because the JH7110 will not set them for us. See the module comment.
    bits | PTE_A | PTE_D
}

/// Index into the level-`lvl` table for `va`. Level 2 is the root.
#[inline]
fn vpn(va: u64, lvl: usize) -> usize {
    ((va >> (12 + 9 * lvl)) & 0x1ff) as usize
}

/// A zeroed frame for a new table level.
///
/// Zeroing is not tidiness. An unzeroed frame is a table full of whatever the last owner left, and
/// any word with bit 0 set is a VALID PTE pointing somewhere arbitrary - which the walk would
/// follow.
fn alloc_table() -> Option<u64> {
    let f = alloc_frame()?;
    let phys = f.phys_addr().0;
    // SAFETY: just allocated to us, 4 KiB, page-aligned, and identity-mapped (VA == PA while the
    // kernel's own map keeps it so).
    unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096) };
    Some(phys)
}

/// Allocate a zeroed root table for a new address space.
pub fn new_root() -> Option<u64> {
    alloc_table()
}

/// Wrap a physical address as a `Frame` for return to the neutral kernel.
///
/// # Safety
/// `phys` must be page-aligned and a frame the caller is entitled to hand back.
pub unsafe fn frame_of(phys: u64) -> Frame {
    // SAFETY: contract delegated to the caller, as documented above.
    unsafe { Frame::from_phys(PhysAddr(phys)) }
}

/// Map one 4 KiB page in the table rooted at `root`, creating levels as needed.
///
/// Refuses rather than overwriting an existing leaf. A silent remap is how two owners come to
/// believe they hold the same page, and the second writer wins invisibly.
pub fn map_page(root: u64, va: u64, pa: u64, bits: u64) -> Result<(), MapFail> {
    if !va_is_canonical(va) {
        return Err(MapFail::NotCanonical);
    }
    let mut table = root;
    for lvl in (1..=2).rev() {
        // SAFETY: `table` is a page-aligned frame this kernel owns, identity-mapped; idx < 512.
        let slot = unsafe { (table as *mut u64).add(vpn(va, lvl)) };
        // SAFETY: as above.
        let pte = unsafe { slot.read_volatile() };
        if !pte_is_valid(pte) {
            let next = alloc_table().ok_or(MapFail::NoFrame)?;
            // A POINTER PTE: valid, with R/W/X all clear so the walk descends rather than stopping.
            // SAFETY: as above.
            unsafe { slot.write_volatile(pte_make(next, PTE_V)) };
            table = next;
        } else if pte_is_leaf(pte) {
            // A large page already covers this address. Splitting one is real work with real
            // invariants and nothing here creates one yet, so refuse rather than pretend.
            return Err(MapFail::AlreadyMapped);
        } else {
            table = pte_phys(pte);
        }
    }
    // SAFETY: as above.
    let slot = unsafe { (table as *mut u64).add(vpn(va, 0)) };
    // SAFETY: as above.
    if pte_is_valid(unsafe { slot.read_volatile() }) {
        return Err(MapFail::AlreadyMapped);
    }
    // SAFETY: as above.
    unsafe { slot.write_volatile(pte_make(pa, bits)) };
    Ok(())
}

/// Remove one 4 KiB mapping, returning the frame it pointed at.
///
/// The caller invalidates. This function does not know whether `root` is the ACTIVE table, and an
/// `sfence.vma` for a table nobody is using is a wasted fence rather than a correctness bug -
/// whereas skipping one for the active table is the opposite.
pub fn unmap_page(root: u64, va: u64) -> Result<u64, MapFail> {
    if !va_is_canonical(va) {
        return Err(MapFail::NotCanonical);
    }
    let mut table = root;
    for lvl in (1..=2).rev() {
        // SAFETY: `table` is a page-aligned frame this kernel owns, identity-mapped.
        let pte = unsafe { (table as *const u64).add(vpn(va, lvl)).read_volatile() };
        if !pte_is_valid(pte) || pte_is_leaf(pte) {
            return Err(MapFail::NotMapped);
        }
        table = pte_phys(pte);
    }
    // SAFETY: as above.
    let slot = unsafe { (table as *mut u64).add(vpn(va, 0)) };
    // SAFETY: as above.
    let pte = unsafe { slot.read_volatile() };
    if !pte_is_valid(pte) {
        return Err(MapFail::NotMapped);
    }
    // SAFETY: as above.
    unsafe { slot.write_volatile(0) };
    Ok(pte_phys(pte))
}

/// The leaf PTE for `va`, if one exists.
pub fn translate(root: u64, va: u64) -> Option<u64> {
    if !va_is_canonical(va) {
        return None; // cannot exist, so nothing translates it
    }
    let mut table = root;
    for lvl in (1..=2).rev() {
        // SAFETY: `table` is a page-aligned frame this kernel owns, identity-mapped.
        let pte = unsafe { (table as *const u64).add(vpn(va, lvl)).read_volatile() };
        if !pte_is_valid(pte) {
            return None;
        }
        if pte_is_leaf(pte) {
            return Some(pte);
        }
        table = pte_phys(pte);
    }
    // SAFETY: as above.
    let pte = unsafe { (table as *const u64).add(vpn(va, 0)).read_volatile() };
    if pte_is_valid(pte) { Some(pte) } else { None }
}

/// Identity-map `[0, end)` using 1 GiB leaves at the root level.
///
/// A ROOT-LEVEL PTE WITH R/W/X SET IS A LEAF covering one gigabyte, and using them here is not an
/// optimisation so much as the difference between nine table entries and sixteen megabytes of page
/// tables for the board's eight gigabytes of RAM. The walk in `map_page` already understands leaves
/// at any level, because `pte_is_leaf` asks about the permission bits rather than the depth.
///
/// IDENTITY, DELIBERATELY. The kernel is entered with `satp` zero and every address it holds - its
/// own code, the stack it is running on, the frame allocator's bitmaps, the UART it is about to
/// report through - is physical. Mapping virtual to the same value means enabling translation
/// changes nothing that is already in flight, which is the only version of this step that can be
/// debugged: if the machine goes quiet afterwards, the fault is the mapping and not an address that
/// silently moved.
///
/// Everything below RAM is covered too, in the same sweep. The UART at 0x1000_0000 and the PLIC at
/// 0x0c00_0000 both live under the first gigabyte on both machines, so the first leaf carries them
/// without either address appearing here.
pub fn identity_map_gigapages(root: u64, end: u64, bits: u64) -> Result<(), MapFail> {
    const GIB: u64 = 1 << 30;
    let mut addr: u64 = 0;
    while addr < end {
        let idx = ((addr >> 30) & 0x1ff) as usize;
        if idx >= 512 {
            break; // past what Sv39's 39-bit space can address; the caller sized `end` wrongly
        }
        // SAFETY: `root` is a page-aligned frame this kernel owns, identity-mapped; idx < 512.
        let slot = unsafe { (root as *mut u64).add(idx) };
        // SAFETY: as above.
        unsafe { slot.write_volatile(pte_make(addr, bits)) };
        addr = addr.saturating_add(GIB);
        if addr == 0 {
            break; // wrapped: nothing sane left to map
        }
    }
    Ok(())
}

/// Copy the kernel's identity mapping into a new address space, WITHOUT disturbing what the task
/// already has there.
///
/// **The kernel's map and userspace OVERLAP on this port, and that is the whole difficulty.** x86
/// puts the kernel higher-half, so a task's mappings and the kernel's cannot collide and the kernel
/// half can be shared by copying top-level entries wholesale. Here the kernel is identity-mapped
/// from zero and a service links at 0x400000, so both live inside the FIRST gigapage. Copying that
/// gigapage in wholesale replaces the loader's mapping of the service's own text with a kernel leaf
/// that has no `U` bit - and the task faults on its first instruction fetch, at an address that IS
/// mapped, by a PTE it never asked for. That is exactly what happened the first time the supervisor
/// was scheduled: `instruction page fault ... pte 0xcf VRWX-AD phys 0x0`, the identity gigapage
/// answering for a user address.
///
/// So the copy is done at whatever granularity avoids the task, slot by slot:
///
/// - **Empty root slot** - take the kernel's gigapage whole. One entry, nothing to avoid.
/// - **Task already has a leaf** - leave it alone. The task owns that gigabyte.
/// - **Task has a pointer table** - descend and fill only the 2 MiB slots it has NOT claimed. The
///   kernel gets the rest of that gigabyte, which is how the UART at 0x1000_0000 and the PLIC at
///   0x0c00_0000 stay reachable while a service whose text sits at 0x400000 is running. Losing them
///   would mean losing the console from inside the trap handler, which is the one place it is most
///   needed.
///
/// The permission bits come from the KERNEL's own leaf, so `U` is never set on anything this copies:
/// the task inherits the kernel's map as kernel memory, unreachable from user mode, which is the
/// point.
///
/// A clone is still a SNAPSHOT: a later change to the kernel root does not reach a task root built
/// before it. That is fine because the kernel's map is fixed once paging is on, and it is written
/// down because the day that stops being true, this is where the bug will be.
pub fn clone_kernel_map(dst: u64, src: u64) -> usize {
    const GIB_SLOT_SHIFT: u32 = 30;
    const MIB2_SHIFT: u32 = 21;
    let mut filled = 0;
    for idx in 0..512usize {
        // SAFETY: both are page-aligned root tables this kernel owns, identity-mapped; idx < 512.
        let k = unsafe { (src as *const u64).add(idx).read_volatile() };
        if !pte_is_valid(k) || !pte_is_leaf(k) {
            continue; // the kernel maps this gigabyte through a table, or not at all
        }
        // SAFETY: as above.
        let d = unsafe { (dst as *const u64).add(idx).read_volatile() };

        if !pte_is_valid(d) {
            // SAFETY: as above. Nothing of the task's lives here, so the gigapage goes in whole.
            unsafe { (dst as *mut u64).add(idx).write_volatile(k) };
            filled += 1;
            continue;
        }
        if pte_is_leaf(d) {
            continue; // the task owns this whole gigabyte; it was not ours to give
        }

        // The task has 4 KiB mappings somewhere in this gigabyte. Give it the kernel's map at 2 MiB
        // granularity everywhere it has not already claimed, so neither loses anything.
        let table = pte_phys(d);
        let bits = k & (PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D); // never `U`
        let base = (idx as u64) << GIB_SLOT_SHIFT;
        for j in 0..512usize {
            // SAFETY: `table` came from a pointer PTE this kernel wrote, so it is a 4 KiB table it
            // owns; j < 512.
            let e = unsafe { (table as *const u64).add(j).read_volatile() };
            if pte_is_valid(e) {
                continue; // the task's own mapping, at any granularity: leave it exactly as it is
            }
            let pa = base + ((j as u64) << MIB2_SHIFT);
            // SAFETY: as above. A 2 MiB leaf at level 1, identity, with the kernel's own permissions.
            unsafe { (table as *mut u64).add(j).write_volatile(pte_make(pa, bits)) };
            filled += 1;
        }
    }
    filled
}

/// Free every table BELOW a root, then the root itself, and report how many frames came back.
///
/// Walks only POINTER entries, never leaves. A leaf at any level names a frame the task was given
/// rather than a table it was built out of, and freeing those is the caller's business (the frames
/// have owners; the tables do not). Combined with `clone_leaf_roots`, that is exactly why the
/// kernel's inherited gigapages survive a task's death: they are leaves, so this never follows them.
///
/// # Safety
/// `root` must be a root table belonging to a task that is finished with it, and must not be the
/// live `satp` root - freeing the address space you are executing in is not detectable from here.
pub unsafe fn free_table_tree(root: u64) -> usize {
    let mut freed = 0;
    for i2 in 0..512 {
        // SAFETY: `root` is a page-aligned table this kernel owns, identity-mapped; i2 < 512.
        let l1 = unsafe { (root as *const u64).add(i2).read_volatile() };
        if !pte_is_valid(l1) || pte_is_leaf(l1) {
            continue;
        }
        let t1 = pte_phys(l1);
        for i1 in 0..512 {
            // SAFETY: `t1` came from a pointer PTE this kernel wrote, so it is a table it owns.
            let l0 = unsafe { (t1 as *const u64).add(i1).read_volatile() };
            if !pte_is_valid(l0) || pte_is_leaf(l0) {
                continue;
            }
            // SAFETY: as above; the level-0 table is unreferenced once its parent entry goes.
            unsafe { crate::memory::allocator::free_frame(frame_of(pte_phys(l0))) };
            freed += 1;
        }
        // SAFETY: every entry below it has been dealt with, and nothing else points at it.
        unsafe { crate::memory::allocator::free_frame(frame_of(t1)) };
        freed += 1;
    }
    // SAFETY: contract delegated to the caller - the root belongs to a task that is finished and is
    // not the live one.
    unsafe { crate::memory::allocator::free_frame(frame_of(root)) };
    freed + 1
}
