# tests/qemu/perf/

Performance benchmarks (§22.2, B1–B10). **Complete — 10/10 passing.**

## Status

All ten benchmarks pass in the full suite (`osdev test perf`) and in isolated runs. Results are committed to `baseline.json` and serve as the regression baseline going forward.

## What goes here

Benchmarks for the IPC fast path and syscall paths. Per §20:
- No performance claim is valid without a benchmark in this directory.
- Any change to the IPC fast path (`ipc/`, `syscall/dispatch.rs`) requires a before/after benchmark run.

## Benchmarks (§22 B1–B10)

| ID  | Benchmark                  | Metric                          |
|-----|----------------------------|---------------------------------|
| B1  | `ipc_same_core_roundtrip`  | Latency p50/p99/p99.9 (cycles)  |
| B2  | `ipc_cross_core_roundtrip` | Latency p50/p99/p99.9 (cycles)  |
| B3  | `syscall_yield_floor`      | Round-trip cycles (no-op)       |
| B4  | `cap_validation_cost`      | Cycles per cap + gen check      |
| B5  | `spawn_cost`               | Time supervisor.spawn → "ready" |
| B6  | `restart_cost`             | kill + spawn wall time          |
| B7  | `cap_table_contention`     | Throughput at 1, 2, 4 cores     |
| B8  | `allocator_throughput`     | Pages/sec under contention      |
| B9  | `message_copy_4k`          | Cycles for 4 KiB message copy   |
| B10 | `scheduler_decision`       | Cycles for pick-next            |

## Baseline

Results are committed to `tests/qemu/perf/baseline.json`. CI compares each run against baseline and flags regressions ≥ 10%. The §7.8 single global `RwLock` on the capability table will surface most visibly in B7 — record the baseline now so the v2 sharding migration has a concrete regression target.

## Windows / TCG harness note

On Windows, QEMU's `-serial file:path` backend holds an exclusive write lock on the serial log file while QEMU is running. `poll_serial` (in `osdev/src/validator.rs`) handles this by retrying read failures until the deadline, then applying the same 600 ms QEMU-flush grace period used for normal content timeouts. The final error reported is always `"timeout — lines not seen: ..."` regardless of whether the failure was a read error or a content miss — this avoids masking real content failures with a confusing "serial file unreadable" message.

B2 and B4 are the benchmarks most sensitive to this: they are spawned late in the supervisor sequence (~165 services ahead of them) and have lower iteration counts, so the serial log is produced close to the timeout boundary.

## Format

Each benchmark runs in QEMU with `-smp 4` and KVM when available. Metrics are emitted as structured JSON lines on the serial console and parsed by the harness:

```json
{"benchmark":"ipc_same_core_roundtrip","p50":1240,"p99":1890,"p999":3420,"unit":"cycles"}
```
