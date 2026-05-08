//! Physical memory management — §10.
//!
//! Initialised once by the BSP from the bootloader memory map.
//! Unsafe boundary: raw physical addresses are manipulated in frame.rs
//! and allocator.rs; the rest of the kernel sees only typed `Frame` handles.

pub mod allocator;
pub mod frame;
pub mod ownership;
pub mod page;

use crate::arch::x86_64::BootInfo;

pub fn init(boot_info: &BootInfo) {
    allocator::init(boot_info);
    crate::kprintln!("memory: frame allocator ready");
}
