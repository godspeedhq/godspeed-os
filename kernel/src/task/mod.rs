//! Task management — §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

use crate::smp::SpinLock;

use crate::arch::x86_64::context_switch::TaskContext;
use crate::arch::x86_64::page_tables::{
    get_hhdm_offset, PageFlags, VirtAddr, PAGE_SIZE,
};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, SPAWN_RESOURCE, CONSOLE_READ_RESOURCE};
use crate::capability::cap::ResourceId;
use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::memory::allocator::alloc_frame;
use crate::memory::frame::PhysAddr;

// ---------------------------------------------------------------------------
// Kernel stack pool — one 64 KiB stack per ring-3 task (§14.1).
// ---------------------------------------------------------------------------

const TASK_KSTACK_MAX: usize = 224; // raised from 208 to accommodate Milestone 20 brutal adversarial probes
const KSTACK_SIZE:     usize = 64 * 1024;

#[repr(C, align(16))]
struct KernelStackStorage {
    data: [u8; KSTACK_SIZE * TASK_KSTACK_MAX],
}

static mut KSTACK_STORAGE: KernelStackStorage =
    KernelStackStorage { data: [0u8; KSTACK_SIZE * TASK_KSTACK_MAX] };

// Boolean liveness flags for each kstack slot. Protected by SpinLock so
// concurrent alloc/free on different cores are atomic without volatile tricks.
static KSTACK_USED: SpinLock<[bool; TASK_KSTACK_MAX]> =
    SpinLock::new([false; TASK_KSTACK_MAX]);

fn alloc_kstack() -> Option<*mut u8> {
    let mut used = KSTACK_USED.lock();
    for i in 0..TASK_KSTACK_MAX {
        if !used[i] {
            used[i] = true;
            // SAFETY: i < TASK_KSTACK_MAX; offset is within KSTACK_STORAGE bounds.
            let top = unsafe {
                KSTACK_STORAGE.data.as_mut_ptr().add(i * KSTACK_SIZE + KSTACK_SIZE)
            };
            return Some(top);
        }
    }
    crate::kprintln!("alloc_kstack: pool exhausted (all {} slots used)", TASK_KSTACK_MAX);
    None
}

/// Return a kstack to the pool.
///
/// `kstack_top` is the value previously returned by `alloc_kstack`
/// (the virtual address of the byte one-past the top of the kstack).
/// A value of 0 means the task had no kstack (ring-0 task) and is
/// silently ignored.
pub fn free_kstack(kstack_top: u64) {
    if kstack_top == 0 { return; }
    // SAFETY: KSTACK_STORAGE is a stable static; pointer arithmetic is within bounds.
    let base = unsafe { KSTACK_STORAGE.data.as_ptr() as u64 };
    // top = base + (idx + 1) * KSTACK_SIZE  →  idx = (top - base) / KSTACK_SIZE - 1
    if kstack_top <= base { return; }
    let offset = kstack_top - base;
    if offset % KSTACK_SIZE as u64 != 0 { return; } // misaligned top — ignore
    let idx_plus_one = offset / KSTACK_SIZE as u64;
    if idx_plus_one == 0 || idx_plus_one > TASK_KSTACK_MAX as u64 { return; }
    let idx = (idx_plus_one - 1) as usize;
    KSTACK_USED.lock()[idx] = false;
}

// ---------------------------------------------------------------------------
// ServiceContextData page — written by kernel, read by SDK (§SDK).
//
// Layout is fixed and MUST match `ServiceContextData` in
// `sdk/rust/src/service_context.rs`.
// ---------------------------------------------------------------------------

pub const SERVICE_CTX_VA:    u64 = 0x3ff000;
pub const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;

/// Maximum named send peers per service.
pub const MAX_SEND_PEERS:  usize = 4;
/// Maximum bytes per peer name stored in ServiceContextData.
pub const PEER_NAME_BYTES: usize = 24;

/// One entry in the send-peer slot table.
#[repr(C)]
struct SendPeerEntry {
    slot:     u32,                   // cap slot; u32::MAX = not populated
    name_len: u32,
    name:     [u8; PEER_NAME_BYTES],
}

/// Layout written into the service context page before launch.
#[repr(C)]
struct ServiceContextData {
    magic:              u32,
    log_write_slot:     u32,
    recv_slot:          u32,
    spawn_slot:         u32,
    send_peer_count:    u32,
    core_id:            u32,
    probe_mode:         u32,
    console_read_slot:  u32, // u32::MAX = not present; slot index if service has console_read cap
    send_peers:         [SendPeerEntry; MAX_SEND_PEERS],
}

// ---------------------------------------------------------------------------
// User stack layout constants.
// ---------------------------------------------------------------------------

const USER_STACK_TOP:   u64 = 0x8000_0000;
const USER_STACK_PAGES: u64 = 64; // 256 KiB — enough for pf_handler running on user stack
const USER_STACK_BASE:  u64 = USER_STACK_TOP - USER_STACK_PAGES * PAGE_SIZE as u64;

// ---------------------------------------------------------------------------
// Spawn error.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum SpawnError {
    LoadFailed(crate::loader::LoadError),
    NoMemory,
    MapFailed,
    CapTableFull,
    NotFound,
}

impl From<crate::loader::LoadError> for SpawnError {
    fn from(e: crate::loader::LoadError) -> Self {
        SpawnError::LoadFailed(e)
    }
}

// ---------------------------------------------------------------------------
// Service configuration table.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registry ELF — conditionally replaced for §22 Test 1B.
// When the kernel is built with --features test-bad-registry, the registry
// binary is two garbage bytes that will fail ELF loading, causing init to
// observe a spawn error and call Abort (syscall 9) → kernel panic (§6.2).
// ---------------------------------------------------------------------------

#[cfg(feature = "test-bad-registry")]
const REGISTRY_ELF: &[u8] = b"\xDE\xAD"; // invalid ELF, triggers LoadFailed
#[cfg(not(feature = "test-bad-registry"))]
const REGISTRY_ELF: &[u8] = include_bytes!(env!("SVC_REGISTRY_ELF"));

struct ServiceConfig {
    elf:               &'static [u8],
    has_recv_endpoint: bool,
    /// Names of services this one needs to send to.
    send_peers:        &'static [&'static str],
    /// If true, mint SEND|GRANT caps for send_peers (cap-transfer tests, §22 Test 5A).
    send_peers_grant:  bool,
    /// Preferred core; u32::MAX = round-robin.
    preferred_core:    u32,
    /// Written into ServiceContextData.probe_mode at spawn. 0 for all non-test services.
    probe_mode:        u32,
    /// Maximum bytes the task may allocate via AllocMem (§10.2).
    memory_limit:      u64,
    /// Hardware IRQ lines to route to this service's recv endpoint (§12.3).
    /// At spawn time the kernel calls `interrupt::route::register(irq, endpoint)`
    /// for each entry. Empty for all non-driver services.
    hw_irqs:           &'static [u8],
    /// If true, mint a CONSOLE_READ_RESOURCE cap and write the slot to
    /// ServiceContextData.console_read_slot. Only the shell service sets this.
    has_console_read:  bool,
}

fn service_config(name: &str) -> Option<(&'static str, ServiceConfig)> {
    match name {
        "supervisor" => Some(("supervisor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_SUPERVISOR_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "registry" => Some(("registry", ServiceConfig {
            elf:               REGISTRY_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "logger" => Some(("logger", ServiceConfig {
            elf:               include_bytes!(env!("SVC_LOGGER_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "ping" => Some(("ping", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PING_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["pong", "registry"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "pong" => Some(("pong", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PONG_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Probe services — §22 Group A identity tests.
        // All use the same probe ELF; probe_mode selects the test behaviour.
        // Spawn ordering in supervisor: recv-endpoint services first, then
        // senders that need SEND caps wired to them.
        // ----------------------------------------------------------------
        "probe-recv" => Some(("probe-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        1, // MODE_ECHO_RECV — Test 3A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-victim" => Some(("probe-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE — killed by probe-4a in Test 4A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4b-recv" => Some(("probe-4b-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE — killed by harness in Test 4B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-3b" => Some(("probe-3b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        3, // MODE_NO_SEND_RIGHT — Test 3B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-sender" => Some(("probe-sender", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        2, // MODE_ECHO_SEND — Test 3A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4a" => Some(("probe-4a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-victim"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        4, // MODE_SEND_AFTER_KILL — Test 4A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4b-send" => Some(("probe-4b-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-4b-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        5, // MODE_FILL_AND_BLOCK — Test 4B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-yielder" => Some(("probe-yielder", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        6, // MODE_YIELD_LOGGER — Test 8A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-hog" => Some(("probe-hog", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        7, // MODE_HOG — Test 8B (preemption proven via ping)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-9b" => Some(("probe-9b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        8, // MODE_CAP_FORGE — Test 9B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Cap-transfer probes — §22 Tests 5A and 5B.
        // probe-5a-recv must be spawned before probe-5a-send and probe-5b-send
        // so its endpoint is registered before sender caps are wired.
        // ----------------------------------------------------------------
        "probe-5a-recv" => Some(("probe-5a-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        9, // MODE_GRANT_RECV — Test 5A receiver
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-5a-send" => Some(("probe-5a-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  true,  // mints SEND|GRANT cap to probe-5a-recv
            preferred_core:    0,
            probe_mode:        10, // MODE_GRANT_SEND — Test 5A sender
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-5b-send" => Some(("probe-5b-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  false, // SEND only — no GRANT right; should return CapNotGrantable
            preferred_core:    0,
            probe_mode:        11, // MODE_NO_GRANT_SEND — Test 5B negative
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Memory-limit probes — §22 Tests 7A and 7B.
        // ----------------------------------------------------------------
        "probe-7a" => Some(("probe-7a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        12, // MODE_ALLOC_OK — Test 7A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-7b" => Some(("probe-7b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        13, // MODE_ALLOC_LIMIT — Test 7B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Interrupt-routing probe — §22 Tests IR1A (§12.2, §12.3).
        // hw_irqs registers IRQ 33 to probe-11a's recv endpoint at spawn.
        // ----------------------------------------------------------------
        "probe-11a" => Some(("probe-11a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX, // round-robin
            probe_mode:        160, // MODE_IRQ_RECV — Test IR1A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[33],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Property-test probes — Milestone 9 Phase 1.
        // prop-p9-victim must be listed (and spawned) before prop-p9 so its
        // endpoint is in the name registry when prop-p9's SEND caps are wired.
        // ----------------------------------------------------------------
        "prop-p9-victim" => Some(("prop-p9-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,  // MODE_PASSIVE — killed by prop-p9
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p1" => Some(("prop-p1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        20, // MODE_PROP_P1
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p9" => Some(("prop-p9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            // Three SEND caps to the same endpoint — proves all cap slots are
            // invalidated on endpoint death, not just the first (§7.5).
            send_peers:        &["prop-p9-victim", "prop-p9-victim", "prop-p9-victim"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        21, // MODE_PROP_P9
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p10" => Some(("prop-p10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        22, // MODE_PROP_P10
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Property-test probes — Milestone 9 Phase 2.
        // ----------------------------------------------------------------
        // P2: generation monotonic. prop-p2-victim must be listed before prop-p2.
        // prop-p2 pinned to Core 3 — away from P8 (Core 1) and P6 (Core 2).
        "prop-p2-victim" => Some(("prop-p2-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE — killed/respawned by prop-p2
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p2" => Some(("prop-p2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3,
            probe_mode:        23, // MODE_PROP_P2
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // P3: cap rights non-widening. Self-referential: sends cap to own endpoint.
        "prop-p3" => Some(("prop-p3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        24, // MODE_PROP_P3
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // P6: queue invariants. Self-referential: sends to own endpoint.
        // Pinned to Core 2 — away from the P2 (Core 3) and P8 (Core 1) kill/spawn
        // controllers whose long spawn syscalls would starve P6 of CPU time.
        "prop-p6" => Some(("prop-p6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["prop-p6"],
            send_peers_grant:  false,
            preferred_core:    2,
            probe_mode:        25, // MODE_PROP_P6
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // P8: name resolves to higher generation + liveness. prop-p8-victim before prop-p8.
        // Pinned to Core 1 so P8's kill/spawn loop doesn't share a core with P6 (Core 2)
        // or P2 (Core 3).
        "prop-p8-victim" => Some(("prop-p8-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE — killed/respawned by prop-p8
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p8" => Some(("prop-p8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        26, // MODE_PROP_P8
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Property-test probes — Milestone 9 Phase 3.
        // ----------------------------------------------------------------
        // P4: memory accounting. No victim needed.
        "prop-p4" => Some(("prop-p4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        27, // MODE_PROP_P4
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // P5: endpoint ownership. Victim must be listed before controller.
        "prop-p5-victim" => Some(("prop-p5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE — killed/respawned by prop-p5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p5" => Some(("prop-p5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        28, // MODE_PROP_P5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // P7: TLB shootdown proxy. Victim must be listed before controller.
        "prop-p7-victim" => Some(("prop-p7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE — killed/respawned by prop-p7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p7" => Some(("prop-p7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        29, // MODE_PROP_P7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal property test probes — Milestone 16.
        // 10 escalated-iteration variants of P1–P10, each with its own victim
        // where the original property needed one.  Victims before controllers.
        // ----------------------------------------------------------------
        // BP1: cap unforgeability at 100k iterations.
        "prop-bp1" => Some(("prop-bp1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        104,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP2: generation monotonic over 20 kill/respawn cycles.
        "prop-bp2-victim" => Some(("prop-bp2-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-bp2" => Some(("prop-bp2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        105,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP3: cap rights never widen — 10k iterations (self-referential, like P3).
        "prop-bp3" => Some(("prop-bp3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        106,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP4: alloc accounting exact — 2k iterations.
        "prop-bp4" => Some(("prop-bp4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        107,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP5: endpoint ownership — 150 kill/respawn cycles.
        "prop-bp5-victim" => Some(("prop-bp5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-bp5" => Some(("prop-bp5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        108,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP6: queue invariants — 2k iterations (self-referential, like P6).
        "prop-bp6" => Some(("prop-bp6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["prop-bp6"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        109,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP7: TLB shootdown proxy — 150 kill/respawn cycles.
        "prop-bp7-victim" => Some(("prop-bp7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-bp7" => Some(("prop-bp7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        110,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP8: restart + higher-generation liveness — 20 iterations.
        "prop-bp8-victim" => Some(("prop-bp8-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-bp8" => Some(("prop-bp8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        111,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP9: generation invalidates ALL 3 slots, over 10 kill/respawn cycles.
        "prop-bp9-victim" => Some(("prop-bp9-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-bp9" => Some(("prop-bp9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["prop-bp9-victim", "prop-bp9-victim", "prop-bp9-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        112,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP10: every send returns a defined outcome — 100k iterations.
        "prop-bp10" => Some(("prop-bp10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        113,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Fuzz-test probes — Milestone 10.
        // Recv-endpoint victims must be listed before their fuzz controllers.
        // ----------------------------------------------------------------
        "fuzz-f1" => Some(("fuzz-f1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        30, // FUZZ_F1: random syscall args
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f2" => Some(("fuzz-f2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        31, // FUZZ_F2: random syscall numbers
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F5: IPC message body fuzzing — recv target first.
        "fuzz-f5-recv" => Some(("fuzz-f5-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — soaks up random messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f5" => Some(("fuzz-f5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f5-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        32, // FUZZ_F5: random IPC message bodies
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F6: embedded cap fuzzing — recv target first.
        "fuzz-f6-recv" => Some(("fuzz-f6-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — receives (or rejects) cap-embedded messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f6" => Some(("fuzz-f6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f6-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        33, // FUZZ_F6: random embedded cap slot indices
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F7: stale cap / generation fuzzing — victim first.
        "fuzz-f7-victim" => Some(("fuzz-f7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed/respawned by fuzz-f7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f7" => Some(("fuzz-f7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f7-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        34, // FUZZ_F7: stale-cap sends after kill
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F8: memory request size fuzzing — no peers needed.
        "fuzz-f8" => Some(("fuzz-f8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        35, // FUZZ_F8: edge-case + random memory request sizes
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal fuzz test probes — Milestone 17.
        // Victims/recv-endpoints before controllers.
        // ----------------------------------------------------------------
        "fuzz-bf5-recv" => Some(("fuzz-bf5-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // passive recv sink
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf5" => Some(("fuzz-bf5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf5-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        116, // FUZZ_BF5: random IPC bodies — 5k sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf6-recv" => Some(("fuzz-bf6-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // passive recv sink
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf6" => Some(("fuzz-bf6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf6-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        117, // FUZZ_BF6: random cap slots — 5k SendWithCap
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf7-victim" => Some(("fuzz-bf7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // passive recv — killed/respawned by fuzz-bf7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf7" => Some(("fuzz-bf7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf7-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        118, // FUZZ_BF7: stale cap — 200 kill/respawn cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf1" => Some(("fuzz-bf1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        114, // FUZZ_BF1: syscall args — 500 × 10 calls
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf2" => Some(("fuzz-bf2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        115, // FUZZ_BF2: syscall numbers — 200k random
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf8" => Some(("fuzz-bf8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        119, // FUZZ_BF8: memory sizes — 10 edge + 5k random
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Stress-test probes — Milestone 11 Phase 1.
        // Recv-endpoint victims must be listed before their controllers.
        // ----------------------------------------------------------------
        // S1: IPC saturation. Receiver is passive (never drains).
        "stress-s1-recv" => Some(("stress-s1-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — queue fills; stress-s1 measures saturation
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s1" => Some(("stress-s1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-s1-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        40, // STRESS_S1: 10,000 try_send under saturation
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S2: Restart storm. Victim killed/respawned 50 times.
        "stress-s2-victim" => Some(("stress-s2-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed/respawned by stress-s2
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s2" => Some(("stress-s2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-s2-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        41, // STRESS_S2: 50 kill/respawn cycles; kstack-leak check
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S3: Cross-core thrash. Receiver pinned to core 1, sender to core 0.
        "stress-s3-recv" => Some(("stress-s3-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: sender on 0, receiver on 1
            probe_mode:        43, // STRESS_S3_RECV: drain 500 cross-core messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s3-send" => Some(("stress-s3-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-s3-recv"],
            send_peers_grant:  false,
            preferred_core:    0, // cross-core: sender on 0, receiver on 1
            probe_mode:        42, // STRESS_S3_SEND: 500 blocking sends to core 1
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S4: Cap table churn. Victim killed/respawned 50×; 2 cap slots verified dead each kill.
        "stress-s4-victim" => Some(("stress-s4-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed/respawned by stress-s4
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s4" => Some(("stress-s4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            // Two SEND caps to the same endpoint — both must die on one kill (§7.5).
            send_peers:        &["stress-s4-victim", "stress-s4-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        44, // STRESS_S4: 50 kill/respawn cycles; 2-cap dead check
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S7: Memory pressure. Single probe; no peers needed.
        "stress-s7" => Some(("stress-s7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        45, // STRESS_S7: 100 alloc-to-limit passes
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S10: Cascading revocation. Victim on core 1; coordinator on core 0 (cross-core kill).
        "stress-s10-victim" => Some(("stress-s10-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: coordinator on 0 kills victim on 1
            probe_mode:        0, // PASSIVE — killed by stress-s10
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s10" => Some(("stress-s10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            // Three SEND caps to the same endpoint — all must die on one kill (§7.5, §8.6).
            send_peers:        &["stress-s10-victim", "stress-s10-victim", "stress-s10-victim"],
            send_peers_grant:  false,
            preferred_core:    0, // cross-core: coordinator on 0, victim on 1
            probe_mode:        46, // STRESS_S10: kill victim; verify 3 caps all EndpointDead
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S5: Generation counter integrity (1000 kill/respawn cycles)
        "stress-s5-victim" => Some(("stress-s5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed by stress-s5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s5" => Some(("stress-s5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        47, // STRESS_S5: 1000 kill/respawn; generation strictly monotonic
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S6: Long-running IPC self-ping stability (5000 rounds)
        "stress-s6" => Some(("stress-s6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["stress-s6"], // self-referential: same endpoint for send+recv
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        48, // STRESS_S6: 5000 self-ping rounds
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S8: Idle scheduler heartbeat (600 yield cycles)
        "stress-s8" => Some(("stress-s8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        49, // STRESS_S8: 600 yields, proves scheduler returns from idle
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S9: Cross-core IPI storm — receiver on core 2; two senders on cores 0 and 1
        "stress-s9-recv" => Some(("stress-s9-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // core 2 — distinct from s3/s10 cross-core pairs on cores 0-1
            probe_mode:        51, // STRESS_S9_RECV: drain 1000 messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s9-send-a" => Some(("stress-s9-send-a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-s9-recv"],
            send_peers_grant:  false,
            preferred_core:    0, // cross-core: 0 → 2
            probe_mode:        50, // STRESS_S9_SEND: 500 blocking sends to s9-recv
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s9-send-b" => Some(("stress-s9-send-b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-s9-recv"],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: 1 → 2
            probe_mode:        50, // STRESS_S9_SEND: 500 blocking sends to s9-recv
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal stress-test probes — Milestone 18.
        // Ordering: recv-endpoint victims before controllers; receivers before senders.
        // ----------------------------------------------------------------
        // BS1: IPC saturation, 5× S1.
        "stress-bs1-recv" => Some(("stress-bs1-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — queue fills; bs1 measures saturation
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs1" => Some(("stress-bs1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs1-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        120, // STRESS_BS1: 50k try_send
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS2: restart storm, 4× S2.
        "stress-bs2-victim" => Some(("stress-bs2-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs2" => Some(("stress-bs2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs2-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        121, // STRESS_BS2: 200 kill/respawn cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS3: cross-core thrash, 4× S3. Receiver on core 1, sender on core 0.
        "stress-bs3-recv" => Some(("stress-bs3-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        123, // STRESS_BS3_RECV: drain 2000 cross-core messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs3-send" => Some(("stress-bs3-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs3-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        122, // STRESS_BS3_SEND: 2000 blocking sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS4: cap table churn, 5× S4. Victim before controller; 2 send_peers slots.
        "stress-bs4-victim" => Some(("stress-bs4-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs4" => Some(("stress-bs4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs4-victim", "stress-bs4-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        124, // STRESS_BS4: 50 churn cycles; 2 cap slots
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS5: generation integrity, 5× S5. Victim before controller.
        "stress-bs5-victim" => Some(("stress-bs5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs5" => Some(("stress-bs5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        125, // STRESS_BS5: 5000 kill/respawn; generation monotonic
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS6: self-ping stability, 4× S6. Self-referential send_peers.
        "stress-bs6" => Some(("stress-bs6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["stress-bs6"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        126, // STRESS_BS6: 20000 self-ping rounds
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS7: memory pressure, 5× S7.
        "stress-bs7" => Some(("stress-bs7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        127, // STRESS_BS7: 500 alloc passes
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS8: scheduler heartbeat, 5× S8.
        "stress-bs8" => Some(("stress-bs8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        128, // STRESS_BS8: 3000 yields
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS9: IPI storm, 5× S9. Receiver on core 2; two senders on cores 0, 1.
        "stress-bs9-recv" => Some(("stress-bs9-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2,
            probe_mode:        130, // STRESS_BS9_RECV: drain 5000 msgs
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs9-send-a" => Some(("stress-bs9-send-a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs9-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        129, // STRESS_BS9_SEND: 2500 sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs9-send-b" => Some(("stress-bs9-send-b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs9-recv"],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        129, // STRESS_BS9_SEND: 2500 sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BS10: cascading revocation, 50 cycles. Victim on core 1; controller on core 0.
        "stress-bs10-victim" => Some(("stress-bs10-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0, // PASSIVE
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs10" => Some(("stress-bs10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["stress-bs10-victim", "stress-bs10-victim", "stress-bs10-victim"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        131, // STRESS_BS10: 50 kill/respawn cycles; 3 cap slots
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Performance-benchmark probes — Milestone 12.
        // Sender services are spawned before their echo/recv partners so
        // their endpoints are registered when echo partners wire SEND caps.
        // ----------------------------------------------------------------
        // B1: same-core IPC roundtrip. Sender acquires cap dynamically.
        "perf-b1" => Some(("perf-b1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        60, // PERF_B1: same-core roundtrip sender
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-b1-echo" => Some(("perf-b1-echo", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["perf-b1"],
            send_peers_grant:  false,
            preferred_core:    0, // same core as perf-b1
            probe_mode:        61, // PERF_B1_ECHO: echo messages back
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B2: cross-core IPC roundtrip. Sender on core 0, echo on core 1.
        "perf-b2" => Some(("perf-b2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        62, // PERF_B2: cross-core roundtrip sender
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-b2-echo" => Some(("perf-b2-echo", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["perf-b2"],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: sender on 0, echo on 1
            probe_mode:        63, // PERF_B2_ECHO: echo messages back
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B3: yield floor. No peers needed.
        "perf-b3" => Some(("perf-b3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        64, // PERF_B3: syscall yield floor
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B4: cap validation throughput. Needs recv endpoint to have a cap to query.
        "perf-b4" => Some(("perf-b4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        65, // PERF_B4: cap + generation check throughput
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B5/B6: spawn and restart cost. Victim spawned first so perf-b5 can kill/respawn it.
        "perf-b5-victim" => Some(("perf-b5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed/respawned by perf-b5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-b5" => Some(("perf-b5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        66, // PERF_B5: spawn + restart cost (covers B5 and B6)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B7: cap table insert/remove throughput. Self-referential (acquires SEND cap to self).
        "perf-b7" => Some(("perf-b7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        67, // PERF_B7: cap insert/remove throughput
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B8: allocator throughput. No peers needed.
        "perf-b8" => Some(("perf-b8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        68, // PERF_B8: alloc-4kib throughput
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B9: 4 KiB message copy. Both on core 0 to isolate copy from cross-core routing.
        // Recv partner must be registered before sender's SEND cap is wired.
        "perf-b9-recv" => Some(("perf-b9-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        70, // PERF_B9_RECV: drain large messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-b9" => Some(("perf-b9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["perf-b9-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        69, // PERF_B9: 4 KiB message sender
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // B10: scheduler pick-next cost. No peers needed.
        "perf-b10" => Some(("perf-b10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        71, // PERF_B10: scheduler pick-next cost
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal performance-benchmark probes — Milestone 19 (5× iteration counts).
        // Sender/controller spawned before echo/recv so endpoints register first.
        // ----------------------------------------------------------------
        "perf-bp1" => Some(("perf-bp1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        132, // PERF_BP1: same-core roundtrip sender, 1000 samples
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-bp1-echo" => Some(("perf-bp1-echo", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["perf-bp1"],
            send_peers_grant:  false,
            preferred_core:    0, // same core as perf-bp1
            probe_mode:        133, // PERF_BP1_ECHO
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP2: cross-core roundtrip. Sender on core 0, echo on core 1.
        "perf-bp2" => Some(("perf-bp2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        134, // PERF_BP2: cross-core roundtrip sender, 1000 samples
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-bp2-echo" => Some(("perf-bp2-echo", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["perf-bp2"],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: sender on 0, echo on 1
            probe_mode:        135, // PERF_BP2_ECHO
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP3: yield floor. No peers.
        "perf-bp3" => Some(("perf-bp3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        136, // PERF_BP3: yield floor, 5000 yields
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP4: cap validation. Needs recv endpoint to have a cap to query.
        "perf-bp4" => Some(("perf-bp4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        137, // PERF_BP4: cap + generation check, 50000 checks
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP5/BP6: spawn and restart cost. Victim spawned first.
        "perf-bp5-victim" => Some(("perf-bp5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE — killed/respawned by perf-bp5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-bp5" => Some(("perf-bp5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        138, // PERF_BP5: spawn + restart cost, 50 cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP7: cap table insert/remove. Self-referential.
        "perf-bp7" => Some(("perf-bp7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        139, // PERF_BP7: cap insert/remove, 5000 cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP8: allocator throughput. No peers.
        "perf-bp8" => Some(("perf-bp8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        140, // PERF_BP8: alloc-4kib throughput
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP9: 4 KiB message copy. Both on core 0 to isolate copy from routing overhead.
        "perf-bp9-recv" => Some(("perf-bp9-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        142, // PERF_BP9_RECV: drain 4 KiB messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-bp9" => Some(("perf-bp9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["perf-bp9-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        141, // PERF_BP9: 4 KiB message sender, 1000 sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP10: scheduler pick-next cost. No peers.
        "perf-bp10" => Some(("perf-bp10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        143, // PERF_BP10: scheduler pick-next, 5000 yields
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Adversarial-test probes — Milestone 13.
        // Victim/passive services must be listed before their attackers so
        // their endpoints are registered when the attacker's SEND caps are wired.
        // ----------------------------------------------------------------
        // A1: random cap slots → always Err. No caps needed.
        "adv-a1" => Some(("adv-a1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        80, // ADV_A1: random slot → Err (cap unforgeability)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A2: brute-force slot range → defined errors. No caps needed.
        "adv-a2" => Some(("adv-a2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        81, // ADV_A2: slots 0..=127 + u32::MAX → defined errors
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A3: alloc beyond 4 MiB limit → AllocDenied. Tight memory_limit.
        "adv-a3" => Some(("adv-a3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        82, // ADV_A3: alloc edge cases under 4 MiB cap
            memory_limit:      4 * 1024 * 1024, // 4 MiB — tight limit for the test
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A4: RECV cap used as SEND target → CapInsufficientRights. Has recv endpoint.
        "adv-a4" => Some(("adv-a4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        83, // ADV_A4: RECV cap → try_send → CapInsufficientRights
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A5: TOCTOU — victim must be registered before attacker's SEND cap is wired.
        "adv-a5-victim" => Some(("adv-a5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE — killed by adv-a5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-a5" => Some(("adv-a5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["adv-a5-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        84, // ADV_A5: kill victim then try_send → EndpointDead
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A6: fill own cap table. Has recv endpoint so it can be acquired via name.
        "adv-a6" => Some(("adv-a6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        85, // ADV_A6: acquire_send_cap loop until table full
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A7: timing probe — passive recv target must be registered before sender.
        "adv-a7-recv" => Some(("adv-a7-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE — absorbs timing probe messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-a7" => Some(("adv-a7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["adv-a7-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        86, // ADV_A7: 100 timing sends to passive partner
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A8: tight-loop hog + witness. Both round-robin so preemption is tested.
        "adv-a8" => Some(("adv-a8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        87, // ADV_A8: tight loop attempting monopoly
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-a8-witness" => Some(("adv-a8-witness", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        88, // ADV_A8_WITNESS: 1000 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A9: spawn non-existent service → Err. No caps needed beyond spawn (always present).
        "adv-a9" => Some(("adv-a9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        89, // ADV_A9: spawn unknown service → Err
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A10: kernel addresses as syscall buffer args → rejected. No caps needed.
        "adv-a10" => Some(("adv-a10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        90, // ADV_A10: kernel-addr syscall args → rejected
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Chaos-test probes — Milestone 14.
        // Victim/passive services must be listed before their controllers so
        // their endpoints are registered when the controllers' SEND caps are wired.
        // ----------------------------------------------------------------
        // C2: null-deref → page fault → killed. Monitor on separate round-robin core.
        "chaos-c2" => Some(("chaos-c2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-c2-monitor" => Some(("chaos-c2-monitor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        92, // CHAOS_C2_MON: 1,000 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C3: alloc saturation. Tight 4 MiB limit so impossible requests are denied quickly.
        "chaos-c3" => Some(("chaos-c3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        93, // CHAOS_C3: 500 alloc-deny cycles without panic
            memory_limit:      4 * 1024 * 1024, // 4 MiB — tight limit for the test
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C5: kernel stack depth probe. No peers needed.
        "chaos-c5" => Some(("chaos-c5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        94, // CHAOS_C5: 100-level recursive yield_cpu() depth probe
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C6: hog on core 3 (simulates timer-starved core) + monitor on core 0.
        "chaos-c6-hog" => Some(("chaos-c6-hog", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3, // core 3 — simulates one starved core
            probe_mode:        7, // MODE_HOG: tight loop (reused)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-c6-monitor" => Some(("chaos-c6-monitor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0, // core 0 — cross-core witness: proves core 0 alive
            probe_mode:        95, // CHAOS_C6_MON: 200 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C7: cross-core kill/respawn TLB-shootdown stress.
        // Victim on core 2 must be registered before controller on core 1 gets SEND cap.
        "chaos-c7-victim" => Some(("chaos-c7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core: controller on 1 kills victim on 2
            probe_mode:        0, // MODE_PASSIVE — killed/respawned by chaos-c7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-c7" => Some(("chaos-c7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["chaos-c7-victim"],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: controller on 1, victim on 2
            probe_mode:        96, // CHAOS_C7: 30 cross-core kill/respawn TLB shootdowns
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal identity test probes — Milestone 15.
        // T11: self-referential queue boundary exactness.
        // T12: cap delegation chain A→B→C.
        // T13: cross-core blocked send wakes with EndpointDead.
        // T-SMP: SMP escalation (smp=2, 8, 16 — run via osdev test identity-brutal).
        // ----------------------------------------------------------------
        "brutal-id-11" => Some(("brutal-id-11", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["brutal-id-11"], // self-referential send peer
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        97,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-12-a" => Some(("brutal-id-12-a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["brutal-id-12-b"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        98,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-12-b" => Some(("brutal-id-12-b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["brutal-id-12-c"], // B forwards to C
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        99,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-12-c" => Some(("brutal-id-12-c", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        100,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-13-recv" => Some(("brutal-id-13-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core target: sender on 0, killer on 1, recv on 2
            probe_mode:        101,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-13-send" => Some(("brutal-id-13-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["brutal-id-13-recv"],
            send_peers_grant:  false,
            preferred_core:    0, // fills queue then blocks — must be on different core than recv
            probe_mode:        102,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-13-kill" => Some(("brutal-id-13-kill", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // yields then kills recv on core 2
            probe_mode:        103,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal adversarial test probes — Milestone 20.
        // Victim/passive services must be listed before their attackers so
        // their endpoints are registered when the attacker's SEND caps are wired.
        // ----------------------------------------------------------------
        // BA1: 50k random cap forgery attempts (5× A1). No caps needed.
        "adv-ba1" => Some(("adv-ba1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        144, // MODE_ADV_BA1: 50k random slot → Err
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA2: extended brute-force slots 0..=511 + 4 extreme values.
        "adv-ba2" => Some(("adv-ba2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        145, // MODE_ADV_BA2: 512 + extreme slot sweep
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA3: 5× alloc edge-case cycles. Tight 4 MiB limit so impossible requests fail fast.
        "adv-ba3" => Some(("adv-ba3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        146, // MODE_ADV_BA3: 5× alloc edge cycles
            memory_limit:      4 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA4: RECV cap used as SEND target × 5. Needs own recv endpoint.
        "adv-ba4" => Some(("adv-ba4", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        147, // MODE_ADV_BA4: RECV-cap-as-SEND → CapInsufficientRights
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA5: 5 TOCTOU kill+send cycles. Victim registered before attacker.
        "adv-ba5-victim" => Some(("adv-ba5-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE — killed/re-killed by adv-ba5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-ba5" => Some(("adv-ba5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["adv-ba5-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        148, // MODE_ADV_BA5: 5× kill+try_send → EndpointDead
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA6: fill own cap table × 5 cycles. Needs recv endpoint so acquire_send_cap("adv-ba6") works.
        "adv-ba6" => Some(("adv-ba6", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        149, // MODE_ADV_BA6: 5× cap-table fill → None without panic
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA7: 500 timing samples (5× A7). Passive recv registered before sender.
        "adv-ba7-recv" => Some(("adv-ba7-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE — absorbs timing probe messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-ba7" => Some(("adv-ba7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["adv-ba7-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        150, // MODE_ADV_BA7: 500 timing sends to passive partner
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA8: tight-loop hog + witness (5× A8). Pinned to core 3 to avoid
        // starving IPC/yield probes on cores 0-2 under QEMU TCG.
        "adv-ba8" => Some(("adv-ba8", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3,
            probe_mode:        151, // MODE_ADV_BA8: tight loop hog
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-ba8-witness" => Some(("adv-ba8-witness", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3,
            probe_mode:        152, // MODE_ADV_BA8_WITNESS: 5000 yields alongside hog
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA9: 5 direct-spawn bypass attempts with bogus names → Err.
        "adv-ba9" => Some(("adv-ba9", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        153, // MODE_ADV_BA9: spawn unknown → Err
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BA10: 20 kernel-space address patterns as syscall args (5× A10). No caps needed.
        "adv-ba10" => Some(("adv-ba10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        154, // MODE_ADV_BA10: kernel addr syscall args → rejected
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Brutal chaos-test services — Milestone 21.
        // BC2: 5 simultaneous null-deref faulters + 1 monitor proving system survival.
        // ----------------------------------------------------------------
        "chaos-bc2-a" => Some(("chaos-bc2-a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // MODE_CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc2-b" => Some(("chaos-bc2-b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // MODE_CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc2-c" => Some(("chaos-bc2-c", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // MODE_CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc2-d" => Some(("chaos-bc2-d", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // MODE_CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc2-e" => Some(("chaos-bc2-e", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        91, // MODE_CHAOS_C2: null-deref → page fault → killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc2-monitor" => Some(("chaos-bc2-monitor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        155, // MODE_CHAOS_BC2_MON: 500 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BC3: 2,500 alloc-deny cycles. Tight 4 MiB limit so impossible requests fail fast.
        "chaos-bc3" => Some(("chaos-bc3", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        156, // MODE_CHAOS_BC3: 2,500 alloc-deny cycles
            memory_limit:      4 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BC5: 500-level recursive yield_cpu() stack depth probe.
        "chaos-bc5" => Some(("chaos-bc5", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        157, // MODE_CHAOS_BC5: 500-level recursive yield
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BC6: 2 hogs on cores 2+3, monitor on core 0 runs 1,000 yields.
        "chaos-bc6-hog-a" => Some(("chaos-bc6-hog-a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // tight-loop hog on core 2
            probe_mode:        7, // MODE_HOG: tight loop (reused)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc6-hog-b" => Some(("chaos-bc6-hog-b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3, // tight-loop hog on core 3
            probe_mode:        7, // MODE_HOG: tight loop (reused)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc6-monitor" => Some(("chaos-bc6-monitor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0, // core 0 — cross-core witness
            probe_mode:        158, // MODE_CHAOS_BC6_MON: 1,000 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BC7: 150 cross-core kill/respawn TLB-shootdown cycles.
        // Victim on core 2 must be registered before controller on core 1 gets SEND cap.
        "chaos-bc7-victim" => Some(("chaos-bc7-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core: controller on 1 kills victim on 2
            probe_mode:        0, // MODE_PASSIVE — killed/respawned by chaos-bc7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc7" => Some(("chaos-bc7", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["chaos-bc7-victim"],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: controller on 1, victim on 2
            probe_mode:        159, // MODE_CHAOS_BC7: 150 cross-core kill/respawn cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "observe" => Some(("observe", ServiceConfig {
            elf:               include_bytes!(env!("SVC_OBSERVE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "shell" => Some(("shell", ServiceConfig {
            elf:               include_bytes!(env!("SVC_SHELL_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  true,
        })),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public spawn API.
// ---------------------------------------------------------------------------

/// Spawn a named service by looking up its ELF and configuration.
///
/// Core placement:
/// - If `core_override` is `Some(n)`, spawn on core `n` (§9.2 strict rule).
/// - Otherwise, use `ServiceConfig::preferred_core`; u32::MAX = round-robin
///   across ready cores.
pub fn spawn_service_by_name(name: &str, core_override: Option<u32>) -> Result<(), SpawnError> {
    let (static_name, cfg) = service_config(name).ok_or(SpawnError::NotFound)?;

    let core_id = match core_override {
        Some(n) => n,
        None if cfg.preferred_core == u32::MAX => {
            // Round-robin across ready cores.
            let count = crate::smp::core::ready_count() as u32;
            if count == 0 { 0 } else {
                use core::sync::atomic::{AtomicU32, Ordering};
                static RR: AtomicU32 = AtomicU32::new(0);
                RR.fetch_add(1, Ordering::Relaxed) % count
            }
        }
        None => {
            let p = cfg.preferred_core;
            if crate::smp::core::is_ready(p) {
                p
            } else {
                // Preferred core not available (degraded SMP); fall back to
                // round-robin across ready cores so the probe still runs.
                let count = crate::smp::core::ready_count() as u32;
                use core::sync::atomic::{AtomicU32, Ordering};
                static RR_FALLBACK: AtomicU32 = AtomicU32::new(0);
                RR_FALLBACK.fetch_add(1, Ordering::Relaxed) % count.max(1)
            }
        }
    };

    let result = spawn_service_with_config(static_name, cfg.elf, core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit, cfg.hw_irqs,
                              cfg.has_console_read);
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn '{}' failed: {:?}", name, e);
    }
    result
}

/// Low-level spawn: load ELF, wire caps, enqueue on `core_id`.
fn spawn_service_with_config(
    name:              &'static str,
    elf_bytes:         &[u8],
    core_id:           u32,
    has_recv_endpoint: bool,
    send_peers:        &[&str],
    probe_mode:        u32,
    send_peers_grant:  bool,
    memory_limit:      u64,
    hw_irqs:           &[u8],
    has_console_read:  bool,
) -> Result<(), SpawnError> {
    // DIAG: step markers to narrow bare-metal freeze after "registry spawned OK"
    crate::kprintln!("spawn[elf]: '{}'", name);

    // 1. Parse ELF.
    let crate::loader::LoadedElf { mut page_table, entry_va } =
        crate::loader::load(elf_bytes)?;

    crate::kprintln!("spawn[stack]: '{}'", name);

    // 2. Map user stack.
    let stack_flags = PageFlags::PRESENT | PageFlags::USER
                    | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    {
        let mut va = USER_STACK_BASE;
        while va < USER_STACK_TOP {
            let frame = alloc_frame().ok_or(SpawnError::NoMemory)?;
            let phys  = frame.phys_addr().0;
            // SAFETY: phys from allocator; HHDM covers all usable memory.
            unsafe {
                core::ptr::write_bytes(
                    (get_hhdm_offset() + phys) as *mut u8,
                    0,
                    PAGE_SIZE,
                );
            }
            page_table
                .map(VirtAddr(va), PhysAddr(phys), stack_flags)
                .map_err(|_| SpawnError::MapFailed)?;
            core::mem::forget(frame);
            va += PAGE_SIZE as u64;
        }
    }

    crate::kprintln!("spawn[slot]: '{}'", name);

    // 3. Reserve a task slot and initialise its CapTable directly in BSS.
    let task_slot = scheduler::reserve_task_slot(core_id).ok_or(SpawnError::NoMemory)?;
    crate::kprintln!("spawn[caps]: '{}' slot={}", name, task_slot);
    // SAFETY: task_slot was just reserved; IF=0 in syscall context.
    let caps = unsafe { scheduler::task_cap_init_empty(task_slot) };

    // Slot 0: log_write (always present in v1).
    caps.insert(mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE))
        .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    // Slot 1: spawn authority (every service in v1).
    caps.insert(mint_cap(SPAWN_RESOURCE, Rights::WRITE))
        .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;

    // 4. Optional recv endpoint.
    let mut recv_slot_u32 = u32::MAX;
    let mut own_endpoint:  Option<EndpointId> = None;

    if has_recv_endpoint {
        let ep_id       = crate::ipc::alloc_endpoint_id();
        let resource_id = ResourceId::from(ep_id);

        // For respawns: inherit the old (killed) endpoint's generation so the new
        // endpoint's generation is strictly greater than any previously-issued cap
        // generation for this name — making generation monotonic across kill/respawn (§7.5 P2).
        //
        // We read from GLOBAL_RESOURCES (capability table) rather than the routing
        // table because routing entries are recycled by concurrent spawns: by the
        // time this spawn runs, another service may have claimed the old dead routing
        // slot and overwritten its generation.  GLOBAL_RESOURCES entries are
        // persistent (append-only) so the old dead entry with the bumped generation
        // remains visible until GLOBAL_RESOURCES fills up (capacity: 4096).
        let start_gen = crate::ipc::names::lookup(name)
            .and_then(|old_ep| {
                let old_rid = crate::capability::cap::ResourceId::from(old_ep);
                crate::capability::get_resource_generation(old_rid)
            })
            .unwrap_or(Generation::INITIAL);

        // Register in global cap table at the inherited generation.
        crate::capability::register_resource_at_gen(resource_id, start_gen);

        // Register in routing table at the same generation.
        crate::ipc::routing::register(ep_id, core_id, start_gen);

        // Publish name → endpoint mapping for peer cap resolution.
        crate::ipc::names::register(name, ep_id);

        // Mint RECV cap → first free slot (= slot 2).
        let recv_cap = mint_cap(resource_id, Rights::RECV);
        let cap_slot = caps.insert(recv_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
        recv_slot_u32 = cap_slot as u32;
        own_endpoint  = Some(ep_id);

        // Wire hw_interrupt lines to this endpoint (§12.3).
        for &irq in hw_irqs {
            crate::interrupt::route::register(irq, ep_id);
        }
    }

    // 4b. Optional CONSOLE_READ cap (shell service only).
    let mut console_read_slot_u32 = u32::MAX;
    if has_console_read {
        let cr_cap = mint_cap(CONSOLE_READ_RESOURCE, Rights::READ);
        let cap_slot = caps.insert(cr_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
        console_read_slot_u32 = cap_slot as u32;
    }

    // 5. Send-peer SEND caps (wired at spawn from the name registry).
    let mut peer_data: [(u32, u32, [u8; PEER_NAME_BYTES]); MAX_SEND_PEERS] =
        [(u32::MAX, 0, [0u8; PEER_NAME_BYTES]); MAX_SEND_PEERS];
    let mut peer_count = 0usize;

    for &peer_name in send_peers {
        if peer_count >= MAX_SEND_PEERS { break; }

        if let Some(peer_ep_id) = crate::ipc::names::lookup(peer_name) {
            let peer_resource_id = ResourceId::from(peer_ep_id);
            let peer_rights = if send_peers_grant {
                Rights::SEND | Rights::GRANT
            } else {
                Rights::SEND
            };
            let send_cap = mint_cap(peer_resource_id, peer_rights);
            match caps.insert(send_cap) {
                Ok(cap_slot) => {
                    let nb  = peer_name.as_bytes();
                    let len = nb.len().min(PEER_NAME_BYTES);
                    peer_data[peer_count].0 = cap_slot as u32;
                    peer_data[peer_count].1 = len as u32;
                    peer_data[peer_count].2[..len].copy_from_slice(&nb[..len]);
                    peer_count += 1;
                }
                Err(_) => crate::kprintln!(
                    "task: cap table full, skipping SEND cap to '{}' for '{}'",
                    peer_name, name
                ),
            }
        } else {
            crate::kprintln!(
                "task: peer '{}' not yet registered, no SEND cap for '{}'",
                peer_name, name
            );
        }
    }

    // 6. Allocate and map the ServiceContextData page.
    {
        let ctx_frame = alloc_frame().ok_or(SpawnError::NoMemory)?;
        let ctx_phys  = ctx_frame.phys_addr().0;
        // SAFETY: phys from allocator; task hasn't started yet; HHDM covers it.
        unsafe {
            let virt = (get_hhdm_offset() + ctx_phys) as *mut u8;
            core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            let data = &mut *(virt as *mut ServiceContextData);
            data.magic              = SERVICE_CTX_MAGIC;
            // Readback: confirm write was not silently dropped (should always pass).
            if data.magic != SERVICE_CTX_MAGIC {
                crate::arch::x86_64::serial_write_bytes_lockfree(b"CTX-MAGIC-MISMATCH\n");
            }
            data.log_write_slot     = 0;
            data.recv_slot          = recv_slot_u32;
            data.spawn_slot         = 1;
            data.send_peer_count    = peer_count as u32;
            data.core_id            = core_id;
            data.probe_mode         = probe_mode;
            data.console_read_slot  = console_read_slot_u32;
            for i in 0..peer_count {
                data.send_peers[i].slot     = peer_data[i].0;
                data.send_peers[i].name_len = peer_data[i].1;
                data.send_peers[i].name     = peer_data[i].2;
            }
        }
        let ctx_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::NO_EXEC;
        page_table
            .map(VirtAddr(SERVICE_CTX_VA), PhysAddr(ctx_phys), ctx_flags)
            .map_err(|_| SpawnError::MapFailed)?;
        core::mem::forget(ctx_frame);
    }

    crate::kprintln!("spawn[kstack]: '{}'", name);

    // 7. Kernel stack.
    let kstack_top = alloc_kstack().ok_or(SpawnError::NoMemory)?;
    crate::kprintln!("spawn[commit]: '{}' kstack ok", name);

    // 8. Initial ring-3 context.
    let cr3 = page_table.into_cr3();
    // SAFETY: kstack_top is valid kernel memory; entry_va and USER_STACK_TOP
    // are valid ring-3 addresses in the new page table.
    let ctx = unsafe {
        TaskContext::new_user(kstack_top, entry_va, USER_STACK_TOP, cr3)
    };

    // 9. Finalise the reserved task slot (ctx + metadata → Ready).
    // SAFETY: task_slot reserved above; CapTable initialised; IF=0.
    unsafe {
        scheduler::commit_task(task_slot, name, ctx, true, kstack_top as u64, own_endpoint);
    }

    // 10. Initialise the memory budget for this task (§10.3).
    scheduler::set_task_memory_budget(task_slot, memory_limit);

    crate::kprintln!("task: '{}' spawned OK on core {} (slot {})", name, core_id, task_slot);
    Ok(())
}

/// Spawn `init` on Core 0. Called once by `kernel_main` (§11.1).
pub fn spawn_init() {
    let elf_bytes = include_bytes!(env!("SVC_INIT_ELF"));
    match spawn_service_with_config("init", elf_bytes, 0, false, &[], 0, false, 64 * 1024 * 1024, &[], false) {
        Ok(()) => crate::kprintln!("task: init spawned on core 0"),
        Err(e) => panic!("task: failed to spawn init: {:?}", e),
    }
}

/// Kill all running tasks with the given name.
///
/// Loops until no live task with `name` remains, so duplicate instances
/// (e.g. from a spurious early-boot spawn) are all killed before respawn.
/// Marks each task Dead, kills its endpoint, and marks the resource dead.
pub fn kill_by_name(name: &str) -> bool {
    let mut found = false;
    while let Some(slot) = scheduler::find_task_by_name(name) {
        scheduler::kill_task_by_slot(slot);
        found = true;
    }
    found
}

/// Kill the currently-running task (called from page-fault handler — §10.3).
pub fn kill_current() {
    let slot = scheduler::current_task_slot();
    if slot < scheduler::MAX_TASKS {
        scheduler::kill_task_by_slot(slot);
    }
    // Reschedule — kill_task_by_slot already sets state to Dead; the scheduler
    // will skip this task on the next pick_next pass.
    scheduler::yield_current();
}
