//! `logger` — structured log sink (§11.4). Restartable.
//!
//! Phase 4: log "logger: ready" and enter the scheduler loop.
//!
//! Phase 5 will add:
//!   - `ctx.drain_kernel_ring_buffer()` on startup.
//!   - Receive loop for `log_write` messages from other services.
//!   - Write formatted lines to serial.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("logger: ready");

    loop {
        ctx.yield_cpu();
    }
}
