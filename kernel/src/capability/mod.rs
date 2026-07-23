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

pub fn init() {
    table::init_global();
    // Register stable kernel resources (generation 0 forever - §7.5).
    table::register_resource(LOG_WRITE_RESOURCE);
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
    crate::kprintln!("capability: subsystem ready");
}
