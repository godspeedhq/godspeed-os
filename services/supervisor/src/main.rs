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
    // Spawn pong and ping first so IPC between them is established well before
    // probe services compete for scheduler quanta.  Pong must precede ping:
    // ping's SEND cap to pong is wired by the kernel at spawn time.
    // Skipped in idle-only builds (S8): no active workload by design.
    // Skipped in bp2-only: that mode isolates the BP2 cross-core round-trip
    // (perf-bp2 on core 0 ⇄ perf-bp2-echo on core 1) so echo is not starved by
    // the ping→pong flood on core 1 — gives clean, fast BP2 latency numbers.
    #[cfg(not(any(feature = "idle-only", feature = "bp2-only")))]
    {
        ctx.log("supervisor: spawning pong...");
        if ctx.spawn_on("pong", 1).is_err() {
            ctx.log("supervisor: WARN: failed to spawn pong on core 1, trying core 0");
            let _ = ctx.spawn_on("pong", 0);
        }
        ctx.log("supervisor: spawning ping...");
        if ctx.spawn_on("ping", 0).is_err() {
            ctx.log("supervisor: WARN: failed to spawn ping");
        }
        ctx.log("supervisor: pong+ping done");
    }

    // Identity probe services are harness-driven (QEMU control port sends kill
    // commands in response to sentinel serial strings).  Skip them in bare-metal,
    // perf-only, and perf-brutal-only builds: probe-hog tight-loops on core 0,
    // probe-4b-send blocks waiting for a harness kill that never arrives on HW,
    // and the combined 16-task load starves IPC benchmarks of scheduler quanta.
    #[cfg(not(any(feature = "bare-metal", feature = "perf-only", feature = "perf-brutal-only", feature = "stress-only", feature = "adv-only", feature = "chaos-only", feature = "b2-only", feature = "bp2-only")))]
    {
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
        // Interrupt-routing probe — Test IR1A (§12.2, §12.3).
        let _ = ctx.spawn("probe-11a");
    }

    // Property, fuzz, stress, perf, adversarial, chaos probes.
    // Excluded in identity-only builds so supervisor: ready appears in < 10 s on
    // TCG, giving WithRestart tests plenty of deadline margin (§22 flakiness fix).
    // Also excluded in bare-metal builds (no harness present).
    #[cfg(not(any(feature = "bare-metal", feature = "idle-only")))]
    spawn_extended_probes(&ctx);

    // observe: spawn in bare-metal + full builds only; excluded from all
    // test-specific builds where the extra 224-slot scan every 500 yields
    // would add noise to benchmark and stress timings.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only",
                  feature = "b2-only", feature = "bp2-only")))]
    let _ = ctx.spawn("observe");

    // shell: spawn alongside observe in bare-metal + full builds only.
    // Excluded from test-specific builds for the same reasons as observe.
    #[cfg(not(any(feature = "identity-only", feature = "perf-only",
                  feature = "perf-brutal-only", feature = "stress-only",
                  feature = "adv-only", feature = "chaos-only",
                  feature = "b2-only", feature = "bp2-only")))]
    let _ = ctx.spawn("shell");

    ctx.log("supervisor: ready");

    loop {
        ctx.yield_cpu();
    }
}

// ---------------------------------------------------------------------------
// Extended probes — all non-identity test categories.
//
// Feature-gated variants (in priority order):
//   identity-only     → spawn nothing (fastest boot, used by `osdev test identity`)
//   perf-only         → spawn only regular perf-b* probes (used by `osdev test perf`)
//   perf-brutal-only  → spawn only brutal perf-bp* probes (used by `osdev test perf-brutal`)
//   (none)            → spawn everything (used by `osdev build` / `osdev run`)
// ---------------------------------------------------------------------------

// bare-metal: no probes at all — spawn_extended_probes is never called, but
// the function must exist so the linker is happy.
#[cfg(feature = "bare-metal")]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// idle-only (S8): no probes, no pong/ping.
#[cfg(feature = "idle-only")]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// identity-only: skip all extended probes.
#[cfg(all(not(feature = "bare-metal"), feature = "identity-only"))]
#[inline(always)]
fn spawn_extended_probes(_ctx: &ServiceContext) {}

// perf-only: spawn only the regular performance benchmark probe services.
// Cuts spawn wait from ~18–120 s (178 probes) to ~2–5 s (~30 services) on TCG.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), feature = "perf-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Sender/controller before echo/recv so the sender's endpoint is registered
    // when the echo partner's SEND cap is wired at spawn time.
    // perf-b5-victim must be registered before perf-b5 starts cycling.
    let _ = ctx.spawn("perf-b1");
    let _ = ctx.spawn("perf-b1-echo");
    let _ = ctx.spawn("perf-b2");
    let _ = ctx.spawn("perf-b2-echo");
    let _ = ctx.spawn("perf-b3");
    let _ = ctx.spawn("perf-b4");
    let _ = ctx.spawn("perf-b5-victim");
    let _ = ctx.spawn("perf-b5");
    let _ = ctx.spawn("perf-b7");
    let _ = ctx.spawn("perf-b8");
    let _ = ctx.spawn("perf-b9-recv");
    let _ = ctx.spawn("perf-b9");
    let _ = ctx.spawn("perf-b10");
}

// perf-brutal-only: spawn only the brutal performance benchmark probe services.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), feature = "perf-brutal-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-bp1");
    let _ = ctx.spawn("perf-bp1-echo");
    let _ = ctx.spawn("perf-bp2");
    let _ = ctx.spawn("perf-bp2-echo");
    let _ = ctx.spawn("perf-bp3");
    let _ = ctx.spawn("perf-bp4");
    let _ = ctx.spawn("perf-bp5-victim");
    let _ = ctx.spawn("perf-bp5");
    let _ = ctx.spawn("perf-bp7");
    let _ = ctx.spawn("perf-bp8");
    let _ = ctx.spawn("perf-bp9-recv");
    let _ = ctx.spawn("perf-bp9");
    let _ = ctx.spawn("perf-bp10");
}

// stress-only: spawn only the S1–S10 stress probe services.
// All stress probes are self-contained (use ctx.kill/ctx.spawn internally);
// no QEMU control port required — safe for real hardware.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), feature = "stress-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Receivers/victims must register before their controllers so endpoints
    // exist when sender SEND caps are wired at spawn time.
    let _ = ctx.spawn("stress-s1-recv");
    let _ = ctx.spawn("stress-s1");
    let _ = ctx.spawn("stress-s2-victim");
    let _ = ctx.spawn("stress-s2");
    let _ = ctx.spawn("stress-s3-recv");    // core 1 — cross-core thrash receiver
    let _ = ctx.spawn("stress-s3-send");    // core 0 — cross-core thrash sender
    let _ = ctx.spawn("stress-s4-victim");
    let _ = ctx.spawn("stress-s4");
    let _ = ctx.spawn("stress-s5-victim");
    let _ = ctx.spawn("stress-s5");
    let _ = ctx.spawn("stress-s6");         // self-referential; endpoint registered at spawn
    let _ = ctx.spawn("stress-s7");
    let _ = ctx.spawn("stress-s8");
    let _ = ctx.spawn("stress-s9-recv");    // core 2 — IPI storm receiver
    let _ = ctx.spawn("stress-s9-send-a"); // core 0 → core 2
    let _ = ctx.spawn("stress-s9-send-b"); // core 1 → core 2
    let _ = ctx.spawn("stress-s10-victim"); // core 1 — cascading revocation target
    let _ = ctx.spawn("stress-s10");        // core 0 — kills victim cross-core
}

// chaos-only: spawn only the C2–C7 chaos probe services.
// C1 (degraded SMP boot) and C4 (minimal RAM) use bare-metal + hardware
// reconfiguration instead of probes.  All probes here are self-contained.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), feature = "chaos-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // BC7/C7 victims must be registered before their controllers so endpoints
    // exist when the controller's SEND caps are wired at spawn time.
    let _ = ctx.spawn("chaos-c2");          // non-TCB page fault, system continues
    let _ = ctx.spawn("chaos-c2-monitor");  // witness — alive after c2 faults
    let _ = ctx.spawn("chaos-c3");          // alloc-deny pressure cycles
    let _ = ctx.spawn("chaos-c5");          // recursive yields (kernel stack depth)
    let _ = ctx.spawn("chaos-c6-hog");      // tight-loop hog on core 3
    let _ = ctx.spawn("chaos-c6-monitor");  // witness on core 0
    let _ = ctx.spawn("chaos-c7-victim");   // passive recv target on core 2
    let _ = ctx.spawn("chaos-c7");          // TLB shootdown controller on core 1
}

// adv-only: spawn only the A1–A10 adversarial probe services.
// All adversarial probes are self-contained — no QEMU control port required.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), feature = "adv-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // Passive/victim services before their attackers so endpoints exist when
    // attacker SEND caps are wired at spawn time.
    let _ = ctx.spawn("adv-a1");
    let _ = ctx.spawn("adv-a2");
    let _ = ctx.spawn("adv-a3");
    let _ = ctx.spawn("adv-a4");
    let _ = ctx.spawn("adv-a5-victim"); // passive — killed by adv-a5
    let _ = ctx.spawn("adv-a5");
    let _ = ctx.spawn("adv-a6");
    let _ = ctx.spawn("adv-a7-recv");   // passive recv — registered before sender
    let _ = ctx.spawn("adv-a7");
    let _ = ctx.spawn("adv-a8");        // tight-loop attacker
    let _ = ctx.spawn("adv-a8-witness");
    let _ = ctx.spawn("adv-a9");
    let _ = ctx.spawn("adv-a10");
}

// b2-only: spawn only the regular B2 cross-core IPC probe pair (isolation build).
// No other benchmarks running — eliminates concurrent IPI noise from B5 spawn/kill
// and B6 restart cycles so the blocking round-trip can complete on Goldmont+.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), feature = "b2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-b2");      // B2 sender (core 0) — registers endpoint first
    let _ = ctx.spawn("perf-b2-echo"); // B2 echo  (core 1) — wires SEND cap to perf-b2
}

// bp2-only: spawn only the brutal BP2 cross-core IPC probe pair (isolation build).
// Brutal equivalent of b2-only — higher iteration count, same isolation rationale.
#[cfg(all(not(feature = "bare-metal"), not(feature = "identity-only"), not(feature = "perf-only"), not(feature = "perf-brutal-only"), not(feature = "stress-only"), not(feature = "adv-only"), not(feature = "chaos-only"), not(feature = "b2-only"), feature = "bp2-only"))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    let _ = ctx.spawn("perf-bp2");      // BP2 sender (core 0) — registers endpoint first
    let _ = ctx.spawn("perf-bp2-echo"); // BP2 echo  (core 1) — wires SEND cap to perf-bp2
}

// Full build: spawn all non-identity probe categories.
#[cfg(not(any(feature = "bare-metal", feature = "idle-only", feature = "identity-only", feature = "perf-only", feature = "perf-brutal-only", feature = "stress-only", feature = "adv-only", feature = "chaos-only", feature = "b2-only", feature = "bp2-only")))]
fn spawn_extended_probes(ctx: &ServiceContext) {
    // --- Brutal adversarial test probes — Milestone 20 ---
    // Spawned EARLY, before property/stress kill-respawn loops start, so the
    // supervisor's spawn calls land while the system is still lightly loaded.
    // Victims/passive services must be registered before their attackers so
    // their endpoints exist when the attacker's SEND caps are wired at spawn.
    let _ = ctx.spawn("adv-ba1");
    let _ = ctx.spawn("adv-ba2");
    let _ = ctx.spawn("adv-ba3");
    let _ = ctx.spawn("adv-ba4");
    let _ = ctx.spawn("adv-ba5-victim"); // registered before adv-ba5
    let _ = ctx.spawn("adv-ba5");
    let _ = ctx.spawn("adv-ba6");        // recv endpoint registered so self-fill works
    let _ = ctx.spawn("adv-ba7-recv");   // passive recv registered before sender
    let _ = ctx.spawn("adv-ba7");
    let _ = ctx.spawn("adv-ba8");        // tight-loop hog
    let _ = ctx.spawn("adv-ba8-witness");
    let _ = ctx.spawn("adv-ba9");
    let _ = ctx.spawn("adv-ba10");

    // --- Brutal chaos-test probes — Milestone 21 ---
    // Spawned EARLY before property/stress kill-respawn loops start.
    // BC2: 5 simultaneous faulters registered before the monitor so all 5
    // fault concurrently before the monitor starts counting yields.
    // BC7: victim registered before controller so its endpoint exists when
    // the controller's SEND cap is wired at spawn time.
    let _ = ctx.spawn("chaos-bc2-a");
    let _ = ctx.spawn("chaos-bc2-b");
    let _ = ctx.spawn("chaos-bc2-c");
    let _ = ctx.spawn("chaos-bc2-d");
    let _ = ctx.spawn("chaos-bc2-e");
    let _ = ctx.spawn("chaos-bc2-monitor");
    let _ = ctx.spawn("chaos-bc3");
    let _ = ctx.spawn("chaos-bc5");
    let _ = ctx.spawn("chaos-bc6-hog-a"); // hog on core 2
    let _ = ctx.spawn("chaos-bc6-hog-b"); // hog on core 3
    let _ = ctx.spawn("chaos-bc6-monitor"); // witness on core 0
    let _ = ctx.spawn("chaos-bc7-victim"); // passive target on core 2
    let _ = ctx.spawn("chaos-bc7");        // controller on core 1

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

    // --- Brutal property test probes — Milestone 16 ---
    // Victims before controllers within each pair.
    // Self-referential probes (BP3, BP6) can go in any order.
    let _ = ctx.spawn("prop-bp1");
    let _ = ctx.spawn("prop-bp2-victim");
    let _ = ctx.spawn("prop-bp2");
    let _ = ctx.spawn("prop-bp3");       // self-referential
    let _ = ctx.spawn("prop-bp4");
    let _ = ctx.spawn("prop-bp5-victim");
    let _ = ctx.spawn("prop-bp5");
    let _ = ctx.spawn("prop-bp6");       // self-referential
    let _ = ctx.spawn("prop-bp7-victim");
    let _ = ctx.spawn("prop-bp7");
    let _ = ctx.spawn("prop-bp8-victim");
    let _ = ctx.spawn("prop-bp8");
    let _ = ctx.spawn("prop-bp9-victim");
    let _ = ctx.spawn("prop-bp9");
    let _ = ctx.spawn("prop-bp10");

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

    // --- Brutal fuzz test probes — Milestone 17 ---
    // Recv-endpoint victims must be spawned before controllers so their
    // endpoints are registered when the controllers' SEND caps are wired.
    let _ = ctx.spawn("fuzz-bf5-recv");
    let _ = ctx.spawn("fuzz-bf5");
    let _ = ctx.spawn("fuzz-bf6-recv");
    let _ = ctx.spawn("fuzz-bf6");
    let _ = ctx.spawn("fuzz-bf7-victim");
    let _ = ctx.spawn("fuzz-bf7");
    let _ = ctx.spawn("fuzz-bf1");
    let _ = ctx.spawn("fuzz-bf2");
    let _ = ctx.spawn("fuzz-bf8");

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

    // --- Brutal stress-test probes — Milestone 18 ---
    // Ordering: recv-endpoint victims before their controllers.
    let _ = ctx.spawn("stress-bs1-recv");   // passive saturation target
    let _ = ctx.spawn("stress-bs1");        // 50k try_send
    let _ = ctx.spawn("stress-bs2-victim"); // passive restart victim
    let _ = ctx.spawn("stress-bs2");        // 200 kill/respawn cycles
    let _ = ctx.spawn("stress-bs3-recv");   // core 1 — cross-core thrash receiver
    let _ = ctx.spawn("stress-bs3-send");   // core 0 — 2000 blocking sends
    let _ = ctx.spawn("stress-bs4-victim"); // passive churn victim
    let _ = ctx.spawn("stress-bs4");        // 50 churn cycles; 2 cap slots
    let _ = ctx.spawn("stress-bs5-victim"); // passive generation victim
    let _ = ctx.spawn("stress-bs5");        // 5000 kill/respawn; generation monotonic
    let _ = ctx.spawn("stress-bs6");        // self-referential; 20000 self-ping rounds
    let _ = ctx.spawn("stress-bs7");        // 500 alloc passes
    let _ = ctx.spawn("stress-bs8");        // 3000 yields
    let _ = ctx.spawn("stress-bs9-recv");   // core 2 — IPI storm receiver
    let _ = ctx.spawn("stress-bs9-send-a"); // core 0 → core 2; 2500 sends
    let _ = ctx.spawn("stress-bs9-send-b"); // core 1 → core 2; 2500 sends
    let _ = ctx.spawn("stress-bs10-victim"); // core 1 — cascading revocation victim
    let _ = ctx.spawn("stress-bs10");        // core 0; 50 cycles; 3 cap slots

    // --- Chaos-test probes — Milestone 14 ---
    // c7-victim must be registered on core 2 before chaos-c7 is spawned on core 1
    // so its endpoint exists when chaos-c7's SEND cap is wired at spawn time.
    let _ = ctx.spawn("chaos-c2");
    let _ = ctx.spawn("chaos-c2-monitor");
    let _ = ctx.spawn("chaos-c3");
    let _ = ctx.spawn("chaos-c5");
    let _ = ctx.spawn("chaos-c6-hog");
    let _ = ctx.spawn("chaos-c6-monitor");
    let _ = ctx.spawn("chaos-c7-victim"); // passive recv target — spawned before controller
    let _ = ctx.spawn("chaos-c7");

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

    // --- Brutal performance-benchmark probes — Milestone 19 ---
    // Sender/controller BEFORE echo/recv so endpoints register first.
    // bp5-victim before bp5; bp9-recv before bp9.
    let _ = ctx.spawn("perf-bp1");         // BP1 sender (core 0) — registers endpoint first
    let _ = ctx.spawn("perf-bp1-echo");    // BP1 echo (core 0)
    let _ = ctx.spawn("perf-bp2");         // BP2 sender (core 0)
    let _ = ctx.spawn("perf-bp2-echo");    // BP2 echo (core 1)
    let _ = ctx.spawn("perf-bp3");
    let _ = ctx.spawn("perf-bp4");
    let _ = ctx.spawn("perf-bp5-victim");  // spawned before perf-bp5 so it exists to be killed
    let _ = ctx.spawn("perf-bp5");
    let _ = ctx.spawn("perf-bp7");
    let _ = ctx.spawn("perf-bp8");
    let _ = ctx.spawn("perf-bp9-recv");    // recv registered before sender is wired
    let _ = ctx.spawn("perf-bp9");
    let _ = ctx.spawn("perf-bp10");

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

    // --- Brutal identity test probes — Milestone 15 ---
    // T12 chain: spawn C and B (recv-endpoint) before A (sender), so their
    // endpoints are registered when A's SEND cap to B is wired at spawn time.
    let _ = ctx.spawn("brutal-id-12-c"); // chain endpoint: registered first
    let _ = ctx.spawn("brutal-id-12-b"); // chain middle: registered before 12-a's SEND cap
    let _ = ctx.spawn("brutal-id-12-a"); // chain source: acquires cap to 12-c, sends to 12-b
    // T13 cross-core blocked send: recv must exist before sender's SEND cap is wired.
    // Kill runs independently on core 1 and yields before killing.
    let _ = ctx.spawn("brutal-id-13-recv"); // passive target on core 2
    let _ = ctx.spawn("brutal-id-13-kill"); // kills recv after brief delay on core 1
    let _ = ctx.spawn("brutal-id-13-send"); // fills queue then blocks on core 0
    // T11 self-referential queue: brutal-id-11 sends to itself; any spawn order.
    let _ = ctx.spawn("brutal-id-11");
}
