// SPDX-License-Identifier: GPL-2.0-only
//! Capability structure and validation - §7.2, §7.3, §7.5.

use super::generation::Generation;
use super::rights::Rights;

/// Unique identifier for a kernel-managed resource
/// (endpoint, memory region, MMIO range, service handle).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u64);

/// An unforgeable capability token: `ResourceId + Rights + Generation` (§7.2).
///
/// Only the kernel constructs valid capabilities (§7.3 - Unforgeable).
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
        // Every bit in mask must already be in self.rights - no widening.
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

    /// A copy of this cap with the GRANT right stripped (SEC-7). Never widens. Used when installing an
    /// embedded DELEGATED-resource cap (a file/socket, §7.10) into a receiver: the owning service
    /// mints it with GRANT only so it can transfer it (§8.5 rule 1 - an embedded cap must be
    /// grantable), but the recipient only *uses* it (invokes); it must not re-delegate it - the owner
    /// controls delegation by minting fresh caps.
    pub fn without_grant(&self) -> Self {
        Capability {
            resource_id: self.resource_id,
            rights: self.rights.without(Rights::GRANT),
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn make_cap(rights: Rights, gen: u32) -> Capability {
        Capability {
            resource_id: ResourceId(1),
            rights,
            generation: Generation(gen),
        }
    }

    #[test]
    fn validate_ok_with_matching_gen_and_right() {
        let cap = make_cap(Rights::SEND, 3);
        assert!(cap.validate(Rights::SEND, Generation(3)).is_ok());
    }

    #[test]
    fn validate_fails_on_generation_mismatch() {
        let cap = make_cap(Rights::SEND, 1);
        let err = cap.validate(Rights::SEND, Generation(2)).unwrap_err();
        assert_eq!(err, CapError::GenerationMismatch);
    }

    #[test]
    fn validate_fails_on_insufficient_rights() {
        let cap = make_cap(Rights::READ, 0);
        let err = cap.validate(Rights::WRITE, Generation(0)).unwrap_err();
        assert_eq!(err, CapError::CapInsufficientRights);
    }

    #[test]
    fn validate_checks_gen_before_rights() {
        // If both gen mismatch and wrong right, gen mismatch is returned first.
        let cap = make_cap(Rights::READ, 1);
        let err = cap.validate(Rights::WRITE, Generation(99)).unwrap_err();
        assert_eq!(err, CapError::GenerationMismatch);
    }

    #[test]
    fn narrow_for_grant_reduces_rights() {
        let cap = make_cap(Rights::READ | Rights::WRITE | Rights::GRANT, 0);
        let narrowed = cap.narrow_for_grant(Rights::READ);
        assert!(narrowed.rights.contains(Rights::READ));
        assert!(!narrowed.rights.contains(Rights::WRITE));
        assert!(!narrowed.rights.contains(Rights::GRANT));
    }

    #[test]
    fn narrow_for_grant_preserves_resource_and_gen() {
        let cap = make_cap(Rights::READ | Rights::WRITE, 7);
        let narrowed = cap.narrow_for_grant(Rights::READ);
        assert_eq!(narrowed.resource_id, cap.resource_id);
        assert_eq!(narrowed.generation, cap.generation);
    }

    #[test]
    fn validate_subset_right_passes() {
        // Cap has READ|WRITE; validate requires only READ - should pass.
        let cap = make_cap(Rights::READ | Rights::WRITE, 0);
        assert!(cap.validate(Rights::READ, Generation(0)).is_ok());
    }

    // --- property tests (§22 P1, P3, P9) ------------------------------------

    proptest! {
        /// Any generation mismatch returns GenerationMismatch, regardless of rights (§7.5, P9).
        #[test]
        fn gen_mismatch_always_returns_error(
            cap_gen    in any::<u32>(),
            current    in any::<u32>(),
            rights_bits in 0u8..=0b0011_1111u8,
        ) {
            prop_assume!(cap_gen != current);
            let cap = make_cap(Rights(rights_bits), cap_gen);
            let err = cap.validate(Rights(rights_bits), Generation(current)).unwrap_err();
            prop_assert_eq!(err, CapError::GenerationMismatch);
        }

        /// Matching gen + held right always succeeds (positive path, P1).
        #[test]
        fn matching_gen_and_held_right_passes(
            gen        in any::<u32>(),
            rights_bits in 1u8..=0b0011_1111u8,
        ) {
            // required = lowest set bit in rights_bits (always a strict subset)
            let required = Rights(rights_bits & rights_bits.wrapping_neg());
            let cap = make_cap(Rights(rights_bits), gen);
            prop_assert!(cap.validate(required, Generation(gen)).is_ok());
        }

        /// narrow_for_grant never widens rights - result is always a subset of original (P3).
        #[test]
        fn narrow_for_grant_never_widens(
            rights_bits in 0u8..=0b0011_1111u8,
            mask_extra  in 0u8..=0b0011_1111u8,
        ) {
            // Constrain mask to bits already held so the non-escalation debug_assert passes.
            let mask = Rights(mask_extra & rights_bits);
            let cap  = make_cap(Rights(rights_bits), 0);
            let narrowed = cap.narrow_for_grant(mask);
            prop_assert_eq!(narrowed.rights.0, mask_extra & rights_bits,
                "narrowed={:06b} original={:06b}", narrowed.rights.0, rights_bits);
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
