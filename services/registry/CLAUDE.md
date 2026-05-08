# services/registry/

Name → endpoint resolution. TCB member (§6.1). **Non-restartable.**

## Why it's in the TCB

When a service is restarted, its clients receive `EndpointDead` and must call `registry.lookup(name)` to get a fresh cap. If the registry is down, that lookup fails and clients cannot recover. The entire cap-rebinding mechanism (§14.2) depends on registry being available at the moment clients try to reconnect.

## Operations

| Opcode      | Args                  | Response |
|-------------|-----------------------|----------|
| `Register`  | name (≤32 bytes), cap | `Ok` |
| `Lookup`    | name (≤32 bytes)      | fresh cap or `NotFound` |

## State model

Registry is **in-memory only** in v1. On restart (which cannot happen in v1 — it's TCB), all entries would be lost. This is acceptable because by the time registry restarts, every other service has already restarted too (system rebooted). v2 might persist registry state to disk via `fs`.

## Re-registration on service restart

When `pong` restarts, it calls `Register("pong", new_cap)`. Registry replaces the old entry (which already has a dead endpoint — the kernel bumped its generation). No conflict because the old cap is already stale.

## What registry does NOT do

- Authenticate callers. Any service can register any name. Access control is the contract system's job.
- Cache or proxy caps. It hands out fresh caps each time `Lookup` is called.
- Persist state across its own restarts (v1).
