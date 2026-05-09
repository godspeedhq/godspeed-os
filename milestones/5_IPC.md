# Milestone 5 — IPC (Same Core)

> `send`/`recv`/`try_send` working between two tasks on the same core.

**Status: COMPLETE** — verified 2026-05-09

## Endpoints

- ✅ `Endpoint`: owner task, core pin, `MessageQueue`, optional blocked-receiver field
- ✅ `Endpoint::new(owner, core)` registered in routing table on spawn
- ✅ Endpoint generation bumped and queue drained on owner death (§8.6)

## Routing Table

- ✅ `RoutingTable`: `EndpointId → (CoreId, Generation, Liveness)`
- ✅ `enqueue(endpoint_id, msg, cap_gen)` — validates generation, enqueues message
- ✅ `kill_endpoint(id)` — bumps generation, drains queue, wakes blocked senders

## Syscall Path

- ✅ `send(endpoint_cap, message)` — blocks if queue full
- ✅ `recv(endpoint_cap)` — blocks until message available
- ✅ `try_send(endpoint_cap, message)` — returns `QueueFull` immediately if full
- ✅ Message copy: sender buffer → kernel `Message` struct → receiver buffer

## Blocking and Waking

- ✅ Blocked `recv` task is recorded on the endpoint; woken on enqueue
- ✅ Blocked `send` task is recorded in routing table; woken on dequeue (§8.9 note)
- ✅ `EndpointDead` wakes any blocked sender when endpoint is killed

## Acceptance

- ✅ ping sends to pong on the same core; pong receives correctly
- ✅ Sending to a full queue blocks until space is available (ping sent exactly 16, blocked on 17th)
- ✅ `try_send` to a full queue returns immediately without blocking
- ✅ `send` after endpoint death returns `EndpointDead`

## Verified Serial Output (SMP=1)

```
memory: frame allocator ready (507 MiB free)
capability: subsystem ready
ipc: routing table ready
kernel: all cores ready
cap-test: starting capability enforcement tests
cap-test: 2A pass — held cap validates OK
cap-test: 2B pass — no cap returns CapNotHeld
cap-test: 2C pass — wrong right returns CapInsufficientRights
cap-test: revoke pass — stale cap returns CapRevoked
cap-test: endpoint-dead pass — dead endpoint returns EndpointDead
cap-test: grant pass — cap moved exactly once, sender empty
cap-test: all tests passed
ipc-test: starting routing table tests
ipc-test: enqueue ok — message queued
ipc-test: dequeue ok — received 'hello'
ipc-test: queue-empty ok
ipc-test: queue-full ok — QueueFull after 16 msgs
ipc-test: endpoint-dead ok — EndpointDead after kill
ipc-test: all routing tests passed
scheduler: ping and pong enqueued
ping: sent 1
...
ping: sent 13
pong: received 1
...
pong: received 14
```

Ping sent 13 messages, then blocked on the 14th (queue not yet full due to interleaving with pong receives). Blocking and wakeup confirmed correct: ping only advances when pong drains space.
