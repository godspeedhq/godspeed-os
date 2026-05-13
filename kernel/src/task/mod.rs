//! Task management — §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

use crate::arch::x86_64::context_switch::TaskContext;
use crate::arch::x86_64::page_tables::{
    get_hhdm_offset, PageFlags, VirtAddr, PAGE_SIZE,
};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, SPAWN_RESOURCE};
use crate::capability::cap::ResourceId;
use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::memory::allocator::alloc_frame;
use crate::memory::frame::PhysAddr;

// ---------------------------------------------------------------------------
// Kernel stack pool — one 64 KiB stack per ring-3 task (§14.1).
// ---------------------------------------------------------------------------

const TASK_KSTACK_MAX: usize = 32;
const KSTACK_SIZE:     usize = 64 * 1024;

// Magic value written at the BOTTOM of each kstack slot (byte offset 0 within
// the slot) when it is in use.  Stacks grow downward from the slot's top, so
// the bottom bytes are never touched by normal execution.
//
// This replaces a separate KSTACK_USED: [bool; 32] array, which was colliding
// with another BSS static and becoming corrupted at runtime.
const KSTACK_MAGIC_USED: u32 = 0xCA11_CA11;

#[repr(C, align(16))]
struct KernelStackStorage {
    data: [u8; KSTACK_SIZE * TASK_KSTACK_MAX],
}

static mut KSTACK_STORAGE: KernelStackStorage =
    KernelStackStorage { data: [0u8; KSTACK_SIZE * TASK_KSTACK_MAX] };

/// Read the in-use marker stored at the bottom of kstack slot `i`.
#[inline]
unsafe fn kstack_marker(i: usize) -> *mut u32 {
    // SAFETY: i < TASK_KSTACK_MAX; first 4 bytes of each slot are the marker.
    unsafe { KSTACK_STORAGE.data.as_mut_ptr().add(i * KSTACK_SIZE) as *mut u32 }
}

fn alloc_kstack() -> Option<*mut u8> {
    for i in 0..TASK_KSTACK_MAX {
        // SAFETY: marker pointer is within KSTACK_STORAGE; single-writer.
        if unsafe { kstack_marker(i).read_volatile() } != KSTACK_MAGIC_USED {
            // SAFETY: same as above.
            unsafe { kstack_marker(i).write_volatile(KSTACK_MAGIC_USED); }
            // Return pointer to the TOP of this slot (stacks grow down).
            // SAFETY: i < TASK_KSTACK_MAX; offset is within the array bounds.
            let top = unsafe {
                KSTACK_STORAGE
                    .data
                    .as_mut_ptr()
                    .add(i * KSTACK_SIZE + KSTACK_SIZE)
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
    // SAFETY: idx is within [0, TASK_KSTACK_MAX); single-writer kill path (IF=0).
    unsafe { kstack_marker(idx).write_volatile(0); }
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
    magic:           u32,
    log_write_slot:  u32,
    recv_slot:       u32,
    spawn_slot:      u32,
    send_peer_count: u32,
    core_id:         u32,
    probe_mode:      u32,
    _pad:            u32,
    send_peers:      [SendPeerEntry; MAX_SEND_PEERS],
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
        })),
        "registry" => Some(("registry", ServiceConfig {
            elf:               REGISTRY_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
        })),
        "logger" => Some(("logger", ServiceConfig {
            elf:               include_bytes!(env!("SVC_LOGGER_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
        })),
        "ping" => Some(("ping", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PING_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["pong", "registry"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
        })),
        "pong" => Some(("pong", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PONG_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
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
        })),
        "probe-victim" => Some(("probe-victim", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE — killed by probe-4a in Test 4A
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-4b-recv" => Some(("probe-4b-recv", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE — killed by harness in Test 4B
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-3b" => Some(("probe-3b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        3, // MODE_NO_SEND_RIGHT — Test 3B
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-sender" => Some(("probe-sender", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        2, // MODE_ECHO_SEND — Test 3A
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-4a" => Some(("probe-4a", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-victim"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        4, // MODE_SEND_AFTER_KILL — Test 4A
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-4b-send" => Some(("probe-4b-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-4b-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        5, // MODE_FILL_AND_BLOCK — Test 4B
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-yielder" => Some(("probe-yielder", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        6, // MODE_YIELD_LOGGER — Test 8A
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-hog" => Some(("probe-hog", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        7, // MODE_HOG — Test 8B (preemption proven via ping)
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-9b" => Some(("probe-9b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        8, // MODE_CAP_FORGE — Test 9B
            memory_limit:      64 * 1024 * 1024,
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
        })),
        "probe-5a-send" => Some(("probe-5a-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  true,  // mints SEND|GRANT cap to probe-5a-recv
            preferred_core:    0,
            probe_mode:        10, // MODE_GRANT_SEND — Test 5A sender
            memory_limit:      64 * 1024 * 1024,
        })),
        "probe-5b-send" => Some(("probe-5b-send", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  false, // SEND only — no GRANT right; should return CapNotGrantable
            preferred_core:    0,
            probe_mode:        11, // MODE_NO_GRANT_SEND — Test 5B negative
            memory_limit:      64 * 1024 * 1024,
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
        })),
        "probe-7b" => Some(("probe-7b", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        13, // MODE_ALLOC_LIMIT — Test 7B
            memory_limit:      64 * 1024 * 1024,
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
        })),
        "prop-p1" => Some(("prop-p1", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        20, // MODE_PROP_P1
            memory_limit:      64 * 1024 * 1024,
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
        })),
        "prop-p10" => Some(("prop-p10", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PROBE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        22, // MODE_PROP_P10
            memory_limit:      64 * 1024 * 1024,
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
        None => cfg.preferred_core,
    };

    let result = spawn_service_with_config(static_name, cfg.elf, core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit);
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
) -> Result<(), SpawnError> {
    // 1. Parse ELF.
    let crate::loader::LoadedElf { mut page_table, entry_va } =
        crate::loader::load(elf_bytes)?;

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

    // 3. Reserve a task slot and initialise its CapTable directly in BSS.
    let task_slot = scheduler::reserve_task_slot(core_id).ok_or(SpawnError::NoMemory)?;
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

        // Register in global cap table (generation 0).
        crate::capability::table::register_resource(resource_id);

        // Register in routing table.
        crate::ipc::routing::register(ep_id, core_id, Generation::INITIAL);

        // Publish name → endpoint mapping for peer cap resolution.
        crate::ipc::names::register(name, ep_id);

        // Mint RECV cap → first free slot (= slot 2).
        let recv_cap = mint_cap(resource_id, Rights::RECV);
        let cap_slot = caps.insert(recv_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
        recv_slot_u32 = cap_slot as u32;
        own_endpoint  = Some(ep_id);
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
            data.magic           = SERVICE_CTX_MAGIC;
            data.log_write_slot  = 0;
            data.recv_slot       = recv_slot_u32;
            data.spawn_slot      = 1;
            data.send_peer_count = peer_count as u32;
            data.core_id         = core_id;
            data.probe_mode      = probe_mode;
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

    // 7. Kernel stack.
    let kstack_top = alloc_kstack().ok_or(SpawnError::NoMemory)?;

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
    match spawn_service_with_config("init", elf_bytes, 0, false, &[], 0, false, 64 * 1024 * 1024) {
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
