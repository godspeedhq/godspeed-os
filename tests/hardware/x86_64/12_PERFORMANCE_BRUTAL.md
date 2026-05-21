# Hardware: Performance Benchmarks (Brutal)

Mirrors §22 BP1–BP10. Same metrics as `5_PERFORMANCE.md` but at higher iteration counts with only the benchmark probes on the machine — no concurrent probe noise.

**Build mode:** `osdev image --mode perf-brutal` (`perf-brutal-only` supervisor)

**Status: 9/10** — BP1–BP10 except BP2; from second bare-metal boot 2026-05-21 (perf-brutal-only build).

## Benchmarks — BP1–BP10

Expected serial format: `perf: BPX mean=N cycles/...` followed by `perf: BPX done`

| ID | Benchmark | HW result | ~ns at 3 GHz | Date | Status |
|----|-----------|-----------|--------------|------|--------|
| BP1 | IPC same-core round-trip | p50=55,320 p99=14,261,324 cycles | p50 ~18,440 ns | 2026-05-21 | ✅ |
| BP2 | IPC cross-core round-trip | — | — | — | Pending |
| BP3 | Yield floor | 39,903 cycles/yield | ~13,301 ns | 2026-05-21 | ✅ |
| BP4 | Cap validation | 495 cycles/cap-check | ~165 ns | 2026-05-21 | ✅ |
| BP5 | Spawn cost | 8,121,378 cycles/spawn | ~2.7 ms | 2026-05-21 | ✅ |
| BP6 | Restart cost | 14,462,309 cycles/restart | ~4.8 ms | 2026-05-21 | ✅ |
| BP7 | Cap table contention | 1,168 cycles/cap-insert-remove | ~389 ns | 2026-05-21 | ✅ |
| BP8 | Allocator throughput | 616 cycles/alloc-4KiB | ~205 ns | 2026-05-21 | ✅ |
| BP9 | Message copy 4 KiB | 20,073 cycles/4KiB-send | ~6,691 ns | 2026-05-21 | ✅ |
| BP10 | Scheduler decision | 2,323 cycles/yield | ~774 ns | 2026-05-21 | ✅ |

## BP2 pending — cross-core blocking IPC investigation

BP2 did not emit `perf: BP2 done` in either hardware boot. All other 9 benchmarks completed.

BP1 (same-core, identical code pattern) completed cleanly (p50=55,320 cycles). BP2 uses the same sender→echo→sender ping-pong, but echo is on core 1 (cross-core). The blocking `ctx.send` in bp2-echo back to the sender may be stalling under concurrent BP5 kill/spawn IPI traffic on cores 0/1.

Ping/pong (also cross-core) uses `try_send`, which is non-blocking. BP2 uses blocking `send` in the echo reply path. A dedicated boot with only BP2 probes would isolate whether the blocking cross-core reply is the issue.

## Boot history

### Run 1 — 2026-05-21 (full supervisor build)

Core 0 carried 20+ tasks; BP1/2/5/6/9 did not complete. Results were noisy.

### Run 2 — 2026-05-21 (perf-brutal-only build)

Identity probes excluded by the `perf-brutal-only` cfg fix (supervisor split block). 9/10 benchmarks completed. BP2 cross-core round-trip remains pending.

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

Once all 10 results are collected, commit to `tests/hardware/x86_64/baseline_brutal.json`.

Current partial baseline (9/10 — BP2 pending):

```json
{
  "hw_cpu": "x86_64 ~3GHz 4-core",
  "date": "2026-05-21",
  "BP1": { "p50_cycles": 55320, "p99_cycles": 14261324, "p999_cycles": 14261324 },
  "BP2": { "p50_cycles": 0, "p99_cycles": 0, "p999_cycles": 0 },
  "BP3": { "mean_cycles": 39903 },
  "BP4": { "mean_cycles": 495 },
  "BP5": { "mean_cycles": 8121378 },
  "BP6": { "mean_cycles": 14462309 },
  "BP7": { "mean_cycles": 1168 },
  "BP8": { "mean_cycles": 616 },
  "BP9": { "mean_cycles": 20073 },
  "BP10": { "mean_cycles": 2323 }
}
```

The QEMU brutal baseline is at `tests/qemu/12_PERFORMANCE_BRUTAL/baseline.json`. The hardware file is the source of truth for absolute timing claims in spec and documentation.

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| 2026-05-21 | 5/10 (BP3, BP4, BP7, BP8, BP10) | Full build — core 0 overloaded; BP1/2/5/6/9 cut short |
| 2026-05-21 | 9/10 (all except BP2) | perf-brutal-only build — cross-core blocking IPC (BP2) did not complete |
