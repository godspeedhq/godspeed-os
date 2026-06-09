//! `pong` — receives messages from `ping` and logs them.
//!
//! No contract-specified placement → supervisor places via round-robin.
//! Initially on Core 1; after `osdev restart pong --core 2`, may land elsewhere.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log_fmt(format_args!("pong: ready on core {}", ctx.core_id()));

    // Announce our endpoint to the registry so clients can (re)acquire a SEND cap to
    // us by name (H11). Re-runs on every spawn, so after a restart the registry's
    // entry is overwritten with our fresh endpoint — that is how ping's
    // `reacquire_via_registry("pong")` resolves to the new instance/core.
    if ctx.register("pong") {
        ctx.log("pong: registered with registry");
    }

    loop {
        let msg = ctx.recv();
        ctx.log_fmt(format_args!(
            "pong: received \"{}\"",
            core::str::from_utf8(msg.payload_bytes()).unwrap_or("<invalid utf8>")
        ));
    }
}
