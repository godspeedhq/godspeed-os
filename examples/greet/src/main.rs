// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `greet` - a pipe producer: emits a few friendly lines, then idles.
//!
//! The producer side of a capability-mediated pipe (`greet | upper`). Crucially,
//! `greet` declares **no** send peers in its contract - it has zero ambient
//! authority to talk to anyone. Its only way to send is the SEND cap the *shell*
//! delegated to it at spawn, which the kernel installs as `send_peers[0]`. So
//! `greet` can only reach exactly the sink the shell wired it to. That is the
//! capability-broker model: authority is granted at composition time, not held.

#![no_std]
#![no_main]

use godspeed::{self as gs, ipc::Message, ServiceContext};

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("greet: ready");

    // Each line carries its own newline: the shell concatenates pipe-stage messages verbatim
    // (it adds no separators), so a producer includes the line breaks in what it sends.
    let lines = [
        "hello from godspeed\n",
        "capability pipes work\n",
        "no ambient authority here\n",
    ];

    // send_peers[0] is the SEND cap the shell granted us to the pipe sink.
    match gs::ipc::peer_at(&ctx, 0) {
        Some(sink) => {
            for line in lines.iter() {
                let msg = Message::from_bytes(line.as_bytes());
                // Blocking send: the sink (upper, or the shell as a `| write` sink) wakes and
                // drains each line.
                let _ = gs::ipc::send_to(&ctx, sink, &msg);
            }
            // End-of-stream marker: a one-byte EOT (0x04). A built-in sink (the shell draining
            // `greet | write file`) recvs until it sees this, so it knows the stream is done
            // without waiting forever. (A zero-length message is not a reliable signal - the
            // IPC path does not deliver an empty body.) A service sink like `upper` just
            // uppercases the control byte harmlessly.
            let _ = gs::ipc::send_to(&ctx, sink, &Message::from_bytes(&[0x04]));
            ctx.log("greet: sent 3 lines + EOF through the delegated pipe cap");
        }
        None => {
            ctx.log("greet: no pipe cap was delegated - nothing to send to");
        }
    }

    // A pipe stage with no more output just idles (clean exit semantics are a
    // later refinement - see the shell-pipes notes).
    loop {
        gs::task::yield_now(&ctx);
    }
}
