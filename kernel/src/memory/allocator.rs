// SPDX-License-Identifier: GPL-2.0-only
//! Physical frame allocator - §10.
//!
//! Bitmap allocator: one bit per 4 KiB frame.  0 = used, 1 = free.
//! The bitmap is sized to the machine's ACTUAL RAM at boot and carved from RAM (reached via the HHDM) -
//! no fixed compile-time cap and no wasted .bss; a bounded arena, not a heap (§26.6.1). See `init_from_map`.
//! All frames start marked used; `init_from_map` opens the usable regions.
//!
//! SMP-safe: ALLOC_LOCKED spinlock serialises alloc_frame / free_frame across
//! all cores. Lock is never held across a blocking operation.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::imp::{BootInfo, MemoryKind};
use crate::memory::frame::{Frame, PhysAddr, FRAME_SIZE};

// ---------------------------------------------------------------------------
// Kernel-range guard - fires if alloc_frame ever returns a kernel-image frame.
// ---------------------------------------------------------------------------

static mut GUARD_START: u64 = 0;
static mut GUARD_END:   u64 = 0;

#[inline(never)]
fn guard_bugcheck(phys: u64) {
    // Write directly to COM1 - no lock, no allocator, no stack growth.
    #[inline(always)]
    fn putb(b: u8) { crate::arch::imp::serial_write_byte(b); }
    fn puts(s: &[u8]) { for &b in s { putb(b); } }
    fn puthex(v: u64) {
        puts(b"0x");
        for i in (0..16).rev() { let n = ((v >> (i*4)) & 0xf) as u8; putb(if n < 10 { b'0'+n } else { b'a'+n-10 }); }
    }
    puts(b"\nBUG: alloc_frame returned kernel-range frame phys=");
    puthex(phys);
    puts(b" guard=[");
    // SAFETY: guard statics written once during init, read-only here.
    puthex(unsafe { GUARD_START });
    puts(b",");
    puthex(unsafe { GUARD_END });
    puts(b")\n");
    panic!("alloc_frame: kernel-range frame returned");
}

// ---------------------------------------------------------------------------
// Frame bitmaps - carved from RAM at boot, sized to the machine's ACTUAL RAM.
//
// No fixed compile-time cap and no waste (§26.6.1: a bounded ARENA sized once to the natural bound - all
// of physical memory - not a heap; it is reserved once at init and never resized). `init_from_map` reads
// the memory map, sizes the two bitmaps to the highest RAM frame present, carves one contiguous region
// for them out of the top of the largest usable region (well above the kernel's low-memory page-table
// frames), and reaches them through the HHDM. `BITMAP` = the free map (0 = used, 1 = free); `KPT` = the
// kernel-PT-protected map `free_frame` must never release. Both are set once and read via the slices
// below. Unlike a fixed static bitmap (which reserved GiB of .bss to cover RAM a machine may not have),
// this costs exactly `RAM / 16 KiB` (both bitmaps) and scales to any RAM.
// ---------------------------------------------------------------------------

const FRAME_SIZE_USIZE: usize = FRAME_SIZE as usize;

static mut BITMAP_PTR: *mut u8 = core::ptr::null_mut(); // free bitmap, at an HHDM virtual address
static mut KPT_PTR:    *mut u8 = core::ptr::null_mut(); // kernel-PT-protected bitmap
static mut BITMAP_LEN: usize = 0; // bytes per bitmap = ceil(max_ram_frame / 8); the frame cap is ALLOCATOR.max_ram_frame

/// The free bitmap as a slice (HHDM-backed).
/// # Safety
/// `BITMAP_PTR`/`BITMAP_LEN` are set once by `init_from_map` before any alloc/free; the caller holds
/// `ALLOC_LOCKED` (single writer across cores), so the returned `&mut` is not aliased.
#[inline]
unsafe fn bitmap() -> &'static mut [u8] {
    // SAFETY: ptr+len are a valid, uniquely-owned (under ALLOC_LOCKED) RAM region set at init.
    unsafe { core::slice::from_raw_parts_mut(BITMAP_PTR, BITMAP_LEN) }
}
/// The kernel-PT-protected bitmap as a slice.
/// # Safety
/// As [`bitmap`].
#[inline]
unsafe fn kpt() -> &'static mut [u8] {
    // SAFETY: as bitmap(); KPT_PTR is a disjoint region (BITMAP_PTR + BITMAP_LEN).
    unsafe { core::slice::from_raw_parts_mut(KPT_PTR, BITMAP_LEN) }
}

// ---------------------------------------------------------------------------
// Allocator.
// ---------------------------------------------------------------------------

struct BitmapAllocator {
    free_frames: usize,
    /// Total usable frames at init time. Fixed after init; never decremented.
    total_frames: usize,
    /// Byte-index scan hint - avoids rescanning from 0 on every alloc.
    next_byte: usize,
    /// Highest frame index (exclusive) that was ever marked usable.
    /// Any frame index at or above this value was never handed out by the
    /// allocator and must not be accepted by `free`.
    max_valid_frame: usize,
    /// Highest frame index (exclusive) across ALL RAM-backed regions the HHDM covers (usable +
    /// bootloader-reclaimable + kernel + acpi-reclaimable). `phys_in_ram` uses THIS, not
    /// `max_valid_frame`: Limine puts the kernel's initial page tables in bootloader-reclaimable RAM
    /// ABOVE usable RAM, so walking a legit page table there must not false-positive as corrupt. A
    /// truly corrupt entry (phys far beyond total RAM, e.g. ~68 GB) is still caught.
    max_ram_frame: usize,
    /// Count of double-free attempts (a frame freed while already free). The bitmap absorbs these
    /// idempotently, but they must not inflate `free_frames` (else it exceeds `total_frames` and
    /// observe's RAM read underflows). Counted so the loud log can be rate-limited.
    double_frees: usize,
    /// DMA-arena permanent reservations (§12, the DMA-safety net). Frames backing a driver's DMA arena
    /// are recorded here by `alloc_dma_arena` and NEVER returned to the general pool by `free` - so a
    /// stray device DMA (if the kill-path bus-master quiesce ever fails) lands in a reserved DMA frame,
    /// corrupting only DMA data (caught by AHCI/USB CRC), never a page table or kernel struct. The
    /// per-driver arena is reused across respawns, so this is bounded (one arena per driver). Each entry
    /// is (base_frame_index, n_frames); (0, 0) = empty. Mirrors the KERNEL_PT_PROTECTED guard below.
    dma_reserves: [(usize, usize); MAX_DMA_RESERVES],
}

/// Max distinct DMA-arena reservations: xhci, ehci, block-driver, nic-driver, + headroom for one more.
const MAX_DMA_RESERVES: usize = 6;

impl BitmapAllocator {
    const fn new() -> Self {
        Self {
            free_frames: 0, total_frames: 0, next_byte: 0, max_valid_frame: 0, max_ram_frame: 0, double_frees: 0,
            dma_reserves: [(0, 0); MAX_DMA_RESERVES],
        }
    }

    // SAFETY: caller must guarantee single-threaded access; called once by BSP during memory::init,
    // AFTER set_hhdm_offset (the bitmaps live at HHDM virtual addresses).
    unsafe fn init_from_map(&mut self, boot_info: &BootInfo) {
        let kstart = boot_info.kernel_phys_start;
        let kend   = boot_info.kernel_phys_end;
        let hhdm   = boot_info.hhdm_offset;
        if hhdm == 0 {
            panic!("allocator: HHDM offset not set before init - bitmaps live in the HHDM");
        }

        // Pass 1: measure RAM and find where to put the bitmaps.
        //  - max_ram_frame: highest frame across ALL RAM-backed regions (usable + reclaimable + kernel).
        //    The bitmaps must cover THIS, because protect_kernel_page_table_frames marks kernel PT frames
        //    Limine placed in bootloader-reclaimable RAM ABOVE usable RAM. (Same extent phys_in_ram uses.)
        //  - max_valid_frame: highest USABLE frame (the frames actually handed out / freed).
        //  - largest usable region: where the bitmaps get carved from (its top).
        let mut largest_start = 0u64;
        let mut largest_len   = 0u64;
        for region in boot_info.memory_map {
            if matches!(region.kind, MemoryKind::Usable | MemoryKind::AcpiReclaimable
                                   | MemoryKind::KernelImage | MemoryKind::BootloaderReclaimable) {
                let ram_last = ((region.base + region.len + FRAME_SIZE - 1) / FRAME_SIZE) as usize;
                if ram_last > self.max_ram_frame { self.max_ram_frame = ram_last; }
            }
            if !matches!(region.kind, MemoryKind::Usable) { continue; }
            let start = frame_align_up(region.base);
            let end   = frame_align_down(region.base + region.len);
            if end <= start { continue; }
            let last = (end / FRAME_SIZE) as usize;
            if last > self.max_valid_frame { self.max_valid_frame = last; }
            if end - start > largest_len { largest_len = end - start; largest_start = start; }
        }

        // Size the two bitmaps to cover every frame the allocator can touch (0..max_ram_frame).
        let frame_cap   = self.max_ram_frame;
        let bitmap_len  = (frame_cap + 7) / 8;                              // bytes per bitmap
        let need        = 2 * bitmap_len;                                   // both bitmaps, contiguous
        let need_frames = (need + FRAME_SIZE_USIZE - 1) / FRAME_SIZE_USIZE;

        // Carve the bitmap region from the TOP of the largest usable region. High memory is well clear of
        // the kernel's page-table frames, which Limine places in LOW usable RAM at/below [kstart, kend)
        // (the exact frames protect_kernel_page_table_frames guards), so the carve cannot clobber a live
        // PT. Loud panic (invariant 12) if - impossibly - no region can hold the bitmap.
        if (largest_len / FRAME_SIZE) < need_frames as u64 {
            panic!("allocator: largest usable region ({} KiB) too small for the {} KiB frame bitmap",
                   largest_len / 1024, need / 1024);
        }
        let carve_last_f  = (largest_start + largest_len) / FRAME_SIZE;     // exclusive, top of the region
        let carve_first_f = carve_last_f - need_frames as u64;
        let carve_phys    = carve_first_f * FRAME_SIZE;
        let carve_first   = carve_first_f as usize;
        let carve_last    = carve_last_f as usize;

        // Publish the bitmap pointers (HHDM-backed) + geometry, then zero both (all frames start "used").
        BITMAP_PTR = (hhdm + carve_phys) as *mut u8;
        // SAFETY: BITMAP_PTR + bitmap_len is inside the just-reserved 2*bitmap_len region.
        KPT_PTR    = unsafe { BITMAP_PTR.add(bitmap_len) };
        BITMAP_LEN = bitmap_len;
        // SAFETY: the carve region is RAM the HHDM maps read/write; reserved, not yet handed out.
        unsafe { core::ptr::write_bytes(BITMAP_PTR, 0, need) };
        crate::kprintln!(
            "allocator: frame bitmap {} KiB x2 covers {} frames ({} MiB), carved at phys {:#x} (top of largest region)",
            bitmap_len / 1024, frame_cap, (frame_cap * FRAME_SIZE_USIZE) / (1024 * 1024), carve_phys
        );

        // Pass 2: open the usable frames, skipping the kernel image AND the bitmaps' own frames.
        for region in boot_info.memory_map {
            if !matches!(region.kind, MemoryKind::Usable) { continue; }
            let start = frame_align_up(region.base);
            let end   = frame_align_down(region.base + region.len);
            if start >= end { continue; }
            let first = (start / FRAME_SIZE) as usize;
            let last  = (end   / FRAME_SIZE) as usize; // exclusive
            for idx in first..last {
                if idx >= frame_cap { break; }
                let frame_phys = idx as u64 * FRAME_SIZE;
                // Skip frames that back the kernel image (text, data, BSS); kernel stacks live in BSS,
                // handing those out would zero live kernel stacks.
                if kend > kstart && frame_phys >= kstart && frame_phys < kend { continue; }
                // Skip the bitmaps' own storage (carved above) so alloc never hands it out.
                if idx >= carve_first && idx < carve_last { continue; }
                // SAFETY: idx < frame_cap == FRAME_CAP → within BITMAP_LEN; single-threaded init.
                unsafe { bitmap_set_free(idx) };
                self.free_frames += 1;
            }
        }
        self.total_frames = self.free_frames;
    }

    // SAFETY: caller must hold ALLOC_LOCKED; BITMAP and ALLOCATOR are exclusively accessible under the lock.
    unsafe fn alloc(&mut self) -> Option<Frame> {
        // SAFETY: lock held; bitmap() is the HHDM-backed free map, uniquely owned under ALLOC_LOCKED.
        let bitmap = unsafe { bitmap() };
        let len = bitmap.len();

        // Scan from hint, wrap if not found.
        let idx = scan_free(bitmap, self.next_byte, len)
            .or_else(|| scan_free(bitmap, 0, self.next_byte))?;

        // Mark used.
        bitmap[idx / 8] &= !(1u8 << (idx % 8));
        self.free_frames -= 1;
        self.next_byte = (idx / 8 + 1).min(len - 1);

        let phys_addr = idx as u64 * FRAME_SIZE;
        // Guard: panic if we're about to hand out a kernel-image frame.
        // SAFETY: statics written once during init; any read racing with init
        // is fine because alloc cannot be called before init completes.
        let gs = unsafe { GUARD_START };
        let ge = unsafe { GUARD_END };
        if ge > gs && phys_addr >= gs && phys_addr < ge {
            guard_bugcheck(phys_addr);
        }
        let phys = PhysAddr(phys_addr);
        // SAFETY: idx from free bitmap → page-aligned; now exclusively owned.
        Some(unsafe { Frame::from_phys(phys) })
    }

    /// Allocate `n` physically-contiguous frames; return the phys address of the
    /// first. Scans the bitmap for a run of `n` consecutive free bits. Used for
    /// driver DMA arenas (§12). Kernel-image frames are never free in the
    /// bitmap, so a free run can never straddle them.
    ///
    /// SAFETY: caller must hold ALLOC_LOCKED.
    unsafe fn alloc_contiguous(&mut self, n: usize) -> Option<u64> {
        if n == 0 {
            return None;
        }
        // SAFETY: lock held; bitmap() is uniquely owned under ALLOC_LOCKED.
        let bitmap = unsafe { bitmap() };
        let max = (self.max_valid_frame + 1).min(self.max_ram_frame);
        let mut run = 0usize;
        let mut start = 0usize;
        let mut found = None;
        let mut idx = 0usize;
        while idx < max {
            let free = (bitmap[idx / 8] >> (idx % 8)) & 1 != 0;
            if free {
                if run == 0 {
                    start = idx;
                }
                run += 1;
                if run == n {
                    found = Some(start);
                    break;
                }
            } else {
                run = 0;
            }
            idx += 1;
        }
        let start = found?;
        for i in start..start + n {
            bitmap[i / 8] &= !(1u8 << (i % 8));
        }
        self.free_frames -= n;
        Some(start as u64 * FRAME_SIZE)
    }

    /// Like `alloc_contiguous`, but RECORDS the run as a permanent DMA reservation (§12, the
    /// DMA-safety net): `free` then skips every frame in it, so the arena is never returned to the
    /// general pool to be recycled as a page table. For driver DMA arenas; the per-driver arena is
    /// allocated once and reused across respawns (the spawn path keeps the phys), so the reservation
    /// is bounded - one arena per driver. None if no run is free, or the reservation table is full.
    ///
    /// SAFETY: caller must hold ALLOC_LOCKED.
    unsafe fn alloc_dma_arena(&mut self, n: usize) -> Option<u64> {
        // SAFETY: lock held (caller contract).
        let phys = unsafe { self.alloc_contiguous(n)? };
        let base = (phys / FRAME_SIZE) as usize;
        for slot in self.dma_reserves.iter_mut() {
            if slot.1 == 0 {
                *slot = (base, n);
                return Some(phys);
            }
        }
        // Reservation table full (should never happen: MAX_DMA_RESERVES >= the DMA-driver count). We
        // already took the run; without a slot, `free` would later hand it back to the general pool -
        // the exact hazard this guards. Refuse loudly and return the run rather than leave it exposed.
        crate::kprintln!(
            "alloc_dma_arena: reservation table full ({}); refusing arena", MAX_DMA_RESERVES);
        for i in base..base + n {
            // SAFETY: idx within bounds (just allocated); lock held.
            let _ = unsafe { bitmap_set_free(i) };
        }
        self.free_frames += n;
        None
    }

    // SAFETY: caller must hold ALLOC_LOCKED and have exclusive ownership of `frame`.
    unsafe fn free(&mut self, frame: Frame) {
        let idx = frame.frame_number() as usize;
        // Reject phantom frames: addresses that were never in the usable RAM
        // range.  These arise when a corrupt or stale page-table entry (from a
        // re-animated dead task) is walked and freed.  Setting a bit for an
        // out-of-range frame would allow alloc to return a phantom address,
        // which would then fault the kernel on its next HHDM access.
        // Reject phantom frames above usable RAM, AND any frame at/above the bitmap's capacity
        // (max_ram_frame - the highest RAM frame the bitmaps were sized to cover). max_valid_frame <=
        // max_ram_frame by construction, so the second bound is defensive: it guarantees `byte = idx/8`
        // is within the bitmaps regardless, so a corrupt/stale PTE (from a re-animated dead task) can
        // never OOB-index BITMAP / KPT (the release build compiles out the debug_assert below).
        if idx >= self.max_valid_frame || idx >= self.max_ram_frame {
            crate::kprintln!(
                "free_frame: IGNORED phantom frame idx={} (max_valid={}, cap={})",
                idx, self.max_valid_frame, self.max_ram_frame
            );
            return;
        }
        debug_assert!(idx < self.max_ram_frame, "free_frame: address out of range");
        // Defense-in-depth: refuse to free a frame that was marked as a kernel
        // intermediate page-table frame by protect_kernel_page_table_frames().
        // If such a frame ever appears in a reclaim buffer, freeing it would
        // re-open it for alloc → walk_or_alloc zeros it → KERNEL PF on the
        // next access to the kernel virtual region it was mapping.
        // SAFETY: KPT is written under the lock; read-only here (lock held by free_frame).
        let byte = idx / 8;
        let bit  = idx % 8;
        if unsafe { kpt() }[byte] & (1u8 << bit) != 0 {
            crate::kprintln!(
                "free_frame: REFUSED to free kernel PT frame idx={} phys={:#x}",
                idx, idx as u64 * FRAME_SIZE
            );
            return;
        }
        // DMA permanent-reserve (§12): frames backing a driver's DMA arena are never returned to the
        // general pool, so they can never be recycled into a page table. A stray device DMA (if the
        // kill-path bus-master quiesce ever fails) then lands in a reserved DMA frame - corrupting only
        // DMA data, never a PTE or kernel struct. Reused across the driver's respawns, so this is
        // bounded (one arena per driver), not a leak. Silent (a reclaim walking the arena hits it every
        // kill - it is a reservation, not corruption), unlike the loud rejects above.
        for &(base, n) in self.dma_reserves.iter() {
            if n != 0 && idx >= base && idx < base + n {
                return;
            }
        }
        // SAFETY: idx within bounds; caller guarantees exclusive ownership.
        if unsafe { bitmap_set_free(idx) } {
            // Double-free: the frame is already free. The bitmap absorbed it idempotently (no
            // duplicate in the pool, so no double-allocation), but counting it again would push
            // free_frames past total_frames and underflow observe's RAM read. Don't re-count; log
            // loudly (§26.7), rate-limited so a burst (e.g. chaos max-carnage) can't bury the console.
            self.double_frees += 1;
            if self.double_frees == 1 || self.double_frees % 512 == 0 {
                crate::kprintln!(
                    "free_frame: double-free idx={} (already free; bitmap idempotent, count #{})",
                    idx, self.double_frees
                );
            }
            return;
        }
        self.free_frames += 1;
        if idx / 8 < self.next_byte {
            self.next_byte = idx / 8;
        }
    }

    fn free_frames(&self) -> usize {
        self.free_frames
    }
}

/// Find the first set bit in `bitmap[start..end]`. Returns the frame index.
fn scan_free(bitmap: &[u8], start: usize, end: usize) -> Option<usize> {
    for byte_idx in start..end {
        let byte = bitmap[byte_idx];
        if byte != 0 {
            let bit = byte.trailing_zeros() as usize;
            return Some(byte_idx * 8 + bit);
        }
    }
    None
}

// SAFETY: must be called with exclusive access to BITMAP.
/// Mark frame `idx` free in the bitmap. Returns `true` if it was ALREADY free - a double-free; the
/// set is idempotent (the frame is not duplicated in the pool, so no double-allocation), but the
/// caller must not re-count it in `free_frames`.
unsafe fn bitmap_set_free(idx: usize) -> bool {
    let byte = idx / 8;
    let bit  = 1u8 << (idx % 8);
    // SAFETY: idx < max_ram_frame (caller-checked) => byte < BITMAP_LEN; mutated under ALLOC_LOCKED.
    unsafe {
        let bm = bitmap();
        let was_free = bm[byte] & bit != 0;
        bm[byte] |= bit;
        was_free
    }
}

fn frame_align_up(addr: u64) -> u64 {
    (addr + FRAME_SIZE - 1) & !(FRAME_SIZE - 1)
}

fn frame_align_down(addr: u64) -> u64 {
    addr & !(FRAME_SIZE - 1)
}

// ---------------------------------------------------------------------------
// SMP spinlock.
// ---------------------------------------------------------------------------

static ALLOC_LOCKED: AtomicBool = AtomicBool::new(false);

/// Watchdog bound for the ALLOC_LOCKED spin. The frame allocator's critical sections are microseconds
/// (a bitmap scan/flip); spinning past ~10^9 iterations (a few seconds at GHz) means the holder is never
/// releasing - a task preempted mid-alloc that can't be rescheduled, or a lock-ordering deadlock. This
/// is the single most-contended lock in the spawn/kill path, and it is HAND-ROLLED (not `SpinLock<T>`),
/// so the SpinLock watchdog does not cover it. Panic loudly instead of freezing the machine silently
/// (invariant 12 / §26.7). Huge margin over any real hold, so it cannot false-fire.
const ALLOC_WATCHDOG_SPINS: u64 = 1_000_000_000;

#[inline]
fn alloc_lock() {
    let mut spins: u64 = 0;
    while ALLOC_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        spins += 1;
        if spins >= ALLOC_WATCHDOG_SPINS {
            alloc_lock_wedge(spins);
        }
        core::hint::spin_loop();
    }
}

/// Out-of-line loud panic for the frame-allocator lock watchdog (keeps `alloc_lock` lean).
#[cold]
#[inline(never)]
fn alloc_lock_wedge(spins: u64) -> ! {
    panic!(
        "frame-allocator (ALLOC_LOCKED) WEDGE: spun {} iters - holder never released (preempted holder / deadlock)",
        spins
    );
}

#[inline]
fn alloc_unlock() {
    ALLOC_LOCKED.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Global instance + public API.
// ---------------------------------------------------------------------------

static mut ALLOCATOR: BitmapAllocator = BitmapAllocator::new();

pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by BSP during memory::init, before any allocation.
    unsafe {
        GUARD_START = boot_info.kernel_phys_start;
        GUARD_END   = boot_info.kernel_phys_end;
        (*core::ptr::addr_of_mut!(ALLOCATOR)).init_from_map(boot_info)
    };
}

/// Allocate one physical frame. Returns `None` if memory is exhausted.
pub fn alloc_frame() -> Option<Frame> {
    // IRQ-safe: ALLOC_LOCKED is taken from BOTH task context and interrupt context (the timer ISR's
    // kill path → reclaim_all → free_frame), so the hold MUST mask interrupts (smp::without_interrupts
    // contract). Otherwise a task preempted mid-alloc holds the lock, and any code that then takes it
    // with preemption suppressed deadlocks on the unreschedulable holder - the supervisor respawn
    // (Path C / Phase 6) pins Core 0 and its frame-alloc spun here (the §22 Test 15 wedge). Nests
    // correctly with callers already masked. Same class of bug as the RING/kprintln fix.
    crate::smp::without_interrupts(|| {
        alloc_lock();
        // SAFETY: lock held; single writer across all cores.
        let frame = unsafe { (*core::ptr::addr_of_mut!(ALLOCATOR)).alloc() };
        alloc_unlock();
        frame
    })
}

/// Zero a freshly-allocated physical frame through its HHDM alias. `alloc_frame` returns a frame
/// straight off the free bitmap without zeroing, and `free_frame` does not zero either, so any path
/// that hands a raw frame to userspace must zero it first or it leaks the previous owner's contents.
/// The spawn ELF/stack/page-table paths already zero; this is the shared helper for the AllocMem
/// syscall, which needs no capability and so must never expose stale cross-task memory (SEC-21).
/// Keeping the `unsafe` here in the permitted `memory/` layer lets the syscall caller stay
/// `unsafe`-free (§18.5).
#[inline]
pub fn zero_frame(phys: u64) {
    // SAFETY: `phys` is a frame from this allocator; the HHDM aliases all physical RAM at a fixed
    // kernel offset (set up in memory::init, before any syscall runs), so `hhdm + phys` is a valid
    // writable kernel VA covering exactly one FRAME_SIZE-byte frame.
    unsafe {
        core::ptr::write_bytes(
            (crate::arch::imp::page_tables::get_hhdm_offset() + phys) as *mut u8,
            0,
            FRAME_SIZE as usize,
        );
    }
}

/// Allocate `n` physically-contiguous, page-aligned frames; return the physical
/// address of the first, or `None` if no run that long is free. For driver DMA
/// arenas (§12) where the device DMAs into contiguous memory. The frames are not
/// returned as individual `Frame`s - the driver-spawn path maps them into the
/// driver's address space and they live for the driver's lifetime (v1: trusted
/// drivers are effectively permanent; reclaim-on-restart is future work).
pub fn alloc_contiguous(n: usize) -> Option<u64> {
    // IRQ-safe: see alloc_frame (ALLOC_LOCKED is also taken in interrupt context).
    crate::smp::without_interrupts(|| {
        alloc_lock();
        // SAFETY: lock held; single writer across all cores.
        let phys = unsafe { (*core::ptr::addr_of_mut!(ALLOCATOR)).alloc_contiguous(n) };
        alloc_unlock();
        phys
    })
}

/// Allocate a physically-contiguous DMA arena and RESERVE it permanently (§12, the DMA-safety net):
/// the frames are never returned to the general pool, so a stray device DMA can never land in a frame
/// later recycled as a page table - it lands in DMA-reserved memory (corrupting only DMA data, caught
/// by AHCI/USB CRC). The per-driver arena is allocated once and reused across the driver's respawns
/// (the spawn path keeps the phys), so this is bounded: one arena per driver.
pub fn alloc_dma_arena(n: usize) -> Option<u64> {
    // IRQ-safe: see alloc_contiguous (ALLOC_LOCKED is also taken in interrupt context).
    crate::smp::without_interrupts(|| {
        alloc_lock();
        // SAFETY: lock held; single writer across all cores.
        let phys = unsafe { (*core::ptr::addr_of_mut!(ALLOCATOR)).alloc_dma_arena(n) };
        alloc_unlock();
        phys
    })
}

/// Return a frame to the allocator.
///
/// # Safety
/// The frame must have been obtained from `alloc_frame` and must not be used
/// after this call.
pub unsafe fn free_frame(frame: Frame) {
    // IRQ-safe: see alloc_frame (ALLOC_LOCKED is also taken in interrupt context).
    crate::smp::without_interrupts(|| {
        alloc_lock();
        // SAFETY: lock held; caller guarantees exclusive ownership.
        unsafe { (*core::ptr::addr_of_mut!(ALLOCATOR)).free(frame) }
        alloc_unlock();
    })
}

/// Total free frames available (used for diagnostic output in memory::init).
pub fn free_frame_count() -> usize {
    // SAFETY: read-only; racing reads are harmless for diagnostic use.
    unsafe { (*core::ptr::addr_of!(ALLOCATOR)).free_frames() }
}

/// Return the total number of usable physical frames at boot time (fixed after init).
pub fn total_frame_count() -> usize {
    // SAFETY: read-only; set once at init, never mutated after.
    unsafe { ALLOCATOR.total_frames }
}

/// True if `phys` lies within physical RAM the HHDM covers - i.e. its frame index is below the highest
/// RAM-backed frame recorded at boot. The read-side companion to `free_frame`'s phantom-reject (line ~209):
/// a page-table walk must never DEREFERENCE an entry whose frame is outside RAM (a corrupted or stale
/// entry), or it page-faults the kernel via the HHDM (the chaos `max-carnage` ~68 GB KERNEL PF in
/// `reclaim_user_frames`). Uses `max_ram_frame` (ALL RAM: usable + bootloader-reclaimable + kernel) NOT
/// `max_valid_frame` (usable only) - Limine places the kernel's initial page tables in bootloader-
/// reclaimable RAM above usable RAM, and walking those legit tables must not false-positive (else the
/// guard floods). Set once at init, immutable after, so this is lock-free.
pub fn phys_in_ram(phys: u64) -> bool {
    // SAFETY: read-only; max_ram_frame is set once at init, never mutated after.
    let idx = (phys / FRAME_SIZE) as usize;
    unsafe { idx < ALLOCATOR.max_ram_frame }
}
/// Walk the kernel half of the live PML4 (entries 256-511) and mark every
/// PDPT / PD / PT / PML4 frame as "used" in the bitmap allocator.
///
/// Root cause this closes (BA2):
///   Limine allocates intermediate page-table frames for the kernel BSS mapping
///   from physical pages that appear as `Usable` in its memory map but lie below
///   the kernel image guard range [kstart, kend).  `init_from_map` opens those
///   frames in the bitmap; `alloc_frame` then returns them; `walk_or_alloc` /
///   `PageTable::new` zero them, destroying the kernel's PTE for the BSS page
///   being accessed - causing a KERNEL PF on the first write (BA2: write to
///   kstack_marker(90) at 0xffffffff80e09260 after many spawn/kill cycles).
///
/// Must be called after `allocator::init` (bitmap populated) and after
/// `set_hhdm_offset` (physical↔virtual translation live).
pub fn protect_kernel_page_table_frames() {
    let hhdm = crate::arch::imp::page_tables::get_hhdm_offset();
    if hhdm == 0 {
        return; // HHDM not initialised - cannot walk tables safely.
    }

    // SAFETY: CR3 is always valid after Limine hands control to the kernel.
    let pml4_phys = crate::arch::imp::read_page_table_base() & !0xFFF_u64;

    alloc_lock();
    // SAFETY: lock held; BITMAP and ALLOCATOR.free_frames may be mutated.
    unsafe {
        mark_pt_frame_used(pml4_phys);
        for pml4_i in 256..512usize {
            let pml4e = pt_read(hhdm, pml4_phys, pml4_i);
            if pml4e & 1 == 0 { continue; }
            let pdpt_phys = pml4e & 0x000F_FFFF_FFFF_F000;
            mark_pt_frame_used(pdpt_phys);
            for pdpt_i in 0..512usize {
                let pdpte = pt_read(hhdm, pdpt_phys, pdpt_i);
                if pdpte & 1 == 0 { continue; }
                if pdpte & (1 << 7) != 0 { continue; } // 1 GiB huge - no PD below
                let pd_phys = pdpte & 0x000F_FFFF_FFFF_F000;
                mark_pt_frame_used(pd_phys);
                for pd_i in 0..512usize {
                    let pde = pt_read(hhdm, pd_phys, pd_i);
                    if pde & 1 == 0 { continue; }
                    if pde & (1 << 7) != 0 { continue; } // 2 MiB huge - no PT below
                    let pt_phys = pde & 0x000F_FFFF_FFFF_F000;
                    mark_pt_frame_used(pt_phys);
                    // PT entries are leaf mappings - the data frames they point
                    // to are either already in the kernel guard range or are
                    // owned by tasks.  We do not mark them here.
                }
            }
        }
    }
    alloc_unlock();
}

/// Read one 64-bit entry from the page table at `table_phys`, slot `idx`.
///
/// # Safety
/// HHDM must be initialised; `table_phys` must be a live, HHDM-accessible
/// page-table frame.
#[inline]
unsafe fn pt_read(hhdm: u64, table_phys: u64, idx: usize) -> u64 {
    // SAFETY: caller guarantees table_phys + hhdm maps a valid PT page.
    unsafe { ((hhdm + table_phys) as *const u64).add(idx).read_volatile() }
}

/// Clear the free bit for the frame at `phys` if it is currently marked free,
/// and permanently mark it in `KERNEL_PT_PROTECTED` so `free_frame` can never
/// reclaim it.  `alloc_lock` must be held.
///
/// # Safety
/// `ALLOC_LOCKED` must be held; this mutates `BITMAP`, `ALLOCATOR`, and
/// `KERNEL_PT_PROTECTED`.
#[inline]
unsafe fn mark_pt_frame_used(phys: u64) {
    let idx = (phys / FRAME_SIZE) as usize;
    // SAFETY: max_ram_frame is set once at init, read-only here.
    if idx >= unsafe { ALLOCATOR.max_ram_frame } {
        return;
    }
    let byte = idx / 8;
    let bit  = idx % 8;
    // SAFETY: idx < max_ram_frame => byte < BITMAP_LEN; lock held.
    let currently_free = unsafe { bitmap() }[byte] & (1u8 << bit) != 0;
    if currently_free {
        unsafe { bitmap()[byte] &= !(1u8 << bit) };
        // SAFETY: free_frames was > 0 because the bit was set.
        unsafe { ALLOCATOR.free_frames -= 1 };
    }
    // Mark as permanently protected: free_frame will refuse to release this
    // frame regardless of how it ends up in a caller's reclaim buffer.
    // SAFETY: idx < max_ram_frame; lock held.
    unsafe { kpt()[byte] |= 1u8 << bit };
}
