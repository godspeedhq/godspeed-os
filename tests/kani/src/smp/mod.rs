// SPDX-License-Identifier: GPL-2.0-only
//! The kernel's spin lock, as it ships (table.rs takes it around the global record).

#[path = "../../../../kernel/src/smp/spinlock.rs"]
pub mod spinlock;
pub use spinlock::SpinLock;
