// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `hello` - the minimal GodspeedOS service: it holds only `log_write` and logs
//! a heartbeat. Your first service starts here.
//!
//! Anatomy (the four files every service has):
//!   - Cargo.toml           : the crate; depends on `godspeed` (the standard library)
//!   - build.rs             : links `services/user.ld`, entry point `service_main`
//!   - contracts/hello.toml : declares what this service may do (here: only log)
//!   - src/main.rs          : `service_main`, the function the kernel calls at spawn

#![no_std]
#![no_main]

// The STANDARD LIBRARY, not the SDK. `godspeed` is what an ordinary program is written
// against; `godspeed-sdk` is the layer underneath it, for when the standard library does not
// cover what you are doing (a driver reaching MMIO, say). `ServiceContext` is re-exported
// here, so a first program never has to name the lower layer at all.
use godspeed::ServiceContext;

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
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
