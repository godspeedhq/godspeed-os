// SPDX-License-Identifier: Apache-2.0
//! `pong` - receives messages from `ping` and logs them.
//!
//! No contract-specified placement → supervisor places via round-robin.
//! Initially on Core 1; after `osdev restart pong --core 2`, may land elsewhere.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log_fmt(format_args!("pong: ready on core {}", ctx.core_id()));

    // No self-registration. The kernel name-directory records "pong" at spawn and refreshes it on
    // every restart (in place), so ping reacquires us by name through the directory (syscall 10)
    // with no push from us.

    loop {
        let msg = ctx.recv();
        ctx.log_fmt(format_args!(
            "pong: received \"{}\"",
            core::str::from_utf8(msg.payload_bytes()).unwrap_or("<invalid utf8>")
        ));
    }
}
