# examples/ping/

Demonstration service - sends messages to `pong`, one per scheduling tick (§23.1 asked for one a second; the loop yields between sends rather than sleeping).

## Milestone role (§23.2)

- Pinned to Core 0.
- `osdev logs ping` shows `ping: starting` and, after the twentieth delivered send, `ping: sent 20 messages`; ping logs no line per send (pong logs each one it receives).
- After `osdev restart pong`, ping sees `EndpointDead`, reacquires pong by name through the **kernel name directory** (`gs::cap::reacquire(&ctx, "pong")`, a thin shim over `reacquire_cap`), and continues.
- The resumed send crosses to whatever core pong landed on - transparently.

## Spawn order

Ping is spawned by the supervisor (in every build except `bare-metal` and the isolation builds) **before** any probe services - second only to pong (pong must precede ping because ping's SEND cap to pong is wired at spawn time). This means ping starts sending within seconds of boot, well before the 193 probe services compete for scheduler quanta on Core 0.

## Cap-rebinding pattern

This service demonstrates the canonical client pattern for handling service restarts (§14.2, §6.B test):

```
loop:
  result = gs::ipc::try_send(&ctx, "pong", &msg)
  if Err(gs::Error::Unreachable):        // pong died or its name does not resolve: NOTHING was sent
    log("pong endpoint dead, reacquiring via the kernel name directory")
    gs::cap::reacquire(&ctx, "pong")   // a thin shim over reacquire_cap (syscall 10): looks pong up
                                       // in the kernel NAME DIRECTORY and updates the named-peer cache
                                       // so try_send("pong") uses the fresh cap (possibly on a new
                                       // core).
    log("pong cap reacquired, resuming")
  if Err(gs::Error::Busy):               // pong is alive and its queue is full: also nothing sent,
    (nothing)                            // and nothing to repair - do not go looking for pong
  yield, then send the NEXT message
```

`try_send` is used (not blocking `send`) so that if pong is momentarily restarting, ping logs it and carries on with the next message rather than blocking indefinitely.

## Log strings observed by identity tests

The following strings appear on the serial console and are matched by validator tests:

| String                                          | Test    |
|-------------------------------------------------|---------|
| `"ping: sent 20 messages"`                      | 8B      |
| `"ping: pong endpoint dead, reacquiring via the kernel name directory"` | 6B, 10B |
| `"ping: pong cap reacquired, resuming"`         | 6B, 10B |
