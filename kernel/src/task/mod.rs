// SPDX-License-Identifier: GPL-2.0-only
//! Task management - §9, §14.

pub mod scheduler;
pub mod state;
pub mod task;

pub use task::{Task, TaskId};

use crate::smp::SpinLock;

use crate::arch::imp::context_switch::TaskContext;
use crate::arch::imp::page_tables::{
    get_hhdm_offset, PageFlags, VirtAddr, PAGE_SIZE,
};
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, SPAWN_RESOURCE, CONSOLE_READ_RESOURCE, CONSOLE_PUSH_RESOURCE, INTROSPECT_RESOURCE, SERVICE_CONTROL_RESOURCE, RESOURCE_MINT_RESOURCE, REBOOT_RESOURCE, ACQUIRE_ANY_RESOURCE, NET_DEVICE_RESOURCE, GPIO_DEVICE_RESOURCE, USB_DISK_RESOURCE, SET_CLOCK_RESOURCE, FIRE_IRQ_RESOURCE};
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
    crate::arch::imp::page_tables::unmap_4k_strided(
        base, KSTACK_STRIDE as u64, TASK_KSTACK_MAX);
    // Verify: slot 0's guard is now unmapped, its usable second page still mapped.
    let g = crate::arch::imp::page_tables::entry_for_va(base).is_none();
    let u = crate::arch::imp::page_tables::entry_for_va(base + PAGE_SIZE as u64).is_some();
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

/// VA where the display's framebuffer is mapped into the `console` service's address space.
///
/// A plain 32-bit address, unlike `XHCI_MMIO_VA` (4 GiB) and `XHCI_DMA_VA` (8 GiB), because it has to
/// exist on a 32-bit machine too - the Pi 2 is the first board to use it.
///
/// **0x5800_0000, not 0x5000_0000, and the difference is a real collision I nearly shipped.** On 32-bit
/// `scheduler::TASK_HEAP_VA_START` is 0x5000_0000 - the base every task's dynamic `AllocMem` grows from.
/// A 1824x984 framebuffer is ~7 MiB, so the console service's first few allocations would have been
/// mapped straight on top of the display it was rendering into. It survived only because that service
/// allocates nothing today; the first `AllocMem` in it would have corrupted the screen, or worse, in a
/// way that looked like a rendering bug.
///
/// This address sits between the heap base and `DRIVER_MMIO_VA` (0x6000_0000), leaving 128 MiB of heap
/// room below it and clear of the user stack (0x8000_0000 down) above - room for any framebuffer a
/// display we can drive will have.
pub const FB_VA: u64 = 0x5800_0000;

/// VA where the driver's physically-contiguous DMA arena is mapped (8 GiB).
pub const XHCI_DMA_VA:     u64 = 0x2_0000_0000;

/// Per-driver DMA-arena physical base, allocated ONCE on the first spawn and REUSED across every
/// respawn (§12, the DMA permanent-reserve net). `allocator::alloc_dma_arena` reserves the run out of
/// the general pool so it is never recycled into a page table; keeping the phys here makes the
/// reservation bounded - one arena per driver, reused, rather than one allocated per spawn. So a stray
/// device DMA (if the kill-path bus-master quiesce ever fails) always lands in DMA-reserved memory,
/// never a PTE or kernel struct. 0 = not yet allocated. (xhci/ehci/block-driver; a future NIC = 4th.)
pub static XHCI_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// The arm32 DWC2's permanent DMA reservation, reused across respawns like every other class.
pub static DWC2_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
pub static EHCI_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
pub static AHCI_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
pub static NIC_DMA_PHYS:  portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
/// Pages of contiguous DMA memory for the **xHCI** driver. The first 32 pages
/// hold the control structures (command/event rings, DCBAA, ERST) and the six
/// per-device 4-page slices, plus the scratchpad buffer array at page 31; the
/// remaining 256 pages are the scratchpad buffers the controller DMAs into (real
/// AMD xHCI reports MaxScratchpadBufs=256 - 1 MiB - and malfunctions without
/// them). Six slices (up from two) so hub enumeration can address the hub AND its
/// downstream devices at once (docs/usb-hub.md). Confined identity-mapped, so the
/// device reaches all of it (§12, H1).
/// Plus 4 pages at the tail for the USB MASS-STORAGE region (`services/xhci/src/msc.rs`
/// `DISK_BASE`): the two bulk transfer rings, the CBW/CSW page, and one data page. They sit past the
/// scratchpad rather than sharing any earlier page ON PURPOSE - the Pi 4 port lost days to one DMA
/// page owned by a keyboard report, a hub's port status AND the disk's CBW at once, where an armed
/// interrupt endpoint overwrote a command mid-flight on every keypress. Four pages of arena buys
/// that class of bug being unrepresentable.
const XHCI_DMA_PAGES:      u64 = 32 + 256 + 4;
/// Pages of contiguous DMA memory for the **EHCI** driver - 64 KiB, as on main.
/// EHCI has no scratchpad concept, and its driver zeroes the whole arena on every
/// control transfer; giving it the xHCI-sized 1 MiB arena (a leftover of sharing
/// one constant) regressed back-port enumeration. Keep it small and separate.
const EHCI_DMA_PAGES:      u64 = 16;

/// Maximum named send peers per service.
/// Send peers a service may be wired with.
///
/// RAISED 4 -> 6 (2026-08-29). The shell legitimately needs five (`fs`, `block-driver`, `time`,
/// `console`, `logger`), and at four the fifth was dropped SILENTLY - the contract declared it, the
/// service never got the cap, and the only symptom was a peer that behaved as though it did not exist.
/// The cap itself is right (a fixed array, §26.6); the silence was the bug, and the loud reject below
/// is the other half of this fix.
pub const MAX_SEND_PEERS:  usize = 6;
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
    xhci_mmio_len:      u64, // length of the mapped MMIO register window in bytes (SEC-4)
    xhci_dma_va:        u64, // 0 = none; else VA of the driver's DMA arena (§12)
    xhci_dma_phys:      u64, // physical base of the DMA arena (programmed into the device)
    xhci_dma_len:       u64, // length of the DMA arena in bytes
    console_push_slot:  u32, // u32::MAX = none; else CONSOLE_PUSH cap slot (input driver)
    self_grant_slot:    u32, // u32::MAX = none; else SEND|GRANT cap to this service's OWN
                             // endpoint, so it can register its name in the kernel directory.
    // --- Framebuffer grant (the `console` service only) ---
    // The kernel maps the display's framebuffer into this service's address space Normal NON-cacheable
    // + USER, as a driver's MMIO BAR is mapped, and describes it here. Deliberately PIXEL geometry only:
    // no rows, no columns, no cell size. Character geometry belongs to the terminal, and the terminal is
    // the service (`docs/console-service.md` 9.7).
    fb_va:              u64, // 0 = no framebuffer grant; else VA of the mapped framebuffer
    fb_len:             u64, // length of the mapping in bytes (pitch * height)
    fb_pitch:           u32, // bytes per scanline
    fb_width:           u32, // visible width in pixels
    fb_height:          u32, // visible height in pixels
    fb_bpp:             u32, // bytes per pixel
    fb_shifts:          u32, // r_shift | g_shift << 8 | b_shift << 16
    send_peers:         [SendPeerEntry; MAX_SEND_PEERS],
    /// A SECOND endpoint, for REPLIES only. `u32::MAX` = none.
    ///
    /// A service that serves clients on the endpoint it also awaits replies on cannot drain that
    /// endpoint while it is blocked for a reply. Sixteen client requests arrive, the queue is full,
    /// and the reply it is waiting for is DROPPED by a peer that (correctly) uses `try_send` rather
    /// than deadlocking. The wait then runs to its full deadline - 30 s per block operation on x86,
    /// which is what made `write append` take 73 seconds.
    ///
    /// Correlation tags cannot reach this: a tag identifies a reply that ARRIVED, and this one never
    /// did. `docs/net-tags-design.md` rejected a second endpoint for lacking a `CreateEndpoint`
    /// syscall - true, and not needed: the first endpoint is minted at spawn and so is this one.
    reply_recv_slot:    u32,
    /// SEND|GRANT cap to `reply_recv_slot`'s endpoint, for handing out as a reply cap. `u32::MAX` = none.
    reply_grant_slot:   u32,
}

// The kernel writes this struct and the SDK reads it, from two crates, with no shared definition -
// they are kept in step BY HAND. There was no check on that, and adding a field to one and not the
// other silently misaligns every field after it: a service would read its neighbour's slot numbers.
//
// Pinned by SIZE in both crates. It does not prove field ORDER, but it catches the mistake that
// actually happens - an append on one side only - and it fails at compile time in the crate that
// drifted rather than at boot in a service that reads garbage.
const SERVICE_CONTEXT_DATA_SIZE: usize = 320;   // 256 + 2 x SendPeerEntry(32) after MAX_SEND_PEERS 4 -> 6
const _: () = assert!(
    core::mem::size_of::<ServiceContextData>() == SERVICE_CONTEXT_DATA_SIZE,
    "ServiceContextData changed size: update BOTH kernel/src/task/mod.rs and      sdk/rust/src/service_context.rs, then update SERVICE_CONTEXT_DATA_SIZE in both"
);


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
    /// An explicitly-requested core (contract `placement.core` / `spawn_on`) is not ready (§9.2). The
    /// spawn is rejected rather than rerouted; the caller (e.g. the supervisor) may retry elsewhere.
    PlacementInvalid,
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

/// The discovered-PCI-device class a driver service is granted (audit M7 / T1 Phase B). This is the
/// single DECLARED hardware fact the spawn path drives every MMIO / DMA / IOMMU / bus-master grant off
/// - replacing the old scatter of `name == "block-driver" && pci::AHCI_FOUND` checks repeated across the
/// spawn path. The BAR *address* is still runtime-discovered by the PCI scan (a hardware location is a
/// different irreducible fact from the authorization); only the driver's *class* is declared here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HwClass { None, Ahci, Nic, Xhci, Ehci, Dwc2, Framebuffer }

impl HwClass {
    /// Did the PCI scan find this class of controller?
    fn found(self) -> bool {
        use crate::arch::imp::pci;
        use core::sync::atomic::Ordering::Relaxed;
        match self {
            // The DWC2 is SOLDERED to the BCM283x - there is no bus to discover it on, and no PCI
            // at all on this board. Its presence is a property of the SoC, so it is `true` on arm32
            // and `false` everywhere else. This is the one HwClass whose answer is not a scan result.
            HwClass::Dwc2 => cfg!(target_arch = "arm"),
            // Not a bus device at all: the display is found at boot (a Limine descriptor on x86, a GPU
            // mailbox call on the Pi) and the floor that brought it up is the one that knows.
            HwClass::Framebuffer => crate::bootcon::grant().is_some(),
            HwClass::Xhci => pci::XHCI_FOUND.load(Relaxed),
            HwClass::Ehci => pci::EHCI_FOUND.load(Relaxed),
            HwClass::Ahci => pci::AHCI_FOUND.load(Relaxed),
            HwClass::Nic  => pci::NIC_FOUND.load(Relaxed),
            HwClass::None => false,
        }
    }
    /// The controller's first MMIO BAR base, or 0 if absent (or, for a NIC, not a model we can drive -
    /// an Intel e1000 or a Realtek RTL8168; on any other NIC the driver gets no mapping and idles).
    fn mmio_bar(self) -> u64 {
        use crate::arch::imp::pci;
        use core::sync::atomic::Ordering::Relaxed;
        if !self.found() { return 0; }
        match self {
            // ZERO, deliberately - and this is not "no MMIO".
            //
            // A non-zero BAR sends the spawn path down the PCI route, which maps at `XHCI_MMIO_VA`
            // (0x1_0000_0000). That address does not EXIST on a 32-bit machine, and the failure is
            // exactly as blunt as it sounds:
            //
            //   spawn[mmio]: 'dwc2' BAR 0x3f980000 -> VA 0x100000000
            //   task: spawn 'dwc2' failed: MapFailed
            //
            // ARM's fixed-address peripherals have their own path - `map_fixed_driver_mmio`, which
            // the spawn logic calls precisely WHEN THE BAR IS 0, and which maps at a 32-bit VA with
            // Device/uncached + USER so the service reaches the registers through the SDK's safe
            // `Mmio` wrapper. The DWC2's address therefore belongs there, not here. Returning 0 is
            // how this class says "not on a bus" rather than "not present" - `found()` above is the
            // one that answers presence.
            HwClass::Dwc2 => 0,
            // ZERO for the same reason as the DWC2: not on a bus, so not a BAR. The framebuffer has its
            // own grant path (the `HwClass::Framebuffer` branch in the spawn MMIO block) because it
            // needs geometry as well as a window, and because its size is whatever the display turned
            // out to be.
            HwClass::Framebuffer => 0,
            HwClass::Xhci => pci::XHCI_MMIO_BASE.load(Relaxed),
            HwClass::Ehci => pci::EHCI_MMIO_BASE.load(Relaxed),
            HwClass::Ahci => pci::AHCI_ABAR.load(Relaxed),
            HwClass::Nic if matches!(pci::NIC_VENDOR_DEVICE.load(Relaxed), 0x100E_8086 | 0x8168_10EC)
                => pci::NIC_MMIO_BASE.load(Relaxed),
            _ => 0,
        }
    }
    /// A discovered DMA-capable controller needs a physically-contiguous DMA arena.
    fn needs_dma(self) -> bool {
        // The framebuffer is not a DMA master - the display scans it, this service only writes it - so
        // it gets a window and no arena. Everything else that is `found()` DMAs.
        self != HwClass::None && self != HwClass::Framebuffer && self.found()
    }
    /// Arena size: xHCI needs room for its 256-buffer scratchpad; every other driver gets 64 KiB.
    fn dma_pages(self) -> u64 { if self == HwClass::Xhci { XHCI_DMA_PAGES } else { EHCI_DMA_PAGES } }
    /// The permanent per-class DMA phys reservation, reused across respawns (§12 DMA permanent-reserve).
    fn dma_phys_slot(self) -> &'static portable_atomic::AtomicU64 {
        match self {
            HwClass::Dwc2 => &DWC2_DMA_PHYS,
            HwClass::Xhci => &XHCI_DMA_PHYS,
            HwClass::Ehci => &EHCI_DMA_PHYS,
            HwClass::Ahci => &AHCI_DMA_PHYS,
            HwClass::Nic  => &NIC_DMA_PHYS,
            // Neither DMAs, so neither ever reaches here - `needs_dma()` gates every caller. They
            // return a real slot rather than panicking because an unreachable arm that aborts is a
            // crash waiting for a refactor, and a wrong-but-unused reservation is not.
            HwClass::None | HwClass::Framebuffer => &XHCI_DMA_PHYS,
        }
    }
    /// Confine this DMA-capable driver via the IOMMU? Only xHCI qualifies today (§6.4; ehci + block-driver
    /// keep a stale firmware DMA pointer that confinement would fault, so they stay in passthrough).
    fn iommu_confine(self) -> bool { self == HwClass::Xhci }
    /// The device's PCI BDF (bus/device/function) for the bus-master + D0 enable, or 0xFFFF if none.
    fn bdf(self) -> u32 {
        use crate::arch::imp::pci;
        use core::sync::atomic::Ordering::Relaxed;
        match self {
            HwClass::Dwc2 => 0xFFFF, // no PCI on this board, so no bus-master enable to perform
            HwClass::Framebuffer => 0xFFFF, // not a PCI device
            HwClass::Xhci => pci::XHCI_BDF.load(Relaxed),
            HwClass::Ehci => pci::EHCI_BDF.load(Relaxed),
            HwClass::Ahci => pci::AHCI_BDF.load(Relaxed),
            HwClass::Nic  => pci::NIC_BDF.load(Relaxed),
            HwClass::None => 0xFFFF,
        }
    }
}

/// The core each USB host-controller driver is pinned to - the SINGLE SOURCE OF TRUTH for USB driver
/// placement. Both the `ServiceConfig.preferred_core` below AND the kernel's MSI/INTx destination
/// routing (`arch::x86_64::pci`) read these, so a controller interrupt is always delivered to the core
/// the driver actually runs on (§12). Co-location is required for interrupt-driven USB (docs/power.md):
/// a keypress MSI must wake the driver's OWN core out of its idle `hlt` locally, because a cross-core
/// wake to a halted AP is not serviced promptly on this hardware. Both sit on cores 2/3 (off core 1)
/// because busy-polling two controllers on one core saturated it; when they block, that can relax.
pub const XHCI_CORE: u32 = 2;
pub const EHCI_CORE: u32 = 3;

/// The hardware class + resource-mint authority a service is granted, keyed by name. This is the ONE
/// place the kernel declares which driver drives which discovered device and which service may mint
/// delegated resource caps (§7.10) - the spawn path reads it, never a scattered `name ==` check (audit
/// M7 / T1 Phase B). For the services that ship a `.toml`, `scripts/contract_check.py` reconciles this
/// against the contract's `hw_device` / `resource_mint`, so the kernel and the contract cannot diverge
/// (Commandment III). `xhci` / `ehci` / `resource-server` have no contract and are declared here only.
fn service_hw(name: &str) -> (HwClass, bool) {
    match name {
        "dwc2"                                 => (HwClass::Dwc2, false),
        "console"                              => (HwClass::Framebuffer, false),
        "xhci"                                 => (HwClass::Xhci, false),
        "ehci"                                 => (HwClass::Ehci, false),
        "block-driver"                         => (HwClass::Ahci, false),
        "nic-driver" | "e1000"                 => (HwClass::Nic,  false),
        // `resource-server` moved to the supervisor (step C): its RESOURCE_MINT now arrives in the
        // spawn request, checked against what the SUPERVISOR holds, instead of being granted here by
        // name. `fs` and `net-stack` still take the by-name path until they move too.
        "fs" | "net-stack" => (HwClass::None, true),
        _                                      => (HwClass::None, false),
    }
}

/// The non-hardware system capabilities a service is granted at spawn (audit U15). Like `service_hw`,
/// this is the ONE place the kernel declares who holds each privileged authority - previously six
/// separate `name == "shell" || name == "supervisor" || ...` blocks scattered down the spawn path, each
/// its own drift risk. Centralizing them here mirrors the `service_hw` doctrine (§26.4 no scattered
/// authority; IV honor contracts declaratively): the spawn path reads these booleans, never a re-derived
/// `name ==` check. `is_probe` (the caller's ELF is `PROBE_ELF`) covers the whole test-probe family by
/// identity, so no probe is missed by name.
/// Bit positions for `SpawnRequest::privileges`. One bit per field of `Privileges` below, in
/// declaration order, so the wire form and the struct cannot drift apart silently.
///
/// A caller may only request bits it HOLDS ITSELF (`privileges_caller_lacks`), which is what keeps
/// this from being ambient authority (3.1): a spawner passes on what it has, it does not mint.
pub mod privbits {
    pub const SPAWN:           u32 = 1 << 0;
    pub const CONSOLE_PUSH:    u32 = 1 << 1;
    pub const INTROSPECT:      u32 = 1 << 2;
    pub const SERVICE_CONTROL: u32 = 1 << 3;
    pub const FIRE_IRQ:        u32 = 1 << 4;
    pub const REBOOT:          u32 = 1 << 5;
    pub const ACQUIRE_ANY:     u32 = 1 << 6;
    pub const RESOURCE_MINT:   u32 = 1 << 7;
    /// Every bit this kernel understands. Anything outside it is refused, so a newer spawner cannot
    /// quietly ask for a privilege this kernel would ignore.
    pub const KNOWN: u32 = SPAWN | CONSOLE_PUSH | INTROSPECT | SERVICE_CONTROL
                         | FIRE_IRQ | REBOOT | ACQUIRE_ANY | RESOURCE_MINT;
}

/// Which requested privilege the CALLING task does not itself hold, if any.
///
/// `None` means every requested bit is one the caller could already exercise, so passing it to a
/// child grants nothing new - the same non-escalation argument as an installed cap (7.3).
pub fn privileges_caller_lacks(requested: u32) -> Option<&'static str> {
    use crate::capability::*;
    if requested & !privbits::KNOWN != 0 { return Some("an unknown privilege bit"); }
    let checks: [(u32, ResourceId, &'static str); 8] = [
        (privbits::SPAWN,           SPAWN_RESOURCE,           "SPAWN"),
        (privbits::CONSOLE_PUSH,    CONSOLE_PUSH_RESOURCE,    "CONSOLE_PUSH"),
        (privbits::INTROSPECT,      INTROSPECT_RESOURCE,      "INTROSPECT"),
        (privbits::SERVICE_CONTROL, SERVICE_CONTROL_RESOURCE, "SERVICE_CONTROL"),
        (privbits::FIRE_IRQ,        FIRE_IRQ_RESOURCE,        "FIRE_IRQ"),
        (privbits::REBOOT,          REBOOT_RESOURCE,          "REBOOT"),
        (privbits::ACQUIRE_ANY,     ACQUIRE_ANY_RESOURCE,     "ACQUIRE_ANY"),
        (privbits::RESOURCE_MINT,   RESOURCE_MINT_RESOURCE,   "RESOURCE_MINT"),
    ];
    for (bit, res, label) in checks {
        // GRANT, not WRITE. Delegating an authority and EXERCISING it are different rights (7.4), and
        // conflating them would force the supervisor to hold every privilege it might ever pass on -
        // a maximally-privileged supervisor that could mint resources, reboot the machine and inject
        // keystrokes, purely so it could delegate those things. With GRANT-only caps it can pass them
        // on and never use them, which is strictly less authority than the alternative.
        if requested & bit != 0 && !scheduler::current_task_holds_resource(res, Rights::GRANT) {
            return Some(label);
        }
    }
    None
}

/// Privileges the SUPERVISOR may DELEGATE to a service it spawns, without being able to use them.
///
/// Minted GRANT-only: `resource_mint` (and every other privileged syscall) checks for WRITE, so the
/// supervisor cannot exercise any of these - it can only pass them to a child. That is the whole
/// point: a spawner needs the right to DELEGATE authority, not the authority itself.
///
/// This is not new reach. The supervisor could already start `fs`, which holds RESOURCE_MINT by name;
/// being able to name the privilege changes which BINARY receives it, not whether the privilege can
/// be obtained at all - and step 2's signing is what re-anchors "which binary" (docs/service-ownership.md).
const SUPERVISOR_DELEGATABLE: &[(u32, crate::capability::cap::ResourceId)] = &[
    (privbits::RESOURCE_MINT,   RESOURCE_MINT_RESOURCE),
    (privbits::INTROSPECT,      INTROSPECT_RESOURCE),
    (privbits::SERVICE_CONTROL, SERVICE_CONTROL_RESOURCE),
    (privbits::SPAWN,           SPAWN_RESOURCE),
    (privbits::ACQUIRE_ANY,     ACQUIRE_ANY_RESOURCE),
    (privbits::CONSOLE_PUSH,    CONSOLE_PUSH_RESOURCE),
    (privbits::FIRE_IRQ,        FIRE_IRQ_RESOURCE),
    (privbits::REBOOT,          REBOOT_RESOURCE),
];

struct Privileges {
    spawn:           bool, // SPAWN: create tasks (supervisor, the shell, chaos' spawn-burst, probes)
    console_push:    bool, // CONSOLE_PUSH: inject keystrokes into the input ring (USB keyboard drivers)
    introspect:      bool, // INTROSPECT: read another task's / system-wide kernel state (§3.1)
    service_control: bool, // SERVICE_CONTROL: kill/restart other services (§14.4)
    fire_irq:        bool, // FIRE_IRQ: inject a test interrupt (`control` only - C1-6)
    reboot:          bool, // REBOOT: hardware-reset the machine (shell `reboot` only - SEC-2)
    acquire_any:     bool, // ACQUIRE_ANY: reach ARBITRARY services by name via AcquireSendCap (§3.1)
    net_device:      bool, // NET_DEVICE: move ethernet frames via the in-kernel USB-net bridge (ARM nic-driver)
    usb_disk:        bool, // USB_DISK: read/write blocks on the in-kernel USB mass-storage device (ARM block-driver)
    gpio:            bool, // GPIO_DEVICE: drive the SoC GPIO pins (ARM `gpio` shell command)
    set_clock:       bool, // SET_CLOCK (WRITE): set the wall clock from SNTP (RTC-less ARM; net-stack)
    set_clock_floor: bool, // SET_CLOCK (READ): raise the persisted clock floor only (the shell)
}

fn service_privileges(name: &str, is_probe: bool) -> Privileges {
    Privileges {
        // supervisor is the spawner (init removed, Phase 5); the shell brokers spawns; chaos spawns
        // mem-pressure tasks for max-carnage's spawn-burst dimension; probes spawn victims.
        spawn: is_probe || matches!(name, "supervisor" | "shell"),
        // Both USB host drivers push decoded keystrokes: xhci (front ports), ehci (USB 2.0 back ports).
        // `dwc2` is the arm32 USB keyboard driver, so it needs this for the same reason `xhci` and
        // `ehci` do. Its absence was the whole of "the keyboard does not work": the transfers were
        // right, the reports were valid HID boot reports (`00 00 0d 0c 12 ...` - real keycodes), and
        // every `console_push` was rejected because the service had never been granted the authority.
        //
        // Deliberate and not merely mechanical (§6.4, SEC-2): a CONSOLE_PUSH holder is inside the
        // SHELL'S TRUST PERIMETER, because keystrokes are commands and the kernel cannot distinguish a
        // faithfully-decoded key-press from a synthesized one. That is inherent to being a keyboard
        // driver and is why the grant is enumerated here by name rather than implied by holding a
        // USB controller.
        console_push: matches!(name, "xhci" | "ehci" | "dwc2"),
        // shell + observe use TaskStat/InspectKernel; supervisor's reconcile loop scans real liveness;
        // chaos does victim selection; the prop-/stress- probes query victim generations.
        // EXACT names, not a prefix. `name.starts_with("observe")` granted INTROSPECT to anything
        // whose name began with those letters - harmless while the kernel owned every name, and a
        // privilege obtainable by CHOOSING A STRING once a caller supplies them (step C). The
        // `observe*` trio is now enumerated; when they move, the bit comes in the spawn request and
        // these names leave too.
        //
        // The `prop-`/`stress-` prefixes remain and are a KNOWN HOLE, recorded rather than papered
        // over (26.7): probe names ARE caller-supplied since step A, so a service holding a spawn cap
        // can spawn a probe named `prop-x` and receive INTROSPECT. Narrow - the caller gets the fixed
        // probe binary running a fixed test mode, not arbitrary code - but it is authority obtained
        // by choosing a string. It cannot simply become `is_probe`: adv-a11 asserts that a probe
        // WITHOUT INTROSPECT is denied, so widening it to every probe would delete that test's
        // subject. The real fix is for `spawn_probe` to carry a privilege word like `SpawnImage`
        // does, checked against what the CALLER may delegate. See docs/service-ownership.md.
        introspect: matches!(name, "shell" | "supervisor")
            || name.starts_with("prop-") || name.starts_with("stress-"),
        // shell (interactive broker), supervisor (restart authority), chaos (the point of max-carnage),
        // and every probe (they kill victims to exercise kill/revocation).
        service_control: is_probe || matches!(name, "shell" | "supervisor"),
        // SEC-2: REBOOT lives ONLY with the shell (its `reboot` command); the USB drivers no longer
        // hold it. A keyboard driver can synthesize any keystroke (the console's inherent trust, §6.4),
        // but it must not ALSO be able to hard-reset the machine directly from any context.
        // FIRE_IRQ: only the control service. It exists so the COM2 command interpreter could leave
        // the kernel; naming the authority is what made that possible (C1-6).
        fire_irq: false, // `control` carries FIRE_IRQ in its spawn request now (step C)
        reboot: matches!(name, "shell"),
        // Operator/test instruments that legitimately reach arbitrary services by name: shell (chaos
        // flooding, pipe sinks), supervisor (reconcile-by-name), probes. `adv-a13` is the §22 Test A13
        // NEGATIVE pin - deliberately excluded so it holds no ACQUIRE_ANY (proves AcquireSendCap denies
        // a non-holder). Ordinary services get none; their AcquireSendCap is limited to declared peers.
        acquire_any: (is_probe && name != "adv-a13") || matches!(name, "shell" | "supervisor"),
        // NET_DEVICE, GPIO_DEVICE, USB_DISK and SET_CLOCK are SANCTIONED KERNEL-ONLY BY-NAME GRANTS (the U15 / userspace-audit
        // A5-U1 doctrine): they are deliberately NOT contract capabilities - the kernel is their single
        // source of truth, and `contract_check.py` does not reconcile them. Both are arch-gated to ARM
        // (off ARM the syscalls are inert stubs; SEC-31) so no dormant authority is handed out elsewhere.
        // nic-driver (which DOES ship a contract) carries an ARM note in nic-driver.toml so a contract
        // reader is not misled; the shell ships no contract, so the kernel is trivially its only record.
        //   nic-driver bridges ethernet frames to/from the in-kernel USB-net device (NetFrame*, 42-44).
        // aarch64 joins arm here: the Pi 4's GENET driver backs the same NET_DEVICE syscalls the Pi 2's
        // in-kernel USB-net bridge does, so `nic-driver` needs the same grant to reach it. Without it
        // the service loads and runs and every frame call is denied, which looks like a dead network
        // rather than a missing capability.
        // ARM32 has LEFT this set: its USB-net device moved into the `dwc2` SERVICE (slice 4b), so
        // nic-driver reaches frames over IPC and the syscalls have nothing behind them. Keeping the
        // grant would be authority it cannot use - the exact over-grant the audits keep finding.
        net_device: cfg!(target_arch = "aarch64") && matches!(name, "nic-driver"),
        // USB_DISK: `block-driver` reaches a USB stick through syscalls 46-48 rather than MMIO, on
        // the port where the USB stack is IN THE KERNEL - which is now ARM32 (Pi 2) ONLY. On aarch64
        // the in-kernel driver was deleted (CLAUDE.md §6.4, 2026-08-09) and block-driver goes through
        // the `xhci` SERVICE over IPC, so the grant buys it nothing there.
        //
        // KEPT for aarch64 all the same, deliberately, and this is the honest reason: the syscalls
        // still EXIST on that port as stubs, and a grant that matches where the mechanism lives is
        // easier to reason about than one that does not. It is also a vestigial authority (audit
        // SEC-37) - whole-device read/write reach handed to a service that no longer uses it - so the
        // right end state is to delete the aarch64 stubs and narrow this to `target_arch = "arm"`.
        // Recorded rather than done, because removing syscalls is a separate change with its own test.
        //
        // (The original note here claimed the stack is in-kernel on BOTH ARM ports. That was true when
        // it was written and stopped being true when the aarch64 driver was deleted.)
        usb_disk: cfg!(any(target_arch = "arm", target_arch = "aarch64"))
            && matches!(name, "block-driver"),
        //   the shell's `gpio` command drives the SoC pins (the gated `Gpio` syscall, 45).
        gpio: cfg!(target_arch = "arm") && matches!(name, "shell"),
        //   SET_CLOCK, in two strengths (rights narrow, §7.4). WRITE = set the wall clock itself, held only
        //   by net-stack, which runs the SNTP exchange (the RTC-less ARM port has no other time source).
        //   READ = raise the persisted clock FLOOR only, held by the shell, which reads the last-known time
        //   off the disk at startup and records it before a reboot. The shell needs the bound, not the
        //   clock, so it does not get the power to step every task's time of day. A kernel-only by-name
        //   grant like NET_DEVICE (not a contract cap). ARM-gated: x86's CMOS RTC is the authority there.
        // aarch64 joins arm for the same reason arm has it: the Pi 4 has no RTC either, so SNTP is the
        // only source of a wall clock. Without the grant net-stack does the whole query, gets a real
        // answer, and is refused at the last step - the clock stays at the boot epoch while the log
        // says the time was fetched.
        set_clock:       cfg!(any(target_arch = "arm", target_arch = "aarch64"))
            && matches!(name, "net-stack"),
        // aarch64 joins arm: the Pi 4 has no RTC either, so the floor the shell persists to
        // /clock.last is what carries a network sync across a power cycle. READ, not WRITE - raising
        // the floor only constrains which clock values are acceptable, where WRITE would let the shell
        // step every task's view of the time of day. The narrower right already existed here; granting
        // the shell plain `set_clock` instead would have handed it exactly the authority this split was
        // built to withhold, and would have failed anyway - a WRITE cap does not satisfy a READ check.
        set_clock_floor: cfg!(any(target_arch = "arm", target_arch = "aarch64"))
            && matches!(name, "shell"),
    }
}

/// True if the calling task's contract declares `peer` as a send-peer (§13) - so reacquiring a SEND
/// cap to it (`AcquireSendCap`) is contract-authorized recovery (§14.2), not ambient authority (§3.1).
/// The caller's name comes from the existing `task_stat` snapshot and its declared peers from the
/// static `service_config`, so this adds no new per-task kernel state and no new `unsafe`.
/// Did the calling task declare `peer` as a send-peer at spawn?
///
/// Reads what the task was ACTUALLY WIRED WITH, recorded by the spawn path, rather than looking the
/// service up in the kernel catalogue. The catalogue answer is wrong for any service whose config has
/// moved to the supervisor (step C): it says "declares nothing", which denied a supervisor-owned
/// service the reacquire it needs after a peer restarts (14.3) - `ping` could never re-find `pong`.
pub fn current_task_declares_peer(peer: &str) -> bool {
    scheduler::task_declares_peer(scheduler::current_task_slot(), peer)
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
        // The terminal (docs/console-service.md 9). Holds the framebuffer grant (`service_hw`) and
        // renders every console byte the kernel `try_send`s to its endpoint.
        //
        // ARM: core 3, and NOT core 0. I put it on core 0 first, reasoning that a console write is what
        // the user is waiting to see so it should sit with the shell - and that was wrong twice over.
        // The shell is on core 1, and core 0 is deliberately left to `dwc2` ALONE (see the note on
        // net-stack below, and the logger's, which was moved off core 0 for exactly this).
        //
        // It matters more here than it did for the logger. A full-screen repaint is millions of
        // non-cacheable pixel stores in one un-preemptible stretch, and dwc2's split transactions have
        // to hit 125 us microframe windows. Sharing a core with it produced NYET storms and two
        // "keyboard TT wedged" stalls, one of them six seconds long, on the first boot that had a
        // terminal to starve it.
        //
        // Core 3 rather than 2 because the logger and block-driver are on 2; the terminal is the one
        // service whose latency the user watches directly, so it gets the idle core.
        "console" => Some(("console", ServiceConfig {
            elf:               include_bytes!(env!("SVC_CONSOLE_ELF")),
            has_recv_endpoint: true, // the console byte stream AND geometry requests arrive here
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    if cfg!(target_arch = "arm") { 3 } else { 0 },
            probe_mode:        0,
            memory_limit:      8 * 1024 * 1024, // matches console.toml
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // xhci - USB host-controller driver (§12). Receives its controller's
        // MMIO BAR (mapped by name in the spawn path) + later its IRQ. Trusted
        // userspace driver. has_recv_endpoint for future interrupt delivery.
        // `dwc2` - the arm32 userspace USB host driver (Phase 2 skeleton).
        //
        // Granting `hw_irqs = [0x29]` is what makes `arm_irq_dispatch` route the USB interrupt HERE
        // instead of to the in-kernel stack: the dispatcher picks by registration, so spawning this
        // service is what takes the controller away from the kernel. Deliberate, and the whole point
        // of the phase - but it means USB is expected to be degraded while a skeleton holds the
        // vector. See docs/arm32-usb-userspace.md.
        //
        // Core 0: the DWC2 interrupt is routed to core 0 by `route_usb_irq_to_core0`, and a driver
        // that receives its interrupt on one core while running on another pays a cross-core wake for
        // every single one. The Pi 4 learned this the expensive way - `xhci`'s MSI destination had
        // drifted to a core the driver did not run on, and co-locating them was what took it from
        // 100% CPU to 0%.
        "dwc2" => Some(("dwc2", ServiceConfig {
            elf:               include_bytes!(env!("SVC_DWC2_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            preferred_core:    0,
            probe_mode:        0,
            memory_limit:      64 * 1024 * 1024,
            hw_irqs:           &[0x29],
            has_console_read:  false,
        })),
        "xhci" => Some(("xhci", ServiceConfig {
            elf:               include_bytes!(env!("SVC_XHCI_ELF")),
            has_recv_endpoint: true,
            send_peers:        &[],
            send_peers_grant:  false,
            // Core 2: the USB drivers busy-poll their controllers at ~100% CPU, so co-locating both
            // on core 1 (with nic-driver + net-stack + fs + block-driver) SATURATED it - starving the
            // networking (net-stack's frame requests to nic-driver timed out) and the keyboard itself
            // (input garbled then died on the T630). Spreading the two busy-pollers onto the idle cores
            // (xhci=2, ehci=3) leaves core 1 for the request-driven services. Falls back to round-robin
            // if core 2 is not ready.
            preferred_core:    XHCI_CORE,
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
            preferred_core:    EHCI_CORE,   // core 3: the other busy-poller, off the saturated core 1 (see xhci)
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
            // With the USB stack in userspace, block-driver reaches the disk by IPC to the `xhci`
            // SERVICE - so it needs a SEND cap to that name. Without one, `request_with_reply("xhci",
            // ..)` finds no send slot and returns None INSTANTLY. That is exactly what the Pi 4
            // showed: the service sat in its poll loop with the disk bound, never receiving a single
            // message, while block-driver burned its whole 20 s wait failing to address it. Every
            // layer looked healthy in isolation, because the missing piece was the EDGE between them.
            //
            // block-driver reaches the disk THROUGH a USB host-controller SERVICE on both ARM
            // targets now: `xhci` on aarch64, `dwc2` on arm32. The arm32 arm used to be empty, with a
            // comment saying "the USB stack is still in the kernel... so there is no such peer" -
            // true when it was written and false since the driver moved out.
            //
            // The paragraph above is the reason this edge cannot be left to be noticed later: without
            // the SEND cap, `request_with_reply` finds no send slot and returns None INSTANTLY, so
            // the driver sits in its poll loop with the disk bound and never receives a message while
            // block-driver reports no storage. Every layer healthy in isolation, and the missing
            // piece the edge between them.
            #[cfg(target_arch = "aarch64")]
            send_peers:        &["xhci"],
            #[cfg(target_arch = "arm")]
            send_peers:        &["dwc2"],
            #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
            send_peers:        &[], // Path C: recorded in the kernel directory at spawn; no peers
            send_peers_grant:  false,
            // ARM KEEPS THIS ON CORE 0 - and the reason is neither of the two written here before.
            // Both were wrong, and hardware refuted each in turn.
            //
            //   1. "The `msc_*` syscalls refuse when `!on_core0()`." True until slice 5 deleted the
            //      in-kernel DWC2 stack; now provably false (no `on_core0` reference remains in
            //      `arch/arm`, and `usb_disk_*` are inert stubs).
            //   2. "Cross-core request/reply does not complete." False. Unpinned services stalled
            //      because `dwc2` DROPPED block requests when it had no disk (fixed separately), not
            //      because of cores; with that fixed they came up correctly on all four.
            //
            // The real reason is LATENCY, and it was found by an operator noticing `ls` and `read`
            // had gone sluggish: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB, so the
            // scheduler's cross-core wake does nothing and the target core does not notice a message
            // until its next 10 ms timer tick. Cross-core IPC costs up to a full quantum PER HOP, and
            // a file read is shell -> fs -> block-driver -> dwc2. Unpinning turned a same-core chain
            // into three cross-core hops and made every command visibly slow. The giveaway was that a
            // chaos run made it FASTER: respawns re-drew placement and happened to co-locate the
            // chain again.
            //
            // UNPINNED, now that the IPI exists (BCM2836 core mailboxes; proven every boot by
            // `arm32: IPI selftest PASS`). Cross-core wakes are immediate, so spreading these no
            // longer costs a 10 ms tick per hop - the reason they were pinned in the first place.
            //
            // The stronger reason is what the IPI exposed. With wakes now actually preempting, `dwc2`
            // shares core 0 with everything that gets woken, and its split-transaction sequencing is
            // microframe-timed: a start-split and its complete-split must land in specific 125 us
            // windows. Preempted between them, the transaction translator is left holding a
            // transaction nobody collects - it wedges, gets cleared, and wedges again. Hardware showed
            // that loop plainly: eight Clear_TT_Buffer calls and eight port re-enumerations in a
            // couple of minutes, which the operator experiences as the keyboard pausing.
            //
            // So moving storage and the network OFF core 0 is not load-balancing here, it is giving
            // the one timing-critical service on this board room to hit its deadlines. `dwc2` stays on
            // core 0 because that is where its interrupt is routed.
            //
            // (Superseded rationale kept below so the next reader sees what was believed and why.)
            //
            // The old reason - the in-kernel DWC2 stack's `msc_*` entry points refused when
            // `!on_core0()` - died with slice 5, and is verifiably gone (no `on_core0` reference
            // remains in `arch/arm`; `usb_disk_*` are inert stubs). Unpinning on that basis was tried
            // and it BROKE STORAGE: block-driver spawned on core 2 and logged nothing at all - not
            // even its own "no disk" line - while `fs` on core 1 never reached "serving file API"
            // behind it, and `nic-driver` on core 3 stopped after "starting". Everything left on core
            // 0 was fine.
            //
            // The real constraint is underneath: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB,
            // so a task blocked in `recv` on another core is never woken by its sender (§8.3 relies on
            // that IPI). Cross-core request/reply therefore does not complete on this port, and every
            // service that uses it has been co-located on core 0 - which is why three cores sit idle.
            //
            // So the pin stays until cross-core wakeups work, and it is now documented as a WORKAROUND
            // FOR A KERNEL GAP rather than as a property of this driver. Fixing the IPI is what
            // unpins these three, and it unpins them everywhere at once.
            // arm32: core 2. Off core 0 for the same reason as the networking pair (see nic-driver),
            // and off core 1 so a burst of disk I/O and a burst of frames do not queue behind
            // each other on one core.
            preferred_core:    if cfg!(target_arch = "arm") { 2 } else { 1 },
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
            // ARM32: the USB-net device is behind the `dwc2` SERVICE, so nic-driver needs a send cap
            // to it - the same edge `block-driver` got in slice 3c. Without it `request_with_reply`
            // finds no send slot and returns None INSTANTLY, which looks like a dead cable rather
            // than a missing grant: every layer healthy in isolation, the edge between them absent.
            #[cfg(target_arch = "arm")]
            send_peers:        &["dwc2"],
            #[cfg(not(target_arch = "arm"))]
            send_peers:        &[],
            send_peers_grant:  false,
            // ARM KEEPS THIS ON CORE 0 - and the reason is neither of the two written here before.
            // Both were wrong, and hardware refuted each in turn.
            //
            //   1. "The `msc_*` syscalls refuse when `!on_core0()`." True until slice 5 deleted the
            //      in-kernel DWC2 stack; now provably false (no `on_core0` reference remains in
            //      `arch/arm`, and `usb_disk_*` are inert stubs).
            //   2. "Cross-core request/reply does not complete." False. Unpinned services stalled
            //      because `dwc2` DROPPED block requests when it had no disk (fixed separately), not
            //      because of cores; with that fixed they came up correctly on all four.
            //
            // The real reason is LATENCY, and it was found by an operator noticing `ls` and `read`
            // had gone sluggish: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB, so the
            // scheduler's cross-core wake does nothing and the target core does not notice a message
            // until its next 10 ms timer tick. Cross-core IPC costs up to a full quantum PER HOP, and
            // a file read is shell -> fs -> block-driver -> dwc2. Unpinning turned a same-core chain
            // into three cross-core hops and made every command visibly slow. The giveaway was that a
            // chaos run made it FASTER: respawns re-drew placement and happened to co-locate the
            // chain again.
            //
            // UNPINNED, now that the IPI exists (BCM2836 core mailboxes; proven every boot by
            // `arm32: IPI selftest PASS`). Cross-core wakes are immediate, so spreading these no
            // longer costs a 10 ms tick per hop - the reason they were pinned in the first place.
            //
            // The stronger reason is what the IPI exposed. With wakes now actually preempting, `dwc2`
            // shares core 0 with everything that gets woken, and its split-transaction sequencing is
            // microframe-timed: a start-split and its complete-split must land in specific 125 us
            // windows. Preempted between them, the transaction translator is left holding a
            // transaction nobody collects - it wedges, gets cleared, and wedges again. Hardware showed
            // that loop plainly: eight Clear_TT_Buffer calls and eight port re-enumerations in a
            // couple of minutes, which the operator experiences as the keyboard pausing.
            //
            // So moving storage and the network OFF core 0 is not load-balancing here, it is giving
            // the one timing-critical service on this board room to hit its deadlines. `dwc2` stays on
            // core 0 because that is where its interrupt is routed.
            //
            // (Superseded rationale kept below so the next reader sees what was believed and why.)
            //
            // The old reason - the in-kernel DWC2 stack's `msc_*` entry points refused when
            // `!on_core0()` - died with slice 5, and is verifiably gone (no `on_core0` reference
            // remains in `arch/arm`; `usb_disk_*` are inert stubs). Unpinning on that basis was tried
            // and it BROKE STORAGE: block-driver spawned on core 2 and logged nothing at all - not
            // even its own "no disk" line - while `fs` on core 1 never reached "serving file API"
            // behind it, and `nic-driver` on core 3 stopped after "starting". Everything left on core
            // 0 was fine.
            //
            // The real constraint is underneath: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB,
            // so a task blocked in `recv` on another core is never woken by its sender (§8.3 relies on
            // that IPI). Cross-core request/reply therefore does not complete on this port, and every
            // service that uses it has been co-located on core 0 - which is why three cores sit idle.
            //
            // So the pin stays until cross-core wakeups work, and it is now documented as a WORKAROUND
            // FOR A KERNEL GAP rather than as a property of this driver. Fixing the IPI is what
            // unpins these three, and it unpins them everywhere at once.
            // arm32: core 1, and NOT unpinned.
            //
            // Unpinning these three was meant to "give the timing-critical USB driver a quieter core
            // 0" (75c18457). It did the opposite: unpinned means ROUND-ROBIN, and round-robin put
            // `net-stack` straight back onto core 0 alongside `dwc2` - observed on hardware as
            // "'dwc2' spawned OK on core 0 / 'net-stack' spawned OK on core 0".
            //
            // That is the worst possible pairing. `net-stack` waits for replies by POLLING
            // (`drain_scan` -> try_recv + yield_cpu), and the scheduler quantum is 10 ms, so it can
            // hold core 0 for 10 ms at a stretch. `dwc2`'s split transactions have to hit 125 us
            // windows - eighty times finer. So the service waiting for a DHCP or ARP reply was
            // starving the driver that had to deliver it: a spin that defeats itself.
            //
            // Pinned, and pinned TOGETHER with net-stack, which is what the contract used to say
            // before the audit deleted it ("co-located with nic-driver - the two exchange frames
            // constantly"). Same-core request/reply is safe now that the SDK waits poll rather than
            // block. Core 0 is left to `dwc2` alone, which is what the unpin was for.
            preferred_core:    if cfg!(target_arch = "arm") { 1 } else { 1 },
            probe_mode:        0,
            memory_limit:      16 * 1024 * 1024,
            // GENET's macirq on aarch64 (SPI 157 -> neutral vector 0x2A). x86's nic-driver is a
            // PCIe NIC with no such route, so the grant is arch-gated rather than unconditional.
            #[cfg(target_arch = "aarch64")]
            hw_irqs:           &[0x2A],
            #[cfg(not(target_arch = "aarch64"))]
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // net-stack (services/net-stack): the model-AGNOSTIC half of networking (docs/networking.md).
        // Owns its endpoint (nic-driver replies frames there via the per-request reply cap) and sends
        // to nic-driver (the frame interface). Spawned AFTER nic-driver so its send-peer cap wires from
        // the kernel name table at spawn. Core 1. No hardware - it speaks ARP/IP over raw frames.
        "net-stack" => Some(("net-stack", ServiceConfig {
            elf:               include_bytes!(env!("SVC_NET_STACK_ELF")),
            has_recv_endpoint: true,               // nic-driver replies frames here (per-request reply cap)
            // `time` (clock slice 2): SNTP is a network fact this service fetches; whether to BELIEVE it
            // is the clock's policy, so the reading is handed over rather than written to a syscall.
            send_peers:        &["nic-driver", "time"],    // the frame interface; reacquired by name on death
            send_peers_grant:  false,
            // ARM KEEPS THIS ON CORE 0 - and the reason is neither of the two written here before.
            // Both were wrong, and hardware refuted each in turn.
            //
            //   1. "The `msc_*` syscalls refuse when `!on_core0()`." True until slice 5 deleted the
            //      in-kernel DWC2 stack; now provably false (no `on_core0` reference remains in
            //      `arch/arm`, and `usb_disk_*` are inert stubs).
            //   2. "Cross-core request/reply does not complete." False. Unpinned services stalled
            //      because `dwc2` DROPPED block requests when it had no disk (fixed separately), not
            //      because of cores; with that fixed they came up correctly on all four.
            //
            // The real reason is LATENCY, and it was found by an operator noticing `ls` and `read`
            // had gone sluggish: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB, so the
            // scheduler's cross-core wake does nothing and the target core does not notice a message
            // until its next 10 ms timer tick. Cross-core IPC costs up to a full quantum PER HOP, and
            // a file read is shell -> fs -> block-driver -> dwc2. Unpinning turned a same-core chain
            // into three cross-core hops and made every command visibly slow. The giveaway was that a
            // chaos run made it FASTER: respawns re-drew placement and happened to co-locate the
            // chain again.
            //
            // UNPINNED, now that the IPI exists (BCM2836 core mailboxes; proven every boot by
            // `arm32: IPI selftest PASS`). Cross-core wakes are immediate, so spreading these no
            // longer costs a 10 ms tick per hop - the reason they were pinned in the first place.
            //
            // The stronger reason is what the IPI exposed. With wakes now actually preempting, `dwc2`
            // shares core 0 with everything that gets woken, and its split-transaction sequencing is
            // microframe-timed: a start-split and its complete-split must land in specific 125 us
            // windows. Preempted between them, the transaction translator is left holding a
            // transaction nobody collects - it wedges, gets cleared, and wedges again. Hardware showed
            // that loop plainly: eight Clear_TT_Buffer calls and eight port re-enumerations in a
            // couple of minutes, which the operator experiences as the keyboard pausing.
            //
            // So moving storage and the network OFF core 0 is not load-balancing here, it is giving
            // the one timing-critical service on this board room to hit its deadlines. `dwc2` stays on
            // core 0 because that is where its interrupt is routed.
            //
            // (Superseded rationale kept below so the next reader sees what was believed and why.)
            //
            // The old reason - the in-kernel DWC2 stack's `msc_*` entry points refused when
            // `!on_core0()` - died with slice 5, and is verifiably gone (no `on_core0` reference
            // remains in `arch/arm`; `usb_disk_*` are inert stubs). Unpinning on that basis was tried
            // and it BROKE STORAGE: block-driver spawned on core 2 and logged nothing at all - not
            // even its own "no disk" line - while `fs` on core 1 never reached "serving file API"
            // behind it, and `nic-driver` on core 3 stopped after "starting". Everything left on core
            // 0 was fine.
            //
            // The real constraint is underneath: `arch::arm`'s `send_ipi_to_lapic` is an EMPTY STUB,
            // so a task blocked in `recv` on another core is never woken by its sender (§8.3 relies on
            // that IPI). Cross-core request/reply therefore does not complete on this port, and every
            // service that uses it has been co-located on core 0 - which is why three cores sit idle.
            //
            // So the pin stays until cross-core wakeups work, and it is now documented as a WORKAROUND
            // FOR A KERNEL GAP rather than as a property of this driver. Fixing the IPI is what
            // unpins these three, and it unpins them everywhere at once.
            // arm32: core 1, co-located with nic-driver and OFF core 0 - see the note there.
            preferred_core:    if cfg!(target_arch = "arm") { 1 } else { 1 },
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
            // "logger" is the EMIT cap: holding it is what makes this service traced, and its absence
            // is what makes an untraced service cost one relaxed load (`sdk::trace`). Authority, visible
            // in `caps fs`, revocable - not a global switch (3.1).
            send_peers:        &["block-driver", "logger"],
            send_peers_grant:  false,
            preferred_core:    1,
            probe_mode:        0,
            memory_limit:      32 * 1024 * 1024,
            hw_irqs:           &[],
            has_console_read:  false,
        })),
        // ----------------------------------------------------------------
        // Adversarial-test probes - Milestone 13.
        // Victim/passive services must be listed before their attackers so
        // their endpoints are registered when the attacker's SEND caps are wired.
        // ----------------------------------------------------------------
        // A1: random cap slots → always Err. No caps needed.
        // THE ONE PROBE ENTRY. 193 rows used to sit here, the same binary differing by a test mode -
        // one program and a table of parameters. The parameters now come from the spawner
        // (`spawn_probe`, `docs/probe-params-design.md`); this holds only what never varied: the
        // image, and the defaults a caller may leave unset.
        "probe" => Some(("probe", ServiceConfig {
            elf:               PROBE_ELF,
            has_recv_endpoint: false,      // per-spawn: ProbeParams.has_recv_endpoint
            send_peers:        &[],        // per-spawn: the peer list in the spawn payload
            send_peers_grant:  false,      // AUTHORITY: `probe_authority`, keyed by name
            preferred_core:    u32::MAX,   // per-spawn: the core argument
            probe_mode:        0,          // per-spawn: ProbeParams.mode
            memory_limit:      64 * 1024 * 1024, // per-spawn override: ProbeParams.mem_mib
            hw_irqs:           &[],        // AUTHORITY: `probe_authority`, keyed by name
            has_console_read:  false,
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
            // `block-driver` as well as `fs`, so `drives` can ask the DEVICE about the device.
            //
            // "Is there a disk and how big" is block-driver's fact; "is it mounted, what label, how
            // free" is fs's. Routing both through `fs` made it answer a hardware question from its
            // own mount state - which is how `drives` reported 15 GB for an unplugged stick. Each
            // fact now comes from its owner (Commandment III).
            //
            // It also gives a useful answer when `fs` is dead: "disk present, filesystem
            // unavailable" instead of nothing at all (§26.7).
            // `time` (clock slice 2): the wall clock is a service, so `date` and the boot floor ask it.
            // `console`: terminal geometry, for the pager and `edit`. It used to come from the KERNEL
            // (`InspectKernel` query 9, now deleted) - the shell was asking the wrong party for a fact
            // the terminal owns (docs/console-service.md 9.7).
            send_peers:        &["fs", "block-driver", "time", "console", "logger", "supervisor"],
            send_peers_grant:  false,
            // ARM: OFF CORE 0, to keep the serial writer away from the microframe-timed USB driver.
            //
            // Measured: a 125 us sleep averages 9398 us during boot and 608 us on a quiet system - a
            // 15x difference made entirely of console traffic. A serial write is a syscall, 115200
            // baud is ~87 us per byte, and this port deliberately refuses to preempt a user task
            // mid-syscall (preempting SVC corrupts the banked SPSR/sp). So one ~100-character log
            // line holds its core, un-preemptible, for about 9 ms.
            //
            // That blocks only the core it runs on - and this service was sharing core 0 with `dwc2`,
            // whose split transactions must hit 125 us windows. Moving the writer is the cheap half
            // of the fix; it needs no console rework and it uses the cores this board has.
            preferred_core:    if cfg!(target_arch = "arm") { 1 } else { 0 },
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
fn resolve_spawn_core(core_override: Option<u32>, preferred_core: u32) -> Result<u32, SpawnError> {
    use core::sync::atomic::{AtomicU32, Ordering};
    static RR: AtomicU32 = AtomicU32::new(0);
    match core_override {
        // An explicitly-requested core (contract `placement.core`, or the supervisor's `spawn_on`) is
        // STRICT (§9.2): if it is not ready, REJECT with PlacementInvalid rather than silently placing
        // the service on a core no scheduler runs (which would strand it). On a multi-core machine the
        // requested core is ready, so this always passes and nothing changes; on a single-core machine
        // (the Pi 2, APs parked) it is what makes the supervisor's `spawn_on(x, 1)` fall back to core 0
        // instead of stranding `x` on the parked core.
        Some(n) if crate::smp::core::is_ready(n) => Ok(n),
        Some(_) => Err(SpawnError::PlacementInvalid),
        None if preferred_core == u32::MAX => {
            let count = crate::smp::core::ready_count() as u32;
            Ok(if count == 0 { 0 } else { RR.fetch_add(1, Ordering::Relaxed) % count })
        }
        None => {
            if crate::smp::core::is_ready(preferred_core) {
                Ok(preferred_core)
            } else {
                let count = crate::smp::core::ready_count() as u32;
                Ok(RR.fetch_add(1, Ordering::Relaxed) % count.max(1))
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
    let core_id = resolve_spawn_core(core_override, cfg.preferred_core)?;
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
    let result = spawn_service_with_image(static_name, crate::loader::ImageSource::Kernel(cfg.elf), core_id,
        cfg.has_recv_endpoint, &pipe_peers[..np], cfg.probe_mode, cfg.send_peers_grant,
        cfg.memory_limit, cfg.hw_irqs, cfg.has_console_read, None, None);
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn pipe '{}' -> '{}' failed: {:?}", producer, sink, e);
    }
    result.map(|_| ())
}

/// Spawn a task from an image the CALLER supplied (step C, `SpawnImage`).
///
/// This is the path that ends the kernel's service catalogue. It takes no `ServiceConfig` and looks
/// nothing up by name: the image, the name, the placement, the memory ceiling, the mailbox and the
/// peer list all arrive from the spawner, and the kernel's job is to enforce, not to decide
/// (`docs/service-ownership.md`).
///
/// What it still refuses, and why the refusal is not a leftover: a caller may not claim a name the
/// kernel's own catalogue still uses. While ANY name-keyed policy remains, letting a caller pick
/// such a name would let it inherit that policy for arbitrary code - the same squatting hole
/// `spawn_probe` closes, and for the same reason (the kernel name directory is the recovery anchor).
/// When the catalogue reaches its single `supervisor` entry this check narrows to that one name,
/// which must never be claimable by anything.
pub fn spawn_from_image(
    name:              &str,
    image:             crate::loader::ImageSource,
    core_override:     Option<u32>,
    memory_limit:      u64,
    has_recv_endpoint: bool,
    has_console_read:  bool,
    peers:             &[&str],
    // Caller-provided peer caps, GRANT-validated by the syscall. `None` = name-wire the peers
    // instead, which is the path a service with no provided caps takes.
    installs:          Option<&[InstallCap]>,
    // Privilege bits the spawner asks the child be given. Already checked against the CALLER's own
    // holdings by the syscall, so this cannot escalate (3.1, 7.3).
    privileges:        u32,
    // The service's mode selector (see `SpawnRequest::probe_mode`).
    mode:              u32,
) -> Result<Option<EndpointId>, SpawnError> {
    if service_config(name).is_some() {
        crate::kprintln!("task: SpawnImage '{}' rejected: that name belongs to the kernel catalogue", name);
        return Err(SpawnError::NotFound);
    }
    if scheduler::find_task_by_name(name).is_some() {
        crate::kprintln!("task: spawn '{}' rejected: already running", name);
        return Err(SpawnError::AlreadyRunning);
    }

    let core_id = resolve_spawn_core(core_override, u32::MAX)?;
    let mem = if memory_limit == 0 { 64 * 1024 * 1024 } else { memory_limit };

    let result = spawn_service_with_image(name, image, core_id, has_recv_endpoint, peers, mode,
                                          false, mem, &[], has_console_read,
                                          Some(privileges), installs);
    if let Err(ref e) = result {
        crate::kprintln!("task: SpawnImage '{}' failed: {:?}", name, e);
    }
    result
}

/// Parameters a SPAWNER supplies for a probe, instead of the kernel holding a row per probe.
///
/// 193 of the kernel's 221 `service_config` entries are the same `probe` binary differing by a test
/// mode - one program and a table of test parameters, and a parameter is policy (26.10). These are
/// the fields that actually varied; everything else was identical across all 193.
///
/// What is NOT here is deliberate: `hw_irqs` and `send_peers_grant` stay keyed by name in the kernel,
/// because routing an interrupt line and handing out a re-delegatable capability are AUTHORITY
/// decisions, not settings. The line is: the kernel decides what a service may DO, the caller says
/// what it IS. See `docs/probe-params-design.md`.
#[derive(Clone, Copy)]
pub struct ProbeParams {
    pub mode:              u32,
    pub has_recv_endpoint: bool,
    /// Memory ceiling in MiB. 0 means "the probe default" (64 MiB).
    pub mem_mib:           u32,
}

/// Spawn the probe binary under a CALLER-SUPPLIED name and parameters.
///
/// The name is owned by the task (`scheduler::set_task_name`), which is what makes this possible at
/// all - see the commit that made task names bytes rather than literals.
///
/// `peers` are resolved through the kernel name directory exactly as a contract's `send_peers` are;
/// the caller supplies the LIST, not the authority - each peer must already be registered, and an
/// absent one is skipped with the same loud line as any other spawn.
pub fn spawn_probe(name: &str, core_override: Option<u32>, p: ProbeParams, peers: &[&str])
    -> Result<Option<EndpointId>, SpawnError>
{
    // The probe image and the fields that never varied come from ONE kernel entry.
    let (_, cfg) = service_config("probe").ok_or(SpawnError::NotFound)?;

    // A CALLER-SUPPLIED NAME MAY NOT BE A REAL SERVICE'S NAME.
    //
    // This is the one new authority the parameterised path creates, so it is closed here rather than
    // left to convention. Before, `Spawn` could only start a service the kernel already knew, under
    // the name the kernel gave it - name and binary were bound together. Now a SPAWN holder chooses
    // the name, and every service holds a spawn cap (22 Test A9). Without this check, a compromised
    // service could wait for `fs` to die and register the PROBE binary under the name `fs`; clients
    // reacquiring by name (14.3) would then wire themselves to it. The name directory is a recovery
    // anchor, and an anchor that can be squatted is not one.
    //
    // Refusing the whole real catalogue is deliberately blunter than refusing the live set: a name
    // is dangerous precisely while its service is DEAD, which is when a liveness test would pass.
    if service_config(name).is_some() {
        crate::kprintln!("task: spawn probe '{}' rejected: that is a real service's name", name);
        return Err(SpawnError::NotFound);
    }

    if scheduler::find_task_by_name(name).is_some() {
        crate::kprintln!("task: spawn '{}' rejected: already running", name);
        return Err(SpawnError::AlreadyRunning);
    }

    let core_id = resolve_spawn_core(core_override, cfg.preferred_core)?;
    let mem = if p.mem_mib == 0 { cfg.memory_limit } else { (p.mem_mib as u64) * 1024 * 1024 };

    // AUTHORITY STAYS WITH THE KERNEL. A probe that needs an IRQ line routed to it, or peer caps that
    // carry GRANT, is asking for something the caller may not simply assert - so those two are looked
    // up by name here rather than taken from the parameters.
    let (hw_irqs, grant) = probe_authority(name);

    let result = spawn_service_with_image(name, crate::loader::ImageSource::Kernel(cfg.elf), core_id,
                              p.has_recv_endpoint, peers, p.mode,
                              grant, mem, hw_irqs,
                              false, None, None);
    if let Err(ref e) = result {
        crate::kprintln!("task: spawn probe '{}' failed: {:?}", name, e);
    }
    result
}

/// The two probes whose configuration is AUTHORITY rather than parameter, kept in the kernel.
///
/// `probe-11a` needs IRQ 33 routed to its endpoint (12.3) - routing a hardware interrupt line to a
/// service is a grant. `probe-5a-send` needs its peer caps minted with GRANT so it can re-delegate
/// them (22 Test 5A) - handing out a re-delegatable capability is a grant too. Everything else about
/// all 193 probes is a parameter, and parameters come from the caller.
fn probe_authority(name: &str) -> (&'static [u8], bool) {
    match name {
        "probe-11a"     => (&[33], false),
        "probe-5a-send" => (&[],   true),
        _               => (&[],   false),
    }
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

    let core_id = resolve_spawn_core(core_override, cfg.preferred_core)?;

    let result = spawn_service_with_image(static_name, crate::loader::ImageSource::Kernel(cfg.elf), core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit, cfg.hw_irqs,
                              cfg.has_console_read, None, None);
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
    let core_id = resolve_spawn_core(core_override, cfg.preferred_core)?;
    let result = spawn_service_with_image(static_name, crate::loader::ImageSource::Kernel(cfg.elf), core_id,
                              cfg.has_recv_endpoint, cfg.send_peers, cfg.probe_mode,
                              cfg.send_peers_grant, cfg.memory_limit, cfg.hw_irqs,
                              cfg.has_console_read, None, Some(installs));
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

/// Undo a partially-built spawn on any error path (V2, kernel-audit-2).
///
/// A spawn that fails AFTER the recv-endpoint block (a later driver MMIO/DMA map, or the
/// ctx-frame / kstack allocation) must not leak what that block registered. In particular a
/// leaked routing entry stays `valid + Alive`, so `routing::register` can never recycle its
/// slot and eventually panics at `MAX_ENDPOINTS`; independently a leaked endpoint id never
/// returns to the free list and marches `alloc_endpoint_id` into its `DELEGATED_BASE` panic.
/// Under a `chaos max-carnage` + `mem-pressure` storm those failures accumulate into a kernel
/// panic. This unwinds the endpoint registrations (mirroring the endpoint-teardown half of
/// `kill_task_by_slot` for a task that never ran - so no blocked waiters / delegated resources
/// to handle) and releases the reserved task slot.
///
/// `own_endpoint` is `None` for a service with no recv endpoint (and at the pre-endpoint cap
/// inserts), in which case only the task slot is released - identical to the prior behaviour.
fn cleanup_partial_spawn(task_slot: usize, name: &str, own_endpoint: Option<EndpointId>) {
    // A spawn that fails half-way must give back BOTH endpoints, for the same reason death must:
    // a leaked endpoint is permanent, and enough of them fill the routing table and take the kernel
    // down. Read-and-clear, so a later kill of this slot cannot reclaim the same one twice.
    if let Some(rep) = crate::task::scheduler::take_task_reply_endpoint(task_slot) {
        let _ = crate::ipc::routing::kill_endpoint(rep);
        crate::capability::table::mark_dead_resource(
            crate::capability::cap::ResourceId::from(rep));
    }
    if let Some(ep_id) = own_endpoint {
        // Mark the routing entry Dead (recyclable) + drain its queue + bump generation.
        let _ = crate::ipc::routing::kill_endpoint(ep_id);
        // Invalidate the resource so any cap already handed out fails its generation check.
        crate::capability::table::mark_dead_resource(
            crate::capability::cap::ResourceId::from(ep_id));
        // Clear the name mapping while the id is still ours, THEN free the id (the same
        // load-bearing order as the kill path: free is the barrier against id reuse).
        crate::ipc::names::unregister_endpoint(name, ep_id);
        crate::ipc::free_endpoint_id(ep_id);
    }
    scheduler::release_task_slot(task_slot);
}

/// Low-level spawn: load ELF, wire caps, enqueue on `core_id`. Returns the new task's recv
/// `EndpointId` (`None` if it has no endpoint) - the caller (via the spawn syscall) can mint a
/// cap to it. This is the Phase-0 seam for moving naming out of the kernel (`docs/naming-design.md`):
/// a spawner can collect a cap to every service it starts without the kernel resolving names.
fn spawn_service_with_image(
    // NOT `&'static str`. A caller-supplied name is what lets a spawner name what it spawns; the
    // task owns its bytes now (`scheduler::set_task_name`), so nothing here needs the literal.
    name:              &str,
    // Where the image lives: kernel rodata (the catalogue path, until it is gone) or the CALLER's
    // address space (`SpawnImage`). See `loader::ImageSource` for the double-fetch discipline.
    image:             crate::loader::ImageSource,
    core_id:           u32,
    has_recv_endpoint: bool,
    send_peers:        &[&str],
    probe_mode:        u32,
    send_peers_grant:  bool,
    memory_limit:      u64,
    hw_irqs:           &[u8],
    has_console_read:  bool,
    // `Some(bits)` = the SPAWNER named these privileges (SpawnImage, already checked against what the
    // caller itself holds). `None` = resolve them from the kernel's by-name table, which is the
    // catalogue path and goes away with the catalogue.
    priv_override:     Option<u32>,
    // Phase 0b (docs/naming-design.md): if `Some`, wire the child's send-peers from these
    // caller-supplied `(label, cap)` entries instead of resolving `send_peers` against the kernel
    // name table. The kernel installs each cap and records `label → slot` in the child's send-peer
    // metadata, so the child's `ctx.capability(label)` resolves exactly as it does on the old path.
    // `None` = the old name-resolution path (unchanged).
    installs:          Option<&[InstallCap]>,
) -> Result<Option<EndpointId>, SpawnError> {
    // The declared hardware class + mint authority for this service (audit M7 / T1 Phase B). Every
    // MMIO / DMA / IOMMU / bus-master / RESOURCE_MINT grant below is driven off these, not a `name ==`
    // check - one declaration (`service_hw`), reconciled against the .toml for contracted services.
    let (hw, resource_mint_by_name) = service_hw(name);
    // RESOURCE_MINT is NOT a field of `Privileges` - it is a separate flag out of `service_hw`, so a
    // spawner-supplied privilege set has to be threaded to it explicitly. Missing this meant a moved
    // service spawned fine and then idled with "no RESOURCE_MINT cap", which the dedicated
    // `osdev test resource-server` caught and identity/shell/files could not: none of them exercise it.
    let resource_mint = match priv_override {
        Some(bits) => bits & privbits::RESOURCE_MINT != 0,
        None       => resource_mint_by_name,
    };

    // DIAG step markers (gated by SPAWN_TRACE; off by default - see its doc).
    if SPAWN_TRACE { crate::kprintln!("spawn[elf]: '{}'", name); }

    // 1. Parse ELF.
    let crate::loader::LoadedElf { mut page_table, entry_va, mapped_bytes: elf_mapped_bytes } =
        crate::loader::load_from(&image)?;

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
    // Declared before the slot is reserved so every error path below can route through
    // cleanup_partial_spawn(task_slot, name, own_endpoint) (V2, kernel-audit-2): None until
    // the recv-endpoint block registers an endpoint, Some(ep_id) after - so a failure before
    // the block releases only the slot, and one after also unwinds the endpoint registrations.
    let mut own_endpoint: Option<EndpointId> = None;
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
    // All six non-hardware authorities below come from the ONE `service_privileges` table (audit U15),
    // not a re-derived `name ==` check per grant. `is_probe` covers the whole test-probe family by ELF
    // identity so no probe is missed by name.
    // A caller-supplied image is never the probe: `is_probe` exists to give the whole test-probe
    // family its privileges by ELF identity rather than a re-derived `name ==` check, and only the
    // kernel's own rodata has an identity to compare.
    let is_probe = match image {
        crate::loader::ImageSource::Kernel(b) => core::ptr::eq(b.as_ptr(), PROBE_ELF.as_ptr()),
        crate::loader::ImageSource::User { .. } => false,
    };
    let privs = match priv_override {
        // The spawner named them. Every bit was checked against the CALLER's own holdings before we
        // got here, so this cannot escalate - it can only pass on what the spawner already has.
        Some(bits) => Privileges {
            spawn:           bits & privbits::SPAWN           != 0,
            console_push:    bits & privbits::CONSOLE_PUSH    != 0,
            introspect:      bits & privbits::INTROSPECT      != 0,
            service_control: bits & privbits::SERVICE_CONTROL != 0,
            fire_irq:        bits & privbits::FIRE_IRQ        != 0,
            reboot:          bits & privbits::REBOOT          != 0,
            acquire_any:     bits & privbits::ACQUIRE_ANY     != 0,
            // Not yet expressible in the wire form; the by-name table still answers for these on the
            // catalogue path, and a moved service simply does not get them (refused, not silent).
            net_device:      false,
            usb_disk:        false,
            gpio:            false,
            set_clock:       false,
            set_clock_floor: false,
        },
        None => service_privileges(name, is_probe),
    };

    let mut spawn_slot_u32 = u32::MAX;
    if privs.spawn {
        let sp_slot = caps.insert(mint_cap(SPAWN_RESOURCE, Rights::WRITE))
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        spawn_slot_u32 = sp_slot as u32;
    }

    // 4. Optional recv endpoint.
    let mut recv_slot_u32 = u32::MAX;
    let mut self_grant_slot_u32 = u32::MAX;
    // Carried to `commit_task` so the scheduler can reclaim it on death. Without this the reply
    // endpoint outlives its task and the routing table fills - it panicked the kernel under a chaos
    // kill storm. See `scheduler::TASK_REPLY_ENDPOINT`.
    let mut reply_ep_for_slot: Option<crate::ipc::EndpointId> = None;
    let mut reply_recv_slot_u32 = u32::MAX;
    let mut reply_grant_slot_u32 = u32::MAX;

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
        // A FULL ROUTING TABLE FAILS THE SPAWN. It used to panic the kernel, which is the one thing
        // nothing above the kernel is allowed to cause: a chaos kill storm exhausted the table and
        // took the machine down with it. A service that cannot get a mailbox genuinely cannot serve,
        // so the spawn is refused - but refusing a spawn is an ordinary, recoverable outcome the
        // supervisor already handles by logging and carrying on, and the kernel stays up.
        if !crate::ipc::routing::try_register(ep_id, core_id, start_gen) {
            crate::kprintln!(
                "task: '{}' spawn REFUSED - IPC routing table full, no mailbox available",
                name);
            cleanup_partial_spawn(task_slot, name, None);
            return Err(SpawnError::NoMemory);
        }

        // Publish name → endpoint mapping for peer cap resolution.
        crate::ipc::names::register(name, ep_id);

        // Record the endpoint NOW (before the cap inserts below), so any error from here on
        // unwinds these three registrations via cleanup_partial_spawn (V2, kernel-audit-2).
        own_endpoint  = Some(ep_id);

        // Mint RECV cap → first free slot (= slot 2).
        let recv_cap = mint_cap(resource_id, Rights::RECV);
        let cap_slot = caps.insert(recv_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        recv_slot_u32 = cap_slot as u32;

        // Self-grant cap: a SEND|GRANT cap to this service's OWN endpoint, so it can
        // announce its name to the kernel directory by granting a derived copy. GRANT is
        // required for the cap to be transferable via SendWithCap; the service keeps
        // this original and derives copies for re-registration after a restart.
        if let Ok(sg) = caps.insert(mint_cap(resource_id, Rights::SEND | Rights::GRANT)) {
            self_grant_slot_u32 = sg as u32;
        }

        // A SECOND endpoint, for replies only.
        //
        // The first one is where clients send their requests. A service that also AWAITS replies
        // there cannot drain it while blocked, so client traffic fills the 16-deep queue and the
        // reply it is waiting for is dropped by a peer that (rightly) uses `try_send` instead of
        // deadlocking. The wait then runs to its deadline - 30 s per block op on x86.
        //
        // Not registered in the name directory: nobody looks this up, it is handed out per-request as
        // a reply cap. Not gated on a contract either - every service that can receive gets one, so
        // there is no capability to declare and no way to get it wrong. It costs one endpoint and two
        // cap slots per task, and it removes an entire class of self-inflicted stall.
        //
        // `docs/net-tags-design.md` rejected this for needing a `CreateEndpoint` syscall. It does not:
        // this is the same mint as above, at the same point in spawn.
        let reply_ep_id  = crate::ipc::alloc_endpoint_id();
        let reply_res_id = ResourceId::from(reply_ep_id);
        let reply_gen    = crate::capability::next_generation();
        crate::capability::register_resource_at_gen(reply_res_id, reply_gen);
        // FALLIBLE on purpose, and now RESERVED against. The routing table holds 96 endpoints and
        // the probe builds spawn ~178 services; taking one unconditionally would turn
        // `osdev test identity` into a boot panic. Without it the task awaits replies on its shared
        // endpoint, exactly as before this existed.
        //
        // `try_register_optional` additionally refuses once the table nears full, because being
        // merely fallible was not enough: this endpoint is optional but was competing for slots on
        // equal terms with the MANDATORY receive endpoint above, and winning by getting there
        // first. Property P5 caught the consequence - a real service refused with "IPC routing
        // table full" while convenience endpoints held slots they could have done without.
        let reply_routed =
            crate::ipc::routing::try_register_optional(reply_ep_id, core_id, reply_gen);
        if reply_routed {
        // Recorded HERE, not at commit: every fallible step after this point runs
        // `cleanup_partial_spawn`, which can only give the endpoint back if it knows about it.
        reply_ep_for_slot = Some(reply_ep_id);
        scheduler::set_task_reply_endpoint(task_slot, reply_ep_for_slot);
        if let Ok(rr) = caps.insert(mint_cap(reply_res_id, Rights::RECV)) {
            reply_recv_slot_u32 = rr as u32;
            if let Ok(rg) = caps.insert(mint_cap(reply_res_id, Rights::SEND | Rights::GRANT)) {
                reply_grant_slot_u32 = rg as u32;
            } else {
                // Half a reply mailbox is worse than none: a RECV with no way to hand out a reply cap
                // would have callers wait on an endpoint nothing can answer. Fall back to the shared
                // endpoint, which is what every service did until now.
                reply_recv_slot_u32 = u32::MAX;
            }
        }
        } else {
            // REFUSED, so give the id back. The allocation happened before the registration could
            // fail, and without this an id vanished on every refusal - the same leak the death path
            // had, on the path that runs precisely when the table is under pressure and can least
            // afford it. The task simply awaits replies on its shared endpoint, as it did before
            // reply endpoints existed.
            crate::ipc::free_endpoint_id(reply_ep_id);
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
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        console_read_slot_u32 = cap_slot as u32;
    }

    // CONSOLE_PUSH: inject decoded keystrokes into the console input ring (§12). WHO holds it is in
    // `service_privileges` (the single authority table); here we only mint it.
    let mut console_push_slot_u32 = u32::MAX;
    if privs.console_push {
        let cp_cap = mint_cap(CONSOLE_PUSH_RESOURCE, Rights::WRITE);
        let cap_slot = caps.insert(cp_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        console_push_slot_u32 = cap_slot as u32;
    }

    // INTROSPECT: read another task's / system-wide kernel state via TaskStat + InspectKernel (§3.1;
    // docs/introspection-capability.md). Self-state (own alloc bytes) and the TSC stay ungated, so a
    // service not in `service_privileges` needs nothing. No slot is stored - the gate scans holdings.
    if privs.introspect {
        let in_cap = mint_cap(INTROSPECT_RESOURCE, Rights::READ);
        caps.insert(in_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // SERVICE_CONTROL: kill/restart other services (§3.1/§14.4; docs/service-control-cap.md). WHO holds
    // it is in `service_privileges`; here we only mint it.
    if privs.service_control {
        let sc_cap = mint_cap(SERVICE_CONTROL_RESOURCE, Rights::WRITE);
        caps.insert(sc_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
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
    // The supervisor gets a GRANT-ONLY cap for every delegatable privilege, so it can pass authority
    // to a service it spawns without being able to exercise any of it (see SUPERVISOR_DELEGATABLE).
    if name == "supervisor" {
        for (_, res) in SUPERVISOR_DELEGATABLE {
            let d = mint_cap(*res, Rights::GRANT);
            caps.insert(d)
                .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        }
    }

    if resource_mint {
        let rm_cap = mint_cap(RESOURCE_MINT_RESOURCE, Rights::WRITE);
        caps.insert(rm_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // REBOOT (§3.1): hardware-reset the machine (`Reboot`/18). WHO holds it is in `service_privileges`;
    // here we only mint it. No other service can hardware-reset the machine.
    // FIRE_IRQ (C1-6): inject a test interrupt. Held only by `control`, which needs it because the
    // interrupt-routing identity tests drive IRQ injection over the operator channel.
    if privs.fire_irq {
        let fi_cap = mint_cap(FIRE_IRQ_RESOURCE, Rights::WRITE);
        caps.insert(fi_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }
    if privs.reboot {
        let rb_cap = mint_cap(REBOOT_RESOURCE, Rights::WRITE);
        caps.insert(rb_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // ACQUIRE_ANY (§3.1): reach ARBITRARY services by name via `AcquireSendCap`. WHO holds it is in
    // `service_privileges`; here we only mint it. Ordinary services get NONE - their AcquireSendCap is
    // restricted to their contract-declared send-peers (recovery), so they hold no ambient send authority.
    if privs.acquire_any {
        let aa_cap = mint_cap(ACQUIRE_ANY_RESOURCE, Rights::WRITE);
        caps.insert(aa_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // NET_DEVICE: the ARM `nic-driver` moves ethernet frames via the in-kernel USB-net bridge
    // (NetFrame*/NetInfo, syscalls 42-44). WHO holds it is in `service_privileges`; here we only mint it.
    if privs.net_device {
        let nd_cap = mint_cap(NET_DEVICE_RESOURCE, Rights::WRITE);
        caps.insert(nd_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // USB_DISK: the ARM `block-driver` reads/writes a USB stick through the in-kernel Bulk-Only stack
    // (UsbDisk*, syscalls 46-48). Minted here; WHO holds it is in `service_privileges`.
    if privs.usb_disk {
        let ud_cap = mint_cap(USB_DISK_RESOURCE, Rights::WRITE);
        caps.insert(ud_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // GPIO_DEVICE: the shell's `gpio` command drives the SoC pins (ARM `Gpio` syscall). Minted here; WHO
    // holds it is in `service_privileges`.
    if privs.gpio {
        let g_cap = mint_cap(GPIO_DEVICE_RESOURCE, Rights::WRITE);
        caps.insert(g_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // SET_CLOCK: net-stack sets the wall clock from SNTP (`SetClock` syscall) on the RTC-less ARM port.
    // Minted here; WHO holds it is in `service_privileges`. Inert (no-op syscall) off ARM.
    if privs.set_clock {
        let sc_cap = mint_cap(SET_CLOCK_RESOURCE, Rights::WRITE);
        caps.insert(sc_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }
    // The floor-only strength: READ raises the bound, it cannot set the clock (§7.4 - rights narrow).
    if privs.set_clock_floor {
        let cf_cap = mint_cap(SET_CLOCK_RESOURCE, Rights::READ);
        caps.insert(cf_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
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
    // OVERFLOW IS LOUD (invariant 12). A contract declaring more peers than fit used to lose the extras
    // in SILENCE: the declaration was there, the cap never arrived, and the only symptom was a peer
    // behaving as though it did not exist - which reads as "that service is broken", not "you are over
    // a limit". It cost a debugging cycle on the very change that raised this cap. Keeping the cap
    // fixed is correct (26.6); losing data without saying so is the same shape as the x86 input ring.
    if send_peers.len() > MAX_SEND_PEERS {
        crate::kprintln!(
            "task: '{}' declares {} send peers, limit {} - the extras are NOT wired (raise              MAX_SEND_PEERS in task/mod.rs AND sdk/service_context.rs, and SERVICE_CONTEXT_DATA_SIZE              in both)",
            name, send_peers.len(), MAX_SEND_PEERS);
    }
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
    // The mapped MMIO window's VA + byte length; the length lets the SDK's `Mmio` wrapper bounds-check
    // accesses (SEC-4). (0, 0) = this service gets no MMIO.
    // Set by the framebuffer branch below; carried out so the context page can describe the grant.
    let mut fb_grant: Option<crate::bootcon::FbGrant> = None;
    let (xhci_mmio_va, xhci_mmio_len) = {
        // The controller BAR for this driver's declared class (audit M7): xHCI/EHCI/AHCI use their
        // register base; a NIC only when it is a model we drive (e1000 / RTL8168) - otherwise 0, so the
        // driver gets no mapping and idles, never touching foreign hardware (Commandment VII).
        let bar = hw.mmio_bar();
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
                    .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::MapFailed })?;
            }
            crate::kprintln!("spawn[mmio]: '{}' BAR {:#x} -> VA {:#x}", name, bar, XHCI_MMIO_VA);
            (XHCI_MMIO_VA, XHCI_MMIO_PAGES * PAGE_SIZE as u64)
        } else if hw == HwClass::Framebuffer {
            // The display's framebuffer, for the `console` service (docs/console-service.md 9).
            //
            // `PCD | PWT` = Normal NON-cacheable: uncached, but the write buffer may still gather a run
            // of pixel stores into a burst. A framebuffer store has no side effect - it is memory the
            // display happens to scan - so the Device attribute the driver MMIO grants use would forbid
            // that merging for nothing, at a bus transaction per pixel. It also has to MATCH the
            // kernel's own mapping of these same physical pages (`mmu::section_fb` on ARM): mismatched
            // memory attributes for one physical page are UNPREDICTABLE on ARM.
            match crate::bootcon::grant() {
                Some(g) => {
                    // UC- (PCD alone), NOT strong UC (PCD|PWT). The PAT index is
                    // (PAT<<2)|(PCD<<1)|PWT, so PCD|PWT selects entry 3 - strong uncacheable, the
                    // one memory type an MTRR can never upgrade. Every 4-byte pixel write then goes
                    // to the bus on its own with no combining, and this display measured 596 ms to
                    // repaint one scroll: 19 MB at about 32 MB/s.
                    //
                    // PCD alone selects entry 2, UC-, which is defined to yield WC where the MTRR
                    // for the range says WC - and firmware routinely marks a framebuffer WC for
                    // precisely this reason. Strictly no worse than before: if the MTRR says UC the
                    // effective type stays UC, exactly as it is today.
                    //
                    // A framebuffer wants write-combining, not cacheability. It is still not
                    // cached: nothing here reads back, and stores stay ordered enough for a display
                    // (which has no side effects to order against, unlike a register BAR - that is
                    // why a BAR keeps strong UC and this does not).
                    //
                    // X86 ONLY, because the two architectures read these same two bits in OPPOSITE
                    // senses and the neutral `PageFlags` name hides it. On arm32, `PCD | PWT` is
                    // Normal NON-cacheable - which permits exactly the gathering a framebuffer
                    // wants - while `PCD` ALONE is Device, which forbids it. Dropping PWT there
                    // would slow the Pi down for the same reason it speeds the PC up, and would
                    // also MISMATCH the kernel's own mapping of these physical pages
                    // (`mmu::section_fb`), which is unpredictable on ARM rather than merely slow.
                    // On aarch64 the attribute is `PCD || PWT`, so this bit makes no difference.
                    //
                    // Borrow the silicon's requirement, not the other port's answer (§26.14).
                    #[allow(unused_mut)]
                    let mut flags = PageFlags::PRESENT
                        | PageFlags::WRITABLE
                        | PageFlags::USER
                        | PageFlags::NO_EXEC
                        | PageFlags::PCD
                        // This is a FRAMEBUFFER - RAM the display controller scans out - not device
                        // registers, and saying so is what lets an arch pick the right memory type.
                        // AArch64 was mapping it Device-nGnRnE (the faithful reading of PCD|PWT), which
                        // forbids the gathering and buffering a bulk pixel write depends on: one
                        // 1920x1080 repaint measured 582 ms, about 14 MB/s, which is the slow and
                        // jittery rendering reported from the television. An arch with nothing better
                        // ignores this bit and keeps its uncached-MMIO type, so x86 is unchanged.
                        | PageFlags::WRITE_COMBINE;
                    #[cfg(not(target_arch = "x86_64"))]
                    {
                        flags |= PageFlags::PWT;
                    }
                    let pages = g.len.div_ceil(PAGE_SIZE as u64);
                    // The framebuffer is DEVICE memory the kernel is about to map into a service.
                    // The kill-path reclaim walks a dead task's leaves and frees them, so without a
                    // reservation the display's pages go into the RAM free pool the first time
                    // `console` dies - inflating the free count past the total AND making the
                    // framebuffer allocatable, so a later task can be handed the screen as RAM.
                    //
                    // The walker has a guard for this, keyed on the mapping being uncached, and it
                    // was disarmed by a change nowhere near it (see `reserve_no_free`). Reserving the
                    // range here puts the refusal on the RESOURCE, where how it is mapped cannot
                    // affect it. Idempotent, so a console respawn does not consume a second slot.
                    crate::memory::allocator::reserve_no_free(g.phys, pages as usize);
                    for i in 0..pages {
                        let off = i * PAGE_SIZE as u64;
                        page_table
                            .map(VirtAddr(FB_VA + off), PhysAddr(g.phys + off), flags)
                            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::MapFailed })?;
                    }
                    fb_grant = Some(g);
                    // The grant IS the handover: the kernel has just given this framebuffer away, so it
                    // stops writing to it now rather than when the first console byte arrives. Those
                    // moments are seconds apart on a quiet boot, and drawing in the gap put the floor's
                    // text on top of the terminal's.
                    crate::bootcon::release();
                    crate::kprintln!(
                        "spawn[fb]: '{}' {}x{} at phys {:#x} -> VA {:#x} ({} KiB)",
                        name, g.width, g.height, g.phys, FB_VA, g.len / 1024
                    );
                    (FB_VA, g.len)
                }
                // `found()` said there was a framebuffer and `grant()` now says there is not. Nothing
                // maps, the service is told it has no display and says so (invariant 12); it does not
                // get a window it cannot use.
                None => (0, 0),
            }
        } else if let Some((va, len)) = crate::arch::imp::map_fixed_driver_mmio(&mut page_table, name) {
            // Non-PCI fixed-physical peripheral MMIO grant (§12.3 for a bus with no PCI scan - the Pi's
            // peripherals are at fixed addresses). The arch layer maps the window Device+USER and returns
            // its (VA, len); on x86 this is always None (PCI BARs handle it above).
            crate::kprintln!("spawn[mmio]: '{}' fixed peripheral -> VA {:#x} ({} B)", name, va, len);
            (va, len)
        } else {
            (0, 0)
        }
    };

    // 6b. Allocate + map a physically-contiguous DMA arena for the xHCI driver
    // (§12). The controller DMAs into this memory (rings/contexts), so the driver
    // needs both the VA (to build structures) and the physical base (to program
    // the controller). Normal cacheable mapping - x86 DMA is cache-coherent.
    // Grant a physically-contiguous DMA arena to a USB driver (xhci or ehci) for
    // its queue structures. Shared VA/fields, separate address spaces (§12).
    let dma_for_driver = hw.needs_dma();
    // Per-driver arena size: xHCI needs room for its 256 scratchpad buffers;
    // EHCI gets the small 64 KiB arena it had on main; the AHCI block driver needs
    // only its command list/FIS/command table + a data buffer - 64 KiB is plenty.
    let dma_pages = hw.dma_pages();
    let (xhci_dma_va, xhci_dma_phys, xhci_dma_len) = if dma_for_driver {
        // DMA permanent-reserve (§12): allocate this driver's arena ONCE, then reuse the same physical
        // frames across every respawn. `alloc_dma_arena` reserves the run out of the general pool (so it
        // is never recycled into a page table); keeping the phys keeps the reservation bounded - one
        // arena per driver, not one per spawn. So a stray DMA (if the kill-path bus-master quiesce ever
        // fails) always lands in DMA-reserved memory, never a PTE or kernel struct.
        let kept = hw.dma_phys_slot();
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
                // Cacheable or not is the ARCH's call, not this function's.
                //
                // The SDK's `Dma` wrapper does no cache maintenance, and says so: it assumes x86 DMA
                // coherence and warns that a non-coherent arch "must add cache maintenance here ... or
                // map the arena non-cacheable" (SEC-28). AArch64 and ARMv7 are non-coherent, so a
                // userspace driver there would exchange stale data with its device and never be told -
                // the same fault the in-kernel GENET driver had until every buffer got an explicit
                // `dma_sync`. A service has no such primitive, so the MAPPING removes the need rather
                // than resting on the driver author remembering.
                let mut flags = PageFlags::PRESENT
                    | PageFlags::WRITABLE
                    | PageFlags::USER
                    | PageFlags::NO_EXEC;
                if crate::arch::imp::DMA_ARENA_UNCACHED {
                    flags |= PageFlags::PCD;
                }
                for i in 0..dma_pages {
                    let off = i * PAGE_SIZE as u64;
                    page_table
                        .map(VirtAddr(crate::arch::imp::DRIVER_DMA_VA + off), PhysAddr(phys + off), flags)
                        .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::MapFailed })?;
                }
                let len = dma_pages * PAGE_SIZE as u64;
                crate::kprintln!(
                    "spawn[dma]: '{}' arena phys {:#x} -> VA {:#x} ({} KiB)",
                    name, phys, crate::arch::imp::DRIVER_DMA_VA, len / 1024
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
                    use crate::arch::imp::pci;
                    if CONFINE_USB_DRIVERS && hw.iommu_confine() {
                        crate::arch::imp::iommu::confine_device(
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
                    let bdf = hw.bdf();
                    pci::set_power_d0(bdf);  // bring the device to D0 first - firmware may park a non-boot NIC in D3
                    pci::set_bus_master(bdf);
                }
                (crate::arch::imp::DRIVER_DMA_VA, phys, len)
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
        let ctx_frame = alloc_frame()
            .ok_or_else(|| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::NoMemory })?;
        let ctx_phys  = ctx_frame.phys_addr().0;
        // SAFETY: phys from allocator; task hasn't started yet; HHDM covers it.
        unsafe {
            let virt = (get_hhdm_offset() + ctx_phys) as *mut u8;
            core::ptr::write_bytes(virt, 0, PAGE_SIZE);
            let data = &mut *(virt as *mut ServiceContextData);
            data.magic              = SERVICE_CTX_MAGIC;
            // Readback: confirm write was not silently dropped (should always pass).
            if data.magic != SERVICE_CTX_MAGIC {
                crate::arch::imp::serial_write_bytes_lockfree(b"CTX-MAGIC-MISMATCH\n");
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
            data.reply_recv_slot    = reply_recv_slot_u32;
            data.reply_grant_slot   = reply_grant_slot_u32;
            data.xhci_mmio_va       = xhci_mmio_va;
            data.xhci_mmio_len      = xhci_mmio_len;
            data.xhci_dma_va        = xhci_dma_va;
            data.xhci_dma_phys      = xhci_dma_phys;
            data.xhci_dma_len       = xhci_dma_len;
            data.fb_va              = fb_grant.map_or(0, |_| FB_VA);
            data.fb_len             = fb_grant.map_or(0, |g| g.len);
            data.fb_pitch           = fb_grant.map_or(0, |g| g.pitch);
            data.fb_width           = fb_grant.map_or(0, |g| g.width);
            data.fb_height          = fb_grant.map_or(0, |g| g.height);
            data.fb_bpp             = fb_grant.map_or(0, |g| g.bpp);
            data.fb_shifts          = fb_grant.map_or(0, |g| g.shifts);
            for i in 0..peer_count {
                data.send_peers[i].slot     = peer_data[i].0;
                data.send_peers[i].name_len = peer_data[i].1;
                data.send_peers[i].name     = peer_data[i].2;
            }

            // Record the same peer NAMES kernel-side. `AcquireSendCap` authorises a reacquire for a
            // name the task declared (14.3 recovery, without the broad ACQUIRE_ANY), and that check
            // used to read the kernel CATALOGUE - which answers "declares nothing" for a service whose
            // config lives in the supervisor. Recording the actual wiring here is both the fix and the
            // honester source: what the task was wired with, not what a table says it should have been.
            {
                let mut names: [&str; MAX_SEND_PEERS] = [""; MAX_SEND_PEERS];
                let mut nn = 0usize;
                for i in 0..peer_count {
                    let l = peer_data[i].1 as usize;
                    if let Ok(nm) = core::str::from_utf8(&peer_data[i].2[..l.min(PEER_NAME_BYTES)]) {
                        names[nn] = nm; nn += 1;
                    }
                }
                scheduler::set_task_peers(task_slot, &names[..nn]);
            }
        }
        let ctx_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::NO_EXEC;
        page_table
            .map(VirtAddr(SERVICE_CTX_VA), PhysAddr(ctx_phys), ctx_flags)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::MapFailed })?;
        // ctx_frame owned by the page table now; Frame is Copy/no-Drop (no release).
    }

    if SPAWN_TRACE { crate::kprintln!("spawn[kstack]: '{}'", name); }

    // 7. Kernel stack.
    let kstack_top = alloc_kstack()
        .ok_or_else(|| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::NoMemory })?;
    if SPAWN_TRACE { crate::kprintln!("spawn[commit]: '{}' kstack ok", name); }

    // 8. Initial ring-3 context.
    let cr3 = page_table.into_cr3();
    // SAFETY: `finalize_service_address_space` - arch hook: on ARM it clones the kernel identity into
    // the service page table and cleans the D-cache for the non-cacheable walker (the kernel is not
    // shared higher-half as on x86), a no-op on x86; runs after ALL of the service's regions (ELF,
    // stack, ctx) are mapped and `cr3` is the freshly-built, not-yet-active root. `new_user`:
    // kstack_top is valid kernel memory; entry_va and USER_STACK_TOP are valid ring-3 addresses in the
    // new page table. (Both in one block so this stays at `task/`'s grandfathered unsafe floor, §18.5.)
    let ctx = unsafe {
        crate::arch::imp::page_tables::finalize_service_address_space(cr3);
        TaskContext::new_user(kstack_top, entry_va, USER_STACK_TOP, cr3)
    };

    // 9. Initialise the memory budget for this task (§10.3) BEFORE committing the slot Ready (SEC-19).
    // commit_task publishes Ready last, making the task schedulable - possibly on a DIFFERENT core;
    // seeded after, that core could run the task and read the PREVIOUS occupant's TASK_LIMIT_BYTES /
    // TASK_ALLOC_BYTES in the window before this line ran (a transient wrong quota). Seed it first,
    // with the base footprint - the mapped binary (code+data+BSS), the 256 KiB user stack, and the
    // ctx page - so MEM_USED reflects real occupancy, not just dynamic alloc_mem (which most no-heap
    // services never call). Mirrors commit_task's own "every field set before Ready is published" rule.
    let base_bytes = elf_mapped_bytes
        + USER_STACK_PAGES * PAGE_SIZE as u64
        + PAGE_SIZE as u64; // ctx page
    scheduler::set_task_memory_budget(task_slot, memory_limit, base_bytes);

    // 10. Finalise the reserved task slot (ctx + metadata -> Ready). The budget above is already in
    // place, so a task scheduled the instant Ready publishes sees its own quota, never a stale one.
    // SAFETY: task_slot reserved above; CapTable initialised; IF=0.
    unsafe {
        scheduler::commit_task(task_slot, name, ctx, true, kstack_top as u64, own_endpoint);
    }

    crate::kprintln!("task: '{}' spawned OK on core {} (slot {})", name, core_id, task_slot);
    Ok(own_endpoint)
}

/// Spawn `init` on Core 0. Called once by `kernel_main` (§11.1).
/// The kernel's ONE direct spawn (Path C / Phase 5 - `init` is removed). The kernel boots the
/// SUPERVISOR directly; the supervisor then spawns logger and all services. Uses `SUPERVISOR_ELF`
/// (garbage under `test-bad-supervisor` → §22 Test 1B). `has_recv_endpoint = true` (the supervisor
/// owns the death-notification endpoint). A *boot-time* spawn failure is fatal (§6.2, §11.3); a later
/// *runtime* death is recovered by the kernel respawning it (Phase 6 - see below).
// C1-1: `arm_spawn_logger_neutral` and `arm_spawn_shell_neutral` USED TO LIVE HERE, and with them the
// `arm-sched-spawn` / `arm-shell` / `arm-spawn-logger` / `pi4-sched-spawn` bring-up builds in which the
// kernel started a service directly. They were scaffolding from before the supervisor path worked, and
// they were gated on ARCHITECTURE rather than on the features that called them, so every ARM and
// AArch64 kernel carried the ability whether or not anything reached it.
//
// The kernel spawns the supervisor and NOTHING ELSE. Bringing up a new ISA now means getting the
// supervisor up, which is the thing that has to work anyway. If that ever proves too large a first
// step, the answer is a smaller supervisor - not a second spawn path in the kernel.
pub fn spawn_supervisor() {
    match spawn_service_with_image("supervisor", crate::loader::ImageSource::Kernel(SUPERVISOR_ELF), 0, true, &[], 0, false, 64 * 1024 * 1024, &[], false, None, None) {
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
// teardown of the dying supervisor). `scheduler::run` on CORE 0 polls the flag at its loop top,
// where IF=1, and does the respawn (`poll_supervisor_respawn`, below).
//
// NOT the timer ISR, and not `control::process_pending` - which does not exist any more, and could
// not have done this anyway. A ~22 ms spawn issuing all-core TLB shootdowns wedges the box at IF=0,
// because core 0 cannot ACK other cores' IPIs while stuck in it. `poll_supervisor_respawn` carries
// the full argument; this note was still naming the old caller after that one was corrected on
// 2026-08-14, which is how a fix applied at ONE site leaves the same false statement standing forty
// lines above the comment that warns about it.
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
static SUPERVISOR_RESPAWN_COUNT: portable_atomic::AtomicU64 =
    portable_atomic::AtomicU64::new(0);
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

/// If the supervisor died, respawn it (Path C / Phase 6). Called from `scheduler::run` (Core 0) at an
/// IF=1 point - a spawn-safe deferred point. It is NOT called from `control::process_pending`, which
/// runs at IF=0 from the timer ISR: a ~22 ms spawn issuing all-core TLB shootdowns wedged the box
/// there, because core 0 could not ACK other cores' IPIs while stuck in it. This comment said
/// otherwise until 2026-08-14 and cost a wrong finding - a doc comment naming the wrong caller is not
/// a cosmetic error, it is a false statement about control flow that a reader will act on.
/// Always respawns; never gives up (see the note above).
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
    // A RUNTIME respawn must NEVER panic (kernel audit C3). spawn_supervisor() panics on any SpawnError,
    // but the reachable ones here - NoMemory / MapFailed / CapTableFull - are TRANSIENT resource pressure
    // (a `mem-pressure` + `kill supervisor` storm can win the reclaim-vs-alloc race for an instant), not
    // corrupted kernel state, so §6.2 does not sanction a panic. Panicking would force the very reboot
    // Phase 6 exists to eliminate - a userspace-reachable DoS reboot. So call the non-panicking spawn
    // directly; on a transient failure, log LOUD (§26.7) and RE-ARM PENDING so the next Core-0 tick
    // retries. The supervisor's footprint is constant and just-reclaimed, so a retry succeeds the moment
    // the pressure eases. (Only the BOOT-time spawn_supervisor keeps its fatal panic - §22 Test 1B.)
    match spawn_service_with_image(
        "supervisor", crate::loader::ImageSource::Kernel(SUPERVISOR_ELF), 0, true, &[], 0, false,
        64 * 1024 * 1024, &[], false, None, None,
    ) {
        Ok(_) => crate::kprintln!("task: supervisor spawned on core 0"),
        Err(e) => {
            crate::kprintln!(
                "kernel: supervisor respawn #{} FAILED ({:?}) - re-arming, retry next tick (transient resource pressure, NOT a reboot)",
                n, e
            );
            SUPERVISOR_RESPAWN_PENDING.store(true, Ordering::Release);
        }
    }
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
