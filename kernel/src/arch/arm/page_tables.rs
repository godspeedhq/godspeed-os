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

use core::sync::atomic::{AtomicUsize, Ordering};

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
        const NO_EXEC  = 1 << 63;
    }
}

#[derive(Debug)]
pub enum MapError { FrameAllocFailed, AlreadyMapped, NotMapped }

// ---- Descriptor encoding ----

const L1_TYPE_TABLE: u32 = 0b01;
const L2_TYPE_SMALL: u32 = 0b10; // small page; bit 0 (XN) is ORed in separately

/// Encode an L2 small-page descriptor for `pa` with the given flags.
///
/// Normal, cacheable, write-back memory (TEX=0b001, C=1, B=1) - kernel RAM. AP/APX come from
/// `WRITABLE`; XN from `NO_EXEC`. `S` (shareable) is set to match the section mappings `mmu.rs` made,
/// so a page and a section covering the same memory agree on shareability.
fn l2_small_page(pa: u32, flags: PageFlags) -> u32 {
    let mut d = (pa & 0xFFFF_F000) | L2_TYPE_SMALL;
    // Normal WB/WA: TEX[2:0] at bits [8:6] = 0b001, C bit 3, B bit 2.
    d |= 0b001 << 6;
    d |= 1 << 3; // C
    d |= 1 << 2; // B
    d |= 1 << 10; // S (shareable), matching mmu.rs sections
    if flags.contains(PageFlags::WRITABLE) {
        d |= 0b01 << 4; // AP=0b01, APX=0 -> PL1 RW
    } else {
        d |= 0b01 << 4; // AP=0b01 ...
        d |= 1 << 9; //    ... + APX=1 -> PL1 RO
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
/// visible bound (§26.6.1); it is generous for the kernel-only path (each covers 1 MiB of 4 KiB
/// pages) and the whole arena is replaced by `alloc_frame` once `memory::init` is wired on ARM.
const L2_TABLES: usize = 16;
#[repr(align(1024))]
struct L2Arena([[u32; 256]; L2_TABLES]);
static mut L2_ARENA: L2Arena = L2Arena([[0; 256]; L2_TABLES]);
static L2_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Fresh L1 tables (16 KiB each, 16 KiB aligned) for `PageTable::new`. Two suffice with no real user
/// tasks yet; this too becomes `alloc_frame` when the neutral allocator is wired.
const L1_TABLES: usize = 2;
#[repr(align(16384))]
struct L1Arena([[u32; 4096]; L1_TABLES]);
static mut L1_ARENA: L1Arena = L1Arena([[0; 4096]; L1_TABLES]);
static L1_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Hand out a zeroed L2 table; returns its physical (== virtual, identity-mapped) address.
fn alloc_l2() -> Option<u32> {
    let i = L2_NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= L2_TABLES {
        return None;
    }
    // SAFETY: Boot-time, single-threaded; each index is handed out once by the atomic bump, so no two
    // callers alias the same table. The arena is 1 KiB aligned as the L1 pointer descriptor requires.
    unsafe {
        let t = core::ptr::addr_of_mut!(L2_ARENA.0[i]);
        (*t) = [0; 256];
        Some(t as u32)
    }
}

fn alloc_l1() -> Option<u32> {
    let i = L1_NEXT.fetch_add(1, Ordering::Relaxed);
    if i >= L1_TABLES {
        return None;
    }
    // SAFETY: As `alloc_l2`; 16 KiB aligned as TTBR0 requires.
    unsafe {
        let t = core::ptr::addr_of_mut!(L1_ARENA.0[i]);
        (*t) = [0; 4096];
        Some(t as u32)
    }
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
            let l2 = alloc_l2().ok_or(MapError::FrameAllocFailed)?;
            l1.add(l1_index).write_volatile(l1_table_ptr(l2));
            l2 as *mut u32
        } else {
            return Err(MapError::AlreadyMapped); // a live section - do not clobber
        };

        let pf = PageFlags::from_bits_truncate(flags);
        l2_base.add(l2_index).write_volatile(l2_small_page(pa, pf));
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
                let l2 = alloc_l2().ok_or(MapError::FrameAllocFailed)?;
                l1.add(l1_index).write_volatile(l1_table_ptr(l2));
                l2 as *mut u32
            } else {
                return Err(MapError::AlreadyMapped);
            };
            if l2_base.add(l2_index).read_volatile() & 0b11 != 0 {
                return Err(MapError::AlreadyMapped);
            }
            l2_base.add(l2_index).write_volatile(l2_small_page(pa, flags));
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

// ---- The remaining neutral surface (honest stubs / no-ops for the kernel-only path) ----

pub fn get_hhdm_offset() -> u64 { 0 }
pub unsafe fn set_hhdm_offset(_offset: u64) {}
pub fn entry_for_va(_virt: u64) -> Option<u64> { None }
pub fn unmap_4k_strided(_base: u64, _stride: u64, _count: usize) {}
pub fn harden_hhdm_nx() {}
pub unsafe fn reclaim_user_frames(_cr3: u64) -> usize { 0 }

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
        l1.add((TEST_VA_RW >> 20) as usize).write_volatile(0);
        invalidate_tlb_page(TEST_VA_RW as u64);
    }
}
