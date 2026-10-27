// SPDX-License-Identifier: GPL-2.0-only
//! The kernel's capability files, compiled as they ship, for Kani to check (backlog/90, layer C).
//!
//! Nothing here is a model. Each `#[path]` names the real file in `kernel/src/`, and the scaffolding
//! around them is the same thin seam `kernel/src/lib.rs` gives its host tests: a `kprintln` that
//! formats nothing, and an arch seam whose interrupt masking is a no-op. What Kani proves is
//! therefore a property of the code the kernel runs - for every input inside the stated bound, not
//! for the inputs a test happened to try.
//!
//! The harnesses are in `harnesses.rs`. `cargo kani` runs them all; README.md says how.

#![cfg_attr(not(kani), allow(dead_code))]

#[macro_export]
macro_rules! kprintln {
    ($($args:tt)*) => { let _ = format_args!($($args)*); };
}

pub mod capability;

pub mod arch {
    pub mod imp {
        #[inline(always)] pub fn local_irq_save() -> bool { false }
        #[inline(always)] pub fn local_irq_restore(_was_enabled: bool) {}
    }
}

pub mod smp;

#[cfg(kani)]
mod harnesses;
