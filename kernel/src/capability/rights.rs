// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Capability rights bitfield — §7.4.

/// Actions a capability may authorise on its target resource.
///
/// Rights are **non-escalating**: a `GRANT` transfer can only narrow rights,
/// never widen them. The kernel enforces this on every cap insertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights(pub(crate) u8);

impl Rights {
    pub const READ:   Rights = Rights(1 << 0);
    pub const WRITE:  Rights = Rights(1 << 1);
    pub const SEND:   Rights = Rights(1 << 2);
    pub const RECV:   Rights = Rights(1 << 3);
    pub const GRANT:  Rights = Rights(1 << 4);
    pub const REVOKE: Rights = Rights(1 << 5);

    pub const fn empty() -> Self { Rights(0) }
    pub const fn all()   -> Self { Rights(0b0011_1111) }

    pub fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Narrow `self` to the intersection with `mask`. Never widens.
    pub fn narrow(self, mask: Rights) -> Rights {
        Rights(self.0 & mask.0)
    }

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
        /// narrow(a, b) == a & b: never sets bits absent from either operand (§7.3 non-escalating).
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
