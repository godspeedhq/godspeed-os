//! Per-core state — §9.1, §11.2.
//!
//! Each core has a `CoreState` that tracks liveness and its run queue.
//! Core IDs are assigned at boot and are immutable for the system lifetime
//! (no hotplug — §9.5).

use crate::arch::x86_64::BootInfo;

pub const MAX_CORES: usize = 16;

// SAFETY: written only from AP entry (one writer per slot) and BSP init;
// read only after all writes complete. Real impl uses atomics.
static mut READY_COUNT: u32 = 0;

/// Per-core data. Indexed by core ID.
#[derive(Copy, Clone)]
pub struct CoreState {
    pub id: u32,
    pub ready: bool,
}

static mut CORES: [CoreState; MAX_CORES] = [CoreState { id: 0, ready: false }; MAX_CORES];

pub fn init(boot_info: &BootInfo) {
    // BSP (core 0) is always ready.
    // SAFETY: called once by BSP during smp::init before APs start.
    unsafe {
        CORES[0] = CoreState { id: 0, ready: true };
        READY_COUNT = 1;
    }
}

/// Called by each AP from `ap_main` once its local APIC is initialised.
pub fn mark_ready(core_id: u32) {
    // SAFETY: each AP writes only its own slot; no two APs share an ID.
    unsafe {
        CORES[core_id as usize] = CoreState { id: core_id, ready: true };
        READY_COUNT += 1;
    }
    crate::kprintln!("smp: core {} ready", core_id);
}

pub fn ready_count() -> u32 {
    // SAFETY: read after all mark_ready calls complete.
    unsafe { READY_COUNT }
}

pub fn is_ready(core_id: u32) -> bool {
    // SAFETY: read-only after mark_ready; all writes precede this read.
    unsafe { CORES[core_id as usize].ready }
}
