# tests/qemu/perf/

Performance benchmarks (§22.2, B1-B10). **Complete - 10/10 passing.**
Brutal performance benchmarks (§22.2, BP1-BP10). **Complete - 10/10 passing.**

## Status

All ten regular benchmarks pass in the full suite (`osdev test perf`). All ten brutal benchmarks pass in `osdev test perf-brutal`. A regular run that passes anything REWRITES `tests/qemu/perf/baseline.json`; a brutal run writes `build/tests/12_PERFORMANCE_BRUTAL/baseline.json`, which is not committed.

### Brutal benchmark iteration counts (TCG-calibrated)

| Benchmark | N     | Rationale |
|-----------|-------|-----------|
| BP1       | 100   | ~800ms/round-trip on TCG; 100 × 800ms ≈ 80s < 600s timeout |
| BP2       | 100   | Same as BP1 (cross-core, same TCG cost) |
| BP10      | 200   | `perf-brutal-only` spawns ~30 services; reduced load vs. original full-suite assumption |

## What goes here

Benchmarks for the IPC fast path and syscall paths. Per §20:
- No performance claim is valid without a benchmark in this directory.
- Any change to the IPC fast path (`ipc/`, `syscall/dispatch.rs`) requires a before/after benchmark run.

## Benchmarks (§22 B1-B10)

| ID  | Benchmark                  | Metric                          |
|-----|----------------------------|---------------------------------|
| B1  | `ipc_same_core_roundtrip_latency` | Latency p50/p99/p99.9 (cycles)  |
| B2  | `ipc_cross_core_roundtrip_latency` | Latency p50/p99/p99.9 (cycles)  |
| B3  | `syscall_yield_floor`      | Round-trip cycles (no-op)       |
| B4  | `cap_validation_throughput` | Cycles per cap + gen check      |
| B5  | `spawn_syscall_cost`       | Time supervisor.spawn → "ready" |
| B6  | `restart_kill_plus_spawn_cost` | kill + spawn wall time          |
| B7  | `cap_table_throughput`     | Throughput at 1, 2, 4 cores     |
| B8  | `allocator_throughput`     | Pages/sec under contention      |
| B9  | `message_copy_4kib`        | Cycles for 4 KiB message copy   |
| B10 | `scheduler_decision_cost`  | Cycles for pick-next            |

## Baseline

Results go to `tests/qemu/perf/baseline.json`. **Comparison is manual**: nothing reads the file back, the `regression_threshold_pct: 10` field in it is inert, and no workflow runs the perf suite (CLAUDE.md §22.2). The §7.8 single global lock on the capability table (a `SpinLock`, not an `RwLock`) will surface most visibly in B7 - record the baseline now so the v2 sharding migration has a concrete regression target.

## Windows / TCG harness note

On Windows, QEMU's `-serial file:path` backend holds an exclusive write lock on the serial log file while QEMU is running. `poll_serial` (in `osdev/src/validator.rs`) handles this by retrying read failures until the deadline, then applying the same 600 ms QEMU-flush grace period used for normal content timeouts. The final error reported is always `"timeout - lines not seen: ..."` regardless of whether the failure was a read error or a content miss - this avoids masking real content failures with a confusing "serial file unreadable" message.

B2 and B4 are the benchmarks most sensitive to this: they are spawned late in the supervisor sequence (~165 services ahead of them) and have lower iteration counts, so the serial log is produced close to the timeout boundary.

## Format

Each benchmark runs in QEMU with `-smp 4` and KVM when available. Metrics are emitted on the serial console as `perf: <ID> key=value ...` lines, and `collect_perf_baseline` keeps every integer `key=value` from them, keyed by ID:

```
perf: B1 p50=1240 p99=1890
```
