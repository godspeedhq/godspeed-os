//! Physical frame allocator — §10.
//!
//! Bitmap allocator: one bit per 4 KiB frame.  0 = used, 1 = free.
//! Covers up to 4 GiB of physical address space (128 KiB of bitmap in .bss).
//! All frames start marked used; `init_from_map` opens the usable regions.
//!
//! v1 uses a single global protected by the single-core boot invariant.
//! A spinlock replaces the raw `static mut` when SMP goes live (Milestone 6).

use crate::arch::x86_64::{BootInfo, MemoryKind};
use crate::memory::frame::{Frame, PhysAddr, FRAME_SIZE};

// ---------------------------------------------------------------------------
// Bitmap — lives in .bss (zero-init = every frame starts as "used").
// ---------------------------------------------------------------------------

const FRAME_SIZE_USIZE: usize = FRAME_SIZE as usize;
const MAX_FRAMES: usize = (4 * 1024 * 1024 * 1024_usize) / FRAME_SIZE_USIZE;
const BITMAP_BYTES: usize = MAX_FRAMES / 8; // 128 KiB

// 0 = used, 1 = free; zero-init means all used at startup.
static mut BITMAP: [u8; BITMAP_BYTES] = [0u8; BITMAP_BYTES];

// ---------------------------------------------------------------------------
// Allocator.
// ---------------------------------------------------------------------------

struct BitmapAllocator {
    free_frames: usize,
    /// Byte-index scan hint — avoids rescanning from 0 on every alloc.
    next_byte: usize,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self { free_frames: 0, next_byte: 0 }
    }

    unsafe fn init_from_map(&mut self, boot_info: &BootInfo) {
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

            for idx in first..last {
                if idx >= MAX_FRAMES { break; }
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

        let phys = PhysAddr(idx as u64 * FRAME_SIZE);
        // SAFETY: idx from free bitmap → page-aligned; now exclusively owned.
        Some(unsafe { Frame::from_phys(phys) })
    }

    unsafe fn free(&mut self, frame: Frame) {
        let idx = frame.frame_number() as usize;
        debug_assert!(idx < MAX_FRAMES, "free_frame: address out of range");
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
// Global instance + public API.
// ---------------------------------------------------------------------------

static mut ALLOCATOR: BitmapAllocator = BitmapAllocator::new();

pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by BSP during memory::init, before any allocation.
    unsafe { ALLOCATOR.init_from_map(boot_info) };
}

/// Allocate one physical frame. Returns `None` if memory is exhausted.
pub fn alloc_frame() -> Option<Frame> {
    // SAFETY: single-core until Milestone 6 adds the spinlock.
    unsafe { ALLOCATOR.alloc() }
}

/// Return a frame to the allocator.
///
/// # Safety
/// The frame must have been obtained from `alloc_frame` and must not be used
/// after this call.
pub unsafe fn free_frame(frame: Frame) {
    // SAFETY: caller guarantees exclusive ownership and post-free non-use.
    unsafe { ALLOCATOR.free(frame) }
}

/// Total free frames available (used for diagnostic output in memory::init).
pub fn free_frame_count() -> usize {
    // SAFETY: read-only; single-core until Milestone 6.
    unsafe { ALLOCATOR.free_frames() }
}
