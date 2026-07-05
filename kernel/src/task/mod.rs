// SPDX-License-Identifier: GPL-2.0-only
//! Task management - §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

use crate::smp::SpinLock;

use crate::arch::x86_64::context_switch::TaskContext;
use crate::arch::x86_64::page_tables::{
    get_hhdm_offset, PageFlags, VirtAddr, PAGE_SIZE,
};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, SPAWN_RESOURCE, CONSOLE_READ_RESOURCE, CONSOLE_PUSH_RESOURCE, INTROSPECT_RESOURCE, SERVICE_CONTROL_RESOURCE, RESOURCE_MINT_RESOURCE, REBOOT_RESOURCE, ACQUIRE_ANY_RESOURCE};
use crate::capability::cap::ResourceId;
use crate::capability::generation::Generation;
use crate::ipc::endpoint::EndpointId;
use crate::memory::allocator::alloc_frame;
use crate::memory::frame::PhysAddr;

// ---------------------------------------------------------------------------
// Kernel stack pool - one 64 KiB stack per ring-3 task (§14.1).
// ---------------------------------------------------------------------------

const TASK_KSTACK_MAX: usize = 224; // raised from 208 to accommodate Milestone 20 brutal adversarial probes
const KSTACK_SIZE:     usize = 64 * 1024; // usable stack per slot (unchanged)
const KSTACK_GUARD:    usize = 4096;      // unmapped guard page below each slot
const KSTACK_STRIDE:   usize = KSTACK_SIZE + KSTACK_GUARD; // 68 KiB per slot

// Page-aligned (4 KiB) so each slot starts on a page boundary - required for the
// per-slot guard page (`install_kstack_guards`). Each slot is a 4 KiB guard page
// followed by 64 KiB of usable stack; usable size is unchanged, the guard is extra.
#[repr(C, align(4096))]
struct KernelStackStorage {
    data: [u8; KSTACK_STRIDE * TASK_KSTACK_MAX],
}

static mut KSTACK_STORAGE: KernelStackStorage =
    KernelStackStorage { data: [0u8; KSTACK_STRIDE * TASK_KSTACK_MAX] };

// Boolean liveness flags for each kstack slot. Protected by SpinLock so
// concurrent alloc/free on different cores are atomic without volatile tricks.
static KSTACK_USED: SpinLock<[bool; TASK_KSTACK_MAX]> =
    SpinLock::new([false; TASK_KSTACK_MAX]);

/// Base virtual address of the kstack pool. The single encapsulated read of the
/// `static mut` pool address; `alloc_kstack` / `free_kstack` / guard install all go
/// through it so the `unsafe` lives in exactly one place.
pub fn kstack_pool_base() -> u64 {
    // SAFETY: read-only address-of a stable static; `addr_of!` yields a raw pointer
    // without materialising a `&mut`, and the casts are pure value conversions.
    unsafe { core::ptr::addr_of!(KSTACK_STORAGE.data) as *const u8 as u64 }
}

/// Install a guard page below every kstack slot (hardening H4 guard-pages). The
/// low 4 KiB page of each 68 KiB slot is unmapped; the 64 KiB usable stack sits
/// above it. A kernel-stack overflow grows down from the top, past the 64 KiB of
/// usable space, and faults loudly on the unmapped guard instead of silently
/// corrupting the slot below - the structural cause of the kstack-overlap bug.
/// Usable size is unchanged (64 KiB); the guard is extra space, so no legitimate
/// deep path can false-positive.
///
/// **Boot-ordering contract** (not a memory-safety one, so this is a safe `fn`):
/// run once on the BSP after `memory::init` (page tables live) and **before APs
/// start and before the first kstack is allocated** - so only the BSP has a TLB
/// (no shootdown needed) and `init`'s stack already carries its guard. Calling it
/// out of order wedges boot; it is not UB. Same shape as `memory::init`/`smp::init`.
pub fn install_kstack_guards() {
    let base = kstack_pool_base();
    debug_assert!(base & (PAGE_SIZE as u64 - 1) == 0, "kstack pool not page-aligned");
    // Page-table work lives in the arch layer (§18.1) - no `unsafe` here.
    crate::arch::x86_64::page_tables::unmap_4k_strided(
        base, KSTACK_STRIDE as u64, TASK_KSTACK_MAX);
    // Verify: slot 0's guard is now unmapped, its usable second page still mapped.
    let g = crate::arch::x86_64::page_tables::entry_for_va(base).is_none();
    let u = crate::arch::x86_64::page_tables::entry_for_va(base + PAGE_SIZE as u64).is_some();
    crate::kprintln!(
        "kstack: {} guard pages installed (64 KiB usable/slot); guard_unmapped={} usable_mapped={}",
        TASK_KSTACK_MAX, g, u);
}

fn alloc_kstack() -> Option<*mut u8> {
    // Interrupt-safe acquisition: KSTACK_USED is ALSO taken by `drain_pending_kstack` from the timer
    // ISR (via `free_kstack`). Without masking, a timer firing while we hold it here re-enters the
    // lock in the ISR on this very core and self-deadlocks (freezes the machine - the `chaos
    // max-carnage` 1-in-~60k hang). The hold is short.
    crate::smp::without_interrupts(|| {
        let mut used = KSTACK_USED.lock();
        for i in 0..TASK_KSTACK_MAX {
            if !used[i] {
                used[i] = true;
                // SAFETY: i < TASK_KSTACK_MAX; offset is within KSTACK_STORAGE bounds.
                // addr_of_mut! yields the same pointer without materialising a &mut
                // to the `static mut` (avoids the static_mut_refs lint).
                // Top = high end of slot i. Usable stack is the 64 KiB just below it;
                // the slot's low 4 KiB (the guard) sits beneath the usable region.
                let top = unsafe {
                    (core::ptr::addr_of_mut!(KSTACK_STORAGE.data) as *mut u8)
                        .add(i * KSTACK_STRIDE + KSTACK_STRIDE)
                };
                return Some(top);
            }
        }
        crate::kprintln!("alloc_kstack: pool exhausted (all {} slots used)", TASK_KSTACK_MAX);
        None
    })
}

/// Return a kstack to the pool.
///
/// `kstack_top` is the value previously returned by `alloc_kstack`
/// (the virtual address of the byte one-past the top of the kstack).
/// A value of 0 means the task had no kstack (ring-0 task) and is
/// silently ignored.
pub fn free_kstack(kstack_top: u64) {
    if kstack_top == 0 { return; }
    let base = kstack_pool_base();
    // top = base + (idx + 1) * KSTACK_STRIDE  →  idx = (top - base) / KSTACK_STRIDE - 1
    if kstack_top <= base { return; }
    let offset = kstack_top - base;
    if offset % KSTACK_STRIDE as u64 != 0 { return; } // misaligned top - ignore
    let idx_plus_one = offset / KSTACK_STRIDE as u64;
    if idx_plus_one == 0 || idx_plus_one > TASK_KSTACK_MAX as u64 { return; }
    let idx = (idx_plus_one - 1) as usize;
    // Interrupt-safe: this runs in BOTH the syscall kill path AND the timer-ISR drain
    // (`drain_pending_kstack`). Masking interrupts while holding KSTACK_USED prevents a timer from
    // re-entering this lock on the same core and self-deadlocking (see `alloc_kstack`). When already
    // called from the ISR (IF=0) the mask is a no-op and IF stays disabled.
    crate::smp::without_interrupts(|| {
        KSTACK_USED.lock()[idx] = false;
    });
}

// ---------------------------------------------------------------------------
// ServiceContextData page - written by kernel, read by SDK (§SDK).
//
// Layout is fixed and MUST match `ServiceContextData` in
// `sdk/rust/src/service_context.rs`.
// ---------------------------------------------------------------------------

pub const SERVICE_CTX_VA:    u64 = 0x3ff000;
pub const SERVICE_CTX_MAGIC: u32 = 0xD0_5D_EA_D5;

/// VA where the xHCI controller's MMIO BAR is mapped into the driver's address
/// space (§12). 4 GiB - well above the user stack (0x8000_0000) and ctx page.
pub const XHCI_MMIO_VA:    u64 = 0x1_0000_0000;
/// Pages of MMIO to map for the xHCI BAR (64 KiB - cap/op/runtime/doorbell regs).
const XHCI_MMIO_PAGES:     u64 = 16;

/// Master switch for IOMMU confinement of the USB drivers (H1).
///
/// `true`  → xHCI is handed off (BIOS→OS) + confined: the proven flagship - a
///           confined front-port keyboard types on hardware. EHCI stays in
///           passthrough (controller stale-pointer quirk, docs/iommu.md).
/// `false` → no handoff, no confinement. Counter-intuitively this does NOT
///           restore a working keyboard: without the handoff firmware and the
///           driver contend for xHCI and Enable Slot never completes. So the
///           clean "both keyboards work" config is **main** (this branch is not
///           merged), not this switch off.
///
/// Default `true`: keep the flagship live + the front keyboard working. For a
/// fully-working daily machine use a `main` build. EHCI dual-keyboard support on
/// this branch is parked, well-characterised future work.
///
/// SETTLED 2026-06-11: EHCI's regression is the IOMMU being enabled, not the xHCI
/// handoff - with the handoff off and EHCI in passthrough, enabling the IOMMU
/// still breaks it (works only on main, IOMMU off). So back to `true`: the
/// flagship (confined xHCI keyboard) is the best the branch can do; EHCI cannot
/// run while the IOMMU is on, by current evidence.
pub const CONFINE_USB_DRIVERS: bool = true;

/// VA where the driver's physically-contiguous DMA arena is mapped (8 GiB).
pub const XHCI_DMA_VA:     u64 = 0x2_0000_0000;

/// Per-driver DMA-arena physical base, allocated ONCE on the first spawn and REUSED across every
/// respawn (§12, the DMA permanent-reserve net). `allocator::alloc_dma_arena` reserves the run out of
/// the general pool so it is never recycled into a page table; keeping the phys here makes the
/// reservation bounded - one arena per driver, reused, rather than one allocated per spawn. So a stray
/// device DMA (if the kill-path bus-master quiesce ever fails) always lands in DMA-reserved memory,
/// never a PTE or kernel struct. 0 = not yet allocated. (xhci/ehci/block-driver; a future NIC = 4th.)
pub static XHCI_DMA_PHYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static EHCI_DMA_PHYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static AHCI_DMA_PHYS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static NIC_DMA_PHYS:  core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Pages of contiguous DMA memory for the **xHCI** driver. The first 16 pages
/// hold the control structures (command/event rings, DCBAA, ERST, per-device
/// slices, plus the scratchpad buffer array at page 15); the remaining 256 pages
/// are the scratchpad buffers the controller DMAs into (real AMD xHCI reports
/// MaxScratchpadBufs=256 - 1 MiB - and malfunctions without them). Confined
/// identity-mapped, so the device reaches all of it (§12, H1).
const XHCI_DMA_PAGES:      u64 = 16 + 256;
/// Pages of contiguous DMA memory for the **EHCI** driver - 64 KiB, as on main.
/// EHCI has no scratchpad concept, and its driver zeroes the whole arena on every
/// control transfer; giving it the xHCI-sized 1 MiB arena (a leftover of sharing
/// one constant) regressed back-port enumeration. Keep it small and separate.
const EHCI_DMA_PAGES:      u64 = 16;

/// Maximum named send peers per service.
pub const MAX_SEND_PEERS:  usize = 4;
/// Maximum bytes per peer name stored in ServiceContextData.
pub const PEER_NAME_BYTES: usize = 24;

/// One caller-supplied send-peer to install in a new task (Phase 0b, `docs/naming-design.md`):
/// a `(label, Capability)` pair the supervisor hands the kernel at spawn, instead of the kernel
/// resolving `label` against the name table. The kernel inserts `cap` into the child's cap table
/// and records `label → slot` in its send-peer metadata, so the child's `ctx.capability(label)`
/// resolves exactly as on the old name-wiring path. The cap is a copy of one the caller holds
/// (validated with GRANT in the syscall handler), so this is non-escalating (§7.3).
#[derive(Clone, Copy)]
pub struct InstallCap {
    pub name:     [u8; PEER_NAME_BYTES],
    pub name_len: u8,
    pub cap:      crate::capability::Capability,
}

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
    xhci_mmio_va:       u64, // 0 = not mapped; else VA of the driver's controller BAR - xHCI or EHCI (§12)
    xhci_dma_va:        u64, // 0 = none; else VA of the driver's DMA arena (§12)
    xhci_dma_phys:      u64, // physical base of the DMA arena (programmed into the device)
    xhci_dma_len:       u64, // length of the DMA arena in bytes
    console_push_slot:  u32, // u32::MAX = none; else CONSOLE_PUSH cap slot (input driver)
    self_grant_slot:    u32, // u32::MAX = none; else SEND|GRANT cap to this service's OWN
                             // endpoint, so it can register its name in the kernel directory.
    send_peers:         [SendPeerEntry; MAX_SEND_PEERS],
}

// ---------------------------------------------------------------------------
// User stack layout constants.
// ---------------------------------------------------------------------------

const USER_STACK_TOP:   u64 = 0x8000_0000;
const USER_STACK_PAGES: u64 = 64; // 256 KiB - enough for pf_handler running on user stack
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
    /// A live task with this name already exists. Refused to avoid duplicate
    /// instances - in particular a second trusted-root service (§6.2).
    AlreadyRunning,
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
// Supervisor ELF - conditionally replaced for §22 Test 1B.
// When the kernel is built with --features test-bad-supervisor, the supervisor binary is two
// garbage bytes that fail ELF loading, so the kernel's DIRECT spawn of the supervisor fails →
// kernel panic ("supervisor spawn failed", §6.2). This is §22 Test 1B (TCB-failure-panics):
// the supervisor is the corrupt-and-fail TCB.
// ---------------------------------------------------------------------------

#[cfg(feature = "test-bad-supervisor")]
const SUPERVISOR_ELF: &[u8] = b"\xDE\xAD"; // invalid ELF, triggers LoadFailed
#[cfg(not(feature = "test-bad-supervisor"))]
const SUPERVISOR_ELF: &[u8] = include_bytes!(env!("SVC_SUPERVISOR_ELF"));

/// The one shared probe ELF. Every probe/test-driver service uses this exact
/// reference, so the spawn path can identify "is a probe" by pointer identity
/// (`elf_bytes` == `PROBE_ELF`) - used to mint the service_control cap for the
/// test drivers without enumerating every probe name. A single const guarantees
/// the pointer compares equal; separate `include_bytes!` sites would not.
const PROBE_ELF: &[u8] = include_bytes!(env!("SVC_PROBE_ELF"));

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

/// True if the calling task's contract declares `peer` as a send-peer (§13) - so reacquiring a SEND
/// cap to it (`AcquireSendCap`) is contract-authorized recovery (§14.2), not ambient authority (§3.1).
/// The caller's name comes from the existing `task_stat` snapshot and its declared peers from the
/// static `service_config`, so this adds no new per-task kernel state and no new `unsafe`.
pub fn current_task_declares_peer(peer: &str) -> bool {
    let slot = scheduler::current_task_slot();
    let name = scheduler::task_stat(slot).name;
    match service_config(name) {
        Some((_, cfg)) => cfg.send_peers.iter().any(|p| *p == peer),
        None           => false,
    }
}

fn service_config(name: &str) -> Option<(&'static str, ServiceConfig)> {
    match name {
        "supervisor" => Some(("supervisor", ServiceConfig {
            elf:               SUPERVISOR_ELF, // garbage under test-bad-supervisor (Test 1B)
            has_recv_endpoint: true, // death-notification endpoint (H11 ph6 restart loop)
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
        // mem-pressure: a spawn-on-demand memory-pressure victim for `chaos mem-pressure` (allocs 4 MiB
        // chunks up to this limit, then AllocDenied; killed to reclaim). Not in any auto-spawn set.
        "mem-pressure" => Some(("mem-pressure", ServiceConfig {
            elf:               include_bytes!(env!("SVC_MEM_PRESSURE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      32 * 1024 * 1024, // ~8 chunks then AllocDenied; a clear free-frames swing
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // chaos: the spawn-on-demand system-stress orchestrator for `chaos max-carnage`. It kills +
        // floods other services and is the one program a run never kills (it excludes ITSELF). Holds
        // SERVICE_CONTROL (kill), INTROSPECT (task_stat victim selection), ACQUIRE_ANY (flood), SPAWN
        // (mem-pressure spawn-burst), CONSOLE_READ (q-poll + the foreground claim, syscall 40), LOG_WRITE
        // (the TUI, via ConsoleWrite). Excluded from auto-spawn; the shell spawns it by name on demand.
        "chaos" => Some(("chaos", ServiceConfig {
            elf:               include_bytes!(env!("SVC_CHAOS_ELF")),
            has_recv_endpoint: true,  // recv the round count from the shell launcher at startup
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  true,
        })),
        "ping" => Some(("ping", ServiceConfig {
            elf:               include_bytes!(env!("SVC_PING_ELF")),
            has_recv_endpoint: true,
            // ping reaches pong (name-wired here, or supervisor-provided); it reacquires pong by
            // name via the kernel directory on EndpointDead.
            send_peers:        &["pong"],
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
            send_peers:        &[], // Path C: recorded in the kernel directory at spawn; no peers
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // greet / upper - capability-mediated pipe demo (Appendix D.3).
        // `upper` recvs and uppercases each line. `greet` has NO static send
        // authority (send_peers empty) - the shell delegates it a SEND cap to
        // upper's endpoint at spawn, which becomes its send_peers[0]. Authority
        // is granted at composition time, not held by contract.
        // ----------------------------------------------------------------
        "upper" => Some(("upper", ServiceConfig {
            elf:               include_bytes!(env!("SVC_UPPER_ELF")),
            has_recv_endpoint: true,
            // A pipe SINK is recorded in the kernel name-directory at spawn; the shell resolves its
            // endpoint by name at runtime (`builtin | service`, `acquire_send_grant_cap`) - no
            // contracted cap.
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "greet" => Some(("greet", ServiceConfig {
            elf:               include_bytes!(env!("SVC_GREET_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[], // delegated at runtime by the shell, not contracted
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // roster - record-producing pipe demo (docs/records.md): emits a typed Table as JSON
        // through the shell-delegated SEND cap. Same zero-ambient-authority shape as greet.
        "roster" => Some(("roster", ServiceConfig {
            elf:               include_bytes!(env!("SVC_ROSTER_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[], // delegated at runtime by the shell, not contracted
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // xhci - USB host-controller driver (§12). Receives its controller's
        // MMIO BAR (mapped by name in the spawn path) + later its IRQ. Trusted
        // userspace driver. has_recv_endpoint for future interrupt delivery.
        "xhci" => Some(("xhci", ServiceConfig {
            elf:               include_bytes!(env!("SVC_XHCI_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            // Pin to core 1: keep the driver off core 0 where the shell + TCB live (§9.2).
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            // Route the xHCI MSI (interrupts::XHCI_MSI_VECTOR = 0x28) to this driver's recv
            // endpoint (§12). The kernel programmed the controller's MSI-X to this vector at
            // boot; the driver enables the controller's interrupter and drains the events.
            hw_irqs:           &[0x28],
            has_console_read:  false,
        })),
        // `ehci` - userspace USB 2.0 driver (§12) for the back ports' EHCI controller. Same
        // shape as `xhci`; the kernel grants its MMIO/DMA at spawn (E1b+). Busy-polls on core 1
        // (alongside xHCI) - the model that worked flawlessly. The EHCI's legacy INTx can't drive
        // a block-and-wake loop on this hardware (deliver() fired zero times once the driver
        // blocked across many T630 flashes), and the CPU-reduction attempts introduced quirks, so
        // both USB drivers are back on plain busy-poll. Core 1 runs hot; reclaiming that idle is
        // deferred (revisit later).
        "ehci" => Some(("ehci", ServiceConfig {
            elf:               include_bytes!(env!("SVC_EHCI_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            // Route the EHCI INTx (interrupts::EHCI_MSI_VECTOR = 0x29, IOAPIC-routed) to this
            // driver's recv endpoint (§12). The driver enables USBINTR + acks + unmasks.
            hw_irqs:           &[0x29],
            has_console_read:  false,
        })),
        // `block-driver` - userspace ATA PIO disk driver (persistence, v2; §6.3,
        // docs/persistence.md). The kernel grants its ATA port window by name in
        // the spawn path (6a-pio); no MMIO, no DMA, no IRQ wired yet (polled).
        // Phase 1 reads sector 0 and logs it. Pinned to core 1, off the shell/TCB.
        "block-driver" => Some(("block-driver", ServiceConfig {
            elf:               include_bytes!(env!("SVC_BLOCK_DRIVER_ELF")),
            has_recv_endpoint: true, // serves block read/write requests from fs (§4)
            send_peers:        &[], // Path C: recorded in the kernel directory at spawn; no peers
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      16 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // `nic-driver` - userspace NIC driver (networking, v2; docs/networking.md, Phase 1).
        // The kernel maps the Intel e1000's BAR0 by name at spawn (gated on the discovered NIC
        // actually being an e1000), like the USB/AHCI controllers. Phase 1 step 2 is reset +
        // read the MAC; TX/RX rings, the RX IRQ, and the frame interface to net-stack follow.
        "nic-driver" => Some(("nic-driver", ServiceConfig {
            elf:               include_bytes!(env!("SVC_NIC_DRIVER_ELF")),
            has_recv_endpoint: true, // will serve the frame interface to net-stack (§12)
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      16 * 1024 * 1024,
            hw_irqs:           &[], // Phase 1 step 2: reset + MAC only; RX IRQ wired later
            has_console_read:  false,
        })),
        // net-stack (services/net-stack): the model-AGNOSTIC half of networking (docs/networking.md).
        // Owns its endpoint (nic-driver replies frames there via the per-request reply cap) and sends
        // to nic-driver (the frame interface). Spawned AFTER nic-driver so its send-peer cap wires from
        // the kernel name table at spawn. Core 1. No hardware - it speaks ARP/IP over raw frames.
        "net-stack" => Some(("net-stack", ServiceConfig {
            elf:               include_bytes!(env!("SVC_NET_STACK_ELF")),
            has_recv_endpoint: true,               // nic-driver replies frames here (per-request reply cap)
            send_peers:        &["nic-driver"],    // the frame interface; reacquired by name on death
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      16 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // `fs` - userspace filesystem (persistence, v2; §15, docs/persistence.md).
        // Phase 1: mounts by reading the superblock (LBA 0) from `block-driver`
        // over IPC and validating its magic. Spawned AFTER block-driver (its
        // send-peer cap wires from the kernel name table at spawn). Core 1.
        "fs" => Some(("fs", ServiceConfig {
            elf:               include_bytes!(env!("SVC_FS_ELF")),
            has_recv_endpoint: true, // owns an endpoint (reply target + future fs API)
            send_peers:        &["block-driver"], // fs's block-driver peer (supervisor-provided / name-wired)
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      32 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // counter (examples/counter): a STATEFUL service that survives its OWN restart by
        // persisting its running count to `fs` and reconstructing it on spawn (§14 restart, §15
        // persistence). Owns its endpoint (fs replies there via the per-request reply cap) and sends
        // to `fs` (read/write /counter.dat). Spawned only in the counter-test build (`osdev test
        // counter`); idle/absent everywhere else. Restartable: its death notifies the supervisor,
        // which respawns it (scheduler death-notification set + supervisor restart loop).
        "counter" => Some(("counter", ServiceConfig {
            elf:               include_bytes!(env!("SVC_COUNTER_ELF")),
            has_recv_endpoint: true,            // fs reply target (request_with_reply embeds a reply cap)
            send_peers:        &["fs"],         // file-API ops to fs; reacquired by name on EndpointDead
            send_peers_grant:  false,
            preferred_core:    u32::MAX,        // round-robin (no [placement] in its contract)
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // reply-server (examples/reply-server): the request/reply (RPC) SERVER. Owns its endpoint
        // (clients send requests here) and has NO named send peer - it replies only over the reply
        // capability each request embeds (§7.10/§8.5). Spawned only in the reply-test build (`osdev
        // test reply-server`); idle/absent everywhere else - standalone it just blocks on recv().
        "reply-server" => Some(("reply-server", ServiceConfig {
            elf:               include_bytes!(env!("SVC_REPLY_SERVER_ELF")),
            has_recv_endpoint: true,            // clients send requests here; replies via embedded cap
            send_peers:        &[],             // no named peer: it answers over the client's reply cap
            send_peers_grant:  false,
            preferred_core:    u32::MAX,        // round-robin (no [placement] in its contract)
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // asker (examples/asker): the request/reply CLIENT that exercises reply-server. Owns its
        // endpoint (reply-server replies there via the embedded reply cap) and sends to `reply-server`.
        // Spawned only in the reply-test build (`osdev test reply-server`); idle/absent elsewhere.
        "asker" => Some(("asker", ServiceConfig {
            elf:               include_bytes!(env!("SVC_ASKER_ELF")),
            has_recv_endpoint: true,            // reply target (request_with_reply blocks on this endpoint)
            send_peers:        &["reply-server"], // sends requests to reply-server; reacquired by name on EndpointDead
            send_peers_grant:  false,
            preferred_core:    u32::MAX,        // round-robin (no [placement] in its contract)
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // resource-server (examples/resource-server): the delegated-resource-capability OWNER (§7.10).
        // Owns its endpoint (a cap holder's `resource_invoke` is routed here, badged) and SENDs to
        // `holder` to GRANT it the minted resource cap. Holds RESOURCE_MINT (granted by name below,
        // like fs). Spawned only in the resource-test build (`osdev test resource-server`); idle/absent
        // everywhere else - standalone, without the mint grant, it just idles (graceful degrade, §7.10).
        "resource-server" => Some(("resource-server", ServiceConfig {
            elf:               include_bytes!(env!("SVC_RESOURCE_SERVER_ELF")),
            has_recv_endpoint: true,            // resource_invoke is routed here, badged with (rid, right)
            send_peers:        &["holder"],     // sends the GRANT (the resource cap) to holder
            send_peers_grant:  false,
            preferred_core:    u32::MAX,        // round-robin (no [placement] in its contract)
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // holder (examples/holder): the delegated-resource-capability CLIENT. Owns its endpoint (the
        // GRANT lands here, and resource-server's replies to its cap invocations come back here). It
        // declares NO send-peer: it acts only through the granted cap (the kernel routes a
        // resource_invoke to the owner). Spawned only in the resource-test build; idle/absent elsewhere.
        "holder" => Some(("holder", ServiceConfig {
            elf:               include_bytes!(env!("SVC_HOLDER_ELF")),
            has_recv_endpoint: true,            // grant target + reply target for its resource_invokes
            send_peers:        &[],             // names no one; uses the granted cap, routed by the kernel
            send_peers_grant:  false,
            preferred_core:    u32::MAX,        // round-robin (no [placement] in its contract)
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Probe services - §22 Group A identity tests.
        // All use the same probe ELF; probe_mode selects the test behaviour.
        // Spawn ordering in supervisor: recv-endpoint services first, then
        // senders that need SEND caps wired to them.
        // ----------------------------------------------------------------
        "probe-recv" => Some(("probe-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        1, // MODE_ECHO_RECV - Test 3A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-victim" => Some(("probe-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE - killed by probe-4a in Test 4A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4b-recv" => Some(("probe-4b-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0, // MODE_PASSIVE - killed by harness in Test 4B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-3b" => Some(("probe-3b", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        3, // MODE_NO_SEND_RIGHT - Test 3B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-sender" => Some(("probe-sender", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["probe-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        2, // MODE_ECHO_SEND - Test 3A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4a" => Some(("probe-4a", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["probe-victim"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        4, // MODE_SEND_AFTER_KILL - Test 4A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-4b-send" => Some(("probe-4b-send", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["probe-4b-recv"],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        5, // MODE_FILL_AND_BLOCK - Test 4B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-yielder" => Some(("probe-yielder", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        6, // MODE_YIELD_LOGGER - Test 8A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-hog" => Some(("probe-hog", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        7, // MODE_HOG - Test 8B (preemption proven via ping)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-9b" => Some(("probe-9b", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        8, // MODE_CAP_FORGE - Test 9B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Cap-transfer probes - §22 Tests 5A and 5B.
        // probe-5a-recv must be spawned before probe-5a-send and probe-5b-send
        // so its endpoint is registered before sender caps are wired.
        // ----------------------------------------------------------------
        "probe-5a-recv" => Some(("probe-5a-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        9, // MODE_GRANT_RECV - Test 5A receiver
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-5a-send" => Some(("probe-5a-send", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  true,  // mints SEND|GRANT cap to probe-5a-recv
            preferred_core:    0,
            probe_mode:        10, // MODE_GRANT_SEND - Test 5A sender
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-5b-send" => Some(("probe-5b-send", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["probe-5a-recv"],
            send_peers_grant:  false, // SEND only - no GRANT right; should return CapNotGrantable
            preferred_core:    0,
            probe_mode:        11, // MODE_NO_GRANT_SEND - Test 5B negative
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Memory-limit probes - §22 Tests 7A and 7B.
        // ----------------------------------------------------------------
        "probe-7a" => Some(("probe-7a", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        12, // MODE_ALLOC_OK - Test 7A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "probe-7b" => Some(("probe-7b", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        13, // MODE_ALLOC_LIMIT - Test 7B
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Interrupt-routing probe - §22 Tests IR1A (§12.2, §12.3).
        // hw_irqs registers IRQ 33 to probe-11a's recv endpoint at spawn.
        // ----------------------------------------------------------------
        "probe-11a" => Some(("probe-11a", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX, // round-robin
            probe_mode:        160, // MODE_IRQ_RECV - Test IR1A
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[33],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Property-test probes - Milestone 9 Phase 1.
        // prop-p9-victim must be listed (and spawned) before prop-p9 so its
        // endpoint is in the name directory when prop-p9's SEND caps are wired.
        // ----------------------------------------------------------------
        "prop-p9-victim" => Some(("prop-p9-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,  // MODE_PASSIVE - killed by prop-p9
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p1" => Some(("prop-p1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            // Three SEND caps to the same endpoint - proves all cap slots are
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
            elf:               PROBE_ELF,
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
        // Property-test probes - Milestone 9 Phase 2.
        // ----------------------------------------------------------------
        // P2: generation monotonic. prop-p2-victim must be listed before prop-p2.
        // prop-p2 pinned to Core 3 - away from P8 (Core 1) and P6 (Core 2).
        "prop-p2-victim" => Some(("prop-p2-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE - killed/respawned by prop-p2
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p2" => Some(("prop-p2", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Pinned to Core 2 - away from the P2 (Core 3) and P8 (Core 1) kill/spawn
        // controllers whose long spawn syscalls would starve P6 of CPU time.
        "prop-p6" => Some(("prop-p6", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE - killed/respawned by prop-p8
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p8" => Some(("prop-p8", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Property-test probes - Milestone 9 Phase 3.
        // ----------------------------------------------------------------
        // P4: memory accounting. No victim needed.
        "prop-p4" => Some(("prop-p4", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE - killed/respawned by prop-p5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p5" => Some(("prop-p5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0,  // MODE_PASSIVE - killed/respawned by prop-p7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "prop-p7" => Some(("prop-p7", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Brutal property test probes - Milestone 16.
        // 10 escalated-iteration variants of P1-P10, each with its own victim
        // where the original property needed one.  Victims before controllers.
        // ----------------------------------------------------------------
        // BP1: cap unforgeability at 100k iterations.
        "prop-bp1" => Some(("prop-bp1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        105,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP3: cap rights never widen - 10k iterations (self-referential, like P3).
        "prop-bp3" => Some(("prop-bp3", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        106,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP4: alloc accounting exact - 2k iterations.
        "prop-bp4" => Some(("prop-bp4", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        107,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP5: endpoint ownership - 150 kill/respawn cycles.
        "prop-bp5-victim" => Some(("prop-bp5-victim", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        108,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP6: queue invariants - 2k iterations (self-referential, like P6).
        "prop-bp6" => Some(("prop-bp6", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &["prop-bp6"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        109,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP7: TLB shootdown proxy - 150 kill/respawn cycles.
        "prop-bp7-victim" => Some(("prop-bp7-victim", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        110,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP8: restart + higher-generation liveness - 20 iterations.
        "prop-bp8-victim" => Some(("prop-bp8-victim", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["prop-bp9-victim", "prop-bp9-victim", "prop-bp9-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        112,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BP10: every send returns a defined outcome - 100k iterations.
        "prop-bp10" => Some(("prop-bp10", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Fuzz-test probes - Milestone 10.
        // Recv-endpoint victims must be listed before their fuzz controllers.
        // ----------------------------------------------------------------
        "fuzz-f1" => Some(("fuzz-f1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        31, // FUZZ_F2: random syscall numbers
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F5: IPC message body fuzzing - recv target first.
        "fuzz-f5-recv" => Some(("fuzz-f5-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - soaks up random messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f5" => Some(("fuzz-f5", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f5-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        32, // FUZZ_F5: random IPC message bodies
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F6: embedded cap fuzzing - recv target first.
        "fuzz-f6-recv" => Some(("fuzz-f6-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - receives (or rejects) cap-embedded messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f6" => Some(("fuzz-f6", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f6-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        33, // FUZZ_F6: random embedded cap slot indices
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F7: stale cap / generation fuzzing - victim first.
        "fuzz-f7-victim" => Some(("fuzz-f7-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed/respawned by fuzz-f7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-f7" => Some(("fuzz-f7", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-f7-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        34, // FUZZ_F7: stale-cap sends after kill
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // F8: memory request size fuzzing - no peers needed.
        "fuzz-f8" => Some(("fuzz-f8", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Brutal fuzz test probes - Milestone 17.
        // Victims/recv-endpoints before controllers.
        // ----------------------------------------------------------------
        "fuzz-bf5-recv" => Some(("fuzz-bf5-recv", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf5-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        116, // FUZZ_BF5: random IPC bodies - 5k sends
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf6-recv" => Some(("fuzz-bf6-recv", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf6-recv"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        117, // FUZZ_BF6: random cap slots - 5k SendWithCap
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf7-victim" => Some(("fuzz-bf7-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // passive recv - killed/respawned by fuzz-bf7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf7" => Some(("fuzz-bf7", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["fuzz-bf7-victim"],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        118, // FUZZ_BF7: stale cap - 200 kill/respawn cycles
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf1" => Some(("fuzz-bf1", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        114, // FUZZ_BF1: syscall args - 500 × 10 calls
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf2" => Some(("fuzz-bf2", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        115, // FUZZ_BF2: syscall numbers - 200k random
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "fuzz-bf8" => Some(("fuzz-bf8", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        119, // FUZZ_BF8: memory sizes - 10 edge + 5k random
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Stress-test probes - Milestone 11 Phase 1.
        // Recv-endpoint victims must be listed before their controllers.
        // ----------------------------------------------------------------
        // S1: IPC saturation. Receiver is passive (never drains).
        "stress-s1-recv" => Some(("stress-s1-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - queue fills; stress-s1 measures saturation
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s1" => Some(("stress-s1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed/respawned by stress-s2
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s2" => Some(("stress-s2", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Cross-core try_send diagnostic (osdev image --mode iso-xsend): sender on
        // core 1 → receiver on core 2, mirroring C7's controller→victim direction.
        "xsend-recv" => Some(("xsend-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // receiver on core 2 (C7's victim core)
            probe_mode:        201, // XSEND_RECV: drain forever
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "xsend" => Some(("xsend", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["xsend-recv"],
            send_peers_grant:  false,
            preferred_core:    1, // sender on core 1 (C7's controller core)
            probe_mode:        200, // XSEND: time cross-core try_send to xsend-recv
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // Cross-core task-lifecycle diagnostic (osdev image --mode iso-xlife):
        // controller on core 1 kills/respawns a same-core victim (xlife-near, core 1)
        // and a cross-core victim (xlife-far, core 2) to attribute C7's ~1.04 s respawn.
        "xlife" => Some(("xlife", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // controller on core 1 (C7's controller core)
            probe_mode:        202, // XLIFE: time kill/spawn of near+far victims
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "xlife-near" => Some(("xlife-near", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // same core as the controller
            probe_mode:        203, // XLIFE_VICTIM: idle until killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "xlife-far" => Some(("xlife-far", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core from the controller (C7's victim core)
            probe_mode:        203, // XLIFE_VICTIM: idle until killed
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s3-recv" => Some(("stress-s3-recv", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed/respawned by stress-s4
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s4" => Some(("stress-s4", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            // Two SEND caps to the same endpoint - both must die on one kill (§7.5).
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    1, // cross-core: coordinator on 0 kills victim on 1
            probe_mode:        0, // PASSIVE - killed by stress-s10
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s10" => Some(("stress-s10", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            // Three SEND caps to the same endpoint - all must die on one kill (§7.5, §8.6).
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed by stress-s5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s5" => Some(("stress-s5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        49, // STRESS_S8: 600 yields, proves scheduler returns from idle
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // S9: Cross-core IPI storm - receiver on core 2; two senders on cores 0 and 1
        "stress-s9-recv" => Some(("stress-s9-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // core 2 - distinct from s3/s10 cross-core pairs on cores 0-1
            probe_mode:        51, // STRESS_S9_RECV: drain 1000 messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-s9-send-a" => Some(("stress-s9-send-a", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Brutal stress-test probes - Milestone 18.
        // Ordering: recv-endpoint victims before controllers; receivers before senders.
        // ----------------------------------------------------------------
        // BS1: IPC saturation, 5× S1.
        "stress-bs1-recv" => Some(("stress-bs1-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - queue fills; bs1 measures saturation
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "stress-bs1" => Some(("stress-bs1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Performance-benchmark probes - Milestone 12.
        // Sender services are spawned before their echo/recv partners so
        // their endpoints are registered when echo partners wire SEND caps.
        // ----------------------------------------------------------------
        // B1: same-core IPC roundtrip. Sender acquires cap dynamically.
        "perf-b1" => Some(("perf-b1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed/respawned by perf-b5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-b5" => Some(("perf-b5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Brutal performance-benchmark probes - Milestone 19 (5× iteration counts).
        // Sender/controller spawned before echo/recv so endpoints register first.
        // ----------------------------------------------------------------
        "perf-bp1" => Some(("perf-bp1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // PASSIVE - killed/respawned by perf-bp5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "perf-bp5" => Some(("perf-bp5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Adversarial-test probes - Milestone 13.
        // Victim/passive services must be listed before their attackers so
        // their endpoints are registered when the attacker's SEND caps are wired.
        // ----------------------------------------------------------------
        // A1: random cap slots → always Err. No caps needed.
        "adv-a1" => Some(("adv-a1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        82, // ADV_A3: alloc edge cases under 4 MiB cap
            memory_limit:      4 * 1024 * 1024, // 4 MiB - tight limit for the test
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A4: RECV cap used as SEND target → CapInsufficientRights. Has recv endpoint.
        "adv-a4" => Some(("adv-a4", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        83, // ADV_A4: RECV cap → try_send → CapInsufficientRights
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A5: TOCTOU - victim must be registered before attacker's SEND cap is wired.
        "adv-a5-victim" => Some(("adv-a5-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE - killed by adv-a5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-a5" => Some(("adv-a5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        85, // ADV_A6: acquire_send_cap loop until table full
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A7: timing probe - passive recv target must be registered before sender.
        "adv-a7-recv" => Some(("adv-a7-recv", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE - absorbs timing probe messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-a7" => Some(("adv-a7", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        90, // ADV_A10: kernel-addr syscall args → rejected
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A11: introspection gated - TaskStat denied without INTROSPECT cap (§3.1).
        // Name matches no introspect grant, so adv-a11 holds no introspect cap.
        "adv-a11" => Some(("adv-a11", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        161, // ADV_A11: gated query denied without INTROSPECT
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A12: reboot gated - Reboot/18 denied without the REBOOT cap (§3.1).
        // Name matches no reboot grant (only shell/xhci/ehci get it), so adv-a12 holds none.
        "adv-a12" => Some(("adv-a12", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        162, // ADV_A12: reboot denied without REBOOT cap
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // A13: AcquireSendCap gated (§3.1). adv-a13 holds NO ACQUIRE_ANY (excluded from the grant
        // above) and declares NO send-peers, so acquiring a SEND cap to any service must be DENIED.
        "adv-a13" => Some(("adv-a13", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        163, // ADV_A13: AcquireSendCap denied without ACQUIRE_ANY / declared peer
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Chaos-test probes - Milestone 14.
        // Victim/passive services must be listed before their controllers so
        // their endpoints are registered when the controllers' SEND caps are wired.
        // ----------------------------------------------------------------
        // C2: null-deref → page fault → killed. Monitor on separate round-robin core.
        "chaos-c2" => Some(("chaos-c2", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        93, // CHAOS_C3: 500 alloc-deny cycles without panic
            memory_limit:      4 * 1024 * 1024, // 4 MiB - tight limit for the test
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C5: kernel stack depth probe. No peers needed.
        "chaos-c5" => Some(("chaos-c5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    3, // core 3 - simulates one starved core
            probe_mode:        7, // MODE_HOG: tight loop (reused)
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-c6-monitor" => Some(("chaos-c6-monitor", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0, // core 0 - cross-core witness: proves core 0 alive
            probe_mode:        95, // CHAOS_C6_MON: 200 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // C7: cross-core kill/respawn TLB-shootdown stress.
        // Victim on core 2 must be registered before controller on core 1 gets SEND cap.
        "chaos-c7-victim" => Some(("chaos-c7-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core: controller on 1 kills victim on 2
            probe_mode:        0, // MODE_PASSIVE - killed/respawned by chaos-c7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-c7" => Some(("chaos-c7", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Brutal identity test probes - Milestone 15.
        // T11: self-referential queue boundary exactness.
        // T12: cap delegation chain A→B→C.
        // T13: cross-core blocked send wakes with EndpointDead.
        // T-SMP: SMP escalation (smp=2, 8, 16 - run via osdev test identity-brutal).
        // ----------------------------------------------------------------
        "brutal-id-11" => Some(("brutal-id-11", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &["brutal-id-13-recv"],
            send_peers_grant:  false,
            preferred_core:    0, // fills queue then blocks - must be on different core than recv
            probe_mode:        102,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "brutal-id-13-kill" => Some(("brutal-id-13-kill", ServiceConfig {
            elf:               PROBE_ELF,
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
        // Brutal adversarial test probes - Milestone 20.
        // Victim/passive services must be listed before their attackers so
        // their endpoints are registered when the attacker's SEND caps are wired.
        // ----------------------------------------------------------------
        // BA1: 50k random cap forgery attempts (5× A1). No caps needed.
        "adv-ba1" => Some(("adv-ba1", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE - killed/re-killed by adv-ba5
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-ba5" => Some(("adv-ba5", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        0, // MODE_PASSIVE - absorbs timing probe messages
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "adv-ba7" => Some(("adv-ba7", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
        // Brutal chaos-test services - Milestone 21.
        // BC2: 5 simultaneous null-deref faulters + 1 monitor proving system survival.
        // ----------------------------------------------------------------
        "chaos-bc2-a" => Some(("chaos-bc2-a", ServiceConfig {
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
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
            elf:               PROBE_ELF,
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0, // core 0 - cross-core witness
            probe_mode:        158, // MODE_CHAOS_BC6_MON: 1,000 yields then log pass
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // BC7: 150 cross-core kill/respawn TLB-shootdown cycles.
        // Victim on core 2 must be registered before controller on core 1 gets SEND cap.
        "chaos-bc7-victim" => Some(("chaos-bc7-victim", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    2, // cross-core: controller on 1 kills victim on 2
            probe_mode:        0, // MODE_PASSIVE - killed/respawned by chaos-bc7
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        "chaos-bc7" => Some(("chaos-bc7", ServiceConfig {
            elf:               PROBE_ELF,
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
            probe_mode:        0, // MODE_LIVE
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // `observe now` - one-shot static metrics frame (probe_mode 1 = MODE_NOW).
        // Same ELF as `observe`; the shell brokers a kill-then-spawn of this.
        "observe-now" => Some(("observe-now", ServiceConfig {
            elf:               include_bytes!(env!("SVC_OBSERVE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    u32::MAX,
            probe_mode:        1, // MODE_NOW
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // `observe` (live) - full-screen foreground view (probe_mode 2 = MODE_LIVE).
        // Same ELF as `observe`; the shell spawns it, pauses its own read loop, and
        // resumes when it parks (the shell-brokered foreground handoff). Holds
        // CONSOLE_READ so it can poll for `q` (non-blocking) and toggle echo while
        // it owns the screen.
        "observe-live" => Some(("observe-live", ServiceConfig {
            elf:               include_bytes!(env!("SVC_OBSERVE_ELF")),
            has_recv_endpoint: false,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        2, // MODE_LIVE
            memory_limit:      8 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  true,
        })),
        "shell" => Some(("shell", ServiceConfig {
            elf:               include_bytes!(env!("SVC_SHELL_ELF")),
            // Endpoint + an `fs` send-peer so the `drives`/file commands can request_with_reply
            // to `fs` (the reply-cap pattern needs the shell's own endpoint). The shell holds
            // only a narrow SEND to fs - fs enforces all disk authority. `fs` must be spawned
            // before the shell so this cap resolves (supervisor order). The shell resolves a pipe
            // sink's endpoint at runtime via the kernel directory (`acquire_send_grant_cap`) -
            // no contracted peer.
            has_recv_endpoint: true,
            send_peers:        &["fs"],
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
/// Resolve which core a spawn lands on: explicit override, else the contract's
/// preferred core (falling back to round-robin if it isn't ready), else
/// round-robin across ready cores.
fn resolve_spawn_core(core_override: Option<u32>, preferred_core: u32) -> u32 {
    use core::sync::atomic::{AtomicU32, Ordering};
    static RR: AtomicU32 = AtomicU32::new(0);
    match core_override {
        Some(n) => n,
        None if preferred_core == u32::MAX => {
            let count = crate::smp::core::ready_count() as u32;
            if count == 0 { 0 } else { RR.fetch_add(1, Ordering::Relaxed) % count }
        }
        None => {
            if crate::smp::core::is_ready(preferred_core) {
                preferred_core
            } else {
                let count = crate::smp::core::ready_count() as u32;
                RR.fetch_add(1, Ordering::Relaxed) % count.max(1)
            }
        }
    }
}

/// Spawn a producer and delegate it a SEND cap to `sink`'s endpoint as its
/// `send_peers[0]` - the capability-broker primitive behind shell pipes
/// (`producer | sink`). The producer's *contract* send peers are intentionally
/// not used: its only send authority is this runtime-delegated pipe cap, so it
/// can reach exactly the sink the shell wired it to and nothing else (§3.1, no
/// ambient authority - composition grants, it doesn't assume).
///
/// `sink` must already be spawned and have registered its endpoint, so the SEND
/// cap can be minted against it. The shell spawns the consumer before the producer.
pub fn spawn_service_pipe(producer: &str, sink: &str, core_override: Option<u32>)
    -> Result<(), SpawnError>
{
    let (static_name, cfg) = service_config(producer).ok_or(SpawnError::NotFound)?;
    let core_id = resolve_spawn_core(core_override, cfg.preferred_core);
    // The delegated pipe peer goes FIRST so the producer/filter reaches it via
    // `send_peer_at(0)` (its "downstream"); the contract's own peers follow, so a filter that
    // must register its name to receive a stage's input (e.g. `upper`) still can. Bounded by
    // MAX_SEND_PEERS - extra contract peers past the cap are dropped (the pipe peer is kept).
    let mut pipe_peers: [&str; MAX_SEND_PEERS] = [""; MAX_SEND_PEERS];
    pipe_peers[0] = sink;
    let mut np = 1usize;
    for &p in cfg.send_peers {
        if np >= MAX_SEND_PEERS { break; }
        pipe_peers[np] = p;
        np += 1;
    }
    let result = spawn_service_with_config(static_name, cfg.elf, core_id,
        cfg.has_recv_endpoint, &pipe_peers[..np], cfg.probe_mode, cfg.send_peers_grant,
        cfg.memory_limit, cfg.hw_irqs, cfg.has_console_read, None);
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn pipe '{}' -> '{}' failed: {:?}", producer, sink, e);
    }
    result.map(|_| ())
}

pub fn spawn_service_by_name(name: &str, core_override: Option<u32>) -> Result<Option<EndpointId>, SpawnError> {
    let (static_name, cfg) = service_config(name).ok_or(SpawnError::NotFound)?;

    // Singleton guard (§6.2, §26.6 bounded behaviour): refuse to spawn a service
    // whose name is already live. This blocks duplicate instances in general, and
    // in particular a second trusted-root service - the supervisor is
    // always live while the system runs, so this always rejects spawning/restarting
    // them, the same protection `handle_kill` gives. It does NOT block boot: there
    // each service is spawned exactly once, before any instance is live. Loud
    // rejection, never silent (§3.12).
    if scheduler::find_task_by_name(static_name).is_some() {
        crate::kprintln!("task: spawn '{}' rejected: already running", static_name);
        return Err(SpawnError::AlreadyRunning);
    }

    let core_id = resolve_spawn_core(core_override, cfg.preferred_core);

    let result = spawn_service_with_config(static_name, cfg.elf, core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit, cfg.hw_irqs,
                              cfg.has_console_read, None);
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn '{}' failed: {:?}", name, e);
    }
    result
}

/// Phase 0b (`docs/naming-design.md`): spawn `name`, but wire its send-peers from caller-supplied
/// `installs` (`(label, cap)` pairs) instead of the kernel name table. Same singleton guard +
/// placement as `spawn_service_by_name`; returns the new task's recv `EndpointId` (`None` if it has
/// none). The caps in `installs` are copies the caller held (GRANT-validated by the syscall handler).
pub fn spawn_service_by_name_with_installs(
    name: &str, core_override: Option<u32>, installs: &[InstallCap],
) -> Result<Option<EndpointId>, SpawnError> {
    let (static_name, cfg) = service_config(name).ok_or(SpawnError::NotFound)?;
    if scheduler::find_task_by_name(static_name).is_some() {
        crate::kprintln!("task: spawn '{}' rejected: already running", static_name);
        return Err(SpawnError::AlreadyRunning);
    }
    let core_id = resolve_spawn_core(core_override, cfg.preferred_core);
    let result = spawn_service_with_config(static_name, cfg.elf, core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit, cfg.hw_irqs,
                              cfg.has_console_read, Some(installs));
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn '{}' (with installs) failed: {:?}", name, e);
    }
    result
}

/// Per-spawn DIAG step-markers (`spawn[elf]`, `spawn[stack]`, …). Added to narrow a
/// bare-metal boot freeze; kept as a debug aid but **off by default**. They were a
/// real performance trap: in builds with no shell (the `iso-*`/probe images) the
/// framebuffer mirror never turns off, so every kprintln line triggers a full-screen
/// scroll that reads back uncached VRAM - ~130 ms per line on the T630. Seven markers
/// per spawn made a respawn look ~40× a cold spawn (see the iso-c7/iso-xlife dig).
/// Flip to `true` only to debug a spawn-path freeze; the compiler dead-code-eliminates
/// the `kprintln!`s when `false`. The `task: … spawned OK` announce and `kill_task:`
/// line are kept (legitimate lifecycle output, one line each).
const SPAWN_TRACE: bool = false;

/// Low-level spawn: load ELF, wire caps, enqueue on `core_id`. Returns the new task's recv
/// `EndpointId` (`None` if it has no endpoint) - the caller (via the spawn syscall) can mint a
/// cap to it. This is the Phase-0 seam for moving naming out of the kernel (`docs/naming-design.md`):
/// a spawner can collect a cap to every service it starts without the kernel resolving names.
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
    // Phase 0b (docs/naming-design.md): if `Some`, wire the child's send-peers from these
    // caller-supplied `(label, cap)` entries instead of resolving `send_peers` against the kernel
    // name table. The kernel installs each cap and records `label → slot` in the child's send-peer
    // metadata, so the child's `ctx.capability(label)` resolves exactly as it does on the old path.
    // `None` = the old name-resolution path (unchanged).
    installs:          Option<&[InstallCap]>,
) -> Result<Option<EndpointId>, SpawnError> {
    // DIAG step markers (gated by SPAWN_TRACE; off by default - see its doc).
    if SPAWN_TRACE { crate::kprintln!("spawn[elf]: '{}'", name); }

    // 1. Parse ELF.
    let crate::loader::LoadedElf { mut page_table, entry_va, mapped_bytes: elf_mapped_bytes } =
        crate::loader::load(elf_bytes)?;

    if SPAWN_TRACE { crate::kprintln!("spawn[stack]: '{}'", name); }

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
            // Frame owned by the page table now; Frame is Copy/no-Drop (no release).
            va += PAGE_SIZE as u64;
        }
    }

    if SPAWN_TRACE { crate::kprintln!("spawn[slot]: '{}'", name); }

    // 3. Reserve a task slot and initialise its CapTable directly in BSS.
    let task_slot = scheduler::reserve_task_slot(core_id).ok_or(SpawnError::NoMemory)?;
    if SPAWN_TRACE { crate::kprintln!("spawn[caps]: '{}' slot={}", name, task_slot); }
    // SAFETY: task_slot was just reserved; IF=0 in syscall context.
    let caps = unsafe { scheduler::task_cap_init_empty(task_slot) };

    // Slot 0: log_write (always present in v1).
    caps.insert(mint_cap(LOG_WRITE_RESOURCE, Rights::WRITE))
        .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;

    // Spawn authority - least privilege (§3.1; H10 audit in
    // security/hardening-strategy.md §9). Granted only to the services that
    // actually start other services: init (spawns the trusted root), supervisor
    // (spawns services + probes), the shell (brokers spawn/kill/restart), and the
    // test-driver probes (property/stress/perf/chaos modes spawn victims; matched by
    // ELF identity so no probe family is missed). logger, the drivers,
    // ping, pong, and observe never spawn and no longer hold the authority to.
    // Previously every service got this unconditionally ("spawn authority, every
    // service in v1") - a system-wide blast-radius widening this closes. Capture the
    // slot (u32::MAX when not granted); the SDK already treats MAX as "not held".
    let mut spawn_slot_u32 = u32::MAX;
    if name == "supervisor"            // init removed (Path C / Phase 5) - supervisor is the spawner
        || name == "shell"
        || name == "chaos"             // spawns mem-pressure tasks for the spawn-burst dimension of max-carnage
        || core::ptr::eq(elf_bytes.as_ptr(), PROBE_ELF.as_ptr())
    {
        let sp_slot = caps.insert(mint_cap(SPAWN_RESOURCE, Rights::WRITE))
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
        spawn_slot_u32 = sp_slot as u32;
    }

    // 4. Optional recv endpoint.
    let mut recv_slot_u32 = u32::MAX;
    let mut self_grant_slot_u32 = u32::MAX;
    let mut own_endpoint:  Option<EndpointId> = None;

    if has_recv_endpoint {
        let ep_id       = crate::ipc::alloc_endpoint_id();
        let resource_id = ResourceId::from(ep_id);

        // The new endpoint's generation comes from the single GLOBAL monotonic counter (§7.5): it
        // strictly exceeds every previously-issued endpoint generation, so a respawn always
        // out-generations the service's prior instance (per-service monotonicity, P2/P8) AND any
        // earlier holder of a reclaimed endpoint id (the ABA guard). This replaces the old
        // by-name/by-slot seeding, whose by-NAME source the self-heal removed: it read the prior
        // generation through `names::lookup(name)`, but unregister-on-death (§14.2) now clears that
        // name, so a respawn handed a *reused* id from a different service's lineage would otherwise
        // seed below its own prior generation. A global counter needs neither the name nor the id.
        let start_gen = crate::capability::next_generation();

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

        // Self-grant cap: a SEND|GRANT cap to this service's OWN endpoint, so it can
        // announce its name to the kernel directory by granting a derived copy. GRANT is
        // required for the cap to be transferable via SendWithCap; the service keeps
        // this original and derives copies for re-registration after a restart.
        if let Ok(sg) = caps.insert(mint_cap(resource_id, Rights::SEND | Rights::GRANT)) {
            self_grant_slot_u32 = sg as u32;
        }

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

    // The USB keyboard driver gets a CONSOLE_PUSH cap so it can inject decoded
    // keystrokes into the console input ring (§12). Both USB drivers hold it -
    // `xhci` for front-port keyboards, `ehci` for the back-port (USB 2.0) ones.
    let mut console_push_slot_u32 = u32::MAX;
    if name == "xhci" || name == "ehci" {
        let cp_cap = mint_cap(CONSOLE_PUSH_RESOURCE, Rights::WRITE);
        let cap_slot = caps.insert(cp_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
        console_push_slot_u32 = cap_slot as u32;
    }

    // Services that read another task's or system-wide kernel state hold the
    // INTROSPECT cap (§3.1; docs/introspection-capability.md). The shell and the
    // observe utility use TaskStat + the aggregate InspectKernel queries; the
    // prop-*/stress-* test-harness probes query victim generations (InspectKernel
    // query 2). Self-state (own alloc bytes) and the TSC stay ungated, so every
    // other service needs nothing. No slot is stored - the gate scans holdings.
    if name == "shell"
        || name == "supervisor"        // task_stat: the reconcile loop scans real liveness to respawn a
                                       //   service whose death notification was dropped under a storm
        || name == "chaos"             // task_stat: victim selection + recovery checks in max-carnage
        || name.starts_with("observe") // observe + observe-now (and future modes)
        || name.starts_with("prop-")
        || name.starts_with("stress-")
    {
        let in_cap = mint_cap(INTROSPECT_RESOURCE, Rights::READ);
        caps.insert(in_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    }

    // Services that kill other services hold the service_control cap (§3.1/§14.4;
    // docs/service-control-cap.md): the shell (interactive broker), the supervisor
    // (restart authority), and every test-driver probe (they kill victim services
    // to exercise kill/revocation). Probes are identified by ELF identity
    // (elf_bytes == PROBE_ELF) so no probe family is missed by name.
    if name == "shell"
        || name == "supervisor"
        || name == "chaos"             // kills victim services (the whole point of max-carnage)
        || core::ptr::eq(elf_bytes.as_ptr(), PROBE_ELF.as_ptr())
    {
        let sc_cap = mint_cap(SERVICE_CONTROL_RESOURCE, Rights::WRITE);
        caps.insert(sc_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    }

    // The resource-mint authority (§7.10, P2 file-as-capability): held only by services that
    // issue delegated resources whose meaning they define. `fs` mints a file cap per open file.
    // Least-privilege (§3.1) - no other service can create delegated resources.
    // `resource-server` (examples/) is also granted it BY NAME (the same e1000-BAR-style by-name
    // kernel grant, never a contract field): this turns the example from a compile-only template
    // into the real, QEMU-proven `osdev test resource-server`. It only takes effect in the
    // resource-test build, the only build that spawns `resource-server` - in every other build it
    // is never spawned, so the grant never fires.
    // `net-stack` mints SOCKET capabilities (a socket is a delegated resource cap, §7.10, the same
    // mechanism `fs` uses for files) - so it needs the same minting authority.
    if name == "fs" || name == "resource-server" || name == "net-stack" {
        let rm_cap = mint_cap(RESOURCE_MINT_RESOURCE, Rights::WRITE);
        caps.insert(rm_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    }

    // The reboot authority (§3.1): the `shell` (its `reboot` command) and the USB drivers `xhci`/`ehci`
    // (the Ctrl+Alt+Del secure-attention reboot) are the only legitimate rebooters - no other service
    // can hardware-reset the machine. Closes the last ambient-authority gap (`Reboot`/18 was ungated).
    if name == "shell" || name == "xhci" || name == "ehci" {
        let rb_cap = mint_cap(REBOOT_RESOURCE, Rights::WRITE);
        caps.insert(rb_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    }

    // The broad-acquire authority (§3.1): the operator/test instruments that legitimately reach
    // ARBITRARY services by name via `AcquireSendCap` - the `shell` (chaos flooding, pipe sinks), the
    // `supervisor` (reconcile-by-name), and test probes. Ordinary services get NONE: their
    // `AcquireSendCap` is restricted to their contract-declared send-peers (recovery), so they hold no
    // ambient send authority. Probes are matched by ELF identity so no probe family is missed.
    // `adv-a13` is the §22 Test A13 negative pin: it is deliberately EXCLUDED so it holds no
    // ACQUIRE_ANY (and declares no send-peers), proving AcquireSendCap denies a non-holder.
    if name == "shell" || name == "supervisor" || name == "chaos"  // chaos floods arbitrary services by name
        || (core::ptr::eq(elf_bytes.as_ptr(), PROBE_ELF.as_ptr()) && name != "adv-a13")
    {
        let aa_cap = mint_cap(ACQUIRE_ANY_RESOURCE, Rights::WRITE);
        caps.insert(aa_cap)
            .map_err(|_| { scheduler::release_task_slot(task_slot); SpawnError::CapTableFull })?;
    }

    // 5. Send-peer SEND caps (wired at spawn from the name directory).
    let mut peer_data: [(u32, u32, [u8; PEER_NAME_BYTES]); MAX_SEND_PEERS] =
        [(u32::MAX, 0, [0u8; PEER_NAME_BYTES]); MAX_SEND_PEERS];
    let mut peer_count = 0usize;

    // Wiring is a MERGE (Phase 0b/2, docs/naming-design.md): install the caller-supplied caps
    // first, then name-wire any declared send-peer the caller did NOT provide. This lets the
    // supervisor flip peers one at a time (provide what it holds in its name→cap map; the kernel
    // fills the rest from the name table until Phase 5 removes it). `installs == None` (every
    // existing spawn) means the install step is skipped and ALL declared peers are name-wired -
    // the old behaviour, verbatim. A peer is "provided" if its label matches an install entry.

    // 1. Install caller-supplied caps (a copy the caller already held, GRANT-validated in the
    //    syscall handler - non-escalating §7.3). Each becomes a send-peer under its label, so the
    //    child resolves `ctx.capability(label)` identically. A delegated peer not in the contract
    //    (e.g. `greet`'s sink at index 0) arrives this way too.
    if let Some(installs) = installs {
        for entry in installs {
            if peer_count >= MAX_SEND_PEERS { break; }
            match caps.insert(entry.cap) {
                Ok(cap_slot) => {
                    let len = (entry.name_len as usize).min(PEER_NAME_BYTES);
                    peer_data[peer_count].0 = cap_slot as u32;
                    peer_data[peer_count].1 = len as u32;
                    peer_data[peer_count].2[..len].copy_from_slice(&entry.name[..len]);
                    peer_count += 1;
                }
                Err(_) => crate::kprintln!(
                    "task: cap table full, skipping installed cap for '{}'", name),
            }
        }
    }

    // 2. Name-wire each declared send-peer the caller did NOT already provide.
    for &peer_name in send_peers {
        if peer_count >= MAX_SEND_PEERS { break; }

        // Skip peers already supplied by the install list (matched by label).
        let provided = match installs {
            Some(installs) => installs.iter()
                .any(|e| &e.name[..(e.name_len as usize).min(PEER_NAME_BYTES)] == peer_name.as_bytes()),
            None => false,
        };
        if provided { continue; }

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

    // 6a. Map the xHCI controller's MMIO BAR into the driver's address space
    // (§12). Name-gated: only the `xhci` service receives it, and only if the
    // PCI scan found a controller. Device registers must be uncached (PCD|PWT).
    // Map the USB host-controller BAR for a driver service into its address space
    // at XHCI_MMIO_VA. Both the xhci and ehci drivers use this one window - a
    // service holds exactly one controller, and each has its own address space, so
    // the shared VA + ctx field (`xhci_mmio_va`, read by `ctx.xhci_mmio()` /
    // `ctx.ehci_mmio()`) is unambiguous (§12).
    let xhci_mmio_va = {
        use core::sync::atomic::Ordering::Relaxed;
        use crate::arch::x86_64::pci;
        let bar = if name == "xhci" && pci::XHCI_FOUND.load(Relaxed) {
            pci::XHCI_MMIO_BASE.load(Relaxed)
        } else if name == "ehci" && pci::EHCI_FOUND.load(Relaxed) {
            pci::EHCI_MMIO_BASE.load(Relaxed)
        } else if name == "block-driver" && pci::AHCI_FOUND.load(Relaxed) {
            pci::AHCI_ABAR.load(Relaxed) // AHCI HBA registers (docs/ahci.md)
        } else if (name == "nic-driver" || name == "e1000") && pci::NIC_FOUND.load(Relaxed)
            && matches!(pci::NIC_VENDOR_DEVICE.load(Relaxed), 0x100E_8086 | 0x8168_10EC) {
            // The NIC's first memory BAR (its register space), mapped for `nic-driver` (or the `e1000`
            // example) and ONLY when the discovered NIC is one nic-driver can drive: an Intel e1000
            // (0x100E:8086) or a Realtek RTL8168 (0x8168:10EC, the T630). On any other NIC this is
            // false, so the driver gets no mapping and idles - it never touches foreign hardware
            // (Commandment VII: a hardware capability is granted explicitly, for the device asked for).
            pci::NIC_MMIO_BASE.load(Relaxed)
        } else {
            0
        };
        if bar != 0 {
            let mmio_flags = PageFlags::PRESENT
                | PageFlags::WRITABLE
                | PageFlags::USER
                | PageFlags::NO_EXEC
                | PageFlags::PCD
                | PageFlags::PWT;
            for i in 0..XHCI_MMIO_PAGES {
                let off = i * PAGE_SIZE as u64;
                page_table
                    .map(VirtAddr(XHCI_MMIO_VA + off), PhysAddr(bar + off), mmio_flags)
                    .map_err(|_| SpawnError::MapFailed)?;
            }
            crate::kprintln!("spawn[mmio]: '{}' BAR {:#x} -> VA {:#x}", name, bar, XHCI_MMIO_VA);
            XHCI_MMIO_VA
        } else {
            0
        }
    };

    // 6b. Allocate + map a physically-contiguous DMA arena for the xHCI driver
    // (§12). The controller DMAs into this memory (rings/contexts), so the driver
    // needs both the VA (to build structures) and the physical base (to program
    // the controller). Normal cacheable mapping - x86 DMA is cache-coherent.
    // Grant a physically-contiguous DMA arena to a USB driver (xhci or ehci) for
    // its queue structures. Shared VA/fields, separate address spaces (§12).
    let dma_for_driver = {
        use core::sync::atomic::Ordering::Relaxed;
        use crate::arch::x86_64::pci;
        (name == "xhci" && pci::XHCI_FOUND.load(Relaxed))
            || (name == "ehci" && pci::EHCI_FOUND.load(Relaxed))
            || (name == "block-driver" && pci::AHCI_FOUND.load(Relaxed)) // AHCI (docs/ahci.md)
            || (name == "nic-driver" && pci::NIC_FOUND.load(Relaxed)) // e1000 TX/RX rings (docs/networking.md)
    };
    // Per-driver arena size: xHCI needs room for its 256 scratchpad buffers;
    // EHCI gets the small 64 KiB arena it had on main; the AHCI block driver needs
    // only its command list/FIS/command table + a data buffer - 64 KiB is plenty.
    let dma_pages = if name == "ehci" || name == "block-driver" || name == "nic-driver" {
        EHCI_DMA_PAGES
    } else {
        XHCI_DMA_PAGES
    };
    let (xhci_dma_va, xhci_dma_phys, xhci_dma_len) = if dma_for_driver {
        // DMA permanent-reserve (§12): allocate this driver's arena ONCE, then reuse the same physical
        // frames across every respawn. `alloc_dma_arena` reserves the run out of the general pool (so it
        // is never recycled into a page table); keeping the phys keeps the reservation bounded - one
        // arena per driver, not one per spawn. So a stray DMA (if the kill-path bus-master quiesce ever
        // fails) always lands in DMA-reserved memory, never a PTE or kernel struct.
        let kept = match name {
            "xhci"         => &XHCI_DMA_PHYS,
            "ehci"         => &EHCI_DMA_PHYS,
            "block-driver" => &AHCI_DMA_PHYS,
            "nic-driver"   => &NIC_DMA_PHYS,
            _              => &XHCI_DMA_PHYS, // unreachable: dma_for_driver gates these names
        };
        let arena = match kept.load(core::sync::atomic::Ordering::Relaxed) {
            0 => {
                let p = crate::memory::allocator::alloc_dma_arena(dma_pages as usize);
                if let Some(phys) = p { kept.store(phys, core::sync::atomic::Ordering::Relaxed); }
                p
            }
            p => Some(p), // reuse the permanent arena allocated on a prior spawn
        };
        match arena {
            Some(phys) => {
                let flags = PageFlags::PRESENT
                    | PageFlags::WRITABLE
                    | PageFlags::USER
                    | PageFlags::NO_EXEC;
                for i in 0..dma_pages {
                    let off = i * PAGE_SIZE as u64;
                    page_table
                        .map(VirtAddr(XHCI_DMA_VA + off), PhysAddr(phys + off), flags)
                        .map_err(|_| SpawnError::MapFailed)?;
                }
                let len = dma_pages * PAGE_SIZE as u64;
                crate::kprintln!(
                    "spawn[dma]: '{}' arena phys {:#x} -> VA {:#x} ({} KiB)",
                    name, phys, XHCI_DMA_VA, len / 1024
                );
                // H1 Phase 1d: confine this DMA-capable driver to its arena via
                // the IOMMU, so a compromised driver cannot DMA outside it. No-op
                // if no IOMMU is present (drivers then remain in the TCB).
                //
                // Confinement is per-driver, EARNED by the driver being complete
                // enough to run fully confined (BIOS handoff + all controller DMA
                // inside the arena). The xHCI driver qualifies (handoff + 256-buffer
                // scratchpad: a confined keyboard works on hardware). The EHCI
                // controller retains a stale internal DMA pointer into the firmware
                // ROM region (~0xffffffc0) that survives HCRESET - its async/qTD
                // schedule is provably correct (verified by byte-dump), so this is a
                // controller quirk, not a driver bug. Confining it makes that benign
                // read fatal and breaks the keyboard, so EHCI stays in passthrough
                // until the quirk is resolved (e.g. a deeper PCI-level reset). See
                // docs/iommu.md.
                {
                    use core::sync::atomic::Ordering::Relaxed;
                    use crate::arch::x86_64::pci;
                    if CONFINE_USB_DRIVERS && name == "xhci" {
                        crate::arch::x86_64::iommu::confine_device(
                            pci::XHCI_BDF.load(Relaxed), phys, len);
                    } else {
                        // `block-driver` (AHCI) stays in IOMMU passthrough, like ehci:
                        // the T630 BIOS hands the SATA controller over with a stale
                        // firmware DMA pointer (~0xffffffc0). Confining it makes that
                        // benign stale read a fatal IO_PAGE_FAULT (CI stuck); in
                        // passthrough the read is harmless and AHCI works. Confinement
                        // needs an AHCI BIOS/OS handoff first (a future step, §6.4;
                        // docs/ahci.md) - same situation the USB drivers hit.
                        crate::kprintln!(
                            "spawn[dma]: '{}' left in IOMMU passthrough (CONFINE_USB_DRIVERS={})",
                            name, CONFINE_USB_DRIVERS
                        );
                    }
                    // Re-enable PCI bus-mastering for this DMA driver. The kill path CLEARS it to quiesce the
                    // controller before the frame reclaim (the max-carnage corruption fix), and firmware sets
                    // it only once at boot - so a RESPAWN must re-enable it or the new instance's DMA silently
                    // never starts. Idempotent (no-op if already set). Per-driver BDF.
                    let bdf = match name {
                        "xhci"         => pci::XHCI_BDF.load(Relaxed),
                        "ehci"         => pci::EHCI_BDF.load(Relaxed),
                        "block-driver" => pci::AHCI_BDF.load(Relaxed),
                        "nic-driver"   => pci::NIC_BDF.load(Relaxed),
                        _              => 0xFFFF,
                    };
                    pci::set_bus_master(bdf);
                }
                (XHCI_DMA_VA, phys, len)
            }
            None => {
                crate::kprintln!("spawn[dma]: '{}' WARN: no contiguous DMA arena", name);
                (0, 0, 0)
            }
        }
    } else {
        (0, 0, 0)
    };

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
            data.spawn_slot         = spawn_slot_u32;
            data.send_peer_count    = peer_count as u32;
            data.core_id            = core_id;
            data.probe_mode         = probe_mode;
            data.console_read_slot  = console_read_slot_u32;
            data.console_push_slot  = console_push_slot_u32;
            data.self_grant_slot    = self_grant_slot_u32;
            data.xhci_mmio_va       = xhci_mmio_va;
            data.xhci_dma_va        = xhci_dma_va;
            data.xhci_dma_phys      = xhci_dma_phys;
            data.xhci_dma_len       = xhci_dma_len;
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
        // ctx_frame owned by the page table now; Frame is Copy/no-Drop (no release).
    }

    if SPAWN_TRACE { crate::kprintln!("spawn[kstack]: '{}'", name); }

    // 7. Kernel stack.
    let kstack_top = alloc_kstack().ok_or(SpawnError::NoMemory)?;
    if SPAWN_TRACE { crate::kprintln!("spawn[commit]: '{}' kstack ok", name); }

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

    // 10. Initialise the memory budget for this task (§10.3). Seed it with the base footprint -
    // the mapped binary (code+data+BSS), the 256 KiB user stack, and the ctx page - so MEM_USED
    // reflects real occupancy, not just dynamic alloc_mem (which most no-heap services never call).
    let base_bytes = elf_mapped_bytes
        + USER_STACK_PAGES * PAGE_SIZE as u64
        + PAGE_SIZE as u64; // ctx page
    scheduler::set_task_memory_budget(task_slot, memory_limit, base_bytes);

    crate::kprintln!("task: '{}' spawned OK on core {} (slot {})", name, core_id, task_slot);
    Ok(own_endpoint)
}

/// Spawn `init` on Core 0. Called once by `kernel_main` (§11.1).
/// The kernel's ONE direct spawn (Path C / Phase 5 - `init` is removed). The kernel boots the
/// SUPERVISOR directly; the supervisor then spawns logger and all services. Uses `SUPERVISOR_ELF`
/// (garbage under `test-bad-supervisor` → §22 Test 1B). `has_recv_endpoint = true` (the supervisor
/// owns the death-notification endpoint). A *boot-time* spawn failure is fatal (§6.2, §11.3); a later
/// *runtime* death is recovered by the kernel respawning it (Phase 6 - see below).
pub fn spawn_supervisor() {
    match spawn_service_with_config("supervisor", SUPERVISOR_ELF, 0, true, &[], 0, false, 64 * 1024 * 1024, &[], false, None) {
        Ok(_) => crate::kprintln!("task: supervisor spawned on core 0"),
        Err(e) => panic!("supervisor spawn failed: {:?}", e),
    }
}

// ---------------------------------------------------------------------------
// Supervisor respawn (Path C / Phase 6 - the supervisor is restartable; §6.2).
//
// The supervisor is no longer the non-restartable trusted root: when it dies, the KERNEL respawns it
// (the kernel is the one thing that cannot die - the last-resort recovery anchor of Path C, §3.7).
// The death path (`kill_task`) only FLAGS the respawn - running it inline is unsafe (we are mid-
// teardown of the dying supervisor). `control::process_pending` (Core 0 control tick, already a
// spawn-safe deferred point that respawns services for RESTART) polls the flag and does the respawn.
//
// **No bound on the number of respawns - deliberately.** A cap that panicked after N respawns would
// re-introduce the very reboot Phase 6 eliminates (just deferred from 1 death to N), and would hand
// any attacker a trivial denial-of-service: kill the supervisor N times to force a reboot. So the
// kernel respawns it *unconditionally, forever*. This is NOT unbounded-resource behavior (§26.6):
// each respawn first reclaims the dead instance's frames/kstack/caps, then allocates fresh, so the
// footprint is constant and reclaimed every time - only the *count* grows, and a count is not a
// resource. The respawn is loud (logged with a running count, §26.4/§26.7); a sustained loop floods
// the log and an operator intervenes, but the system stays alive rather than rebooting. The new
// instance re-registers its endpoint in `ipc::names`, so death notifications re-point to it, and it
// reconciles live services on boot. The only truly unkillable thing is the kernel itself.
// ---------------------------------------------------------------------------
static SUPERVISOR_RESPAWN_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
static SUPERVISOR_RESPAWN_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
/// True while a supervisor respawn is in flight (from just before PENDING is claimed in
/// `poll_supervisor_respawn` until `spawn_supervisor` returns). The timer ISR uses it to ROUND-ROBIN
/// the spawn with ready tasks (see `scheduler::timer_tick_from_irq`): when a task is running it
/// switches OUT to the scheduler context to RESUME the spawn; when the spawn is running (prev==IDLE)
/// the normal switch PREEMPTS it and runs a ready task. So the spawn is preemptible (lock-holders run
/// and release) and resumable (it gets quanta) - replacing the old IF=1 pin, which suppressed the
/// switch to keep the spawn running but STARVED any Core-0 lock-holder and deadlocked under load
/// (§22 Test 15). The spawn's locks are IRQ-safe, so it is only ever preempted between holds.
static SUPERVISOR_RESPAWN_IN_PROGRESS: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Flag that the supervisor died and must be respawned. Called from the death path (`kill_task`);
/// the actual respawn runs later from `poll_supervisor_respawn` at the Core-0 scheduler loop top -
/// an IF=1 point (see `scheduler::run` and the timer-ISR routing in `timer_tick_from_irq`).
pub fn flag_supervisor_respawn() {
    SUPERVISOR_RESPAWN_PENDING.store(true, core::sync::atomic::Ordering::Release);
}

/// Whether a supervisor respawn is pending. The timer ISR (`timer_tick_from_irq`) checks this to
/// route Core 0 into the scheduler context (an IF=1 point) when a respawn is due, rather than doing
/// the heavy ~22 ms spawn in the IF=0 ISR (which would block cross-core IPI ACKs and wedge the box).
pub fn supervisor_respawn_pending() -> bool {
    SUPERVISOR_RESPAWN_PENDING.load(core::sync::atomic::Ordering::Acquire)
}

/// If the supervisor died, respawn it (Path C / Phase 6). Called from `control::process_pending`
/// (Core 0) - a spawn-safe deferred point. Always respawns; never gives up (see the note above).
/// The count is observability only (§26.4), not a bound.
pub fn poll_supervisor_respawn() {
    use core::sync::atomic::Ordering;
    // Cheap fast path: a plain load on the (very hot) Core-0 scheduler loop - no atomic RMW when the
    // supervisor is healthy (the common case, every iteration).
    if !SUPERVISOR_RESPAWN_PENDING.load(Ordering::Acquire) {
        return;
    }
    // Mark IN_PROGRESS *before* claiming PENDING, so the (now preemptible) scheduler context is ALWAYS
    // covered by PENDING-or-IN_PROGRESS - no gap where a timer preemption would strand the poll and lose
    // the respawn (between the PENDING.load above and here, PENDING is still set, so the timer ISR's
    // pending branch keeps us; from here on IN_PROGRESS keeps us). The respawn is no longer pinned: the
    // timer ROUND-ROBINS it (see scheduler::timer_tick_from_irq) so it is preemptible (lock-holders run)
    // and resumable (it gets quanta) - the spawn no longer strands in CORE_SCHED_CTX under load.
    SUPERVISOR_RESPAWN_IN_PROGRESS.store(true, Ordering::Release);
    // Claim PENDING. Core-0-only, so the swap always succeeds; the guard is defensive.
    if !SUPERVISOR_RESPAWN_PENDING.swap(false, Ordering::AcqRel) {
        SUPERVISOR_RESPAWN_IN_PROGRESS.store(false, Ordering::Release);
        return;
    }
    let n = SUPERVISOR_RESPAWN_COUNT.fetch_add(1, Ordering::AcqRel) + 1;
    crate::kprintln!("kernel: supervisor died - respawning (#{}) (Path C / Phase 6)", n);
    spawn_supervisor();
    SUPERVISOR_RESPAWN_IN_PROGRESS.store(false, Ordering::Release);
}

/// True only while `spawn_supervisor` runs at the Core-0 scheduler loop top. The timer ISR checks
/// this and returns instead of preempting, so the spawn runs to completion (IF=1; not switched away).
pub fn supervisor_respawn_in_progress() -> bool {
    SUPERVISOR_RESPAWN_IN_PROGRESS.load(core::sync::atomic::Ordering::Acquire)
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

/// Kill the currently-running task (called from page-fault handler - §10.3).
pub fn kill_current() {
    let slot = scheduler::current_task_slot();
    if slot < scheduler::MAX_TASKS {
        scheduler::kill_task_by_slot(slot);
    }
    // Reschedule - kill_task_by_slot already sets state to Dead; the scheduler
    // will skip this task on the next pick_next pass.
    scheduler::yield_current();
}
