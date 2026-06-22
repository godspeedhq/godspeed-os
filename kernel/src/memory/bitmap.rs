// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Bitmap allocator model for property testing - §10, §22 item 6.1.
//!
//! `TestBitmapAllocator` is a host-compilable model of `memory/allocator.rs`'s
//! `BitmapAllocator`. It exercises the same algorithmic invariants (uniqueness,
//! count consistency, phantom-frame rejection, free-all recovery) with a small,
//! heap-backed bitmap so property tests finish in milliseconds.
//!
//! This module is ONLY compiled when running `cargo test -p kernel --lib`
//! (gated via `#[cfg(test)] mod memory { mod bitmap; }` in lib.rs).
//! It is never part of the bare-metal kernel binary.

use std::collections::HashSet;

/// Configurable small allocator used only by property tests.
pub struct TestBitmapAllocator {
    free:       Vec<bool>,  // true = frame is free
    max_frames: usize,
    live_count: usize,
}

impl TestBitmapAllocator {
    pub fn new(max_frames: usize) -> Self {
        TestBitmapAllocator {
            free:       vec![true; max_frames],
            max_frames,
            live_count: 0,
        }
    }

    /// Allocate the first free frame. Returns `None` when fully exhausted.
    pub fn alloc(&mut self) -> Option<usize> {
        let idx = self.free.iter().position(|&f| f)?;
        self.free[idx] = false;
        self.live_count += 1;
        Some(idx)
    }

    /// Free a frame.
    /// Returns `false` (and does nothing) for phantom frames (idx >= max) or double-frees,
    /// mirroring the real allocator's phantom-frame guard and double-free defence.
    pub fn free(&mut self, frame: usize) -> bool {
        if frame >= self.max_frames { return false; }
        if self.free[frame] { return false; }  // already free - double-free
        self.free[frame] = true;
        self.live_count -= 1;
        true
    }

    pub fn live_count(&self) -> usize { self.live_count }
    pub fn free_count(&self) -> usize { self.free.iter().filter(|&&f| f).count() }
    pub fn is_all_free(&self)  -> bool { self.live_count == 0 }

    /// Snapshot the set of currently-live (allocated) frame indices.
    #[allow(dead_code)]
    pub fn live_frames(&self) -> HashSet<usize> {
        (0..self.max_frames).filter(|&i| !self.free[i]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MAX_FRAMES: usize = 64;

    #[derive(Debug, Clone)]
    enum Op { Alloc, Free(usize) }

    fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
        proptest::collection::vec(
            prop_oneof![
                Just(Op::Alloc),
                (0usize..MAX_FRAMES).prop_map(Op::Free),
            ],
            0..128,
        )
    }

    /// Run a sequence of ops, keeping a reference `live` set in sync with the allocator.
    fn run(alloc: &mut TestBitmapAllocator, live: &mut HashSet<usize>, ops: &[Op]) {
        for op in ops {
            match op {
                Op::Alloc => {
                    if let Some(f) = alloc.alloc() { live.insert(f); }
                }
                Op::Free(f) => {
                    if live.remove(f) { alloc.free(*f); }
                }
            }
        }
    }

    // --- properties (§22 item 6.1) -----------------------------------------

    proptest! {
        /// live_count + free_count == max_frames under any alloc/free sequence.
        #[test]
        fn count_always_sums_to_total(ops in ops_strategy()) {
            let mut alloc = TestBitmapAllocator::new(MAX_FRAMES);
            let mut live  = HashSet::new();
            run(&mut alloc, &mut live, &ops);
            prop_assert_eq!(
                alloc.live_count() + alloc.free_count(),
                MAX_FRAMES,
                "count mismatch: live={} free={} max={}",
                alloc.live_count(), alloc.free_count(), MAX_FRAMES
            );
        }

        /// No two outstanding allocations ever return the same frame index.
        #[test]
        fn live_allocations_never_overlap(ops in ops_strategy()) {
            let mut alloc = TestBitmapAllocator::new(MAX_FRAMES);
            let mut live: HashSet<usize> = HashSet::new();
            for op in &ops {
                match op {
                    Op::Alloc => {
                        if let Some(f) = alloc.alloc() {
                            prop_assert!(
                                !live.contains(&f),
                                "frame {} returned twice (double allocation)", f
                            );
                            live.insert(f);
                        }
                    }
                    Op::Free(f) => { if live.remove(f) { alloc.free(*f); } }
                }
            }
        }

        /// alloc never returns a frame index at or above max_frames.
        #[test]
        fn alloc_never_exceeds_max_valid_frame(ops in ops_strategy()) {
            let mut alloc = TestBitmapAllocator::new(MAX_FRAMES);
            let mut live  = HashSet::new();
            for op in &ops {
                match op {
                    Op::Alloc => {
                        if let Some(f) = alloc.alloc() {
                            prop_assert!(
                                f < MAX_FRAMES,
                                "frame {} >= max_frames {}", f, MAX_FRAMES
                            );
                            live.insert(f);
                        }
                    }
                    Op::Free(f) => { if live.remove(f) { alloc.free(*f); } }
                }
            }
        }

        /// After freeing all live frames the allocator returns to fully-free state.
        #[test]
        fn free_all_returns_to_initial_state(ops in ops_strategy()) {
            let mut alloc = TestBitmapAllocator::new(MAX_FRAMES);
            let mut live  = HashSet::new();
            run(&mut alloc, &mut live, &ops);
            for f in live.iter() { alloc.free(*f); }  // free everything remaining
            prop_assert!(alloc.is_all_free(),  "allocator not empty after free-all");
            prop_assert_eq!(alloc.free_count(), MAX_FRAMES);
        }

        /// free() silently rejects phantom frames (idx >= max_frames).
        #[test]
        fn phantom_frame_free_is_silently_rejected(
            phantom in MAX_FRAMES..=(MAX_FRAMES * 4),
        ) {
            let mut alloc = TestBitmapAllocator::new(MAX_FRAMES);
            let rejected  = !alloc.free(phantom);
            prop_assert!(rejected, "phantom frame {} was not rejected", phantom);
            prop_assert_eq!(alloc.live_count(), 0);   // state unchanged
            prop_assert_eq!(alloc.free_count(), MAX_FRAMES);
        }
    }
}
