# examples/ping/

Demonstration service — sends one message to `pong` per second (§23.1).

## Milestone role (§23.2)

- Pinned to Core 0.
- `osdev logs ping` shows a send every second.
- After `osdev restart pong`, ping sees `EndpointDead`, looks up via the **registry service** (H11: `reacquire_via_registry("pong")`), and continues.
- The resumed send crosses to whatever core pong landed on — transparently.

## Spawn order

Ping is spawned by the supervisor **before** any probe services — second only to pong (pong must precede ping because ping's SEND cap to pong is wired at spawn time). This means ping starts sending within seconds of boot, well before the 178 probe services compete for scheduler quanta on Core 0.

## Cap-rebinding pattern

This service demonstrates the canonical client pattern for handling service restarts (§14.2, §6.B test):

```
loop:
  result = try_send("pong", msg)
  if EndpointDead:
    log("pong endpoint dead, reacquiring via registry service")
    reacquire_via_registry("pong")   // lookup via the registry service; updates the
                                     // named-peer cache so try_send("pong") uses the
                                     // fresh cap (possibly on a new core). Retries on a
                                     // later tick if pong has not re-registered yet.
    log("pong cap reacquired, resuming")
    retry
  if QueueFull:
    backoff, retry
```

`try_send` is used (not blocking `send`) so that if pong is momentarily restarting, ping logs a retry and continues rather than blocking indefinitely.

## Log strings observed by identity tests

The following strings appear on the serial console and are matched by validator tests:

| String                                          | Test    |
|-------------------------------------------------|---------|
| `"ping: sent 20 messages"`                      | 8B      |
| `"ping: pong endpoint dead, reacquiring via registry service"` | 6B, 10B |
| `"ping: pong cap reacquired, resuming"`         | 6B, 10B |
