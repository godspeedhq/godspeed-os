// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! `asker` - the request/reply (RPC) CLIENT. The request-side counterpart to
//! `examples/reply-server` (the reply side), exactly as `ping` is to `pong`.
//!
//! This is what makes reply-server a REAL, exercised service: asker sends it a
//! request carrying an embedded REPLY capability, blocks for the reply, and checks
//! that what came back is what it sent. The whole round-trip in one call:
//!
//!   gs::call::request_within(&ctx, "reply-server", &req, ASK_SECS)
//!
//! Under the hood (`stdlib/rust/src/call.rs`, over the kernel's `CallDeadline`) that
//! call derives a per-request reply cap - a SEND|GRANT copy of asker's REPLY MAILBOX,
//! the second endpoint the kernel gives a receiving task for replies alone (its own
//! served endpoint only if it got no mailbox) - embeds it in the request, sends it to
//! reply-server, and blocks for the reply until it arrives, the server dies, or
//! `ASK_SECS` passes.
//! The reply cap is the ONLY authority the server has to call asker back: no ambient
//! channel, no identity-based reach (Commandment VII, §7, §8.5).
//!
//! Commandments this teaches (the client half of request/reply):
//!   VI   - it talks over IPC, never shared memory.
//!   VII  - it hands the server authority to reply by GRANTing a cap derived from its
//!          own reply mailbox - explicit, minted, non-ambient.
//!   VIII - a successful send is QUEUED, not processed (§8.6); asker then waits for the
//!          REPLY (truth), never for a fixed sleep (time). The generation check, not a
//!          delay, settles a reply-server restart: a stale peer cap is reacquired by
//!          name and the request sent once more, inside `request_within`.
//!   IX   - on a failed exchange (reply-server still spawning, or restarted) asker
//!          reacquires "reply-server" by name via the kernel directory and retries.
//!   X    - the request's meaning is policy in the two services; the kernel only routes.

#![no_std]
#![no_main]

use godspeed::{self as gs, ipc::Message, ServiceContext};

/// How long an ordinary echo may take. An echo server answers at once; this bounds a wedged one.
const ASK_SECS: i64 = 5;

/// How long the HANG request waits before giving up on its own. Ten minutes, and the size is the
/// point: `osdev test reply-dead` kills the server and allows up to 120 s for the wake (30 s, times 4
/// under TCG), so this must be far outside that window or a deadline expiring could pass for the
/// kernel's `ReplyDead` wake the test exists to prove. Finite, so a server that stays alive and silent
/// still cannot hold asker forever (CLAUDE.md 26.6).
const HANG_SECS: i64 = 600;

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("asker: starting");

    let mut counter: u64 = 0;

    loop {
        counter += 1;

        // Commandment VIII / §8.6 peer-death demonstration (driven by `osdev test reply-dead`). Once,
        // after the round-trip has proven itself, send a request the server deliberately never answers
        // (b"HANG") and block for the reply. If the server is killed while we wait, the kernel wakes us
        // with `ReplyDead` at once, which the library reports as `PeerDied` - the request ARRIVED,
        // so it may have been acted on, and it is not re-sent. We survive it and carry on. The deadline
        // is the other bound: a server that stays alive and silent ends the wait after `HANG_SECS`
        // rather than never. (In the plain reply-server test that is what happens, long after the
        // round-trip has proven itself.)
        if counter == 3 {
            ctx.log("asker: sending HANG - blocking for a reply the server withholds (peer-death test)");
            let hang = Message::from_bytes(b"HANG");
            let asked = gs::driver::wait::Since::now(&ctx);
            match gs::call::request_within(&ctx, "reply-server", &hang, HANG_SECS) {
                Ok(_)  => ctx.log("asker: HANG unexpectedly answered"),
                Err(e) => ctx.log_fmt(format_args!(
                    "asker: HANG woke with no reply after {} ms ({}) - did NOT hang",
                    asked.elapsed_ms(&ctx), e.as_str())),
            }
            gs::cap::reacquire(&ctx, "reply-server");
            gs::task::yield_now(&ctx);
            continue;
        }

        let payload = make_payload(counter);
        let req = Message::from_bytes(&payload[..payload_len(&payload)]);

        // The whole RPC round-trip: embed a reply cap, send, block for the reply.
        // An error => no reply: the peer could not be reached (still spawning, or just
        // restarted), it died holding the request, or the deadline passed. Where the request
        // never left, the embedded reply cap was reclaimed for us (no leak, §26.6); where it
        // was delivered, the cap is the peer's to answer on.
        match gs::call::request_within(&ctx, "reply-server", &req, ASK_SECS) {
            Ok(reply) => {
                // THE PROOF of a correct round-trip: the reply echoes the exact request
                // bytes. reply-server is an echo server, so reply == request iff the
                // request reached it AND its reply reached us back over the embedded cap.
                if reply.payload_bytes() == &payload[..payload_len(&payload)] {
                    ctx.log_fmt(format_args!("asker: reply = {} (echo OK)", counter));
                } else {
                    ctx.log("asker: reply MISMATCH - echo did not round-trip");
                }
            }
            Err(_) => {
                // reply-server not reachable yet (first ticks of boot) or mid-restart.
                // Reacquire it by name through the kernel directory and retry next tick
                // (§14.3) - wait for truth, not a sleep (Commandment VIII/IX).
                ctx.log("asker: no reply (reply-server unreachable) - reacquiring by name");
                gs::cap::reacquire(&ctx, "reply-server");
            }
        }

        gs::task::yield_now(&ctx);
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
