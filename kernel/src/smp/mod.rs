// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! SMP coordination - §9, §11.1.
//!
//! Manages per-core state, IPI dispatch, and static service placement.
//! Unsafe boundary: raw APIC MMIO writes live in `ipi.rs`.

pub mod core;
pub mod ipi;
pub mod percpu;
pub mod placement;
pub mod spinlock;

pub use spinlock::SpinLock;
pub use spinlock::without_interrupts;

use crate::arch::x86_64::BootInfo;

/// Size + initialise the boot-time per-core arenas (§26.6.1, `percpu`). Call ONCE after the frame
/// allocator is up and before any per-core state is touched (before `spawn_supervisor` / the APs).
///
/// N = the cores Limine reported (BSP + APs), **clamped to the `MAX_CORES` sanity ceiling**. A machine
/// with more cores than `MAX_CORES` runs on `MAX_CORES` of them and says so loudly (§26.7) - never a
/// silent truncation. (Linux's model: a generous `NR_CPUS` ceiling above boot-sized per-CPU areas.)
pub fn percpu_init(boot_info: &BootInfo) {
    let reported = boot_info.ap_ids.len() + 1; // BSP + every AP Limine enumerated
    let cap = core::MAX_CORES;
    let n = reported.min(cap);
    if reported > cap {
        crate::kprintln!(
            "smp: machine reports {reported} cores; this build handles {cap} (MAX_CORES) - {} unused",
            reported - cap
        );
    }
    percpu::set_num_cores(n);
    ipi::init_arenas(n);
}

pub fn init(boot_info: &BootInfo) {
    core::init(boot_info);
    // SAFETY: BSP APIC is already initialised in arch::x86_64::init_timer.
    unsafe { crate::arch::x86_64::ap_boot::start_all_aps(boot_info) };
}
