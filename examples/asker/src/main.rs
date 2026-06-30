// SPDX-License-Identifier: Apache-2.0
//! `asker` - the request/reply (RPC) CLIENT. The request-side counterpart to
//! `examples/reply-server` (the reply side), exactly as `ping` is to `pong`.
//!
//! This is what makes reply-server a REAL, exercised service: asker sends it a
//! request carrying an embedded REPLY capability, blocks for the reply, and checks
//! that what came back is what it sent. The whole round-trip in one call:
//!
//!   ctx.request_with_reply("reply-server", &req)
//!
//! Under the hood (`sdk/rust/src/service_context.rs`) that call derives a per-request
//! reply cap - a SEND|GRANT copy of asker's OWN endpoint cap - embeds it in the
//! request, sends it to reply-server, and blocks on asker's endpoint for the reply.
//! The reply cap is the ONLY authority the server has to call asker back: no ambient
//! channel, no identity-based reach (Commandment VII, §7, §8.5).
//!
//! Commandments this teaches (the client half of request/reply):
//!   VI   - it talks over IPC, never shared memory.
//!   VII  - it hands the server authority to reply by GRANTing a cap derived from its
//!          own endpoint - explicit, minted, non-ambient.
//!   VIII - a successful send is QUEUED, not processed (§8.6); asker then waits for the
//!          REPLY (truth), never for a fixed sleep (time). The generation check, not a
//!          delay, settles a reply-server restart: a stale peer cap returns None, and
//!          asker reacquires by name and retries.
//!   IX   - on a failed exchange (reply-server still spawning, or restarted) asker
//!          reacquires "reply-server" by name via the kernel directory and retries.
//!   X    - the request's meaning is policy in the two services; the kernel only routes.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("asker: starting");

    let mut counter: u64 = 0;

    loop {
        counter += 1;
        let payload = make_payload(counter);
        let req = Message::from_bytes(&payload[..payload_len(&payload)]);

        // The whole RPC round-trip: embed a reply cap, send, block for the reply.
        // None => the peer could not be reached (still spawning, or just restarted),
        // in which case the embedded reply cap was reclaimed for us (no leak, §26.6).
        match ctx.request_with_reply("reply-server", &req) {
            Some(reply) => {
                // THE PROOF of a correct round-trip: the reply echoes the exact request
                // bytes. reply-server is an echo server, so reply == request iff the
                // request reached it AND its reply reached us back over the embedded cap.
                if reply.payload_bytes() == &payload[..payload_len(&payload)] {
                    ctx.log_fmt(format_args!("asker: reply = {} (echo OK)", counter));
                } else {
                    ctx.log("asker: reply MISMATCH - echo did not round-trip");
                }
            }
            None => {
                // reply-server not reachable yet (first ticks of boot) or mid-restart.
                // Reacquire it by name through the kernel directory and retry next tick
                // (§14.3) - wait for truth, not a sleep (Commandment VIII/IX).
                ctx.log("asker: no reply (reply-server unreachable) - reacquiring by name");
                ctx.reacquire_by_name("reply-server");
            }
        }

        ctx.yield_cpu();
    }
}

/// Format the counter as ASCII decimal into a fixed buffer (no heap, §26.6.1).
fn make_payload(n: u64) -> [u8; 20] {
    let mut buf = [0u8; 20];
    let mut tmp = [0u8; 20];
    let mut i = 0;
    let mut v = if n == 0 { 1 } else { n };
    if n == 0 { tmp[0] = b'0'; i = 1; }
    while v > 0 {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    buf
}

fn payload_len(buf: &[u8; 20]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(20)
}
