//! `init` — PID 1 equivalent. TCB member (§6.1).
//!
//! Spawns supervisor, registry, and logger in order via the Spawn syscall.
//! Panics (→ kernel panic) if any TCB spawn fails (§6.2).
//! Logger is not TCB: a single retry is attempted before continuing without it.
//! After startup, init loops forever yielding the CPU.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("init: ready");

    // Spawn TCB services in order; kernel panic semantics if any fail (§6.2).
    if ctx.spawn("supervisor").is_err() {
        ctx.log("init: FATAL: failed to spawn supervisor");
        loop {}
    }

    if ctx.spawn("registry").is_err() {
        ctx.log("init: FATAL: failed to spawn registry");
        loop {}
    }

    // logger is not TCB (§11.3); retry once on failure and continue without it.
    if ctx.spawn("logger").is_err() {
        ctx.log("init: logger spawn failed, retrying");
        let _ = ctx.spawn("logger");
    }

    loop {
        ctx.yield_cpu();
    }
}
