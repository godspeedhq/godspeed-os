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
use crate::capability::{mint_cap, Rights, LOG_WRITE_RESOURCE, SPAWN_RESOURCE, CONSOLE_READ_RESOURCE, CONSOLE_PUSH_RESOURCE, INTROSPECT_RESOURCE, SERVICE_CONTROL_RESOURCE, RESOURCE_MINT_RESOURCE, REBOOT_RESOURCE, ACQUIRE_ANY_RESOURCE, NET_DEVICE_RESOURCE, GPIO_DEVICE_RESOURCE, USB_DISK_RESOURCE, SET_CLOCK_RESOURCE, FIRE_IRQ_RESOURCE, IMAGE_SPAWN_RESOURCE, PCI_CFG_RESOURCE, DEVICE_POWER_RESOURCE, CPU_CLOCK_RESOURCE};
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
/// (no shootdown needed) and the supervisor's stack, the first allocated, already carries its guard. Calling it
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
/// The most of one BAR a single grant maps: 16 MiB, so a device with a huge BAR (a GPU's aperture) costs a
/// bounded number of page-table frames (26.6). Every driver here has a BAR well under it.
const MMIO_WINDOW_MAX:     u64 = 16 * 1024 * 1024;

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
/// The PWM audio driver's arena, kept across its respawns like every other (the DMA engine may still be
/// reading the ring when a driver dies; the reservation keeps that harmless).
pub static AUDIO_PWM_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
pub static EHCI_DMA_PHYS: portable_atomic::AtomicU64 = portable_atomic::AtomicU64::new(0);
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
/// `pwm-audio`: a page of DMA control blocks and a 128 KiB ring of PWM words - 16 periods of 8 KiB,
/// about 370 ms at 44.1 kHz, which is the margin a POLLED refill needs (no interrupt is routed). That is
/// 33 pages (`ARENA_NEEDED` in `services/pwm-audio`); 36 leaves three spare, and nothing records why.
const AUDIO_PWM_DMA_PAGES: u64 = 36;

/// Maximum named send peers per service.
/// Send peers a service may be wired with.
///
/// RAISED 4 -> 6 (2026-08-29). The shell legitimately needs five (`fs`, `block-driver`, `time`,
/// `console`, `events`), and at four the fifth was dropped SILENTLY - the contract declared it, the
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
    // --- Framebuffer grant (whichever task's request names the FRAMEBUFFER kind - in practice `console`) ---
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
    /// The interrupt VECTOR(s) this service was granted, so a driver can learn which one it got.
    ///
    /// The kernel used to hand every driver a vector it could also hardcode: `XHCI_MSI_VECTOR` was a
    /// kernel constant, so both sides knew 0x28 statically and a driver comparing against its own copy
    /// was correct. The MSI pool (step D1b) makes the vector ALLOCATED AT SPAWN, and that quietly
    /// invalidated the hardcoded copy: the kernel began routing 0x30 while `services/xhci` still tested
    /// for 0x28, so it classified every real interrupt as an unknown message and reported `0 MSI, 7799
    /// msg` on hardware. Nothing broke - the interrupt still WOKE the driver - but the one instrument
    /// that says whether interrupts work had started reporting the opposite.
    ///
    /// The design gap was this field's absence, not the stale constant. A driver already learns its
    /// MMIO window and DMA arena from the kernel (`ctx.mmio()`, `ctx.dma_region()`) precisely because
    /// it may not name them; its vector is the same kind of fact and now travels the same way. A
    /// caller still cannot ASK for a particular vector - that is authority (§12.3) - it can only be
    /// told which one it was given.
    irq_count:          u32,      // 0 = no interrupt granted
    irqs:               [u8; 4],  // the granted vectors; only the first `irq_count` are meaningful
}

// The kernel writes this struct and the SDK reads it, from two crates, with no shared definition -
// they are kept in step BY HAND. There was no check on that, and adding a field to one and not the
// other silently misaligns every field after it: a service would read its neighbour's slot numbers.
//
// Pinned by SIZE in both crates. It does not prove field ORDER, but it catches the mistake that
// actually happens - an append on one side only - and it fails at compile time in the crate that
// drifted rather than at boot in a service that reads garbage.
const SERVICE_CONTEXT_DATA_SIZE: usize = 328;   // 320 + irq_count(4) + irqs[4] - the granted vector (D1b)
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
enum HwClass {
    None,
    // ---- NON-PCI kinds. These are SoC or boot facts, not bus facts: nothing enumerates them, so
    // they stay named. `Dwc2` is soldered to the BCM283x, the framebuffer is a Limine/mailbox
    // handoff, and the test IRQ is software. A name is the only way to refer to them.
    Dwc2,
    // An audio jack driven by PWM and fed by the SoC's DMA engine (the Pis, `docs/audio.md`): soldered
    // to the SoC, so named, like the DWC2.
    AudioPwm,
    // A WiFi radio on an SDIO host at a fixed SoC address (the Pi 4's CYW43455 behind the Arasan, the
    // VisionFive 2 Lite's AIC8800 behind a DesignWare `dw_mmc`). A
    // kind, so the kernel grants the window and the power control to what the spawn request ASKED FOR,
    // never to whatever is called `wifi-driver`.
    WifiSdio,
    Framebuffer,
    TestIrq,
    /// ---- ANY PCI DEVICE, named by what the BUS says it is rather than by what the kernel was
    /// taught (step D1).
    ///
    /// `class_code` is the industry-standard 24-bit (class, subclass, prog-if) triple, so a driver
    /// for a device this kernel has never heard of needs NO kernel change: the supervisor puts the
    /// code in the spawn request and the scan table answers it.
    ///
    /// The other three fields are the facts a class name used to imply, now stated by the caller
    /// because they belong to the DRIVER, not to the bus:
    ///   - `bar_ix`  which BAR holds the registers (xHCI 0, AHCI 5 - an INDEX, never an address),
    ///               or `BAR_AUTO` for the first mapped MEMORY BAR
    ///   - `dma_pages` how much DMA arena it needs (a SIZE, bounded by the syscall)
    ///   - `confine` whether to put it behind the IOMMU (policy, §6.4)
    ///   - `bdf`     WHICH device, when the caller knows (step D3). Zero means "not supplied" and
    ///               the class is resolved against the kernel's own scan, as before. This is an
    ///               IDENTIFIER, not an address: the kernel reads that device's registers to learn
    ///               what it is worth, so naming it grants nothing a class name would not.
    Pci { class_code: u32, bar_ix: u8, dma_pages: u32, confine: bool, bdf: u32 },
    // ---- The legacy per-class names, still populated by the scan and still the default path until
    // every caller names a class code. They go away with their last reader.
    Nic, Xhci, Ehci,
}

/// PERMANENT per-device DMA reservations for PCI devices found by the scan (§12 DMA
/// permanent-reserve), indexed by the device's slot in the table.
///
/// A restarted driver must be handed back the SAME physical arena - that is what makes a respawn
/// transparent to a controller that is still DMAing into it. Keyed by device index rather than by
/// class name, which is the whole point: a device the kernel cannot name still gets a reservation.
static PCI_DMA_PHYS: [portable_atomic::AtomicU64; crate::arch::imp::pci::MAX_DEVICES] =
    [const { portable_atomic::AtomicU64::new(0) }; crate::arch::imp::pci::MAX_DEVICES];

impl HwClass {
    /// The named kind, for a device at a FIXED address the arch layer grants by kind (the Pis' SoC
    /// blocks). `None` for a PCI device - its window comes from its BAR - and for no device.
    fn fixed_kind(self) -> Option<u32> {
        match self {
            HwClass::Dwc2 => Some(kind::DWC2),
            HwClass::Nic => Some(kind::NIC),
            HwClass::AudioPwm => Some(kind::AUDIO_PWM),
            HwClass::WifiSdio => Some(kind::WIFI_SDIO),
            _ => None,
        }
    }
    /// The kind code recorded per task (`scheduler::set_task_hw_kind`); 0 for none or a PCI device.
    fn kind_code(self) -> u32 {
        match self {
            HwClass::Nic => kind::NIC,
            HwClass::Xhci => kind::XHCI,
            HwClass::Ehci => kind::EHCI,
            HwClass::Dwc2 => kind::DWC2,
            HwClass::Framebuffer => kind::FRAMEBUFFER,
            HwClass::TestIrq => kind::TEST_IRQ,
            HwClass::AudioPwm => kind::AUDIO_PWM,
            HwClass::WifiSdio => kind::WIFI_SDIO,
            HwClass::Pci { .. } | HwClass::None => 0,
        }
    }
    /// Did the PCI scan find this class of controller?
    fn found(self) -> bool {
        use crate::arch::imp::pci;
        use core::sync::atomic::Ordering::Relaxed;
        match self {
            // The DWC2 is SOLDERED to the SoC - there is no bus to discover it on. Its presence is
            // a property of the chip, so the ARCH answers rather than this file asking which ISA it
            // was built for; it read `cfg!(target_arch = "arm")` until every arch answered. Still the
            // one HwClass whose answer is not a scan result, which is why it is a seam member and not
            // a `pci::` scan like the three below it.
            HwClass::Dwc2 => pci::dwc2_present(),
            // Soldered to the SoC like the DWC2, so the ARCH answers, by kind.
            HwClass::AudioPwm => crate::arch::imp::fixed_device_present(kind::AUDIO_PWM),
            HwClass::WifiSdio => crate::arch::imp::fixed_device_present(kind::WIFI_SDIO),
            // Not a bus device at all: the display is found at boot (a Limine descriptor on x86, a GPU
            // mailbox call on the Pi) and the floor that brought it up is the one that knows.
            HwClass::Framebuffer => crate::bootcon::grant().is_some(),
            HwClass::Xhci => crate::arch::imp::pci::xhci().is_some(),
            HwClass::Ehci => crate::arch::imp::pci::ehci().is_some(),
            // Present if the bus reports one. On a port with no PCI the arch stub answers None,
            // which is the same "no NIC here" this used to get from a static nothing ever set.
            HwClass::Nic  => crate::arch::imp::pci::nic().is_some()
                             || crate::arch::imp::soc_nic_present(),
            // Not a device: the software-generated test interrupt (`FireIrq`) that IR1 delivers.
            // Always "present" because the kernel raises it itself - there is nothing to scan for.
            HwClass::TestIrq => true,
            // Present iff the device this request names is on the bus (`pci_dev`).
            HwClass::Pci { .. } => self.pci_dev().is_some(),
            HwClass::None => false,
        }
    }

    /// THE device a PCI spawn request names - the one place that decides it, so the window, the arena,
    /// the vector, the confinement and the bus mastering cannot each pick a different one.
    ///
    /// A supplied BDF selects the device; with none (0), the first device of the class, as before
    /// step D3. A supplied BDF whose device is NOT of the requested class is refused, loudly, rather than
    /// granted: the caller asked for a class and named something else, and on the T630 on 2026-10-08 a
    /// stale answer did exactly that and confined the SATA controller to `xhci`'s arena. Before this, a
    /// supplied BDF chose only the bus mastering and the confinement while the window, arena and vector
    /// came from the first device of the class - so on the T630 `audio-driver` was granted the HDMI audio
    /// controller's registers whatever it was told (docs/audio.md, "Found while preparing", 2).
    /// The bus device behind `mmio_bar`, for sizing its window: the one `pci_dev` resolves for a class
    /// request, the scan's for the three legacy names. `None` for anything not on a bus.
    fn bar_device(self) -> Option<crate::arch::imp::pci::PciDevice> {
        use crate::arch::imp::pci;
        match self {
            HwClass::Pci { .. } => self.pci_dev(),
            HwClass::Xhci => pci::xhci(),
            HwClass::Ehci => pci::ehci(),
            HwClass::Nic => pci::nic(),
            _ => None,
        }
    }

    fn pci_dev(self) -> Option<crate::arch::imp::pci::PciDevice> {
        use crate::arch::imp::pci;
        let HwClass::Pci { class_code, bdf, .. } = self else { return None };
        if bdf == 0 {
            return pci::find_by_class(class_code);
        }
        let d = (0..pci::MAX_DEVICES).filter_map(pci::device_at).find(|d| d.bdf == bdf)?;
        if d.class_code != class_code {
            crate::kprintln!(
                "task: BDF {:#06x} was supplied for class {:#08x} but that device is class {:#08x} - REFUSED, no device granted",
                bdf, class_code, d.class_code);
            return None;
        }
        Some(d)
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
            // ARM's fixed-address peripherals have their own path - `map_fixed_device`, by kind, which
            // the spawn logic calls precisely WHEN THE BAR IS 0, and which maps at a 32-bit VA with
            // Device/uncached + USER so the service reaches the registers through the SDK's safe
            // `Mmio` wrapper. The DWC2's address therefore belongs there, not here. Returning 0 is
            // how this class says "not on a bus" rather than "not present" - `found()` above is the
            // one that answers presence.
            // The BAR the DRIVER named, from the device the BUS reported. Never an address the
            // caller supplied - the index picks which of the six the scan already read.
            // `BAR_AUTO` asks for the first mapped memory BAR instead of a numbered one. This is
            // what lets ONE driver cover devices that put their registers in different places: the
            // e1000 uses BAR0, while the RTL8168 on the Wyse puts I/O ports there and its registers
            // in BAR2. Both are "the first memory BAR". Still not an address - a rule the kernel
            // evaluates over its own scan, exactly as an index is.
            HwClass::Pci { bar_ix: BAR_AUTO, .. } =>
                self.pci_dev()
                    .and_then(|d| d.bar.iter().copied().find(|&b| b != 0))
                    .unwrap_or(0),
            HwClass::Pci { bar_ix, .. } =>
                self.pci_dev().map_or(0, |d| d.bar[(bar_ix as usize).min(5)]),
            HwClass::Dwc2 => 0,
            // ZERO for the same reason as the DWC2: not on a bus, so not a BAR. The framebuffer has its
            // own grant path (the `HwClass::Framebuffer` branch in the spawn MMIO block) because it
            // needs geometry as well as a window, and because its size is whatever the display turned
            // out to be.
            HwClass::Framebuffer => 0,
            HwClass::Xhci => crate::arch::imp::pci::xhci().map_or(0, |d| d.bar[0]),
            HwClass::Ehci => crate::arch::imp::pci::ehci().map_or(0, |d| d.bar[0]),
            // A DEVICE OF THE CLASS IS A DEVICE, whatever chip it turns out to be. This arm used to
            // read a per-class static AND require the vendor to be one of two the kernel had been
            // taught (`0x100E_8086` e1000, `0x8168_10EC` RTL8168). That whitelist is precisely what
            // step D removes: a third card would have enumerated, appeared in the table, been named
            // by its own class code - and been handed no MMIO, because the kernel had not heard of
            // it. Whether a driver can drive the chip it finds is the DRIVER's judgement, and it can
            // read the vendor id for itself (InspectKernel query 14).
            HwClass::Nic => crate::arch::imp::pci::nic()
                .map_or(0, |d| crate::arch::imp::pci::first_memory_bar(&d)),
            _ => 0,
        }
    }
    /// A discovered DMA-capable controller needs a physically-contiguous DMA arena.
    fn needs_dma(self) -> bool {
        // The framebuffer is not a DMA master - the display scans it, this service only writes it - so
        // it gets a window and no arena. The test interrupt is not a device at all: it only needs a
        // vector routed to an endpoint. Everything else that is `found()` DMAs.
        //
        // TestIrq was caught here rather than reasoned out: `found()` is `true` for it (the kernel
        // raises the vector itself), so it fell through and `probe-11a` was handed a 64 KiB arena
        // whose permanent physical reservation is the one `dma_phys_slot` gives unclassified
        // callers - `XHCI_DMA_PHYS`, the REAL xHCI driver's. A test probe aliasing a driver's DMA
        // region, on a machine that has both. Every suite passed while it did.
        // A PCI device DMAs iff the caller asked for an arena. Zero pages = a register-only
        // driver, which is a legitimate shape (block-driver's AHCI needs one, a serial card does not).
        if let HwClass::Pci { dma_pages, .. } = self {
            return dma_pages > 0 && self.found();
        }
        // The radio has no arena: every command it issues rides the SDIO command line (`docs/wifi.md`).
        self != HwClass::None && self != HwClass::Framebuffer && self != HwClass::TestIrq
            && self != HwClass::WifiSdio && self.found()
    }
    /// Arena size: a PCI spawn states its own (xHCI's room for its 256-buffer scratchpad among them); the
    /// PWM audio jack gets `AUDIO_PWM_DMA_PAGES`; every other named class 64 KiB.
    fn dma_pages(self) -> u64 {
        match self {
            // The CALLER states it: how much DMA a driver needs is the driver's fact, and the
            // xHCI-needs-more special case was the last per-class size in the kernel.
            HwClass::Pci { dma_pages, .. } => dma_pages as u64,
            HwClass::Xhci => XHCI_DMA_PAGES,
            HwClass::AudioPwm => AUDIO_PWM_DMA_PAGES,
            _ => EHCI_DMA_PAGES,
        }
    }
    /// The permanent per-class DMA phys reservation, reused across respawns (§12 DMA permanent-reserve).
    fn dma_phys_slot(self) -> &'static portable_atomic::AtomicU64 {
        match self {
            HwClass::Dwc2 => &DWC2_DMA_PHYS,
            HwClass::AudioPwm => &AUDIO_PWM_DMA_PHYS,
            HwClass::Xhci => &XHCI_DMA_PHYS,
            HwClass::Ehci => &EHCI_DMA_PHYS,
            HwClass::Nic  => &NIC_DMA_PHYS,
            // None of these DMA, so none ever reaches here - `needs_dma()` gates every caller. They
            // return a real slot rather than panicking because an unreachable arm that aborts is a
            // crash waiting for a refactor, and a wrong-but-unused reservation is not. It must STAY
            // unused: this hands back the xHCI driver's own reservation, so a class that reached here
            // by accident would alias it (TestIrq did, until `needs_dma` was corrected).
            // Per DEVICE, not per class - a device the kernel cannot name still needs its arena
            // handed back on restart. `index` is the scan slot, stable for the boot.
            HwClass::Pci { .. } =>
                match self.pci_dev() {
                    Some(d) if d.index < crate::arch::imp::pci::MAX_DEVICES => &PCI_DMA_PHYS[d.index],
                    // Unreachable: `needs_dma()` gates every caller and is false when the device is
                    // absent. Returns a real slot rather than panicking, as the arms below do.
                    _ => &PCI_DMA_PHYS[0],
                },
            HwClass::None | HwClass::Framebuffer | HwClass::TestIrq | HwClass::WifiSdio => &XHCI_DMA_PHYS,
        }
    }
    /// Confine this DMA-capable driver via the IOMMU? xHCI, plus any PCI spawn whose request asks for it
    /// (`audio-driver` does; CLAUDE.md 6.4, the 2026-10-03 amendment). Not ehci + block-driver, which
    /// keep a stale firmware DMA pointer that confinement would fault, so they stay in passthrough.
    fn iommu_confine(self) -> bool {
        match self {
            // Policy, so the caller states it (§6.4). `ehci` and `block-driver` keep a stale
            // firmware DMA pointer that confinement would fault, which is why this was never
            // "confine every driver" in the first place.
            HwClass::Pci { confine, .. } => confine,
            _ => self == HwClass::Xhci,
        }
    }
    /// The device's PCI BDF (bus/device/function) for the bus-master + D0 enable, or 0xFFFF if none.
    fn bdf(self) -> u32 {
        use crate::arch::imp::pci;
        use core::sync::atomic::Ordering::Relaxed;
        match self {
            HwClass::Dwc2 => 0xFFFF, // no PCI on this board, so no bus-master enable to perform
            HwClass::AudioPwm => 0xFFFF, // the same: an SoC block, not a PCI device
            HwClass::WifiSdio => 0xFFFF,
            HwClass::Framebuffer => 0xFFFF, // not a PCI device
            HwClass::TestIrq     => 0xFFFF, // not a device at all - a software-raised vector
            // A SUPPLIED BDF WINS, because it is the caller saying WHICH device rather than the
            // kernel guessing from a class. `find_by_class` returns the FIRST match, so on a machine
            // with two devices of one class it picks by scan order - which is not a decision the
            // kernel has any basis to make. Zero means "not supplied": fall back to the class.
            HwClass::Pci { class_code, bdf, .. } if bdf != 0 => {
                // The device `pci_dev` resolved, so the bus mastering and the confinement are the same
                // device the window, arena and vector came from. One it refused gets none of them.
                if self.pci_dev().is_none() {
                    return 0xFFFF;
                }
                // Cross-check while both paths exist (step D3 is additive until the scan goes).
                let by_class = crate::arch::imp::pci::find_by_class(class_code).map_or(0xFFFF, |d| d.bdf);
                if by_class != 0xFFFF && by_class != bdf {
                    crate::kprintln!(
                        "task: BDF {:#06x} supplied for class {:#08x}, the first of that class is {:#06x} - the supplied one is granted",
                        bdf, class_code, by_class);
                } else {
                    // SAID OUT LOUD, because agreement and absence look identical otherwise. Without
                    // this line a supplied BDF that matches the scan produces exactly the same log as
                    // no BDF at all - so "no disagreement" would be evidence of nothing, and the
                    // switch-over would be unverifiable at the very moment it starts mattering.
                    crate::kprintln!(
                        "task: device chosen by SUPPLIED BDF {:#06x} for class {:#08x} (scan agrees)",
                        bdf, class_code);
                }
                bdf
            }
            // NO BDF SUPPLIED - the kernel falls back to its own scan, and SAYS SO.
            //
            // This is the last thing standing between here and D3's actual goal, and until now it was
            // silent. Every driver spawn on all four machines logged "chosen by SUPPLIED BDF", but a
            // spawn that took THIS path logged nothing at all, so "the reporter always answers" was an
            // inference from an absence rather than a fact - which is the exact shape of mistake this
            // work keeps uncovering. One line converts it into evidence: if no hardware run ever
            // prints it, the fallback is dead code and the scan can go with it.
            HwClass::Pci { class_code, .. } => {
                let by_class = crate::arch::imp::pci::find_by_class(class_code).map_or(0xFFFF, |d| d.bdf);
                crate::kprintln!(
                    "task: NO BDF supplied for class {:#08x} - fell back to the kernel's own scan, which says {:#06x}",
                    class_code, by_class);
                by_class
            }
            HwClass::Xhci => crate::arch::imp::pci::xhci().map_or(0xFFFF, |d| d.bdf),
            HwClass::Ehci => crate::arch::imp::pci::ehci().map_or(0xFFFF, |d| d.bdf),
            HwClass::Nic  => crate::arch::imp::pci::nic().map_or(0xFFFF, |d| d.bdf),
            HwClass::None => 0xFFFF,
        }
    }
}

/// The core each USB host-controller driver is expected on, as the kernel's MSI/INTx destination
/// routing (`arch::x86_64::pci`) reads it, so a controller interrupt is delivered to the core the
/// driver runs on (§12). NOT a single source of truth any more: the drivers' placement now comes from
/// the supervisor's `USB_IMAGES` rows (`services/supervisor/src/main.rs`), which carry the same 2 and 3
/// as literals, and nothing checks that the two agree. (2026-10-09: this said `ServiceConfig.preferred_core`
/// read these; the kernel catalogue is `supervisor` alone, so no catalogue row does.) Co-location is required for interrupt-driven USB (docs/power.md):
/// a keypress MSI must wake the driver's OWN core out of its idle `hlt` locally, because a cross-core
/// wake to a halted AP is not serviced promptly on this hardware. Both sit on cores 2/3 (off core 1)
/// because busy-polling two controllers on one core saturated it; when they block, that can relax.
pub const XHCI_CORE: u32 = 2;
pub const EHCI_CORE: u32 = 3;

/// Is this a device class this kernel understands? 0 = none.
///
/// An UNKNOWN class is refused rather than treated as none: a spawner asking for a device this kernel
/// cannot grant must hear so, not receive a driver with no MMIO window that enumerates nothing and
/// reports no error (invariant 12).
pub fn hw_class_known(class: u32) -> bool {
    // Bit 31 = "this is a PCI CLASS CODE, not one of the kernel's named kinds". Any class code is
    // acceptable BY DESIGN - that is the whole of step D1. The kernel does not have a list of the
    // ones it knows, because having one is what forced a kernel rebuild per driver.
    if class & HW_PCI_FLAG != 0 { return true; }
    // DERIVED from the decoder, not a second copy of its range: this read `class <= 7`, so adding the
    // eighth kind (`AudioPwm`) to `hw_class_of` left every spawn of it refused with InvalidArgument -
    // found by the first boot that tried, in QEMU (`pwm-audio`, 2026-10-03).
    class == 0 || hw_class_of(class) != HwClass::None
}

/// `hw_flags` bit 31: the low bits describe a PCI device rather than a named kind.
pub const HW_PCI_FLAG: u32 = 1 << 31;
/// `hw_flags` bit 28: put this device behind the IOMMU (§6.4).
pub const HW_PCI_CONFINE: u32 = 1 << 28;
/// `hw_flags` bit 29: this driver wants an INTERRUPT - allocate a vector from the MSI pool and
/// program the device's MSI with it (step D1b).
pub const HW_PCI_IRQ: u32 = 1 << 29;

/// The MSI vector allocated to each PCI device, indexed by its slot in the scan table. 0 = none yet.
///
/// Per DEVICE, not per spawn, and that is what makes a restart work: the vector is written into the
/// device's own MSI register, so a respawned driver must be given the SAME one back - allocating a
/// fresh vector each time would both exhaust the pool after eight restarts and leave the controller
/// raising an interrupt nobody routes. Same reasoning as the permanent DMA reservation beside it.
static PCI_MSI_VEC: [core::sync::atomic::AtomicU8; crate::arch::imp::pci::MAX_DEVICES] =
    [const { core::sync::atomic::AtomicU8::new(0) }; crate::arch::imp::pci::MAX_DEVICES];

/// Next free slot in the MSI pool. Monotonic: a vector is never returned, because the device it was
/// written into keeps using it for the life of the boot.
static MSI_POOL_NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The vector this PCI device should raise, allocating and programming one on first use.
///
/// Returns 0 when there is nothing to route: no such device, the pool is exhausted, or the device
/// refused MSI. Every one of those is reported - a driver silently left without interrupts presents
/// later as a device that never responds, which is the diagnosis this saves (invariant 12).
fn pci_msi_vector(hw: HwClass, core_id: u32) -> u8 {
    use core::sync::atomic::Ordering;
    let HwClass::Pci { class_code, .. } = hw else { return 0 };
    let Some(d) = hw.pci_dev() else { return 0 };
    if d.index >= crate::arch::imp::pci::MAX_DEVICES { return 0; }

    // Deliver to the core the driver is pinned to, so a device event wakes that core directly out
    // of its idle halt rather than via a cross-core IPI - the same reasoning the named vectors use.
    // Recomputed on a restart because the respawn may land on a DIFFERENT core than the instance
    // that died (§9.2: placement is re-evaluated from scratch, the previous core is not remembered),
    // and an interrupt still aimed at the old core would be delivered where nobody is waiting.
    let dest = crate::arch::imp::pci::msi_dest_lapic(core_id);

    // A RESTART: same device, so the same vector - allocating a fresh one would both drain the pool
    // after eight restarts and leave the controller raising an interrupt nobody routes.
    //
    // But it is REPROGRAMMED rather than just handed back. Returning early without writing assumes
    // the device's MSI configuration survived the driver's death, and that is an assumption about
    // every driver that will ever use this pool, not a guarantee. It happens to hold for xHCI (MSI
    // lives in PCI config space; `HCRST` resets the operational registers, not config space) - but a
    // driver whose reset path is heavier, or a device that loses config on a function-level reset,
    // would come back with no MSI programmed at all. The failure would be silent and awful to
    // diagnose: the driver restarts, reports ready, and the keyboard never types again.
    //
    // The write is idempotent and costs a few config-space accesses once per spawn, so paying it
    // unconditionally is strictly cheaper than the class of bug it forecloses. The destination is
    // re-derived above, so this also FIXES the case where the respawn moved cores.
    let existing = PCI_MSI_VEC[d.index].load(Ordering::Relaxed);
    if existing != 0 {
        if !crate::arch::imp::pci::program_msi(d.bdf, existing, dest)
            && !crate::arch::imp::pci::program_msix(d.bdf, existing, dest)
        {
            // It took this vector once and will not take it now. Loud, because the driver is about
            // to run believing it has interrupts (invariant 12).
            crate::kprintln!(
                "pci-msi: BDF {:#06x} REFUSED its own vector {:#04x} on restart - no interrupt",
                d.bdf, existing);
            return 0;
        }
        crate::kprintln!("pci-msi: class {:#08x} BDF {:#06x} -> vector {:#04x} (restart, same vector)",
                         class_code, d.bdf, existing);
        return existing;
    }

    // The slot is consumed only once the device is actually PROGRAMMED. Reserving first and
    // programming after would let a device that accepts neither MSI nor MSI-X burn a slot on every
    // spawn attempt, so eight restarts of one such device would exhaust the pool for every other
    // device - a bounded resource drained by a path that never used it (§26.6).
    //
    // The CAS makes that safe without ASSUMING spawns are serialised. If another core took the slot
    // in between we retry with the next one and reprogram, so the last write to the device is
    // always the vector we end up holding.
    loop {
        let slot = MSI_POOL_NEXT.load(Ordering::Acquire) as usize;
        if slot >= crate::arch::imp::interrupts::MSI_POOL_LEN {
            crate::kprintln!(
                "pci-msi: pool exhausted ({} vectors) - class {:#08x} gets NO interrupt and must poll",
                crate::arch::imp::interrupts::MSI_POOL_LEN, class_code);
            return 0;
        }
        let vector = crate::arch::imp::interrupts::MSI_POOL_BASE + slot as u8;

        if !crate::arch::imp::pci::program_msi(d.bdf, vector, dest)
            && !crate::arch::imp::pci::program_msix(d.bdf, vector, dest)
        {
            crate::kprintln!(
                "pci-msi: BDF {:#06x} (class {:#08x}) accepted neither MSI nor MSI-X - no interrupt, must poll",
                d.bdf, class_code);
            return 0;   // no slot consumed
        }

        if MSI_POOL_NEXT.compare_exchange(
                slot as u32, slot as u32 + 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;   // lost the slot to another core; take the next one and reprogram
        }
        PCI_MSI_VEC[d.index].store(vector, Ordering::Relaxed);
        crate::kprintln!("pci-msi: class {:#08x} BDF {:#06x} -> vector {:#04x} (pool slot {})",
                         class_code, d.bdf, vector, slot);
        return vector;
    }
}

/// `bar_ix` value meaning "the first mapped MEMORY BAR" rather than a numbered one (see
/// `mmio_bar`). 7 is free because the field is 3 bits and only 0..5 are real BARs.
pub const BAR_AUTO: u8 = 7;

/// Decode a `hw_flags` PCI descriptor: `bit31 | confine<<28 | bar_ix<<24 | class_code`.
fn hw_pci_of(class: u32, dma_pages: u32, bdf: u32) -> HwClass {
    HwClass::Pci {
        class_code: class & 0x00FF_FFFF,
        bar_ix:     ((class >> 24) & 0x7) as u8,
        dma_pages,
        confine:    class & HW_PCI_CONFINE != 0,
        bdf,
    }
}

/// Does this `hw_flags` describe a PCI device? A BDF means nothing for the named non-bus kinds
/// (`Dwc2` is soldered on, the framebuffer is a boot handoff, the test IRQ is software), so supplying
/// one for those is a caller error worth refusing rather than ignoring.
pub fn hw_class_is_pci(class: u32) -> bool { class & HW_PCI_FLAG != 0 }

/// The named device kinds a spawn request may carry (the SDK's `hwclass` constants), shared with the
/// arch layer: it answers by KIND - is this kind present, map its window, can its power be cut - and
/// never by the name of the service that drives it (`docs/audio.md`, "No service names in the kernel").
pub mod kind {
    pub const NIC: u32 = 2;
    pub const XHCI: u32 = 3;
    pub const EHCI: u32 = 4;
    pub const DWC2: u32 = 5;
    pub const FRAMEBUFFER: u32 = 6;
    pub const TEST_IRQ: u32 = 7;
    pub const AUDIO_PWM: u32 = 8;
    pub const WIFI_SDIO: u32 = 9;
}

/// Resolve a spawn request's device class to the kernel's own scan results.
fn hw_class_of(class: u32) -> HwClass {
    if class & HW_PCI_FLAG != 0 { return hw_pci_of(class, 0, 0); }
    match class {
        kind::NIC => HwClass::Nic,
        kind::XHCI => HwClass::Xhci,
        kind::EHCI => HwClass::Ehci,
        kind::DWC2 => HwClass::Dwc2,
        kind::FRAMEBUFFER => HwClass::Framebuffer,
        kind::TEST_IRQ => HwClass::TestIrq,
        kind::AUDIO_PWM => HwClass::AudioPwm,
        kind::WIFI_SDIO => HwClass::WifiSdio,
        _ => HwClass::None,
    }
}

/// The interrupt vector(s) a device CLASS is served by, as THIS kernel routes them.
///
/// An IRQ vector is AUTHORITY, not a setting. Routing a vector to a task is what makes that task
/// receive the device's interrupts - on ARM, granting the USB vector is precisely what takes the
/// controller away from whoever held it. So a spawner able to NAME a vector could hand a service
/// another device's interrupts, which is the ambient authority 3.1 forbids.
///
/// The supervisor therefore names a device CLASS and the kernel supplies the vector IT assigned, by
/// the same rule and for the same reason as the MMIO window and the DMA arena (`SpawnImage` refuses
/// those outright). The vector is a property of the device and of this kernel's own routing, so it
/// is the kernel's to state - exactly the 26.14 question: a property of the device, not of the
/// caller's design.
fn hw_irqs_for(class: HwClass) -> &'static [u8] {
    match class {
        HwClass::Xhci => &[crate::arch::imp::interrupts::XHCI_MSI_VECTOR],
        HwClass::Ehci => &[crate::arch::imp::interrupts::EHCI_MSI_VECTOR],
        // THROUGH THE SEAM, like the two above it. These read
        //
        //     #[cfg(target_arch = "arm")]      HwClass::Dwc2 => &[arch::imp::irq::USB_VECTOR],
        //     #[cfg(not(target_arch = "arm"))] HwClass::Dwc2 => &[],
        //
        // and the same again for aarch64 and GENET - four `#[cfg]`s in a NEUTRAL file, which is the
        // leak §4.1 is about: a fifth port has to edit this function to be routed anything, and
        // nothing would have told it so. `XHCI_MSI_VECTOR` on the line above was always done the
        // right way round; these two simply were not, because at the time only one arch answered.
        //
        // The aarch64 arm also carried a bare `0x2A` while `arch/aarch64/exceptions.rs` already
        // defined `GENET_VECTOR = 0x2A` - a second copy of a constant, inside the leak (Commandment
        // III). The seam answer names the constant.
        HwClass::Dwc2 => crate::arch::imp::interrupts::DWC2_VECTORS,
        HwClass::Nic  => crate::arch::imp::interrupts::SOC_NIC_VECTORS,
        // The SOFTWARE test interrupt (§22 IR1): `control` raises vector 33 with FireIrq and the
        // kernel must route it to the registered driver's endpoint. It is a vector like any other -
        // the caller names the class and the kernel states the number - which is what lets the probe
        // that receives it stop being a kernel-known name.
        HwClass::TestIrq => &[33],
        HwClass::Framebuffer | HwClass::None => &[],
        // No vector: the DMA engine's interrupt lines are shared between channels, and routing one
        // would hand over the others'. The driver polls its ring, which `gs::driver::irq` supports.
        HwClass::AudioPwm | HwClass::WifiSdio => &[],
        // NO VECTOR YET, and this is the honest edge of step D1 rather than an oversight.
        //
        // The named classes above return a vector the KERNEL assigned and programmed into the
        // device's MSI/MSI-X at boot (`pci: MSI-X enabled on 00:04.0 vector=0x28`). Doing that for a
        // device the kernel cannot name means allocating a vector from a pool and programming that
        // device's MSI generically - which is real work, and squarely the kernel's (interrupt
        // routing is one of the six, §4.3), so it belongs here rather than in the caller.
        //
        // IT LANDED. This paragraph used to end "until it lands, a driver spawned by CLASS CODE gets
        // MMIO, DMA and its BDF but no interrupt: it must poll" - and the very next line already said
        // it was resolved. Both sentences stood, and the first one reads as current to anyone who
        // stops there. A comment that describes a limitation the code below it removed is the same
        // defect as a counter nobody increments: it looks like fact.
        //
        // Resolved at the SPAWN SITE instead, by `pci_msi_vector`: allocating a vector needs the
        // core the driver will run on (for the delivery destination), and this returns a 'static
        // slice which an allocated value cannot live in.
        HwClass::Pci { .. } => &[],
    }
}

/// The hardware class + resource-mint authority a CATALOGUE spawn is granted, keyed by name (audit M7
/// / T1 Phase B). Every arm has gone: drivers name their device class, and `fs`/`net-stack` carry
/// RESOURCE_MINT, in the supervisor's spawn request. What is left answers `(None, false)` for every
/// name, including `supervisor`.
fn service_hw(name: &str) -> (HwClass, bool) {
    match name {
        // EVERY driver has moved to the supervisor (step C). Each names its device CLASS in the
        // spawn request - Ahci, Framebuffer, Xhci, Ehci, Dwc2, Nic - and the kernel resolves that
        // against its OWN bus scan, so the caller never names an address or an interrupt vector.
        // Nothing is left to look up by name here.
        // `resource-server`, `fs` and `net-stack` all moved to the supervisor (step C): their
        // RESOURCE_MINT arrives in the spawn request, checked against what the SUPERVISOR may
        // delegate, instead of being granted here by name. Nothing is left in this arm.
        _                                      => (HwClass::None, false),
    }
}

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
    /// ARM-only in practice (`cfg!(target_arch = "arm")`), but the BIT is arch-neutral: the kernel
    /// still refuses it unless the caller may delegate it, and the supervisor's table simply does not
    /// set it elsewhere.
    pub const GPIO:            u32 = 1 << 8;
    /// SET_CLOCK with READ, not WRITE. The narrow right: raise the persisted clock FLOOR, which only
    /// constrains which clock values are acceptable - where WRITE would let the holder step every
    /// task's view of the time of day. Granting plain SET_CLOCK instead would hand over exactly the
    /// authority that split was built to withhold, and would fail anyway (a WRITE cap does not satisfy
    /// a READ check).
    pub const SET_CLOCK_FLOOR: u32 = 1 << 9;
    /// SET_CLOCK with WRITE: SET the wall clock (net-stack held it for SNTP on RTC-less ARM; nothing does
    /// since the clock moved to `time`, and no syscall spends it - `SET_CLOCK_RESOURCE`). Distinct
    /// from SET_CLOCK_FLOOR above, which is the same resource with READ - see the note there for why
    /// the split exists and must not be collapsed.
    pub const SET_CLOCK:       u32 = 1 << 10;
    /// NET_DEVICE: move ethernet frames through the in-kernel network device (`NetFrame*`/`NetInfo`,
    /// syscalls 42-44). HELD BY NOTHING NOW: on arm32 the USB-net device moved into the `dwc2`
    /// SERVICE, and on aarch64 GENET moved into `nic-driver` itself, which drives the MAC through its
    /// own register window. The syscalls are stubs, the supervisor does not hold the bit (so cannot
    /// delegate it), and no spawn row asks for it - see `service_privileges` and `backlog/21`.
    ///
    /// It got a bit when `nic-driver`'s image moved to the supervisor, so that a moved service could
    /// still receive an authority it then used. The bit stays so that a request naming it is
    /// understood and refused by `privileges_caller_lacks`, rather than silently ignored.
    pub const NET_DEVICE:      u32 = 1 << 11;
    /// PCI_CFG: read PCI configuration space through the legacy CF8/CFC ports (step D2).
    ///
    /// READ-ONLY, permanently, and the kernel enforces WHICH PORTS rather than what they mean. CF8
    /// selects which register CFC reaches, so a CFC write would be a write to any config register of
    /// any device on the bus; there is no narrower form of that at port granularity, so it is not on
    /// offer. Held by ONE service (`hw-enumerator`), because the pair is stateful - two holders do
    /// not merely race, they silently read each other's device.
    pub const PCI_CFG:         u32 = 1 << 12;
    /// CPU_CLOCK: set the Arm cores to the platform's minimum or maximum clock (`CpuClock`, syscall 55).
    /// Held by ONE service, `power`, which owns the policy - who may ask for speed and for how long.
    /// One holder because the clock is one machine-wide setting: two holders would simply overwrite
    /// each other, and the second would never know (`docs/power.md`).
    pub const CPU_CLOCK:       u32 = 1 << 13;
    /// Every bit this kernel understands. Anything outside it is refused, so a newer spawner cannot
    /// quietly ask for a privilege this kernel would ignore.
    pub const KNOWN: u32 = SPAWN | CONSOLE_PUSH | INTROSPECT | SERVICE_CONTROL
                         | FIRE_IRQ | REBOOT | ACQUIRE_ANY | RESOURCE_MINT
                         | GPIO | SET_CLOCK_FLOOR | SET_CLOCK | NET_DEVICE | PCI_CFG | CPU_CLOCK;
}

/// Which requested privilege the CALLING task does not itself hold, if any.
///
/// `None` means every requested bit is one the caller could already exercise, so passing it to a
/// child grants nothing new - the same non-escalation argument as an installed cap (7.3).
pub fn privileges_caller_lacks(requested: u32) -> Option<&'static str> {
    use crate::capability::*;
    if requested & !privbits::KNOWN != 0 { return Some("an unknown privilege bit"); }
    let checks: [(u32, ResourceId, &'static str); 14] = [
        (privbits::SPAWN,           SPAWN_RESOURCE,           "SPAWN"),
        (privbits::CONSOLE_PUSH,    CONSOLE_PUSH_RESOURCE,    "CONSOLE_PUSH"),
        (privbits::INTROSPECT,      INTROSPECT_RESOURCE,      "INTROSPECT"),
        (privbits::SERVICE_CONTROL, SERVICE_CONTROL_RESOURCE, "SERVICE_CONTROL"),
        (privbits::FIRE_IRQ,        FIRE_IRQ_RESOURCE,        "FIRE_IRQ"),
        (privbits::REBOOT,          REBOOT_RESOURCE,          "REBOOT"),
        (privbits::ACQUIRE_ANY,     ACQUIRE_ANY_RESOURCE,     "ACQUIRE_ANY"),
        (privbits::RESOURCE_MINT,   RESOURCE_MINT_RESOURCE,   "RESOURCE_MINT"),
        (privbits::GPIO,            GPIO_DEVICE_RESOURCE,     "GPIO"),
        (privbits::SET_CLOCK_FLOOR, SET_CLOCK_RESOURCE,       "SET_CLOCK_FLOOR"),
        (privbits::SET_CLOCK,       SET_CLOCK_RESOURCE,       "SET_CLOCK"),
        (privbits::NET_DEVICE,      NET_DEVICE_RESOURCE,      "NET_DEVICE"),
        (privbits::PCI_CFG,         PCI_CFG_RESOURCE,         "PCI_CFG"),
        (privbits::CPU_CLOCK,       CPU_CLOCK_RESOURCE,       "CPU_CLOCK"),
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
    (privbits::GPIO,            GPIO_DEVICE_RESOURCE),
    (privbits::SET_CLOCK_FLOOR, SET_CLOCK_RESOURCE),
    (privbits::SET_CLOCK,       SET_CLOCK_RESOURCE),
    (privbits::NET_DEVICE,      NET_DEVICE_RESOURCE),
    (privbits::PCI_CFG,         PCI_CFG_RESOURCE),
    (privbits::CPU_CLOCK,       CPU_CLOCK_RESOURCE),
];

/// The privileges a CATALOGUE spawn is granted (`service_privileges`). The catalogue is `supervisor`
/// alone, so in practice these are the supervisor's own; every other task's privileges arrive in its
/// spawn request and are checked by `privileges_caller_lacks`. The per-field notes say who holds each
/// authority on the running system, by either route.
struct Privileges {
    spawn:           bool, // SPAWN: create tasks (supervisor, the shell, chaos' spawn-burst, probes)
    console_push:    bool, // CONSOLE_PUSH: inject keystrokes into the input ring (USB keyboard drivers)
    introspect:      bool, // INTROSPECT: read another task's / system-wide kernel state (§3.1)
    service_control: bool, // SERVICE_CONTROL: kill/restart other services (§14.4)
    fire_irq:        bool, // FIRE_IRQ: inject a test interrupt (`control` only - C1-6)
    reboot:          bool, // REBOOT: hardware-reset the machine (shell `reboot` only - SEC-2)
    acquire_any:     bool, // ACQUIRE_ANY: reach ARBITRARY services by name via AcquireSendCap (§3.1)
    net_device:      bool, // NET_DEVICE: move ethernet frames via an in-kernel net device (held by nothing now)
    pci_cfg:         bool, // PCI_CFG: read PCI config space via CF8/CFC (hw-enumerator, step D2)
    cpu_clock:       bool, // CPU_CLOCK: set the Arm cores to their minimum or maximum rate (power)
    usb_disk:        bool, // USB_DISK: read/write blocks on an in-kernel USB mass-storage device (held by nothing now)
    gpio:            bool, // GPIO_DEVICE: drive the SoC GPIO pins (ARM `gpio` shell command)
    set_clock:       bool, // SET_CLOCK (WRITE): set the wall clock (no syscall spends it now; SET_CLOCK_RESOURCE)
    set_clock_floor: bool, // SET_CLOCK (READ): raise the persisted clock floor only (the shell)
    /// IMAGE_SPAWN: start a task from a CALLER-SUPPLIED image (`SpawnImage`). The supervisor alone -
    /// see `IMAGE_SPAWN_RESOURCE` for why this is not the same authority as `spawn`, and why it is
    /// deliberately absent from `SUPERVISOR_DELEGATABLE`.
    image_spawn:     bool,
}

fn service_privileges(name: &str) -> Privileges {
    Privileges {
        // supervisor is the spawner (init removed, Phase 5); the shell brokers spawns; chaos spawns
        // mem-pressure tasks for max-carnage's spawn-burst dimension; probes spawn victims.
        spawn: matches!(name, "supervisor"),
        // The supervisor is the only principal that HOLDS images, and the only caller of
        // `SpawnImage` in the tree. Everything else asks it over IPC (`supcmd::SPAWN`).
        image_spawn: matches!(name, "supervisor"),
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
        // NOT BY NAME. This read `matches!(name, "xhci" | "ehci" | "dwc2")`, and it could never fire:
        // this table answers only for a CATALOGUE spawn, and the catalogue is `supervisor` alone. The
        // USB drivers' CONSOLE_PUSH arrives in their spawn rows, checked against what the supervisor may
        // delegate. Removed with the kernel's other service names (`docs/audio.md`).
        console_push: false,
        // THE PREFIX HOLE IS CLOSED. This used to read
        //     `matches!(name, "supervisor") || name.starts_with("prop-") || name.starts_with("stress-")`
        // and was recorded here as a known one (26.7): probe names are caller-supplied, so a service
        // holding a spawn cap could obtain INTROSPECT BY CHOOSING A STRING. The fix named in that note
        // - "carry a privilege word like `SpawnImage` does, checked against what the CALLER may
        // delegate" - is what moving the probe image did. The prop-/stress- drivers get INTROSPECT
        // from `probes::privileges_of` in the supervisor's request now, and adv-a11 still gets none,
        // so the test whose subject is a probe WITHOUT the cap keeps its subject.
        //
        // Only the supervisor is left, because only the supervisor is still spawned by name.
        introspect: matches!(name, "supervisor"),
        // shell (interactive broker), supervisor (restart authority), chaos (the point of max-carnage),
        // and every probe (they kill victims to exercise kill/revocation).
        service_control: matches!(name, "supervisor"),
        // SEC-2: REBOOT lives ONLY with the shell (its `reboot` command); the USB drivers no longer
        // hold it. A keyboard driver can synthesize any keystroke (the console's inherent trust, §6.4),
        // but it must not ALSO be able to hard-reset the machine directly from any context.
        // FIRE_IRQ: only the control service. It exists so the COM2 command interpreter could leave
        // the kernel; naming the authority is what made that possible (C1-6).
        fire_irq: false, // `control` carries FIRE_IRQ in its spawn request now (step C)
        reboot: false, // the shell carries REBOOT in its spawn request now (step C)
        // Operator/test instruments that legitimately reach arbitrary services by name: shell (chaos
        // flooding, pipe sinks), supervisor (reconcile-by-name), probes. `adv-a13` is the §22 Test A13
        // NEGATIVE pin - deliberately excluded so it holds no ACQUIRE_ANY (proves AcquireSendCap denies
        // a non-holder). Ordinary services get none; their AcquireSendCap is limited to declared peers.
        acquire_any: matches!(name, "supervisor"),
        // NET_DEVICE, GPIO_DEVICE, USB_DISK and SET_CLOCK were once kernel-only BY-NAME grants here (the
        // U15 / userspace-audit A5-U1 doctrine). None is granted by name any more: GPIO and the clock
        // bits travel in the spawn request like every other privilege, and NET_DEVICE and USB_DISK are
        // held by nothing (below).
        // NOTHING HOLDS NET_DEVICE ANY MORE, for the same two reasons `usb_disk` below reads
        // `false`, and this arm is now dead in exactly the same way that one is.
        //
        // (a) It named `nic-driver` alone, and `nic-driver`'s image moved to the SUPERVISOR - this
        //     table is only the fallback for a catalogue spawn (`None => service_privileges(name)`)
        //     and the kernel catalogue is `supervisor` alone, so the arm could not fire even if the
        //     authority were still wanted.
        // (b) It is not wanted. arm32 left this set when its USB-net device moved into the `dwc2`
        //     service; aarch64 left it when GENET moved into `nic-driver` itself (CLAUDE.md §6.4,
        //     2026-08-09) and the service began driving the MAC through its own register window. The
        //     NetFrame syscalls 42-44 have had no caller in any service since - `backlog/21` - and
        //     the supervisor stopped requesting the privilege in `10d3b43e`, which booted on the Pi 4
        //     with DHCP, ARP, ping and SNTP all working. So the capability has already been absent on
        //     hardware for a boot; this removes the dead arm rather than changing anything.
        net_device: false,
        // No service in the KERNEL's catalogue holds PCI_CFG. `hw-enumerator` is a moved service -
        // the supervisor owns its image and requests this privilege in the spawn request, which the
        // kernel grants only because the supervisor itself holds a GRANT cap for it. That is step C's
        // shape and the reason this reads `false` rather than naming a service (§7.4).
        pci_cfg: false,
        // CPU_CLOCK: the same shape as PCI_CFG above - `power` is a supervisor-owned service, which asks
        // for the bit in its spawn request; nothing in the kernel's own catalogue holds it.
        cpu_clock: false,
        // Nothing holds USB_DISK any more. It named `block-driver` alone, and `block-driver`'s image
        // moved to the supervisor - so this arm could not fire even if the authority were still
        // wanted, and it is not: the driver reaches its stick through the `dwc2` / `xhci` service over
        // IPC on both ARM ports. Left as `false` rather than deleted along with the resource and its
        // three syscalls, which is a separate change (audit SEC-37).
        usb_disk: false,
        //   the shell's `gpio` command drives the SoC pins (the gated `Gpio` syscall, 45).
        gpio: false, // delegated in the spawn request (step C)
        //   SET_CLOCK, in two strengths (rights narrow, §7.4): WRITE = set the wall clock, READ = raise
        //   the persisted clock FLOOR only. NO SYSCALL CHECKS EITHER any more: the wall clock moved to the
        //   `time` SERVICE in clock slice 3, so `SET_CLOCK_RESOURCE` is minted and never spent (see its
        //   note in `capability/mod.rs`, `backlog/59`). The supervisor still delegates SET_CLOCK_FLOOR to
        //   the shell and no longer requests SET_CLOCK for net-stack.
        set_clock:       false, // never by name; see above
        set_clock_floor: false, // delegated in the spawn request (step C)
    }
}

/// Did the calling task declare `peer` as a send-peer at spawn? If so, reacquiring a SEND cap to it
/// (`AcquireSendCap`) is authorized recovery (§14.2), not ambient authority (§3.1).
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
        // The probe entry is GONE with the rest: the supervisor holds the test image and supplies
        // every parameter, privilege and authority in the spawn request. `supervisor` is the only
        // service the kernel still bootstraps, because nothing is beneath it.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public spawn API.
// ---------------------------------------------------------------------------

/// Resolve which core a spawn lands on: an explicit (strict) override, else the
/// preferred core (falling back, loudly, to round-robin if it isn't ready), else
/// round-robin across ready cores (`preferred_core == u32::MAX`).
fn resolve_spawn_core(core_override: Option<u32>, preferred_core: u32) -> Result<u32, SpawnError> {
    use core::sync::atomic::{AtomicU32, Ordering};
    static RR: AtomicU32 = AtomicU32::new(0);
    match core_override {
        // A core requested with SPAWN_FLAG_CORE_STRICT - an operator's `--core N`, or a restart's
        // placement_override - is STRICT (§9.2): if it is not ready, REJECT with PlacementInvalid
        // rather than placing the service on a core no scheduler runs (which would strand it).
        //
        // NOT this arm: a CONTRACT's `placement.core`. That arrives as `preferred_core` (see
        // `SPAWN_FLAG_CORE_STRICT` in syscall/dispatch.rs) and is rerouted, not rejected - which is
        // what §9.2 and §13.2 forbid in the strictest language the constitution uses. That gap is
        // backlog/01 and is a CLAUDE.md decision, not an implementation one. What the arm below
        // does guarantee is that the reroute is LOUD.
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
                // A PREFERENCE that could not be honoured is reported (invariant 12). It stays a
                // reroute - §11.3 needs a machine with fewer ready cores to come up - but a silent
                // reroute is the exact failure backlog/01 opened on, and saying so costs nothing.
                let count = crate::smp::core::ready_count() as u32;
                let chosen = RR.fetch_add(1, Ordering::Relaxed) % count.max(1);
                crate::kprintln!(
                    "task: preferred core {} is not ready - placing on core {} instead (§9.2 preference)",
                    preferred_core, chosen);
                Ok(chosen)
            }
        }
    }
}

/// Spawn a producer and delegate it a SEND cap to `sink`'s endpoint as its
/// `send_peers[0]` - the capability-broker primitive behind shell pipes
/// (`producer | sink`). The pipe peer goes first, followed by the producer's
/// catalogue send peers (see the comment in the body).
///
/// `producer` is looked up in the kernel catalogue, which holds `supervisor` alone,
/// so this refuses every producer today (`NotFound`). Its one caller is the
/// supervisor's fallback for a name it holds no image for.
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
        cfg.memory_limit, cfg.hw_irqs, cfg.has_console_read, None, None, None, false);
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
/// kernel's own catalogue uses. Letting a caller pick such a name would let it inherit that name's
/// policy for arbitrary code - the same squatting hole the kernel's own probe spawn closed until the
/// probe image left it (`dd5d176b`), and for the same reason (the kernel name directory is the
/// recovery anchor). The catalogue has reached its single `supervisor` entry, so this check is now
/// that one name, which must never be claimable by anything.
pub fn spawn_from_image(
    name:              &str,
    image:             crate::loader::ImageSource,
    // A STRICT placement: reject with `PlacementInvalid` if that core is not ready (§9.2).
    core_override:     Option<u32>,
    // The service's PREFERRED core (`u32::MAX` = none): used only when there is no override, and
    // falls back to round-robin if that core is not ready, so a machine with fewer cores still boots.
    core_preferred:    u32,
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
    // The device class this service drives (0 = none). Its IRQ lines are DERIVED from the class by
    // `hw_irqs_for` rather than named by the caller - see that function for why a vector is
    // authority. `SpawnImage` refuses a request that tries to name one.
    hw_class:          u32,
    // DMA arena size in pages, for a PCI descriptor. A size, not an address (see the syscall).
    dma_pages:         u32,
    // WHICH device, when the caller knows it (step D3). An IDENTIFIER, not an address: the kernel
    // reads that device's own registers to learn its BAR and IRQ, so naming it grants nothing that
    // naming the class would not. Zero = not supplied, and the class is resolved against the
    // kernel's own scan as before.
    bdf:               u32,
    // Mint the name-wired peer caps with GRANT so the child may re-delegate them (§22 Test 5A).
    peers_grant:       bool,
    // The spawner asks to hear of this task's death and to have it counted as a restart
    // (`SPAWN_FLAG_WATCHED`). The supervisor sets it for what it manages; the kernel holds no list.
    watched:           bool,
) -> Result<Option<EndpointId>, SpawnError> {
    if service_config(name).is_some() {
        crate::kprintln!("task: SpawnImage '{}' rejected: that name belongs to the kernel catalogue", name);
        return Err(SpawnError::NotFound);
    }
    if scheduler::find_task_by_name(name).is_some() {
        crate::kprintln!("task: spawn '{}' rejected: already running", name);
        return Err(SpawnError::AlreadyRunning);
    }

    let core_id = resolve_spawn_core(core_override, core_preferred)?;
    let mem = if memory_limit == 0 { 64 * 1024 * 1024 } else { memory_limit };

    let hw = if hw_class & HW_PCI_FLAG != 0 { hw_pci_of(hw_class, dma_pages, bdf) }
             else { hw_class_of(hw_class) };

    // A PCI driver that asked for an interrupt gets a pool vector, allocated once per DEVICE and
    // programmed into its MSI. Held in a local so it can be passed as a slice - `hw_irqs_for`
    // returns 'static and cannot carry an allocated value.
    let pci_irq: [u8; 1] = match hw {
        HwClass::Pci { .. } if hw_class & HW_PCI_IRQ != 0 =>
            [pci_msi_vector(hw, core_id)],
        _ => [0],
    };
    let irqs: &[u8] = if pci_irq[0] != 0 { &pci_irq } else { hw_irqs_for(hw) };
    let result = spawn_service_with_image(name, image, core_id, has_recv_endpoint, peers, mode,
                                          peers_grant, mem, irqs, has_console_read,
                                          Some(privileges), Some(hw), installs, watched);
    if let Err(ref e) = result {
        crate::kprintln!("task: SpawnImage '{}' failed: {:?}", name, e);
    }
    result
}

/// Spawn the one service the kernel still bootstraps, from the one row it still holds.
///
/// This used to start 221 services. What remains is `supervisor`, because nothing is beneath it -
/// every other image, parameter and privilege now belongs to the supervisor and arrives in a
/// `SpawnImage` request (`docs/service-ownership.md`).
///
/// The last two things to leave were AUTHORITY rather than settings, and are worth recording because
/// the split they forced is now resolved: a probe's IRQ route and its grantable peer caps were kept
/// keyed by name here, on the rule "the kernel decides what a service may DO, the caller says what it
/// IS". Both now travel in the request without weakening that rule - the IRQ as a device CLASS whose
/// vector the kernel still chooses (`hw_irqs_for`), the grant as a flag checked like any privilege.
/// The caller still cannot assert an authority; it can only name one the kernel already understood.
pub fn spawn_service_by_name(name: &str, core_override: Option<u32>) -> Result<Option<EndpointId>, SpawnError> {
    let (static_name, cfg) = service_config(name).ok_or(SpawnError::NotFound)?;

    // Singleton guard (§6.2, §26.6 bounded behaviour): refuse to spawn a service
    // whose name is already live. This blocks duplicate instances in general, and
    // in particular a second supervisor - while it is live, this rejects
    // spawning another by name (`handle_kill` no longer refuses it: the kernel
    // respawns the supervisor on death, §6.2). It does NOT block boot: there
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
                              cfg.has_console_read, None, None, None, false);
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
                              cfg.has_console_read, None, None, Some(installs), false);
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
    // The device record too: a spawn can fail after recording it, and the kill path quiesces whatever
    // device a slot's record names, so a stale one would be inherited by the next task in this slot.
    let _ = crate::task::scheduler::take_task_hw_bdf(task_slot);
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
        // Same report as the kill path (see `scheduler.rs`): a half-spawned instance must never take
        // a LIVE instance's name with it. Rarer here - a partial spawn is a failure already - which is
        // exactly why it would otherwise go unnoticed.
        if crate::ipc::names::unregister_endpoint(name, ep_id)
            == crate::ipc::names::UnregisterOutcome::Cleared
        {
            if let Some(other) = crate::task::scheduler::find_task_by_name_excluding(name, task_slot) {
                crate::kprintln!(
                    "ipc::names: '{}' UNREGISTERED by a FAILED spawn's endpoint {:?} while slot {} is STILL ALIVE under that name - the live instance is now unreachable by name",
                    name, ep_id, other);
            }
        }
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
    // task owns its bytes now (`scheduler::TASK_NAMES`), so nothing here needs the literal.
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
    // `Some(class)` = the SPAWNER named the device this service drives. `None` = resolve it by name
    // from `service_hw`, the catalogue path, which goes away with the catalogue.
    hw_override:       Option<HwClass>,
    // Phase 0b (docs/naming-design.md): if `Some`, wire the child's send-peers from these
    // caller-supplied `(label, cap)` entries instead of resolving `send_peers` against the kernel
    // name table. The kernel installs each cap and records `label → slot` in the child's send-peer
    // metadata, so the child's `ctx.capability(label)` resolves exactly as it does on the old path.
    // `None` = the old name-resolution path (unchanged).
    installs:          Option<&[InstallCap]>,
    // Report this task's death to the supervisor and count it as a restart (`SPAWN_FLAG_WATCHED`).
    watched:           bool,
) -> Result<Option<EndpointId>, SpawnError> {
    // The declared hardware class + mint authority for this service (audit M7 / T1 Phase B). Every
    // MMIO / DMA / IOMMU / bus-master / RESOURCE_MINT grant below is driven off these, not a `name ==`
    // check: the spawn request's `hw_override` / `priv_override` where there is one (every spawn but
    // the supervisor's), else `service_hw`, which now answers none for every name.
    let (hw_by_name, resource_mint_by_name) = service_hw(name);
    let hw = hw_override.unwrap_or(hw_by_name);
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

    // Spawn authority - least privilege (§3.1; the H10 audit). Granted only to the services that
    // actually start other services: the supervisor (spawns services + probes), the shell (brokers
    // spawn/kill/restart), and the test-driver probes (property/stress/perf/chaos modes spawn victims;
    // their SPAWN bit is in their spawn request, `probes::privileges_of`). events, the drivers,
    // ping, pong, and observe never spawn and no longer hold the authority to.
    // Previously every service got this unconditionally ("spawn authority, every
    // service in v1") - a system-wide blast-radius widening this closes. Capture the
    // slot (u32::MAX when not granted); the SDK already treats MAX as "not held".
    // The non-hardware authorities come from the spawn request's privilege word, or for the
    // supervisor's catalogue spawn from `service_privileges` (audit U15) - never a re-derived
    // `name ==` check per grant.
    //
    // `is_probe` is GONE with the probe image. It compared the spawning ELF against the kernel's own
    // `PROBE_ELF` rodata to give the whole test-probe family its privileges by identity rather than
    // by name - which only worked while the kernel HELD that image. The probes' privileges now
    // travel in the spawn request like every other service's (`probes::privileges_of`), checked
    // against what the supervisor may delegate, which is what the `prop-`/`stress-` prefix hole
    // recorded below always needed.
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
            gpio:            bits & privbits::GPIO            != 0,
            set_clock_floor: bits & privbits::SET_CLOCK_FLOOR != 0,
            set_clock:       bits & privbits::SET_CLOCK       != 0,
            net_device:      bits & privbits::NET_DEVICE      != 0,
            pci_cfg:         bits & privbits::PCI_CFG         != 0,
            cpu_clock:       bits & privbits::CPU_CLOCK       != 0,
            // NO BIT, and none is coming. A spawner cannot pass on the authority to spawn arbitrary
            // images: that is exactly the widening this capability exists to close, and a wire bit
            // for it would re-open the hole one grant later.
            image_spawn:     false,
            // USB_DISK has NO bit, and that is the finding rather than an omission. It gated
            // `block-driver`'s reach to a USB stick through the in-kernel Bulk-Only stack - and on
            // BOTH ARM ports that stack is gone: the driver now asks the `dwc2` / `xhci` SERVICE over
            // IPC (`xhciblk`), which calls no `usb_disk_*` syscall at all. So the authority is
            // vestigial, and moving `block-driver` to the supervisor dropped it rather than
            // preserving it. That is the narrowing audit SEC-37 asked for, reached as a consequence
            // of the move: whole-device read/write reach is no longer handed to a service that
            // stopped using it. Deleting the syscalls themselves is a separate change with its own
            // test, so the resource still exists and simply has no holder.
            usb_disk:        false,
        },
        None => service_privileges(name),
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
        // Who may take a mailbox back past the reserve: what the supervisor manages (WATCHED), and the
        // supervisor itself, which the kernel respawns and so never marks watched - "watched" means
        // "tell the supervisor of this death", which for the supervisor would be telling the dead.
        // It is the one name the kernel knows, as the restart counter in the death path also uses.
        let may_take_back = watched || name == "supervisor";
        let reply_routed =
            match crate::ipc::routing::try_register_optional(reply_ep_id, core_id, reply_gen, may_take_back) {
                Ok(crate::ipc::routing::OptionalGrant::Free) => true,
                Ok(crate::ipc::routing::OptionalGrant::Credit { free, total, reserve }) => {
                    // Said, as the refusal is: a grant past the reserve is the exception this makes.
                    crate::kprintln!(
                        "spawn[ipc]: '{}' takes back a reply mailbox released by a service that died - {} of {} routing slots free, reserve {}",
                        name, free, total, reserve);
                    true
                }
                Err(r) => {
                    // EVERY refusal, NAMED: who runs without a reply mailbox is the fact `backlog/74`
                    // turns on, and the routing table cannot say it because it holds ids, not tasks.
                    crate::kprintln!(
                        "spawn[ipc]: '{}' gets no reply mailbox - {} of {} routing slots free, reserve {} ({} refused since boot); it awaits replies on its own endpoint",
                        name, r.free, r.total, r.reserve, r.count);
                    false
                }
            };
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

    // CONSOLE_PUSH: inject decoded keystrokes into the console input ring (§12). WHO holds it is the
    // spawn request's privilege word (the USB host drivers' rows in the supervisor); here we only mint it.
    let mut console_push_slot_u32 = u32::MAX;
    if privs.console_push {
        let cp_cap = mint_cap(CONSOLE_PUSH_RESOURCE, Rights::WRITE);
        let cap_slot = caps.insert(cp_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        console_push_slot_u32 = cap_slot as u32;
    }

    // INTROSPECT: read another task's / system-wide kernel state via TaskStat + InspectKernel (§3.1;
    // docs/introspection-capability.md). Self-state (own alloc bytes) and the TSC stay ungated, so a
    // service that was not given the privilege needs nothing. No slot is stored - the gate scans holdings.
    if privs.introspect {
        let in_cap = mint_cap(INTROSPECT_RESOURCE, Rights::READ);
        caps.insert(in_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // SERVICE_CONTROL: kill/restart other services (§3.1/§14.4; docs/service-control-cap.md). WHO holds
    // it is `privs`, resolved above; here we only mint it.
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

    // REBOOT (§3.1): hardware-reset the machine (`Reboot`/18). WHO holds it is `privs`, resolved above
    // (the shell's spawn request); here we only mint it. No other service can hardware-reset the machine.
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

    // ACQUIRE_ANY (§3.1): reach ARBITRARY services by name via `AcquireSendCap`. WHO holds it is the
    // spawn request's privilege word (or `service_privileges` for the supervisor); here we only mint it.
    // Ordinary services get NONE - their AcquireSendCap is restricted to the send-peers they were
    // spawned with (recovery), so they hold no ambient send authority.
    if privs.acquire_any {
        let aa_cap = mint_cap(ACQUIRE_ANY_RESOURCE, Rights::WRITE);
        caps.insert(aa_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // NET_DEVICE: move ethernet frames via an in-kernel net device (NetFrame*/NetInfo, syscalls 42-44;
    // stubs on every port now, and nothing requests it). WHO holds it is `privs`; here we only mint it.
    if privs.net_device {
        let nd_cap = mint_cap(NET_DEVICE_RESOURCE, Rights::WRITE);
        caps.insert(nd_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // PCI_CFG: `hw-enumerator` reads PCI configuration space (step D2). ONE syscall performs a
    // complete read - latch the selector, fetch the register - and there is no write of any kind.
    //
    // The kernel learns two opaque numbers, a selector and an offset. It does not learn what they
    // name, which is the whole point: bus walking and class codes are hardware semantics that belong
    // in the service (§26.10). What the kernel DOES enforce is admissibility - a well-formed cycle,
    // and on a board whose bridge aborts reads past its subordinate bus, only a bus it forwards.
    // That is not interpretation, it is the kernel refusing to perform an access that could halt the
    // machine on an argument a service chose.
    //
    // READ-ONLY, permanently. Config space holds every BAR and command register, and the target is
    // chosen by data rather than by the interface, so write authority here is write authority over
    // every device on the bus and there is no narrower form of it to mint.
    if privs.pci_cfg {
        // READ alone. There is no longer any write operation behind this capability, so granting
        // WRITE would be authority nobody can exercise - and a right that is minted but unused is
        // the kind of thing a later change quietly finds a use for (§7.3, grant the least).
        let pc_cap = mint_cap(PCI_CFG_RESOURCE, Rights::READ);
        caps.insert(pc_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // CPU_CLOCK: `power` sets the Arm cores to their minimum or maximum rate (`CpuClock`, syscall 55).
    // WHO holds it is the spawn request's privilege word; here it is only minted. WRITE alone - there
    // is no read of the authority to grant, since the syscall reports the rate it set either way.
    if privs.cpu_clock {
        let cc_cap = mint_cap(CPU_CLOCK_RESOURCE, Rights::WRITE);
        caps.insert(cc_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        crate::kprintln!("spawn[clock]: '{}' may set the Arm clock (CPU_CLOCK)", name);
    }

    // DEVICE_POWER: derived from the DEVICE GRANT, not from a privilege bit the spawner passes. A service
    // this spawn hands a fixed peripheral window - and whose device the arch layer can power - also
    // receives the authority to cut and restore that device's power (`DevicePower`, syscall 54). The
    // grant, renewable: the kernel powered the domain at boot to make the window mean anything, and a
    // chip that only returns to power-on when its power is cut needs the holder able to ask for that
    // again (`docs/wifi.md` 45-46). One holder per window, so one holder per device; the arch layer
    // answers which devices it can power: today the radio behind the SDIO host on the Pi 4 and on the
    // VisionFive 2 Lite.
    if hw.fixed_kind().is_some_and(|k| crate::arch::imp::device_power_control(k)) {
        let dp_cap = mint_cap(DEVICE_POWER_RESOURCE, Rights::WRITE);
        caps.insert(dp_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
        crate::kprintln!("spawn[power]: '{}' may cut and restore its device's power (DEVICE_POWER)", name);
    }

    // USB_DISK: read/write an in-kernel USB stick (UsbDisk*, syscalls 46-49). There is no in-kernel USB
    // stack on any port now and `service_privileges` grants it to nobody, so this never mints.
    if privs.usb_disk {
        let ud_cap = mint_cap(USB_DISK_RESOURCE, Rights::WRITE);
        caps.insert(ud_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // GPIO_DEVICE: the shell's `gpio` command drives the SoC pins (ARM `Gpio` syscall). Minted here; WHO
    // holds it is `privs` (the shell's spawn request).
    if privs.gpio {
        let g_cap = mint_cap(GPIO_DEVICE_RESOURCE, Rights::WRITE);
        caps.insert(g_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // IMAGE_SPAWN: the supervisor starts services from images IT holds (`SpawnImage`, syscall 52).
    // Minted here; WHO holds it is `service_privileges` (no privilege bit carries it) - and it is
    // exactly one principal.
    if privs.image_spawn {
        let is_cap = mint_cap(IMAGE_SPAWN_RESOURCE, Rights::WRITE);
        caps.insert(is_cap)
            .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::CapTableFull })?;
    }

    // SET_CLOCK: once net-stack's, to set the wall clock from SNTP. Minted here if `privs` asks for it;
    // the supervisor no longer requests it for anyone.
    //
    // MINTED AND CHECKED BY NOBODY, on every arch. This said the authority is spent through a
    // `SetClock` syscall, "inert (no-op syscall) off ARM" - implying it is live ON arm. There is no
    // such syscall on any arch: clock slice 3 moved the wall clock to the `time` SERVICE, which
    // `net-stack` asks over IPC. The grant is real and nothing consumes it (`backlog/59`).
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
            "task: '{}' declares {} send peers, limit {} - the extras are NOT wired (raise MAX_SEND_PEERS in task/mod.rs AND sdk/service_context.rs, and SERVICE_CONTEXT_DATA_SIZE in both)",
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
            // The DECLARATION is still recorded below, so this is a delay rather than a life
            // sentence. It did not used to be: the name was dropped here along with the cap, and
            // `AcquireSendCap` - which authorises a reacquire only for a declared name - then refused
            // this service its peer forever. Saying "will reacquire" is the difference between a
            // line that reports a transient wiring gap and one that reported a permanent break
            // without knowing it.
            crate::kprintln!(
                "task: peer '{}' not yet registered, no SEND cap for '{}' (declared - will reacquire)",
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
        // TAKE THE CONTROLLER FROM THE FIRMWARE BEFORE HANDING IT TO A DRIVER.
        //
        // On a machine with USB legacy support the BIOS is still RUNNING the EHCI when we get here -
        // measured on the T630: `USBCMD.RS=1`, periodic schedule enabled and active, polling the
        // keyboard out of firmware memory. A driver that then writes HCRESET is resetting a
        // controller its owner is using, and the firmware takes an SMI over it.
        //
        // An SMI is serviced on the CORE THAT RAISED IT. With several cores the OS keeps running on
        // the others while the firmware copes, and the damage is invisible; with ONE core there is
        // nowhere else to run, and the platform goes down - a silent reset, no panic, mid-log. That
        // is the single-core T630 reboot loop, and it is the same latent defect on every machine.
        //
        // `ehci_bios_handoff` is the standard procedure (EHCI 2.1.7): claim the OS-Owned semaphore
        // in USBLEGSUP, wait for the firmware to release BIOS-Owned, and disable its SMIs on this
        // controller. It has existed and been correct for some time, marked `#[allow(dead_code)]`
        // and never called, because EHCI was deliberately left co-owned in IOMMU passthrough - the
        // arrangement the back-port keyboard worked in. Co-ownership is exactly what cannot survive
        // one core, and it is not needed: the `ehci` service drives that keyboard itself, HID
        // decode, key repeat and all.
        //
        // Idempotent, bounded, and it reports whether the firmware actually let go. Done at the
        // GRANT rather than once at boot so a restarted driver - which chaos does constantly - also
        // gets a controller nobody else is running.
        // No cfg: whether there is firmware to take the controller FROM is a fact about this
        // machine's firmware, and the arch states it (an empty body where there is no BIOS).
        if hw == HwClass::Ehci && bar != 0 {
            crate::arch::imp::pci::ehci_bios_handoff();
        }
        if bar != 0 {
            let mmio_flags = PageFlags::PRESENT
                | PageFlags::WRITABLE
                | PageFlags::USER
                | PageFlags::NO_EXEC
                | PageFlags::PCD
                | PageFlags::PWT;
            // THE WINDOW IS THE BAR, sized by the arch (backlog/80 K1). It was a fixed 64 KiB whatever
            // the device had, and on the T630 the audio controller's 64 KiB reached the HDMI audio,
            // `xhci`, EHCI and AHCI registers - authority the grant never named (CLAUDE.md 3.1). The
            // pages mapped are whole; the length the driver's `Mmio` is given is the BAR's own, and
            // `Mmio` checks every access against it, so a sub-page BAR is not widened to the page. A port
            // that cannot measure a BAR answers 0 and keeps the fixed window, said in the line below.
            let measured = hw.bar_device()
                .and_then(|d| d.bar.iter().position(|&b| b == bar).map(|ix| crate::arch::imp::pci::bar_len(&d, ix)))
                .unwrap_or(0);
            let len = match measured {
                0 => XHCI_MMIO_PAGES * PAGE_SIZE as u64,
                n => n.min(MMIO_WINDOW_MAX),
            };
            for i in 0..len.div_ceil(PAGE_SIZE as u64) {
                let off = i * PAGE_SIZE as u64;
                page_table
                    .map(VirtAddr(XHCI_MMIO_VA + off), PhysAddr(bar + off), mmio_flags)
                    .map_err(|_| { cleanup_partial_spawn(task_slot, name, own_endpoint); SpawnError::MapFailed })?;
            }
            match measured {
                0 => crate::kprintln!(
                    "spawn[mmio]: '{}' BAR {:#x} -> VA {:#x}, {} bytes - the BAR's size is not measured on this port, so the fixed window",
                    name, bar, XHCI_MMIO_VA, len),
                n if n > len => crate::kprintln!(
                    "spawn[mmio]: '{}' BAR {:#x} -> VA {:#x}, the first {} of its {} bytes - the most one grant maps",
                    name, bar, XHCI_MMIO_VA, len, n),
                _ => crate::kprintln!(
                    "spawn[mmio]: '{}' BAR {:#x} -> VA {:#x}, {} bytes, the BAR's own size", name, bar, XHCI_MMIO_VA, len),
            }
            (XHCI_MMIO_VA, len)
        } else if hw == HwClass::Framebuffer {
            // The display's framebuffer, for the task that asked for the FRAMEBUFFER kind - the `console`
            // service in practice (docs/console-service.md 9).
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
                        | PageFlags::WRITE_COMBINE
                        // WHAT THIS ARCH'S PAGE TABLES NEED to express that intent. The long note
                        // above is a fact about silicon - arm32 and x86 read PCD and PWT in OPPOSITE
                        // senses - and it lived here as `#[cfg(not(target_arch = "x86_64"))] flags |=
                        // PageFlags::PWT`, which left a fifth port inheriting arm32's answer by
                        // default. The arch says which bits it wants; this file says what it wants
                        // them to MEAN (26.14).
                        | crate::arch::imp::fb_extra_page_flags();
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
        } else if let Some((va, len)) = hw.fixed_kind()
            .and_then(|k| crate::arch::imp::map_fixed_device(&mut page_table, k))
        {
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
                        // THIS driver's device, not "the xHCI". This read `pci::XHCI_BDF`, so it
                        // confined the xHCI controller whenever ANY driver asked to be confined -
                        // harmless only while `xhci` was the sole one that did (`audio-driver` asks
                        // too, since 2026-10-03), and the same
                        // by-class assumption the kill path carried until D3b. `hw.bdf()` is the
                        // device this spawn actually resolved.
                        crate::arch::imp::iommu::confine_device(hw.bdf(), phys, len);
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
                    // REMEMBERED, so the kill path can quiesce this exact controller without the
                    // kernel keeping a name->device table (see `scheduler::TASK_HW_BDF`).
                    scheduler::set_task_hw_bdf(task_slot, bdf);
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
            // The granted interrupt vector(s). Written here beside the MMIO and DMA grants because it
            // is the same kind of fact: something the kernel CHOSE for this driver, which the driver
            // must be able to read and may not name. Truncated to the field, loudly - a driver told
            // about fewer vectors than it was routed would misclassify the rest.
            data.irq_count = core::cmp::min(hw_irqs.len(), data.irqs.len()) as u32;
            if hw_irqs.len() > data.irqs.len() {
                crate::kprintln!(
                    "task: '{}' routed {} IRQs but the context carries {} - the rest are ROUTED but not reported",
                    name, hw_irqs.len(), data.irqs.len());
            }
            for (i, &v) in hw_irqs.iter().take(data.irqs.len()).enumerate() {
                data.irqs[i] = v;
            }
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
                // AND EVERY DECLARED PEER THAT COULD NOT BE WIRED. `peer_data` holds only the peers
                // whose names RESOLVED at this instant, so recording just those conflated two
                // different things: what this service is allowed to talk to (a contract fact, fixed
                // for its lifetime) and who happened to be running when it spawned (an accident of
                // scheduling). A peer that was down for the 47 ms of this spawn was therefore never
                // recorded as declared - and since `AcquireSendCap` authorises a reacquire only for a
                // DECLARED name, that service could never reach that peer again. Not "until the peer
                // came back": never, for the life of the instance.
                //
                // Measured on the T630: `fs` spawned 47 ms before `block-driver` during a chaos storm,
                // came up with no declaration for it, and answered every file operation with "storage
                // unavailable" for the rest of the boot while `block-driver` sat idle beside it having
                // received one message. 113 failures, one dropped string.
                //
                // 14.3 is explicit that a client reacquires a restarted peer; denying the declaration
                // denies the recovery the constitution promises. The CAP is legitimately absent here -
                // there was nothing to mint one from - but the DECLARATION is not the kernel's to drop.
                for &pn in send_peers {
                    if nn >= MAX_SEND_PEERS { break; }
                    if pn.is_empty() || names[..nn].iter().any(|&n| n == pn) { continue; }
                    names[nn] = pn; nn += 1;
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
    // What this task IS to the death path, recorded on EVERY spawn so a reused slot never inherits a
    // previous holder's: whether its death is reported and counted, and the device kind it was granted.
    // They replace two lists of service names and four name checks (`docs/audio.md`).
    scheduler::set_task_watched(task_slot, watched);
    scheduler::set_task_hw_kind(task_slot, hw.kind_code());
    unsafe {
        scheduler::commit_task(task_slot, name, ctx, true, kstack_top as u64, own_endpoint);
    }

    crate::kprintln!("task: '{}' spawned OK on core {} (slot {})", name, core_id, task_slot);
    Ok(own_endpoint)
}

/// Spawn the supervisor on core 0. Called once per boot, from every port's boot path (x86-64
/// `kernel_main`, and each other port's `sched_supervisor` or boot entry; §11.1).
/// The kernel's ONE direct spawn (Path C / Phase 5 - `init` is removed). The kernel boots the
/// SUPERVISOR directly; the supervisor then spawns events and all services. Uses `SUPERVISOR_ELF`
/// (garbage under `test-bad-supervisor` → §22 Test 1B). `has_recv_endpoint = true` (the supervisor
/// owns the death-notification endpoint). A *boot-time* spawn failure is fatal (§6.2, §11.3); a later
/// *runtime* death is recovered by the kernel respawning it (Phase 6 - see below).
// C1-1: `arm_spawn_events_neutral` and `arm_spawn_shell_neutral` USED TO LIVE HERE, and with them the
// `arm-sched-spawn` / `arm-shell` / `arm-spawn-events` / `pi4-sched-spawn` bring-up builds in which the
// kernel started a service directly. They were scaffolding from before the supervisor path worked, and
// they were gated on ARCHITECTURE rather than on the features that called them, so every ARM and
// AArch64 kernel carried the ability whether or not anything reached it.
//
// The kernel spawns the supervisor and NOTHING ELSE. Bringing up a new ISA now means getting the
// supervisor up, which is the thing that has to work anyway. If that ever proves too large a first
// step, the answer is a smaller supervisor - not a second spawn path in the kernel.
pub fn spawn_supervisor() {
    match spawn_service_with_image("supervisor", crate::loader::ImageSource::Kernel(SUPERVISOR_ELF), 0, true, &[], 0, false, 64 * 1024 * 1024, &[], false, None, None, None, false) {
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
        64 * 1024 * 1024, &[], false, None, None, None, false,
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
