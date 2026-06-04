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

    // Stub: no IPC log draining yet (services log via the kernel ring buffer, not
    // by sending to this service). Park instead of busy-yielding so core 0 can
    // reach the idle loop and halt. A real recv loop replaces this once the logger
    // gains an input endpoint (see CLAUDE.md).
    ctx.park();
}
