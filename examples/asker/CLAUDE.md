# Example: asker

> **Verified by `osdev test reply-server`** - asker is the client that proves the pair: it sends
> reply-server a request carrying an embedded reply cap and asserts the echoed reply comes back
> (`asker: reply = N (echo OK)`). Run it yourself to re-confirm.

The **request/reply (RPC) CLIENT** - the request-side counterpart to `examples/reply-server`, exactly
as `ping` is the client to `pong`. asker is what makes reply-server a *real, exercised* service: it
sends reply-server a request carrying an embedded reply capability, blocks for the answer, and checks
that what comes back is what it sent.

> **Verified by `osdev test reply-server`** - it boots asker + reply-server together, drives the
> round-trip, and asserts asker logged `asker: reply = <N> (echo OK)` and reply-server logged
> `reply-server: replied to a request`, with no kernel panic. That pair of lines is the proof the
> request reached the server AND its reply reached the client back over the embedded cap.

## Purpose

Show the **client** half of RPC end to end, so reply-server's server half has something to answer. The
whole round-trip is one SDK call:

```rust
let reply = ctx.request_with_reply("reply-server", &request);   // Some(reply) | None
```

which (see `sdk/rust/src/service_context.rs`) derives a per-request reply cap from asker's OWN endpoint,
embeds it in the request, sends it to reply-server, and blocks on asker's endpoint for the reply.

## What it demonstrates

| Step | Call | What happens |
|------|------|--------------|
| Own an endpoint | (from the contract's `ipc_receive`) | reply-server sends the reply here |
| Build a request | `Message::from_bytes(...)` | the payload (an incrementing decimal here) |
| Round-trip | `ctx.request_with_reply("reply-server", &req)` | derive reply cap from our endpoint, GRANT it embedded in the request, block for the reply |
| Check the echo | `reply.payload_bytes() == request` | the reply must equal the request - proof the round-trip closed |
| Recover | `ctx.reacquire_by_name("reply-server")` | on `None` (peer still spawning / restarted) reacquire by name and retry |

## Why it is built this way (the Commandments)

- **Commandment VII (no ambient authority).** asker grants reply-server the authority to call it back by
  embedding a reply cap - a SEND|GRANT copy of its OWN endpoint cap (`derive_cap` of `self_grant_handle`,
  packaged inside `request_with_reply`). The server gets exactly that cap and nothing else; there is no
  "reply to the sender" channel in the kernel. *(COMMANDMENTS.md VII; CLAUDE.md §7, §8.5, §8.9.)*
- **Commandment VIII (wait on truth, not time).** asker blocks for the *reply* - the truth that the work
  is done - never for a fixed sleep. And it never assumes a send arrived: a successful send is *queued*,
  not processed (§8.6). A reply-server restart is settled by the generation check (a stale peer cap
  returns `None`), not by a delay. *(COMMANDMENTS.md VIII; CLAUDE.md §8.6, §7.5.)*
- **Commandment IX (assume you will be killed; recover by reacquiring).** When reply-server is not yet up,
  or has just restarted, the exchange returns `None`; asker reacquires "reply-server" **by name** through
  the kernel directory and retries on the next tick - it does not hang or die. *(COMMANDMENTS.md IX;
  CLAUDE.md §14.3.)*
- **Commandment X (place complexity where it belongs).** The request's *meaning* is policy in asker and
  reply-server; the kernel only routes the message and validates the cap. *(COMMANDMENTS.md X; §26.10.)*

**Cross-cutting: Commandment II (love Chaos).** asker assumes its peer can vanish mid-exchange: a `None`
is met with reacquire-and-retry, never a panic. That is the same survive-the-kill-storm discipline every
example owes `chaos max-carnage`.

## What you must NOT do

- **Do not block-`send` the request while the server might block replying to you.** That re-opens the
  §8.9 deadlock. `request_with_reply` is safe because the *server's* reply is non-blocking
  (`try_send_by_handle`); the cycle cannot form. *(Commandment VIII.)*
- **Do not treat a returned reply as guaranteed-correct without checking it.** Here asker compares the
  echo to what it sent - the actual proof the round-trip closed, not just that *a* message arrived.
- **Do not reach for the server by identity or a hardcoded endpoint id.** Resolve it by name and embed a
  reply cap; on failure reacquire by name. Authority and addressing are by capability, not ancestry.
  *(Commandment VII/IX.)*
- **Do not panic when the peer is missing.** A `None` is normal during boot and restart - reacquire and
  retry. *(§26.7.)*

## How to adapt this

This is the skeleton of any client that calls a server and needs the answer. Replace the payload (an
incrementing counter) with your real request, and the echo check with parsing the server's reply. For
richer protocols, badge the request payload with an operation code and have the server branch on it
(see `services/fs` for the production version of both halves).

## See also

- **`examples/reply-server`** - the server half this client exercises (read it first).
- **Commandments VII, VIII, IX, X** in `COMMANDMENTS.md`.
- **CLAUDE.md** §8 (IPC), §8.5 (embedded capabilities), §8.6 (queued, not processed), §8.9 (deadlock
  avoidance), §14.3 (reacquire by name on `EndpointDead`).
- `sdk/rust/src/service_context.rs` - `request_with_reply`, `derive_cap`, `self_grant_handle`.
- `examples/ping` + `examples/pong` - the one-way-IPC contrast (a producer, no reply).
