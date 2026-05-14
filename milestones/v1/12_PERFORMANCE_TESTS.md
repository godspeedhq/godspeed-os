# Milestone 12 — Performance Benchmarks

**Status:** ✅ 10/10 implemented — all pass, baseline committed  
**Spec ref:** §22 Performance Benchmarks  
**Command:** `osdev test perf`

---

## Purpose

Performance benchmarks lock in numbers so regressions are detected commit-to-commit.
Absolute values matter less than deltas.

The benchmarks run inside QEMU (TCG mode). RDTSC cycle counts are collected via
`InspectKernel` query 3 and logged to serial. The harness watches for `perf: BN done`
lines (pass criterion). After all benchmarks pass, extracted metrics are written to
`tests/qemu/perf/baseline.json`.

> **QEMU TCG note:** Cycle counts are not comparable across hosts or QEMU versions.
> They are useful for detecting large regressions within one environment (≥ 10% change
> in the same setup). Absolute performance testing requires bare metal.

---

## Benchmarks

| ID  | Name                              | Metric logged                                    | Mode | Status |
|-----|-----------------------------------|--------------------------------------------------|------|--------|
| B1  | IPC same-core roundtrip latency   | `p50=NNN p99=NNN cycles/roundtrip`               | 60   | ✅     |
| B2  | IPC cross-core roundtrip latency  | `p50=NNN p99=NNN cycles/roundtrip`               | 62   | ✅     |
| B3  | Syscall yield floor               | `mean=NNN cycles/yield`                          | 64   | ✅     |
| B4  | Cap validation throughput         | `mean=NNN cycles/cap-check`                      | 65   | ✅     |
| B5  | Spawn syscall cost                | `spawn_mean=NNN cycles/spawn`                    | 66   | ✅     |
| B6  | Restart (kill+spawn) cost         | `restart_mean=NNN cycles/restart`                | 66   | ✅     |
| B7  | Cap table insert/remove           | `mean=NNN cycles/cap-insert-remove`              | 67   | ✅     |
| B8  | Allocator throughput              | `n=NNN mean=NNN cycles/alloc-4kib`               | 68   | ✅     |
| B9  | 4 KiB message copy cost           | `mean=NNN cycles/4kib-send`                      | 69   | ✅     |
| B10 | Scheduler pick-next cost          | `mean=NNN cycles/yield`                          | 71   | ✅     |

---

## Design

### Timing mechanism

`ctx.read_tsc()` wraps `InspectKernel` syscall 13 with query ID 3. The kernel reads
RDTSC in ring 0 and returns the value. Services cannot call RDTSC directly (no unsafe
in service code per §18.2), so the syscall is the canonical interface.

### B1 / B2 — IPC roundtrip latency

Two probe services form a ping-pong pair. The **sender** dynamically acquires a SEND
cap to the **echo** partner (which registers its endpoint at spawn). The sender sends
N=200 messages and receives N=200 echo replies, recording TSC delta per round-trip.
Samples are insertion-sorted (no_std, O(n²) for N≤200 is acceptable) and p50/p99
percentiles are logged.

- B1: sender and echo both on **core 0** (same-core IPC, no IPI wakeup overhead)
- B2: sender on **core 0**, echo on **core 1** (cross-core IPC, IPI on every wakeup)

B2−B1 isolates the IPI overhead of cross-core wakeups (§8.8).

### B3 — Syscall yield floor

N=1,000 `yield_cpu()` calls are bracketed by RDTSC reads. Mean cycles/yield is reported.
In a busy system this includes scheduler overhead and time other tasks spend running
before control returns — this is the "round-trip" cost as seen by the caller (§9.3).

### B4 — Cap validation throughput

N=10,000 `query_cap_rights()` calls on the service's own recv cap. Each call invokes
the full cap-lookup + generation-check path in ring 0 (§7.5). Mean cycles/check.

### B5 / B6 — Spawn and restart cost

B5 and B6 share one probe (mode 66, `perf-b5`). A victim service (`perf-b5-victim`)
is cycled N=10 times.

- **B5**: kill victim → measure `ctx.spawn()` syscall → kill again → repeat.
  Reports mean cycles for the spawn syscall (task queue insertion, not full service startup).
- **B6**: kill+spawn together as one TSC-bracketed operation → repeat.
  Reports mean cycles for a complete restart (kill + spawn in sequence).

### B7 — Cap table insert/remove throughput

N=1,000 cycles of `acquire_send_cap("perf-b7")` (self-referential, inserting one cap
slot) followed by `remove_cap(handle)`. Measures the cap table allocator and the
generation-mint path (§7.5, §7.8) under no contention (single task, own table).

### B8 — Allocator throughput

Allocates 4 KiB pages via `alloc_mem(4096)` until `AllocDenied` (contract limit
64 MiB → ~16,384 allocs). Total time / successful allocs = mean cycles/alloc.
Memory is reclaimed at service death; no explicit free syscall is needed.

### B9 — 4 KiB message copy cost

`perf-b9` sends N=200 maximum-size (4096-byte) messages to `perf-b9-recv`. Both
services are pinned to **core 0** to isolate copy cost from cross-core IPI overhead.
The kernel copies sender→receiver on every send (zero-copy permanently rejected,
§2.5). Mean cycles/send is the kernel-side memcpy + IPC enqueue cost.

### B10 — Scheduler pick-next cost

Identical measurement to B3 (N=1,000 yields, mean cycles/yield) but tracked
separately for baseline regression detection. In a round-robin scheduler with K
tasks on a core, each yield hands off to the next task and comes back after K×quantum
ms; the per-yield measurement thus captures both pick-next and the scheduling latency
back to this task.

---

## Probe services

| Service         | Core    | Mode | Notes                                    |
|-----------------|---------|------|------------------------------------------|
| perf-b1         | 0       | 60   | B1 sender; has recv endpoint             |
| perf-b1-echo    | 0       | 61   | B1 echo; SEND cap to perf-b1             |
| perf-b2         | 0       | 62   | B2 sender; has recv endpoint             |
| perf-b2-echo    | 1       | 63   | B2 echo; SEND cap to perf-b2            |
| perf-b3         | r-robin | 64   | No peers                                 |
| perf-b4         | r-robin | 65   | Has recv endpoint (cap to validate)      |
| perf-b5-victim  | r-robin | 0    | Passive; killed/respawned by perf-b5     |
| perf-b5         | r-robin | 66   | B5+B6 combined; kills/spawns victim      |
| perf-b7         | r-robin | 67   | Has recv endpoint (self-cap acquisition) |
| perf-b8         | r-robin | 68   | No peers; allocs until limit             |
| perf-b9-recv    | 0       | 70   | B9 receiver; drains 4 KiB messages       |
| perf-b9         | 0       | 69   | B9 sender; SEND cap to perf-b9-recv      |
| perf-b10        | r-robin | 71   | No peers; same as B3 measurement         |

---

## Pass criteria

Each benchmark logs `perf: BN done` on success. The harness passes if all 10 "done"
lines appear within their timeout without a KERNEL PANIC.

**No minimum threshold is enforced** — any number is acceptable as a QEMU baseline.
The stored `tests/qemu/perf/baseline.json` documents the first run's values. Future CI
runs compare against baseline and flag regressions ≥ 10%.

---

## Implementation checklist

- ✅ `kernel/src/syscall/dispatch.rs` — `InspectKernel` query 3 reads RDTSC
- ✅ `sdk/rust/src/service_context.rs` — `ctx.read_tsc()`, `ctx.send_by_handle()`
- ✅ `services/probe/src/main.rs` — modes 60–71 (12 modes across 10 benchmarks)
- ✅ `kernel/src/task/mod.rs` — 13 perf service configs
- ✅ `services/supervisor/src/main.rs` — perf service spawns
- ✅ `osdev/src/validator.rs` — `PERF_TESTS`, `run_perf_tests()`, `run_perf_one()`, `perf_serial_path()`, `collect_perf_baseline()`
- ✅ `osdev/src/main.rs` — `"perf"` branch in `cmd_test`
- ✅ `build/tests/5_PERFORMANCE/.gitkeep`
- ✅ `tests/qemu/perf/baseline.json` — placeholder; updated by harness after first run

---

## Baseline results

`tests/qemu/perf/baseline.json` is committed. Regression threshold: ≥ 10% change flags a failure.
All values are QEMU TCG RDTSC cycle counts — not comparable across hosts or QEMU versions.

| ID  | Metric           | Baseline (cycles) | Notes                        |
|-----|------------------|-------------------|------------------------------|
| B1  | p50 roundtrip    | 51,330,536        | same-core                    |
| B1  | p99 roundtrip    | 104,634,106       | same-core                    |
| B2  | p50 roundtrip    | 28,077,512        | cross-core; includes IPI     |
| B2  | p99 roundtrip    | 181,409,927       | cross-core                   |
| B3  | mean yield       | 3,505,831         | includes scheduler + tasks   |
| B4  | mean cap check   | 88,611            | ring-0 lookup + gen check    |
| B5  | mean spawn       | 3,446,155         | spawn syscall only           |
| B6  | mean restart     | 31,098,700        | kill+spawn syscalls          |
| B7  | mean cap I/R     | 61,935            | insert + remove cap slot     |
| B8  | mean alloc       | 57,919            | 4 KiB page allocation        |
| B8  | n allocs         | 16,384            | total before AllocDenied     |
| B9  | mean send        | 5,010,740         | 4 KiB copy + IPC enqueue     |
| B10 | mean pick-next   | 6,269,961         | yield round-trip             |
