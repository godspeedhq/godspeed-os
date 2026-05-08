# kernel/src/capability/

The capability system (§7). Unsafe boundary: the global resource table uses a raw static; access is serialised by a spinlock (v1: global RwLock per §7.8).

## Files

| File             | Responsibility |
|------------------|---------------|
| `mod.rs`         | Public API: re-exports, `init()` |
| `cap.rs`         | `Capability` struct (ResourceId + Rights + Generation), `validate()`, `narrow_for_grant()`, `CapError` enum |
| `rights.rs`      | `Rights` bitfield: READ, WRITE, SEND, RECV, GRANT, REVOKE |
| `generation.rs`  | `Generation` monotonic counter; `bump()` |
| `table.rs`       | `CapTable` (per-task, 64 slots), `GlobalResourceTable` (kernel-wide) |
| `revoke.rs`      | `revoke(resource_id)`: bumps generation, lazily invalidates all outstanding caps |

## The generation contract (§7.5)

- **Every syscall that touches a resource calls `cap.validate(required_right, current_gen)` before acting.** No exceptions.
- `validate` returns `Err(GenerationMismatch)` on a stale cap. The syscall dispatcher maps this to `CapRevoked` or `EndpointDead` based on whether the resource was explicitly revoked or just died.
- Generation bump is **lazy invalidation**: outstanding caps in remote tasks' tables are NOT deleted. They become stale and fail on next use. This is safe because the generation check is atomic and the bump is visible to all cores after a memory barrier.

## Rights non-escalation (§7.3)

`narrow_for_grant` asserts in debug builds that it does not widen rights. This assert is the mechanical enforcement of the non-escalation property. If you are calling `narrow_for_grant` and the assert fires, the caller is violating the cap model.

## Concurrency (§7.8)

v1: a single global `RwLock` around `GlobalResourceTable`. Reads (cap lookup + gen check) take a read lock; writes (spawn, death, revoke) take a write lock. This is a known bottleneck; sharding is v2 work requiring benchmarks.

Per-task `CapTable` is NOT shared: only one core accesses a given task's table (the task is pinned to that core). No lock needed for `CapTable`.
