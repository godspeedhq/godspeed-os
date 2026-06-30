// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory management - §10.
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
    // SAFETY: called once by BSP before any PageTable is created.
    unsafe { crate::arch::x86_64::page_tables::set_hhdm_offset(boot_info.hhdm_offset) };
    crate::kprintln!(
        "memory: kernel phys [{:#x}, {:#x}) hhdm={:#x}",
        boot_info.kernel_phys_start, boot_info.kernel_phys_end,
        boot_info.hhdm_offset,
    );
    allocator::init(boot_info);
    // Protect Limine's intermediate page-table frames for the kernel BSS
    // mapping from being handed out by alloc_frame (BA2 fix - see allocator.rs).
    allocator::protect_kernel_page_table_frames();
    let free_mib = allocator::free_frame_count() * 4096 / (1024 * 1024);
    crate::kprintln!("memory: frame allocator ready ({} MiB free)", free_mib);
}
