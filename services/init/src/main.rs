//! `init` — PID 1 equivalent. TCB member (§6.1).
//!
//! Responsibilities (§11.1):
//!   1. Spawn `supervisor` on Core 0.
//!   2. Spawn `registry` on Core 0.
//!   3. Spawn `logger` on Core 0.
//!   4. Signal readiness: print "init: ready".
//!   5. Enter a wait loop; failure of init causes kernel panic (§6.2).
//!
//! init does NOT restart services — that is supervisor's sole authority (§14).
//! init also does NOT hold restart capability; it only holds spawn capability
//! for the three services listed above.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Result};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Spawn TCB services in the order defined by §11.1.
    ctx.spawn("supervisor").expect("init: supervisor spawn failed — kernel should panic");
    ctx.spawn("registry").expect("init: registry spawn failed — kernel should panic");
    ctx.spawn("logger").expect("init: logger spawn failed — will retry");

    ctx.log("init: ready");

    // init never exits. If it does, the kernel panics (§6.2).
    loop {
        ctx.yield_cpu();
    }
}
