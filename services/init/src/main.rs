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

    // Spawn TCB services in order; Abort syscall (9) triggers kernel panic on
    // failure (§6.2).  The panic handler emits "KERNEL PANIC" + reason to serial.
    ctx.log("init: spawning supervisor...");
    if ctx.spawn("supervisor").is_err() {
        ctx.log("init: FATAL: failed to spawn supervisor");
        ctx.abort("supervisor spawn failed");
    }

    ctx.log("init: spawning registry...");
    if ctx.spawn("registry").is_err() {
        ctx.log("init: FATAL: failed to spawn registry");
        ctx.abort("registry spawn failed");
    }

    // logger is not TCB (§11.3); retry once on failure and continue without it.
    ctx.log("init: spawning logger...");
    if ctx.spawn("logger").is_err() {
        ctx.log("init: logger spawn failed, retrying");
        let _ = ctx.spawn("logger");
    }

    ctx.log("init: all spawns done, entering yield loop");
    loop {
        ctx.yield_cpu();
    }
}
