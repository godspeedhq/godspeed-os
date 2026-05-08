# examples/pong/

Demonstration service — receives messages from `ping` (§23.1).

## Milestone role (§23.2)

- No `[placement]` in contract → round-robin placement.
- Initially on Core 1; after `osdev restart pong`, supervisor may place it on Core 2.
- `osdev logs pong` shows each received message, proving cross-core IPC works.
- After restart, `osdev logs pong` shows it receiving again on the new core.

## Why pong has no placement

The placement-free design exercises the round-robin path and demonstrates that identity (the name "pong") is stable while location (the core) is not (invariant §3.11). After restart, ping gets a fresh cap pointing to the new core but never learns which core that is.

## Restartability

Pong is stateless — it logs each message and replies nothing. No state to reconstruct. Restart is instant.
