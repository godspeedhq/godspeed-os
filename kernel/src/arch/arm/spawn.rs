// SPDX-License-Identifier: GPL-2.0-only
//! The finish line: spawn a real GodspeedOS service and run it to `ready` on ARM.
//!
//! Everything the campaign built converges here. This takes the ARM `logger` ELF (embedded by
//! `build.rs`, built in increment 4), loads it with the neutral loader (5) into a fresh address space
//! (page tables from 3, frames from 1), sets up a task with a `LOG_WRITE` capability, and enters it at
//! PL0 (3). The service runs its own compiled `service_main`, calls `ctx.log("logger: ready")`, which
//! issues a real `svc` (2) into the neutral syscall dispatcher, which validates the capability and
//! writes to the kernel log - and the line appears on the console.
//!
//! It is a **minimal** spawn on purpose: one service, one capability, entered directly rather than
//! through the full scheduler. The neutral `task::spawn` does far more (IPC endpoints, the registry,
//! the supervisor manifest) and carries x86 memory-layout assumptions; wiring all of it is the rest of
//! the port. This proves the essential path end to end - a loaded service runs unprivileged and its
//! syscalls are served - which is what "GodspeedOS runs on ARM" means.

use crate::arch::imp::{BootInfo, MemoryKind, MemoryRegion};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, Capability};
use super::pl011_write;
use super::page_tables::{self, PageFlags, PageTable, VirtAddr};
use crate::memory::frame::PhysAddr;

/// The ARM `logger` ELF, embedded by `build.rs`. Empty placeholder on a not-yet-ported arch.
static LOGGER_ELF: &[u8] = include_bytes!(env!("SVC_LOGGER_ELF"));

// Must match the SDK's ServiceContext layout (sdk/rust/src/service_context.rs) - the kernel writes
// this page, the service reads it. Only the fields the logger touches on startup need real values.
pub(super) const SERVICE_CTX_VA: u32 = 0x003f_f000;
pub(super) const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;
pub(super) const USER_STACK_TOP: u32 = 0x8000_0000;

/// A service loaded into a fresh address space with a task slot reserved and its cap installed, ready
/// either to enter directly (`boot_service`) or to commit to the scheduler (`sched_user`).
pub(super) struct LoadedService {
    pub entry: u32,
    pub pt_root: u32,
    pub slot: usize,
}

/// Bring up the neutral subsystems a spawn needs (per-core arenas, scheduler slots, capability
/// resources). Shared by every ARM spawn path; `boot_info` is only lightly used (percpu ignores it,
/// the allocator is already live), but building a faithful one keeps the neutral init honest.
pub(super) fn neutral_bootstrap(ram_end: u32, reserve_end: u32) {
    static REGIONS: [MemoryRegion; 2] = [
        MemoryRegion { base: 0, len: 0, kind: MemoryKind::Reserved },
        MemoryRegion { base: 0, len: 0, kind: MemoryKind::Usable },
    ];
    // SAFETY: single-threaded boot; REGIONS filled once before percpu_init reads the BootInfo.
    let regions = unsafe {
        let r = core::ptr::addr_of!(REGIONS) as *mut [MemoryRegion; 2];
        (*r)[0] = MemoryRegion { base: 0, len: reserve_end as u64, kind: MemoryKind::Reserved };
        (*r)[1] = MemoryRegion { base: reserve_end as u64, len: (ram_end - reserve_end) as u64, kind: MemoryKind::Usable };
        &*(core::ptr::addr_of!(REGIONS))
    };
    let boot_info = BootInfo {
        memory_map: regions, kernel_phys_start: 0x8000, kernel_phys_end: reserve_end as u64,
        hhdm_offset: 0, rsdp_addr: 0,
    };

    crate::smp::percpu_init(&boot_info);
    crate::task::scheduler::init_arenas(crate::smp::percpu::num_cores());
    crate::capability::init();
    // Mark core 0 ready (the only core the Pi 2 runs; the other three A7s are parked in `_start`). This
    // makes `is_ready(0)` true so placement on core 0 succeeds, while `is_ready(N>0)` stays false
    // (num_cores == 1) - so the supervisor's `spawn_on(x, 1)` is rejected (§9.2) and falls back to
    // core 0 rather than stranding the service on a parked core.
    crate::smp::core::mark_ready(0);
}

/// The service-context virtual address (`SERVICE_CTX_VA`) mapped into `pt`, backed by a fresh frame.
/// Also maps 8 user stack pages below `USER_STACK_TOP`. Returns the ctx frame's physical address (so
/// the caller can write the `ServiceContext` there), or `None` (having logged) on failure.
///
/// Several stack pages because a service builds a 4 KiB IPC message buffer on its stack (`ctx.log`/
/// `recv`/`send`), so one page is not enough (the first run faulted just below a single page).
fn map_stack_and_ctx(pt: &mut PageTable) -> Option<u32> {
    const STACK_PAGES: u32 = 8;
    let uflags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    for p in 1..=STACK_PAGES {
        let f = match crate::memory::allocator::alloc_frame() {
            Some(f) => f.phys_addr().0 as u32,
            None => { pl011_write(b"arm32: spawn FAIL - no frame for user stack\r\n"); return None; }
        };
        let va = USER_STACK_TOP - p * 0x1000;
        if pt.map(VirtAddr(va as u64), PhysAddr(f as u64), uflags).is_err() {
            pl011_write(b"arm32: spawn FAIL - could not map user stack\r\n"); return None;
        }
    }
    let ctx_frame = match crate::memory::allocator::alloc_frame() {
        Some(f) => f.phys_addr().0 as u32,
        None => { pl011_write(b"arm32: spawn FAIL - no frame for service context\r\n"); return None; }
    };
    if pt.map(VirtAddr(SERVICE_CTX_VA as u64), PhysAddr(ctx_frame as u64), uflags).is_err() {
        pl011_write(b"arm32: spawn FAIL - could not map service context\r\n");
        return None;
    }
    Some(ctx_frame)
}

/// A service loaded into a fresh address space with a task slot reserved, its `LOG_WRITE` cap at
/// cap-slot 0, and each `extra_caps` entry inserted at slots 1.. (in order). The service-context page
/// is mapped but **left for the caller to fill** (so an IPC service can wire its recv/send slots), and
/// `fill_kernel_identity` is **not** yet applied - the caller does both, then `clean_invalidate_dcache_all`.
pub(super) struct RawService {
    pub entry: u32,
    pub pt_root: u32,
    pub ctx_frame: u32,
    pub slot: usize,
}

/// Load an arbitrary service ELF into a fresh address space, reserve a task slot, install its
/// `LOG_WRITE` cap (slot 0) and any `extra_caps` (slots 1..). See [`RawService`] for what the caller
/// still owes (ctx write, `fill_kernel_identity`). Returns `None` (having logged) on any failure.
pub(super) fn load_service_raw(elf: &[u8], extra_caps: &[Capability]) -> Option<RawService> {
    if elf.len() < 64 { pl011_write(b"arm32: spawn SKIP - empty service ELF\r\n"); return None; }
    let loaded = match crate::loader::load(elf) {
        Ok(l) => l,
        Err(_) => { pl011_write(b"arm32: spawn FAIL - loader rejected the service ELF\r\n"); return None; }
    };
    let entry = loaded.entry_va as u32;
    let mut pt = loaded.page_table;
    let pt_root = pt.cr3_value() as u32;
    let ctx_frame = map_stack_and_ctx(&mut pt)?;
    let slot = match crate::task::scheduler::reserve_task_slot(0) {
        Some(s) => s,
        None => { pl011_write(b"arm32: spawn FAIL - no free task slot\r\n"); return None; }
    };
    // SAFETY: slot just reserved; single-threaded boot with interrupts effectively quiescent for setup.
    unsafe {
        let caps = crate::task::scheduler::task_cap_init_empty(slot);
        let _ = caps.insert(mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE)); // slot 0
        for c in extra_caps { let _ = caps.insert(*c); }                  // slots 1..
    }
    Some(RawService { entry, pt_root, ctx_frame, slot })
}

/// Load the embedded `logger` ELF and write its (minimal) `ServiceContext`. Thin wrapper over
/// `load_service_raw` for the no-endpoint case: the logger needs only `log_write` + a (dead) `recv`.
pub(super) fn load_logger_into_slot() -> Option<LoadedService> {
    let raw = load_service_raw(LOGGER_ELF, &[])?;
    // Write the ServiceContext the SDK reads. Identity-mapped, so writable at the frame's phys addr.
    // SAFETY: `raw.ctx_frame` is a fresh frame we own; writing the SDK's context struct into it.
    unsafe {
        let p = raw.ctx_frame as *mut u32;
        p.add(0).write_volatile(SERVICE_CTX_MAGIC); // magic
        p.add(1).write_volatile(0);                 // log_write_slot = 0 (the cap inserted at slot 0)
        // Every other slot "not present" (u32::MAX); the logger only needs log_write + recv.
        for i in 2..8 { p.add(i).write_volatile(u32::MAX); }
        // recv_slot (index 2) MAX is fine - ctx.recv() returns EndpointDead, the logger loops harmlessly.
    }
    // Clone the kernel into the service's address space (empty L1 slots get kernel identity, privileged).
    // SAFETY: pt_root is the freshly-built service L1, not yet in use.
    unsafe { page_tables::fill_kernel_identity(raw.pt_root); }
    Some(LoadedService { entry: raw.entry, pt_root: raw.pt_root, slot: raw.slot })
}

/// Bring up the neutral subsystems, then load the logger and enter it **directly** at PL0 (bypassing
/// the scheduler). The minimal proof that a real service runs unprivileged on ARM; `sched_user` runs
/// the same service *through* the scheduler instead.
pub fn boot_service(ram_end: u32, reserve_end: u32) {
    neutral_bootstrap(ram_end, reserve_end);
    let svc = match load_logger_into_slot() {
        Some(s) => s,
        None => return,
    };
    let (entry, pt_root, slot) = (svc.entry, svc.pt_root, svc.slot);

    // SAFETY: slot just reserved by the loader; make it the current task so the syscall dispatch finds
    // its cap table. Single-threaded boot.
    unsafe { crate::task::scheduler::set_current_task(0, slot); }

    pl011_write(b"arm32: spawn - logger loaded + LOG_WRITE cap set up; entering PL0 at service_main.\r\n");
    pl011_write(b"arm32: ===> below this line is a real GodspeedOS SERVICE running unprivileged <===\r\n");

    // Switch to the service address space (with the TLB flush an address-space change needs), then
    // drop to PL0 at the service entry. The service runs from here; the kernel (mapped privileged in
    // this same address space) serves its syscalls. Does not return.
    // SAFETY: pt_root is the complete service address space; entry is USER-executable, USER_STACK_TOP
    // maps a writable user stack.
    unsafe {
        page_tables::clean_invalidate_dcache_all(); // coherency across the address-space (granule) change
        page_tables::write_page_table_base(pt_root as u64);
        core::arch::asm!("mov r0, #0", "mcr p15, 0, r0, c8, c7, 0", "dsb", "isb", out("r0") _, options(nostack));
        enter_pl0(entry, USER_STACK_TOP);
    }
}

/// Drop to PL0 at `entry` with user stack `sp` (the fabricated exception return from `usermode.rs`).
/// The TTBR0 switch already happened in `boot_service`. Does not return - the service runs.
///
/// # Safety
/// `entry` must be mapped USER-executable and `sp` USER-writable in the active address space.
#[unsafe(naked)]
unsafe extern "C" fn enter_pl0(entry: u32, sp: u32) -> ! {
    core::arch::naked_asm!(
        "cps #0x1f",                     // system mode: shares the USR banked SP
        "mov sp, r1",                    // set the user stack
        "cps #0x13",                     // back to SVC
        "mov r3, #0x10",                 // SPSR = USR mode, interrupts enabled
        "msr spsr_cxsf, r3",
        "mov lr, r0",                    // LR = service entry (service_main)
        "movs pc, lr",                   // drop to PL0 at the service entry
    )
}
