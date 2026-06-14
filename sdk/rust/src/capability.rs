// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Typed capability handles for service code — §7.
//!
//! Services never see raw `Capability` structs; the kernel manages the cap table.
//! Services hold opaque `CapHandle` values (slot indices) and pass them to
//! syscall wrappers. The kernel resolves them against the task's cap table.

/// Opaque handle to a capability slot in the calling task's cap table.
/// Not a raw pointer; the kernel does the resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapHandle(pub u32);

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
