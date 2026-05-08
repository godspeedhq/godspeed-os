//! Physical frame allocator — §10.
//!
//! Initialised once from the bootloader memory map. Hands out 4 KiB frames
//! and reclaims them on service death. The allocator is protected by a
//! spinlock shared across all cores.
//!
//! v1 uses a simple free-list bitmap. Performance is not the priority (§20).

use crate::arch::x86_64::BootInfo;
use crate::memory::frame::{Frame, PhysAddr, FRAME_SIZE};

pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by BSP during memory::init, before any allocation.
    unsafe { ALLOCATOR.init_from_map(boot_info) };
}

/// Allocate one physical frame. Returns `None` if memory is exhausted.
pub fn alloc_frame() -> Option<Frame> {
    // SAFETY: spinlock guards concurrent access.
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

// ---

struct BitmapAllocator {
    // Placeholder: a real bitmap over the usable physical memory range.
    initialized: bool,
}

impl BitmapAllocator {
    const fn new() -> Self {
        Self { initialized: false }
    }

    unsafe fn init_from_map(&mut self, _boot_info: &BootInfo) {
        // Milestone 2: build free-list bitmap from boot_info.memory_map.
        self.initialized = true;
    }

    unsafe fn alloc(&mut self) -> Option<Frame> {
        todo!("find first free bit, mark used, return Frame")
    }

    unsafe fn free(&mut self, frame: Frame) {
        todo!("clear the bit for frame.frame_number()")
    }
}

static mut ALLOCATOR: BitmapAllocator = BitmapAllocator::new();
