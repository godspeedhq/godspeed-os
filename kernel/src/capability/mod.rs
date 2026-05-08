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
pub use table::CapTable;

pub fn init() {
    table::init_global();
    crate::kprintln!("capability: subsystem ready");
}
