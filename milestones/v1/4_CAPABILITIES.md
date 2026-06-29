# Milestone 4 — Capability System ✅

> Capability table enforces rights and generation checks on every syscall.

**Status: COMPLETE** — 2026-05-09

Serial output:
```
cap-test: starting capability enforcement tests
cap-test: 2A pass — held cap validates OK
cap-test: 2B pass — no cap returns CapNotHeld
cap-test: 2C pass — wrong right returns CapInsufficientRights
cap-test: revoke pass — stale cap returns CapRevoked
cap-test: endpoint-dead pass — dead endpoint returns EndpointDead
cap-test: grant pass — cap moved exactly once, sender empty
cap-test: all tests passed
```

## Capability Table

- ✅ `CapTable` per task: stores `(ResourceId, Rights, Generation)` entries (64 slots)
- ✅ `insert(cap)` / `get(slot, right) -> Result<Capability, CapError>` (returns by value)
- ✅ `remove(slot)` on transfer (GRANT) or revocation

## Global Resource Table

- ✅ `GlobalResourceTable` tracks `(ResourceId → (Generation, Liveness))`
- ✅ `revoke_resource(id)` — sets `Liveness::Revoked`, invalidates caps → `CapRevoked`
- ✅ `mark_dead_resource(id)` — sets `Liveness::Dead`, invalidates caps → `EndpointDead`
- ✅ `mint_cap(id, rights)` — mints a cap at the resource's current generation

## Syscall Validation

- ✅ Log syscall (5) validates `Rights::WRITE` on `LOG_WRITE_RESOURCE` before acting
- ✅ Yield syscall (4) requires no cap (advisory)
- ✅ Returns `CapNotHeld`, `CapInsufficientRights`, `CapRevoked`, `EndpointDead` correctly
- ✅ Generation check is one field comparison in `CapTable::get`
- ✅ `TASK_CAP` parallel array in scheduler; `current_task_lookup_cap` exposes cap table
      to syscall dispatch without unsafe sharing

## Capability Transfer

- ✅ `current_task_remove_cap` / `current_task_insert_cap` implement the GRANT move
- ✅ Cap removed from sender's table, inserted into receiver's table (tested in grant test)
- ✅ Sender has `CapNotHeld` on the slot after transfer

## Acceptance

- ✅ A task without a cap cannot invoke the associated resource (§3.1)
- ✅ After generation bump, all outstanding caps return `CapRevoked` on next use
- ✅ After endpoint death, caps return `EndpointDead` (distinct from `CapRevoked`)
- ✅ Cap transfer moves the cap exactly once (not duplicated)
