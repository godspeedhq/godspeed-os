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
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE};
use super::pl011_write;
use super::page_tables::{self, PageFlags, PageTable, VirtAddr};
use crate::memory::frame::PhysAddr;

/// The ARM `logger` ELF, embedded by `build.rs`. Empty placeholder on a not-yet-ported arch.
static LOGGER_ELF: &[u8] = include_bytes!(env!("SVC_LOGGER_ELF"));

// Must match the SDK's ServiceContext layout (sdk/rust/src/service_context.rs) - the kernel writes
// this page, the service reads it. Only the fields the logger touches on startup need real values.
const SERVICE_CTX_VA: u32 = 0x003f_f000;
const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;
const USER_STACK_TOP: u32 = 0x8000_0000;

/// Bring up the neutral subsystems a spawn needs, then load and run the logger service.
///
/// `boot_info` is only lightly used (percpu ignores it; the allocator is already live), but building a
/// faithful one keeps the neutral init honest.
pub fn boot_service(ram_end: u32, reserve_end: u32) {
    // --- Neutral bootstrap: per-core arenas, scheduler slots, capability resources. ---
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

    // --- Load the service ELF into a fresh address space. ---
    if LOGGER_ELF.len() < 64 {
        pl011_write(b"arm32: spawn SKIP - no ARM logger ELF embedded\r\n");
        return;
    }
    let loaded = match crate::loader::load(LOGGER_ELF) {
        Ok(l) => l,
        Err(_) => { pl011_write(b"arm32: spawn FAIL - loader rejected the service ELF\r\n"); return; }
    };
    let entry = loaded.entry_va as u32;
    let mut pt = loaded.page_table;
    let pt_root = pt.cr3_value() as u32;

    // --- Map the user stack and the service-context page into that address space. ---
    // Several stack pages: a service builds a 4 KiB IPC message buffer on its stack (ctx.log/recv),
    // so one page is not enough (the first run faulted just below a single page). 8 pages = 32 KiB.
    const STACK_PAGES: u32 = 8;
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    for p in 1..=STACK_PAGES {
        let f = match crate::memory::allocator::alloc_frame() {
            Some(f) => f.phys_addr().0 as u32,
            None => { pl011_write(b"arm32: spawn FAIL - no frame for user stack\r\n"); return; }
        };
        let va = USER_STACK_TOP - p * 0x1000;
        if pt.map(VirtAddr(va as u64), PhysAddr(f as u64), stack_flags).is_err() {
            pl011_write(b"arm32: spawn FAIL - could not map user stack\r\n"); return;
        }
    }
    let ctx_frame = match crate::memory::allocator::alloc_frame() {
        Some(f) => f.phys_addr().0 as u32,
        None => { pl011_write(b"arm32: spawn FAIL - no frame for service context\r\n"); return; }
    };
    let ctx_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE | PageFlags::NO_EXEC;
    if pt.map(VirtAddr(SERVICE_CTX_VA as u64), PhysAddr(ctx_frame as u64), ctx_flags).is_err() {
        pl011_write(b"arm32: spawn FAIL - could not map service context\r\n");
        return;
    }

    // Write the ServiceContext the SDK reads. Identity-mapped, so writable at the frame's phys addr.
    // SAFETY: `ctx_frame` is a fresh frame we own; writing the SDK's context struct into it.
    unsafe {
        let p = ctx_frame as *mut u32;
        p.add(0).write_volatile(SERVICE_CTX_MAGIC); // magic
        p.add(1).write_volatile(0);                 // log_write_slot = 0 (the cap we insert below)
        // Every other slot is "not present" (u32::MAX) or zero; the logger only needs log_write + recv.
        for i in 2..8 { p.add(i).write_volatile(u32::MAX); }
        // recv_slot (index 2) MAX is fine - ctx.recv() returns EndpointDead, the logger loops harmlessly.
    }

    // --- Reserve a task slot, give it the LOG_WRITE capability, make it current. ---
    let slot = match crate::task::scheduler::reserve_task_slot(0) {
        Some(s) => s,
        None => { pl011_write(b"arm32: spawn FAIL - no free task slot\r\n"); return; }
    };
    // SAFETY: slot just reserved; single-threaded boot with interrupts effectively quiescent for setup.
    unsafe {
        let caps = crate::task::scheduler::task_cap_init_empty(slot);
        // Insert the log-write cap at cap-slot 0 (matching log_write_slot in the ctx page above).
        let _ = caps.insert(mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE));
        crate::task::scheduler::set_current_task(0, slot);
    }

    // --- Clone the kernel into the service's address space, then enter PL0. ---
    // SAFETY: pt_root is the service L1; filling the empty (non-service) slots with the kernel identity
    // makes the vectors/kernel/peripherals reachable (privileged) once we switch TTBR0 to it.
    unsafe { page_tables::fill_kernel_identity(pt_root); }

    pl011_write(b"arm32: spawn - logger loaded + LOG_WRITE cap set up; entering PL0 at service_main.\r\n");
    pl011_write(b"arm32: ===> below this line is a real GodspeedOS SERVICE running unprivileged <===\r\n");

    // Switch to the service address space (with the TLB flush an address-space change needs), then
    // drop to PL0 at the service entry. The service runs from here; the kernel (mapped privileged in
    // this same address space) serves its syscalls. Does not return.
    // SAFETY: pt_root is the complete service address space; entry is USER-executable, USER_STACK_TOP
    // maps a writable user stack.
    unsafe {
        clean_invalidate_dcache_all();   // coherency across the address-space (granule) change
        page_tables::write_page_table_base(pt_root as u64);
        core::arch::asm!("mov r0, #0", "mcr p15, 0, r0, c8, c7, 0", "dsb", "isb", out("r0") _, options(nostack));
        enter_pl0(entry, USER_STACK_TOP);
    }
}

/// Clean + invalidate the entire L1 data cache by set/way (`DCCISW`).
///
/// The kernel's memory is mapped as 1 MiB sections in its own tables but as 4 KiB pages in the
/// service's (the ctx/code 1 MiB become tables). Switching TTBR0 between two views of the same
/// physical memory can leave stale cache lines; cleaning the whole D-cache first makes every line
/// coherent with memory before the walker and exclusive monitor see the new mappings.
///
/// # Safety
/// A pure cache-maintenance sweep with no memory effects; reads CCSIDR to size the cache.
unsafe fn clean_invalidate_dcache_all() {
    core::arch::asm!(
        "mov  {t0}, #0",
        "mcr  p15, 2, {t0}, c0, c0, 0", // CSSELR = L1 data cache
        "isb",
        "mrc  p15, 1, {t0}, c0, c0, 0", // CCSIDR
        "and  {t1}, {t0}, #7",          // line size (log2 words - 2)
        "add  {t1}, {t1}, #4",          // + word/byte shift
        "ubfx {t2}, {t0}, #3, #10",     // associativity - 1 (ways)
        "ubfx {t3}, {t0}, #13, #15",    // num sets - 1
        "clz  {t4}, {t2}",              // way position shift
        "2:",                           // set loop ({t3} = current set)
        "mov  {t5}, {t2}",              // ways
        "3:",                           // way loop ({t5} = current way)
        "lsl  {t6}, {t5}, {t4}",        // way << A
        "lsl  {t0}, {t3}, {t1}",        // set << L (t0 reused as scratch)
        "orr  {t6}, {t6}, {t0}",        // set/way value
        "mcr  p15, 0, {t6}, c7, c14, 2",// DCCISW - clean+invalidate by set/way
        "subs {t5}, {t5}, #1",
        "bge  3b",
        "subs {t3}, {t3}, #1",
        "bge  2b",
        "dsb",
        "isb",
        t0 = out(reg) _, t1 = out(reg) _, t2 = out(reg) _, t3 = out(reg) _,
        t4 = out(reg) _, t5 = out(reg) _, t6 = out(reg) _,
        options(nostack),
    );
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
