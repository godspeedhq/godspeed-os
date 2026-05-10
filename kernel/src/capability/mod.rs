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
pub use table::{CapTable, mint_cap, register_resource, mark_dead_resource, revoke_resource};

// ---------------------------------------------------------------------------
// Well-known kernel resource IDs.
// ---------------------------------------------------------------------------

/// The kernel log (ring buffer + serial). A task must hold this resource with
/// `Rights::WRITE` to call `SyscallNumber::Log` (syscall 5).
pub const LOG_WRITE_RESOURCE: ResourceId = ResourceId(1);

/// The spawn authority. A task must hold this resource with `Rights::WRITE`
/// to call `SyscallNumber::Spawn` (syscall 7).
pub const SPAWN_RESOURCE: ResourceId = ResourceId(2);

pub fn init() {
    table::init_global();
    // Register stable kernel resources (generation 0 forever — §7.5).
    table::register_resource(LOG_WRITE_RESOURCE);
    table::register_resource(SPAWN_RESOURCE);
    crate::kprintln!("capability: subsystem ready");
}
