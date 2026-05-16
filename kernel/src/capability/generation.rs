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
}
