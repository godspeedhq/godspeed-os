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

#[repr(C, align(16))]
struct KernelStackStorage {
    data: [u8; KSTACK_SIZE * TASK_KSTACK_MAX],
}

static mut KSTACK_STORAGE: KernelStackStorage =
    KernelStackStorage { data: [0u8; KSTACK_SIZE * TASK_KSTACK_MAX] };

static mut KSTACK_USED: [bool; TASK_KSTACK_MAX] = [false; TASK_KSTACK_MAX];

fn alloc_kstack() -> Option<*mut u8> {
    for i in 0..TASK_KSTACK_MAX {
        // SAFETY: single-core at spawn time; no concurrent modifications.
        if !unsafe { KSTACK_USED[i] } {
            unsafe { KSTACK_USED[i] = true; }
            // SAFETY: i < TASK_KSTACK_MAX; base + KSTACK_SIZE within the array.
            let top = unsafe {
                KSTACK_STORAGE
                    .data
                    .as_mut_ptr()
                    .add(i * KSTACK_SIZE + KSTACK_SIZE)
            };
            return Some(top);
        }
    }
    None
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
    _pad:            [u32; 3],
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

struct ServiceConfig {
    elf:               &'static [u8],
    has_recv_endpoint: bool,
    /// Names of services this one needs to send to.
    send_peers:        &'static [&'static str],
    /// Preferred core; u32::MAX = round-robin.
    preferred_core:    u32,
}

fn service_config(name: &str) -> Option<(&'static str, ServiceConfig)> {
    match name {
        "supervisor" => Some(("supervisor", ServiceConfig {
            elf:               include_bytes!(env!("SVC_SUPERVISOR_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            preferred_core:    0,
        })),
        "registry" => Some(("registry", ServiceConfig {
            elf:               include_bytes!(env!("SVC_REGISTRY_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            preferred_core:    0,
        })),
        "logger" => Some(("logger", ServiceConfig {
            elf:               include_bytes!(env!("SVC_LOGGER_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            preferred_core:    0,
        })),
        "ping" => Some(("ping", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PING_ELF")),
            has_recv_endpoint: true,
            send_peers:        &["pong", "registry"],
            preferred_core:    0,
        })),
        "pong" => Some(("pong", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PONG_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            preferred_core:    1,
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

    spawn_service_with_config(static_name, cfg.elf, core_id,
                              cfg.has_recv_endpoint, cfg.send_peers)
}

/// Low-level spawn: load ELF, wire caps, enqueue on `core_id`.
fn spawn_service_with_config(
    name:              &'static str,
    elf_bytes:         &[u8],
    core_id:           u32,
    has_recv_endpoint: bool,
    send_peers:        &[&str],
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
    let task_slot = scheduler::reserve_task_slot(core_id)
        .ok_or(SpawnError::NoMemory)?;
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
            let send_cap         = mint_cap(peer_resource_id, Rights::SEND);
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

    Ok(())
}

/// Spawn `init` on Core 0. Called once by `kernel_main` (§11.1).
pub fn spawn_init() {
    let elf_bytes = include_bytes!(env!("SVC_INIT_ELF"));
    match spawn_service_with_config("init", elf_bytes, 0, false, &[]) {
        Ok(()) => crate::kprintln!("task: init spawned on core 0"),
        Err(e) => panic!("task: failed to spawn init: {:?}", e),
    }
}

/// Kill a running task by name.
///
/// Marks the task Dead, kills its endpoint (bumps generation, wakes blocked
/// tasks), and marks the resource dead in the capability table.
pub fn kill_by_name(name: &str) -> bool {
    if let Some(slot) = scheduler::find_task_by_name(name) {
        scheduler::kill_task_by_slot(slot);
        true
    } else {
        false
    }
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
