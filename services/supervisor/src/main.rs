//! `supervisor` — restart authority. TCB member (§6.1). Non-restartable.
//!
//! Phase 5:
//!   1. Spawns `pong` on core 1 and `ping` on core 0 (§23.2 acceptance criteria).
//!   2. Logs "supervisor: ready".
//!   3. Yields indefinitely (death-notification restart loop deferred to Phase 6).
//!
//! The kernel wires send-peer SEND caps at spawn time, so supervisor does not
//! need to coordinate cap distribution manually.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Spawn pong first so the kernel registers "pong" in its name table before
    // ping is spawned (ping needs a SEND cap to pong at spawn time — §5 in
    // task/mod.rs service_config).
    if ctx.spawn_on("pong", 1).is_err() {
        ctx.log("supervisor: WARN: failed to spawn pong on core 1, trying core 0");
        let _ = ctx.spawn_on("pong", 0);
    }

    if ctx.spawn_on("ping", 0).is_err() {
        ctx.log("supervisor: WARN: failed to spawn ping");
    }

    ctx.log("supervisor: ready");

    loop {
        ctx.yield_cpu();
    }
}
