//! `supervisor` — restart authority. TCB member (§6.1).
//!
//! Phase 4: log "supervisor: ready" and enter the scheduler loop.
//!
//! Phase 5 will add:
//!   - Reading the boot manifest and spawning non-TCB services (§9.2).
//!   - Monitoring services via kernel death-notification endpoint.
//!   - kill/restart API (§14.4).

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("supervisor: ready");

    loop {
        ctx.yield_cpu();
    }
}
