// SPDX-License-Identifier: Apache-2.0
//! `hello` - the minimal GodspeedOS service: it holds only `log_write` and logs
//! a heartbeat. Your first service starts here.
//!
//! Anatomy (the four files every service has):
//!   - Cargo.toml           : the crate; depends on `godspeed-sdk`
//!   - build.rs             : links `services/user.ld`, entry point `service_main`
//!   - contracts/hello.toml : declares what this service may do (here: only log)
//!   - src/main.rs          : `service_main`, the function the kernel calls at spawn

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // The kernel handed us a `ServiceContext`: the ONE gateway to every OS
    // operation. We can only do what our contract granted - here, just log.
    // There is no ambient authority (Commandment VII).
    ctx.log("hello: starting");
    ctx.log("hello: I hold only the log_write capability (no ambient authority)");

    let mut ticks: u64 = 0;
    loop {
        ticks += 1;
        if ticks == 1 {
            ctx.log("hello: alive; yielding the CPU each tick");
        }
        // Cooperative yield. Preemption (the 10 ms quantum, CLAUDE.md §9.1) happens
        // regardless; `yield_cpu` is advisory - never rely on timing for correctness
        // (Commandment VIII). A real service would block on `recv` here instead.
        ctx.yield_cpu();
    }
}
