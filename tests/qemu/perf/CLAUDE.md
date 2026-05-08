# tests/qemu/perf/

Performance benchmarks (§22.2). **Deferred — not part of the v1 milestone.**

## What goes here (when implemented)

Benchmarks for the IPC fast path and syscall paths. Per §20:
- No perf claim is valid without a benchmark in this directory.
- Any change to the IPC fast path (`ipc/`, `syscall/dispatch.rs`) requires a benchmark before and after.

## v1 status

Empty. The v1 milestone is correctness and identity tests only. Do not add performance optimisations without benchmarks, and do not add benchmarks before the identity tests pass.

## Planned benchmarks

| Benchmark              | What it measures |
|------------------------|-----------------|
| `ipc_same_core_throughput` | Messages/sec, same-core send/recv round-trip |
| `ipc_cross_core_latency`   | Latency (cycles) for one cross-core send + IPI wake |
| `syscall_cap_check`        | Cycles for the cap validation path on a no-op syscall |
| `context_switch`           | Cycles for one context switch on a single core |
