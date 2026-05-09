//! SMP coordination — §9, §11.1.
//!
//! Manages per-core state, IPI dispatch, and static service placement.
//! Unsafe boundary: raw APIC MMIO writes live in `ipi.rs`.

pub mod core;
pub mod ipi;
pub mod placement;

use crate::arch::x86_64::BootInfo;

pub fn init(boot_info: &BootInfo) {
    core::init(boot_info);
    // SAFETY: BSP APIC is already initialised in arch::x86_64::init_timer.
    unsafe { crate::arch::x86_64::ap_boot::start_all_aps(boot_info) };
}
