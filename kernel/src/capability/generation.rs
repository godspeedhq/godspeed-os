//! Per-resource generation counters — §7.5.
//!
//! Every capability carries a generation. Every resource in the kernel tracks
//! its current generation. A mismatch on use means the cap is stale.
//!
//! Bumping rules (§7.5):
//!   - Restartable/destroyable resources bump when destroyed or replaced.
//!   - Stable kernel-owned resources stay at generation 0 forever.
//!
//! The generation check is the v1 cross-core revocation mechanism: bumping on
//! one core makes every cap on every other core stale without synchronous
//! notification. Correctness relies on a memory barrier before the bump is
//! visible (§7.8).

/// Monotonically-increasing generation counter for one resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(pub u32);

impl Generation {
    pub const INITIAL: Generation = Generation(0);

    /// Bump to the next generation. Never wraps — a u32 gives 4 billion
    /// restarts per resource before overflow, sufficient for v1.
    pub fn bump(self) -> Self {
        Generation(self.0.checked_add(1).expect("generation overflow"))
    }

    pub fn matches(self, other: Generation) -> bool {
        self.0 == other.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn initial_is_zero() {
        assert_eq!(Generation::INITIAL.0, 0);
    }

    #[test]
    fn bump_is_monotonic() {
        let g = Generation::INITIAL;
        let g1 = g.bump();
        let g2 = g1.bump();
        assert!(g1.0 > g.0);
        assert!(g2.0 > g1.0);
    }

    #[test]
    fn matches_same_value() {
        let g = Generation(42);
        assert!(g.matches(Generation(42)));
    }

    #[test]
    fn does_not_match_different_value() {
        let g = Generation(1);
        assert!(!g.matches(Generation(2)));
    }

    #[test]
    fn stale_cap_detected_after_bump() {
        let live = Generation::INITIAL;
        let cap_gen = live;           // cap was minted at generation 0
        let after_restart = live.bump(); // resource was restarted
        assert!(!cap_gen.matches(after_restart)); // cap is now stale
    }

    #[test]
    fn many_bumps_stay_monotonic() {
        let mut g = Generation::INITIAL;
        for _ in 0..1000 {
            let next = g.bump();
            assert!(next.0 > g.0);
            g = next;
        }
    }

    // --- property tests (§22 P2) -------------------------------------------

    proptest! {
        /// For any non-max value, bump increments by exactly 1 (§7.5 monotonic).
        #[test]
        fn bump_increments_by_one(v in 0u32..u32::MAX) {
            let g = Generation(v);
            prop_assert_eq!(g.bump().0, v + 1);
        }

        /// matches is true iff the two values are equal.
        #[test]
        fn matches_iff_values_equal(a in any::<u32>(), b in any::<u32>()) {
            prop_assert_eq!(Generation(a).matches(Generation(b)), a == b);
        }

        /// A cap minted at generation v is always stale after one bump (§7.5).
        #[test]
        fn stale_cap_always_rejected_after_bump(v in 0u32..u32::MAX) {
            let live = Generation(v);
            prop_assert!(!live.matches(live.bump()));
        }
    }
}
