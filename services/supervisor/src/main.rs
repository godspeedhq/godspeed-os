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
    // Cap-transfer probes (Tests 5A and 5B) — receiver first so its endpoint
    // is registered before the senders' SEND|GRANT caps are wired.
    let _ = ctx.spawn("probe-5a-recv"); // Test 5A/5B receiver
    let _ = ctx.spawn("probe-5a-send"); // Test 5A sender (SEND|GRANT cap)
    let _ = ctx.spawn("probe-5b-send"); // Test 5B negative (SEND-only cap)
    // Probes with no send peers.
    let _ = ctx.spawn("probe-yielder"); // Test 8A
    let _ = ctx.spawn("probe-hog");     // Test 8B (tight loop; preemption via ping)
    let _ = ctx.spawn("probe-9b");      // Test 9B
    // Memory-limit probes — Tests 7A and 7B.
    let _ = ctx.spawn("probe-7a");
    let _ = ctx.spawn("probe-7b");
    // Property-test probes — Milestone 9 Phase 1.
    // prop-p9-victim must register its endpoint before prop-p9 is spawned
    // (SEND caps to prop-p9-victim are wired at prop-p9 spawn time).
    let _ = ctx.spawn("prop-p9-victim");
    let _ = ctx.spawn("prop-p9");
    let _ = ctx.spawn("prop-p1");
    let _ = ctx.spawn("prop-p10");
    // Property-test probes — Milestone 9 Phase 2.
    // P3 and P6 are spawned BEFORE the kill/respawn controllers (P2, P8) so they
    // are already running by the time P2 and P8 begin their kill/respawn loops.
    // P2 and P8 each do rapid kill/respawn cycles that compete for kernel resources;
    // spawning the self-contained probes first prevents CPU starvation of P3/P6.
    let _ = ctx.spawn("prop-p3");        // P3: self-referential cap bounce (no victims)
    let _ = ctx.spawn("prop-p6");        // P6: self-referential queue depth test (no victims)
    // Kill/respawn victims must be registered before their controller probes start.
    let _ = ctx.spawn("prop-p2-victim"); // P2: kill/respawn generation target
    let _ = ctx.spawn("prop-p2");        // P2 controller — starts cycling immediately
    let _ = ctx.spawn("prop-p8-victim"); // P8: kill/respawn generation target
    let _ = ctx.spawn("prop-p8");        // P8 controller — starts cycling immediately

    // Property-test probes — Milestone 9 Phase 3.
    // P4 has no victim. P5 and P7 victims must be registered before their
    // controllers so endpoints exist when the controllers start cycling.
    let _ = ctx.spawn("prop-p4");
    let _ = ctx.spawn("prop-p5-victim");
    let _ = ctx.spawn("prop-p5");
    let _ = ctx.spawn("prop-p7-victim");
    let _ = ctx.spawn("prop-p7");

    // --- Fuzz-test probes — Milestone 10 Phase 1 ---
    // Recv-endpoint victims/targets must be spawned before their controllers.
    let _ = ctx.spawn("fuzz-f1");
    let _ = ctx.spawn("fuzz-f2");
    let _ = ctx.spawn("fuzz-f5-recv");
    let _ = ctx.spawn("fuzz-f5");
    let _ = ctx.spawn("fuzz-f6-recv");
    let _ = ctx.spawn("fuzz-f6");
    let _ = ctx.spawn("fuzz-f7-victim");
    let _ = ctx.spawn("fuzz-f7");
    let _ = ctx.spawn("fuzz-f8");

    // --- Stress-test probes — Milestone 11 Phase 1 ---
    // Recv-endpoint victims must be spawned before their controllers so their
    // endpoints are registered before the controllers' SEND caps are wired.
    let _ = ctx.spawn("stress-s1-recv");
    let _ = ctx.spawn("stress-s1");
    let _ = ctx.spawn("stress-s2-victim");
    let _ = ctx.spawn("stress-s2");
    let _ = ctx.spawn("stress-s3-recv");   // core 1 — cross-core thrash receiver
    let _ = ctx.spawn("stress-s3-send");   // core 0 — cross-core thrash sender
    let _ = ctx.spawn("stress-s4-victim");
    let _ = ctx.spawn("stress-s4");
    let _ = ctx.spawn("stress-s7");
    let _ = ctx.spawn("stress-s10-victim"); // core 1 — cascading revocation target
    let _ = ctx.spawn("stress-s10");        // core 0 — kills victim cross-core
    // Stress Phase 2 — S5, S6, S8, S9.
    // s5-victim must register before s5 starts cycling.
    // s9-recv must register before s9-send-a/b are wired with SEND caps.
    let _ = ctx.spawn("stress-s5-victim");
    let _ = ctx.spawn("stress-s5");
    let _ = ctx.spawn("stress-s6");        // self-referential; endpoint registered at spawn time
    let _ = ctx.spawn("stress-s8");
    let _ = ctx.spawn("stress-s9-recv");   // core 2 — concurrent IPI storm receiver
    let _ = ctx.spawn("stress-s9-send-a"); // core 0 → core 2
    let _ = ctx.spawn("stress-s9-send-b"); // core 1 → core 2

    // --- Adversarial-test probes — Milestone 13 ---
    // Passive/victim services must be spawned before their attackers so their
    // endpoints are registered when the attackers' SEND caps are wired.
    let _ = ctx.spawn("adv-a1");
    let _ = ctx.spawn("adv-a2");
    let _ = ctx.spawn("adv-a3");
    let _ = ctx.spawn("adv-a4");
    let _ = ctx.spawn("adv-a5-victim"); // passive — killed by adv-a5
    let _ = ctx.spawn("adv-a5");
    let _ = ctx.spawn("adv-a6");
    let _ = ctx.spawn("adv-a7-recv");   // passive — recv target before sender wired
    let _ = ctx.spawn("adv-a7");
    let _ = ctx.spawn("adv-a8");
    let _ = ctx.spawn("adv-a8-witness");
    let _ = ctx.spawn("adv-a9");
    let _ = ctx.spawn("adv-a10");

    // --- Performance-benchmark probes — Milestone 12 ---
    // Spawn sender/controller probes BEFORE their echo/recv partners so the
    // sender's endpoint is registered when the echo partner wires its SEND cap.
    // perf-b5-victim must be registered before perf-b5 starts cycling.
    let _ = ctx.spawn("perf-b1");         // B1 sender (core 0) — registers endpoint first
    let _ = ctx.spawn("perf-b1-echo");    // B1 echo (core 0)   — wires SEND cap to perf-b1
    let _ = ctx.spawn("perf-b2");         // B2 sender (core 0) — registers endpoint first
    let _ = ctx.spawn("perf-b2-echo");    // B2 echo  (core 1)  — wires SEND cap to perf-b2
    let _ = ctx.spawn("perf-b3");
    let _ = ctx.spawn("perf-b4");
    let _ = ctx.spawn("perf-b5-victim");  // spawned before perf-b5 so it exists to be killed
    let _ = ctx.spawn("perf-b5");
    let _ = ctx.spawn("perf-b7");
    let _ = ctx.spawn("perf-b8");
    let _ = ctx.spawn("perf-b9-recv");    // recv partner registered before sender is wired
    let _ = ctx.spawn("perf-b9");
    let _ = ctx.spawn("perf-b10");

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
