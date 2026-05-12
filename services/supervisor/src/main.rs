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
    // --- Probe services (§22 Group A identity tests) ---
    // Recv-endpoint probes must come first so their endpoints are registered
    // before sender probes are spawned (caps are wired at spawn time).
    let _ = ctx.spawn("probe-recv");    // Test 3A receiver
    let _ = ctx.spawn("probe-victim");  // Test 4A kill target
    let _ = ctx.spawn("probe-4b-recv"); // Test 4B kill target
    let _ = ctx.spawn("probe-3b");      // Test 3B (has recv slot for wrong-right probe)
    // Sender / active probes — need SEND caps to the services above.
    let _ = ctx.spawn("probe-sender");  // Test 3A sender; SEND cap to probe-recv
    let _ = ctx.spawn("probe-4a");      // Test 4A; SEND cap to probe-victim
    let _ = ctx.spawn("probe-4b-send"); // Test 4B; SEND cap to probe-4b-recv
    // Probes with no send peers.
    let _ = ctx.spawn("probe-yielder"); // Test 8A
    let _ = ctx.spawn("probe-hog");     // Test 8B (tight loop; preemption via ping)
    let _ = ctx.spawn("probe-9b");      // Test 9B

    // --- Original ping/pong services ---
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
