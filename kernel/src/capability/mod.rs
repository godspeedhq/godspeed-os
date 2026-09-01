// SPDX-License-Identifier: GPL-2.0-only
//! Capability system - §7.
//!
//! Public API for the rest of the kernel. All cap operations go through here;
//! the internal table and generation logic are private to this module.

pub mod cap;
pub mod delegated;
pub mod generation;
pub mod revoke;
pub mod rights;
pub mod table;

pub use cap::{Capability, CapError, ResourceId};
pub use generation::next_generation;
pub use rights::Rights;
pub use table::{CapTable, mint_cap, register_resource, register_resource_at_gen,
                get_resource_generation, mark_dead_resource, revoke_resource, cap_read_rights};

// ---------------------------------------------------------------------------
// Well-known kernel resource IDs.
// ---------------------------------------------------------------------------

/// The kernel log (ring buffer + serial). A task must hold this resource with
/// `Rights::WRITE` to call `SyscallNumber::Log` (syscall 5).
pub const LOG_WRITE_RESOURCE: ResourceId = ResourceId(1);

/// The spawn authority. A task must hold this resource with `Rights::WRITE`
/// to call `SyscallNumber::Spawn` (syscall 7).
pub const SPAWN_RESOURCE: ResourceId = ResourceId(2);

/// The console read authority. A task must hold this resource with `Rights::READ`
/// to call `SyscallNumber::ConsoleRead` (syscall 17).
pub const CONSOLE_READ_RESOURCE: ResourceId = ResourceId(3);

/// The console push authority - inject a byte into the console input ring (§12).
/// Held only by an input-driver service (the USB keyboard driver) with
/// `Rights::WRITE` to call `SyscallNumber::ConsolePush` (syscall 20). Gating
/// this prevents an arbitrary service from forging keystrokes into the shell.
pub const CONSOLE_PUSH_RESOURCE: ResourceId = ResourceId(4);

/// The introspection authority - read another task's or system-wide kernel state
/// via `InspectKernel` (syscall 13, the system-state queries) and `TaskStat`
/// (syscall 16). A task must hold this resource with `Rights::READ`. Self-state
/// queries (own alloc bytes) and the TSC clock remain ungated. Gating prevents an
/// arbitrary service from enumerating every task's name / memory / restart count
/// (§3.1). See `docs/introspection-capability.md`.
pub const INTROSPECT_RESOURCE: ResourceId = ResourceId(5);

/// The service-control authority - kill a service via `Kill` (syscall 8), and so
/// the kill half of restart. A task must hold this resource with `Rights::WRITE`.
/// Held by the shell (the interactive broker) and the test-driver probes (they
/// kill victim services to exercise the kill/revocation machinery), plus the
/// supervisor (§14.4). Gating closes the §3.1 ambient-authority hole: without it,
/// any service could kill any non-trusted-root service. See
/// `docs/service-control-cap.md`.
pub const SERVICE_CONTROL_RESOURCE: ResourceId = ResourceId(6);

/// The resource-mint authority - allocate a **delegated resource** and mint a cap for it
/// via `ResourceMint` (syscall 30, §7.10, P2 file-as-capability). A task must hold this
/// resource with `Rights::WRITE`. Granted only to services that legitimately issue
/// resources whose meaning they define - `fs` (files) in v1 - so delegated minting is
/// explicit authority, never ambient (§3.1). See `docs/persistence.md` §7.4.
pub const RESOURCE_MINT_RESOURCE: ResourceId = ResourceId(7);

/// The reboot authority - hardware-reset the machine via `Reboot` (syscall 18). A reset is a
/// denial-of-service, so it is a privileged action (§3.1): a task must hold this resource with
/// `Rights::WRITE`. Granted only to the legitimate rebooters - the `shell` (its `reboot` command) and
/// the USB drivers `xhci`/`ehci` (the Ctrl+Alt+Del secure-attention reboot) - so no other service can
/// reset the box. Validated by holdings (like `kill`/8 and the introspection reads), since `Reboot`
/// takes no arguments and leaves no slot to pass.
pub const REBOOT_RESOURCE: ResourceId = ResourceId(8);

/// The broad-acquire authority - mint a SEND cap to ANY registered service by name (`AcquireSendCap`,
/// syscall 10), bypassing the default restriction to the caller's contract-declared send-peers. A task
/// must hold this resource with `Rights::WRITE`. Granted only to the operator/test instruments that
/// legitimately reach arbitrary services - the `shell` (chaos flooding, pipe sinks), the `supervisor`
/// (reconcile by name), and test probes. Without it, `AcquireSendCap` is limited to declared peers
/// (recovery, §13/§14.2), so an ordinary service holds no ambient send authority (§3.1).
pub const ACQUIRE_ANY_RESOURCE: ResourceId = ResourceId(9);

/// Authority to move raw ethernet frames to/from the in-kernel USB-net device (the ARM DWC2 CDC-ECM
/// bridge: `NetFrameTx`/`NetFrameRx`/`NetInfo`, syscalls 42-44). Held only by the ARM `nic-driver`, which
/// bridges those frames to the frame IPC net-stack speaks. On non-ARM arches the NIC is a userspace PCIe
/// driver and these syscalls return unsupported, so nothing holds this there. A frame is raw wire bytes,
/// so - like a DMA-capable driver (§6.4) - this is real reach; it is granted explicitly, never ambient.
pub const NET_DEVICE_RESOURCE: ResourceId = ResourceId(10);

/// Authority to drive the SoC GPIO pins (the ARM `Gpio` syscall: set a pin's direction, drive it high/low,
/// read its level). Real hardware reach - GPIO pins carry the UART console and the SD card, so toggling the
/// wrong one breaks the machine; granted only to the `shell` (its `gpio` command, the operator interface).
/// A no-op off ARM. Like REBOOT/NET_DEVICE, it is explicit authority, never ambient.
pub const GPIO_DEVICE_RESOURCE: ResourceId = ResourceId(11);

/// Authority to read and write blocks on the in-kernel USB mass-storage device (the ARM DWC2 Bulk-Only
/// bridge: `UsbDiskInfo`/`UsbDiskRead`/`UsbDiskWrite`, syscalls 46-48). Held only by the ARM
/// `block-driver`, which serves those blocks to `fs` over the same block IPC protocol as any disk. This
/// is whole-device read/write reach - a holder can rewrite any sector, so it is granted explicitly,
/// never ambient (the same posture as NET_DEVICE). A no-op off ARM, where disks are userspace drivers.
pub const USB_DISK_RESOURCE: ResourceId = ResourceId(12);

/// Authority to set the wall clock via `SetClock` (the SNTP-fed time-of-day). The RTC-less ARM port has
/// no hardware clock, so `date` reads zero until a network time source sets it; setting it changes every
/// task's view of the time of day, so it is a privileged action (§3.1), not ambient. Granted only to
/// `net-stack`, which runs the SNTP round-trip. A no-op on arches with a real RTC (x86). Validated by
/// holdings (like `reboot`/8), since `SetClock` spends its one argument register on the epoch.
pub const SET_CLOCK_RESOURCE: ResourceId = ResourceId(13);

/// Inject a test interrupt (`FireIrq`, syscall 51). Held ONLY by the control service.
///
/// This exists so `control.rs` can leave the kernel (C1-6). `KILL` and `RESTART` already map onto
/// SERVICE_CONTROL + SPAWN, but injecting an interrupt had no capability at all, so the module could
/// not fully move and the finding could not close. Naming the authority is the honest way to hand it
/// out: a test hook that pokes the interrupt controller IS real authority, and pretending otherwise by
/// leaving it un-named in ring 0 was the weaker position.
///
/// The trade is deliberate and worth stating: the syscall and authority pins GROW by one each so that a
/// 123-line module of developer tooling can leave the kernel entirely. The pins count SURFACES, and a
/// gated syscall is a smaller, more visible surface than an ungated command interpreter.
pub const FIRE_IRQ_RESOURCE: ResourceId = ResourceId(14);

/// Start a task from a CALLER-SUPPLIED IMAGE (`SpawnImage`, syscall 52). Held ONLY by the supervisor.
///
/// Distinct from `SPAWN`, and that distinction is the whole point. `SPAWN` means "start a service the
/// system already knows"; this means "start ARBITRARY BYTES, under a name you choose". They were the
/// same capability until now, so every SPAWN holder - the shell, `chaos`, `control`, every probe -
/// could introduce new code under a real service's name in the window while that service is dead, and
/// clients reacquiring by name (§14.3) would wire themselves to it.
///
/// That widening arrived with step C (`docs/service-ownership.md`): before it, the kernel held every
/// image and a spawner could only ask for one the kernel already had. Once images moved to the
/// supervisor, `SpawnImage` had to accept bytes from userspace - and it was gated by the capability
/// the shell happens to hold for entirely unrelated reasons.
///
/// NOT DELEGATABLE, deliberately: it is absent from `SUPERVISOR_DELEGATABLE`, so the supervisor cannot
/// pass it on even to a service it trusts. A delegatable version would re-open the hole one grant
/// later, and nothing else has a reason for it - the only two callers in the tree are the supervisor's
/// own `spawn_by_image` and `spawn_probe_row`. Everything else asks the supervisor over IPC.
///
/// This does not make a compromised supervisor safe - it is the trusted root and always could do this.
/// It stops the authority leaking to the twelve services that merely wanted to start something.
pub const IMAGE_SPAWN_RESOURCE: ResourceId = ResourceId(15);

pub fn init() {
    table::init_global();
    // Register stable kernel resources (generation 0 forever - §7.5).
    table::register_resource(LOG_WRITE_RESOURCE);
    table::register_resource(FIRE_IRQ_RESOURCE);
    table::register_resource(SPAWN_RESOURCE);
    table::register_resource(CONSOLE_READ_RESOURCE);
    table::register_resource(CONSOLE_PUSH_RESOURCE);
    table::register_resource(INTROSPECT_RESOURCE);
    table::register_resource(SERVICE_CONTROL_RESOURCE);
    table::register_resource(RESOURCE_MINT_RESOURCE);
    table::register_resource(REBOOT_RESOURCE);
    table::register_resource(ACQUIRE_ANY_RESOURCE);
    table::register_resource(NET_DEVICE_RESOURCE);
    table::register_resource(GPIO_DEVICE_RESOURCE);
    table::register_resource(USB_DISK_RESOURCE);
    table::register_resource(SET_CLOCK_RESOURCE);
    table::register_resource(IMAGE_SPAWN_RESOURCE);
    crate::kprintln!("capability: subsystem ready");
}
