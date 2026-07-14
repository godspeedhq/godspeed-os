// SPDX-License-Identifier: GPL-2.0-only
//! Per-core state - §9.1, §11.2.
//!
//! Each core has a `CoreState` that tracks liveness. Core IDs are assigned at boot
//! and are immutable for the system lifetime (no hotplug - §9.5).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::arch::imp::BootInfo;
use crate::smp::percpu::{num_cores, PerCore};

static READY_COUNT: AtomicU32 = AtomicU32::new(0);

/// Per-core data, one slot per core in a boot-sized arena. `ready` is atomic so a core flips its own
/// bit while others read it (single-writer per slot, but read cross-core).
pub struct CoreState {
    pub id: u32,
    pub ready: AtomicBool,
}

/// Liveness + identity per core. Boot-sized arena (`smp::percpu`), allocated in `init_arenas`.
static CORES: PerCore<CoreState> = PerCore::new();

/// Local APIC ID for each core (indexed by core ID). Set by the BSP during AP startup; read-only after.
static CORE_LAPIC_ID: PerCore<AtomicU32> = PerCore::new();

/// Allocate the per-core liveness + LAPIC-id arenas for `n` cores. Call ONCE at boot
/// (`smp::percpu_init`), after the frame allocator is up and before any core state is read - which
/// includes the supervisor spawn's placement check. All cores start not-ready (`id` = index); `init`
/// marks the BSP, `mark_ready` marks each AP.
pub fn init_arenas(n: usize) {
    CORES.init_with(n, |i| CoreState { id: i as u32, ready: AtomicBool::new(false) });
    CORE_LAPIC_ID.init_with(n, |_| AtomicU32::new(0));
}

pub fn init(boot_info: &BootInfo) {
    let _ = boot_info;
    // BSP (core 0) is always ready.
    CORES.get(0).ready.store(true, Ordering::Relaxed);
    READY_COUNT.store(1, Ordering::Relaxed);
}

/// Store the local APIC ID for the given core.
///
/// Called by the BSP in `start_all_aps` before any AP starts, so there is no concurrent access.
pub fn set_core_lapic_id(core_id: u32, lapic_id: u32) {
    CORE_LAPIC_ID.get(core_id as usize).store(lapic_id, Ordering::Relaxed);
}

/// Return the local APIC ID for the given core.
pub fn core_lapic_id(core_id: u32) -> u32 {
    CORE_LAPIC_ID.get(core_id as usize).load(Ordering::Relaxed)
}

/// Translate a local APIC ID to its assigned core ID.
/// Returns 0 (BSP) if no match is found.
pub fn lapic_to_core_id(lapic_id: u32) -> u32 {
    for i in 0..num_cores() {
        if CORES.get(i).ready.load(Ordering::Acquire)
            && CORE_LAPIC_ID.get(i).load(Ordering::Relaxed) == lapic_id
        {
            return i as u32;
        }
    }
    0
}

/// Called by each AP from `ap_main` once its local APIC is initialised.
pub fn mark_ready(core_id: u32) {
    // Each AP writes only its own slot; no two APs share an ID.
    CORES.get(core_id as usize).ready.store(true, Ordering::Release);
    READY_COUNT.fetch_add(1, Ordering::Release);
    crate::kprintln!("smp: core {} ready", core_id);
}

pub fn ready_count() -> u32 {
    READY_COUNT.load(Ordering::Acquire)
}

pub fn is_ready(core_id: u32) -> bool {
    let c = core_id as usize;
    // A contract may name a core beyond the machine's count; that core is simply not ready.
    if c >= num_cores() {
        return false;
    }
    CORES.get(c).ready.load(Ordering::Acquire)
}
