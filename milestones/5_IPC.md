# Milestone 5 — IPC (Same Core)

> `send`/`recv`/`try_send` working between two tasks on the same core.

## Endpoints

- [ ] `Endpoint`: owner task, core pin, `MessageQueue`, optional blocked-receiver field
- [ ] `Endpoint::new(owner, core)` registered in routing table on spawn
- [ ] Endpoint generation bumped and queue drained on owner death (§8.6)

## Routing Table

- [ ] `RoutingTable`: `EndpointId → (CoreId, Generation, Liveness)`
- [ ] `enqueue(endpoint_id, msg, cap_gen)` — validates generation, enqueues message
- [ ] `kill_endpoint(id)` — bumps generation, drains queue, wakes blocked senders

## Syscall Path

- [ ] `send(endpoint_cap, message)` — blocks if queue full
- [ ] `recv(endpoint_cap)` — blocks until message available
- [ ] `try_send(endpoint_cap, message)` — returns `QueueFull` immediately if full
- [ ] Message copy: sender buffer → kernel `Message` struct → receiver buffer

## Blocking and Waking

- [ ] Blocked `recv` task is recorded on the endpoint; woken on enqueue
- [ ] Blocked `send` task is recorded in routing table; woken on dequeue (§8.9 note)
- [ ] `EndpointDead` wakes any blocked sender when endpoint is killed

## Acceptance

- ping sends to pong on the same core; pong receives correctly
- Sending to a full queue blocks until space is available
- `try_send` to a full queue returns immediately without blocking
- `send` after endpoint death returns `EndpointDead`
