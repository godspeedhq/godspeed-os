// Pure-logic library target — only used for unit testing and coverage.
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
}

pub mod ipc {
    pub mod message;
    pub mod queue;
}

// Bitmap allocator model — compiled only in test mode.
// memory/bitmap.rs has no hardware dependencies and uses std (Vec, HashSet).
#[cfg(test)]
mod memory {
    pub mod bitmap;
}
