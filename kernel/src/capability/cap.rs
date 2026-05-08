//! Capability structure and validation — §7.2, §7.3, §7.5.

use super::generation::Generation;
use super::rights::Rights;

/// Unique identifier for a kernel-managed resource
/// (endpoint, memory region, MMIO range, service handle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u64);

/// An unforgeable capability token: `ResourceId + Rights + Generation` (§7.2).
///
/// Only the kernel constructs valid capabilities (§7.3 — Unforgeable).
/// User-mode cannot fabricate a `Capability`; it only receives opaque handles
/// that the kernel resolves against the per-task cap table on each syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capability {
    pub resource_id: ResourceId,
    pub rights: Rights,
    pub generation: Generation,
}

impl Capability {
    /// Validate this capability against the kernel's current resource state.
    ///
    /// Returns `Ok(())` if the cap is held, the generation matches, and the
    /// requested right is present. Otherwise returns the specific error (§7.7).
    pub fn validate(&self, required_right: Rights, current_gen: Generation) -> Result<(), CapError> {
        if !self.generation.matches(current_gen) {
            // Caller must distinguish CapRevoked vs EndpointDead based on
            // whether the resource was explicitly revoked or just died.
            return Err(CapError::GenerationMismatch);
        }
        if !self.rights.contains(required_right) {
            return Err(CapError::CapInsufficientRights);
        }
        Ok(())
    }

    /// Produce a narrowed copy of this cap for a GRANT transfer (§7.4).
    /// Panics in debug builds if `mask` would widen rights.
    pub fn narrow_for_grant(&self, mask: Rights) -> Self {
        // Every bit in mask must already be in self.rights — no widening.
        debug_assert!(
            self.rights.narrow(mask) == mask,
            "narrow_for_grant must not widen rights"
        );
        Capability {
            resource_id: self.resource_id,
            rights: self.rights.narrow(mask),
            generation: self.generation,
        }
    }
}

/// Errors returned by capability validation (§7.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// Cap not in the calling task's table.
    CapNotHeld,
    /// Cap held but lacks the required right.
    CapInsufficientRights,
    /// Cap embedded in a message without the GRANT right.
    CapNotGrantable,
    /// Cap targets a different resource than the action requires.
    CapWrongScope,
    /// Authority was explicitly revoked.
    CapRevoked,
    /// The endpoint/service this cap targeted has terminated.
    EndpointDead,
    /// Internal: generation mismatch; caller maps to CapRevoked or EndpointDead.
    GenerationMismatch,
}
