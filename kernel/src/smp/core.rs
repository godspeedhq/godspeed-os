//! Per-core state — §9.1, §11.2.
//!
//! Each core has a `CoreState` that tracks liveness and its run queue.
//! Core IDs are assigned at boot and are immutable for the system lifetime
//! (no hotplug — §9.5).

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::x86_64::BootInfo;

pub const MAX_CORES: usize = 16;

static READY_COUNT: AtomicU32 = AtomicU32::new(0);

/// Per-core data. Indexed by core ID.
#[derive(Copy, Clone)]
pub struct CoreState {
    pub id: u32,
    pub ready: bool,
}

static mut CORES: [CoreState; MAX_CORES] = [CoreState { id: 0, ready: false }; MAX_CORES];

/// Local APIC ID for each core (indexed by core ID).
/// Set by BSP during AP startup; read-only after that.
static mut CORE_LAPIC_ID: [u32; MAX_CORES] = [0u32; MAX_CORES];

pub fn init(boot_info: &BootInfo) {
    let _ = boot_info;
    // BSP (core 0) is always ready.
    // SAFETY: called once by BSP during smp::init before APs start.
    unsafe {
        CORES[0] = CoreState { id: 0, ready: true };
    }
    READY_COUNT.store(1, Ordering::Relaxed);
}

/// Store the local APIC ID for the given core.
///
/// Called by the BSP in `start_all_aps` before any AP starts, so there
/// is no concurrent access to `CORE_LAPIC_ID`.
pub fn set_core_lapic_id(core_id: u32, lapic_id: u32) {
    // SAFETY: written from BSP before APs start; read-only after.
    unsafe { CORE_LAPIC_ID[core_id as usize] = lapic_id; }
}

/// Return the local APIC ID for the given core.
pub fn core_lapic_id(core_id: u32) -> u32 {
    // SAFETY: read-only after set_core_lapic_id; written before any AP starts.
    unsafe { CORE_LAPIC_ID[core_id as usize] }
}

/// Translate a local APIC ID to its assigned core ID.
/// Returns 0 (BSP) if no match is found.
pub fn lapic_to_core_id(lapic_id: u32) -> u32 {
    // SAFETY: CORE_LAPIC_ID is read-only after start_all_aps; no racing writes.
    unsafe {
        for i in 0..MAX_CORES {
            if CORES[i].ready && CORE_LAPIC_ID[i] == lapic_id {
                return i as u32;
            }
        }
    }
    0
}

/// Called by each AP from `ap_main` once its local APIC is initialised.
pub fn mark_ready(core_id: u32) {
    // SAFETY: each AP writes only its own slot; no two APs share an ID.
    unsafe {
        CORES[core_id as usize] = CoreState { id: core_id, ready: true };
    }
    READY_COUNT.fetch_add(1, Ordering::Release);
    crate::kprintln!("smp: core {} ready", core_id);
}

pub fn ready_count() -> u32 {
    READY_COUNT.load(Ordering::Acquire)
}

pub fn is_ready(core_id: u32) -> bool {
    // SAFETY: read-only after mark_ready; all writes precede this read.
    unsafe { CORES[core_id as usize].ready }
}
