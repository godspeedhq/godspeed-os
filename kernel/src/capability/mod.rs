// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

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
    crate::kprintln!("capability: subsystem ready");
}
