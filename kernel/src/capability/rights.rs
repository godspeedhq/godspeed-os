// SPDX-License-Identifier: GPL-2.0-only
//! Capability rights bitfield - §7.4.

/// Actions a capability may authorise on its target resource.
///
/// Rights are **non-escalating**: a `GRANT` transfer can only narrow rights,
/// never widen them. The kernel holds this by construction rather than by a check
/// at insertion (`CapTable::insert` checks nothing): every transfer path installs a
/// copy of a cap the sender already holds, or one narrowed by `without`/`narrow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Creusot models a derived `PartialEq` through this; the kernel's build never sees it (`tests/creusot`).
#[cfg_attr(creusot, derive(creusot_std::model::DeepModel))]
// `pub`, not `pub(crate)`: the kernel is one crate, so the two grant the same access, and Creusot refuses a
// `pub` constant built from a less visible field.
pub struct Rights(pub u8);

impl Rights {
    pub const READ:   Rights = Rights(1 << 0);
    pub const WRITE:  Rights = Rights(1 << 1);
    pub const SEND:   Rights = Rights(1 << 2);
    pub const RECV:   Rights = Rights(1 << 3);
    pub const GRANT:  Rights = Rights(1 << 4);
    pub const REVOKE: Rights = Rights(1 << 5);
    /// Not a right but a limit on the others: this cap carries ONE message (7.4). It is what makes a
    /// reply cap one-shot. Adding it only ever takes authority away, so it is the one bit a derive may
    /// set (`DeriveCap`), `narrow` keeps it, and `all` leaves it out.
    pub const ONCE:   Rights = Rights(1 << 6);

    pub const fn empty() -> Self { Rights(0) }
    pub const fn all()   -> Self { Rights(0b0011_1111) }

    #[cfg_attr(creusot, creusot_std::macros::bitwise_proof)]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result == (other.0 & !self.0 == 0u8)))]
    pub fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Narrow `self` to the intersection with `mask`. Never widens, and never drops `ONCE`, which a
    /// mask that did not name it would otherwise turn back into a cap of many uses.
    ///
    /// The contract is proved by Creusot over this function as it ships (`tests/creusot`), for every
    /// rights byte; the kernel's own build never sees the attributes.
    #[cfg_attr(creusot, creusot_std::macros::bitwise_proof)]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & !self.0 == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & !(mask.0 | 64u8) == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & 64u8 == self.0 & 64u8))]
    pub fn narrow(self, mask: Rights) -> Rights {
        Rights(self.0 & (mask.0 | Self::ONCE.0))
    }

    /// Remove `mask`'s bits from `self` - only ever clears bits, never widens (SEC-7). Proved by Creusot,
    /// as `narrow` is.
    #[cfg_attr(creusot, creusot_std::macros::bitwise_proof)]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & mask.0 == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & !self.0 == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(self.0 & !mask.0 & !result.0 == 0u8))]
    pub fn without(self, mask: Rights) -> Rights {
        Rights(self.0 & !mask.0)
    }

    /// Every right of either, and nothing else. Proved by Creusot, as `narrow` is.
    #[cfg_attr(creusot, creusot_std::macros::bitwise_proof)]
    #[cfg_attr(creusot, creusot_std::macros::ensures(self.0 & !result.0 == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(other.0 & !result.0 == 0u8))]
    #[cfg_attr(creusot, creusot_std::macros::ensures(result.0 & !(self.0 | other.0) == 0u8))]
    pub fn union(self, other: Rights) -> Rights {
        Rights(self.0 | other.0)
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { Rights(self.0 | rhs.0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn contains_single_right() {
        let r = Rights::SEND;
        assert!(r.contains(Rights::SEND));
        assert!(!r.contains(Rights::RECV));
    }

    #[test]
    fn contains_subset() {
        let r = Rights::READ | Rights::WRITE | Rights::SEND;
        assert!(r.contains(Rights::READ));
        assert!(r.contains(Rights::WRITE));
        assert!(r.contains(Rights::SEND));
        assert!(!r.contains(Rights::GRANT));
    }

    #[test]
    fn contains_all_is_superset_of_everything() {
        let all = Rights::all();
        for bit in [Rights::READ, Rights::WRITE, Rights::SEND,
                    Rights::RECV, Rights::GRANT, Rights::REVOKE] {
            assert!(all.contains(bit));
        }
    }

    #[test]
    fn narrow_never_widens() {
        let r = Rights::READ | Rights::WRITE;
        let narrowed = r.narrow(Rights::READ);
        assert!(narrowed.contains(Rights::READ));
        assert!(!narrowed.contains(Rights::WRITE));
    }

    #[test]
    fn narrow_to_empty_yields_empty() {
        assert!(!(Rights::all().narrow(Rights::empty()).contains(Rights::READ)));
    }

    #[test]
    fn narrow_is_idempotent() {
        let r = Rights::READ | Rights::SEND;
        assert_eq!(r.narrow(r).0, r.0);
    }

    #[test]
    fn union_is_superset() {
        let a = Rights::READ;
        let b = Rights::WRITE;
        let u = a.union(b);
        assert!(u.contains(Rights::READ));
        assert!(u.contains(Rights::WRITE));
    }

    #[test]
    fn bitor_operator_matches_union() {
        let a = Rights::SEND;
        let b = Rights::RECV;
        assert_eq!((a | b).0, a.union(b).0);
    }

    /// Overlapping operands: a right held on both sides stays held (an exclusive-or would drop it).
    #[test]
    fn bitor_keeps_a_right_both_sides_hold() {
        let a = Rights::READ | Rights::GRANT;
        let b = Rights::GRANT | Rights::SEND;
        assert_eq!((a | b).0, (Rights::READ | Rights::GRANT | Rights::SEND).0);
        assert_eq!((a | a).0, a.0);
    }

    /// A cap of one use stays one through any narrowing.
    #[test]
    fn narrow_never_drops_once() {
        let r = Rights::SEND | Rights::GRANT | Rights::ONCE;
        assert!(r.narrow(Rights::SEND).contains(Rights::ONCE));
        assert!(!r.narrow(Rights::READ).contains(Rights::SEND));
        assert!(!(Rights::SEND).narrow(Rights::SEND).contains(Rights::ONCE), "narrow adds nothing");
    }

    /// `without` clears exactly the mask and keeps everything else (SEC-7 strips GRANT this way).
    #[test]
    fn without_clears_exactly_the_mask() {
        let r = Rights::READ | Rights::SEND | Rights::GRANT;
        assert_eq!(r.without(Rights::GRANT).0, (Rights::READ | Rights::SEND).0);
        assert_eq!(r.without(Rights::WRITE).0, r.0, "a right not held is not added");
        assert_eq!(r.without(Rights::empty()).0, r.0);
        assert_eq!(r.without(Rights::all()).0, 0);
    }

    /// Each named right is its own bit.
    #[test]
    fn the_named_rights_are_the_six_low_bits() {
        let named = [Rights::READ, Rights::WRITE, Rights::SEND, Rights::RECV, Rights::GRANT, Rights::REVOKE];
        for (i, r) in named.iter().enumerate() { assert_eq!(r.0, 1 << i); }
    }

    #[test]
    fn empty_contains_nothing() {
        let e = Rights::empty();
        for bit in [Rights::READ, Rights::WRITE, Rights::SEND,
                    Rights::RECV, Rights::GRANT, Rights::REVOKE] {
            assert!(!e.contains(bit));
        }
    }

    // --- property tests (§22 P3) -------------------------------------------
    // Only the lower 6 bits are used; generate values in that range.

    proptest! {
        /// Over the six rights, narrow(a, b) == a & b: never sets a bit absent from either operand
        /// (§7.3 non-escalating). `ONCE`, which narrow keeps, is `narrow_never_drops_once`.
        #[test]
        fn narrow_result_equals_bitwise_and(
            a in 0u8..=0b0011_1111u8,
            b in 0u8..=0b0011_1111u8,
        ) {
            prop_assert_eq!(Rights(a).narrow(Rights(b)).0, a & b);
        }

        /// contains(a, b) iff every bit in b is also in a.
        #[test]
        fn contains_is_bitwise_subset(
            a in 0u8..=0b0011_1111u8,
            b in 0u8..=0b0011_1111u8,
        ) {
            prop_assert_eq!(Rights(a).contains(Rights(b)), (a & b) == b);
        }

        /// union(a, b) is a superset of both operands.
        #[test]
        fn union_is_superset_of_both_operands(
            a in 0u8..=0b0011_1111u8,
            b in 0u8..=0b0011_1111u8,
        ) {
            let u = Rights(a).union(Rights(b));
            prop_assert!(u.contains(Rights(a)));
            prop_assert!(u.contains(Rights(b)));
        }

        /// Rights::all() contains every valid single-bit right.
        #[test]
        fn all_contains_every_valid_right(r in 0u8..=0b0011_1111u8) {
            prop_assert!(Rights::all().contains(Rights(r)));
        }

        /// Rights::empty() never contains any non-zero right.
        #[test]
        fn empty_never_contains_nonzero_right(r in 1u8..=0b0011_1111u8) {
            prop_assert!(!Rights::empty().contains(Rights(r)));
        }
    }
}
