# Hardware: Performance Benchmarks

Mirrors §22 B1–B10 (regular) and BP1–BP10 (brutal). Real cycle counts on ~3 GHz silicon.

**Hardware baseline supersedes QEMU TCG numbers** for all absolute timing claims. QEMU TCG is used only for regression detection (relative change), not for quoting actual latencies.

## Build modes

| Suite | Command | Supervisor feature |
|-------|---------|-------------------|
| Regular B1–B10 | `osdev image --mode perf` | `perf-only` |
| Brutal BP1–BP10 | `osdev image --mode perf-brutal` | `perf-brutal-only` |

## Regular benchmarks — B1–B10

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

## Brutal benchmarks — BP1–BP10

**Status: 5/10 — first boot 2026-05-21**

Expected serial format: `perf: BPX mean=N cycles/...` followed by `perf: BPX done`

| ID | Benchmark | HW result | ~ns at 3 GHz | Date | Status |
|----|-----------|-----------|--------------|------|--------|
| BP1 | IPC same-core round-trip (brutal) | — | — | — | Pending |
| BP2 | IPC cross-core round-trip (brutal) | — | — | — | Pending |
| BP3 | Yield floor (brutal) | 13,330 cycles | ~4,443 ns | 2026-05-21 | ✅ |
| BP4 | Cap validation (brutal) | 543 cycles | ~181 ns | 2026-05-21 | ✅ |
| BP5 | Spawn cost (brutal) | — | — | — | Pending |
| BP6 | Restart cost (brutal) | — | — | — | Pending |
| BP7 | Cap table contention (brutal) | 1,656 cycles | ~552 ns | 2026-05-21 | ✅ |
| BP8 | Allocator throughput (brutal) | 681 cycles/4KiB | ~227 ns | 2026-05-21 | ✅ |
| BP9 | Message copy 4 KiB (brutal) | — | — | — | Pending |
| BP10 | Scheduler decision (brutal) | 4,850 cycles | ~1,617 ns | 2026-05-21 | ✅ |

## Notes on first run (2026-05-21)

First boot used the full supervisor build (not `perf-brutal-only`), so core 0 was overloaded with 20+ tasks. BP1, BP2, BP5, BP6, BP9 did not complete before the machine rebooted. The 5 that completed had minimal contention.

Next run should use `osdev image --mode perf-brutal` which spawns only BP1–BP10 and their echo/victim pairs. This should give all 10 results in a single boot.

## Flash procedure

```
osdev image --mode perf-brutal
# elevated Cygwin:
dd if=build/os.img of=/dev/sdb bs=4M
# reboot hardware, observe PuTTY
```

## Pass criteria

Each benchmark emits `perf: BPX done` on serial. All 10 must appear before the machine is considered to have completed the suite. No `KERNEL PANIC` allowed.

## Hardware baseline file

Once all 10 brutal results are collected, commit them to `tests/hardware/x86_64/baseline_brutal.json`:

```json
{
  "hw_cpu": "x86_64 ~3GHz 4-core",
  "date": "YYYY-MM-DD",
  "BP3": { "mean_cycles": 13330 },
  "BP4": { "mean_cycles": 543 },
  ...
}
```

The QEMU baseline lives at `tests/qemu/perf/baseline.json` and is used for regression detection. The hardware baseline is the source of truth for absolute timing claims in the spec and documentation.

## Pass record

| Date | Suite | Completed | Notes |
|------|-------|-----------|-------|
| 2026-05-21 | perf-brutal | 5/10 (BP3, BP4, BP7, BP8, BP10) | Full build — core 0 overloaded; BP1/2/5/6/9 cut short |
