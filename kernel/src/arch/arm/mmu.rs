// SPDX-License-Identifier: GPL-2.0-only
//! ARMv7-A MMU - short-descriptor translation, 1 MB sections.
//!
//! This is the gate on everything above it: no MMU means no address spaces, so no task isolation and
//! no `kernel_main`. It is deliberately the step AFTER exception vectors, because a bad mapping is a
//! translation fault, and without vectors that fault is a silent hang. With them it prints
//! `translation fault (section) - NOT MAPPED` and the offending address.
//!
//! **Short descriptors, not long (LPAE).** ARMv7 offers both. Short is the simpler format - a flat
//! 4096-entry level-1 table where each entry maps 1 MB - and 1 MB granularity is all this milestone
//! needs: the whole point is to get translation ON and proven. Second-level 4 KB page tables (needed
//! for per-task address spaces and page-granular permissions) come next and slot in under the same
//! L1. LPAE would buy >4 GB physical addressing, which a 1 GB board does not need.
//!
//! **Everything is identity-mapped here (VA == PA).** That keeps the enable sequence safe: the PC,
//! the stack and the vector table stay valid across the instant the MMU turns on. A higher-half
//! kernel split comes with the task work, not now.

use core::sync::atomic::{AtomicU32, Ordering};

use super::pl011_write;
use super::exceptions::write_hex32;

/// Level-1 table: 4096 entries x 4 bytes = 16 KiB, and the hardware requires 16 KiB alignment
/// (TTBR0 ignores the low 14 bits, so a misaligned table would be silently truncated - the same class
/// of trap as VBAR's 32-byte alignment). `repr(align)` puts it in BSS, which `_start` has already
/// zeroed, so every entry starts as "invalid" and we fill in only what we mean to map.
#[repr(align(16384))]
struct L1Table([u32; 4096]);

static mut L1: L1Table = L1Table([0; 4096]);

/// Usable RAM end, learned from the firmware's device tree at boot (`dtb.rs`).
///
/// This used to be a hardcoded constant copied from what the firmware told Linux, with a comment
/// admitting that was not how a real port should learn it. It now IS learned - the DTB is the
/// firmware's own description of the machine - and the constant below survives only as a fallback for
/// a board that hands us no usable blob. That case is announced loudly rather than assumed, because a
/// wrong RAM size becomes frame-allocator corruption later, far from its cause.
///
/// The GPU's carve-out sits above this (`vc_mem.mem_base=0x3ec00000`) and is deliberately left
/// unmapped - we do not own it.
pub const FALLBACK_RAM_END: u32 = 0x3B40_0000;

static RAM_END: AtomicU32 = AtomicU32::new(FALLBACK_RAM_END);

/// Record the usable RAM end before `enable()` builds the tables from it.
pub fn set_ram_end(end: u32) {
    RAM_END.store(end, Ordering::Relaxed);
}

/// BCM2836 peripherals (16 MiB) and the core-local block (timers, mailboxes, IPIs - what SMP and the
/// timer will need next). Both are Device memory, never Normal: speculative or reordered accesses to
/// MMIO are how drivers get mysterious, and Device + XN forbids executing from them at all.
const PERIPH_BASE: u32 = 0x3F00_0000;
const PERIPH_END: u32 = 0x4000_0000;
const LOCAL_BASE: u32 = 0x4000_0000;
const LOCAL_END: u32 = 0x4100_0000;

const SECTION_SIZE: u32 = 0x0010_0000; // 1 MiB

/// Build one 1 MB section descriptor.
///
/// Short-descriptor section layout: `[31:20]` base, `[18]` 0 = section (1 = supersection), `[16]` S,
/// `[15]` APX, `[14:12]` TEX, `[11:10]` AP, `[8:5]` domain, `[4]` XN, `[3]` C, `[2]` B, `[1:0]` 0b10.
///
/// Permissions: `AP=0b01` with `APX=0` is PL1 read/write, **PL0 no access** - kernel-only, which is
/// what every mapping here should be. User mappings arrive with per-task page tables.
/// Domain 0 throughout, with DACR set to *client* so permissions are actually checked (a *manager*
/// domain would bypass permission checks entirely - an easy way to accidentally have no protection).
fn section(pa: u32, device: bool, execute: bool) -> u32 {
    let mut d = (pa & 0xFFF0_0000) | 0b10; // section descriptor
    d |= 0b01 << 10; // AP = PL1 RW, PL0 none
    d |= 1 << 16; // S = shareable
    if device {
        // Device: TEX=0b000, C=0, B=1. Not Normal memory - no speculation, no reordering games.
        d |= 1 << 2;
    } else {
        // Normal, write-back write-allocate: TEX=0b001, C=1, B=1.
        d |= 0b001 << 12;
        d |= 1 << 3;
        d |= 1 << 2;
    }
    if !execute {
        d |= 1 << 4; // XN
    }
    d
}

/// A 1 MB section for the framebuffer: Normal but NON-cacheable (TEX=0b100, C=0, B=0), PL1 RW, non-exec.
/// Non-cacheable so the GPU (which scans RAM) sees ARM writes without cache maintenance, while still
/// allowing fast buffered writes and a byte memmove for scrolling - Device memory forbids the unaligned
/// accesses a memmove makes and is much slower to read back for a scroll.
fn section_fb(pa: u32) -> u32 {
    // NON-shareable: the framebuffer is not shared *kernel* data - only this core writes glyphs and the
    // GPU scans it through its own bus, so it never needs the SMP coherency fabric. Marking it Shareable
    // (as an earlier cut did) dragged QEMU TCG into its slow cross-core coherency path for every access,
    // crawling the whole system under -smp; dropping S keeps it out of that path (and is more accurate).
    (pa & 0xFFF0_0000)
        | 0b10          // section descriptor
        | (0b01 << 10)  // AP = PL1 RW, PL0 none
        | (0b100 << 12) // TEX = Normal, outer + inner non-cacheable
        | (1 << 4)      // XN
}

/// Fill the L1 table: identity-map RAM as Normal, peripherals and the core-local block as Device.
///
/// Everything not written stays 0 (an invalid descriptor), so any access outside these ranges takes a
/// translation fault and gets reported. That is the property the selftest below actually checks.
fn build_tables() {
    // SAFETY: Single-threaded boot context - the secondary cores are parked in `_start` and the MMU
    // is still off, so nothing is walking this table while we fill it. `L1` is a `static mut`
    // accessed only here and in `enable()`, both before any other core or task exists.
    let l1 = unsafe { &mut *core::ptr::addr_of_mut!(L1) };

    // RAM: Normal memory, executable (the kernel's own text lives down here).
    let ram_end = RAM_END.load(Ordering::Relaxed);
    let mut pa = 0u32;
    while pa < ram_end {
        l1.0[(pa / SECTION_SIZE) as usize] = section(pa, false, true);
        pa += SECTION_SIZE;
    }

    // Peripherals + core-local: Device, never executable.
    let mut pa = PERIPH_BASE;
    while pa < PERIPH_END {
        l1.0[(pa / SECTION_SIZE) as usize] = section(pa, true, false);
        pa += SECTION_SIZE;
    }
    let mut pa = LOCAL_BASE;
    while pa < LOCAL_END {
        l1.0[(pa / SECTION_SIZE) as usize] = section(pa, true, false);
        pa += SECTION_SIZE;
    }
}

/// Ask the hardware to translate a VA, using the ATS1CPR address-translation operation.
///
/// This is the honest way to check the tables: rather than re-reading our own descriptors (which
/// would only prove we can read what we just wrote), it runs the CPU's own table walker and returns
/// the physical address it produced. `PAR` bit 0 is the fault flag; on success the PA is in `[31:12]`.
///
/// Returns `None` if translation faulted - which is the *expected* answer for an unmapped address.
fn translate(va: u32) -> Option<u32> {
    let par: u32;
    // SAFETY: ATS1CPR (`c7, c8, 0`) is a PL1 address-translation op with no side effects on memory -
    // it walks the tables and writes the result to PAR (`c7, c4, 0`). The ISB orders the walk before
    // the PAR read. A faulting VA sets PAR.F rather than raising an exception, which is precisely why
    // this is safe to call on an address we expect to be unmapped.
    unsafe {
        core::arch::asm!(
            "mcr p15, 0, {va}, c7, c8, 0",
            "isb",
            "mrc p15, 0, {par}, c7, c4, 0",
            va = in(reg) va,
            par = out(reg) par,
            options(nostack),
        );
    }
    if par & 1 != 0 {
        None // PAR.F set: translation faulted
    } else {
        Some((par & 0xFFFF_F000) | (va & 0x0000_0FFF))
    }
}

/// Turn the MMU on, then prove it is actually translating.
///
/// The enable sequence has a strict order and every step matters:
/// 1. **Invalidate** TLBs, branch predictor and I-cache - stale entries from before the tables
///    existed would be honoured over them.
/// 2. **DACR = client for domain 0**, so permission bits are enforced (manager would skip the checks).
/// 3. **TTBCR = 0**, so TTBR0 covers the whole 4 GB (no TTBR1 split yet).
/// 4. **TTBR0 = table address.** Table walks are left non-cacheable here - one fewer attribute to get
///    wrong on the first bring-up; making walks cacheable is a later optimisation, not correctness.
/// 5. **DSB + ISB**, then set `SCTLR.M`, then DSB + ISB again. The barriers are not decoration: the
///    instruction after the enable must be fetched under the new regime.
///
/// Identity mapping is what makes this survivable - PC, SP and VBAR all mean the same thing on both
/// sides of the switch.
/// Map the GPU framebuffer region `[base, base+size)` as Device memory in the LIVE kernel L1, so ARM
/// writes to it reach the display. The framebuffer sits in the gap between usable RAM and the
/// peripherals, which `build_tables` leaves unmapped, so it must be added after the fact. Rounds to the
/// enclosing 1 MiB sections. Runs after the MMU + caches are on, so the new descriptors are cleaned to
/// RAM (the walker reads the table non-cacheable) and the TLB is flushed.
pub fn map_framebuffer(base: u32, size: u32) {
    // SAFETY: `L1` is the live kernel table; we add previously-empty (unmapped-gap) entries, so no
    // running mapping is disturbed. Single writer on this boot path.
    let l1 = unsafe { &mut *core::ptr::addr_of_mut!(L1) };
    let start = base & !(SECTION_SIZE - 1);
    let end   = base.saturating_add(size).saturating_add(SECTION_SIZE - 1) & !(SECTION_SIZE - 1);
    let mut pa = start;
    while pa < end {
        l1.0[(pa / SECTION_SIZE) as usize] = section_fb(pa); // Normal non-cacheable, GPU-coherent
        pa = pa.wrapping_add(SECTION_SIZE);
        if pa == 0 { break; } // wrapped past 4 GiB
    }
    // Publish the new descriptors to the Point of Coherency (RAM). The walker reads the table
    // non-cacheable, PAST the A7's L2 cache, so a set/way L1 clean is NOT enough - the fb write faulted
    // NOT-MAPPED because the new entries sat in L2. `clean_dcache` (DCCMVAC by MVA) reaches the PoC, the
    // same publish `fill_kernel_identity` uses for its L1/L2 writes.
    super::page_tables::clean_dcache(core::ptr::addr_of!(L1) as u32, 16384);
    // SAFETY: flush the TLB so the next framebuffer access uses the new descriptors. `dsb`/`isb`/TLBIALL
    // are PL1 barriers with no operand hazards.
    unsafe {
        core::arch::asm!(
            "dsb",
            "mcr p15, 0, {z}, c8, c7, 0", // TLBIALL
            "dsb", "isb",
            z = in(reg) 0u32, options(nostack),
        );
    }
}

pub fn enable() {
    build_tables();
    // SAFETY: core 0, tables just built; enables translation + caches on this core.
    unsafe { enable_on_this_core(); }

    pl011_write(b"arm32: MMU ON (short descriptors, 1 MiB sections, L1 @ ");
    write_hex32(core::ptr::addr_of!(L1) as u32);
    pl011_write(b")\r\n");
    selftest();
    pl011_write(b"arm32: caches ON (I + D + branch prediction)\r\n");
}

/// Enable translation + caches on the CALLING core, using the `L1` table `build_tables` already
/// populated. Core 0 calls this from `enable` after building the tables; each secondary core (SMP)
/// calls it from its bring-up path to load the SAME kernel address space. Idempotent per core: it
/// invalidates this core's TLB/caches, points its TTBR0 at the shared L1, and sets SCTLR.M + caches.
///
/// # Safety
/// The `L1` table must already be built (core 0 ran `build_tables`), and the caller must be at PL1
/// with interrupts masked. Every mapping in `L1` is identity, so PC/SP/VBAR stay valid across the
/// SCTLR.M write on any core.
pub unsafe fn enable_on_this_core() {
    let table = core::ptr::addr_of!(L1) as u32;

    // SAFETY: Every `mcr` below is an
    // architecturally valid PL1 system-register write, issued in the order the ARM ARM requires for
    // enabling translation. The table is 16 KiB aligned (`repr(align(16384))`) as TTBR0 demands, and
    // every mapping is identity, so the PC/SP/VBAR remain valid across the SCTLR.M write. The
    // DSB/ISB pairs order the table writes before the walk and the enable before the next fetch.
    unsafe {
        core::arch::asm!(
            "dsb",
            "mov  {tmp}, #0",
            "mcr  p15, 0, {tmp}, c8, c7, 0",   // TLBIALL - invalidate entire TLB
            "mcr  p15, 0, {tmp}, c7, c5, 6",   // BPIALL  - invalidate branch predictor
            "mcr  p15, 0, {tmp}, c7, c5, 0",   // ICIALLU - invalidate I-cache
            "dsb",
            "isb",
            "mov  {tmp}, #1",
            "mcr  p15, 0, {tmp}, c3, c0, 0",   // DACR: domain 0 = client (permissions enforced)
            "mov  {tmp}, #0",
            "mcr  p15, 0, {tmp}, c2, c0, 2",   // TTBCR = 0 (TTBR0 covers all 4 GB)
            "mcr  p15, 0, {tbl}, c2, c0, 0",   // TTBR0 = L1 table
            "dsb",
            "isb",
            "mrc  p15, 0, {tmp}, c1, c0, 0",   // SCTLR
            "orr  {tmp}, {tmp}, #1",           //   .M = 1  (MMU on)
            "mcr  p15, 0, {tmp}, c1, c0, 0",
            "dsb",
            "isb",
            tbl = in(reg) table,
            tmp = out(reg) _,
        );
    }

    // Caches are enabled only after translation is proven, so that if the machine wedges we know
    // which of the two steps did it. Memory attributes above (Normal WB/WA for RAM, Device for MMIO)
    // are what make this safe to do at all - with the MMU off, everything behaves as Strongly-ordered.
    // SAFETY: Same boot context. Sets SCTLR.C (data cache), .I (instruction cache) and .Z (branch
    // prediction); valid at PL1 and meaningful only now that translation supplies memory attributes.
    unsafe {
        core::arch::asm!(
            "mrc p15, 0, {tmp}, c1, c0, 0",
            "orr {tmp}, {tmp}, #(1 << 2)",     // C - data cache
            "orr {tmp}, {tmp}, #(1 << 12)",    // I - instruction cache
            "orr {tmp}, {tmp}, #(1 << 11)",    // Z - branch prediction
            "mcr p15, 0, {tmp}, c1, c0, 0",
            "dsb",
            "isb",
            tmp = out(reg) _,
        );
    }
}

/// Prove translation is live, using the CPU's own table walker.
///
/// Three checks, and the third is the one that matters. Confirming that mapped addresses translate
/// only shows the table is not empty; confirming that an address *outside* every mapped range does
/// **not** translate is what shows the table is a real boundary rather than a blanket identity map.
/// That is the same reasoning as the x86 IOMMU selftest (§22 Test 12), which pins "the page past the
/// arena is unmapped" rather than merely "the arena works".
fn selftest() {
    let mut pass = true;

    // 1. Low RAM (where the kernel itself runs) must translate identity.
    match translate(0x0000_8000) {
        Some(pa) if pa == 0x0000_8000 => {}
        _ => {
            pl011_write(b"  selftest FAIL: kernel text at 0x00008000 does not translate identity\r\n");
            pass = false;
        }
    }

    // 2. The PL011 must translate identity - we are about to keep talking through it.
    match translate(PERIPH_BASE + 0x20_1000) {
        Some(pa) if pa == PERIPH_BASE + 0x20_1000 => {}
        _ => {
            pl011_write(b"  selftest FAIL: PL011 MMIO does not translate identity\r\n");
            pass = false;
        }
    }

    // 3. An address beyond every mapped range must NOT translate. Without this the first two checks
    //    would pass just as happily on a table that mapped all 4 GB.
    if let Some(pa) = translate(0xF000_0000) {
        pl011_write(b"  selftest FAIL: unmapped 0xF0000000 translated to ");
        write_hex32(pa);
        pl011_write(b" - the table is not bounding anything\r\n");
        pass = false;
    }

    if pass {
        pl011_write(b"arm32: MMU selftest PASS (RAM + MMIO identity, unmapped faults)\r\n");
    } else {
        pl011_write(b"arm32: MMU selftest FAILED - see above\r\n");
    }
}
