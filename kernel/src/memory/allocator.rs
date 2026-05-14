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
