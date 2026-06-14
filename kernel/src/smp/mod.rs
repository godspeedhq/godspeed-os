// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! SMP coordination — §9, §11.1.
//!
//! Manages per-core state, IPI dispatch, and static service placement.
//! Unsafe boundary: raw APIC MMIO writes live in `ipi.rs`.

pub mod core;
pub mod ipi;
pub mod placement;
pub mod spinlock;

pub use spinlock::SpinLock;

use crate::arch::x86_64::BootInfo;

pub fn init(boot_info: &BootInfo) {
    core::init(boot_info);
    // SAFETY: BSP APIC is already initialised in arch::x86_64::init_timer.
    unsafe { crate::arch::x86_64::ap_boot::start_all_aps(boot_info) };
}
