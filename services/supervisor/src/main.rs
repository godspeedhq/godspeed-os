//! `supervisor` — restart authority. TCB member (§6.1).
//!
//! Sole holder of the `service_control` capability (§14.4).
//! Responsibilities:
//!   - Read the boot manifest and spawn all non-TCB services (§11.1).
//!   - Monitor services for failure (via kernel death notifications).
//!   - Kill and restart failed services per their restart policy.
//!   - Log all lifecycle events.
//!
//! Supervisor itself is non-restartable. Its failure causes kernel panic (§6.2).
//!
//! Placement decisions use §9.2 rules:
//!   - Contract specifies core → require that core (PlacementInvalid if down).
//!   - No contract core → round-robin.
//!   - On restart: re-evaluate from scratch; previous core NOT remembered.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Result};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("supervisor: reading boot manifest");
    let manifest = ctx.read_boot_manifest();

    for service in manifest.services() {
        match ctx.spawn_service(service) {
            Ok(_)  => ctx.log_fmt(format_args!("supervisor: spawned {}", service.name())),
            Err(e) => ctx.log_fmt(format_args!("supervisor: spawn failed for {}: {:?}", service.name(), e)),
        }
    }

    ctx.log("supervisor: ready");

    loop {
        // Block until a service death notification arrives.
        match ctx.recv_death_notification() {
            Some(dead) => handle_death(&ctx, dead),
            None => ctx.yield_cpu(),
        }
    }
}

fn handle_death(ctx: &ServiceContext, service_name: &str) {
    ctx.log_fmt(format_args!("supervisor: {} died, restarting", service_name));
    if let Err(e) = ctx.restart(service_name, None) {
        ctx.log_fmt(format_args!("supervisor: restart failed for {}: {:?}", service_name, e));
    }
}
