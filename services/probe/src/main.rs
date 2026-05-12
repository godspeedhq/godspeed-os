//! `probe` — single-binary identity test probe service (§22 Group A).
//!
//! One binary, multiple service_config entries with different `probe_mode` values.
//! The kernel writes `probe_mode` into ServiceContextData at spawn time; the SDK
//! exposes it via `ctx.probe_mode()`.
//!
//! Modes:
//!   0 = PASSIVE         — idle; exists only to be a kill target
//!   1 = ECHO_RECV       — recv one message; log "probe: 3A recv OK"         (Test 3A)
//!   2 = ECHO_SEND       — send to probe-recv; log "probe: 3A send OK"       (Test 3A)
//!   3 = NO_SEND_RIGHT   — try_send via recv-slot cap → CapInsufficientRights (Test 3B)
//!   4 = SEND_AFTER_KILL — kill probe-victim then try_send → EndpointDead     (Test 4A)
//!   5 = FILL_AND_BLOCK  — fill 16-slot queue + blocking send; woken by KILL  (Test 4B)
//!   6 = YIELD_LOGGER    — yield then log; proves preemption/yield path        (Test 8A)
//!   7 = HOG             — tight loop; proves preemption via ping output       (Test 8B)
//!   8 = CAP_FORGE       — try_send on slot 99 (out of range) → CapNotHeld    (Test 9B)

#![no_std]
#![no_main]

use godspeed_sdk::{CapError, CapHandle, IpcError, Message, ServiceContext};

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
