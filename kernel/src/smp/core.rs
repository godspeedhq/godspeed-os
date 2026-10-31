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
///
/// **Zero is right during bring-up and wrong after it.** A core asks before it is marked ready - the boot
/// core before `init`, an AP before `mark_ready` - and every caller of that window expects core 0. Once
/// the arch has reported its cores, a miss means a core is running as core 0, on core 0's per-core
/// state. That is said by `check_this_core_known`, on the tick, rather than here: this is the lookup
/// every syscall makes, and it stays a pure read (backlog/90 D28).
pub fn lapic_to_core_id(lapic_id: u32) -> u32 {
    // Before `init_arenas` only the boot core runs, and it is core 0. A kernel fault taken that early
    // asks this from its report, and reading the unallocated arena faulted the report itself (the Pi 4's
    // `kernel-wx-test`: FAR 0x4, `ready`'s offset from a null base).
    if !CORES.initialised() || !CORE_LAPIC_ID.initialised() {
        return 0;
    }
    for i in 0..num_cores() {
        if CORES.get(i).ready.load(Ordering::Acquire)
            && CORE_LAPIC_ID.get(i).load(Ordering::Relaxed) == lapic_id
        {
            return i as u32;
        }
    }
    0
}

/// This core's hardware id - the LAPIC id on x86, the hart or MPIDR affinity elsewhere - as the arch
/// reads it. The one `unsafe` read `task::scheduler::current_core_id` used to make itself, here in a
/// permitted layer (18.5).
pub fn current_lapic_id() -> u32 {
    // SAFETY: the arch's own read of this core's id register; mapped and valid before the scheduler or
    // the tick first runs on any core, which is when this is first called.
    unsafe { crate::arch::imp::boot::get_lapic_id() }
}

/// Say, once, that this core's id matches no ready core, once the arch has reported its cores. Called
/// from every core's tick (`timer_tick_from_irq`), so a core running as core 0 by mistake is named at
/// its first tick after bring-up (backlog/90 D28). Through the lock-free writer, not `kprintln`, which
/// would ask `lapic_to_core_id` which core it is on. Not a panic: a core that checks in late (backlog/90
/// D17) runs before it is ready, and that is the case worth seeing.
pub fn check_this_core_known() {
    if !CORES_REPORTED.load(Ordering::Relaxed) || MISS_SAID.load(Ordering::Relaxed) {
        return;
    }
    #[allow(unused_mut)] // written only by `lapic-miss-test`
    let mut id = current_lapic_id();
    // `lapic-miss-test`: the first check after bring-up asks for an id no core has.
    #[cfg(feature = "lapic-miss-test")]
    {
        static ASKED: AtomicBool = AtomicBool::new(false);
        if !ASKED.swap(true, Ordering::Relaxed) {
            id = 0xDEAD;
        }
    }
    for i in 0..num_cores() {
        if CORES.get(i).ready.load(Ordering::Acquire) && CORE_LAPIC_ID.get(i).load(Ordering::Relaxed) == id {
            return;
        }
    }
    if MISS_SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut line = *b"smp: LAPIC id 0x00000000 matches no ready core - it runs as core 0, on core 0's per-core state (backlog/90 D28; said once)\n";
    let digits = b"0123456789abcdef";
    for k in 0..8 {
        line[16 + k] = digits[((id >> (28 - 4 * k)) & 0xf) as usize];
    }
    crate::arch::imp::serial_write_bytes_lockfree(&line);
}

/// Set once the arch has reported its cores; after it, a miss is not bring-up (`check_this_core_known`).
static CORES_REPORTED: AtomicBool = AtomicBool::new(false);
/// Whether a miss after bring-up has been said; said once.
static MISS_SAID: AtomicBool = AtomicBool::new(false);

/// Called by each AP from `ap_main` once its local APIC is initialised.
pub fn mark_ready(core_id: u32) {
    // Each AP writes only its own slot; no two APs share an ID.
    CORES.get(core_id as usize).ready.store(true, Ordering::Release);
    READY_COUNT.fetch_add(1, Ordering::Release);
    crate::kprintln!("smp: core {} ready", core_id);
}

/// Announce how many cores reached the scheduler, in one canonical sentence.
///
/// **The wording lives here so every architecture cannot help but agree.** It used to be printed by
/// each arch's own bring-up: x86 said `kernel: N cores ready`, the Pi 2 said `smp: N cores ready`, and
/// the Pi 4 grew a third phrasing of its own. Three ports, three vocabularies, one fact - and a reader
/// comparing boot logs across boards had to translate before they could compare.
///
/// GodspeedOS should read the same whatever it is running on. Anything genuinely arch-specific (which
/// core stopped where, on a machine that came up short) belongs in a separate line the arch adds, not
/// in a different spelling of the shared one.
pub fn report_cores_ready() {
    // "1 core ready", not "1 cores ready". A single-core boot is a SUPPORTED configuration (§11.3
    // says so, and a machine whose APs all fail to start reaches it by accident), so this is a line
    // people will read on a real machine rather than a corner case nobody sees. A log that cannot
    // count its own subject invites the reader to wonder what else it is approximating.
    let n = ready_count();
    crate::kprintln!("smp: {} core{} ready", n, if n == 1 { "" } else { "s" });
    CORES_REPORTED.store(true, Ordering::Relaxed);
}

pub fn ready_count() -> u32 {
    READY_COUNT.load(Ordering::Acquire)
}

/// The `k`-th ready core's id, counting from 0 in id order; `None` past the last.
///
/// **A count is not an id range.** `ready_count()` says how many cores came up, not which: on a machine
/// whose AP 2 failed and AP 3 started (11.3 says carry on), the ready ids are 0, 1 and 3, and treating
/// `0..ready_count()` as the ids placed a service on core 2, which runs no scheduler, and never checked
/// core 3's liveness (backlog/90 D17). Placement and the watchdog ask by id through this and `is_ready`.
pub fn nth_ready(k: u32) -> Option<u32> {
    let mut seen = 0u32;
    for c in 0..num_cores() as u32 {
        if is_ready(c) {
            if seen == k { return Some(c); }
            seen += 1;
        }
    }
    None
}

pub fn is_ready(core_id: u32) -> bool {
    let c = core_id as usize;
    // A contract may name a core beyond the machine's count; that core is simply not ready.
    if c >= num_cores() {
        return false;
    }
    CORES.get(c).ready.load(Ordering::Acquire)
}
