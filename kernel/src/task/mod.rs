//! Task management — §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

use crate::arch::x86_64::context_switch::TaskContext;
use crate::arch::x86_64::page_tables::{
    get_hhdm_offset, PageFlags, VirtAddr, PAGE_SIZE,
};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE};
use crate::capability::table::CapTable;
use crate::memory::allocator::alloc_frame;
use crate::memory::frame::PhysAddr;

// ---------------------------------------------------------------------------
// Kernel stack pool — one 64 KiB stack per ring-3 task (§14.1).
// ---------------------------------------------------------------------------

const TASK_KSTACK_MAX:  usize = 32;
const KSTACK_SIZE:      usize = 64 * 1024;

/// Flat backing store for all kernel stacks. Lives in .bss (zero-init).
/// 16-byte alignment satisfies the SysV AMD64 ABI stack-alignment rule.
#[repr(C, align(16))]
struct KernelStackStorage {
    data: [u8; KSTACK_SIZE * TASK_KSTACK_MAX],
}

static mut KSTACK_STORAGE: KernelStackStorage =
    KernelStackStorage { data: [0u8; KSTACK_SIZE * TASK_KSTACK_MAX] };

static mut KSTACK_USED: [bool; TASK_KSTACK_MAX] = [false; TASK_KSTACK_MAX];

/// Allocate one kernel stack slot. Returns a pointer to the stack top (one-past-end).
///
/// Single-threaded; called only from BSP before APs start.
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
// ServiceContextData page — written by the kernel, read by the SDK (§SDK).
// ---------------------------------------------------------------------------

/// Virtual address of the ServiceContextData page in every service address space.
pub const SERVICE_CTX_VA: u64 = 0x3ff000;
/// Magic value validated by `ServiceContext::ctx_data()` in the SDK.
pub const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;

/// Layout written into the ServiceContextData page before a service is launched.
/// MUST match the definition in `sdk/rust/src/service_context.rs`.
#[repr(C)]
struct ServiceContextData {
    magic:          u32,
    log_write_slot: u32,  // cap slot for LOG_WRITE; u32::MAX = not held
    recv_slot:      u32,  // cap slot for primary recv endpoint; u32::MAX = not held
    _pad:           u32,
}

// ---------------------------------------------------------------------------
// User stack layout constants.
// ---------------------------------------------------------------------------

/// Initial user-space RSP (one-past-end of user stack).
const USER_STACK_TOP:   u64 = 0x8000_0000;
/// Number of 4 KiB pages mapped for the user stack.
const USER_STACK_PAGES: u64 = 4;
/// Base VA of the user stack.
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
}

impl From<crate::loader::LoadError> for SpawnError {
    fn from(e: crate::loader::LoadError) -> Self {
        SpawnError::LoadFailed(e)
    }
}

// ---------------------------------------------------------------------------
// Public spawn API.
// ---------------------------------------------------------------------------

/// Load `elf_bytes`, build a user address space, and enqueue on `core_id`.
///
/// Sets up:
///  - ELF PT_LOAD segments mapped with correct permissions
///  - User stack (USER_STACK_PAGES pages, writable, no-exec)
///  - ServiceContextData page at SERVICE_CTX_VA with cap slot assignments
///  - Cap table: slot 0 = log_write
///  - Kernel stack from the static pool
///
/// Called from BSP before APs start (single-core invariant holds).
pub fn spawn_service(
    name:      &'static str,
    elf_bytes: &[u8],
    core_id:   u32,
) -> Result<(), SpawnError> {
    // 1. Parse ELF and create initial page table with segment mappings.
    let crate::loader::LoadedElf { mut page_table, entry_va } =
        crate::loader::load(elf_bytes)?;

    // 2. Map user stack (writable, no-exec).
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
            core::mem::forget(frame); // owned by page table; freed at task death
            va += PAGE_SIZE as u64;
        }
    }

    // 3. Allocate the ServiceContextData page and write cap slot assignments.
    {
        let ctx_frame = alloc_frame().ok_or(SpawnError::NoMemory)?;
        let ctx_phys  = ctx_frame.phys_addr().0;
        // SAFETY: phys from allocator; HHDM covers it; task hasn't started yet.
        unsafe {
            let virt = (get_hhdm_offset() + ctx_phys) as *mut u8;
            core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            let data = &mut *(virt as *mut ServiceContextData);
            data.magic          = SERVICE_CTX_MAGIC;
            data.log_write_slot = 0;        // slot 0 = log_write (always)
            data.recv_slot      = u32::MAX; // not held in Phase 3
            data._pad           = 0;
        }
        // Map read-only (the service only reads this page).
        let ctx_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::NO_EXEC;
        page_table
            .map(VirtAddr(SERVICE_CTX_VA), PhysAddr(ctx_phys), ctx_flags)
            .map_err(|_| SpawnError::MapFailed)?;
        core::mem::forget(ctx_frame);
    }

    // 4. Mint capabilities for this service.
    let mut caps = CapTable::empty();
    // Slot 0: log_write (every service gets this in v1).
    caps.insert(mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE))
        .map_err(|_| SpawnError::CapTableFull)?;

    // 5. Allocate kernel stack from the static pool.
    let kstack_top = alloc_kstack().ok_or(SpawnError::NoMemory)?;

    // 6. Consume the page table and build the initial ring-3 task context.
    let cr3 = page_table.into_cr3();
    // SAFETY: kstack_top is valid kernel memory from the static pool;
    // entry_va and USER_STACK_TOP are valid ring-3 addresses in the new PT.
    let ctx = unsafe {
        TaskContext::new_user(kstack_top, entry_va, USER_STACK_TOP, cr3)
    };

    // 7. Enqueue on the target core.
    scheduler::enqueue(name, ctx, caps, core_id, true, kstack_top as u64);

    Ok(())
}

/// Spawn the `init` service on Core 0. Called once by `kernel_main` (§11.1).
///
/// Requires that service ELFs are pre-built (`osdev build` handles ordering).
pub fn spawn_init() {
    // NOTE: SVC_INIT_ELF is emitted by kernel/build.rs once `osdev build`
    // compiles services before the kernel.
    let elf_bytes = include_bytes!(env!("SVC_INIT_ELF"));
    match spawn_service("init", elf_bytes, 0) {
        Ok(()) => crate::kprintln!("task: init spawned on core 0"),
        Err(e) => panic!("task: failed to spawn init: {:?}", e),
    }
}

/// Kill the currently-running task (called from page-fault handler — §10.3).
pub fn kill_current() {
    todo!("mark current task dead, notify supervisor, reclaim memory, run scheduler")
}
