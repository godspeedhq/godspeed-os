# examples/ping/

Demonstration service - sends one message to `pong` per second (§23.1).

## Milestone role (§23.2)

- Pinned to Core 0.
- `osdev logs ping` shows a send every second.
- After `osdev restart pong`, ping sees `EndpointDead`, reacquires pong by name through the **kernel name directory** (`reacquire_by_name("pong")`, a thin shim over `reacquire_cap` - the registry service is retired, naming Path C / Phase 4), and continues.
- The resumed send crosses to whatever core pong landed on - transparently.

## Spawn order

Ping is spawned by the supervisor **before** any probe services - second only to pong (pong must precede ping because ping's SEND cap to pong is wired at spawn time). This means ping starts sending within seconds of boot, well before the 178 probe services compete for scheduler quanta on Core 0.

## Cap-rebinding pattern

This service demonstrates the canonical client pattern for handling service restarts (§14.2, §6.B test):

```
loop:
  result = try_send("pong", msg)
  if EndpointDead:
    log("pong endpoint dead, reacquiring via the kernel name directory")
    reacquire_by_name("pong")   // a thin shim over reacquire_cap (syscall 10): looks pong up
                                     // in the kernel NAME DIRECTORY and updates the named-peer cache
                                     // so try_send("pong") uses the fresh cap (possibly on a new
                                     // core). NOT a registry service - that is retired (Path C).
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
| `"ping: pong endpoint dead, reacquiring via the kernel name directory"` | 6B, 10B |
| `"ping: pong cap reacquired, resuming"`         | 6B, 10B |
