// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `init` — PID 1 equivalent. TCB member (§6.1).
//!
//! Spawns the supervisor and logger via the Spawn syscall, then parks.
//! Panics (→ kernel panic) if the supervisor spawn fails (§6.2).
//! Logger is not TCB: a single retry is attempted before continuing without it.
//!
//! **`registry` is spawned by the supervisor, not init** (naming Phase 3b,
//! `docs/naming-design.md`, §11): the supervisor owns naming, so it spawns the name service
//! first and holds its cap to wire every other service. init no longer touches registry.

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

    // registry is spawned by the supervisor now (Phase 3b) — it owns naming, so it must hold
    // registry's cap. init no longer spawns it. Its boot-time spawn failure is still fatal,
    // enforced by the supervisor (§11.3).

    // logger is not TCB (§11.3); retry once on failure and continue without it.
    ctx.log("init: spawning logger...");
    if ctx.spawn("logger").is_err() {
        ctx.log("init: logger spawn failed, retrying");
        let _ = ctx.spawn("logger");
    }

    ctx.log("init: all spawns done, parking");
    // Park (block forever) instead of busy-yielding: init has no further work, and
    // a yield loop would keep core 0 from ever halting — pegging it at 100%.
    ctx.park();
}
