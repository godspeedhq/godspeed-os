// SPDX-License-Identifier: GPL-2.0-only
//! SMP coordination - §9, §11.1.
//!
//! Manages per-core state, IPI dispatch, and static service placement.
//! Unsafe boundary: raw APIC MMIO writes live in `ipi.rs`.

pub mod core;
pub mod ipi;
pub mod names;
pub mod percpu;
pub mod placement;
pub mod spinlock;

pub use spinlock::SpinLock;
pub use spinlock::without_interrupts;

use crate::arch::imp::BootInfo;

/// Size + initialise the boot-time per-core arenas (§26.6.1, `percpu`). Call ONCE after the frame
/// allocator is up and before any per-core state is touched (before `spawn_supervisor` / the APs).
///
/// N = the cores Limine reported (BSP + every AP it enumerated), with **no fixed ceiling** - the
/// machine's real core count is sized directly. Every per-core structure is a boot arena sized to N,
/// so a 4-core box reserves 4 slots and a 512-core box reserves 512; the only bound is RAM (each arena
/// carve panics loudly, §26.7, if it cannot be backed). There is no `MAX_CORES` clamp: nothing is a
/// fixed `[_; MAX_CORES]` array any more.
pub fn percpu_init(boot_info: &BootInfo) {
    let _ = boot_info;
    // SINGLE-CORE BUILD: size every per-core arena for the BSP alone. See kernel/Cargo.toml.
    let n = crate::arch::imp::ap_count() + 1; // BSP + every AP Limine enumerated (live count)
    percpu::set_num_cores(n);
    ipi::init_arenas(n);
    core::init_arenas(n);
    // Per-core user-copy arenas (the V1 read-scratch + copy-in-progress flag), sized to N.
    crate::arch::imp::syscall_entry::init_percore_arenas(n);
    // Per-core SYSCALL GS arena, sized to N; also re-points the BSP's GS from its bootstrap slot to
    // arena[0] (the BSP set GS in init_bsp, before this arena existed).
    crate::arch::imp::syscall_entry::init_percore_syscall_arena(n);
}

pub fn init(boot_info: &BootInfo) {
    core::init(boot_info);
    // Core 0's LAPIC id, published BEFORE any AP starts. On x86 this used to happen inside
    // `start_all_aps`, so a boot that started no APs never did it at all and core 0's identity
    // stayed an unwritten 0 - which is worse than absent, because callers ask `is_ready` first, get
    // true, and then trust the zero. See `publish_bsp_lapic_id`.
    crate::arch::imp::publish_bsp_lapic_id();
    // Start whatever APs the machine reports. A machine with none reaches the single-core path by
    // REPORTING none (11.3), not by being built differently - one artefact, every core count.
    // SAFETY: BSP APIC is already initialised in arch::imp::init_timer.
    unsafe { crate::arch::imp::ap_boot::start_all_aps(boot_info) };
}
