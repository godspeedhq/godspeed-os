# Hardware: Performance Benchmarks (Brutal)

Mirrors §22 BP1–BP10. Same metrics as `5_PERFORMANCE.md` but at higher iteration counts with only the benchmark probes on the machine — no concurrent probe noise.

**Build mode:** `osdev image --mode perf-brutal` (`perf-brutal-only` supervisor)

**Status: 5/10** — BP3, BP4, BP7, BP8, BP10 from first bare-metal boot 2026-05-21.

## Benchmarks — BP1–BP10

Expected serial format: `perf: BPX mean=N cycles/...` followed by `perf: BPX done`

| ID | Benchmark | HW result | ~ns at 3 GHz | Date | Status |
|----|-----------|-----------|--------------|------|--------|
| BP1 | IPC same-core round-trip | — | — | — | Pending |
| BP2 | IPC cross-core round-trip | — | — | — | Pending |
| BP3 | Yield floor | 13,330 cycles | ~4,443 ns | 2026-05-21 | ✅ |
| BP4 | Cap validation | 543 cycles | ~181 ns | 2026-05-21 | ✅ |
| BP5 | Spawn cost | — | — | — | Pending |
| BP6 | Restart cost | — | — | — | Pending |
| BP7 | Cap table contention | 1,656 cycles | ~552 ns | 2026-05-21 | ✅ |
| BP8 | Allocator throughput | 681 cycles/4KiB | ~227 ns | 2026-05-21 | ✅ |
| BP9 | Message copy 4 KiB | — | — | — | Pending |
| BP10 | Scheduler decision | 4,850 cycles | ~1,617 ns | 2026-05-21 | ✅ |

## Why 5/10 on first run

First boot (2026-05-21) used the full supervisor build, not `perf-brutal-only`. Core 0 carried 20+ tasks so BP1, BP2, BP5, BP6, BP9 — all dependent on clean IPC or spawn headroom — did not complete before the machine rebooted. The 5 that finished (cap check, yield, cap table, allocator, scheduler) had minimal cross-task contention.

Next run with `osdev image --mode perf-brutal` spawns only BP1–BP10 and their echo/victim pairs (~12 tasks total). All 10 should complete in a single boot.

## Flash procedure

```
osdev image --mode perf-brutal
# elevated Cygwin:
dd if=build/os.img of=/dev/sdb bs=4M
# reboot hardware, observe PuTTY
```

## Pass criteria

All 10 benchmarks emit `perf: BPX done` on serial. No `KERNEL PANIC` allowed.

## Hardware baseline file

Once all 10 results are collected, commit to `tests/hardware/x86_64/baseline_brutal.json`:

```json
{
  "hw_cpu": "x86_64 ~3GHz 4-core",
  "date": "YYYY-MM-DD",
  "BP1": { "p50_cycles": 0, "p99_cycles": 0 },
  "BP2": { "p50_cycles": 0, "p99_cycles": 0 },
  "BP3": { "mean_cycles": 13330 },
  "BP4": { "mean_cycles": 543 },
  "BP5": { "mean_cycles": 0 },
  "BP6": { "mean_cycles": 0 },
  "BP7": { "mean_cycles": 1656 },
  "BP8": { "mean_cycles": 681 },
  "BP9": { "mean_cycles": 0 },
  "BP10": { "mean_cycles": 4850 }
}
```

The QEMU brutal baseline is at `tests/qemu/12_PERFORMANCE_BRUTAL/baseline.json`. The hardware file is the source of truth for absolute timing claims in spec and documentation.

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| 2026-05-21 | 5/10 (BP3, BP4, BP7, BP8, BP10) | Full build — core 0 overloaded; BP1/2/5/6/9 cut short |
