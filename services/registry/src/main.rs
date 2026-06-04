//! `registry` — name → endpoint resolution. TCB member (§6.1).
//!
//! Phase 4: log "registry: ready" and enter the scheduler loop.
//!
//! Phase 5 will add:
//!   - `register(name, endpoint_cap)` — service announces endpoint.
//!   - `lookup(name) -> endpoint_cap` — client gets a fresh cap.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("registry: ready");

    // Stub: register/lookup is not implemented yet (services are wired at spawn),
    // so there is nothing to service. Park instead of busy-yielding so core 0 can
    // reach the idle loop and halt. The Phase-5 recv loop (register/lookup) blocks
    // on its endpoint, which also idles the core.
    ctx.park();
}
