# Hardware: Performance Benchmarks (Regular)

Mirrors §22 B1–B10. Real cycle counts on ~3 GHz silicon.

**Build mode:** `osdev image --mode perf` (`perf-only` supervisor)

**Hardware baseline supersedes QEMU TCG numbers** for all absolute timing claims. QEMU TCG is used only for regression detection (relative change), not for quoting actual latencies.

For brutal benchmarks (BP1–BP10) see `12_PERFORMANCE_BRUTAL.md`.

## Benchmarks — B1–B10

**Status: 0/10 — no hardware run yet**

Expected serial format: `perf: BX ...` followed by `perf: BX done`

| ID | Benchmark | Metric | HW result | Date |
|----|-----------|--------|-----------|------|
| B1 | IPC same-core round-trip | p50/p99/p99.9 cycles | — | — |
| B2 | IPC cross-core round-trip | p50/p99/p99.9 cycles | — | — |
| B3 | Syscall yield floor | cycles/round-trip | — | — |
| B4 | Cap validation cost | cycles/check | — | — |
| B5 | Spawn cost | supervisor.spawn → "ready" | — | — |
| B6 | Restart cost | kill + spawn wall time | — | — |
| B7 | Cap table contention | throughput at 1/2/4 cores | — | — |
| B8 | Allocator throughput | pages/sec | — | — |
| B9 | Message copy 4 KiB | cycles/copy | — | — |
| B10 | Scheduler decision | cycles/pick-next | — | — |

## Flash procedure

```
osdev image --mode perf
# elevated Cygwin:
dd if=build/os.img of=/dev/sdb bs=4M
# reboot hardware, observe PuTTY
```

## Pass criteria

Each benchmark emits `perf: BN done` on serial. All 10 must appear. No `KERNEL PANIC` allowed.

## Hardware baseline file

Once all 10 results are collected, commit to `tests/hardware/x86_64/baseline.json`:

```json
{
  "hw_cpu": "x86_64 ~3GHz 4-core",
  "date": "YYYY-MM-DD",
  "B1": { "p50_cycles": 0, "p99_cycles": 0 },
  "B2": { "p50_cycles": 0, "p99_cycles": 0 },
  "B3": { "mean_cycles": 0 },
  ...
}
```

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| — | 0/10 | No hardware run yet |
