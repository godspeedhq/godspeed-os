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

pub mod capability {
    pub mod cap;
    pub mod generation;
    pub mod rights;
}

pub mod ipc {
    pub mod message;
    pub mod queue;
}
