// SPDX-License-Identifier: Apache-2.0
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

use godspeed_sdk::{Message, ServiceContext};

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
    match ctx.send_peer_at(0) {
        Some(sink) => {
            for line in lines.iter() {
                let msg = Message::from_bytes(line.as_bytes());
                // Blocking send: the sink (upper, or the shell as a `| write` sink) wakes and
                // drains each line.
                let _ = ctx.send_by_handle(sink, &msg);
            }
            // End-of-stream marker: a one-byte EOT (0x04). A built-in sink (the shell draining
            // `greet | write file`) recvs until it sees this, so it knows the stream is done
            // without waiting forever. (A zero-length message is not a reliable signal - the
            // IPC path does not deliver an empty body.) A service sink like `upper` just
            // uppercases the control byte harmlessly.
            let _ = ctx.send_by_handle(sink, &Message::from_bytes(&[0x04]));
            ctx.log("greet: sent 3 lines + EOF through the delegated pipe cap");
        }
        None => {
            ctx.log("greet: no pipe cap was delegated - nothing to send to");
        }
    }

    // A pipe stage with no more output just idles (clean exit semantics are a
    // later refinement - see the shell-pipes notes).
    loop {
        ctx.yield_cpu();
    }
}
