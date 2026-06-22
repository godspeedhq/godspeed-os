// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Per-resource generation counters - §7.5.
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

    /// Bump to the next generation. **Never wraps** - `checked_add` panics on
    /// overflow rather than rolling over. A wrap to a low value would be a silent
    /// *authority resurrection*: a stale cap minted at that low generation would
    /// match a live resource again, defeating revocation (§7.5). A u32 gives ~4
    /// billion restarts of a single resource, unreachable in practice; the panic is
    /// the loud-failure backstop (§3.12; hardening H7). Pinned by
    /// `bump_at_max_panics_never_wraps`.
    pub fn bump(self) -> Self {
        Generation(self.0.checked_add(1).expect("generation overflow"))
    }

    pub fn matches(self, other: Generation) -> bool {
        self.0 == other.0
    }
}

use core::sync::atomic::{AtomicU32, Ordering};

/// The single global source for a NEW endpoint's starting generation (§7.5).
///
/// Every endpoint creation (service spawn/respawn) takes the next value, so a respawn's generation
/// strictly exceeds EVERY previously-issued endpoint generation. That gives per-service monotonicity —
/// a restart always resolves to a higher generation (property P2/P8) — **without** depending on the
/// service's name still being in the kernel directory (the self-heal unregisters it on death, §14.2)
/// or on which endpoint-id the reclaim free-list hands out (a higher global generation invalidates
/// older caps regardless, subsuming the ABA guard id-reuse needed). It replaces the by-name/by-slot
/// seeding, whose by-name source the self-heal removed.
///
/// Per-resource `bump()` on death/revoke is unchanged; this only sources the generation a fresh
/// endpoint STARTS at. Starts at 1 (0 = `INITIAL`, reserved for stable kernel resources). Panics on
/// u32 wrap rather than rolling to 0 (which would alias `INITIAL` and resurrect authority) — ~4.2B
/// creations per boot, unreachable in practice; loud per H7.
static NEXT_GENERATION: AtomicU32 = AtomicU32::new(1);

pub fn next_generation() -> Generation {
    let g = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
    if g == 0 { panic!("generation counter overflow (wrapped to 0)"); }
    Generation(g)
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

    /// H7 - the overflow guarantee. `bump` NEVER wraps: at `u32::MAX` it panics
    /// loudly rather than rolling over to a low value, which would let a stale cap
    /// (minted at that low generation) match a live resource again - silent
    /// authority resurrection. This pins the behaviour so a future change to
    /// `wrapping_add` cannot reintroduce the resurrection path unnoticed; the
    /// existing property tests deliberately exclude `u32::MAX`, leaving this the
    /// only test of the boundary.
    #[test]
    #[should_panic(expected = "generation overflow")]
    fn bump_at_max_panics_never_wraps() {
        let _ = Generation(u32::MAX).bump();
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
