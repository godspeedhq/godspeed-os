//! Physical frame allocator — §10.
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
// Kernel-range guard — fires if alloc_frame ever returns a kernel-image frame.
// ---------------------------------------------------------------------------

static mut GUARD_START: u64 = 0;
static mut GUARD_END:   u64 = 0;

#[inline(never)]
fn guard_bugcheck(phys: u64) {
    // Write directly to COM1 — no lock, no allocator, no stack growth.
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
// Bitmap — lives in .bss (zero-init = every frame starts as "used").
// ---------------------------------------------------------------------------

const FRAME_SIZE_USIZE: usize = FRAME_SIZE as usize;
const MAX_FRAMES: usize = (4 * 1024 * 1024 * 1024_usize) / FRAME_SIZE_USIZE;
const BITMAP_BYTES: usize = MAX_FRAMES / 8; // 128 KiB

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
    /// Byte-index scan hint — avoids rescanning from 0 on every alloc.
    next_byte: usize,
    /// Highest frame index (exclusive) that was ever marked usable.
    /// Any frame index at or above this value was never handed out by the
    /// allocator and must not be accepted by `free`.
    max_valid_frame: usize,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self { free_frames: 0, next_byte: 0, max_valid_frame: 0 }
    }

    unsafe fn init_from_map(&mut self, boot_info: &BootInfo) {
        let kstart = boot_info.kernel_phys_start;
        let kend   = boot_info.kernel_phys_end;

        for region in boot_info.memory_map {
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
    }

    unsafe fn alloc(&mut self) -> Option<Frame> {
        // SAFETY: exclusive access guaranteed by single-core invariant (v1).
        let bitmap = unsafe { &mut BITMAP };

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

    unsafe fn free(&mut self, frame: Frame) {
        let idx = frame.frame_number() as usize;
        // Reject phantom frames: addresses that were never in the usable RAM
        // range.  These arise when a corrupt or stale page-table entry (from a
        // re-animated dead task) is walked and freed.  Setting a bit for an
        // out-of-range frame would allow alloc to return a phantom address,
        // which would then fault the kernel on its next HHDM access.
        if idx >= self.max_valid_frame {
            crate::kprintln!(
                "free_frame: IGNORED phantom frame idx={} (max_valid={})",
                idx, self.max_valid_frame
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
        // SAFETY: idx within bounds; caller guarantees exclusive ownership.
        unsafe { bitmap_set_free(idx) };
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
unsafe fn bitmap_set_free(idx: usize) {
    unsafe { BITMAP[idx / 8] |= 1u8 << (idx % 8) };
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

#[inline]
fn alloc_lock() {
    while ALLOC_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
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
        ALLOCATOR.init_from_map(boot_info)
    };
}

/// Allocate one physical frame. Returns `None` if memory is exhausted.
pub fn alloc_frame() -> Option<Frame> {
    alloc_lock();
    // SAFETY: lock held; single writer across all cores.
    let frame = unsafe { ALLOCATOR.alloc() };
    alloc_unlock();
    frame
}

/// Return a frame to the allocator.
///
/// # Safety
/// The frame must have been obtained from `alloc_frame` and must not be used
/// after this call.
pub unsafe fn free_frame(frame: Frame) {
    alloc_lock();
    // SAFETY: lock held; caller guarantees exclusive ownership.
    unsafe { ALLOCATOR.free(frame) }
    alloc_unlock();
}

/// Total free frames available (used for diagnostic output in memory::init).
pub fn free_frame_count() -> usize {
    // SAFETY: read-only; racing reads are harmless for diagnostic use.
    unsafe { ALLOCATOR.free_frames() }
}

/// Walk the kernel half of the live PML4 (entries 256–511) and mark every
/// PDPT / PD / PT / PML4 frame as "used" in the bitmap allocator.
///
/// Root cause this closes (BA2):
///   Limine allocates intermediate page-table frames for the kernel BSS mapping
///   from physical pages that appear as `Usable` in its memory map but lie below
///   the kernel image guard range [kstart, kend).  `init_from_map` opens those
///   frames in the bitmap; `alloc_frame` then returns them; `walk_or_alloc` /
///   `PageTable::new` zero them, destroying the kernel's PTE for the BSS page
///   being accessed — causing a KERNEL PF on the first write (BA2: write to
///   kstack_marker(90) at 0xffffffff80e09260 after many spawn/kill cycles).
///
/// Must be called after `allocator::init` (bitmap populated) and after
/// `set_hhdm_offset` (physical↔virtual translation live).
pub fn protect_kernel_page_table_frames() {
    let hhdm = crate::arch::x86_64::page_tables::get_hhdm_offset();
    if hhdm == 0 {
        return; // HHDM not initialised — cannot walk tables safely.
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
                if pdpte & (1 << 7) != 0 { continue; } // 1 GiB huge — no PD below
                let pd_phys = pdpte & 0x000F_FFFF_FFFF_F000;
                mark_pt_frame_used(pd_phys);
                for pd_i in 0..512usize {
                    let pde = pt_read(hhdm, pd_phys, pd_i);
                    if pde & 1 == 0 { continue; }
                    if pde & (1 << 7) != 0 { continue; } // 2 MiB huge — no PT below
                    let pt_phys = pde & 0x000F_FFFF_FFFF_F000;
                    mark_pt_frame_used(pt_phys);
                    // PT entries are leaf mappings — the data frames they point
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
