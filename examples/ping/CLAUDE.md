# examples/ping/

Demonstration service — sends one message to `pong` per second (§23.1).

## Milestone role (§23.2)

- Pinned to Core 0.
- `osdev logs ping` shows a send every second.
- After `osdev restart pong`, ping sees `EndpointDead`, looks up via registry, and continues.
- The resumed send crosses to whatever core pong landed on — transparently.

## Cap-rebinding pattern

This service demonstrates the canonical client pattern for handling service restarts (§14.2, §6.B test):

```
loop:
  result = try_send("pong", msg)
  if EndpointDead:
    pong_cap = registry.lookup("pong")  // fresh cap, possibly new core
    retry
  if QueueFull:
    sleep, retry
```

Both `try_send` and `send` are valid here. `try_send` is used because if pong is momentarily busy, it is better to log "queue full" and retry than to block ping indefinitely.
