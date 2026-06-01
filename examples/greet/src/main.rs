//! `greet` — a pipe producer: emits a few friendly lines, then idles.
//!
//! The producer side of a capability-mediated pipe (`greet | upper`). Crucially,
//! `greet` declares **no** send peers in its contract — it has zero ambient
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

    let lines = [
        "hello from godspeed",
        "capability pipes work",
        "no ambient authority here",
    ];

    // send_peers[0] is the SEND cap the shell granted us to the pipe sink.
    match ctx.send_peer_at(0) {
        Some(sink) => {
            for line in lines.iter() {
                let msg = Message::from_bytes(line.as_bytes());
                // Blocking send: the sink (upper) wakes and drains each line.
                let _ = ctx.send_by_handle(sink, &msg);
            }
            ctx.log("greet: sent 3 lines through the delegated pipe cap");
        }
        None => {
            ctx.log("greet: no pipe cap was delegated — nothing to send to");
        }
    }

    // A pipe stage with no more output just idles (clean exit semantics are a
    // later refinement — see the shell-pipes notes).
    loop {
        ctx.yield_cpu();
    }
}
