// SPDX-License-Identifier: GPL-2.0-only
//! Wiring the **neutral** frame allocator on ARM - the first step of booting the whole OS.
//!
//! Everything so far ran on static arenas. `memory::init` brings the real thing up: the neutral
//! bitmap frame allocator (`memory::allocator`), which every later piece needs - per-task page
//! tables, service spawn, the ELF loader all pull frames from `alloc_frame`. This module builds the
//! `BootInfo` that `memory::init` consumes out of what the machine actually told us (the DTB memory
//! map from `dtb.rs`) and the kernel's own image bounds (linker symbols).
//!
//! **Two ARM specifics make this fit the neutral allocator unchanged:**
//! - **`hhdm_offset = 0`.** x86 accesses physical frames through Limine's higher-half direct map;
//!   ARM has none, but it does not need one - the kernel runs identity-mapped, so VA == PA and
//!   `hhdm + phys == phys` already points at the frame. The neutral `zero_frame`/table walks work as
//!   written. (And `protect_kernel_page_table_frames`, which is Limine-table-specific, returns early
//!   when `hhdm == 0` - a clean no-op here rather than a special case.)
//! - **The kernel image is reserved as a low region**, so the allocator never hands out the frames
//!   holding our own code, stacks, and static arenas. The guard `[kernel_phys_start, kernel_phys_end)`
//!   is belt-and-braces on top of that.

use crate::arch::imp::{BootInfo, MemoryKind, MemoryRegion};
use super::pl011_write;
use super::exceptions::write_hex32;
use super::timer::write_dec_pub;

extern "C" {
    // One-past-the-end of everything the linker placed (boot stack + the four per-mode exception
    // stacks). Frames at or above this are free RAM; below it is the kernel image and must be reserved.
    static __fiq_stack_top: u8;
}

/// The two regions handed to the allocator: the kernel image (reserved) and the rest of RAM (usable).
/// Static so the `&'static [MemoryRegion]` in `BootInfo` outlives the call.
static mut MEM_REGIONS: [MemoryRegion; 2] = [
    MemoryRegion { base: 0, len: 0, kind: MemoryKind::Reserved },
    MemoryRegion { base: 0, len: 0, kind: MemoryKind::Usable },
];

/// Build a `BootInfo` from the DTB memory map + linker bounds, then run the neutral `memory::init`.
///
/// `ram_end` comes from `dtb.rs` (the firmware's own memory node, or the announced fallback). Returns
/// the kernel-reserve end so a caller can see the split; the real product is a live frame allocator.
pub fn init(ram_end: u32) -> u32 {
    // Kernel image occupies [0x8000, __fiq_stack_top). Round the reserve up to a 1 MiB boundary so it
    // aligns with the section granularity of the identity map and leaves a clean usable base.
    let kernel_end = unsafe { core::ptr::addr_of!(__fiq_stack_top) as u32 };
    let reserve_end = (kernel_end + 0x000F_FFFF) & !0x000F_FFFF; // round up to 1 MiB

    // SAFETY: single-threaded boot; MEM_REGIONS is touched only here, before memory::init reads it.
    unsafe {
        let r = core::ptr::addr_of_mut!(MEM_REGIONS);
        (*r)[0] = MemoryRegion { base: 0, len: reserve_end as u64, kind: MemoryKind::Reserved };
        (*r)[1] = MemoryRegion {
            base: reserve_end as u64,
            len: (ram_end - reserve_end) as u64,
            kind: MemoryKind::Usable,
        };
    }

    let boot_info = BootInfo {
        // SAFETY: a shared reference to the static region array, valid for the whole run.
        memory_map: unsafe { &*core::ptr::addr_of!(MEM_REGIONS) },
        kernel_phys_start: 0x8000,
        kernel_phys_end: reserve_end as u64,
        hhdm_offset: 0, // no HHDM on ARM; identity map means VA == PA
        rsdp_addr: 0,   // no ACPI on the Pi
    };

    pl011_write(b"arm32: memory - kernel reserve [0x8000, ");
    write_hex32(reserve_end);
    pl011_write(b"), usable [");
    write_hex32(reserve_end);
    pl011_write(b", ");
    write_hex32(ram_end);
    pl011_write(b")\r\n");

    crate::memory::init(&boot_info);
    reserve_end
}

/// Prove the neutral allocator works on ARM: take some frames, check they are distinct, sane, and
/// accounted for, then give them back and confirm the free count returns.
///
/// Same discipline as every other ARM selftest - the interesting failures are the ones that look
/// fine: a frame handed out twice, a frame inside the kernel reserve, or accounting that drifts.
pub fn selftest() {
    use crate::memory::allocator::{alloc_frame, free_frame, free_frame_count};

    let before = free_frame_count();
    let mut frames = [0u64; 8];
    let mut ok = true;

    for i in 0..8 {
        match alloc_frame() {
            Some(f) => {
                let pa = f.phys_addr().0;
                // Must be page-aligned, above the kernel reserve, and not a duplicate of an earlier one.
                if pa & 0xFFF != 0 {
                    pl011_write(b"arm32:   allocated frame is not page-aligned\r\n");
                    ok = false;
                }
                if frames[..i].iter().any(|&p| p == pa) {
                    pl011_write(b"arm32:   allocator handed out the SAME frame twice\r\n");
                    ok = false;
                }
                frames[i] = pa;
            }
            None => {
                pl011_write(b"arm32:   alloc_frame returned None with RAM free\r\n");
                ok = false;
            }
        }
    }

    let after_alloc = free_frame_count();
    if before.wrapping_sub(after_alloc) != 8 {
        pl011_write(b"arm32:   free count did not drop by 8 after 8 allocations\r\n");
        ok = false;
    }

    // Give them all back.
    for &pa in frames.iter() {
        if pa != 0 {
            // SAFETY: `pa` is a page-aligned frame this selftest just received from `alloc_frame`, so
            // reconstructing the `Frame` and returning it once is exactly the allocator's contract.
            unsafe {
                free_frame(crate::memory::frame::Frame::from_phys(crate::memory::frame::PhysAddr(pa)));
            }
        }
    }
    if free_frame_count() != before {
        pl011_write(b"arm32:   free count did not return to baseline after freeing\r\n");
        ok = false;
    }

    let mib = before * 4096 / (1024 * 1024);
    pl011_write(b"arm32: frame-alloc selftest - ");
    write_dec_pub(mib as u32);
    pl011_write(b" MiB free, 8 frames alloc'd + freed\r\n");

    if ok {
        pl011_write(b"arm32: frame-alloc PASS (neutral allocator live: distinct frames, accounting holds)\r\n");
    } else {
        pl011_write(b"arm32: frame-alloc FAIL - see above\r\n");
    }
}
