# examples/pong/

Demonstration service — receives messages from `ping` (§23.1).

## Milestone role (§23.2)

- No `[placement]` in contract → round-robin placement.
- Initially on Core 1; after `osdev restart pong --core 2`, supervisor places it on Core 2.
- `osdev logs pong` shows each received message, proving cross-core IPC works.
- After restart, `osdev logs pong` shows it receiving again on the new core.

## Spawn order

Pong is the **first** service spawned by the supervisor — before ping and before all 178 probe services. This ensures pong's endpoint is registered and ready before ping starts sending, and before probe services compete for scheduler quanta. Cross-core IPC between ping and pong is established within ~10 s of boot.

## Why pong has no placement

The placement-free design exercises the round-robin path and demonstrates that identity (the name "pong") is stable while location (the core) is not (invariant §3.11). After restart, ping gets a fresh cap pointing to the new core but never learns which core that is.

## Restartability

Pong is stateless — it logs each received message. No state to reconstruct on restart.

## Log strings observed by identity tests

| String                         | Test           |
|--------------------------------|----------------|
| `"pong: ready on core"`        | 9A, 10A        |
| `"pong: ready on core 2"`      | 10A, 10B       |
| `"pong: received"`             | 6A, 6B, 9A, 10A, 10B (indirect) |
