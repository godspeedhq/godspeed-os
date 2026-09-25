// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `ping` - sends a message to `pong` on every scheduler tick.
//!
//! Pinned to Core 0 (§23.1). On `EndpointDead`, reacquires a fresh SEND cap
//! via the kernel name directory and resumes (§14.2, test 10).

#![no_std]
#![no_main]

use godspeed::{self as gs, ipc::Message, ServiceContext};

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("ping: starting");

    let mut counter: u64 = 0;
    let mut success_count: u64 = 0;

    loop {
        counter += 1;
        let payload = make_payload(counter);
        let msg = Message::from_bytes(&payload[..payload_len(&payload)]);

        match gs::ipc::try_send(&ctx, "pong", &msg) {
            Ok(()) => {
                success_count += 1;
                if success_count == 20 {
                    ctx.log("ping: sent 20 messages");
                }
            }
            // The peer died, or its name does not resolve right now. NOTHING WAS SENT, so there is
            // no half-delivered message to reason about - reacquire and the next tick carries on.
            Err(gs::Error::Unreachable) => {
                ctx.log("ping: pong endpoint dead, reacquiring via the kernel name directory");
                if gs::cap::reacquire(&ctx, "pong") {
                    ctx.log("ping: pong cap reacquired, resuming");
                } else {
                    ctx.log("ping: reacquire failed, retrying next tick");
                }
            }
            // pong is alive and its queue is full. Also nothing sent, but a DIFFERENT situation:
            // going looking for a peer that never went away would be the wrong repair.
            Err(gs::Error::Busy) => {}
            Err(_) => {}
        }

        gs::task::yield_now(&ctx);
    }
}

/// Format the counter as ASCII decimal into a fixed buffer.
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
    // Reverse into buf.
    for j in 0..i {
        buf[j] = tmp[i - 1 - j];
    }
    buf
}

fn payload_len(buf: &[u8; 20]) -> usize {
    buf.iter().position(|&b| b == 0).unwrap_or(20)
}
