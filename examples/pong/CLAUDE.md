# examples/pong/

Demonstration service - receives messages from `ping` (§23.1).

## Milestone role (§23.2)

- No `[placement]` in contract → round-robin placement.
- Initially on Core 1; after `osdev restart pong --core 2`, supervisor places it on Core 2.
- `osdev logs pong` shows each received message, proving cross-core IPC works.
- After restart, `osdev logs pong` shows it receiving again on the new core.

## Spawn order

Pong is the **first** service spawned by the supervisor - before ping and before all 178 probe services. This ensures pong's endpoint is registered and ready before ping starts sending, and before probe services compete for scheduler quanta. Cross-core IPC between ping and pong is established within ~10 s of boot.

## Why pong has no placement

The placement-free design exercises the round-robin path and demonstrates that identity (the name "pong") is stable while location (the core) is not (invariant §3.11). After restart, ping gets a fresh cap pointing to the new core but never learns which core that is.

## Resolvable through the kernel name directory

pong does NOT self-register - there is no `ctx.register` call. Instead the kernel **name
directory** (`ipc::names`) records
`"pong" -> its endpoint` synchronously at spawn, and refreshes that entry on every restart with
the fresh endpoint/core. So ping reacquires pong by name through the directory
(`reacquire_by_name("pong")`, a thin shim over `reacquire_cap` / syscall 10) with **no push
from pong** - the restarted pong's new endpoint is already in the directory, so ping's next lookup
resolves to the new instance. pong therefore declares no `ipc_send` peer at all; it only receives.

## Restartability

Pong is stateless - it logs each received message. No state to reconstruct on restart.

## Log strings observed by identity tests

| String                         | Test           |
|--------------------------------|----------------|
| `"pong: ready on core"`        | 9A, 10A        |
| `"pong: ready on core 2"`      | 10A, 10B       |
| `"pong: received"`             | 6A, 6B, 9A, 10A, 10B (indirect) |
