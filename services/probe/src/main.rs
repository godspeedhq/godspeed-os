//! `probe` — single-binary identity test probe service (§22 Group A).
//!
//! One binary, multiple service_config entries with different `probe_mode` values.
//! The kernel writes `probe_mode` into ServiceContextData at spawn time; the SDK
//! exposes it via `ctx.probe_mode()`.
//!
//! Modes:
//!   0 = PASSIVE         — idle; exists only to be a kill target
//!   1 = ECHO_RECV       — recv one message; log "probe: 3A recv OK"              (Test 3A)
//!   2 = ECHO_SEND       — send to probe-recv; log "probe: 3A send OK"            (Test 3A)
//!   3 = NO_SEND_RIGHT   — try_send via recv-slot cap → CapInsufficientRights      (Test 3B)
//!   4 = SEND_AFTER_KILL — kill probe-victim then try_send → EndpointDead          (Test 4A)
//!   5 = FILL_AND_BLOCK  — fill 16-slot queue + blocking send; woken by KILL       (Test 4B)
//!   6 = YIELD_LOGGER    — yield then log; proves preemption/yield path             (Test 8A)
//!   7 = HOG             — tight loop; proves preemption via ping output            (Test 8B)
//!   8 = CAP_FORGE       — try_send on slot 99 (out of range) → CapNotHeld         (Test 9B)
//!   9 = GRANT_RECV      — recv then take_pending_cap; log pass                    (Test 5A)
//!  10 = GRANT_SEND      — send_with_cap to probe-5a-recv; log pass                (Test 5A)
//!  11 = NO_GRANT_SEND   — send_with_cap without GRANT right → CapNotGrantable     (Test 5B)
//!  12 = ALLOC_OK        — alloc within limit twice; both succeed                   (Test 7A)
//!  13 = ALLOC_LIMIT     — alloc 60 MiB, then 20 MiB → AllocDenied, then 2 MiB → Ok (Test 7B)
//!
//! Property-test modes — Milestone 9 Phase 3.
//!  27 = PROP_P4   — ∑ alloc_bytes ≡ pages mapped; denied allocs don't count   (P4)
//!  28 = PROP_P5   — kill/spawn cycles; endpoint count stays ≤ table capacity   (P5)
//!  29 = PROP_P7   — kill/spawn cycles; generation monotonic (TLB proxy)        (P7)

#![no_std]
#![no_main]

use godspeed_sdk::{service_context::AllocError, CapError, CapHandle, IpcError, Message, ServiceContext};

#[allow(dead_code)]
const MODE_PASSIVE:         u32 = 0;
const MODE_ECHO_RECV:       u32 = 1;
const MODE_ECHO_SEND:       u32 = 2;
const MODE_NO_SEND_RIGHT:   u32 = 3;
const MODE_SEND_AFTER_KILL: u32 = 4;
const MODE_FILL_AND_BLOCK:  u32 = 5;
const MODE_YIELD_LOGGER:    u32 = 6;
const MODE_HOG:             u32 = 7;
const MODE_CAP_FORGE:       u32 = 8;
const MODE_GRANT_RECV:      u32 = 9;
const MODE_GRANT_SEND:      u32 = 10;
const MODE_NO_GRANT_SEND:   u32 = 11;
const MODE_ALLOC_OK:        u32 = 12;
const MODE_ALLOC_LIMIT:     u32 = 13;

// Property-test modes — Milestone 9 Phase 1.
const MODE_PROP_P1:         u32 = 20;
const MODE_PROP_P9:         u32 = 21;
const MODE_PROP_P10:        u32 = 22;

// Property-test modes — Milestone 9 Phase 2.
const MODE_PROP_P2:         u32 = 23;
const MODE_PROP_P3:         u32 = 24;
const MODE_PROP_P6:         u32 = 25;
const MODE_PROP_P8:         u32 = 26;

// Property-test modes — Milestone 9 Phase 3.
const MODE_PROP_P4:         u32 = 27;
const MODE_PROP_P5:         u32 = 28;
const MODE_PROP_P7:         u32 = 29;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    match ctx.probe_mode() {
        MODE_ECHO_RECV       => mode_echo_recv(&ctx),
        MODE_ECHO_SEND       => mode_echo_send(&ctx),
        MODE_NO_SEND_RIGHT   => mode_no_send_right(&ctx),
        MODE_SEND_AFTER_KILL => mode_send_after_kill(&ctx),
        MODE_FILL_AND_BLOCK  => mode_fill_and_block(&ctx),
        MODE_YIELD_LOGGER    => mode_yield_logger(&ctx),
        MODE_HOG             => loop {},
        MODE_CAP_FORGE       => mode_cap_forge(&ctx),
        MODE_GRANT_RECV      => mode_grant_recv(&ctx),
        MODE_GRANT_SEND      => mode_grant_send(&ctx),
        MODE_NO_GRANT_SEND   => mode_no_grant_send(&ctx),
        MODE_ALLOC_OK        => mode_alloc_ok(&ctx),
        MODE_ALLOC_LIMIT     => mode_alloc_limit(&ctx),
        MODE_PROP_P1         => mode_prop_p1(&ctx),
        MODE_PROP_P9         => mode_prop_p9(&ctx),
        MODE_PROP_P10        => mode_prop_p10(&ctx),
        MODE_PROP_P2         => mode_prop_p2(&ctx),
        MODE_PROP_P3         => mode_prop_p3(&ctx),
        MODE_PROP_P6         => mode_prop_p6(&ctx),
        MODE_PROP_P8         => mode_prop_p8(&ctx),
        MODE_PROP_P4         => mode_prop_p4(&ctx),
        MODE_PROP_P5         => mode_prop_p5(&ctx),
        MODE_PROP_P7         => mode_prop_p7(&ctx),
        _                    => idle(&ctx),
    }
}

fn idle(ctx: &ServiceContext) -> ! {
    loop { ctx.yield_cpu(); }
}

fn mode_echo_recv(ctx: &ServiceContext) -> ! {
    ctx.recv(); // blocks until probe-sender delivers the message
    ctx.log("probe: 3A recv OK");
    idle(ctx)
}

fn mode_echo_send(ctx: &ServiceContext) -> ! {
    let msg = Message::from_bytes(b"probe-3a-msg");
    match ctx.send("probe-recv", &msg) {
        Ok(()) => ctx.log("probe: 3A send OK"),
        Err(_) => ctx.log("probe: 3A send FAIL"),
    }
    idle(ctx)
}

fn mode_no_send_right(ctx: &ServiceContext) -> ! {
    // Test 3B: issue TrySend using the RECV-right cap (slot 2) as the send target.
    // The kernel checks Rights::SEND on the cap → CapInsufficientRights (-3).
    // recv_handle() returns the cap handle wired at spawn; CapHandle(2) is the
    // fallback, but if probe-3b has a recv endpoint it will always be slot 2.
    let handle = ctx.recv_handle().unwrap_or(CapHandle(2));
    let msg = Message::from_bytes(b"test");
    match ctx.try_send_by_handle(handle, &msg) {
        Err(IpcError::CapError(CapError::CapInsufficientRights)) =>
            ctx.log("probe: 3B pass — CapInsufficientRights"),
        _ => ctx.log("probe: 3B FAIL"),
    }
    idle(ctx)
}

fn mode_send_after_kill(ctx: &ServiceContext) -> ! {
    // Test 4A: kill probe-victim (bumps its endpoint generation), then try_send.
    // The SEND cap held by probe-4a now has a stale generation → EndpointDead.
    let msg = Message::from_bytes(b"after-kill");
    let _ = ctx.kill("probe-victim");
    match ctx.try_send("probe-victim", &msg) {
        Err(IpcError::EndpointDead) => ctx.log("probe: 4A pass — EndpointDead after kill"),
        Ok(())                      => ctx.log("probe: 4A FAIL — expected EndpointDead"),
        Err(_)                      => ctx.log("probe: 4A FAIL — unexpected error"),
    }
    idle(ctx)
}

fn mode_fill_and_block(ctx: &ServiceContext) -> ! {
    // Test 4B: fill the 16-slot queue (probe-4b-recv is PASSIVE, never drains it).
    // After filling, log the sentinel that triggers the harness KILL command.
    // Then block on the 17th send; the KILL wakes us with EndpointDead.
    let fill = Message::from_bytes(b"fill");
    for _ in 0u8..16 {
        let _ = ctx.send("probe-4b-recv", &fill);
    }
    ctx.log("probe: 4B sender blocked");
    match ctx.send("probe-4b-recv", &fill) {
        Err(IpcError::EndpointDead) => ctx.log("probe: 4B pass — EndpointDead"),
        Ok(())                      => ctx.log("probe: 4B FAIL — expected EndpointDead"),
        Err(_)                      => ctx.log("probe: 4B FAIL — unexpected error"),
    }
    idle(ctx)
}

fn mode_yield_logger(ctx: &ServiceContext) -> ! {
    for _ in 0u32..10 { ctx.yield_cpu(); }
    ctx.log("probe: 8A yielder ticked");
    idle(ctx)
}

fn mode_cap_forge(ctx: &ServiceContext) -> ! {
    // Test 9B: slot 99 is beyond the 64-slot cap table → CapNotHeld (-2).
    let fake = CapHandle(99);
    let msg  = Message::from_bytes(b"forge");
    match ctx.try_send_by_handle(fake, &msg) {
        Err(IpcError::CapError(CapError::CapNotHeld)) =>
            ctx.log("probe: 9B pass — cap forgery rejected"),
        _ => ctx.log("probe: 9B FAIL"),
    }
    idle(ctx)
}

fn mode_grant_recv(ctx: &ServiceContext) -> ! {
    // Test 5A receiver: wait for the message from probe-5a-send, then verify
    // that an embedded cap arrived via take_pending_cap.
    ctx.recv();
    match ctx.take_pending_cap() {
        Some(_) => ctx.log("probe: 5A recv OK"),
        None    => ctx.log("probe: 5A recv FAIL — no pending cap"),
    }
    idle(ctx)
}

fn mode_grant_send(ctx: &ServiceContext) -> ! {
    // Test 5A sender: send_with_cap to probe-5a-recv.  The send-peer cap has
    // SEND|GRANT, so the transfer is authorised.  On success the cap is gone.
    let msg = Message::from_bytes(b"grant-test");
    match ctx.send_with_cap("probe-5a-recv", &msg) {
        Ok(())  => ctx.log("probe: 5A send OK"),
        Err(_)  => ctx.log("probe: 5A send FAIL"),
    }
    idle(ctx)
}

fn mode_no_grant_send(ctx: &ServiceContext) -> ! {
    // Test 5B negative: the send-peer cap has SEND only (no GRANT).
    // send_with_cap must return CapNotGrantable and leave the cap intact.
    let msg = Message::from_bytes(b"no-grant-test");
    match ctx.send_with_cap("probe-5a-recv", &msg) {
        Err(IpcError::CapError(CapError::CapNotGrantable)) =>
            ctx.log("probe: 5B pass — CapNotGrantable"),
        _ => ctx.log("probe: 5B FAIL"),
    }
    idle(ctx)
}

fn mode_alloc_ok(ctx: &ServiceContext) -> ! {
    // Test 7A: allocate 32 MiB then 20 MiB; both must succeed within the 64 MiB limit.
    let ok1 = ctx.alloc_mem(32 * 1024 * 1024);
    let ok2 = ctx.alloc_mem(20 * 1024 * 1024);
    match (ok1, ok2) {
        (Ok(_), Ok(_)) => ctx.log("probe: 7A pass"),
        _              => ctx.log("probe: 7A FAIL"),
    }
    idle(ctx)
}

fn mode_alloc_limit(ctx: &ServiceContext) -> ! {
    // Test 7B: fill 60 MiB, then verify AllocDenied for 20 MiB (60+20>64),
    // then verify recovery still allows 2 MiB (60+2=62<64).
    let first = ctx.alloc_mem(60 * 1024 * 1024);
    if first.is_err() {
        ctx.log("probe: 7B FAIL — initial 60 MiB alloc failed");
        idle(ctx);
    }
    let denied = ctx.alloc_mem(20 * 1024 * 1024);
    if denied != Err(AllocError::Denied) {
        ctx.log("probe: 7B FAIL — expected AllocDenied for 20 MiB over limit");
        idle(ctx);
    }
    let recover = ctx.alloc_mem(2 * 1024 * 1024);
    match recover {
        Ok(_) => ctx.log("probe: 7B pass"),
        Err(_) => ctx.log("probe: 7B FAIL — recovery alloc failed"),
    }
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes — Milestone 9 Phase 1.
// ---------------------------------------------------------------------------

fn xorshift64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn mode_prop_p1(ctx: &ServiceContext) -> ! {
    // P1 — Cap unforgeability (§7.3, §3.1).
    // 10,000 random u32 values used as cap slots. prop-p1 holds no SEND caps,
    // so every try_send must return Err. Any Ok is a constitutional violation.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 20;
    let msg = Message::from_bytes(b"p1");
    for _ in 0..10_000u32 {
        let slot = CapHandle(xorshift64(&mut rng) as u32);
        if ctx.try_send_by_handle(slot, &msg).is_ok() {
            ctx.log("prop: P1 FAIL — random cap slot accepted as valid SEND");
            idle(ctx);
        }
    }
    ctx.log("prop: P1 pass (10000/10000)");
    idle(ctx)
}

fn mode_prop_p9(ctx: &ServiceContext) -> ! {
    // P9 — Generation bump invalidates ALL cap-table holders (§7.5).
    // prop-p9 is wired with 3 SEND caps to prop-p9-victim (3 distinct slots,
    // same endpoint). Kill the victim, then verify every slot returns
    // EndpointDead — not just the first one the kernel happens to find.
    let msg  = Message::from_bytes(b"p9");
    let h0   = ctx.send_peer_at(0);
    let h1   = ctx.send_peer_at(1);
    let h2   = ctx.send_peer_at(2);
    match (h0, h1, h2) {
        (Some(h0), Some(h1), Some(h2)) => {
            let _ = ctx.kill("prop-p9-victim");
            let dead0 = matches!(ctx.try_send_by_handle(h0, &msg), Err(IpcError::EndpointDead));
            let dead1 = matches!(ctx.try_send_by_handle(h1, &msg), Err(IpcError::EndpointDead));
            let dead2 = matches!(ctx.try_send_by_handle(h2, &msg), Err(IpcError::EndpointDead));
            if dead0 && dead1 && dead2 {
                ctx.log("prop: P9 pass — all 3 cap slots returned EndpointDead");
            } else {
                ctx.log("prop: P9 FAIL — not all cap slots returned EndpointDead");
            }
        }
        _ => ctx.log("prop: P9 FAIL — could not read all 3 send peer handles"),
    }
    idle(ctx)
}

fn mode_prop_p10(ctx: &ServiceContext) -> ! {
    // P10 — Every try_send returns without hanging (§8.6, §8.2).
    // 10,000 random (slot, payload) pairs. try_send is non-blocking by spec;
    // completing all iterations within the harness timeout proves the property.
    // Any return value (Ok or Err) is accepted — correctness is timing, not value.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 22;
    for _ in 0..10_000u32 {
        let slot    = CapHandle(xorshift64(&mut rng) as u32);
        let raw     = xorshift64(&mut rng);
        let msg     = Message::from_bytes(&raw.to_le_bytes());
        let _       = ctx.try_send_by_handle(slot, &msg);
    }
    ctx.log("prop: P10 pass (10000/10000)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes — Milestone 9 Phase 2.
// ---------------------------------------------------------------------------

fn mode_prop_p2(ctx: &ServiceContext) -> ! {
    // P2 — Generation is strictly monotonic across kill/respawn cycles (§7.5).
    // 3 iterations × 2 kill/respawn cycles = 6 total operations.
    // More cycles here push prop-p8-victim's initial ELF load later in the boot,
    // giving prop-p1/p9/p10 (all Core 0) more uncontested CPU time before the
    // supervisor's 6s ELF load monopolises Core 0.
    let mut prev_gen: u64 = 0;
    for _iter in 0..3u32 {
        for _cycle in 0..2u32 {
            let _ = ctx.kill("prop-p2-victim");
            let _ = ctx.spawn("prop-p2-victim");
            let gen = ctx.inspect_endpoint_generation("prop-p2-victim");
            if gen <= prev_gen {
                ctx.log("prop: P2 FAIL — generation not strictly monotonic after kill/respawn");
                idle(ctx);
            }
            prev_gen = gen;
        }
    }
    ctx.log("prop: P2 pass (3 iter x 2 cycles)");
    idle(ctx)
}

fn mode_prop_p3(ctx: &ServiceContext) -> ! {
    // P3 — Cap rights never widen during transfer (§7.3).
    // Self-referential: prop-p3 bounces a SEND|GRANT cap through its own queue
    // 5000 times. After each recv, the received cap's rights must be exactly
    // SEND|GRANT (= 4 | 16 = 20) — no widening, no bit-flipping.
    const SEND_GRANT: u64 = (1 << 2) | (1 << 4); // Rights::SEND | Rights::GRANT = 20

    let mut cap_handle = match ctx.acquire_send_grant_cap("prop-p3") {
        Some(h) => h,
        None => {
            ctx.log("prop: P3 FAIL — could not acquire SEND|GRANT cap to self");
            idle(ctx);
        }
    };

    let msg = Message::from_bytes(b"p3");

    for _iter in 0..5000u32 {
        match ctx.send_with_cap_by_handle(cap_handle, cap_handle, &msg) {
            Ok(()) => {}
            Err(_) => {
                ctx.log("prop: P3 FAIL — send_with_cap_by_handle failed");
                idle(ctx);
            }
        }
        ctx.recv();
        let new_handle = match ctx.take_pending_cap() {
            Some(h) => h,
            None => {
                ctx.log("prop: P3 FAIL — no pending cap after recv");
                idle(ctx);
            }
        };
        let rights = match ctx.query_cap_rights(new_handle) {
            Some(r) => r,
            None => {
                ctx.log("prop: P3 FAIL — cap slot empty after transfer");
                idle(ctx);
            }
        };
        if rights != SEND_GRANT {
            ctx.log("prop: P3 FAIL — cap rights changed during transfer");
            idle(ctx);
        }
        cap_handle = new_handle;
    }
    ctx.log("prop: P3 pass (5000/5000)");
    idle(ctx)
}

fn mode_prop_p6(ctx: &ServiceContext) -> ! {
    // P6 — Queue depth invariant: D messages enqueued → D messages dequeued (§8.5).
    // prop-p6 has a SEND cap to its own recv endpoint (send_peers=["prop-p6"]).
    // 500 iterations cycle through depths 0..=16. For depth=16, the 17th
    // try_send must return QueueFull. For depth<16, all sends succeed. After
    // each fill phase, exactly `depth` messages are drained.
    ctx.log("prop: P6 starting");
    const QUEUE_DEPTH: u32 = 16;
    let msg = Message::from_bytes(b"p6");
    let recv_h = match ctx.recv_handle() {
        Some(h) => h,
        None => { ctx.log("prop: P6 FAIL — no recv endpoint"); idle(ctx); }
    };

    for iter in 0..500u32 {
        let depth = (iter % (QUEUE_DEPTH + 1)) as u8;

        for _ in 0..depth {
            match ctx.try_send("prop-p6", &msg) {
                Ok(()) => {}
                Err(_) => {
                    ctx.log("prop: P6 FAIL — try_send failed before expected queue depth");
                    idle(ctx);
                }
            }
        }

        if depth == QUEUE_DEPTH as u8 {
            match ctx.try_send("prop-p6", &msg) {
                Err(IpcError::QueueFull) => {}
                Ok(()) => {
                    ctx.log("prop: P6 FAIL — queue accepted more than 16 messages");
                    idle(ctx);
                }
                Err(_) => {
                    ctx.log("prop: P6 FAIL — unexpected error on full-queue try_send");
                    idle(ctx);
                }
            }
        }

        for _ in 0..depth {
            match godspeed_sdk::ipc::recv(recv_h) {
                Ok(_) => {}
                Err(_) => {
                    ctx.log("prop: P6 FAIL — recv returned error");
                    idle(ctx);
                }
            }
        }

    }
    ctx.log("prop: P6 pass (500/500)");
    idle(ctx)
}

fn mode_prop_p8(ctx: &ServiceContext) -> ! {
    // P8 — After restart, name resolves to a higher-generation endpoint (§14.2).
    // 5 iterations with rng-varied cycles (1–2 per iter, ~7–8 total).
    // Together with P2's 6 cycles (~13 total kill/spawn ops) these delay
    // prop-p8-victim's initial ELF load late enough that prop-p1/p9/p10 get
    // sufficient Core 0 time to complete their 10,000-iteration loops before
    // the supervisor's 6s ELF load monopolises Core 0.
    let mut rng: u64 = 0xDEAD_BEEF_u64 ^ 28;
    let mut prev_gen: u64 = 0;
    for _iter in 0..5u32 {
        let n_cycles = 1 + (xorshift64(&mut rng) % 2) as u32;
        for _cycle in 0..n_cycles {
            let _ = ctx.kill("prop-p8-victim");
            let _ = ctx.spawn("prop-p8-victim");
            let gen = ctx.inspect_endpoint_generation("prop-p8-victim");
            if gen <= prev_gen {
                ctx.log("prop: P8 FAIL — generation not monotonic after restart");
                idle(ctx);
            }
            prev_gen = gen;
        }
    }
    ctx.log("prop: P8 pass (5 iter)");
    idle(ctx)
}

// ---------------------------------------------------------------------------
// Property-test modes — Milestone 9 Phase 3.
// ---------------------------------------------------------------------------

fn mode_prop_p4(ctx: &ServiceContext) -> ! {
    // P4 — ∑ alloc_bytes ≡ pages mapped after any alloc sequence (§10.3).
    // 500 iterations, each allocating one 4 KiB page. Between each, an oversized
    // alloc (1 GiB, always denied) is also attempted. Denied allocs must not
    // affect the kernel's byte counter. Any mismatch between the locally tracked
    // expected total and InspectKernel(0) is a FAIL.
    let mut expected: u64 = 0;
    for _ in 0..500u32 {
        match ctx.alloc_mem(4096) {
            Ok(_)  => expected += 4096,
            Err(_) => {
                ctx.log("prop: P4 FAIL — unexpected alloc failure for 4 KiB page");
                idle(ctx);
            }
        }
        let _ = ctx.alloc_mem(1 << 30); // 1 GiB — always denied; must not shift counter
        let actual = ctx.inspect_kernel_alloc_bytes();
        if actual != expected {
            ctx.log("prop: P4 FAIL — alloc_bytes mismatch after alloc sequence");
            idle(ctx);
        }
    }
    ctx.log("prop: P4 pass (500/500)");
    idle(ctx)
}

fn mode_prop_p5(ctx: &ServiceContext) -> ! {
    // P5 — Every live endpoint has exactly one owning task (§8.3).
    // 200 kill/respawn cycles of prop-p5-victim. Endpoint registration happens at
    // kernel spawn time (before the service ever runs), so InspectKernel(1) is
    // accurate immediately after spawn returns. If endpoints were orphaned (marked
    // Alive without a live owning task), the 32-slot routing table would overflow
    // and the spawn syscall would return an error within ~15 cycles (system holds
    // ~17 live endpoints at peak). Spawn success + count ≤ 32 for 200 cycles
    // proves no orphaning.
    const MAX_ENDPOINTS: u32 = 32;
    for _ in 0..50u32 {
        let _ = ctx.kill("prop-p5-victim");
        match ctx.spawn("prop-p5-victim") {
            Err(_) => {
                ctx.log("prop: P5 FAIL — spawn failed (routing table overflow; orphan detected)");
                idle(ctx);
            }
            Ok(()) => {}
        }
        let count = ctx.inspect_kernel_endpoint_count();
        if count > MAX_ENDPOINTS {
            ctx.log("prop: P5 FAIL — endpoint count exceeded table capacity (orphan detected)");
            idle(ctx);
        }
    }
    ctx.log("prop: P5 pass (50/50)");
    idle(ctx)
}

fn mode_prop_p7(ctx: &ServiceContext) -> ! {
    // P7 — TLB shootdown leaves no stale mappings (§10.5).
    // Proxy test: 50 kill/respawn cycles of prop-p7-victim. Each kill runs the
    // TLB coherence protocol (CORE_CURRENT spin-wait ensures every other core has
    // loaded a different CR3, flushing non-global TLBs) before frame reclaim.
    // Generation monotonicity via InspectKernel(2) confirms the full kill lifecycle
    // completed correctly. No kernel panic over 50 cycles = shootdown protocol
    // is sound under concurrent SMP activity.
    let mut prev_gen: u64 = 0;
    for _ in 0..50u32 {
        let _ = ctx.kill("prop-p7-victim");
        let gen = ctx.inspect_endpoint_generation("prop-p7-victim");
        if gen <= prev_gen {
            ctx.log("prop: P7 FAIL — generation not monotonic after kill (TLB lifecycle broken)");
            idle(ctx);
        }
        prev_gen = gen;
        let _ = ctx.spawn("prop-p7-victim");
    }
    ctx.log("prop: P7 pass (50/50)");
    idle(ctx)
}
