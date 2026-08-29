// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7-A two-level page tables - 4 KiB pages, the machinery per-task address spaces need.
//!
//! `mmu.rs` brought translation up with **1 MiB sections** - the coarse form, enough to get the MMU
//! on and identity-map the world. This is the fine form: a second-level table under an L1 entry, so
//! individual 4 KiB pages can be mapped with their own permissions. That is what a real address space
//! is made of, and what the neutral kernel's `page_tables` surface (`PageTable::new`/`map`, the TLB
//! primitives) is implemented in terms of.
//!
//! **Short descriptors, two levels:**
//! - **L1** (already built by `mmu.rs`): 4096 entries x 4 bytes. An entry is either a 1 MiB *section*
//!   or a *pointer* to an L2 table (bits `[1:0] = 0b01`).
//! - **L2**: 256 entries x 4 bytes = 1 KiB, each a 4 KiB *small page* (bits `[1:0] = 0b1x`).
//!
//! **Permissions use the AP + APX split**, and getting it wrong is how a port ends up with either no
//! protection or unusable memory. The four cases this file needs: kernel RW is `APX=0, AP=0b01`;
//! kernel RO is `APX=1, AP=0b01`. (PL0/user variants arrive with real user tasks.)
//!
//! **The frame source is a static arena, deliberately.** `PageTable::new` on x86 pulls L1/L2 frames
//! from the neutral `alloc_frame`, which needs `memory::init` and a real memory map - and that pulls
//! in Limine-shaped assumptions (`protect_kernel_page_table_frames`) that are their own integration
//! step, not this one. So table memory comes from a bounded static arena here (§26.6.1), with the
//! allocator swap called out as the remaining seam. The *algorithm* - build an L2, point an L1 entry
//! at it, encode the page - is the real one, identical to what the neutral path will drive.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::memory::frame::PhysAddr;
use super::pl011_write;
use super::exceptions::write_hex32;

pub const PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtAddr(pub u64);

bitflags::bitflags! {
    /// Neutral page flags. The names are x86-flavoured (the documented leak, `arch/CLAUDE.md`); the
    /// ARM encoder below maps them onto short-descriptor bits. `WRITABLE` off = read-only; `NO_EXEC`
    /// sets XN; `USER` is accepted for signature parity but PL0 mappings are not built yet.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub struct PageFlags: u64 {
        const PRESENT  = 1 << 0;
        const WRITABLE = 1 << 1;
        const USER     = 1 << 2;
        const PWT      = 1 << 3;
        const PCD      = 1 << 4;
        // A framebuffer, not device registers: RAM the display controller scans out. Each arch
        // picks the weakest type that is still coherent without cache maintenance - on AArch64
        // Normal Non-cacheable, which gathers and buffers writes where Device-nGnRnE cannot. An
        // arch that has nothing better may ignore it and keep its uncached-MMIO type.
        const WRITE_COMBINE = 1 << 5;
        const NO_EXEC  = 1 << 63;
    }
}

#[derive(Debug)]
pub enum MapError { FrameAllocFailed, AlreadyMapped, NotMapped, VirtOutOfRange }

// ---- Descriptor encoding ----

const L1_TYPE_TABLE: u32 = 0b01;
const L2_TYPE_SMALL: u32 = 0b10; // small page; bit 0 (XN) is ORed in separately

/// Encode an L2 small-page descriptor for `pa` with the given flags.
///
/// Small-page descriptor. Three memory types, selected by the two cache-control flags:
///
/// - **Normal cacheable write-back** by default (TEX=0b001, C=1, B=1) - kernel and service RAM.
/// - **Device** (TEX=0b000, C=0, B=1) when `PCD` alone is set - uncached, no reordering, no gathering.
///   This is what a driver service's peripheral MMIO wants: every store is a register write with side
///   effects, so none of them may be merged or moved. (The same `PCD` the x86 encoder treats as uncached.)
/// - **Normal NON-cacheable** (TEX=0b001, C=0, B=0) when `PCD | PWT` are both set - uncached, but
///   gathering and reordering are allowed. This is what a **framebuffer** wants: a pixel store has no
///   side effect, it is just memory the display happens to scan, so forbidding the write buffer from
///   merging a run of them costs a bus transaction per pixel and buys nothing. It is also the attribute
///   `mmu::section_fb` gives the kernel's own mapping of those same physical pages, and they MUST agree -
///   ARM leaves mismatched memory attributes for one physical page UNPREDICTABLE.
///
/// AP/APX come from `WRITABLE`; XN from `NO_EXEC`. `S` (shareable) matches the section mappings `mmu.rs`
/// made, so a page and a section covering the same memory agree on shareability.
fn l2_small_page(pa: u32, flags: PageFlags) -> u32 {
    let mut d = (pa & 0xFFFF_F000) | L2_TYPE_SMALL;
    if flags.contains(PageFlags::PCD) && flags.contains(PageFlags::PWT) {
        // Normal non-cacheable: TEX=0b001, C=0, B=0. Uncached but gathering - see the note above.
        d |= 0b001 << 6;
    } else if flags.contains(PageFlags::PCD) {
        // Device: TEX=0b000, C=0, B=1 (Shareable Device) - correct for MMIO, never cached or reordered.
        d |= 1 << 2; // B
    } else {
        // Normal WB/WA: TEX[2:0] at bits [8:6] = 0b001, C bit 3, B bit 2.
        d |= 0b001 << 6;
        d |= 1 << 3; // C
        d |= 1 << 2; // B
    }
    d |= 1 << 10; // S (shareable), matching mmu.rs sections
    // AP/APX encode both privilege levels. USER = PL0 gets access; without it PL0 has none (kernel
    // page). AP=0b11 is PL1 RW / PL0 RW; AP=0b10 is PL1 RW / PL0 RO; AP=0b01 is PL1 RW / PL0 none;
    // APX=1 turns the PL1 half read-only. That is the whole security model of a page in two bits.
    match (flags.contains(PageFlags::USER), flags.contains(PageFlags::WRITABLE)) {
        (true, true)   => d |= 0b11 << 4,             // PL0 RW
        (true, false)  => d |= 0b10 << 4,             // PL0 RO
        (false, true)  => d |= 0b01 << 4,             // PL1 RW, PL0 none
        (false, false) => { d |= 0b01 << 4; d |= 1 << 9; } // PL1 RO, PL0 none
    }
    if flags.contains(PageFlags::NO_EXEC) {
        d |= 1; // XN (bit 0 of a small-page descriptor)
    }
    d
}

/// Encode an L1 descriptor pointing at an L2 table at `l2_pa` (1 KiB aligned), domain 0.
fn l1_table_ptr(l2_pa: u32) -> u32 {
    (l2_pa & 0xFFFF_FC00) | L1_TYPE_TABLE
}

// ---- Static table arena (the seam to the neutral frame allocator) ----

/// L2 tables: 1 KiB each, 1 KiB aligned. A bump allocator hands them out. The count is a fixed,
/// visible bound (§26.6.1); each covers 1 MiB of 4 KiB pages, and a service uses ~3-4 (its code, ctx,
/// stack, plus the kernel-identity fill). Sized for the boot loader selftest plus several concurrent
/// services (IPC pair, supervisor, shell); the whole arena is replaced by `alloc_frame` once
/// `memory::init` owns page-table frames on ARM.
/// Reclaimable now: a per-slot `used` flag replaces the old bump counter, so a slot freed on task death
/// (`free_l2`) is handed out again by the next spawn. Without this a restart storm (`chaos max-carnage`)
/// would exhaust the arena in a handful of respawns. Headroom is for the concurrent-live set plus the
/// brief overlap of a dying and its replacement instance, NOT the cumulative spawn count.
const L2_TABLES: usize = 128;
#[repr(align(1024))]
struct L2Arena([[u32; 256]; L2_TABLES]);
static mut L2_ARENA: L2Arena = L2Arena([[0; 256]; L2_TABLES]);
static L2_USED: [AtomicBool; L2_TABLES] = [const { AtomicBool::new(false) }; L2_TABLES];

/// The L1 root that ALLOCATED each L2 slot (0 = unowned). This is the reclaim discriminator, and it has
/// to be exact: `fill_kernel_identity` copies the SPAWNER's L1 table pointers into a child that does not
/// map that megabyte (a smaller binary inherits the spawner's higher binary L2s), so the child's L1
/// then points at L2s it does NOT own. Freeing those on the child's death would free the spawner's LIVE
/// pages (a real use-after-free - the double-free the allocator bitmap only *happened* to catch, and the
/// UNDEF from a live service's code being reused). `reclaim_user_frames` frees an L2 (and its pages)
/// only when `L2_OWNER == the dying task's root`; an inherited/shared L2 is owned by someone else and
/// left alone.
static L2_OWNER: [AtomicU32; L2_TABLES] = [const { AtomicU32::new(0) }; L2_TABLES];

/// First-alias-only latch for the reclaim dedup log (aliasing is expected + handled, so log once).
static RECLAIM_ALIAS_LOGGED: AtomicBool = AtomicBool::new(false);
/// Has a device-MMIO skip been reported? Said once: it is the CORRECT outcome for every driver death, so
/// it is a fact worth stating rather than an event worth repeating (§26.7 - loud is a budget).
static RECLAIM_DEVICE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Map an L2 physical base to its arena slot index (the arena is a contiguous static array of 1 KiB
/// tables). `None` for an address outside the arena (e.g. a kernel L2 not from this arena).
fn l2_slot(pa: u32) -> Option<usize> {
    let base = core::ptr::addr_of!(L2_ARENA) as u32;
    if pa < base { return None; }
    let idx = ((pa - base) / 1024) as usize; // each L2 = 256 * 4 = 1024 bytes
    if idx < L2_TABLES { Some(idx) } else { None }
}

/// Fresh L1 tables (16 KiB each, 16 KiB aligned) for `PageTable::new`. One per address space (the boot
/// loader selftest and each live service). Reclaimable like the L2 arena.
const L1_TABLES: usize = 16;
#[repr(align(16384))]
struct L1Arena([[u32; 4096]; L1_TABLES]);
static mut L1_ARENA: L1Arena = L1Arena([[0; 4096]; L1_TABLES]);
static L1_USED: [AtomicBool; L1_TABLES] = [const { AtomicBool::new(false) }; L1_TABLES];

/// Hand out a zeroed L2 table for the address space rooted at `owner`; returns its physical
/// (== virtual, identity-mapped) address. Claims the first free slot (CAS on its `used` flag) and
/// records `owner` so `reclaim_user_frames` frees it only for its true owner, never an inheritor.
fn alloc_l2(owner: u32) -> Option<u32> {
    for i in 0..L2_TABLES {
        if L2_USED[i].compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            L2_OWNER[i].store(owner, Ordering::Relaxed);
            // SAFETY: we exclusively claimed slot `i`; no other caller can alias it until `free_l2`.
            // The arena is 1 KiB aligned as the L1 pointer descriptor requires.
            unsafe {
                let t = core::ptr::addr_of_mut!(L2_ARENA.0[i]);
                (*t) = [0; 256];
                // The zeroed table must reach the PoC before the non-cacheable walker reads any entry.
                clean_dcache(t as u32, 1024);
                return Some(t as u32);
            }
        }
    }
    None
}

/// Return an L2 table to the arena (mapping its physical base back to a slot index) and clear its
/// owner. Called from `reclaim_user_frames` for a dead task's own L2s. Out-of-arena addresses are
/// ignored defensively.
fn free_l2(pa: u32) {
    if let Some(idx) = l2_slot(pa) {
        L2_OWNER[idx].store(0, Ordering::Relaxed);
        L2_USED[idx].store(false, Ordering::Release);
    }
}

fn alloc_l1() -> Option<u32> {
    for i in 0..L1_TABLES {
        if L1_USED[i].compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            // SAFETY: As `alloc_l2`; 16 KiB aligned as TTBR0 requires.
            unsafe {
                let t = core::ptr::addr_of_mut!(L1_ARENA.0[i]);
                (*t) = [0; 4096];
                clean_dcache(t as u32, 16384); // zeroed L1 -> PoC for the non-cacheable walker
                return Some(t as u32);
            }
        }
    }
    None
}

/// Return an L1 table (an address-space root) to the arena. Called by `free_page_table_root` from the
/// kill path. Out-of-arena addresses are ignored defensively.
fn free_l1(pa: u32) {
    let base = core::ptr::addr_of!(L1_ARENA) as u32;
    if pa < base { return; }
    let idx = ((pa - base) / 16384) as usize; // each L1 = 4096 * 4 = 16384 bytes
    if idx < L1_TABLES {
        L1_USED[idx].store(false, Ordering::Release);
    }
}

/// Clean `len` bytes from `addr` out of the D-cache to the Point of Coherency (`DCCMVAC`), then
/// `dsb`.
///
/// **This is the fix for a hardware-only bug QEMU cannot show.** `mmu.rs` leaves table walks
/// *non-cacheable*, so the hardware page-table walker reads descriptors from the PoC - but ordinary
/// stores land in the write-back D-cache first. A descriptor written and not cleaned is invisible to
/// the walker: on real silicon the very read translation faults, while QEMU's flat memory model honours
/// it regardless. This is the same class as SEC-28 (DMA coherence): a second observer (here the walker)
/// that does not go through the CPU's cache. Cortex-A7 lines are 64 bytes; a 32-byte stride is a safe
/// lower bound.
pub(super) fn clean_dcache(addr: u32, len: u32) {
    let mut p = addr & !31;
    let end = addr + len;
    while p < end {
        // SAFETY: `DCCMVAC` (`c7, c10, 1`) cleans one cache line by MVA to the PoC - no memory is
        // modified, only written back. `p` walks the descriptor bytes just written.
        unsafe {
            core::arch::asm!("mcr p15, 0, {a}, c7, c10, 1", a = in(reg) p, options(nostack));
        }
        p += 32;
    }
    // SAFETY: `dsb` orders the cleans before any subsequent table walk observes the memory.
    unsafe { core::arch::asm!("dsb", options(nostack)) }
}

// ---- TLB + TTBR0 primitives (the neutral surface) ----

/// Invalidate one TLB entry by VA (`TLBIMVA`), then a barrier so the change is visible before the
/// next translation. On ARM this is what stops a stale mapping being honoured after a remap.
pub unsafe fn invalidate_tlb_page(addr: u64) {
    // SAFETY: `mcr p15, 0, _, c8, c7, 1` is TLBIMVA at PL1; DSB/ISB order it before subsequent
    // fetches. Caller ensures `addr` is the VA whose mapping just changed.
    unsafe {
        core::arch::asm!(
            "mcr p15, 0, {a}, c8, c7, 1",
            "dsb",
            "isb",
            a = in(reg) (addr as u32) & 0xFFFF_F000,
            options(nostack),
        );
    }
}

pub fn read_page_table_base() -> u64 {
    let ttbr0: u32;
    // SAFETY: reading TTBR0 (`c2, c0, 0`) is a side-effect-free PL1 register read.
    unsafe {
        core::arch::asm!("mrc p15, 0, {t}, c2, c0, 0", t = out(reg) ttbr0, options(nomem, nostack));
    }
    ttbr0 as u64
}

/// Install a new address space (`TTBR0`), then ISB so the next fetch uses it. Per SEC-26/27 a real
/// ASID switch also needs TLB maintenance; while every task shares the identity map that never
/// happens, and the obligation is documented for when private address spaces land.
pub unsafe fn write_page_table_base(base: u64) {
    // SAFETY: `mcr p15, 0, _, c2, c0, 0` writes TTBR0 at PL1; ISB ensures the following instruction
    // is fetched under the new tables. Caller guarantees `base` is a valid 16 KiB-aligned L1.
    unsafe {
        core::arch::asm!(
            "mcr p15, 0, {b}, c2, c0, 0",
            "isb",
            b = in(reg) base as u32,
            options(nostack),
        );
    }
}

/// Map a 4 KiB page into the **live** L1 (the identity tables from `mmu.rs`).
///
/// The safe, provable path for the kernel-only milestone: it does not disturb the running identity
/// map, it just fills in an L2 under a currently-*unmapped* L1 slot. (Converting a live 1 MiB section
/// to a page table would momentarily unmap running code.) Callers use a VA in the unmapped gap
/// between RAM end and the peripherals for exactly this reason.
pub unsafe fn map_in_active_tables(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    let va = virt as u32;
    let pa = phys as u32;
    let l1_index = (va >> 20) as usize;
    let l2_index = ((va >> 12) & 0xFF) as usize;

    // The live L1 base is TTBR0 (its low bits are attributes; mask to the 16 KiB-aligned table).
    let l1_base = (read_page_table_base() as u32) & 0xFFFF_C000;

    // SAFETY: `l1_base` is the active L1 (identity-mapped, so readable at this address). We only touch
    // an entry that must currently be *invalid* (an unmapped slot); refusing to overwrite a live
    // section is what keeps running code mapped.
    unsafe {
        let l1 = l1_base as *mut u32;
        let existing = l1.add(l1_index).read_volatile();

        let l2_base = if existing & 0b11 == L1_TYPE_TABLE {
            (existing & 0xFFFF_FC00) as *mut u32 // already a table
        } else if existing == 0 {
            // The live kernel map owns any L2 it splits (l1_base = the active kernel L1), so no service
            // reclaim ever frees it (a service that inherits it is not its owner).
            let l2 = alloc_l2(l1_base).ok_or(MapError::FrameAllocFailed)?;
            l1.add(l1_index).write_volatile(l1_table_ptr(l2));
            clean_dcache(l1.add(l1_index) as u32, 4); // L1 entry -> PoC for the walker
            l2 as *mut u32
        } else {
            return Err(MapError::AlreadyMapped); // a live section - do not clobber
        };

        let pf = PageFlags::from_bits_truncate(flags);
        let ent = l2_base.add(l2_index);
        ent.write_volatile(l2_small_page(pa, pf));
        clean_dcache(ent as u32, 4); // L2 entry -> PoC
    }

    invalidate_tlb_page(virt);
    Ok(())
}

// ---- Neutral PageTable API (real; exercised by the same encoders the selftest proves) ----

pub struct PageTable {
    root: u32,
}

impl PageTable {
    /// A fresh, empty address space: a zeroed L1 from the arena. Every entry invalid until mapped.
    pub fn new() -> Result<Self, MapError> {
        alloc_l1().map(|root| PageTable { root }).ok_or(MapError::FrameAllocFailed)
    }

    /// Map `virt -> phys` in *this* table (not the live one). Builds an L2 under the L1 slot as
    /// needed and writes the small-page descriptor - the same encoders `map_in_active_tables` uses
    /// and the selftest proves.
    pub fn map(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        // REJECT a virtual address this architecture cannot express. Do not truncate it.
        //
        // `virt.0` is a u64 because the page-table API is arch-neutral; ARMv7 short descriptors
        // address 32 bits. `virt.0 as u32` silently discards the high half, and a discarded high half
        // does not fail - it MAPS SOMEWHERE ELSE. A 64-bit constant carried over from x86 is the
        // realistic source, and this is what one did:
        //
        //   spawn[dma]: 'dwc2' arena phys 0x2f41000 -> VA 0x200000000 (64 KiB)
        //
        // 0x2_0000_0000 truncates to 0x0, so the arena was mapped over L1 entry 0 - the identity
        // mapping of the low megabyte, which on this board holds the kernel image and the exception
        // vectors. The task then made no progress and printed nothing, and the liveness watchdog
        // panicked ten seconds later pointing at the task rather than at the mapping that had
        // destroyed its address space.
        //
        // The MMIO grant one commit earlier failed LOUDLY with `MapFailed` for the same root cause
        // and cost one boot to find. This path silently corrupted memory instead and cost three.
        // Same bug, and the only difference in what it cost was whether anything checked.
        if virt.0 > u32::MAX as u64 || phys.0 > u32::MAX as u64 {
            return Err(MapError::VirtOutOfRange);
        }
        let va = virt.0 as u32;
        let pa = phys.0 as u32;
        let l1_index = (va >> 20) as usize;
        let l2_index = ((va >> 12) & 0xFF) as usize;

        // SAFETY: `self.root` is an arena L1 (identity-mapped, so writable here); we own it
        // exclusively (`&mut self`). Entries start invalid, so a table pointer we write is fresh.
        unsafe {
            let l1 = self.root as *mut u32;
            let existing = l1.add(l1_index).read_volatile();
            let l2_base = if existing & 0b11 == L1_TYPE_TABLE {
                (existing & 0xFFFF_FC00) as *mut u32
            } else if existing == 0 {
                // This L2 belongs to THIS address space (self.root); reclaim frees it only for this
                // root, never for a child that later inherits the pointer via fill_kernel_identity.
                let l2 = alloc_l2(self.root).ok_or(MapError::FrameAllocFailed)?;
                l1.add(l1_index).write_volatile(l1_table_ptr(l2));
                clean_dcache(l1.add(l1_index) as u32, 4);
                l2 as *mut u32
            } else {
                return Err(MapError::AlreadyMapped);
            };
            let ent = l2_base.add(l2_index);
            if ent.read_volatile() & 0b11 != 0 {
                return Err(MapError::AlreadyMapped);
            }
            ent.write_volatile(l2_small_page(pa, flags));
            clean_dcache(ent as u32, 4);
        }
        Ok(())
    }

    pub fn cr3_value(&self) -> u64 {
        self.root as u64
    }
    pub fn into_cr3(self) -> u64 {
        self.root as u64
    }
}

/// Copy the live kernel identity map into a service page table, so the kernel is reachable (as
/// privileged memory) while running under that table - which it must be, or the service's very first
/// `svc` would fault with the vectors/kernel unmapped.
///
/// Copies each active L1 entry into the service L1 **only where the service L1 is empty**, so the
/// service's own USER pages (its code, stack, and context, at their own L1 slots) are never
/// overwritten. The kernel sections are PL1-only, so a PL0 service still cannot touch them - it is
/// present-but-privileged, exactly the split a user/kernel address space needs.
///
/// # Safety
/// `pt_root` must be a service L1 (16 KiB aligned) built by `PageTable::new`, not yet in use.
pub unsafe fn fill_kernel_identity(pt_root: u32) {
    // Copy from the kernel's OWN boot L1, never from the active root. The active root is the
    // spawner's (or, on a kernel-driven respawn, possibly a DEAD task's): copying it gave the child
    // the spawner's PL0 user entries - which the child's reclaim later freed out from under the LIVE
    // spawner (the supervisor-corruption death loop) - and could inherit dangling L2 pointers from a
    // reclaimed space. The kernel L1 holds only kernel sections + kernel-owned L2s, and cannot die.
    // (See `mmu::kernel_l1_root`.) It also restores invariant 2: a child no longer sees one byte of
    // its spawner's memory.
    let active = super::mmu::kernel_l1_root() & 0xFFFF_C000;
    // SAFETY: both L1s are identity-mapped RAM. For each 1 MiB slot:
    //  - service slot empty  -> copy the kernel's section wholesale (fast, the common case).
    //  - service slot is a TABLE over a kernel SECTION -> the service mapped a *page* in this 1 MiB
    //    (its ctx at 0x3ff000), so the kernel's own data elsewhere in the SAME 1 MiB (the per-core
    //    arenas the allocator handed out just above the reserve) would be left unmapped. Fill the
    //    service L2's empty entries with kernel identity PAGES so that data stays reachable. This is
    //    the fault the first version hit (0x370004): kernel data sharing the ctx's 1 MiB.
    unsafe {
        let src = active as *const u32;
        let dst = pt_root as *mut u32;
        for i in 0..4096 {
            let s = src.add(i).read_volatile();
            let d = dst.add(i).read_volatile();
            if d == 0 {
                dst.add(i).write_volatile(s);            // whole-section copy
            } else if d & 0b11 == L1_TYPE_TABLE && s & 0b11 == 0b10 {
                // Kernel section under a service table: fill the L2's holes with kernel pages.
                let l2 = (d & 0xFFFF_FC00) as *mut u32;
                let sect_base = s & 0xFFF0_0000;         // the 1 MiB physical base
                for j in 0..256 {
                    if l2.add(j).read_volatile() == 0 {
                        let page_pa = sect_base | (j as u32) << 12;
                        // Kernel RW, PL0 none (PRESENT|WRITABLE) - present but privileged.
                        l2.add(j).write_volatile(l2_small_page(page_pa, PageFlags::PRESENT | PageFlags::WRITABLE));
                    }
                }
                clean_dcache(l2 as u32, 1024);
            } else if d & 0b11 == L1_TYPE_TABLE && s & 0b11 == L1_TYPE_TABLE {
                // BOTH the child and the source split this 1 MiB into pages: each mapped its own ctx at
                // 0x3ff000, so both L1 entries are TABLEs. The two branches above assume the SOURCE is a
                // kernel SECTION; when a *service* spawns another service (the shell spawning
                // observe-now), the spawner ALSO split this megabyte, so its kernel pages live in its L2
                // - and neither branch above fires. Left unfixed, the kernel data this 1 MiB holds (an
                // embedded service ELF at 0x34xxxx - .rodata is ~21 MiB of include_bytes! - and the
                // per-core arenas) is unmapped in the child, and the child's ELF loader faults reading
                // the ELF magic (0x34eb68). Fill the child L2's HOLES from the source L2's present
                // entries; the child's own ctx page is already non-zero, so it is never overwritten.
                let dl2 = (d & 0xFFFF_FC00) as *mut u32;
                let sl2 = (s & 0xFFFF_FC00) as *const u32;
                for j in 0..256 {
                    if dl2.add(j).read_volatile() == 0 {
                        let sp = sl2.add(j).read_volatile();
                        // Copy only PRIVILEGED entries (AP[1:0] == 0b01). With the kernel L1 as the
                        // source this is a no-op filter (kernel pages are all privileged) - it exists
                        // so no PL0 entry can EVER be copied into this child-owned L2, because the
                        // child's reclaim frees PL0 entries in owned L2s as its own. A copied user
                        // page here is a frame the child would free out from under its real owner.
                        if sp != 0 && (sp >> 4) & 0b11 == 0b01 { dl2.add(j).write_volatile(sp); }
                    }
                }
                clean_dcache(dl2 as u32, 1024);
            }
        }
    }
    // The whole L1 must reach the PoC before the (non-cacheable) walker reads it under the new TTBR0.
    clean_dcache(pt_root, 16384);
}

/// Clean + invalidate the entire L1 data cache by set/way (`DCCISW`).
///
/// Table walks are non-cacheable (`mmu.rs`: TTBR0 carries no cacheability attributes), so a page
/// table's descriptors must reach the point of coherency before the walker reads them under a new
/// TTBR0. `fill_kernel_identity` and the loader write those descriptors while the D-cache is on, so a
/// service's whole page table is flushed **once** with this before it is ever scheduled; thereafter
/// `switch_context` only re-points TTBR0 and flushes the TLB, needing no further cache maintenance
/// (the descriptors do not change after spawn). This is also why the first direct spawn cleaned here
/// before switching TTBR0 - the same one-shot, hoisted to spawn time for the scheduled path.
///
/// # Safety
/// A pure cache-maintenance sweep with no memory effects; reads CCSIDR to size the cache.
pub(super) unsafe fn clean_invalidate_dcache_all() {
    // SAFETY: set/way D-cache clean+invalidate is a PL1 maintenance operation with no memory effects
    // beyond making the D-cache coherent with memory. Sizes the cache from CCSIDR/CSSELR.
    unsafe {
        core::arch::asm!(
            "mov  {t0}, #0",
            "mcr  p15, 2, {t0}, c0, c0, 0", // CSSELR = L1 data cache
            "isb",
            "mrc  p15, 1, {t0}, c0, c0, 0", // CCSIDR
            "and  {t1}, {t0}, #7",          // line size (log2 words - 2)
            "add  {t1}, {t1}, #4",          // + word/byte shift
            "ubfx {t2}, {t0}, #3, #10",     // associativity - 1 (ways)
            "ubfx {t3}, {t0}, #13, #15",    // num sets - 1
            "clz  {t4}, {t2}",              // way position shift
            "2:",                           // set loop ({t3} = current set)
            "mov  {t5}, {t2}",              // ways
            "3:",                           // way loop ({t5} = current way)
            "lsl  {t6}, {t5}, {t4}",        // way << A
            "lsl  {t0}, {t3}, {t1}",        // set << L (t0 reused as scratch)
            "orr  {t6}, {t6}, {t0}",        // set/way value
            "mcr  p15, 0, {t6}, c7, c14, 2",// DCCISW - clean+invalidate by set/way
            "subs {t5}, {t5}, #1",
            "bge  3b",
            "subs {t3}, {t3}, #1",
            "bge  2b",
            "dsb",
            "isb",
            t0 = out(reg) _, t1 = out(reg) _, t2 = out(reg) _, t3 = out(reg) _,
            t4 = out(reg) _, t5 = out(reg) _, t6 = out(reg) _,
            options(nostack),
        );
    }
}

/// Finalize a freshly-built service page table for use as a TTBR0 (called by the neutral spawn after
/// all of the service's own regions are mapped). On ARM this is two steps x86 does not need: clone the
/// kernel identity map into the service L1 (so the vectors/kernel/peripherals stay reachable, as
/// privileged memory, once TTBR0 is switched to this table), and clean the D-cache so the non-cacheable
/// table walker sees every descriptor. The x86 kernel is shared higher-half, so its hook is a no-op.
///
/// # Safety
/// `cr3` must be the root of a service page table built by `PageTable::new` and not yet in use.
pub unsafe fn finalize_service_address_space(cr3: u64) {
    // SAFETY: cr3 is the service L1 root; fill_kernel_identity + the D-cache clean are the exact steps
    // the direct-spawn path (spawn.rs) does by hand before entering a service.
    unsafe {
        fill_kernel_identity(cr3 as u32);
        clean_invalidate_dcache_all();
        publish_user_pages_to_other_cores(cr3 as u32);
    }
}

/// Make this freshly-loaded address space visible to the table walker and to every other core:
/// the mapped PAGES, every L2 table, and the L1 root - all cleaned to the PoC by MVA.
///
/// **The set/way sweep above is not enough, and that is an ARM trap worth stating plainly.** Set/way
/// maintenance (`DCCISW`) is **local to the core that executes it** - it is never broadcast, even with
/// `ACTLR.SMP` set. Only **MVA-based** operations are broadcast to the inner-shareable domain. So at
/// spawn: the loader's writes (the service's text, rodata and data) sit in the SPAWNING core's D-cache,
/// the set/way sweep pushes them out to RAM - and another core that still holds STALE lines for those
/// same physical frames, recycled from a task that died earlier, never hears about it. It then runs the
/// service against its own stale cache.
///
/// The service still *executes* (its text is mostly refetched) but every pointer it loads from its own
/// data is garbage, which is exactly the observed signature: a correct `SP_usr`, faults at wild
/// addresses (`0xfffffffc`, `0x400`, just past the stack top) and only ever on a core other than the
/// spawner. QEMU cannot show it - TCG has no per-core caches.
///
/// So walk the new address space and clean+invalidate each USER page **by MVA** (`DCCIMVAC`), which is
/// broadcast, then invalidate the instruction cache and branch predictor across the domain
/// (`ICIALLUIS`/`BPIALLIS`) since the text was freshly written too. Pages are identity-mapped on this
/// port, so the physical address is the address to maintain.
unsafe fn publish_user_pages_to_other_cores(root: u32) {
    let svc_l1 = (root & 0xFFFF_C000) as *const u32;
    // SAFETY: `root` is a complete service L1 built by `PageTable::new`; every table/page address read
    // from it is identity-mapped RAM. Cache maintenance has no memory effects beyond coherency.
    unsafe {
        for i in 0..4096usize {
            let e1 = svc_l1.add(i).read_volatile();
            if e1 & 0b11 != L1_TYPE_TABLE { continue; } // unmapped, or a shared kernel SECTION
            let l2 = (e1 & 0xFFFF_FC00) as *const u32;
            for j in 0..256usize {
                let e2 = l2.add(j).read_volatile();
                if e2 & 0b11 == 0 { continue; }             // invalid
                if (e2 >> 4) & 0b11 < 0b10 { continue; }    // not PL0-accessible => not this service's
                clean_invalidate_dcache_range(e2 & 0xFFFF_F000, 0x1000);
            }
            // ...and the L2 TABLE ITSELF (1 KiB). See below - the descriptors matter as much as the
            // pages they describe, and this loop was publishing only the pages.
            clean_invalidate_dcache_range(e1 & 0xFFFF_FC00, 0x400);
        }
        // The L1 TABLE (16 KiB). THIS is the one that was missing, and it is the whole bug:
        //
        // The table walker is configured NON-CACHEABLE (TTBR0 carries no cacheability attributes), so
        // it reads descriptors from the point of coherency - not through the D-cache the kernel wrote
        // them with. Publishing them was left entirely to `clean_invalidate_dcache_all`, a SET/WAY
        // sweep, and this file already states why that is not enough: set/way is local to the core that
        // runs it and is never broadcast. On a Cortex-A7 it also cleans L1 into L2 rather than to the
        // PoC. So a table could be complete and correct in memory-as-the-CPU-sees-it while the walker
        // still read the previous contents of those frames.
        //
        // The symptom is a fault that looks impossible: `mem-pressure` took a PERMISSION fault at
        // 0x0040002c, an address squarely inside its own valid R-X segment. Not a wrong mapping - a
        // stale read of a right one. It showed up under a respawn storm because that is when tables are
        // built and torn down fastest, and when freed frames are recycled into new tables soonest.
        //
        // MVA maintenance is the correct instrument: it reaches the PoC and is broadcast to the inner
        // shareable domain, which is exactly what a non-cacheable walker on any core requires.
        clean_invalidate_dcache_range(root & 0xFFFF_C000, 0x4000);
        core::arch::asm!(
            "dsb",
            "mcr p15, 0, {z}, c7, c1, 0",  // ICIALLUIS - I-cache invalidate all, inner shareable
            "mcr p15, 0, {z}, c7, c1, 6",  // BPIALLIS  - branch predictor invalidate, inner shareable
            "dsb",
            "isb",
            z = in(reg) 0u32,
            options(nostack),
        );
    }
}

/// Clean **and invalidate** a range by MVA (`DCCIMVAC`). Unlike `clean_dcache` (which only cleans) this
/// also evicts the line, and unlike the set/way sweep it is **broadcast to the inner-shareable domain**,
/// so other cores drop any stale copy of the same physical memory.
fn clean_invalidate_dcache_range(addr: u32, len: u32) {
    let mut p = addr & !31;
    let end = addr.saturating_add(len);
    // `p >= addr` is the wrap guard: at the very top of the address space `p += 32` overflows to 0
    // and, with overflow checks off in release, would loop forever. The sibling `flush_dcache` uses
    // wrapping arithmetic and exits naturally; this one did not.
    while p < end && p >= addr {
        // SAFETY: `DCCIMVAC` (`c7, c14, 1`) cleans+invalidates one line by MVA to the PoC. No memory is
        // modified; identity-mapped physical addresses are valid MVAs on this port.
        unsafe {
            core::arch::asm!("mcr p15, 0, {a}, c7, c14, 1", a = in(reg) p, options(nostack));
        }
        p += 32;
    }
    // SAFETY: order the maintenance before the new address space is entered on any core.
    unsafe { core::arch::asm!("dsb", options(nostack)) }
}

// ---- The remaining neutral surface (honest stubs / no-ops for the kernel-only path) ----

/// ARM runs identity-mapped (VA == PA), so hhdm=0 is the correct value, not "unset".
pub const PHYS_IS_IDENTITY: bool = true;

/// No bootloader placed page tables for this port - the kernel builds its own, in `.bss` inside the
/// kernel image, which the memory map already excludes from usable RAM. So there is nothing for
/// `protect_kernel_page_table_frames` to protect, and its x86-format walk must not run here.
pub const BOOTLOADER_PLACED_TABLES: bool = false;


pub fn get_hhdm_offset() -> u64 { 0 }
pub unsafe fn set_hhdm_offset(_offset: u64) {}
pub fn entry_for_va(_virt: u64) -> Option<u64> { None }
pub fn unmap_4k_strided(_base: u64, _stride: u64, _count: usize) {}
pub fn harden_hhdm_nx() {}
/// Free every user frame a dying task owns, and return its own L2 tables to the arena; return the count
/// of frames freed. Mirrors x86's `reclaim_user_frames` for the ARM two-level short-descriptor tables.
///
/// The trap is the SHARED kernel identity map: `fill_kernel_identity` copies kernel sections AND the
/// SPAWNER's L1 table pointers into every service L1, so a service's L1 points at L2s it does NOT own
/// (kernel L2s, and a smaller binary inherits the spawner's higher binary L2s). Freeing those would
/// free the kernel's or a LIVE sibling's pages - a real use-after-free. The exact discriminator is
/// ownership, recorded at `alloc_l2` time:
/// - free an L2 (and its pages) only when `L2_OWNER[slot] == this task's root` - never an inherited or
///   kernel-owned L2;
/// - within an owned L2, free only PL0-accessible pages (AP[1:0] >= 0b10) - the USER pages the
///   loader/`map_stack_and_ctx` `alloc_frame`d - and leave the kernel hole-fill pages (AP == 0b01),
///   which point at kernel RAM and are shared.
/// Leaves the L1 ROOT for `free_page_table_root` (mirrors x86 leaving the PML4 to the root-free path).
///
/// # Safety
/// `cr3` is the Dead task's L1 root; no core will load it again, so its tables are ours to walk + free.
pub unsafe fn reclaim_user_frames(cr3: u64) -> usize {
    use crate::memory::frame::{Frame, PhysAddr};
    let root = (cr3 as u32) & 0xFFFF_C000;
    let svc_l1 = root as *const u32;
    let mut freed = 0usize;
    // A frame must be freed at most ONCE per reclaim: if the same physical page appears in two of this
    // task's owned L2 entries (an alias), freeing it twice would double-free (an uncaught UAF if it were
    // reallocated between). Dedup within the call, and log the first alias loudly so the source is
    // pinned (a service is ~76 frames; 512 is ample headroom, and past it we simply stop deduping).
    let mut seen: [u32; 512] = [0; 512];
    let mut nseen = 0usize;
    // SAFETY: the arena L1 is identity-mapped, readable here; the task is Dead, so its tables are ours.
    unsafe {
        for i in 0..4096usize {
            let svc = svc_l1.add(i).read_volatile();
            if svc & 0b11 != L1_TYPE_TABLE { continue; }   // 0 (unmapped) or a kernel SECTION
            let l2_pa = svc & 0xFFFF_FC00;
            // Only walk+free an L2 THIS root owns. A kernel L2 or one inherited from the spawner is
            // owned by someone else (or is outside the arena) - its live pages must NOT be freed here.
            match l2_slot(l2_pa) {
                Some(s) if L2_OWNER[s].load(Ordering::Relaxed) == root => {}
                _ => continue,
            }
            let l2 = l2_pa as *const u32;
            for j in 0..256usize {
                let e = l2.add(j).read_volatile();
                if e & 0b11 == 0 { continue; }                    // invalid entry
                if (e >> 4) & 0b11 >= 0b10 {                      // PL0 has access => USER page => ours
                    // ...but PL0 access does NOT mean "allocator RAM". A driver service maps its
                    // peripheral's registers into its own address space (§12), and that mapping is a
                    // user page too - so this walk was handing a device MMIO frame to `free_frame` on
                    // every driver death. On the Pi 2 that is physical 0x3F300000 (the EMMC block inside
                    // the 0x3F000000 peripheral window), and a chaos run produced it 141 times.
                    //
                    // It is contained today only by a bounds check DOWNSTREAM: the frame index lands
                    // above usable RAM, so the allocator rejects it as a phantom and logs a no-op. That
                    // is luck about where this SoC puts its peripherals. A device whose registers sit
                    // BELOW the top of RAM would pass the bounds check, enter the free pool, and be
                    // handed to a service as ordinary memory - which is a driver's registers becoming
                    // someone's heap, and then a double-free on the next kill.
                    //
                    // The mapping already says what it is, so ask it rather than infer from the address:
                    // normal service RAM is the ONLY thing `l2_small_page` encodes as TEX=0b001 + C=1.
                    // x86's reclaim has skipped its equivalent (PCD|PWT) since the chaos double-free was
                    // found there; this is the same rule, which the ARM port simply never carried over.
                    //
                    // Phrased as "is this allocator RAM", NOT as "is this Device". It used to test for
                    // Device specifically (TEX=0b000 + C=0), which silently stopped covering everything
                    // the moment a third memory type appeared: the `console` service's framebuffer grant
                    // is Normal NON-cacheable (TEX=0b001 + C=0), so it failed the Device test, passed as
                    // ordinary RAM, and was handed to `free_frame` - 1,755 pages of GPU memory on every
                    // console death. Contained only by the same downstream bounds check this comment
                    // already calls luck (the Pi's framebuffer sits above usable RAM). A board whose
                    // framebuffer sat below the top of RAM would have freed it into the general pool.
                    //
                    // An allowlist cannot rot this way. A new memory type is now skipped by default, and
                    // wrongly freeing a page requires deliberately encoding it as cacheable service RAM.
                    let is_service_ram = (e >> 6) & 0b111 == 0b001 && e & (1 << 3) != 0;
                    if !is_service_ram {
                        if !RECLAIM_DEVICE_LOGGED.swap(true, Ordering::Relaxed) {
                            crate::kprintln!(
                                "reclaim: skipped granted page pa={:#010x} (not allocator RAM; further skips silent)",
                                e & 0xFFFF_F000);
                        }
                        continue;
                    }
                    let pa = e & 0xFFFF_F000;
                    let mut dup = false;
                    for k in 0..nseen { if seen[k] == pa { dup = true; break; } }
                    if dup {
                        // A frame mapped at more than one VA in this task: free the physical page ONCE
                        // (the correct reclaim semantics), skip the duplicate. Log the FIRST such alias
                        // per boot only (it is expected/handled, not an error - rate-limited so a storm
                        // does not bury the console, §26.7).
                        if !RECLAIM_ALIAS_LOGGED.swap(true, Ordering::Relaxed) {
                            crate::kprintln!(
                                "reclaim: aliased frame pa={:#010x} at l1={} l2={} - freed once (further aliases silent)",
                                pa, i, j);
                        }
                        continue; // already freed this frame in this reclaim; do NOT free again
                    }
                    if nseen < seen.len() { seen[nseen] = pa; nseen += 1; }
                    free_frame(Frame::from_phys(PhysAddr(pa as u64)));
                    freed += 1;
                }
                // AP == 0b01 => kernel hole-fill page (shared) => leave it
            }
            free_l2(l2_pa); // return this task's own L2 to the arena
        }
    }
    freed
}

/// Free a dying task's page-table ROOT (its L1). On ARM the L1 is an ARENA slot, not a general-allocator
/// frame, so it goes back to the arena - never to `free_frame`, which would corrupt the frame bitmap
/// (the `alloc_frame returned kernel-range frame` panic that motivated this whole path). The neutral
/// kill path calls this instead of `free_frame` for the root (immediate non-self-kill + deferred drain).
///
/// # Safety
/// `root` is a Dead task's L1 root; no core will load this address space again.
pub unsafe fn free_page_table_root(root: u64) {
    free_l1((root as u32) & 0xFFFF_C000);
}

use crate::memory::allocator::free_frame;

// ---- Selftest: build a real 4 KiB mapping and prove translation + permissions ----

/// Translate `va` as a privileged **read** (`ATS1CPR`); `None` if it faults.
fn translate_read(va: u32) -> Option<u32> {
    let par: u32;
    // SAFETY: ATS1CPR (`c7, c8, 0`) walks the tables and writes PAR with no memory side effects; a
    // faulting VA sets PAR.F rather than raising an exception, which is why this is safe on an
    // address that may be unmapped.
    unsafe {
        core::arch::asm!(
            "mcr p15, 0, {va}, c7, c8, 0",
            "isb",
            "mrc p15, 0, {par}, c7, c4, 0",
            va = in(reg) va, par = out(reg) par, options(nostack),
        );
    }
    if par & 1 != 0 { None } else { Some((par & 0xFFFF_F000) | (va & 0xFFF)) }
}

/// Translate `va` as a privileged **write** (`ATS1CPW`); `None` if the write is not permitted.
///
/// This is the trick that proves read-only enforcement *without triggering a fault*: the CPU runs a
/// write-permission translation and reports the answer in PAR.F, so a RO page returns `None` here
/// while `translate_read` still returns the address.
fn translate_write(va: u32) -> Option<u32> {
    let par: u32;
    // SAFETY: ATS1CPW (`c7, c8, 1`) is the privileged-write counterpart of ATS1CPR; same no-side-
    // effect PAR semantics, and it is precisely designed to be a non-faulting permission probe.
    unsafe {
        core::arch::asm!(
            "mcr p15, 0, {va}, c7, c8, 1",
            "isb",
            "mrc p15, 0, {par}, c7, c4, 0",
            va = in(reg) va, par = out(reg) par, options(nostack),
        );
    }
    if par & 1 != 0 { None } else { Some((par & 0xFFFF_F000) | (va & 0xFFF)) }
}

/// Two spare VAs in the unmapped gap between RAM end (`0x3B40_0000`) and the peripherals
/// (`0x3F00_0000`) - a region `mmu.rs` deliberately left invalid, so mapping here disturbs nothing.
const TEST_VA_RW: u32 = 0x3C00_0000;
const TEST_VA_RO: u32 = 0x3C00_1000;

/// Prove the page-table machinery: map a page RW and another RO into the live tables, and confirm via
/// the CPU's own walker that both translate for read, RW is writable, and RO is **not**.
///
/// The negatives carry the weight (same discipline as the MMU and IOMMU selftests): "RW translates"
/// only shows the L2 was built; "RO refuses a write" shows the permission bits are actually enforced.
pub fn selftest() {
    // Back both test pages with a real frame each - the frames holding this kernel's low RAM are
    // fine to point at; we only translate, never overwrite them.
    let frame_rw = 0x0010_0000u32; // 1 MiB - inside kernel RAM, mapped Normal
    let frame_ro = 0x0010_1000u32;

    let rw = PageFlags::PRESENT | PageFlags::WRITABLE;
    let ro = PageFlags::PRESENT; // WRITABLE absent -> read-only

    // SAFETY: mapping into the active tables at VAs in the deliberately-unmapped RAM/peripheral gap;
    // single-threaded boot context. Errors are reported, not unwrapped-and-panicked.
    let m1 = unsafe { map_in_active_tables(TEST_VA_RW as u64, frame_rw as u64, rw.bits()) };
    let m2 = unsafe { map_in_active_tables(TEST_VA_RO as u64, frame_ro as u64, ro.bits()) };
    if m1.is_err() || m2.is_err() {
        pl011_write(b"arm32: pgtable selftest FAIL - map_in_active_tables returned an error\r\n");
        return;
    }

    let mut pass = true;

    // RW page: reads and writes both translate to the backing frame.
    match (translate_read(TEST_VA_RW), translate_write(TEST_VA_RW)) {
        (Some(r), Some(w)) if r == frame_rw && w == frame_rw => {}
        _ => {
            pl011_write(b"arm32:   RW page did not translate read+write to its frame\r\n");
            pass = false;
        }
    }

    // RO page: reads translate; writes are DENIED (the load-bearing check).
    match (translate_read(TEST_VA_RO), translate_write(TEST_VA_RO)) {
        (Some(r), None) if r == frame_ro => {}
        (Some(_), Some(_)) => {
            pl011_write(b"arm32:   RO page is WRITABLE - permission bits not enforced\r\n");
            pass = false;
        }
        _ => {
            pl011_write(b"arm32:   RO page did not translate for read\r\n");
            pass = false;
        }
    }

    pl011_write(b"arm32: pgtable selftest - RW frame ");
    write_hex32(frame_rw);
    pl011_write(b", RO frame ");
    write_hex32(frame_ro);
    pl011_write(b" (4 KiB pages via L2)\r\n");

    if pass {
        pl011_write(b"arm32: pgtable PASS (4 KiB map translates; read-only is enforced)\r\n");
    } else {
        pl011_write(b"arm32: pgtable FAIL - see above\r\n");
    }

    // Leave the tables as we found them: invalidate the two test entries. (They point into the gap,
    // so leaving them would be harmless, but tidy is better than harmless.)
    let l1_base = (read_page_table_base() as u32) & 0xFFFF_C000;
    // SAFETY: clearing the single L1 slot for the test gap (index 0x3C0); it was invalid before us.
    unsafe {
        let l1 = l1_base as *mut u32;
        let slot = l1.add((TEST_VA_RW >> 20) as usize);
        slot.write_volatile(0);
        clean_dcache(slot as u32, 4);
        invalidate_tlb_page(TEST_VA_RW as u64);
    }
}
