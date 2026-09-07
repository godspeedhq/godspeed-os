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
