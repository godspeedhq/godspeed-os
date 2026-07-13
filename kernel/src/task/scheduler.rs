// SPDX-License-Identifier: GPL-2.0-only
//! Per-core round-robin scheduler - §9.1, §9.3.
//!
//! Each core has a static run queue of up to MAX_TASKS slots.  Tasks are
//! pinned to cores at enqueue time and never migrate (§9.1).  The 10 ms
//! preemption quantum is enforced by the local APIC timer; `yield` is
//! advisory (§9.3).
//!
//! Cross-core wakeups (§8.4, §9.4): `wake_by_slot` sends a WAKE_RECEIVER
//! IPI when the target task lives on a different core.

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::arch::x86_64::context_switch::{switch_context, TaskContext};
use crate::capability::cap::{CapError, Capability};
use crate::capability::rights::Rights;
use crate::capability::table::CapTable;
use crate::ipc::endpoint::EndpointId;
use crate::ipc::message::Message;
use crate::ipc::routing;
use crate::smp::percpu::{num_cores, PerCore, PerCoreMut};
use crate::smp::SpinLock;
use crate::task::state::TaskState;

// ---------------------------------------------------------------------------
// Flat task table (all cores share one array; tasks are pinned by TASK_CORE).
// ---------------------------------------------------------------------------

pub const MAX_TASKS: usize = 224;

/// Sentinel meaning "no task running" (scheduler idle loop active).
const IDLE: usize = MAX_TASKS;

static mut TASK_CTX:   [MaybeUninit<TaskContext>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
static mut TASK_CAP:   [MaybeUninit<CapTable>; MAX_TASKS] =
    [const { MaybeUninit::uninit() }; MAX_TASKS];
/// Per-task state stored as AtomicU8 so `wake_by_slot` (cross-core write)
/// and `block_and_reschedule` (CAS) are race-free (§8.4 lost-wakeup fix).
static TASK_STATE: [AtomicU8; MAX_TASKS] =
    [const { AtomicU8::new(TaskState::Dead as u8) }; MAX_TASKS];
static mut TASK_NAME:  [&str; MAX_TASKS]       = [""; MAX_TASKS];
static TASK_VALID: [AtomicBool; MAX_TASKS] = [const { AtomicBool::new(false) }; MAX_TASKS];
/// Which core each task is pinned to (set at enqueue time; immutable after).
static mut TASK_CORE:  [u32; MAX_TASKS]        = [0u32; MAX_TASKS];
/// Wakeup result written by `wake_by_slot`; returned by `block_and_reschedule`.
static mut TASK_WAKEUP_ERR: [i64; MAX_TASKS]  = [0i64; MAX_TASKS];
/// Last message received by each task (filled by `store_recv_message`).
static mut TASK_RECV_BUF: [Option<Message>; MAX_TASKS] =
    [const { None }; MAX_TASKS];
/// Whether each task runs in ring-3 (true) or ring-0 (false).
static mut TASK_IS_USER: [bool; MAX_TASKS] = [false; MAX_TASKS];
/// Top of each ring-3 task's kernel stack (used to set TSS.rsp0 and
/// PER_CORE_SYSCALL.kernel_rsp before every switch to that task).
/// Zero for ring-0 tasks.
static TASK_KERNEL_STACK_TOP: [AtomicU64; MAX_TASKS] =
    [const { AtomicU64::new(0) }; MAX_TASKS];

/// Per-service-NAME restart counter. The first spawn of a name yields 0; each later spawn yields the
/// previous value + 1 (saturating at u64::MAX). Bounded to `NAME_RESTART_MAX` distinct names - past
/// that a name is untracked (reads 0) and we say so loudly (§26.7). Transient per-command utility
/// services (greet, roster, ...) also land here, but `observe`/`status` only ever show persistent
/// services, whose value is the true restart count.
const NAME_RESTART_MAX: usize = 64;

struct NameRestart {
    name:  &'static str,
    count: u64,
}

struct NameRestartTable {
    entries: [NameRestart; NAME_RESTART_MAX],
    len:     usize,
}

impl NameRestartTable {
    const fn new() -> Self {
        Self {
            entries: [const { NameRestart { name: "", count: 0 } }; NAME_RESTART_MAX],
            len:     0,
        }
    }
}

static NAME_RESTART: SpinLock<NameRestartTable> = SpinLock::new(NameRestartTable::new());

/// Restart count to stamp on a (re)spawn of `name`: the value accrued by `bump_name_restart` at this
/// service's prior DEATHS. A name that never died - including every transient utility, which is never
/// bumped - reads 0. READ-ONLY: the increment happens at death and only for the restartable set, so
/// re-running a transient command (observe-*, greet, ...) is NOT a restart ("RESTARTS = it blew up,
/// not it was legitimately closed").
fn next_restart_count(name: &'static str) -> u64 {
    if name.is_empty() {
        return 0;
    }
    let tbl = NAME_RESTART.lock_irq();
    for i in 0..tbl.len {
        if tbl.entries[i].name == name {
            return tbl.entries[i].count;
        }
    }
    0
}

/// Record that restartable service `name` died (and will be respawned): bump its per-name restart
/// count. Called ONLY from the death path for the supervisor-managed/restartable set (`fs`,
/// `block-driver`, `shell`, `xhci`, `ehci`, `logger`, `supervisor`). Transient utilities the shell
/// re-invokes are never bumped, so they never show a restart. The respawn reads the new count via
/// `next_restart_count`; the first death of a name records count 1.
fn bump_name_restart(name: &'static str) {
    if name.is_empty() {
        return;
    }
    let mut tbl = NAME_RESTART.lock_irq();
    for i in 0..tbl.len {
        if tbl.entries[i].name == name {
            tbl.entries[i].count = tbl.entries[i].count.saturating_add(1);
            return;
        }
    }
    if tbl.len < NAME_RESTART_MAX {
        let len = tbl.len;
        tbl.entries[len] = NameRestart { name, count: 1 };
        tbl.len = len + 1;
    } else {
        crate::kprintln!(
            "scheduler: NAME_RESTART table full ({} names) - '{}' restart count not tracked",
            NAME_RESTART_MAX, name
        );
    }
}
/// The recv endpoint owned by each task (None if the task has no endpoint). `AtomicU64` (0 = None;
/// endpoint ids start at 100, so 0 is never a real recv endpoint) so the accessors stay unsafe-free.
static TASK_ENDPOINT: [AtomicU64; MAX_TASKS] =
    [const { AtomicU64::new(0) }; MAX_TASKS];

/// Encode/decode `Option<EndpointId>` as the `u64` held in a `TASK_ENDPOINT` slot.
#[inline] fn ep_to_u64(e: Option<EndpointId>) -> u64 { match e { Some(EndpointId(id)) => id, None => 0 } }
#[inline] fn ep_from_u64(v: u64) -> Option<EndpointId> { if v == 0 { None } else { Some(EndpointId(v)) } }

/// Timed-wake deadline (TSC cycles) for a task blocked in `recv_timeout` (§12). 0 = no
/// timed wake. Set before the task blocks; the core-0 timer ISR scans these and wakes any
/// blocked task whose deadline has passed (so a driver can wait on an interrupt yet still
/// wake on a timer for auto-repeat). Wake granularity is the timer period (coarse - fine
/// for repeat). The owning task clears its entry on every `recv_timeout` return path.
/// Each slot's `recv_timeout` wake deadline, expressed in **BSP timer ticks** (`MONOTONIC_TICKS`),
/// not TSC cycles - `0` = disarmed. Ticks, not cycles, because the deadline is set on the task's
/// core but evaluated by core 0's scan, and AMD cores need not share a TSC (a TSC deadline set on
/// core 1 can never be reached by core 0's TSC - this is exactly what wedged the first blocking
/// build: the driver never woke). `MONOTONIC_TICKS` is a single counter advanced only by the BSP,
/// so any core compares the same clock. Granularity is one quantum (~10 ms) - plenty for an I/O
/// wake. The owning task clears its entry on every `recv_timeout` return path.
static TASK_WAKE_DEADLINE: [AtomicU64; MAX_TASKS] =
    [const { AtomicU64::new(0) }; MAX_TASKS];

/// Monotonic timer-tick counter advanced once per quantum **by the BSP only** (core 0), which
/// ticks reliably even when other cores idle. The core-independent clock the timed-wake (§12)
/// measures deadlines against - see `TASK_WAKE_DEADLINE`. Distinct from `CORE_TOTAL_TICKS` (which
/// is per-core and counts yields too).
static MONOTONIC_TICKS: AtomicU64 = AtomicU64::new(0);

/// Current BSP tick count (the timed-wake clock).
pub fn monotonic_ticks() -> u64 {
    MONOTONIC_TICKS.load(Ordering::Relaxed)
}

/// Convert a `recv_timeout` duration in TSC cycles to a count of BSP ticks (≥1), for arming a
/// timed wake on the core-independent clock. Falls back to 1 tick if the quantum isn't calibrated.
pub fn cycles_to_ticks(cycles: u64) -> u64 {
    let q = crate::arch::x86_64::boot::tsc_ticks_per_quantum();
    if q == 0 { 1 } else { (cycles / q).max(1) }
}

/// Arm a timed wake for `slot` at absolute BSP-tick `deadline` (0 disarms).
pub fn set_wake_deadline(slot: usize, deadline: u64) {
    if slot < MAX_TASKS {
        TASK_WAKE_DEADLINE[slot].store(deadline, Ordering::Relaxed);
    }
}

/// Disarm any timed wake for `slot`.
pub fn clear_wake_deadline(slot: usize) {
    if slot < MAX_TASKS {
        TASK_WAKE_DEADLINE[slot].store(0, Ordering::Relaxed);
    }
}

/// Advance the BSP tick clock and wake every task whose `recv_timeout` deadline has passed.
/// Called from the **BSP** (core 0) timer ISR - the BSP ticks reliably even when other cores
/// idle, so it is the dependable driver of all timed wakes. Cheap: a scan of `MAX_TASKS` relaxed
/// loads, almost all zero. Only wakes a task still `BlockedOnRecv` (one that already returned
/// cleared its deadline; the state guard also avoids reviving one that moved on). The wake
/// result is 0 - the woken `recv_timeout` re-checks its own deadline and returns the timeout.
///
/// **BSP-driven, tick-clocked, by design (§9, §12).** Earlier attempts each failed on real SMP
/// hardware (both invisible under QEMU TCG, which serialises cores and keeps TSCs in lockstep):
/// a core-0 scan comparing a TSC deadline set on core 1 never fired (unsynchronised TSCs - the
/// driver never woke at all); a per-core scan on the task's own core fired only when that core's
/// timer did, but an idle AP's timer is unreliable on this hardware (the driver woke, but
/// rarely → seconds of input lag). The cure is to (1) measure deadlines in BSP **ticks** via a
/// single `MONOTONIC_TICKS` counter, so any core compares one clock, and (2) drive the scan from
/// the BSP, which always ticks. The wake just CASes the task `Ready`; an idle AP's busy
/// `sti`-spin idle loop re-checks `pick_next` every iteration and picks it up immediately, so no
/// cross-core IPI is needed for correctness (the IPI in `wake_by_slot` only shortens the latency).
pub fn scan_timed_wakes() {
    let now = MONOTONIC_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    for slot in 0..MAX_TASKS {
        let deadline = TASK_WAKE_DEADLINE[slot].load(Ordering::Relaxed);
        if deadline != 0
            && now >= deadline
            && TASK_STATE[slot].load(Ordering::Relaxed) == TaskState::BlockedOnRecv as u8
        {
            TASK_WAKE_DEADLINE[slot].store(0, Ordering::Relaxed);
            wake_by_slot(slot, 0);
        }
    }
}

/// Saved user-space RSP for each ring-3 task.  Updated whenever the task is
/// switched away from mid-SYSCALL so the SYSRETQ exit path sees the correct
/// per-task RSP instead of another task's value written to PER_CORE_SYSCALL.
static mut TASK_USER_RSP: [u64; MAX_TASKS] = [0u64; MAX_TASKS];

/// Pending received cap slots for each task, filled when handle_recv
/// processes a message that contained embedded capabilities (§7.6, §8.5).
const MAX_PENDING_RECV_CAPS: usize = 4;
static mut TASK_PENDING_RECV_CAPS: [[u32; MAX_PENDING_RECV_CAPS]; MAX_TASKS] =
    [[0u32; MAX_PENDING_RECV_CAPS]; MAX_TASKS];
static mut TASK_PENDING_RECV_CAP_COUNT: [usize; MAX_TASKS] = [0; MAX_TASKS];
// Delegated-resource badge of the most recently recv'd message (§7.10, file-as-capability).
// Set by handle_recv when it delivers a kernel-badged (file-cap-invoke) message; read+cleared
// by the LastRecvBadge syscall. Packed `(badge_right << 32) | badge_id`; 0 = no badge.
static TASK_LAST_BADGE: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

/// RTC wall-clock datetime (packed, `rtc::read_datetime` format) captured at each task's spawn. Per-service
/// uptime = `now_epoch_monotonic() - epoch_secs(this)` (surfaced by `task_stat`). The RTC gives PORTABLE
/// real seconds; the monotonic BSP tick counter does NOT - its rate is the timer-IRQ rate, ~100 Hz only in
/// TSC-deadline mode; on periodic-APIC HW (the T630, QEMU) it is ~10-20 Hz and not even known to the kernel,
/// so ticks->seconds would be platform-dependent. The "now" read is DEGLITCHED (`now_epoch_monotonic`) so a
/// transient CMOS misread landing on an in-range year can no longer inflate uptime to thousands of days (the
/// "4987d" bug). A restart is a fresh spawn, so uptime resets - "since the last (re)start".
static TASK_SPAWN_DT: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

// ---------------------------------------------------------------------------
// Per-task memory budget (§10.3, §22 Tests 7A/7B).
// ---------------------------------------------------------------------------

/// Base virtual address for task-requested dynamic allocations (AllocMem syscall).
/// Placed well above the ELF load region (~2 MiB), user stack (≤ 0x8000_0000),
/// and ServiceContextData page (0x3ff000) to avoid collisions.
pub const TASK_HEAP_VA_START: u64 = 0x1_0000_0000; // 4 GiB

/// Bytes dynamically allocated so far by each task (via AllocMem).
static mut TASK_ALLOC_BYTES:   [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// Maximum bytes each task may allocate (set from contract at spawn).
static mut TASK_LIMIT_BYTES:   [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// Next virtual address available for dynamic allocation in each task's space.
static mut TASK_NEXT_ALLOC_VA: [u64; MAX_TASKS] = [0u64; MAX_TASKS];
/// Timer ticks each task spent as the running task on its core.
static TASK_RUN_TICKS: [AtomicU64; MAX_TASKS] =
    [const { AtomicU64::new(0) }; MAX_TASKS];

// ---------------------------------------------------------------------------
// Per-core scheduler state.
// ---------------------------------------------------------------------------

/// Index of the task currently running on each core (IDLE if none).
static CORE_CURRENT: PerCore<AtomicUsize> = PerCore::new();
/// AtomicU64 padded to one 64-byte cache line to prevent false sharing between
/// per-core ISR hot-path writes on different cores (`lock xadd` on adjacent
/// elements would otherwise bounce the same cache line between cores).
#[repr(align(64))]
struct CachePaddedU64(AtomicU64);

/// Timer ticks each core spent running a user task (not idle).
static CORE_ACTIVE_TICKS: PerCore<CachePaddedU64> = PerCore::new();
/// Total timer ticks seen on each core.
static CORE_TOTAL_TICKS: PerCore<CachePaddedU64> = PerCore::new();
/// TSC at each core's last scheduler tick - the cross-core LIVENESS heartbeat. A core stamps this every
/// tick; every OTHER core checks it each tick and, if a core has made no progress for far longer than a
/// quantum (a stuck IF=0 loop, or a lost/never-re-armed TSC-Deadline when the Goldmont+ APIC
/// power-gates), PANICS loudly instead of letting the machine freeze silently (invariant 12 / §26.7).
/// The kernel is the last-resort recovery anchor (§6.3); if IT stalls silently nothing recovers it, so
/// it must at minimum fail LOUD. `0` = that core has not ticked yet (still booting) - not a stall.
static CORE_LAST_TICK_TSC: PerCore<CachePaddedU64> = PerCore::new();

/// Sticky round-robin scan pointer per core (§9.1, §9.3).
///
/// `pick_next` starts scanning from `CORE_RR_SLOT[cid]` rather than
/// `CORE_CURRENT[cid]+1`.  After selecting task at slot X it advances to
/// (X+1) % MAX_TASKS.  This guarantees that every slot is visited in turn
/// before the pointer wraps back to an earlier slot, preventing high-numbered
/// task slots (e.g. ping at slot 185) from being permanently starved by a
/// dense band of ready tasks at lower slots (the root cause of 8B flakiness).
static CORE_RR_SLOT: PerCore<AtomicUsize> = PerCore::new();

/// Per-core immediate-schedule hint set by `wake_by_slot`.
///
/// When a task is woken via cross-core IPC, its slot is stored here so
/// `pick_next` can schedule it on the very next call rather than waiting for
/// the round-robin pointer to naturally wrap around to it.  Without this hint,
/// a just-woken task can be starved for an entire RR cycle by a dense band of
/// Ready tasks at lower slot indices that keep the RR pointer pinned below the
/// woken slot (root cause of the BP2 hardware hang).
///
/// Sentinel: `MAX_TASKS` = no hint pending.  Written with Release by
/// `wake_by_slot` so the Ready state written before the hint is visible to
/// `pick_next`'s subsequent Acquire load of TASK_STATE.
static CORE_WAKE_HINT: PerCore<AtomicUsize> = PerCore::new();

/// Per-core queue of kernel stack tops awaiting deferred free after a self-kill.
///
/// When a task kills itself (CORE_CURRENT[core] == slot), RSP is still on that
/// task's kernel stack K_a.  Freeing K_a immediately risks a concurrent alloc +
/// crash on K_a while this core is still executing - KERNEL PF from stack
/// corruption.  Instead, only the kstack free is deferred; the slot itself is
/// released immediately (TASK_VALID=false) so reserve_task_slot can reuse it
/// without the up-to-10ms slot-starvation window the zombie approach caused.
///
/// The queue is drained by `drain_pending_kstack`, called from:
/// - `timer_tick_from_irq` (every ~10 ms, RSP is on the CURRENT live task's kstack)
/// - the scheduler `run()` loop (RSP is on the per-core BSS scheduler stack)
///
/// Safety: draining only happens when RSP is NOT on any pending kstack.
/// The self-kill path runs with IF=0 (interrupt gate / syscall context) from
/// kill_task_by_slot through yield_current's switch_context.  After switch_context
/// RSP is on a different stack; IF=1 is restored in the incoming task.  The timer
/// ISR can only fire after that point, by which time RSP is not on K_a.
const PENDING_KSTACK_CAP: usize = 8;
static CORE_PENDING_KSTACK: PerCoreMut<[u64; PENDING_KSTACK_CAP]> = PerCoreMut::new();
static CORE_PENDING_KSTACK_LEN: PerCore<AtomicUsize> = PerCore::new();

/// Per-core PML4 frame (physical address) awaiting deferred free after a self-kill.
///
/// Root cause of KERNEL PF under concurrent load (6B, 8A failures):
/// In the self-kill path the dying task's CR3 is still active on this core
/// when reclaim_user_frames hands the PML4 frame to free_frame.  Another
/// core can immediately alloc that frame (for a new PageTable::new) and
/// write_volatile-zero it.  If this core then suffers a TLB miss on a kernel
/// VA (e.g. reading a .rodata format string), the hardware page-walker reads
/// the now-zeroed PML4 → PML4[511] = 0 → "not present" → KERNEL PF.
///
/// Fix: skip freeing the PML4 frame during the reclaim loop for self-kills;
/// store it here and release it in drain_pending_pml4, which is called from
/// the scheduler loop and timer tick - both run with a different CR3.
///
/// AtomicU64 (not static mut) so reads and writes at call sites are safe.
/// The value is always written by one core and read by the same core, so
/// Relaxed/Release/Acquire ordering is sufficient.
static CORE_PENDING_PML4: PerCore<AtomicU64> = PerCore::new();

/// Per-core save area for dead task context during self-kill.
///
/// When a task kills itself and immediately releases TASK_VALID=false, a
/// concurrent spawn on another core can claim TASK_CTX[slot] before
/// yield_current's switch_context runs.  Saving the dead task's registers into
/// CORE_DEAD_CTX instead avoids the write-after-claim race.  CORE_DEAD_CTX is
/// never used as a load source; dead tasks are never resumed.
static CORE_DEAD_CTX: PerCoreMut<TaskContext> = PerCoreMut::new();

/// Saved context for each core's idle scheduler loop.
static CORE_SCHED_CTX: PerCoreMut<TaskContext> = PerCoreMut::new();

/// Initialise the owner-mutable per-core scheduler arenas (§26.6.1, `smp::percpu`). Call ONCE at boot
/// (`main`, right after `smp::percpu_init` sets the width) - before any core enters `run()`. The
/// contexts start zeroed (`run()` seeds CR3); the pending-kstack lists start empty.
pub fn init_arenas(n: usize) {
    const ZERO_CTX: TaskContext = TaskContext {
        rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rip: 0, rsp: 0, cr3: 0,
    };
    CORE_PENDING_KSTACK.init_with(n, |_| [0u64; PENDING_KSTACK_CAP]);
    CORE_DEAD_CTX.init_with(n, |_| ZERO_CTX);
    CORE_SCHED_CTX.init_with(n, |_| ZERO_CTX);
    CORE_CURRENT.init_with(n, |_| AtomicUsize::new(IDLE));
    CORE_ACTIVE_TICKS.init_with(n, |_| CachePaddedU64(AtomicU64::new(0)));
    CORE_TOTAL_TICKS.init_with(n, |_| CachePaddedU64(AtomicU64::new(0)));
    CORE_LAST_TICK_TSC.init_with(n, |_| CachePaddedU64(AtomicU64::new(0)));
    CORE_RR_SLOT.init_with(n, |_| AtomicUsize::new(0));
    CORE_WAKE_HINT.init_with(n, |_| AtomicUsize::new(MAX_TASKS));
    CORE_PENDING_KSTACK_LEN.init_with(n, |_| AtomicUsize::new(0));
    CORE_PENDING_PML4.init_with(n, |_| AtomicU64::new(0));
}


// ---------------------------------------------------------------------------
// Helper: identify the current core.
// ---------------------------------------------------------------------------

/// Read the local APIC ID register and look up the assigned core index.
///
/// Must only be called after `init_local_apic` on this core.
pub fn current_core_id() -> usize {
    // SAFETY: APIC is mapped before the scheduler ever runs on any core.
    let lapic_id = unsafe { crate::arch::x86_64::boot::get_lapic_id() };
    crate::smp::core::lapic_to_core_id(lapic_id) as usize
}

// ---------------------------------------------------------------------------
// Public API.
// ---------------------------------------------------------------------------

/// Return the calling core's ID (0-based).
pub fn current_core() -> usize { current_core_id() }

// SMP spinlock for the task-slot table - concurrent spawns on different cores
// (e.g. supervisor on Core 0 and a prop-p2 respawn on Core 3) both scan
// TASK_VALID; the scan-and-set must be atomic across cores.
static TASK_SLOT_LOCKED: AtomicBool = AtomicBool::new(false);

#[inline]
fn task_slot_lock() {
    while TASK_SLOT_LOCKED
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
}

#[inline]
fn task_slot_unlock() {
    TASK_SLOT_LOCKED.store(false, Ordering::Release);
}

/// Reserve a free task slot, pinned to `core_id`.
///
/// Marks the slot VALID (but state remains Dead) so subsequent calls to
/// `task_cap_init_empty` and `commit_task` can use it.  The slot will not
/// be scheduled until `commit_task` sets its state to Ready.
///
/// Returns `None` if all slots are occupied.
pub fn reserve_task_slot(core_id: u32) -> Option<usize> {
    // IRQ-safe: TASK_SLOT_LOCKED is also taken from the kill path (which runs in the timer ISR), so the
    // hold must mask interrupts - else a task preempted mid-reserve holds it and the pinned supervisor
    // respawn deadlocks reserving its own slot. without_interrupts is a safe smp/ call (no task/ unsafe).
    crate::smp::without_interrupts(|| {
        task_slot_lock();
        // SAFETY: lock held; exclusive access to TASK_VALID/TASK_CORE across all cores.
        let result = unsafe {
            let mut found = None;
            for i in 0..MAX_TASKS {
                if !TASK_VALID[i].load(Ordering::Relaxed) {
                    TASK_VALID[i].store(true, Ordering::Release);
                    TASK_CORE[i]  = core_id;
                    found = Some(i);
                    break;
                }
            }
            found
        };
        task_slot_unlock();
        result
    })
}

/// Initialise the CapTable for a reserved slot **in-place in BSS** and return
/// a mutable reference to it.
///
/// Using `write_bytes(0)` avoids placing a 1 536-byte `CapTable` on the
/// caller's kernel stack (which would corrupt the timer-interrupt return
/// address saved at K0T-200 - the rip=0 root cause).
///
/// # Safety
/// * `slot` must have been reserved via `reserve_task_slot`.
/// * IF=0 (syscall context).
/// * `Option<Capability>` None is represented as all-zero bytes in Rust's
///   enum layout for variants without niches; this is verified by the existing
///   diagnostic that confirmed the corruption IS a zero-write.
pub unsafe fn task_cap_init_empty(slot: usize) -> &'static mut CapTable {
    // SAFETY: slot is reserved; write_bytes zeros all Option<Capability> discriminants
    // to 0 (= None) with no intermediate kstack allocation.
    unsafe {
        core::ptr::write_bytes(
            TASK_CAP[slot].as_mut_ptr() as *mut u8,
            0,
            core::mem::size_of::<CapTable>(),
        );
        TASK_CAP[slot].assume_init_mut()
    }
}

/// Release a previously-reserved slot without committing (called on spawn error).
pub fn release_task_slot(slot: usize) {
    // IRQ-safe: see reserve_task_slot (TASK_SLOT_LOCKED is also taken in the timer ISR's kill path).
    crate::smp::without_interrupts(|| {
        task_slot_lock();
        TASK_VALID[slot].store(false, Ordering::Release);
        task_slot_unlock();
    });
}

/// Finalise a reserved task slot: write context + metadata and mark Ready.
///
/// # Safety
/// * `slot` must have been reserved and its CapTable initialised.
/// * IF=0 (syscall context).
pub unsafe fn commit_task(
    slot:             usize,
    name:             &'static str,
    ctx:              TaskContext,
    is_user:          bool,
    kernel_stack_top: u64,
    endpoint_id:      Option<EndpointId>,
) {
    // SAFETY: slot is reserved; IF=0 prevents concurrent modification.
    unsafe {
        TASK_CTX[slot].write(ctx);
        TASK_NAME[slot]             = name;
        TASK_SPAWN_DT[slot].store(crate::arch::x86_64::rtc::read_datetime(), Ordering::Relaxed);
        TASK_IS_USER[slot]          = is_user;
        TASK_KERNEL_STACK_TOP[slot].store(kernel_stack_top, Ordering::Relaxed);
        TASK_ENDPOINT[slot].store(ep_to_u64(endpoint_id), Ordering::Relaxed);
        // TASK_STATE must be last: once Ready is visible to other cores, every
        // other field in this slot must already be correctly set.  A concurrent
        // kill_task_by_slot that observes Ready will immediately read
        // TASK_IS_USER and TASK_KERNEL_STACK_TOP; if those still hold the
        // previous occupant's values the kill frees the wrong kernel stack.
        // Release ordering publishes all preceding stores before this one.
        TASK_STATE[slot].store(TaskState::Ready as u8, Ordering::Release);
    }
}

/// Add a task to the run queue, pinned to `core_id`.
///
/// Legacy single-call path kept for kernel-internal use.  New spawn code
/// should use `reserve_task_slot` + `task_cap_init_empty` + `commit_task`
/// to avoid a 1 536-byte `CapTable` on the kernel stack.
#[allow(dead_code)]
pub fn enqueue(
    name:             &'static str,
    ctx:              TaskContext,
    caps:             CapTable,
    core_id:          u32,
    is_user:          bool,
    kernel_stack_top: u64,
    endpoint_id:      Option<EndpointId>,
) {
    // SAFETY: called from BSP before any AP scheduler starts.
    unsafe {
        for i in 0..MAX_TASKS {
            if !TASK_VALID[i].load(Ordering::Relaxed) {
                TASK_CTX[i].write(ctx);
                TASK_CAP[i].write(caps);
                TASK_STATE[i].store(TaskState::Ready as u8, Ordering::Relaxed);
                TASK_NAME[i]             = name;
                TASK_SPAWN_DT[i].store(crate::arch::x86_64::rtc::read_datetime(), Ordering::Relaxed);
                TASK_VALID[i].store(true, Ordering::Release);
                TASK_CORE[i]             = core_id;
                TASK_IS_USER[i]          = is_user;
                TASK_KERNEL_STACK_TOP[i].store(kernel_stack_top, Ordering::Relaxed);
                TASK_ENDPOINT[i].store(ep_to_u64(endpoint_id), Ordering::Relaxed);
                return;
            }
        }
        panic!("scheduler: run queue full");
    }
}

// ---------------------------------------------------------------------------
// Per-task capability access - used by syscall dispatch (§8.2, §7.5).
// ---------------------------------------------------------------------------

/// Validate and return a copy of the capability at `slot` in the current
/// task's table.
pub fn current_task_lookup_cap(slot: usize, right: Rights) -> Result<Capability, CapError> {
    let cid  = current_core_id();
    // SAFETY: IF=0 in syscall context; CORE_CURRENT is stable for this core.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
            TASK_CAP[cur].assume_init_ref().get(slot, right)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

/// Return true if the current task holds a live capability on `rid` carrying
/// `right`. Used to gate the introspection syscalls (§3.1), which consume all
/// argument registers and so cannot pass a cap-slot. See
/// `docs/introspection-capability.md`.
pub fn current_task_holds_resource(
    rid: crate::capability::ResourceId,
    right: Rights,
) -> bool {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; CORE_CURRENT is stable for this core.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
            TASK_CAP[cur].assume_init_ref().holds_resource(rid, right)
        } else {
            false
        }
    }
}

/// Return the current task's own recv endpoint id, if it has one. Used by `ResourceMint`/
/// `ResourceRevoke` (§7.10) to record/check the owner of a delegated resource.
pub fn current_task_endpoint() -> Option<EndpointId> {
    let cid = current_core_id();
    let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
    if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
        ep_from_u64(TASK_ENDPOINT[cur].load(Ordering::Relaxed))
    } else {
        None
    }
}

/// Remove the capability at `slot` from the current task's table (GRANT).
pub fn current_task_remove_cap(slot: usize) -> Option<Capability> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
            TASK_CAP[cur].assume_init_mut().remove(slot)
        } else {
            None
        }
    }
}

/// Read the rights of the cap at `slot` without validating the generation.
///
/// Used by `QueryCapRights` (syscall 14) - read-only, no side effects.
pub fn current_task_read_cap_rights(slot: usize) -> Option<Rights> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
            crate::capability::cap_read_rights(TASK_CAP[cur].assume_init_ref(), slot)
        } else {
            None
        }
    }
}

/// Call `f` for every capability held by every currently-valid task.
///
/// Used by `invariants::assertions::assert_cap_table_consistent` (§7.8) to
/// verify generation consistency across all task cap tables. Must not be called
/// from a spawn or kill path while TASK_SLOT_LOCKED is held by this core.
pub fn for_each_active_cap<F: FnMut(&Capability)>(mut f: F) {
    // Delegates to `for_each_cap_of` (which holds the single SAFETY block) so the
    // task/ unsafe count does not grow. Best-effort snapshot: a concurrent
    // spawn/kill may be seen inconsistently for one iteration, acceptable for
    // invariant-assertion use only.
    for slot in 0..MAX_TASKS {
        for_each_cap_of(slot, &mut f);
    }
}

/// Iterate the held capabilities of the task in `slot` (any task, not just the
/// current one). Best-effort read-only snapshot for introspection (the `caps`
/// command via `TaskCaps`): a concurrent spawn/kill may be seen inconsistently,
/// which is acceptable for display. Same posture as `for_each_active_cap` and
/// `task_stat`. No-op for an empty/invalid slot.
pub fn for_each_cap_of<F: FnMut(&Capability)>(slot: usize, mut f: F) {
    if slot >= MAX_TASKS { return; }
    if !TASK_VALID[slot].load(Ordering::Acquire) { return; }
    // SAFETY: TASK_VALID[slot] observed true (Acquire) guarantees the CapTable was
    // fully initialised before the Release store that set valid=true.
    unsafe { TASK_CAP[slot].assume_init_ref() }.for_each_slot(&mut f);
}

/// Push a cap slot into the current task's pending-received-caps buffer.
///
/// Called by handle_recv when it installs an embedded cap into the receiver's
/// table. The slot is retrieved by the service via syscall 12 (TakePendingCap).
/// Record the delegated-resource badge of a just-delivered message for the current (receiving)
/// task (§7.10). `badge_id == 0` clears it (an ordinary, unbadged message). Packed so the
/// `LastRecvBadge` syscall returns it in one register.
pub fn set_last_recv_badge(badge_id: u64, badge_right: u8) {
    let cid = current_core_id();
    let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
    if cur < MAX_TASKS {
        let v = if badge_id == 0 { 0 } else { ((badge_right as u64) << 32) | badge_id };
        TASK_LAST_BADGE[cur].store(v, Ordering::Relaxed);
    }
}

/// Take (and clear) the current task's last-recv badge. Returns the packed value (0 = none).
pub fn take_last_recv_badge() -> u64 {
    let cid = current_core_id();
    let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
    if cur < MAX_TASKS {
        TASK_LAST_BADGE[cur].swap(0, Ordering::Relaxed)
    } else {
        0
    }
}

pub fn push_pending_recv_cap(cap_slot: u32) {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS {
            let count = TASK_PENDING_RECV_CAP_COUNT[cur];
            if count < MAX_PENDING_RECV_CAPS {
                TASK_PENDING_RECV_CAPS[cur][count] = cap_slot;
                TASK_PENDING_RECV_CAP_COUNT[cur]   = count + 1;
            }
        }
    }
}

/// Pop the next pending cap slot from the current task's buffer.
/// Returns `None` if no pending caps remain.
pub fn pop_pending_recv_cap() -> Option<u32> {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS {
            let count = TASK_PENDING_RECV_CAP_COUNT[cur];
            if count > 0 {
                let slot = TASK_PENDING_RECV_CAPS[cur][0];
                // Shift remaining entries left.
                for i in 0..count - 1 {
                    TASK_PENDING_RECV_CAPS[cur][i] = TASK_PENDING_RECV_CAPS[cur][i + 1];
                }
                TASK_PENDING_RECV_CAP_COUNT[cur] = count - 1;
                return Some(slot);
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Memory budget API (§10.3, §22 Tests 7A/7B).
// ---------------------------------------------------------------------------

/// Set the memory budget for `slot` at spawn time.
///
/// Seeds alloc_bytes to the task's base footprint (`base_bytes` = mapped binary + user stack + ctx)
/// so MEM_USED reflects real occupancy, not just dynamic `alloc_mem` (which most no-heap services
/// never call). `alloc_mem` then adds on top, all bounded by `limit`. Also seeds the first heap VA.
pub fn set_task_memory_budget(slot: usize, limit: u64, base_bytes: u64) {
    if slot >= MAX_TASKS { return; }
    // SAFETY: called from spawn path with IF=0; single writer for this slot.
    unsafe {
        TASK_ALLOC_BYTES[slot]   = base_bytes;
        TASK_LIMIT_BYTES[slot]   = limit;
        TASK_NEXT_ALLOC_VA[slot] = TASK_HEAP_VA_START;
    }
}

/// Reserve `size` bytes from the current task's memory budget.
///
/// Returns the virtual address at which the caller should map the new pages,
/// or `None` if the allocation would exceed the task's limit (AllocDenied).
///
/// `size` is rounded up to a 4 KiB page boundary before the budget check so
/// that the VA region is always page-aligned.
pub fn current_task_claim_alloc(size: u64) -> Option<u64> {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core writer.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur >= MAX_TASKS || !TASK_VALID[cur].load(Ordering::Relaxed) { return None; }

        // Overflow guard: (size + 4095) wraps for very large values (e.g. u64::MAX).
        // saturating to u64::MAX guarantees the budget check rejects the request.
        let aligned = size.checked_add(4095).map(|v| v & !4095).unwrap_or(u64::MAX);
        let already = TASK_ALLOC_BYTES[cur];
        let limit   = TASK_LIMIT_BYTES[cur];
        if already.saturating_add(aligned) > limit { return None; }

        let va = TASK_NEXT_ALLOC_VA[cur];
        TASK_ALLOC_BYTES[cur]   = already.saturating_add(aligned);
        TASK_NEXT_ALLOC_VA[cur] = va.saturating_add(aligned);
        Some(va)
    }
}

/// Return the bytes dynamically allocated so far by the current task.
///
/// Used by InspectKernel query 0 (P4 property test - §10.3).
pub fn current_task_alloc_bytes() -> u64 {
    let cid = current_core_id();
    // SAFETY: IF=0 in syscall context; single core reader for this slot.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur >= MAX_TASKS || !TASK_VALID[cur].load(Ordering::Relaxed) { return 0; }
        TASK_ALLOC_BYTES[cur]
    }
}

/// Raw task stat snapshot for a given slot. Used by TaskStat syscall (16).
pub struct TaskStatRaw {
    pub valid:       bool,
    pub state:       u8,
    pub core:        u32,
    pub mem_used:    u64,
    pub mem_limit:   u64,
    pub name:        &'static str,
    pub restart_count: u64,
    pub queue_depth: u8,
    pub run_ticks:   u64,
    pub uptime_secs: u64,
}

/// Return a best-effort snapshot of task state at `slot`.
///
/// Called from the TaskStat syscall (16) handler. Consistent with the same
/// best-effort snapshot contract used by `for_each_active_cap`.
pub fn task_stat(slot: usize) -> TaskStatRaw {
    if slot >= MAX_TASKS {
        return TaskStatRaw { valid: false, state: 0, core: 0, mem_used: 0, mem_limit: 0,
                             name: "", restart_count: 0, queue_depth: 0, run_ticks: 0, uptime_secs: 0 };
    }
    // SAFETY: read-only snapshot of static arrays; all reads are individually
    // naturally-atomic on x86_64 (u64/u32/pointer-width). Best-effort consistency
    // is acceptable - same contract as for_each_active_cap.
    unsafe {
        let endpoint = ep_from_u64(TASK_ENDPOINT[slot].load(Ordering::Relaxed));
        let queue_depth = match endpoint {
            Some(ep) => routing::endpoint_queue_depth(ep),
            None     => 0,
        };
        TaskStatRaw {
            valid:       TASK_VALID[slot].load(Ordering::Relaxed),
            state:       TASK_STATE[slot].load(Ordering::Acquire),
            core:        TASK_CORE[slot],
            mem_used:    TASK_ALLOC_BYTES[slot],
            mem_limit:   TASK_LIMIT_BYTES[slot],
            name:        TASK_NAME[slot],
            // DERIVED, not stored (Commandment III): the restart lineage's one truth is NAME_RESTART
            // (the per-name counter). A live instance's count IS its name's count - so we recompute it
            // here rather than cache a per-slot copy that could drift from the source.
            restart_count: next_restart_count(TASK_NAME[slot]),
            queue_depth,
            run_ticks:   TASK_RUN_TICKS[slot].load(Ordering::Relaxed),
            uptime_secs: {
                // RTC epoch delta from the spawn stamp to a DEGLITCHED now. now_epoch_monotonic rejects a
                // backwards step or an implausible (>1 day) forward jump, so a transient CMOS misread of
                // "now" can no longer inflate this to thousands of days (the "4987d" bug). Two further
                // clamps: .max(0) floors a backwards delta, and the result is capped at the system uptime
                // (a service is never older than the system). 0 if never stamped (empty slot).
                let spawn = TASK_SPAWN_DT[slot].load(Ordering::Relaxed);
                if spawn == 0 { 0 } else {
                    let now  = crate::arch::x86_64::rtc::now_epoch_monotonic();
                    let svc  = (now - crate::arch::x86_64::rtc::epoch_secs(spawn)).max(0);
                    let boot = crate::arch::x86_64::rtc::boot_datetime();
                    let sys  = if boot == 0 { svc } else { (now - crate::arch::x86_64::rtc::epoch_secs(boot)).max(0) };
                    svc.min(sys) as u64
                }
            },
        }
    }
}

/// Total timer ticks the given core spent running a user task (not idle).
pub fn core_active_ticks(core: usize) -> u64 {
    if core >= num_cores() { return 0; }
    CORE_ACTIVE_TICKS.get(core).0.load(Ordering::Relaxed)
}

/// Total timer ticks seen on the given core since boot.
pub fn core_total_ticks(core: usize) -> u64 {
    if core >= num_cores() { return 0; }
    CORE_TOTAL_TICKS.get(core).0.load(Ordering::Relaxed)
}

/// Insert a capability into the current task's table (incoming GRANT).
pub fn current_task_insert_cap(cap: Capability) -> Result<usize, CapError> {
    let cid = current_core_id();
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS && TASK_VALID[cur].load(Ordering::Relaxed) {
            TASK_CAP[cur].assume_init_mut().insert(cap)
        } else {
            Err(CapError::CapNotHeld)
        }
    }
}

// ---------------------------------------------------------------------------
// Ring-3 switch preparation.
// ---------------------------------------------------------------------------

/// Update per-core SYSCALL data and TSS.rsp0 before switching to a ring-3 task.
///
/// Must be called (with IF=0) immediately before every `switch_context` whose
/// incoming task is a ring-3 task.  This ensures:
///   • `PER_CORE_SYSCALL[cid].kernel_rsp` → correct kernel stack for SYSCALL.
///   • `TSS[cid].rsp0`                   → correct kernel stack for hardware
///                                          interrupts that hit ring-3 code.
///
/// # Safety
/// IF must be 0.  `slot` must be a valid ring-3 task slot.
unsafe fn prepare_ring3_switch(core_id: usize, slot: usize) {
    // §9.1 (static placement): `pick_next` only ever returns a slot whose
    // TASK_CORE equals the running core, so a task is never resumed on a core it
    // is not pinned to. Assert it as the executable form of the invariant - a
    // mismatch here means the scheduler attempted a forbidden mid-execution
    // migration (§3.11), which is a kernel logic bug, not a recoverable state.
    crate::invariants::assertions::assert_no_mid_execution_migration(
        TASK_CORE[slot], core_id as u32,
    );

    let ksp = TASK_KERNEL_STACK_TOP[slot].load(Ordering::Relaxed);

    // The syscall stack must start well below K0T, NOT at K0T (Bug 2).
    //
    // Both the timer ISR (via TSS.rsp0 → K0T) and the syscall path enter at the
    // top of the kstack; if the syscall chain runs there too, the timer ISR's
    // context-switch path - which descends much deeper than once assumed: canary
    // measurement showed it zero-writing down to ~K0T-504 - clobbers a suspended
    // recv syscall's return address, causing the intermittent rip→kstack #PF.
    //
    // `ud2_syscall_entry` switches RSP to this value before calling the handler,
    // so the whole syscall chain lives below it. K0T-2048 leaves ~1.5 KiB of guard
    // over the measured timer reach (K0T-504); the deepest syscall (~4.6 KiB for a
    // 4 KiB Message) still fits comfortably in the 64 KiB kstack below K0T-2048.
    let syscall_rsp = ksp - 2048;

    // SAFETY: syscall_slot(core_id) points at this core's arena slot (or the BSP bootstrap pre-arena);
    // single writer (this core), per the per-core single-owner invariant.
    unsafe {
        (*crate::arch::x86_64::syscall_entry::syscall_slot(core_id)).kernel_rsp = syscall_rsp;
        // Restore per-task user RSP so SYSRETQ loads the correct stack pointer for
        // this task, not the value left by the last task that ran on this core.
        (*crate::arch::x86_64::syscall_entry::syscall_slot(core_id)).user_rsp =
            TASK_USER_RSP[slot];
    }
    // TSS.rsp0 stays at K0T so hardware interrupts (timer ISR) still enter at the top.
    // SAFETY: set_tss_rsp0 writes only to TSS_PER_CORE[core_id].rsp0.
    unsafe { crate::arch::x86_64::boot::set_tss_rsp0(core_id, ksp) };
}

/// Enter the scheduler loop on the calling core. Never returns.
pub fn run(core_id: u32) -> ! {
    let cid = core_id as usize;

    // Seed the scheduler context's CR3 with the current value.  `switch_context`
    // never saves CR3 (only loads it), so without this, switching back to the
    // scheduler context after `block_and_reschedule` would load CR3=0 and
    // triple-fault on the next instruction fetch.
    // SAFETY: run() is called once per core before any task switch; CORE_SCHED_CTX[cid] exclusively accessible.
    unsafe {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nostack, nomem));
        (*CORE_SCHED_CTX.as_mut_ptr(cid)).cr3 = cr3;
    }

    // Arm the TSC-Deadline timer now that cr3 is seeded.  The arm was deferred
    // from init_local_apic to here so that any timer ISR firing after this point
    // will find a valid CORE_SCHED_CTX[cid].cr3 and can safely call pick_next
    // → switch_context without loading a garbage page table.
    if crate::arch::x86_64::boot::TSC_DEADLINE_MODE.load(Ordering::Relaxed) {
        // SAFETY: ring-0; TSC-Deadline was confirmed in init_local_apic
        // (TSC_DEADLINE_MODE=true implies CPUID check passed and
        // TSC_TICKS_PER_QUANTUM > 0); cr3 seeded above.
        unsafe { crate::arch::x86_64::boot::rearm_tsc_deadline() };
    }

    loop {
        // Free any deferred kstack from a prior self-kill on this core.
        // RSP is on CORE_SCHED_CTX's stack (per-core BSS), not any kstack.
        drain_pending_kstack(cid);

        // Path C / Phase 6: respawn the supervisor here if it died - an IF=1 scheduling point, NOT the
        // timer ISR. `spawn_supervisor` is a full ~22 ms service spawn that issues all-core TLB
        // shootdowns; running it from the Core-0 timer ISR (IF=0, where it used to live, via
        // process_pending) wedged the box under a storm: Core 0 could not ACK other cores' shootdown /
        // WAKE_RECEIVER IPIs while stuck in the IF=0 spawn - the exact hazard process_pending's own
        // comment warns about (it bounds the COM2 drain for the same reason). Here CORE_CURRENT is IDLE
        // and interrupts are enabled (the prior iteration's `sti`/`wait_for_interrupt`), so the spawn
        // participates in cross-core IPI protocols normally. Runs every scheduler iteration on Core 0,
        // so it is prompt even under load (the idle branch a busy core never reaches). One relaxed
        // atomic load when the supervisor is healthy.
        if cid == 0 {
            crate::task::poll_supervisor_respawn();
        }

        // Compiler barrier: force a reload of all scheduler statics (TASK_STATE,
        // TASK_VALID, TASK_CORE) on every iteration.  Without this, the compiler
        // is free to cache the pick_next result across the `sti; hlt` boundary
        // because it sees no intervening writes in this compilation unit.
        // The IPI handler on a remote core writes TASK_STATE, which the
        // compiler cannot observe without this barrier.
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);

        match pick_next(cid) {
            Some(next) => {
                // SAFETY: IF disabled around the switch to prevent preemption.
                unsafe {
                    core::arch::asm!("cli", options(nostack, nomem));
                    // V3 (kernel-audit-2): claim `next` with a CAS, not an
                    // unconditional store, so a cross-core kill that stored Dead
                    // between pick_next and here is not overwritten (mirrors the
                    // next==prev timer path). Then complete the Dekker handshake
                    // with kill_task_by_slot, which does: store Dead; SeqCst fence;
                    // load CORE_CURRENT. Here we publish CORE_CURRENT, fence, and
                    // re-read STATE: if a concurrent kill won the pick->publish race
                    // - and may have missed our publish and already freed `next`'s
                    // page tables - `abort` skips the switch rather than load a freed
                    // CR3. All accesses are SeqCst so the two sides cannot both miss
                    // each other's store (store-buffer / Dekker), so at least one of
                    // {kill waits for us, we observe Dead} holds; both are safe.
                    let mut abort = TASK_STATE[next]
                        .compare_exchange(
                            TaskState::Ready as u8,
                            TaskState::Running as u8,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_err();
                    if !abort {
                        CORE_CURRENT.get(cid).store(next, Ordering::SeqCst);
                        core::sync::atomic::fence(Ordering::SeqCst);
                        abort = TASK_STATE[next].load(Ordering::SeqCst)
                            != TaskState::Running as u8;
                    }
                    if !abort {
                        if TASK_IS_USER[next] {
                            prepare_ring3_switch(cid, next);
                        }
                        let sched    = CORE_SCHED_CTX.as_mut_ptr(cid);
                        let next_ctx = TASK_CTX[next].assume_init_ref() as *const TaskContext;
                        switch_context(sched, next_ctx);
                        // Execution returns here after the task is preempted and
                        // the scheduler loop is re-entered.
                    }
                    // Shared tail (switched, or aborted): leave CORE_CURRENT off
                    // `next` so a racing kill's spin-wait proceeds, then re-loop.
                    CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
                    core::arch::asm!("sti", options(nostack, nomem));
                }
            }
            None => {
                // Core 0 drains the COM2 control channel when idle (§17).
                if cid == 0 {
                    crate::control::process_pending();
                }
                // No ready tasks; re-enable interrupts and loop.
                // `wait_for_interrupt` issues only `sti` - no PAUSE, no HLT.
                // On Goldmont+, both are "low-power hints" that allow firmware
                // C-state promotion, power-gating the LAPIC and dropping timer
                // ticks and IPIs.  The compiler_fence above forces a fresh reload
                // of TASK_STATE on every iteration so wakeups from other cores
                // are always visible.
                crate::arch::x86_64::wait_for_interrupt();
            }
        }
    }
}

/// # Safety
/// Must only be called from the timer ISR with interrupts disabled (IF=0).
/// The interrupted RIP/CS/RSP are passed from `timer_isr_stub` via rdi/rsi/rdx;
/// they are unused now that the ring-3 bring-up diagnostics are removed.
#[no_mangle]
pub extern "C" fn timer_tick_from_irq(_interrupted_rip: u64, _interrupted_cs: u64, _interrupted_rsp: u64) {
    let cid = current_core_id();
    // Free any deferred kstack from a prior self-kill on this core.
    // RSP is now on the current task's kstack, not the dead task's, so
    // freeing the pending kstack is safe.  IF=0 (interrupt gate).
    drain_pending_kstack(cid);
    // SAFETY: IF=0 throughout (interrupt gate clears IF on entry).
    unsafe {
        crate::arch::x86_64::boot::apic_send_eoi();

        // TSC-Deadline mode is one-shot: re-arm immediately after EOI.
        // In periodic mode this is a no-op (TSC_DEADLINE_MODE stays false).
        // SAFETY: IF=0; ring-0; TSC_DEADLINE_MODE=true implies TSC-Deadline
        // was verified at init_local_apic time and TSC_TICKS_PER_QUANTUM > 0.
        if crate::arch::x86_64::boot::TSC_DEADLINE_MODE.load(Ordering::Relaxed) {
            crate::arch::x86_64::boot::rearm_tsc_deadline();
        }

        // Poll the COM2 control channel and COM1 UART RX on every core-0 timer
        // tick (§17).  The idle branch can't be relied on when core 0 always
        // has ready tasks.  COM1 polling replaces IRQ 4 (fully masked by PIC).
        if cid == 0 {
            crate::control::process_pending();
            crate::arch::x86_64::uart_rx_poll();
            // Advance the BSP tick clock + wake any task whose recv_timeout deadline elapsed
            // (§12 timed-wait). BSP-driven because the BSP ticks reliably while APs idle; the
            // wake is tick-clocked so it's valid cross-core. See scan_timed_wakes.
            scan_timed_wakes();
        }

        let prev = CORE_CURRENT.get(cid).load(Ordering::Relaxed);

        // Accumulate CPU utilisation counters.
        CORE_TOTAL_TICKS.get(cid).0.fetch_add(1, Ordering::Relaxed);

        // LIVENESS WATCHDOG (invariant 12 / §26.7 - the constitutionally-complete answer to the silent
        // Wyse wedge). Stamp this core's progress, then check every OTHER core: a core that has made no
        // forward progress for far longer than a quantum has STOPPED - for ANY reason (a stuck IF=0
        // loop, a lost/never-re-armed TSC-Deadline when the Goldmont+ APIC power-gates, a wait nothing
        // wakes). Rather than let the machine freeze silently, the first core to notice PANICS LOUDLY,
        // naming the stalled core, how long it has been dark, and what it was running. The kernel is the
        // last-resort recovery anchor (§6.3): if it stalls silently nothing recovers it, so it must at
        // minimum fail loud. Armed only once the TSC quantum is calibrated (real hardware, TSC-Deadline
        // mode); QEMU's periodic tick has no calibrated rate (0), so it is skipped - and a normal
        // shootdown/critical-section is milliseconds, so the ~3 s deadline cannot false-fire.
        let now = crate::arch::x86_64::read_cycle_counter();
        CORE_LAST_TICK_TSC.get(cid).0.store(now, Ordering::Relaxed);
        let tpq = crate::arch::x86_64::boot::tsc_ticks_per_quantum();
        let ncores = crate::smp::core::ready_count() as usize;
        if tpq > 0 {
            let deadline = tpq.saturating_mul(300); // ~3 s (300 * 10 ms quanta) with no forward progress
            for other in 0..ncores {
                if other == cid { continue; }
                let last = CORE_LAST_TICK_TSC.get(other).0.load(Ordering::Relaxed);
                // saturating_sub: if now < last (small cross-core TSC skew, or it stamped a hair ago)
                // this reads 0, never a spurious huge delta - so skew cannot false-fire the panic.
                let dark = now.saturating_sub(last);
                if last != 0 && dark > deadline {
                    let stuck_task = CORE_CURRENT.get(other).load(Ordering::Relaxed);
                    panic!(
                        "LIVENESS WEDGE: core {} made NO progress for {} TSC (~{} quanta, >3s); last \
                         running task slot {}; detected by core {}. No forward progress = loud stop.",
                        other, dark, dark / tpq, stuck_task, cid
                    );
                }
            }
        }
        // (The per-core "hb:" tick heartbeat that lived here was a diagnostic for the Wyse wedge hunt;
        // removed now that the wedge is fixed and the cross-core LIVENESS WATCHDOG above is the permanent
        // progress defense. It printed every ~2 s forever - idle console spam with no remaining purpose.)
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
            CORE_ACTIVE_TICKS.get(cid).0.fetch_add(1, Ordering::Relaxed);
            // Credit the running task this quantum - the per-task CPU% source for `observe`
            // (a task's run_ticks delta as a share of its core's total-tick delta). Idle quanta
            // aren't credited (prev fails the valid check), so a blocked service reads ~0%.
            TASK_RUN_TICKS[prev].fetch_add(1, Ordering::Relaxed);
        }

        // Path C / Phase 6: the supervisor respawn is NO LONGER pinned here. The old code returned
        // (suppressing the switch) so the ~22 ms spawn ran uninterrupted - but that STARVED any Core-0
        // task holding a lock the spawn needed, deadlocking it under load (§22 Test 15). The respawn is
        // now ROUND-ROBINED instead: see the in-progress RESUME block just after the CAS below, plus the
        // preempt via the normal switch when prev == IDLE (the spawn currently running). Its locks are
        // IRQ-safe, so the spawn is only ever preempted BETWEEN holds; a lock-holder gets a quantum and
        // releases, then the spawn resumes and acquires.

        // CAS instead of store: if a cross-core kill wrote Dead between our
        // load and this transition, the CAS fails and Dead is preserved.
        // An unconditional store(Ready) would silently overwrite Dead, causing
        // kill_task_by_slot's CORE_CURRENT spin-wait to never exit.
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
            let _ = TASK_STATE[prev].compare_exchange(
                TaskState::Running as u8,
                TaskState::Ready as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }

        // Path C / Phase 6 (round-robin respawn): if a supervisor respawn is IN PROGRESS and a TASK is
        // running here, switch it OUT to the scheduler context (CORE_SCHED_CTX) - where the preempted
        // spawn was saved - so the spawn gets THIS quantum and RESUMES. `prev` was set Ready by the CAS
        // above, so it is rescheduled. Combined with the preempt path (when prev == IDLE the spawn is
        // running, and the normal switch below saves it into CORE_SCHED_CTX and runs a ready task), this
        // round-robins the spawn with ready tasks: a lock-holder runs and releases, the spawn resumes and
        // acquires - no pin, no deadlock, no strand. Mirrors the pending switch-to-scheduler-context below.
        if cid == 0 && crate::task::supervisor_respawn_in_progress() && prev < MAX_TASKS {
            // Capture prev's user RSP now (other tasks run before prev is rescheduled, overwriting it).
            if TASK_VALID[prev].load(Ordering::Relaxed) && TASK_IS_USER[prev] {
                TASK_USER_RSP[prev] =
                    (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
            }
            let current_ctx: *mut TaskContext = if !TASK_VALID[prev].load(Ordering::Relaxed) {
                // prev self-killed - discard into CORE_DEAD_CTX (never resumed).
                CORE_DEAD_CTX.as_mut_ptr(cid)
            } else {
                TASK_CTX[prev].assume_init_mut() as *mut TaskContext
            };
            CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
            let sched_ctx: *const TaskContext = CORE_SCHED_CTX.as_ptr(cid);
            switch_context(current_ctx, sched_ctx);
            // Resumes here when prev is next scheduled (after the spawn yields its quantum back).
            return;
        }

        let next = match pick_next(cid) {
            Some(i) => i,
            None => {
                // If the current task was killed (state=Dead) and no other task
                // is runnable, switch to the scheduler idle context so that
                // kill_task_by_slot's spin-wait on CORE_CURRENT sees us leave.
                // Without this, the ISR returns to the dead task, CORE_CURRENT
                // never changes, and the spin-wait deadlocks.
                // Check STATE alone (not TASK_VALID): with the deferred-kstack
                // approach TASK_VALID is false immediately after self-kill, but
                // STATE=Dead is still the signal we need.
                let is_dead = prev < MAX_TASKS
                    && TASK_STATE[prev].load(Ordering::Relaxed) == TaskState::Dead as u8;
                if is_dead {
                    CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
                    // Save into CORE_DEAD_CTX - not TASK_CTX[prev] - to avoid a
                    // write-after-claim race if a concurrent spawn has already
                    // reserved TASK_CTX[prev] (possible now that TASK_VALID=false
                    // immediately).  CORE_DEAD_CTX is never used as a load source.
                    let dead_ctx = CORE_DEAD_CTX.as_mut_ptr(cid);
                    let sched_ctx = CORE_SCHED_CTX.as_ptr(cid);
                    switch_context(dead_ctx, sched_ctx);
                    // Unreachable: dead tasks are never rescheduled.
                } else {
                    // Restore Running only if we changed the state to Ready above.
                    // CAS fails if state is Blocked* or Dead, leaving it untouched.
                    if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
                        TASK_STATE[prev]
                            .compare_exchange(
                                TaskState::Ready as u8,
                                TaskState::Running as u8,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .ok();
                    }
                }
                return;
            }
        };

        // Path C / Phase 6: if a supervisor respawn is pending, land Core 0 in the SCHEDULER context
        // (run()'s loop) instead of switching to `next`, so the heavy ~22 ms spawn runs from run()'s
        // loop top with IF=1 - NEVER here in the IF=0 ISR, where it can't ACK other cores' TLB-shootdown
        // / WAKE_RECEIVER IPIs and wedges the box under a storm (the architectural proof; see
        // scheduler::run + control::process_pending). `next` is left untouched (still Ready), so
        // pick_next re-picks it after the respawn. `prev` was set Ready by the CAS above, so it too is
        // rescheduled. This mirrors the is_dead switch above, but saves `prev` (it is live) so it resumes.
        if cid == 0 && crate::task::supervisor_respawn_pending() {
            if prev >= MAX_TASKS {
                // Already in the scheduler context - just return; run()'s loop top will respawn.
                return;
            }
            // Save prev's user RSP (mirror the normal switch below): other tasks run before prev is
            // rescheduled, overwriting PER_CORE_SYSCALL.user_rsp, so capture it now.
            if TASK_VALID[prev].load(Ordering::Relaxed) && TASK_IS_USER[prev] {
                TASK_USER_RSP[prev] =
                    (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
            }
            let current_ctx: *mut TaskContext = if !TASK_VALID[prev].load(Ordering::Relaxed) {
                // prev self-killed (e.g. the supervisor itself) - discard into CORE_DEAD_CTX.
                CORE_DEAD_CTX.as_mut_ptr(cid)
            } else {
                TASK_CTX[prev].assume_init_mut() as *mut TaskContext
            };
            CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
            let sched_ctx: *const TaskContext = CORE_SCHED_CTX.as_ptr(cid);
            switch_context(current_ctx, sched_ctx);
            // Resumes here when prev is next scheduled (after run()'s loop top did the respawn).
            return;
        }

        if next == prev {
            // CAS: if kill wrote Dead between pick_next and here, preserve Dead.
            // On CAS failure the next timer tick's is_dead branch will switch
            // CORE_CURRENT to IDLE and release the kill spin-wait.
            let _ = TASK_STATE[prev].compare_exchange(
                TaskState::Ready as u8,
                TaskState::Running as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            return;
        }

        // V3 (kernel-audit-2): claim `next` with a CAS (mirror the next==prev path
        // above) so a cross-core kill's Dead is not overwritten; then complete the
        // Dekker handshake with kill_task_by_slot (store Dead; SeqCst fence; load
        // CORE_CURRENT) by publishing CORE_CURRENT, fencing, and re-reading STATE.
        // If a kill won the pick->publish race it may have already freed `next`'s
        // page tables, so `abort_to_sched` re-routes prev to the scheduler context
        // rather than let switch_context load a freed CR3. All accesses SeqCst so
        // the two sides cannot both miss each other's store.
        let mut abort_to_sched = TASK_STATE[next]
            .compare_exchange(
                TaskState::Ready as u8,
                TaskState::Running as u8,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err();
        if !abort_to_sched {
            CORE_CURRENT.get(cid).store(next, Ordering::SeqCst);
            core::sync::atomic::fence(Ordering::SeqCst);
            abort_to_sched =
                TASK_STATE[next].load(Ordering::SeqCst) != TaskState::Running as u8;
        }
        if abort_to_sched {
            // Leave CORE_CURRENT off `next` so the racing kill's spin-wait proceeds.
            CORE_CURRENT.get(cid).store(IDLE, Ordering::SeqCst);
            if prev >= MAX_TASKS {
                // Core was idle (in run()'s loop) - nothing to switch out; return to it.
                return;
            }
            // else: fall through and switch `prev` (set Ready by the CAS at the top
            // of the tick) to the scheduler context via the shared switch below.
        }

        // Save BEFORE prepare_ring3_switch so we capture the value from the last
        // SYSCALL entry for `prev`, not the value prepare_ring3_switch writes for `next`.
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) && TASK_IS_USER[prev] {
            TASK_USER_RSP[prev] =
                (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
        }

        // On abort we do not enter `next`; only prepare the ring-3 switch when we
        // are actually switching to it.
        if !abort_to_sched && TASK_IS_USER[next] {
            prepare_ring3_switch(cid, next);
        }

        let current_ctx: *mut TaskContext = if prev >= MAX_TASKS {
            CORE_SCHED_CTX.as_mut_ptr(cid)
        } else if !TASK_VALID[prev].load(Ordering::Relaxed) {
            // Slot was immediately released by a self-kill (deferred-kstack
            // approach).  Save into CORE_DEAD_CTX to avoid a write-after-claim
            // race with a concurrent spawn.  CORE_DEAD_CTX is never resumed.
            CORE_DEAD_CTX.as_mut_ptr(cid)
        } else {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        };

        // Normal path: switch into `next`. Abort path (kill won the race): switch
        // `prev` to the scheduler context instead, where it is rescheduled - never
        // loading `next`'s possibly-freed page table. (prev >= MAX_TASKS already
        // returned above, so current_ctx here is a real task ctx, distinct from the
        // scheduler ctx - never the same-pointer switch the contract forbids.)
        let next_ctx: *const TaskContext = if abort_to_sched {
            CORE_SCHED_CTX.as_ptr(cid)
        } else {
            TASK_CTX[next].assume_init_ref() as *const TaskContext
        };

        switch_context(current_ctx, next_ctx);
    }
}

/// Advisory yield: mark the current task Ready and reschedule.
/// Also called from the IPI handler for cross-core WAKE_RECEIVER (§9.4).
pub fn yield_current() {
    crate::arch::x86_64::disable_interrupts();

    let cid = current_core_id();

    // SAFETY: IF=0.
    unsafe {
        let prev = CORE_CURRENT.get(cid).load(Ordering::Relaxed);

        // Count each scheduler quantum (yield or timer) for CPU utilisation.
        CORE_TOTAL_TICKS.get(cid).0.fetch_add(1, Ordering::Relaxed);
        // A yield IS forward progress - stamp the liveness heartbeat too, so the cross-core watchdog
        // (timer_tick_from_irq) never mistakes a heavily-yielding core (one making progress via syscalls
        // rather than timer preemption) for a stalled one.
        CORE_LAST_TICK_TSC.get(cid).0.store(crate::arch::x86_64::read_cycle_counter(), Ordering::Relaxed);
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
            CORE_ACTIVE_TICKS.get(cid).0.fetch_add(1, Ordering::Relaxed);
            // Credit the running task this quantum - the per-task CPU% source for `observe`
            // (a task's run_ticks delta as a share of its core's total-tick delta). Idle quanta
            // aren't credited (prev fails the valid check), so a blocked service reads ~0%.
            TASK_RUN_TICKS[prev].fetch_add(1, Ordering::Relaxed);
        }

        // CAS: preserve Dead if a cross-core kill races with this transition.
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
            let _ = TASK_STATE[prev].compare_exchange(
                TaskState::Running as u8,
                TaskState::Ready as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }

        let next = match pick_next(cid) {
            Some(i) => i,
            None => {
                // Same Dead-detection as timer_tick_from_irq: if the current task
                // was killed with no other runnable task, switch to the scheduler
                // so kill_task_by_slot's spin-wait can exit.
                // Check STATE alone (not TASK_VALID): TASK_VALID is immediately
                // false after a self-kill (deferred-kstack approach), but
                // STATE=Dead is still the signal we need.
                let is_dead = prev < MAX_TASKS
                    && TASK_STATE[prev].load(Ordering::Relaxed) == TaskState::Dead as u8;
                if is_dead {
                    CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
                    // Save into CORE_DEAD_CTX, not TASK_CTX[prev], to avoid a
                    // write-after-claim race with a concurrent spawn that may
                    // have already reserved the now-available slot.
                    let dead_ctx = CORE_DEAD_CTX.as_mut_ptr(cid);
                    let sched_ctx = CORE_SCHED_CTX.as_ptr(cid);
                    switch_context(dead_ctx, sched_ctx);
                    // Unreachable: dead tasks are never rescheduled.
                } else {
                    // CAS: don't overwrite Dead if kill raced between is_dead
                    // check and this restore.
                    if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) {
                        TASK_STATE[prev]
                            .compare_exchange(
                                TaskState::Ready as u8,
                                TaskState::Running as u8,
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            )
                            .ok();
                    }
                    core::arch::asm!("sti", options(nostack, nomem));
                }
                return;
            }
        };

        // Path C / Phase 6: route a pending supervisor respawn from the YIELD path too, not only the
        // timer ISR (mirror of timer_tick_from_irq's pending block). `poll_supervisor_respawn` runs at
        // run()'s loop top (an IF=1 point); Core 0 reaches it by BLOCKING or via the timer ISR routing.
        // Under `chaos max-carnage` the foreground task never blocks - it paces with `yield_cpu`, whose
        // `yield_current` has its own pick_next+switch and never reaches the run loop - so recovery would
        // hinge SOLELY on the timer ISR, which the storm starves on real hardware (slow tick + long IF=0
        // console writes + IPI/shootdown pressure), draining the live set to 0 with no respawn. Driving
        // the respawn from the storm's own hot path (yield) makes recovery un-starvable: `next`/`prev`
        // stay Ready and are re-picked after the run-loop top runs poll. Zero cost when healthy (one
        // relaxed load). Skipped when already in the scheduler context (prev == IDLE).
        if cid == 0 && prev < MAX_TASKS && crate::task::supervisor_respawn_pending() {
            if TASK_VALID[prev].load(Ordering::Relaxed) && TASK_IS_USER[prev] {
                TASK_USER_RSP[prev] =
                    (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
            }
            let current_ctx: *mut TaskContext = if !TASK_VALID[prev].load(Ordering::Relaxed) {
                CORE_DEAD_CTX.as_mut_ptr(cid)   // prev self-killed - discard (never resumed)
            } else {
                TASK_CTX[prev].assume_init_mut() as *mut TaskContext
            };
            CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
            let sched_ctx: *const TaskContext = CORE_SCHED_CTX.as_ptr(cid);
            switch_context(current_ctx, sched_ctx);
            // Resumes here when prev is next scheduled (after run()'s loop top did the respawn).
            return;
        }

        if next == prev {
            // CAS: preserve Dead if kill raced between pick_next and here.
            let _ = TASK_STATE[prev].compare_exchange(
                TaskState::Ready as u8,
                TaskState::Running as u8,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            core::arch::asm!("sti", options(nostack, nomem));
            return;
        }

        TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
        CORE_CURRENT.get(cid).store(next, Ordering::Relaxed);

        // Save BEFORE prepare_ring3_switch so we capture the value from SYSCALL
        // entry, not the value prepare_ring3_switch is about to write for `next`.
        if prev < MAX_TASKS && TASK_VALID[prev].load(Ordering::Relaxed) && TASK_IS_USER[prev] {
            TASK_USER_RSP[prev] =
                (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
        }

        if TASK_IS_USER[next] {
            prepare_ring3_switch(cid, next);
        }

        // Save the current execution state.
        // Three cases:
        //   prev >= MAX_TASKS  → scheduler idle context (save to CORE_SCHED_CTX)
        //   TASK_VALID=false   → self-killed task (save to CORE_DEAD_CTX; slot
        //                        already reclaimed, TASK_CTX[prev] may be reused
        //                        by a concurrent spawn - must not write there)
        //   live task          → normal case (save to TASK_CTX[prev])
        // Saving a kstack RSP into CORE_SCHED_CTX would corrupt the scheduler's
        // BSP stack pointer, causing a KERNEL PF on the next scheduler resume.
        // CORE_DEAD_CTX is never used as a load source (dead tasks not resumed).
        let current_ctx: *mut TaskContext = if prev >= MAX_TASKS {
            CORE_SCHED_CTX.as_mut_ptr(cid)
        } else if !TASK_VALID[prev].load(Ordering::Relaxed) {
            CORE_DEAD_CTX.as_mut_ptr(cid)
        } else {
            TASK_CTX[prev].assume_init_mut() as *mut TaskContext
        };
        let next_ctx: *const TaskContext =
            TASK_CTX[next].assume_init_ref() as *const TaskContext;

        switch_context(current_ctx, next_ctx);
        core::arch::asm!("sti", options(nostack, nomem));
    }
}

/// Return the slot index of the currently-running task on this core.
///
/// Returns `IDLE` (== MAX_TASKS) if the scheduler loop is active.
pub fn current_task_slot() -> usize {
    let cid = current_core_id();
    CORE_CURRENT.get(cid).load(Ordering::Relaxed)
}

/// True if the calling task's own slot is Dead - i.e. it just killed itself via the kill syscall. Lets
/// `handle_kill` switch away after a self-kill (the tail `kill_current` already uses) instead of
/// returning into a dead task, whose next syscall would panic "no running task" in block_and_reschedule.
pub fn current_task_is_dead() -> bool {
    TASK_STATE[current_task_slot()].load(Ordering::Acquire) == TaskState::Dead as u8
}

/// Wake the task at `slot` with the given result code.
///
/// If the task lives on a different core, sends a WAKE_RECEIVER IPI to that
/// core so it exits `hlt` and can reschedule (§8.4, §9.4).
pub fn wake_by_slot(slot: usize, result: i64) {
    // SAFETY: IF=0 from IPC/syscall path.
    unsafe {
        if slot < MAX_TASKS && TASK_VALID[slot].load(Ordering::Relaxed) {
            // Do not revive a task that kill_task_by_slot has already marked Dead.
            let first = TASK_STATE[slot].load(Ordering::Acquire);
            if first == TaskState::Dead as u8 { return; }
            TASK_WAKEUP_ERR[slot] = result;

            // CAS retry loop: transition any non-Dead state → Ready.
            //
            // On real SMP hardware, block_and_reschedule (core A) and
            // wake_by_slot (core B) can race on TASK_STATE[slot]:
            //
            //   Core A loads Running, Core B loads Running.
            //   Core A's CAS(Running→Blocked) wins.
            //   Core B's CAS(Running→Ready) fails - state is now Blocked.
            //
            // Without a retry, Core B silently returns and the task stays
            // Blocked forever (confirmed on real hardware; never observed on
            // QEMU TCG because TCG serialises cores).  The retry loop re-reads
            // the updated state (Blocked) and CAS(Blocked→Ready) succeeds.
            //
            // The loop terminates because: Dead terminates early; Ready
            // terminates as "already woken"; every other state (Running,
            // BlockedOnRecv, BlockedOnSend) either has its CAS succeed or
            // transitions to Dead/Ready on the next iteration.
            let mut current = first;
            loop {
                if current == TaskState::Dead as u8 { return; }
                if TaskState::from(current) == TaskState::Ready { break; }
                match TASK_STATE[slot].compare_exchange(
                    current,
                    TaskState::Ready as u8,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        break;
                    }
                    Err(new) => {
                        current = new;
                        core::hint::spin_loop();
                    }
                }
            }

            let task_core = TASK_CORE[slot] as usize;
            let my_core   = current_core_id();

            if task_core != my_core {
                // Cross-core wakeup: the target core's pick_next may be deep into
                // its RR scan and far from `slot`.  Set the hint so pick_next
                // returns `slot` on the very next call, ahead of the RR scan.
                // Same-core wakeups do NOT set the hint: the RR scan will reach
                // the just-readied slot naturally within a few picks, and setting
                // the hint for same-core tasks (e.g. BP1's 6↔7 loop) would cause
                // pick_next to always bypass the scan and return the most recently
                // woken same-core task, starving all other slots indefinitely.
                //
                // CAS instead of store: only install hint when no hint is pending.
                // If a different slot's hint is already set (e.g. hint=8 for
                // BP2-sender when pong tries to overwrite with hint=5 for ping),
                // leave the existing hint alone.  This slot is already Ready (CAS
                // above); if the hint fires for the other slot first, the RR scan
                // on the immediately-following pick_next call will find this slot
                // (it starts from RR_SLOT which hint-fires advance past the hinted
                // slot, keeping the scan pointer near this slot's index).
                CORE_WAKE_HINT.get(task_core)
                    .compare_exchange(MAX_TASKS, slot, Ordering::Release, Ordering::Relaxed)
                    .ok();
                // APIC is initialised; task_core is a ready core (outer unsafe).
                crate::smp::ipi::send_ipi(
                    task_core as u32,
                    crate::smp::ipi::vectors::WAKE_RECEIVER,
                );
            }
        }
    }
}

/// Find the slot of a live task by name. Returns `None` if not found or dead.
pub fn find_task_by_name(name: &str) -> Option<usize> {
    // SAFETY: read-only scan; caller holds no locks.
    unsafe {
        for i in 0..MAX_TASKS {
            if TASK_VALID[i].load(Ordering::Relaxed)
                && TASK_NAME[i] == name
                && TaskState::from(TASK_STATE[i].load(Ordering::Acquire)) != TaskState::Dead
            {
                return Some(i);
            }
        }
    }
    None
}

/// Free all deferred kernel stacks from a prior self-kill on this core.
///
/// Safe to call from timer_tick_from_irq (IF=0, RSP on the current live task's
/// kstack or TSS.rsp0 of the incoming timer frame - either way, NOT on any
/// pending dead kstack) and from the scheduler run() loop (RSP on the per-core
/// BSS scheduler stack).
///
/// Must NOT be called while RSP is on one of the pending kstacks.  This is
/// guaranteed by the self-kill invariant: the window from kill_task_by_slot to
/// yield_current's switch_context runs with IF=0, so the timer cannot fire
/// during that window; after switch_context RSP is on a different stack.
fn drain_pending_kstack(cid: usize) {
    let n = CORE_PENDING_KSTACK_LEN.get(cid).load(Ordering::Relaxed);
    if n != 0 {
        // Clear before processing so re-entrant callers see an empty queue.
        CORE_PENDING_KSTACK_LEN.get(cid).store(0, Ordering::Relaxed);
        for i in 0..n {
            let kstack = unsafe { (*CORE_PENDING_KSTACK.as_mut_ptr(cid))[i] };
            // SAFETY: RSP is NOT on this kstack (see above).  kstack is the top
            // of a TASK_KSTACK_MAX-sized block allocated from the kstack pool.
            if kstack != 0 { super::free_kstack(kstack); }
        }
    }

    // Free any deferred PML4 frame from a prior self-kill.  By the time this
    // runs the core has switched to a different CR3, so freeing the old PML4
    // frame no longer risks a concurrent zeroing race (see CORE_PENDING_PML4).
    // AtomicU64 load/store: no unsafe needed here.
    let pml4_phys = CORE_PENDING_PML4.get(cid).load(Ordering::Acquire);
    if pml4_phys != 0 {
        CORE_PENDING_PML4.get(cid).store(0, Ordering::Relaxed);
        // SAFETY: pml4_phys was the task's own PML4 frame; CR3 has since been
        // switched away so no core's page-walker will read from it.
        unsafe {
            let frame = crate::memory::frame::Frame::from_phys(
                crate::memory::frame::PhysAddr(pml4_phys)
            );
            crate::memory::allocator::free_frame(frame);
        }
    }
}

/// Kill a task by slot: mark Dead, kill its endpoint (if any), notify blocked tasks.
pub fn kill_task_by_slot(slot: usize) {
    // Serialize the TASK_VALID check against concurrent kills and spawns.
    // Two cores calling kill_task_by_slot(slot) simultaneously would otherwise
    // both pass the !TASK_VALID guard, both reach free_kstack, and double-free
    // the kernel stack.  The slot can also be claimed by reserve_task_slot between
    // free_kstack (line ~807) and TASK_VALID=false (line ~817), causing the second
    // killer to free a live kernel stack → KERNEL PF.
    //
    // Fix: hold task_slot_lock while checking TASK_VALID and while atomically
    // claiming the kill (TASK_STATE=Dead).  Release the lock before the long
    // cleanup so spawn on other cores can proceed concurrently.
    task_slot_lock();
    // Lock held; exclusive access to TASK_VALID/TASK_STATE across all cores.
    // All accesses below are atomic loads/stores - no unsafe required.
    let already_dead = {
        if slot >= MAX_TASKS || !TASK_VALID[slot].load(Ordering::Relaxed) {
            task_slot_unlock();
            return;
        }
        let s = TASK_STATE[slot].load(Ordering::Acquire);
        if s == TaskState::Dead as u8 {
            // Another core is already killing this slot - bail.
            task_slot_unlock();
            return;
        }
        // Atomically claim this kill: set Dead under the lock so no concurrent
        // call can also proceed past this point for the same slot.
        // SeqCst (not Release): this store is one half of the Dekker handshake with
        // the scheduler's pick-then-commit (V3, kernel-audit-2). The scheduler does
        // store(CORE_CURRENT=next, SeqCst); fence; load(STATE, SeqCst); this side
        // does store(STATE=Dead, SeqCst); load(CORE_CURRENT, SeqCst) in the spin
        // below. With all four accesses in the single SeqCst total order, the two
        // sides cannot both miss each other, so a kill can never free a task's page
        // tables while a core is committing a switch into it.
        TASK_STATE[slot].store(TaskState::Dead as u8, Ordering::SeqCst);
        false
    };
    task_slot_unlock();
    let _ = already_dead; // always false here; kept for clarity

    // SAFETY: IF=0 or lock-free path; TASK_VALID[slot] and Dead state claimed above.
    unsafe {

        // Capture identity before any slot state changes.
        let task_name = TASK_NAME[slot];
        let task_ep   = ep_from_u64(TASK_ENDPOINT[slot].load(Ordering::Relaxed));

        // If this task was itself blocked in a synchronous CALL, drop its outstanding-call record so
        // that - once this slot is reused - a later death of the endpoint it was awaiting cannot
        // spuriously wake the slot's new occupant with ReplyDead (§8.6 reply-side death-wake).
        crate::ipc::routing::clear_call_await(slot);

        // If this task owned the console foreground (a TUI app like `chaos`), release it so a dead owner
        // cannot leave the system muted forever, and wake any shell parked in a muted blocking read.
        // Unconditional (before the endpoint cleanup) since the owner need not have an endpoint; the
        // if-owner guard is a cheap no-op for the common case of a non-owner death.
        crate::arch::x86_64::release_console_foreground_if_owner(slot as u32);

        // Kill the task's endpoint if it has one.
        if let Some(ep_id) = task_ep {
            // Bump generation in routing table and wake any blocked tasks.
            let (rx_slot, tx_slot) = crate::ipc::routing::kill_endpoint(ep_id);
            // Skip waking `slot` itself: the killed task's rx/tx slot is often
            // its own slot (the task was blocked on recv of its own endpoint).
            // Calling wake_by_slot(slot, -7) would overwrite the Dead state with
            // Ready, causing the scheduler to re-animate the dying task with its
            // freed page tables - the root cause of the use-after-free cascade.
            if let Some(s) = rx_slot { if s != slot { wake_by_slot(s, -7); } }
            if let Some(s) = tx_slot { if s != slot { wake_by_slot(s, -7); } }

            // Reply-side death-wake (§8.6, Commandment VIII): wake every caller blocked in a
            // synchronous CALL awaiting a reply from this now-dead endpoint, with ReplyDead (-12) -
            // the twin of the blocked-sender EndpointDead wake just above. kill_endpoint has already
            // bumped this endpoint's generation/liveness, so no new caller can register against it;
            // take_call_waiter drains the callers that registered while it was alive, one per loop
            // (bounded by one entry per task). Skip `slot` itself defensively (as rx/tx do).
            while let Some(waiter) = crate::ipc::routing::take_call_waiter(ep_id) {
                if waiter != slot { wake_by_slot(waiter, -12); }
            }

            // Mark resource dead in global cap table so generation check fails.
            let resource_id = crate::capability::cap::ResourceId::from(ep_id);
            crate::capability::table::mark_dead_resource(resource_id);

            // Reclaim every delegated resource this service owned (§7.10). A restartable
            // owner (e.g. `fs`, Phase D) gets a fresh endpoint id on respawn and would never
            // free the old instance's file resources, so without this they orphan in the band
            // and leak its capacity on every restart. Each is marked Dead, so a client's stale
            // file cap fails its next use with EndpointDead - the same signal as any dead owner.
            let reclaimed = crate::capability::delegated::release_owner(ep_id.0);
            if reclaimed > 0 {
                crate::kprintln!("delegated: reclaimed {} resource(s) from dead endpoint {}",
                    reclaimed, ep_id.0);
            }

            // Stop this name resolving to the now-dead endpoint (§14.2), BEFORE freeing the id. The
            // ORDER here is load-bearing. free_endpoint_id makes ep_id reusable, and a fresh instance
            // on another core can immediately alloc it and re-register the SAME name to that id. If we
            // freed first and unregistered second, this dead instance's unregister - which matches only
            // the (now-reused) id - would clear the LIVE new entry, so the name vanishes from the
            // directory and clients get "storage unavailable" though the service is alive (the
            // post-max-carnage bug: the endpoint-id guard alone is NOT enough once ids are reused).
            // Unregistering while the id is still ours, then freeing, means a reused id can never clear
            // the new instance's entry (free is the barrier: nobody can alloc the id until after the
            // unregister). The supervisor's reconcile then sees the name MISSING and respawns the
            // service, instead of adopting a stale dead entry - the original reason this exists
            // (fs/block-driver staying dead after a storm killed them while the supervisor was
            // mid-respawn, their death-notifications lost). Normal restarts are unaffected: the service
            // re-registers on respawn; clients briefly see a lookup miss and retry.
            crate::ipc::names::unregister_endpoint(task_name, ep_id);

            // Driver-death IRQ-route teardown (Item 2, driver-death quiesce), BEFORE freeing the id.
            // A driver that registered a hw_interrupt line left a route in IRQ_TABLE; the id is about to
            // be freed and REUSED, and IRQ_TABLE keys on a bare endpoint id, so a reused id could inherit
            // the dead driver's interrupts. Clear the route and mask the line now (while the id is still
            // ours); the respawned driver re-registers and re-opens its gate. The vectors mirror the
            // ServiceConfig hw_irqs (task/mod.rs) - the same by-name device coupling the DMA-quiesce block
            // below already uses; block-driver + nic-driver currently declare no hw_irq.
            let dead_irq: u8 = match task_name {
                "xhci" => 0x28,
                "ehci" => 0x29,
                _ => 0xFF,
            };
            if dead_irq != 0xFF {
                crate::interrupt::route::unregister(dead_irq);
                // Mask the source: effective for a level/INTx line (the ehci); a no-op for edge/MSI (the
                // xhci), whose stray MSI is already harmless once the route is None (deliver finds no
                // endpoint) and stops at the respawned driver's HCRST.
                crate::arch::x86_64::ioapic::mask_vector(dead_irq);
            }

            // Reclaim the endpoint id itself for reuse (§14.2). Its routing entry is Dead and its
            // resource generation is bumped, so handing the id out again is safe: the next endpoint to
            // take it is seeded at a strictly higher generation (task::spawn), so a stale cap to this
            // dead endpoint still fails. Without this the id counter only climbs and a sustained
            // restart storm (`chaos max-carnage`) exhausts the [100, DELEGATED_BASE) band and panics.
            crate::ipc::free_endpoint_id(ep_id);
        }

        // RESTART count (the observe RESTARTS column): only the restartable/managed set accrues a
        // restart when it dies + gets respawned. A transient utility the shell re-invokes (observe-*,
        // greet, ...) is never bumped, so it never shows a restart - RESTARTS means "blew up and was
        // recovered", not "legitimately closed". The respawn reads the new count via next_restart_count.
        if matches!(task_name,
            "fs" | "block-driver" | "shell" | "xhci" | "ehci" | "logger" | "supervisor" | "counter"
            | "nic-driver" | "net-stack")
        {
            bump_name_restart(task_name);
        }

        // Restartable-service death notification. These are restartable userspace services (not
        // trusted root): when one dies, notify the supervisor over its death-notification endpoint so
        // it respawns the service IMMEDIATELY - its own death, not only a lucky supervisor respawn.
        // The set: `fs` + `block-driver` (Phase D); `shell` (the user's prompt); and the drivers
        // `xhci` / `ehci` + `logger`. Without the drivers here, a `chaos max-carnage` that killed
        // them in its last rounds left them dead until the supervisor happened to be respawned (it
        // re-runs its boot sequence and re-spawns them) - so the keyboard could stay dead. Now their
        // own death respawns them. `fs` re-mounts via its journal (Phase C); clients reacquire by
        // name via the kernel directory (§14.3). "Nothing escapes" - every service recovers; the
        // kernel is the only unkillable thing.
        // Gated to this NAMED set so ordinary probe/app churn never floods the supervisor.
        // `enqueue_from_interrupt` is the kernel→endpoint path (no cap needed); wake the supervisor.
        // `counter` (examples/counter) is restartable too: it persists its state to `fs` and
        // reconstructs it on respawn (§14/§15), so its own death notifies the supervisor, which
        // respawns it (counter-test build only - it is absent elsewhere, so this never fires there).
        if matches!(task_name, "fs" | "block-driver" | "shell" | "xhci" | "ehci" | "logger" | "counter"
            | "nic-driver" | "net-stack") {
            if let (Some(sup_ep), Ok(msg)) = (
                crate::ipc::names::lookup("supervisor"),
                crate::ipc::message::Message::new(task_name.as_bytes()),
            ) {
                if let Some(sup_slot) =
                    crate::ipc::routing::enqueue_from_interrupt(sup_ep, msg)
                {
                    wake_by_slot(sup_slot, 0);
                }
            }
        }

        // Path C / Phase 6: the supervisor is restartable - the KERNEL is its recovery anchor (the
        // one thing that cannot die, §3.7). Flag a respawn to run from the next Core-0 control tick
        // (NOT inline: we are mid-teardown of this very task). The new instance re-registers its
        // endpoint in `ipc::names`, so death notifications re-point to it automatically, and it
        // reconciles live services on boot. (§6.2 amended - supervisor death is no longer a panic.)
        if task_name == "supervisor" {
            crate::kprintln!("kernel: supervisor died");
            crate::task::flag_supervisor_respawn();
        }

        // A dying DMA-capable driver: QUIESCE its DMA, then (H1) reclaim its IOMMU resources.
        //
        // DMA quiesce is the cure for the chaos `max-carnage` page-table corruption (the round-4286 KERNEL
        // PF, then caught + skipped by page_tables::walk's guard): under the kill/respawn churn a driver's
        // DMA frames get freed and REUSED (e.g. as a page table), and the controller's still-live engine -
        // EHCI's continuous periodic schedule, or an in-flight AHCI command - scribbles foreign data into
        // them, landing a non-PTE value in a PTE slot. On the T630 ehci + block-driver are IOMMU-PASSTHROUGH,
        // so nothing else stops the stray write. We clear PCI Bus-Master-Enable BEFORE the frame reclaim
        // below (the controller cannot start new DMA; any in-flight transaction drains during the kill's
        // remaining work + the spin-wait); the respawned driver re-enables bus-mastering during init.
        // block-driver (AHCI) is included; xhci is confined (its stray DMA would fault, not corrupt) but
        // quiescing it too is harmless + correct on a no-IOMMU machine where it is passthrough as well.
        if task_name == "xhci" || task_name == "ehci" || task_name == "block-driver"
            || task_name == "nic-driver"
        {
            use core::sync::atomic::Ordering::Relaxed;
            use crate::arch::x86_64::pci;
            let bdf = match task_name {
                "xhci"       => pci::XHCI_BDF.load(Relaxed),
                "ehci"       => pci::EHCI_BDF.load(Relaxed),
                "nic-driver" => pci::NIC_BDF.load(Relaxed),
                _            => pci::AHCI_BDF.load(Relaxed), // block-driver
            };
            pci::clear_bus_master(bdf);
            // H1: revert the IOMMU DTE to passthrough + free the I/O page table so a restart re-confines
            // cleanly (no-op if the device wasn't confined). Confined USB drivers (xhci/ehci) only; AHCI
            // + nic-driver run in IOMMU passthrough, so there is no DTE to revert.
            if task_name == "xhci" || task_name == "ehci" {
                crate::arch::x86_64::iommu::release_device(bdf);
            }
        }

        // SMP safety: spin until no other core has CORE_CURRENT[c] == slot.
        //
        // A core may have selected this slot from pick_next (observing STATE=Ready)
        // before our STATE=Dead store propagated.  That core has set
        // CORE_CURRENT[c]=slot and is about to call switch_context, loading this
        // task's cr3.  We must not free the page-table frames until that core has
        // moved on (CORE_CURRENT[c] changes to a different task), because after
        // switch_context the core is in kernel mode with the shared higher-half
        // mappings - it will load the new cr3 next, and kernel code does not
        // touch user-half frames.  Bounded by one preemption quantum (~10 ms).
        //
        // We skip the calling core: either it holds a different slot (the common
        // cross-core kill path), or this is kill_current where the caller switches
        // away immediately after returning - in both cases the skip is safe.
        //
        // Send a WAKE_RECEIVER IPI to any core currently running the dead task so
        // it immediately calls yield_current → detects Dead → switches to the
        // scheduler, allowing the spin-wait below to exit before the next timer
        // tick (otherwise we spin up to one full 10 ms quantum).  The IPI is sent
        // after STATE=Dead is visible (SeqCst fence above), so the receiving core
        // will observe the Dead state in its yield_current Dead-detection branch.
        {
            let my_core = current_core_id();
            for cid in 0..num_cores() {
                if cid != my_core && CORE_CURRENT.get(cid).load(Ordering::SeqCst) == slot {
                    // cid is a valid core index (loop bound); APIC mapped (outer unsafe).
                    crate::smp::ipi::send_ipi(
                        cid as u32,
                        crate::smp::ipi::vectors::WAKE_RECEIVER,
                    );
                }
            }
        }
        {
            let my_core = current_core_id();
            for cid in 0..num_cores() {
                if cid == my_core { continue; }
                loop {
                    // Compiler + hardware barrier: reload CORE_CURRENT[cid] from
                    // memory on every iteration; do not use a cached register value.
                    // SeqCst load (V3): the other half of the Dekker handshake with
                    // the scheduler's store(CORE_CURRENT=next, SeqCst) - all four
                    // conflicting accesses are SeqCst so the two sides cannot both
                    // miss each other. (The fence is now redundant but harmless.)
                    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                    if CORE_CURRENT.get(cid).load(Ordering::SeqCst) != slot { break; }
                    core::hint::spin_loop();
                }
            }
        }

        // Detect self-kill before reclaim so we can defer the PML4 frame.
        //
        // Self-kill PML4 race (root cause of 6B/8A KERNEL PF):
        //   In the self-kill path this core's CR3 is still the dying task's
        //   PML4.  If we free the PML4 frame here, another core's
        //   PageTable::new() can immediately alloc + write_volatile-zero it.
        //   The hardware page-walker on this core then reads a zeroed PML4 on
        //   any TLB miss (e.g. reading a .rodata format string) → KERNEL PF.
        //
        //   Fix: skip freeing the PML4 frame in the self-kill path; store it
        //   in CORE_PENDING_PML4[my_core] and release it in drain_pending_pml4
        //   (called from the scheduler loop / timer tick) where CR3 has already
        //   been switched to a different page table.
        let my_core = current_core_id();
        let is_self_kill = CORE_CURRENT.get(my_core).load(Ordering::Relaxed) == slot;

        // Reclaim all user-space frames: walk the page table and return each
        // frame to the allocator (§10.5).
        //
        // TLB coherence: the spin-wait above guarantees every other core has
        // loaded a *different* CR3 since last running this task.  A CR3 reload
        // flushes all non-global TLB entries, so no core retains stale
        // translations for this task's virtual addresses.  A separate TLB
        // shootdown IPI would therefore be redundant - and dangerous: if a
        // remote core is mid-syscall with IF=0 (e.g. loading an ELF for a
        // concurrent spawn), it cannot ACK the IPI, causing the caller to spin
        // indefinitely (deadlock).  We skip the broadcast and rely solely on
        // the spin-wait guarantee.
        let freed_frames: usize;
        if TASK_IS_USER[slot] {
            let cr3 = TASK_CTX[slot].assume_init_ref().cr3;
            if cr3 != 0 {
                let pml4_phys = cr3 & !0xFFF_u64;
                // SAFETY: cr3 is the task's PML4 set at spawn and immutable
                // until now.  Task is Dead; all other cores have moved past this
                // slot (spin-wait above); no core will load this cr3 hereafter.
                // reclaim_user_frames frees every user frame INLINE except the PML4 root, and
                // returns the count (no fixed buffer - it no longer leaks large allocations).
                let freed = crate::arch::x86_64::page_tables::reclaim_user_frames(cr3);
                if is_self_kill {
                    // Defer the PML4 frame: our CR3 still points to it, and freeing it now lets
                    // another core zero it (PageTable::new) while we hold that CR3, causing a
                    // TLB-miss → zeroed PML4 → KERNEL PF (see CORE_PENDING_PML4).
                    if pml4_phys != 0 {
                        CORE_PENDING_PML4.get(my_core).store(pml4_phys, Ordering::Release);
                    }
                } else if pml4_phys != 0 {
                    // Not a self-kill: no core holds this CR3, so free the PML4 root now.
                    // (Same unsafe context as the old in-loop free_frame above.)
                    let frame = crate::memory::frame::Frame::from_phys(
                        crate::memory::frame::PhysAddr(pml4_phys)
                    );
                    crate::memory::allocator::free_frame(frame);
                }
                freed_frames = freed;
            } else {
                freed_frames = 0;
            }
        } else {
            freed_frames = 0;
        }
        crate::kprintln!("kill_task: slot={} '{}' freed {} frames", slot, task_name, freed_frames);

        // Kstack free and slot release.
        //
        // Self-kill: RSP is on K_a (page-fault ISR pushed to TSS.RSP0 = K_a).
        // Freeing K_a immediately lets another core alloc K_a for a new task
        // that crashes, pushing its ISR frame to K_a while this core is still
        // executing there - corrupting both stacks (KERNEL PF).
        //
        // Fix - deferred kstack only: release the slot immediately (TASK_VALID=false)
        // so reserve_task_slot can reuse it without the zombie 10ms starvation
        // window, but enqueue the kstack pointer for free at the next timer tick
        // or scheduler idle loop, where RSP is on a different stack.
        //
        // Context save safety: yield_current (called immediately after this return)
        // detects TASK_VALID=false and saves the dead task's registers into
        // CORE_DEAD_CTX[cid] rather than TASK_CTX[slot], preventing a
        // write-after-claim race if a concurrent spawn has already reserved
        // TASK_CTX[slot] via reserve_task_slot.
        //
        // Cross-kill: this core's RSP is on the supervisor's kstack, not K_a.
        // Free immediately and release the slot now.
        if is_self_kill {
            // Self-kill: defer only the kstack free; release slot immediately.
            if TASK_IS_USER[slot] {
                let kstack = TASK_KERNEL_STACK_TOP[slot].load(Ordering::Relaxed);
                if kstack != 0 {
                    let len = CORE_PENDING_KSTACK_LEN.get(my_core).load(Ordering::Relaxed);
                    if len < PENDING_KSTACK_CAP {
                        (*CORE_PENDING_KSTACK.as_mut_ptr(my_core))[len] = kstack;
                        CORE_PENDING_KSTACK_LEN.get(my_core).store(len + 1, Ordering::Relaxed);
                    } else {
                        // Queue overflow (>8 sequential self-kills): free immediately.
                        // Bounded risk - less likely than permanently leaking the stack.
                        super::free_kstack(kstack);
                    }
                }
            }
            // Release slot now - no zombie period, no starvation.
            task_slot_lock();
            TASK_VALID[slot].store(false, Ordering::Release);
            task_slot_unlock();
            return;
        }

        // Cross-kill: free kstack immediately (RSP is on a different kstack).
        if TASK_IS_USER[slot] {
            // SAFETY: Cross-kill - our RSP is on the supervisor's kstack, not K_a.
            super::free_kstack(TASK_KERNEL_STACK_TOP[slot].load(Ordering::Relaxed));
        }

        // Release the slot under the lock.  Re-store Dead to guard against a
        // concurrent wake_by_slot that may have set state=Ready between the
        // top-of-function store and now.
        task_slot_lock();
        TASK_STATE[slot].store(TaskState::Dead as u8, Ordering::Release);
        TASK_VALID[slot].store(false, Ordering::Release);
        task_slot_unlock();
    }
}

/// Block the currently-running task and switch to the next ready task.
///
/// Returns the wakeup result code (0 = success, negative = IpcError).
/// Re-enables interrupts before returning.
///
/// The caller must have already recorded the blocking reason in the routing
/// table (under its spinlock) *before* calling this function. The routing
/// spinlock must be released *before* this call.
/// Park the current task: block it indefinitely with no waker. Used by idle
/// trusted-root services (init, supervisor) once their work is done - they must
/// not busy-`yield`, which keeps their core off the idle (halt) path and pegs it
/// at 100%. Parking lets the core reach the scheduler idle loop and halt (cool)
/// where ARAT/TSC-Deadline allow it. Reuses block_and_reschedule; nothing wakes a
/// parked task in v1 (the supervisor's death-notification loop is future work).
pub fn park_current() -> i64 {
    block_and_reschedule(TaskState::BlockedOnRecv)
}

pub fn block_and_reschedule(state: TaskState) -> i64 {
    // SAFETY: IF=0 (caller ensures this; double-cli is a no-op).
    unsafe {
        core::arch::asm!("cli", options(nostack, nomem));

        let cid  = current_core_id();
        let slot = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        assert!(slot < MAX_TASKS && TASK_VALID[slot].load(Ordering::Relaxed),
                "block_and_reschedule: no running task");

        // Atomically transition Running → Blocked.
        //
        // If wake_by_slot already set state to Ready (between the routing-table
        // unlock and here), compare_exchange sees Running→Ready mismatch and
        // fails → we return immediately instead of overwriting Ready with
        // Blocked (lost-wakeup prevention, §8.4).
        //
        // AcqRel on success, Acquire on failure: in both cases we synchronise
        // with wake_by_slot's Release store so TASK_WAKEUP_ERR is visible.
        if TASK_STATE[slot].compare_exchange(
            TaskState::Running as u8,
            state as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ).is_err() {
            // CAS failed: wake_by_slot already set state to Ready (lost-wakeup prevention).
            core::arch::asm!("sti", options(nostack, nomem));
            return TASK_WAKEUP_ERR[slot];
        }

        let current_ctx = TASK_CTX[slot].assume_init_mut() as *mut TaskContext;

        // Save user_rsp before switching away: the SYSRETQ exit on resume must
        // load this task's RSP, not the value another task wrote to PER_CORE_SYSCALL.
        if TASK_IS_USER[slot] {
            TASK_USER_RSP[slot] =
                (*crate::arch::x86_64::syscall_entry::syscall_slot(cid)).user_rsp;
        }

        match pick_next(cid) {
            Some(next) => {
                TASK_STATE[next].store(TaskState::Running as u8, Ordering::Relaxed);
                CORE_CURRENT.get(cid).store(next, Ordering::Relaxed);
                if TASK_IS_USER[next] {
                    prepare_ring3_switch(cid, next);
                }
                let next_ctx = TASK_CTX[next].assume_init_ref() as *const TaskContext;
                switch_context(current_ctx, next_ctx);
            }
            None => {
                CORE_CURRENT.get(cid).store(IDLE, Ordering::Relaxed);
                let sched = CORE_SCHED_CTX.as_mut_ptr(cid);
                switch_context(current_ctx, sched);
            }
        }

        core::arch::asm!("sti", options(nostack, nomem));
        TASK_WAKEUP_ERR[slot]
    }
}

/// Store `msg` as the last received message for the current task.
pub fn store_recv_message(msg: Message) {
    let cid = current_core_id();
    // SAFETY: CORE_CURRENT[cid] and TASK_RECV_BUF are written only by this core's scheduler path.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS {
            TASK_RECV_BUF[cur] = Some(msg);
        }
    }
}

/// Take (consume) the last received message for the current task.
pub fn take_recv_message() -> Option<Message> {
    let cid = current_core_id();
    // SAFETY: CORE_CURRENT[cid] and TASK_RECV_BUF are written only by this core's scheduler path.
    unsafe {
        let cur = CORE_CURRENT.get(cid).load(Ordering::Relaxed);
        if cur < MAX_TASKS {
            TASK_RECV_BUF[cur].take()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers.
// ---------------------------------------------------------------------------

/// Round-robin scan for the next Ready task pinned to `core_id`.
///
/// Uses `CORE_RR_SLOT` as a sticky scan pointer that advances *past* the
/// selected slot after each pick.  This guarantees every slot in [0, MAX_TASKS)
/// is visited before the pointer wraps, preventing high-numbered slots from
/// being permanently starved by a dense band of ready tasks at lower indices
/// (§9.1 - no service may monopolise a core; §9.3 - yield is advisory).
///
/// Before the RR scan, checks `CORE_WAKE_HINT`: if a task was just woken by
/// `wake_by_slot` it is returned immediately without scanning, so a recently
/// woken task cannot be starved by the current RR position.
fn pick_next(core_id: usize) -> Option<usize> {
    // Fast path: schedule the just-woken task immediately.
    let hint = CORE_WAKE_HINT.get(core_id).load(Ordering::Acquire);
    if hint < MAX_TASKS {
        // Clear the hint regardless - if the slot turns out not to be
        // schedulable the RR scan below will find something else.
        CORE_WAKE_HINT.get(core_id).store(MAX_TASKS, Ordering::Relaxed);
        // SAFETY: hint < MAX_TASKS; TASK_VALID/TASK_STATE are AtomicBool/AtomicU8
        // arrays (no unsafe needed); TASK_CORE is static mut but read-only here
        // after task spawn (immutable once set - see scheduler.rs §9.1 invariant).
        let v = TASK_VALID[hint].load(Ordering::Relaxed);
        let s = TASK_STATE[hint].load(Ordering::Acquire);
        let c = unsafe { TASK_CORE[hint] };
        let ready = v
            && TaskState::from(s) == TaskState::Ready
            && c == core_id as u32;
        if ready {
            // Do NOT advance CORE_RR_SLOT here.
            //
            // Advancing RR from the hint path resets the scan pointer to
            // (hint+1) on every hint fire.  When a frequently-woken slot
            // (e.g. supervisor at slot 1) fires hint repeatedly, RR is
            // continually reset to 2, and any task at a higher slot index
            // is never reached by the RR scan between hint firings.
            //
            // The RR scan path below advances the pointer correctly after
            // each scan-selected pick.  The hint path's sole job is to
            // return the just-woken slot immediately; it must not disturb
            // the scan state.
            return Some(hint);
        }
    }

    let start = CORE_RR_SLOT.get(core_id).load(Ordering::Relaxed);
    for i in 0..MAX_TASKS {
        let idx = (start + i) % MAX_TASKS;
        // Acquire: sees the Ready write from wake_by_slot's Release store.
        // SAFETY: TASK_STATE and TASK_CORE are static mut arrays; idx < MAX_TASKS.
        let (v2, s2, c2) = unsafe {(
            TASK_VALID[idx].load(Ordering::Relaxed),
            TASK_STATE[idx].load(Ordering::Acquire),
            TASK_CORE[idx],
        )};
        let ready = v2
            && TaskState::from(s2) == TaskState::Ready
            && c2 == core_id as u32;
        if ready {
            // Advance past the selected slot so the next call starts after it.
            CORE_RR_SLOT.get(core_id).store((idx + 1) % MAX_TASKS, Ordering::Relaxed);
            return Some(idx);
        }
    }
    None
}
