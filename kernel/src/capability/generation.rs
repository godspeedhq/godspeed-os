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
