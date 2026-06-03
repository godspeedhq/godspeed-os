//! Capability system — §7.
//!
//! Public API for the rest of the kernel. All cap operations go through here;
//! the internal table and generation logic are private to this module.

pub mod cap;
pub mod generation;
pub mod revoke;
pub mod rights;
pub mod table;

pub use cap::{Capability, CapError, ResourceId};
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

/// The console push authority — inject a byte into the console input ring (§12).
/// Held only by an input-driver service (the USB keyboard driver) with
/// `Rights::WRITE` to call `SyscallNumber::ConsolePush` (syscall 20). Gating
/// this prevents an arbitrary service from forging keystrokes into the shell.
pub const CONSOLE_PUSH_RESOURCE: ResourceId = ResourceId(4);

pub fn init() {
    table::init_global();
    // Register stable kernel resources (generation 0 forever — §7.5).
    table::register_resource(LOG_WRITE_RESOURCE);
    table::register_resource(SPAWN_RESOURCE);
    table::register_resource(CONSOLE_READ_RESOURCE);
    table::register_resource(CONSOLE_PUSH_RESOURCE);
    crate::kprintln!("capability: subsystem ready");
}
