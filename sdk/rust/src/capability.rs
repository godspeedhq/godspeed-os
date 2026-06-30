// SPDX-License-Identifier: Apache-2.0
//! Typed capability handles for service code - §7.
//!
//! Services never see raw `Capability` structs; the kernel manages the cap table.
//! Services hold opaque `CapHandle` values (slot indices) and pass them to
//! syscall wrappers. The kernel resolves them against the task's cap table.

/// Opaque handle to a capability slot in the calling task's cap table.
/// Not a raw pointer; the kernel does the resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapHandle(pub u32);

/// Capability right bits, mirroring the kernel `Rights` bitfield (§7.4). Used with the
/// delegated-resource calls (`resource_mint`/`resource_invoke`, §7.10): a file cap minted
/// `RIGHT_READ | RIGHT_WRITE | RIGHT_GRANT`, a read invoked with `RIGHT_READ`, a write with
/// `RIGHT_WRITE` (a cap lacking the invoked right fails - the non-escalation check).
pub const RIGHT_READ: u8 = 1 << 0;
pub const RIGHT_WRITE: u8 = 1 << 1;
pub const RIGHT_GRANT: u8 = 1 << 4;

/// Errors mirrored from the kernel's CapError (§7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    CapNotHeld,
    CapInsufficientRights,
    CapNotGrantable,
    CapWrongScope,
    CapRevoked,
    EndpointDead,
}
