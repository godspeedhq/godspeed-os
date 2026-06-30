// SPDX-License-Identifier: GPL-2.0-only
// Pure-logic library target - only used for unit testing and coverage.
//
// This crate compiles for the HOST when you run `cargo test -p kernel` or
// `cargo llvm-cov -p kernel`.  It exposes the subset of kernel modules that
// are free of hardware/arch/smp dependencies and can therefore be exercised
// without QEMU.
//
// The actual kernel binary is the [[bin]] target (src/main.rs).  Do not add
// imports here that pull in arch/, memory/allocator.rs, smp/, or anything
// else that touches hardware.
#![cfg_attr(not(test), no_std)]

// Pure ELF-segment → page-permission logic (W^X enforcement, H4a). No hardware
// dependencies, so it is host-testable here and also used by the bin's loader.rs.
pub mod elf_flags;

// Pure clock-deglitch logic (the "4987d" uptime guard). No hardware deps, so it is host-testable here and
// is also used by the bin's arch/x86_64/rtc.rs::now_epoch_monotonic. Same pattern as elf_flags above.
pub mod clock;

// capability/table.rs emits diagnostic messages via crate::kprintln!.
// The binary target defines the real kprintln! in log.rs; the lib (host)
// target provides this no-op stub so table.rs compiles without hardware.
#[cfg(test)]
#[macro_export]
macro_rules! kprintln {
    ($($args:tt)*) => { let _ = format_args!($($args)*); };
}

pub mod capability {
    pub mod cap;
    pub mod generation;
    pub mod rights;
    // table.rs uses crate::kprintln! which only exists in test mode (stub in lib.rs)
    // and in the bin target (real impl in log.rs). Gate here so the bare-metal lib
    // build doesn't try to compile it without the macro.
    #[cfg(test)]
    pub mod table;
    // delegated.rs (§7.10) depends only on cap + table + SpinLock (no ipc/hardware), so its
    // pure band logic is host-unit-testable. Gated on test (it uses the test-only `table`).
    #[cfg(test)]
    pub mod delegated;
}

pub mod ipc {
    pub mod message;
    pub mod queue;
    // Routing and name directory models - test-only, no SpinLock or hardware deps.
    // Pattern mirrors memory/bitmap.rs (item 6).
    #[cfg(test)]
    pub mod routing_model;
    #[cfg(test)]
    pub mod names_model;
}

// SpinLock is used by capability/table.rs (GLOBAL_RESOURCES).
// spinlock.rs uses only core primitives so it compiles fine in std test mode.
pub mod smp {
    pub mod spinlock;
    pub use spinlock::SpinLock;
}

// Bitmap allocator model - compiled only in test mode.
// memory/bitmap.rs has no hardware dependencies and uses std (Vec, HashSet).
#[cfg(test)]
mod memory {
    pub mod bitmap;
}
