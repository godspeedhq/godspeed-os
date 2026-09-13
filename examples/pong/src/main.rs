// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `pong` - receives messages from `ping` and logs them.
//!
//! No contract-specified placement → supervisor places via round-robin.
//! Initially on Core 1; after `osdev restart pong --core 2`, may land elsewhere.

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
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
