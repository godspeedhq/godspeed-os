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

/// One past the highest physical address the identity map reaches.
///
/// Exported because `memmap` must clamp the firmware's memory map to it: an 8 GiB Pi 4 reports banks
/// above 4 GiB, and recording those as usable would hand the allocator RAM that is never mapped. The
/// two numbers must agree, so there is exactly one of them.
pub const IDENTITY_MAP_LIMIT: u64 = (L1_USED as u64) * 1024 * 1024 * 1024;
/// Bytes described by one L2 block descriptor.
const BLOCK_SIZE: u64 = 2 * 1024 * 1024;

/// Where RAM stops and peripherals begin, for mapping purposes.
///
/// The BCM2711 in low-peripheral mode puts the main peripheral window at `0xFE00_0000` and the GIC-400
/// at `0xFF84_0000`. The VideoCore also reserves memory at the top of RAM, so the ARM never owns the
/// space immediately below the peripherals either. Rounding the boundary down to `0xFC00_0000` covers
/// both with one comparison and costs at most a little RAM that the firmware had already taken.
const DEVICE_BASE: u64 = 0xFC00_0000;

extern "C" {
    static __el0_start: u8;
    static __el0_end: u8;
}

/// Bring-up shim: the linker-placed EL0 region, plus the peripheral window, are built EL0-accessible.
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
/// **The region must not share a 2 MiB block with kernel code, and that is the whole point.** In the
/// EL1&0 translation regime a region accessible from EL0 is forced **PXN** - the kernel may not execute
/// what userspace can reach (ARM's equivalent of x86 SMEP). Granting EL0 access to the block holding
/// the kernel's `.text` therefore makes the kernel non-executable at EL1, and the core dies on its next
/// instruction fetch with no way to say so, because the exception handler cannot fetch its vector
/// either.
///
/// That cost two hardware round trips, presenting first as `tlbi vmalle1` hanging and then as the MMU
/// enable hanging - the same permission fault, landing wherever the new mapping happened to take
/// effect. The `tlbi` was never at fault.
///
/// So the EL0 code and stack live in their own 2 MiB-aligned `.el0` region (see the linker script), and
/// only that region is granted EL0 access. Deciding it at table-build time also avoids mutating a live
/// map, which would need break-before-make - and which per-task tables (§10.1) will not do either, since
/// they are built complete and then switched to.
fn el0_region() -> (u64, u64) {
    // SAFETY: linker-provided symbols; taking their addresses does not dereference them.
    // Converted to PHYSICAL: the comparison below is against a physical block address, and the kernel
    // is linked high, so the raw symbols would never match anything.
    unsafe {
        (
            virt_to_phys(core::ptr::addr_of!(__el0_start) as u64),
            virt_to_phys(core::ptr::addr_of!(__el0_end) as u64),
        )
    }
}

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

/// Base of the kernel's high half - the `TTBR1_EL1` region.
///
/// With `T1SZ = 25` the high range is the top 512 GiB of the 64-bit space, and physical `P` is mapped
/// at `KERNEL_VA_BASE + P`. That makes it a **direct map**, the same thing Limine's HHDM is on x86, so
/// the neutral allocator's `hhdm_offset` has a real value here once the kernel moves up.
///
/// The address is chosen so that `KERNEL_VA_BASE >> 30 & 511 == 0`: the high L1's entry *i* covers the
/// same GiB as the low L1's entry *i*, so the high tables are built by exactly the same loop rather
/// than by a second, subtly-different one.
pub const KERNEL_VA_BASE: u64 = 0xFFFF_FF80_0000_0000;

/// Convert a kernel virtual address to its physical address.
///
/// The kernel is linked high (see the linker script), so the address of any static is a `TTBR1`
/// address. Anything the HARDWARE consumes - a `TTBR` value, a page-table entry's output address, a
/// DMA address - must be physical, and this is the one conversion that gets it there.
///
/// It tolerates an address that is already physical (below the high base) so it can be used on both
/// sides of the boot transition without a second function that callers must remember to choose
/// between. Before the jump a static's address is still high (the linker made it so), so the
/// conversion is doing real work even then.
#[inline]
pub fn virt_to_phys(va: u64) -> u64 {
    if va >= KERNEL_VA_BASE { va - KERNEL_VA_BASE } else { va }
}

static mut L1_HIGH: Table = Table([0; ENTRIES]);
static mut L2_HIGH: [Table; L1_USED] = [
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
        let (el0_lo, el0_hi) = el0_region();

        for i in 0..L1_USED {
            // L1 entry i -> L2 table i.
            let l2_pa = virt_to_phys((&raw const (*l2)[i]) as u64);
            (*l1).0[i] = l2_pa | DESC_TABLE;

            for j in 0..ENTRIES {
                let pa = (i as u64) * (ENTRIES as u64) * BLOCK_SIZE + (j as u64) * BLOCK_SIZE;
                // EL0 access for the shim ranges, decided here rather than patched in later.
                let el0 = if (pa >= el0_lo && pa < el0_hi) || pa >= DEVICE_BASE { 1 << 6 } else { 0 };
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

        // The HIGH half: physical P at KERNEL_VA_BASE + P, kernel-only, same 2 MiB blocks. Built by
        // the same walk as the low map because the base was chosen to make the L1 indices line up.
        // No EL0 bit anywhere in here - the whole point of moving the kernel up is that a task's
        // address space (TTBR0) cannot name it at all.
        let l1h = &raw mut L1_HIGH;
        let l2h = &raw mut L2_HIGH;
        for i in 0..L1_USED {
            let l2_pa = virt_to_phys((&raw const (*l2h)[i]) as u64);
            (*l1h).0[i] = l2_pa | DESC_TABLE;
            for j in 0..ENTRIES {
                let pa = (i as u64) * (ENTRIES as u64) * BLOCK_SIZE + (j as u64) * BLOCK_SIZE;
                (*l2h)[i].0[j] = if pa >= DEVICE_BASE {
                    pa | DESC_BLOCK | DESC_AF | DESC_AP_RW_EL1
                        | attr_idx(MAIR_IDX_DEVICE) | DESC_PXN | DESC_UXN
                } else {
                    pa | DESC_BLOCK | DESC_AF | DESC_SH_INNER | DESC_AP_RW_EL1
                        | attr_idx(MAIR_IDX_NORMAL) | DESC_UXN
                };
            }
        }

        let ttbr = virt_to_phys(l1 as u64);
        let ttbr1 = virt_to_phys(l1h as u64);

        // TCR_EL1. T0SZ=T1SZ=25 gives a 39-bit VA at each end, so both walks start at L1.
        //
        // **TG1 does not use TG0's encoding.** TG0 spells 4 KiB as `0b00`; TG1 spells it `0b10`
        // (TG1: 01=16K, 10=4K, 11=64K). Copying the TG0 value into TG1 selects a 16 KiB granule, every
        // high walk fails, and the fault points at whatever first touched a high address rather than at
        // this register.
        let tcr: u64 = 25          // T0SZ
            | (0b01 << 8)          // IRGN0: inner write-back write-allocate
            | (0b01 << 10)         // ORGN0: outer write-back write-allocate
            | (0b11 << 12)         // SH0: inner shareable
            | (0b00 << 14)         // TG0: 4 KiB granule
            | (25 << 16)           // T1SZ: 39-bit high VA
            | (0b01 << 24)         // IRGN1
            | (0b01 << 26)         // ORGN1
            | (0b11 << 28)         // SH1: inner shareable
            | (0b10 << 30)         // TG1: 4 KiB granule - NOT 0b00, see above
            | (0b010 << 32);       // IPS: 40-bit intermediate physical address (1 TiB) - covers 4 GiB
        // Note EPD1 (bit 23) is now CLEAR: high walks are enabled.

        core::arch::asm!(
            "msr mair_el1, {mair}",
            "msr tcr_el1,  {tcr}",
            "msr ttbr0_el1,{ttbr}",
            "msr ttbr1_el1,{ttbr1}",
            "dsb ish",             // publish the tables before anything may walk them
            "isb",
            mair = in(reg) MAIR_VALUE,
            tcr  = in(reg) tcr,
            ttbr = in(reg) ttbr,
            ttbr1 = in(reg) ttbr1,
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
/// Move the running kernel into the high half: `SP` and `PC` both, in one step.
///
/// Called immediately after [`enable`], while **both** halves translate - TTBR0 still holds the
/// identity map and TTBR1 the high one. That overlap is what makes this safe: at no instant is the
/// core executing from an address that does not resolve.
///
/// It cannot be written as an ordinary function call. Adding the offset to `SP` invalidates every
/// frame pointer and return address already on the stack (they name low addresses), so the return path
/// must not be taken. Instead the caller passes the function to continue into, and this jumps there
/// with a fresh high stack - a one-way transition, which is why `continue_at` returns `!`.
///
/// # Safety
/// `enable` must have run (both TTBR0 and TTBR1 live), and `continue_at` must be a `#[no_mangle]`
/// `extern "C"` function that never returns. Anything still on the low stack is abandoned.
pub unsafe fn jump_high(continue_at: extern "C" fn() -> !) -> ! {
    let target = (continue_at as usize as u64) | KERNEL_VA_BASE;
    // SAFETY: `target` is the high alias of a mapped kernel function and the high half maps the whole
    // stack region, so the relocated SP is valid. Both the branch target and the new stack translate
    // through TTBR1, which `enable` just installed.
    unsafe {
        // The new SP is computed in Rust and handed in as a plain input, so this asm needs no scratch
        // register at all.
        //
        // The first version did the arithmetic in assembly with a hardcoded `x9` as scratch, while
        // letting the compiler allocate `{base}` - and it chose `x9` too. `mov x9, sp` destroyed the
        // base, `orr x9, x9, x9` was a no-op, and SP was left exactly as it had been.
        //
        // The resulting failure is worth remembering, because nothing about it pointed at the asm: the
        // kernel ran on perfectly, printing several more lines from its still-mapped low stack, and
        // only died when `drop_low_map` took that stack away - whereupon the exception handler faulted
        // on its own push and recursed, walking FAR down by one 272-byte frame each time. Reading SP
        // back and printing it settled it in a single run.
        let sp_now: u64;
        core::arch::asm!("mov {}, sp", out(reg) sp_now, options(nomem, nostack));
        let sp_high = sp_now | KERNEL_VA_BASE; // the same physical stack, named through the high half

        core::arch::asm!(
            "mov  sp, {sp}",
            "br   {target}",
            sp = in(reg) sp_high,
            target = in(reg) target,
            options(noreturn, nostack),
        );
    }
}

/// Are we executing from the high half? Read from the PC rather than assumed.
pub fn running_high() -> bool {
    let pc: u64;
    // SAFETY: `adr` of a local label is a plain PC-relative address computation.
    unsafe { core::arch::asm!("adr {}, .", out(reg) pc, options(nomem, nostack)) };
    pc >= KERNEL_VA_BASE
}

/// Retire the low identity map from `TTBR0_EL1`.
///
/// After this, TTBR0 translates nothing until a task's address space is installed - which is the whole
/// point of the move: a task table no longer has to carry the kernel, so a task page below 4 GiB can no
/// longer collide with it.
///
/// # Safety
/// The caller must already be executing from the high half ([`running_high`]), and nothing may still
/// hold a low pointer.
pub unsafe fn drop_low_map() {
    // SAFETY: caller is running high, so retiring the low map cannot pull the ground out from under the
    // instruction stream or the stack. The invalidate is required - writing TTBR0 flushes nothing.
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, xzr",
            "dsb ish",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

/// Prove the high half translates, before anything is built on it.
///
/// Writes a value through a physical (identity) address and reads it back through
/// `KERNEL_VA_BASE + phys`. If `TG1` were wrong, or `EPD1` still set, or `TTBR1_EL1` unwritten, this is
/// where it shows - at the point of the mistake, rather than as an unexplained fault later in whatever
/// first touched a high address.
///
/// Returns the (physical, high) pair it used, so the caller reports concrete numbers instead of "ok".
///
/// **Must run between [`enable`] and [`jump_high`]**, in the window where both halves translate. That
/// window is the only place the claim "these two addresses are the same memory" can be checked at all:
/// afterwards the low map is gone and the physical address is not addressable, which is precisely what
/// the move was for. Running it later faults on the low write - reported cleanly by the vectors, but a
/// test that cannot pass is not a test.
pub fn selftest_high_half() -> Option<(u64, u64)> {
    const MAGIC: u64 = 0x1122_3344_5566_7788;
    static mut SCRATCH: u64 = 0;

    // SAFETY: taking the address of this module's own static; no dereference yet.
    let addr = unsafe { (&raw const SCRATCH) as u64 };
    // Normalise to physical, then construct the high alias EXPLICITLY.
    //
    // Do not assume the symbol's address is high just because the kernel is linked high: this runs
    // before the jump, where the compiler reaches statics with PC-relative `adrp`, and a low PC
    // therefore yields a LOW address. The first version used the symbol address as the "high" side
    // and `virt_to_phys` of it as the "low" side - which produced the same number twice and passed
    // while comparing an address against itself.
    let phys = virt_to_phys(addr);
    let high = KERNEL_VA_BASE + phys;

    // SAFETY: `SCRATCH` is a live static, reachable at its physical address through the low identity
    // map and at `high` through the map just built. Both are ordinary Normal memory and the `dsb`/`isb`
    // in `enable` published the tables before either is touched.
    unsafe {
        (phys as *mut u64).write_volatile(MAGIC);
        if (high as *const u64).read_volatile() != MAGIC {
            return None;
        }
        // And the other direction, so this cannot pass on a stale line that happened to match.
        (high as *mut u64).write_volatile(!MAGIC);
        if (phys as *const u64).read_volatile() != !MAGIC {
            return None;
        }
    }
    Some((phys, high))
}

/// The kernel's own boot L1, as a physical address.
///
/// Per-task address spaces copy their kernel half from **this**, never from the live `TTBR0_EL1`. The
/// live root belongs to whichever task is running, and on the 32-bit port copying from it gave a child
/// its spawner's user entries - which the child's reclaim later freed out from under the still-running
/// spawner. The boot L1 holds only kernel mappings and cannot die, so it is the one safe source.
pub fn kernel_l1_root() -> u64 {
    // SAFETY: taking the address of this module's static; no dereference.
    unsafe { virt_to_phys((&raw const L1) as u64) }
}

/// Whether translation is currently on, read back from `SCTLR_EL1.M` rather than assumed.
pub fn is_enabled() -> bool {
    let sctlr: u64;
    // SAFETY: reading SCTLR_EL1 at EL1 is a side-effect-free system-register read.
    unsafe { core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr, options(nomem, nostack)) };
    sctlr & 1 != 0
}
