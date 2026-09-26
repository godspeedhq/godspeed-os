// SPDX-License-Identifier: Apache-2.0
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK`s audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
//! reply-server - the request/reply (RPC) IPC pattern (§8, §8.9).
//!
//! The dominant shape of a real GodspeedOS service (`fs`, `block-driver`): own an
//! endpoint, block for a request, do work, send a reply BACK. The twist that makes
//! it a capability system - the server has no ambient way to call anyone. It can
//! reply only because each request carries an embedded REPLY capability (a cap to
//! the client's own endpoint). The server retrieves it with `gs::ipc::take_sent_cap`
//! and answers over it.
//!
//! The one discipline this example exists to teach is §8.9: the reply is sent with
//! `gs::ipc::try_send_to` - NON-BLOCKING. A blocking `send` here could wedge the server
//! forever on a slow or dead client (and, if the client were also blocked sending to
//! us, deadlock outright). At least one direction of a mutual exchange MUST use
//! `try_send`; for a server, the reply is that direction.
//!
//! Standalone (no client wired) this service simply blocks on `recv()` - idle, never
//! panicking. That is its graceful degrade. `fs`/`block-driver` are the runnable proof.

#![no_std]
#![no_main]

use godspeed::{self as gs, ipc::Message, ServiceContext};

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("reply-server: ready");

    loop {
        // 1. Block for the next request. With no client wired this simply parks the
        //    service here (idle) - the graceful degrade, never a panic.
        let request = gs::ipc::recv(&ctx);

        // 2. The request must carry an embedded REPLY capability - a SEND cap to the
        //    client's own endpoint. This is the ONLY authority the server has to call
        //    back: no ambient channel, no identity-based reach (Commandment VII, §7).
        let reply_cap = match gs::ipc::take_sent_cap(&ctx) {
            Some(cap) => cap,
            None => {
                // A malformed request with no reply cap. Degrade, never panic (§26.7):
                // log it and wait for the next one.
                ctx.log("reply-server: request had no reply cap - dropping it");
                continue;
            }
        };

        // Peer-death test hook (Commandment VIII / §8.6 reply-side death-wake). A request of exactly
        // b"HANG" is deliberately NOT answered: we consume its reply cap and loop, leaving the client
        // blocked awaiting a reply that never comes. `osdev test reply-dead` then kills this server and
        // asserts the kernel wakes the blocked client with `ReplyDead` (`request_with_reply` -> None)
        // rather than hanging it forever. This is inert in every other build - no real client sends
        // "HANG" (`asker` sends it only in the reply-test build, once, to drive exactly this test).
        if request.payload_bytes() == b"HANG" {
            ctx.log("reply-server: HANG received - withholding reply (peer-death test hook)");
            gs::cap::remove(&ctx, reply_cap);   // consume the cap but send NO reply - the client stays blocked
            continue;
        }

        // 3. Compute the reply. Here we echo the request payload straight back; a real
        //    server would parse the request and act on it (read a block, open a file).
        //    Policy lives HERE, in the service - the kernel only routes (Commandment X).
        let reply = Message::from_bytes(request.payload_bytes());

        // 4. Send the reply over the embedded cap - NON-BLOCKING (§8.9). A slow or dead
        //    client can never block the server: `gs::ipc::try_send_to` returns immediately.
        //    A successful send means QUEUED, not processed (§8.6, Commandment VIII) - if
        //    the client needs an ack it must build one explicitly.
        match gs::ipc::try_send_to(&ctx, reply_cap, &reply) {
            Ok(())  => ctx.log("reply-server: replied to a request"),
            Err(_)  => ctx.log("reply-server: client unreachable; dropping reply (it must retry)"),
        }

        // 5. The reply cap was installed into our table by `gs::ipc::take_sent_cap`; we are
        //    done with it. Reclaim its slot so a long-running server stays bounded and
        //    does not leak cap-table entries over many requests (§26.6).
        gs::cap::remove(&ctx, reply_cap);
    }
}
