// SPDX-License-Identifier: GPL-2.0-only
//! AArch64 MMU bring-up for the Raspberry Pi 4 (BCM2711, Cortex-A72).
//!
//! **Nothing here carries over from the 32-bit port.** ARMv7 uses short descriptors driven through
//! CP15; AArch64 uses long descriptors and system registers (`TTBR0_EL1`, `TCR_EL1`, `MAIR_EL1`,
//! `SCTLR_EL1`). They share vocabulary and no code, which is why `arch/arm/` and `arch/aarch64/` are
//! separate ports rather than one with `#[cfg]`s.
//!
//! ## The layout, and why this shape
//!
//! 4 KiB granule with a **39-bit** virtual address space (`T0SZ = 25`). That starts translation at
//! level 1, so the walk is L1 (1 GiB per entry) -> L2 (2 MiB) -> L3 (4 KiB) and there is no L0 table to
//! carry. 39 bits is 512 GiB of VA, which is ample and one table shallower than the 48-bit arrangement.
//!
//! The first 4 GiB is identity-mapped with **2 MiB blocks at L2**: four L1 entries, each pointing at a
//! 512-entry L2 table. Blocks rather than pages because nothing here needs 4 KiB granularity yet, and
//! 2 MiB rather than 1 GiB because the top of the address space mixes RAM and peripherals and a 1 GiB
//! block cannot describe that split. Total cost is one L1 plus four L2 tables - 20 KiB of `.bss`, a
//! fixed and visible footprint (§26.6.1: bounded memory means arenas, not a heap).
//!
//! ## The RAM / device split
//!
//! Device memory must be mapped **Device-nGnRnE**, not Normal. Getting this wrong does not fail
//! cleanly: the core may reorder, merge or speculatively repeat accesses to a UART or a controller
//! register, and the symptom is a peripheral that behaves erratically rather than a fault pointing at
//! the mapping. So everything from [`DEVICE_BASE`] up is Device, and RAM below it is Normal
//! write-back cacheable and inner-shareable (the shareability the A72's coherency needs once SMP
//! arrives).
//!
//! Device pages are also marked never-execute (UXN + PXN). Executing from MMIO is never intended, and
//! saying so costs one bit.

use core::sync::atomic::{compiler_fence, Ordering};

/// Entries per table at a 4 KiB granule (512 * 8 bytes = one 4 KiB table).
const ENTRIES: usize = 512;
/// How much of the physical address space we identity-map: the low 4 GiB, which covers the Pi 4's RAM
/// and every peripheral window. Four L1 entries at 1 GiB each.
const L1_USED: usize = 4;
/// Bytes described by one L2 block descriptor.
const BLOCK_SIZE: u64 = 2 * 1024 * 1024;

/// Where RAM stops and peripherals begin, for mapping purposes.
///
/// The BCM2711 in low-peripheral mode puts the main peripheral window at `0xFE00_0000` and the GIC-400
/// at `0xFF84_0000`. The VideoCore also reserves memory at the top of RAM, so the ARM never owns the
/// space immediately below the peripherals either. Rounding the boundary down to `0xFC00_0000` covers
/// both with one comparison and costs at most a little RAM that the firmware had already taken.
const DEVICE_BASE: u64 = 0xFC00_0000;

/// Bring-up shim: blocks below this, plus the peripheral window, are built **EL0-accessible**.
///
/// EL0 currently shares the kernel's single identity map, so the demo task needs `AP` granting it
/// access to the image, its stacks, and the UART it prints through. Real tasks get their own page
/// tables (§10.1) and none of this.
///
/// **Decided at table-BUILD time, deliberately.** The first attempt flipped `AP` on the live map and
/// then flushed - and `tlbi vmalle1`, the first TLB maintenance ever executed with the MMU on, never
/// returned. The descriptors were provably correct either side of the change (`0x705 -> 0x745`,
/// verified on hardware), `dsb` completed, and the machine died on the invalidate with no exception
/// report - which is what a failed instruction fetch looks like when the handler cannot fetch its own
/// vector either.
///
/// Rather than fight that, the access is decided before translation is on, where no maintenance is
/// needed. That is also what a real design does: mutating a live translation table requires
/// break-before-make, and per-task tables are built complete and then switched to, never patched
/// underneath a running core. The `tlbi`-with-MMU-on question is real and deferred, not solved - see
/// `docs/aarch64.md`.
const EL0_SHIM_LIMIT: u64 = 0x40_0000;

// --- Descriptor bits (Armv8-A long descriptor, stage 1) ---
const DESC_TABLE: u64 = 0b11; // points at a next-level table
const DESC_BLOCK: u64 = 0b01; // maps a block directly (valid at L1 and L2)
const DESC_AF: u64 = 1 << 10; // Access Flag - a walk with this clear takes an access fault
const DESC_SH_INNER: u64 = 0b11 << 8; // inner shareable (Normal memory only; ignored for Device)
const DESC_AP_RW_EL1: u64 = 0b00 << 6; // EL1 read/write, EL0 no access
const DESC_PXN: u64 = 1 << 53; // never execute at EL1
const DESC_UXN: u64 = 1 << 54; // never execute at EL0

/// `MAIR_EL1` attribute slots, referenced by a descriptor's `AttrIndx` field.
const MAIR_IDX_DEVICE: u64 = 0;
const MAIR_IDX_NORMAL: u64 = 1;
#[inline]
const fn attr_idx(i: u64) -> u64 {
    i << 2
}

/// `MAIR_EL1`: slot 0 = Device-nGnRnE (0x00), slot 1 = Normal write-back read/write-allocate (0xFF).
const MAIR_VALUE: u64 = 0x00 | (0xFF << 8);

/// Page tables. `.bss`, 4 KiB aligned as the architecture requires for a table base.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES]);

static mut L1: Table = Table([0; ENTRIES]);
static mut L2: [Table; L1_USED] = [
    Table([0; ENTRIES]),
    Table([0; ENTRIES]),
    Table([0; ENTRIES]),
    Table([0; ENTRIES]),
];

/// Build the identity map and turn translation on.
///
/// Returns the physical address installed in `TTBR0_EL1`, so the caller can report it rather than
/// assert that something happened.
pub fn enable() -> u64 {
    // SAFETY: single-threaded boot, MMU still off, and these statics are this function's exclusively
    // until translation is live. Every write below is a plain store to `.bss` we own.
    let ttbr = unsafe {
        let l1 = &raw mut L1;
        let l2 = &raw mut L2;

        for i in 0..L1_USED {
            // L1 entry i -> L2 table i.
            let l2_pa = (&raw const (*l2)[i]) as u64;
            (*l1).0[i] = l2_pa | DESC_TABLE;

            for j in 0..ENTRIES {
                let pa = (i as u64) * (ENTRIES as u64) * BLOCK_SIZE + (j as u64) * BLOCK_SIZE;
                // EL0 access for the shim ranges, decided here rather than patched in later.
                let el0 = if pa < EL0_SHIM_LIMIT || pa >= DEVICE_BASE { 1 << 6 } else { 0 };
                (*l2)[i].0[j] = el0 | if pa >= DEVICE_BASE {
                    // MMIO: Device-nGnRnE, never executable. No shareability - it is meaningless for
                    // Device memory and the architecture ignores the field.
                    pa | DESC_BLOCK | DESC_AF | DESC_AP_RW_EL1
                        | attr_idx(MAIR_IDX_DEVICE) | DESC_PXN | DESC_UXN
                } else {
                    // RAM: Normal write-back, inner shareable. Left executable - the kernel image is
                    // in here. W^X is a later refinement, once there are separate mappings to protect.
                    pa | DESC_BLOCK | DESC_AF | DESC_SH_INNER | DESC_AP_RW_EL1
                        | attr_idx(MAIR_IDX_NORMAL)
                };
            }
        }

        let ttbr = l1 as u64;

        // TCR_EL1. T0SZ=25 gives a 39-bit VA so the walk starts at L1; TTBR1 is disabled because
        // nothing uses the upper half yet, and leaving it enabled would let a stray high address walk
        // a table we never built.
        let tcr: u64 = 25          // T0SZ
            | (0b01 << 8)          // IRGN0: inner write-back write-allocate
            | (0b01 << 10)         // ORGN0: outer write-back write-allocate
            | (0b11 << 12)         // SH0: inner shareable
            | (0b00 << 14)         // TG0: 4 KiB granule
            | (1 << 23)            // EPD1: no table walks via TTBR1
            | (0b010 << 32);       // IPS: 40-bit intermediate physical address (1 TiB) - covers 4 GiB

        core::arch::asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1,  {tcr}",
            "msr ttbr0_el1,{ttbr}",
            "dsb ish",             // publish the tables before anything may walk them
            "isb",
            mair = in(reg) MAIR_VALUE,
            tcr  = in(reg) tcr,
            ttbr = in(reg) ttbr,
            options(nostack),
        );

        // Invalidate any stale translations, then enable. The TLB is architecturally UNKNOWN out of
        // reset, so entering with whatever it happens to hold is a genuine hazard rather than
        // paranoia.
        core::arch::asm!(
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack),
        );

        compiler_fence(Ordering::SeqCst);

        // SCTLR_EL1: M (translation), C (data cache), I (instruction cache). The instruction after the
        // `isb` executes translated - and because the map is identity, the program counter, the stack
        // and the UART all keep the addresses they already had. That is the whole reason to identity
        // map first and relocate later, if ever.
        let mut sctlr: u64;
        core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack));
        sctlr |= (1 << 0) | (1 << 2) | (1 << 12);
        core::arch::asm!(
            "msr sctlr_el1, {v}",
            "isb",
            v = in(reg) sctlr,
            options(nostack),
        );

        ttbr
    };
    ttbr
}

/// Was `allow_el0` - REMOVED. It patched `AP` on the live map and flushed, and the flush was fatal
/// (see `EL0_SHIM_LIMIT`). EL0 access is now decided when the tables are built, so there is no live
/// mutation and no maintenance to get wrong. Kept as a note rather than dead code so the next person
/// does not reinvent it.
/// Whether translation is currently on, read back from `SCTLR_EL1.M` rather than assumed.
pub fn is_enabled() -> bool {
    let sctlr: u64;
    // SAFETY: reading SCTLR_EL1 at EL1 is a side-effect-free system-register read.
    unsafe { core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack)) };
    sctlr & 1 != 0
}
