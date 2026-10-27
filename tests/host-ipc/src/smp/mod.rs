// SPDX-License-Identifier: GPL-2.0-only
// A real directory, so the relative `#[path]` resolves on Linux as well as Windows (tests/kani says why).
#[path = "../../../../kernel/src/smp/spinlock.rs"]
pub mod spinlock;
pub use spinlock::SpinLock;
