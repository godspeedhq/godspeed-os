// SPDX-License-Identifier: Apache-2.0
//! reply-server - the request/reply (RPC) IPC pattern (§8, §8.9).
//!
//! The dominant shape of a real GodspeedOS service (`fs`, `block-driver`): own an
//! endpoint, block for a request, do work, send a reply BACK. The twist that makes
//! it a capability system - the server has no ambient way to call anyone. It can
//! reply only because each request carries an embedded REPLY capability (a cap to
//! the client's own endpoint). The server retrieves it with `take_pending_cap()`
//! and answers over it.
//!
//! The one discipline this example exists to teach is §8.9: the reply is sent with
//! `try_send_by_handle` - NON-BLOCKING. A blocking `send` here could wedge the server
//! forever on a slow or dead client (and, if the client were also blocked sending to
//! us, deadlock outright). At least one direction of a mutual exchange MUST use
//! `try_send`; for a server, the reply is that direction.
//!
//! Standalone (no client wired) this service simply blocks on `recv()` - idle, never
//! panicking. That is its graceful degrade. `fs`/`block-driver` are the runnable proof.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("reply-server: ready");

    loop {
        // 1. Block for the next request. With no client wired this simply parks the
        //    service here (idle) - the graceful degrade, never a panic.
        let request = ctx.recv();

        // 2. The request must carry an embedded REPLY capability - a SEND cap to the
        //    client's own endpoint. This is the ONLY authority the server has to call
        //    back: no ambient channel, no identity-based reach (Commandment VII, §7).
        let reply_cap = match ctx.take_pending_cap() {
            Some(cap) => cap,
            None => {
                // A malformed request with no reply cap. Degrade, never panic (§26.7):
                // log it and wait for the next one.
                ctx.log("reply-server: request had no reply cap - dropping it");
                continue;
            }
        };

        // 3. Compute the reply. Here we echo the request payload straight back; a real
        //    server would parse the request and act on it (read a block, open a file).
        //    Policy lives HERE, in the service - the kernel only routes (Commandment X).
        let reply = Message::from_bytes(request.payload_bytes());

        // 4. Send the reply over the embedded cap - NON-BLOCKING (§8.9). A slow or dead
        //    client can never block the server: `try_send_by_handle` returns immediately.
        //    A successful send means QUEUED, not processed (§8.6, Commandment VIII) - if
        //    the client needs an ack it must build one explicitly.
        match ctx.try_send_by_handle(reply_cap, &reply) {
            Ok(())  => ctx.log("reply-server: replied to a request"),
            Err(_)  => ctx.log("reply-server: client unreachable; dropping reply (it must retry)"),
        }

        // 5. The reply cap was installed into our table by `take_pending_cap`; we are
        //    done with it. Reclaim its slot so a long-running server stays bounded and
        //    does not leak cap-table entries over many requests (§26.6).
        ctx.remove_cap(reply_cap);
    }
}
