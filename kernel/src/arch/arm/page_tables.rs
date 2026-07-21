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
const L2_TABLES: usize = 64;
#[repr(align(1024))]
struct L2Arena([[u32; 256]; L2_TABLES]);
static mut L2_ARENA: L2Arena = L2Arena([[0; 256]; L2_TABLES]);
static L2_NEXT: AtomicUsize = AtomicUsize::new(0);

/// Fresh L1 tables (16 KiB each, 16 KiB aligned) for `PageTable::new`. One per address space: the boot
/// loader selftest takes one, and each live service takes one. Sized (with headroom) for the running
/// service set; becomes `alloc_frame` when the neutral allocator owns these frames.
const L1_TABLES: usize = 8;
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
        // The zeroed table must reach the PoC before the non-cacheable walker reads any entry.
        clean_dcache(t as u32, 1024);
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
        clean_dcache(t as u32, 16384); // zeroed L1 -> PoC for the non-cacheable walker
        Some(t as u32)
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
fn clean_dcache(addr: u32, len: u32) {
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
            let l2 = alloc_l2().ok_or(MapError::FrameAllocFailed)?;
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
    let active = (read_page_table_base() as u32) & 0xFFFF_C000;
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
    }
}

// ---- The remaining neutral surface (honest stubs / no-ops for the kernel-only path) ----

/// ARM runs identity-mapped (VA == PA), so hhdm=0 is the correct value, not "unset".
pub const PHYS_IS_IDENTITY: bool = true;

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
        let slot = l1.add((TEST_VA_RW >> 20) as usize);
        slot.write_volatile(0);
        clean_dcache(slot as u32, 4);
        invalidate_tlb_page(TEST_VA_RW as u64);
    }
}
