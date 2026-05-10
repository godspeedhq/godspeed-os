//! `init` — PID 1 equivalent. TCB member (§6.1).
//!
//! Phase 3: log "init: ready" and loop.
//! Phase 4 will add: spawn supervisor, registry, logger via Spawn syscall.
//!
//! init never exits. If it does, the kernel panics (§6.2).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Result};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("init: ready");

    loop {
        ctx.yield_cpu();
    }
}
