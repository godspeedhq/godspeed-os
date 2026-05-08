# Milestone 4 — Capability System

> Capability table enforces rights and generation checks on every syscall.

## Capability Table

- [ ] `CapTable` per task: stores `(ResourceId, Rights, Generation)` entries
- [ ] `insert(cap)` / `lookup(resource_id) -> Option<Capability>`
- [ ] `remove(resource_id)` on transfer (GRANT) or revocation

## Global Resource Table

- [ ] `GlobalResourceTable` tracks `(ResourceId → (Generation, Liveness))`
- [ ] `bump_generation(resource_id)` — invalidates all outstanding caps
- [ ] `mark_dead(resource_id)` — endpoint death path

## Syscall Validation

- [ ] Every syscall validates: cap held + rights match + generation matches
- [ ] Returns `CapNotHeld`, `CapInsufficientRights`, `CapRevoked`, `EndpointDead` correctly
- [ ] Generation check is one atomic comparison (§7.5)

## Capability Transfer

- [ ] `send` with embedded cap checks `GRANT` right
- [ ] Cap removed from sender's table, inserted into receiver's table
- [ ] Returns `CapNotGrantable` if `GRANT` right absent

## Acceptance

- A task without a cap cannot invoke the associated resource (§3.1)
- After generation bump, all outstanding caps return `CapRevoked` on next use
- Cap transfer moves the cap exactly once (not duplicated)
