//! `pong` — demonstration service. Receives messages from `ping`.
//!
//! No contract-specified placement → supervisor places via round-robin.
//! May land on Core 1 initially; may land on a different core after restart.
//! Logs every received message so `osdev logs pong` shows cross-core IPC working.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("pong: ready");

    loop {
        let msg = ctx.recv();
        ctx.log_fmt(format_args!(
            "pong: received \"{}\"",
            core::str::from_utf8(msg.payload_bytes()).unwrap_or("<invalid utf8>")
        ));
    }
}
