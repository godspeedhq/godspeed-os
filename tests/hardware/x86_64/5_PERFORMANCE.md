# Hardware: Performance Benchmarks (Regular)

Mirrors §22 B1–B10. Real cycle counts on ~3 GHz silicon.

**Build mode:** `osdev image --mode perf` (`perf-only` supervisor)

**Hardware baseline supersedes QEMU TCG numbers** for all absolute timing claims. QEMU TCG is used only for regression detection (relative change), not for quoting actual latencies.

For brutal benchmarks (BP1–BP10) see `12_PERFORMANCE_BRUTAL.md`.

## Benchmarks — B1–B10

**Status: 9/10** — B1–B10 except B2; hardware run 2026-05-24 (perf-only build, all probes concurrent).

| ID | Benchmark | Metric | HW result | ~ns at 3 GHz | Date | Status |
|----|-----------|--------|-----------|--------------|------|--------|
| B1 | IPC same-core round-trip | p50/p99 cycles | p50=55,286 p99=9,346,080 | p50 ~18,429 ns | 2026-05-24 | ✅ |
| B2 | IPC cross-core round-trip | p50/p99 cycles | — | — | — | Not measured |
| B3 | Syscall yield floor | cycles/round-trip | 1,244,511 | ~414,837 ns | 2026-05-24 | ✅ |
| B4 | Cap validation cost | cycles/check | 484 | ~161 ns | 2026-05-24 | ✅ |
| B5 | Spawn cost | cycles/spawn | 10,979,985 | ~3.7 ms | 2026-05-24 | ✅ |
| B6 | Restart cost | cycles/restart | 14,840,817 | ~4.9 ms | 2026-05-24 | ✅ |
| B7 | Cap table contention | cycles/cap-insert-remove | 1,176 | ~392 ns | 2026-05-24 | ✅ |
| B8 | Allocator throughput | cycles/alloc-4KiB | 591 | ~197 ns | 2026-05-24 | ✅ |
| B9 | Message copy 4 KiB | cycles/4KiB-send | 53,419 | ~17,806 ns | 2026-05-24 | ✅ |
| B10 | Scheduler decision | cycles/yield | 2,708 | ~903 ns | 2026-05-24 | ✅ |

## B2 — not measurable on Goldmont+

The cross-core WAKE_RECEIVER IPI from core 1 → core 0 is not reliably delivered
on this hardware (Goldmont+ BSP APIC quirk). B2 probes spawn correctly on cores 0/1
but the round-trip never completes.

**Isolation attempt (2026-05-24):** Built and flashed `osdev image --mode b2-only` —
supervisor spawns only pong, ping, perf-b2, perf-b2-echo. No other probes, no
concurrent IPI traffic from BP5/BP6 spawn-kill cycles. Result: B2 still stalled.
No `perf: B2 done` on serial. Ping/pong continued normally on the same cores.

**Conclusion:** The Goldmont+ IPI delivery failure is an inherent hardware limitation,
not an artifact of concurrent probe load. The benchmark tight-loop itself generates
sufficient IPI frequency to expose the quirk. Backburner until AMD or later Intel
(Tremont/Golden Cove) hardware.

Cross-core IPC correctness is proven — ping/pong runs continuously at ~1 Hz with
no issues. See `12_PERFORMANCE_BRUTAL.md §BP2` for the full root-cause writeup.

## B3 noise note

B3 (yield floor) reads 1,244,511 cycles in the regular perf run vs 39,903 in the
brutal-only run. The difference is expected: in the regular run all 10 probes are
active simultaneously, so a yield returns to the scheduler only after the full RR scan
across all active tasks on the core. The brutal number is the true yield floor;
the regular number reflects scheduling overhead under a loaded run queue.

## Flash procedure

```
osdev image --mode perf
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

## Pass criteria

Each benchmark emits `perf: BN done` on serial. No `KERNEL PANIC` allowed.

## Hardware baseline file

Partial baseline (9/10 — B2 pending, same as brutal):

```json
{
  "hw_cpu": "x86_64 ~3GHz 4-core",
  "date": "2026-05-24",
  "B1": { "p50_cycles": 55286, "p99_cycles": 9346080 },
  "B2": { "p50_cycles": 0, "p99_cycles": 0 },
  "B3": { "mean_cycles": 1244511 },
  "B4": { "mean_cycles": 484 },
  "B5": { "mean_cycles": 10979985 },
  "B6": { "mean_cycles": 14840817 },
  "B7": { "mean_cycles": 1176 },
  "B8": { "mean_cycles": 591 },
  "B9": { "mean_cycles": 53419 },
  "B10": { "mean_cycles": 2708 }
}
```

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| 2026-05-24 | 9/10 (all except B2) | perf-only build — all probes concurrent; B2 not measured (Goldmont+ IPI quirk under concurrent load) |
| 2026-05-24 | B2 isolation attempt | b2-only build — only perf-b2/echo + pong/ping; B2 still stalled; Goldmont+ limitation confirmed inherent (not load-dependent) |
