// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory management - §10.
//!
//! Initialised once by the BSP from the bootloader memory map.
//! Unsafe boundary: raw physical addresses are manipulated in frame.rs
//! and allocator.rs; the rest of the kernel sees only typed `Frame` handles.

pub mod allocator;
pub mod frame;
pub mod page;

use crate::arch::imp::BootInfo;

pub fn init(boot_info: &BootInfo) {
    // SAFETY: called once by BSP before any PageTable is created.
    unsafe { crate::arch::imp::page_tables::set_hhdm_offset(boot_info.hhdm_offset) };
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

/// Give back an address space a spawn built and never started: every frame under `root` the task would
/// own - its image, its stack, its context page, the tables that map them - and the root itself, by the
/// same two calls the kill path uses for a dead task (backlog/90 D15, kernel-audit T1 and A5-11). Returns
/// how many frames that was, root included.
///
/// A safe fn with a contract, the shape 18.5 asks for: `root` is a page-table root the caller built for a
/// task that never ran, so no core has it installed and nothing else holds a pointer into it. Breaking
/// that frees frames still in use; the one caller is `UnstartedSpace`, below.
fn reclaim_unstarted_address_space(root: u64) -> usize {
    // SAFETY: by this fn's contract no core runs in `root` and nothing else references it, which is
    // what both calls require of a dead task's root; the arch walks free only what the task owned.
    unsafe {
        let n = crate::arch::imp::page_tables::reclaim_user_frames(root);
        crate::arch::imp::page_tables::free_page_table_root(root);
        n + 1
    }
}

/// The address space a spawn is building, given back if it is dropped while still armed (backlog/90 D15).
///
/// A spawn that failed after building its page table gave back the slot, the endpoints, the device and the
/// display, and left the address space to nobody: the image, the user stack, the context page and the
/// tables mapping them, on every failure - kernel-audit T1 on x86, and A5-11 on the Pi 2, whose L1 tables
/// come from a sixteen-slot arena that repeated failures exhaust; the supervisor's retry leaked another set
/// each time. The loader arms one when it creates the table and the spawn another when the loader hands
/// it over; each disarms when the root passes on. Any early return in between drops it.
pub struct UnstartedSpace<'a> {
    root: u64,
    what: &'a str,
}

impl<'a> UnstartedSpace<'a> {
    /// Own `root`, the page table being built for `what` (a task's name, or the loader).
    pub fn new(root: u64, what: &'a str) -> Self {
        Self { root, what }
    }

    /// The root has passed on - to the next owner, or to a task that will run in it.
    pub fn disarm(&mut self) {
        self.root = 0;
    }
}

impl Drop for UnstartedSpace<'_> {
    fn drop(&mut self) {
        if self.root != 0 {
            let n = reclaim_unstarted_address_space(self.root);
            crate::kprintln!("spawn: '{}' failed - its address space given back ({} frame(s))", self.what, n);
        }
    }
}
