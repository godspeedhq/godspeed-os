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

pub fn init() {
    table::init_global();
    // Register stable kernel resources (generation 0 forever — §7.5).
    table::register_resource(LOG_WRITE_RESOURCE);
    table::register_resource(SPAWN_RESOURCE);
    table::register_resource(CONSOLE_READ_RESOURCE);
    crate::kprintln!("capability: subsystem ready");
}
