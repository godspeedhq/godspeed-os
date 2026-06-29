# Example: reply-server

> **Verified by `osdev test reply-server`** - it spawns reply-server with its client `asker`, drives a
> request carrying an embedded reply cap, and asserts `asker` logs `reply = N (echo OK)` (the reply
> round-tripped back over the cap) and `reply-server: replied to a request`. Run it yourself to re-confirm.

The **request/reply (RPC)** IPC pattern - the dominant shape of a real GodspeedOS service. A client
sends a request and waits for an answer; the server does work and replies. `fs` and `block-driver`
are exactly this. The capability twist: the server has no ambient way to call anyone back - it can
reply only because the client handed it a reply capability.

## Purpose

Show the server side of an RPC end to end, and teach the one IPC discipline the other examples never
show: **§8.9 deadlock avoidance**. Two services that both block-`send` to each other deadlock, and the
kernel will neither detect nor break it. So at least one direction MUST use `try_send`. For a server,
the reply is that direction - it answers with a non-blocking send, so a slow or dead client can never
wedge it.

## What it demonstrates

The server side, using only real `ServiceContext` methods:

| Step | Call | What happens |
|------|------|--------------|
| Own an endpoint | (from the contract's `ipc_receive`) | clients send requests here |
| Block for a request | `ctx.recv()` | returns the next `Message`; idle (parked) when none - the graceful degrade |
| Take the reply cap | `ctx.take_pending_cap()` | the SEND cap the client embedded, now in OUR table - the ONLY way we can call back (§7.10, §8.5) |
| Compute a reply | (service logic) | echo the payload, or its byte length as text - this is policy, and it lives here |
| Reply, non-blocking | `ctx.try_send_by_handle(reply_cap, &reply)` | answers without ever blocking on the client (§8.9) |
| Reclaim the slot | `ctx.remove_cap(reply_cap)` | keeps a long-running server bounded (§26.6) |

A request that arrives with no reply cap is logged and dropped - the server degrades, it never panics
(§26.7).

## The client side (a separate service)

The server is only half the pattern. A client derives a reply cap from its **own** endpoint and embeds
it in the request, then blocks for the answer. This is the cap-embedding mechanism of
`examples/cap-grant`, used for RPC:

```rust
// In the CLIENT's service_main(ctx):
use godspeed_sdk::Message;

let request = Message::from_bytes(b"echo me");

// 1. A reply cap = a SEND|GRANT copy of our OWN endpoint cap. The server replies here.
let self_cap  = ctx.self_grant_handle().expect("client owns an endpoint");
let reply_cap = ctx.derive_cap(self_cap).expect("derive a copy to give away");

// 2. Find the server and send the request WITH the reply cap embedded. The kernel
//    moves reply_cap into the server's table (it carried GRANT) - that is the server's
//    only authority to answer us (Commandment VII).
let server = ctx.acquire_send_cap("reply-server").expect("server registered");
ctx.send_with_cap_by_handle(server, reply_cap, &request).expect("request sent");

// 3. Block on our own endpoint for the reply.
let reply = ctx.recv();
let _ = reply.payload_bytes();   // "echo me"
```

(The SDK packages this exact dance as `ctx.request_with_reply("reply-server", &request)` - read it in
`sdk/rust/src/service_context.rs` to see the same three steps, plus reply-cap reclamation on a failed
send. The block above is spelled out so the mechanism is visible.) The runnable client is
`examples/asker`, spawned next to this server by `osdev test reply-server`.

## Why it is built this way (the Commandments)

- **Commandment VII (no ambient authority).** The server can reply ONLY because the client handed it a
  reply capability. There is no `ipc_send` in the contract, no "reply to whoever called", no identity
  lookup - the cap retrieved by `take_pending_cap` *is* the authority to call back, and nothing else
  grants it. *(COMMANDMENTS.md VII; CLAUDE.md §7, §7.10, §8.5, Invariant 1.)*
- **Commandment VIII (wait on truth, not time).** A successful reply send means the message was
  *queued*, not *processed* (§8.6) - so a protocol needing confirmation builds an explicit ack. And the
  reply uses `try_send`, which waits on no one: the server returns immediately whether or not the client
  is ready, so it can never block on a peer. *(COMMANDMENTS.md VIII; CLAUDE.md §8.6, §8.9.)*
- **Commandment X (place complexity where it belongs).** Request/reply is *policy* - what a request
  means and what answer it deserves - and policy lives in the service. The kernel only routes the
  message and validates the cap; it has no idea this is an "RPC". *(COMMANDMENTS.md X; CLAUDE.md §26.10.)*

### The §8.9 deadlock rule, explicitly

> In any protocol where A and B both send to each other, at least one direction MUST use `try_send`.

The kernel does **not** detect or break deadlocks (§8.9). If the server replied with a blocking `send`
to a client whose reply queue is full - and that client were itself blocked sending its next request to
the server - both would block forever. Using `try_send_by_handle` for the reply removes the cycle: the
server never blocks on the client, so the mutual-blocking deadlock cannot form. The cost is that a reply
to a full/dead client is dropped (returns an error) rather than waited on - which is exactly the
loud-failure trade GodspeedOS wants (§26.7). The client retries; it does not hang.

## What you must NOT do

- **Do not reply with a blocking `send`.** That re-opens the §8.9 deadlock and lets one slow client
  wedge the whole server. Use `try_send_by_handle` for the reply - always.
- **Do not assume the client received the reply because the send returned `Ok`.** `Ok` means queued,
  not processed (§8.6). If you need confirmation, the client must ack explicitly (**Commandment VIII**).
- **Do not invent a way to "reply to the sender" without the embedded cap.** There is none, by design.
  No ambient channel, no sender identity to look up. If a request omits its reply cap, drop it loudly -
  do not reach for authority you were not given (**Commandment VII**).
- **Do not panic on a malformed request.** Log and continue. A server that dies on bad input is a
  denial-of-service waiting to happen; degrade gracefully (§26.7).

## How to adapt this

This is the skeleton of every real GodspeedOS server. Replace step 3 (the echo) with your service
logic: parse the request payload, do the work (read a block, open a file, look up a name), and
`try_send_by_handle` the result back over the embedded reply cap. For richer protocols, badge requests
with an operation code in the payload and branch on it. To make a request *from* the client side, follow
the code block above (or call `ctx.request_with_reply`).

## Status

**Real and QEMU-proven by `osdev test reply-server`.** Its client, `examples/asker`, is spawned
alongside it; asker sends a request carrying an embedded reply cap, reply-server replies over that cap,
and the test asserts the round-trip closed - asker logs `asker: reply = <N> (echo OK)` and reply-server
logs `reply-server: replied to a request`, with no kernel panic. Standalone (no client wired) it still
blocks on `recv()` (idle) - its graceful degrade. The runnable proof of this pattern in production is
`services/fs` and `services/block-driver`.

## See also

- **Commandments VII, VIII, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §8 (IPC), §8.6 (failure semantics - queued, not processed), §8.9 (deadlock avoidance),
  §7.10 (delegated/reply caps), §8.5 (embedded capabilities).
- `services/fs`, `services/block-driver` - real request/reply servers (the runnable proof).
- `examples/ping` + `examples/pong` - one-way IPC, the contrast (a producer with `ipc_send`, no reply).
- `examples/cap-grant` - the embed-a-cap-in-a-message mechanism this RPC is built on.
