// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `logger` - structured log sink (§11.4). Restartable.
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

    // No IPC log protocol yet (services log via the kernel ring buffer, not by sending to this service).
    // But the endpoint EXISTS, so we MUST drain it: otherwise anything sent here - a chaos flood-storm, a
    // stray send - fills the 16-deep queue and it sits at 16/16 FOREVER (a stub that just parks never
    // recv's). Block on recv and drop each message; recv parks the task between messages, so the core
    // still idles (no busy-loop), exactly as the old park did - it just no longer clogs. A real recv loop
    // that decodes + writes log records replaces the drop once the input format lands (see CLAUDE.md).
    loop {
        let _ = ctx.recv();
    }
}
