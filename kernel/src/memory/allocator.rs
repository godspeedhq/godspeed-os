// SPDX-License-Identifier: GPL-2.0-only
//! Physical frame allocator - §10.
//!
//! Bitmap allocator: one bit per 4 KiB frame.  0 = used, 1 = free.
//! Covers up to 4 GiB of physical address space (128 KiB of bitmap in .bss).
//! All frames start marked used; `init_from_map` opens the usable regions.
//!
//! SMP-safe: ALLOC_LOCKED spinlock serialises alloc_frame / free_frame across
//! all cores. Lock is never held across a blocking operation.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::x86_64::{BootInfo, MemoryKind};
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
    fn putb(b: u8) { crate::arch::x86_64::serial_write_byte(b); }
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
// Bitmap - lives in .bss (zero-init = every frame starts as "used").
// ---------------------------------------------------------------------------

const FRAME_SIZE_USIZE: usize = FRAME_SIZE as usize;
const MAX_FRAMES: usize = (8 * 1024 * 1024 * 1024_usize) / FRAME_SIZE_USIZE;
const BITMAP_BYTES: usize = MAX_FRAMES / 8; // 256 KiB

// 0 = used, 1 = free; zero-init means all used at startup.
static mut BITMAP: [u8; BITMAP_BYTES] = [0u8; BITMAP_BYTES];

// 1 = permanently protected kernel PT frame; free_frame will refuse to free these.
// Set by protect_kernel_page_table_frames(); never cleared.
static mut KERNEL_PT_PROTECTED: [u8; BITMAP_BYTES] = [0u8; BITMAP_BYTES];

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

    // SAFETY: caller must guarantee single-threaded access; called once by BSP during memory::init.
    unsafe fn init_from_map(&mut self, boot_info: &BootInfo) {
        let kstart = boot_info.kernel_phys_start;
        let kend   = boot_info.kernel_phys_end;

        for region in boot_info.memory_map {
            // Track the RAM extent for `phys_in_ram` (the page-table walk guard): include ALL
            // RAM-backed regions the HHDM covers, not just usable, so a legit page table Limine
            // placed in bootloader-reclaimable / kernel RAM (above usable RAM) is not flagged as
            // out-of-RAM. A truly corrupt entry (phys far beyond total RAM) is still caught.
            if matches!(region.kind, MemoryKind::Usable | MemoryKind::AcpiReclaimable
                                   | MemoryKind::KernelImage | MemoryKind::BootloaderReclaimable) {
                let ram_last = ((region.base + region.len + FRAME_SIZE - 1) / FRAME_SIZE) as usize;
                if ram_last > self.max_ram_frame { self.max_ram_frame = ram_last; }
            }
            if !matches!(region.kind, MemoryKind::Usable) {
                continue;
            }
            // Align inward so we only hand out fully-contained frames.
            let start = frame_align_up(region.base);
            let end   = frame_align_down(region.base + region.len);
            if start >= end { continue; }

            let first = (start / FRAME_SIZE) as usize;
            let last  = (end   / FRAME_SIZE) as usize; // exclusive

            // Track the highest valid frame so free_frame can reject phantom
            // addresses (page-table entries from corrupted/reanimated tasks).
            if last > self.max_valid_frame {
                self.max_valid_frame = last;
            }

            for idx in first..last {
                if idx >= MAX_FRAMES { break; }
                // Skip frames that back the kernel image (text, data, BSS).
                // Kernel stacks (KSTACK_STORAGE) live in BSS; handing those
                // frames to a service loader would zero live kernel stacks.
                if kend > kstart {
                    let frame_phys = idx as u64 * FRAME_SIZE;
                    if frame_phys >= kstart && frame_phys < kend {
                        continue;
                    }
                }
                // SAFETY: idx within BITMAP bounds; single-threaded init.
                unsafe { bitmap_set_free(idx) };
                self.free_frames += 1;
            }
        }
        self.total_frames = self.free_frames;
    }

    // SAFETY: caller must hold ALLOC_LOCKED; BITMAP and ALLOCATOR are exclusively accessible under the lock.
    unsafe fn alloc(&mut self) -> Option<Frame> {
        // SAFETY: exclusive access guaranteed by single-core invariant (v1).
        let bitmap = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP) };

        // Scan from hint, wrap if not found.
        let idx = scan_free(bitmap, self.next_byte, BITMAP_BYTES)
            .or_else(|| scan_free(bitmap, 0, self.next_byte))?;

        // Mark used.
        bitmap[idx / 8] &= !(1u8 << (idx % 8));
        self.free_frames -= 1;
        self.next_byte = (idx / 8 + 1).min(BITMAP_BYTES - 1);

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
        // SAFETY: exclusive access guaranteed by the lock.
        let bitmap = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP) };
        let max = (self.max_valid_frame + 1).min(MAX_FRAMES);
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
        // (MAX_FRAMES = 8 GiB). `max_valid_frame` is taken from the memory map UNCLAMPED (init_from_map),
        // so on a machine with > 8 GiB RAM a corrupt/stale PTE whose index lands in
        // [MAX_FRAMES, max_valid_frame) would otherwise pass the first bound and OOB-index the
        // MAX_FRAMES-sized BITMAP / KERNEL_PT_PROTECTED (the release build compiles out the debug_assert
        // below). The alloc path never returns idx >= MAX_FRAMES (scan is bounded to BITMAP_BYTES;
        // alloc_contiguous clamps with .min(MAX_FRAMES)), so no legitimate free is rejected here
        // (kernel-audit-2 B-note).
        if idx >= self.max_valid_frame || idx >= MAX_FRAMES {
            crate::kprintln!(
                "free_frame: IGNORED phantom frame idx={} (max_valid={}, cap={})",
                idx, self.max_valid_frame, MAX_FRAMES
            );
            return;
        }
        debug_assert!(idx < MAX_FRAMES, "free_frame: address out of range");
        // Defense-in-depth: refuse to free a frame that was marked as a kernel
        // intermediate page-table frame by protect_kernel_page_table_frames().
        // If such a frame ever appears in a reclaim buffer, freeing it would
        // re-open it for alloc → walk_or_alloc zeros it → KERNEL PF on the
        // next access to the kernel virtual region it was mapping.
        // SAFETY: KERNEL_PT_PROTECTED is written once at init; read-only here.
        let byte = idx / 8;
        let bit  = idx % 8;
        if unsafe { KERNEL_PT_PROTECTED[byte] } & (1u8 << bit) != 0 {
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
    // SAFETY: idx < MAX_FRAMES (caller-checked); BITMAP is the frame bitmap, mutated under ALLOC_LOCKED.
    unsafe {
        let was_free = BITMAP[byte] & bit != 0;
        BITMAP[byte] |= bit;
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
    let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
    if hhdm == 0 {
        return; // HHDM not initialised - cannot walk tables safely.
    }

    // SAFETY: CR3 is always valid after Limine hands control to the kernel.
    let cr3: u64;
    unsafe { core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem)) };
    let pml4_phys = cr3 & !0xFFF_u64;

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
    if idx >= MAX_FRAMES {
        return;
    }
    let byte = idx / 8;
    let bit  = idx % 8;
    // SAFETY: idx < MAX_FRAMES; lock held.
    let currently_free = unsafe { BITMAP[byte] } & (1u8 << bit) != 0;
    if currently_free {
        unsafe { BITMAP[byte] &= !(1u8 << bit) };
        // SAFETY: free_frames was > 0 because the bit was set.
        unsafe { ALLOCATOR.free_frames -= 1 };
    }
    // Mark as permanently protected: free_frame will refuse to release this
    // frame regardless of how it ends up in a caller's reclaim buffer.
    // SAFETY: idx < MAX_FRAMES; lock held.
    unsafe { KERNEL_PT_PROTECTED[byte] |= 1u8 << bit };
}
