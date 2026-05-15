# Milestone 12 — Performance Benchmarks

**Status:** ✅ Phase 1 (10/10) + ✅ Phase 3 Brutal (10/10) — all pass, baselines committed  
**Spec ref:** §22 Performance Benchmarks  
**Commands:** `osdev test perf` · `osdev test perf-brutal`

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

## Baseline results (Phase 1 — Milestone 12)

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

---

## Phase 3 — Brutal Performance Benchmarks (Milestone 19)

**Status:** ✅ 10/10 — all pass

Runs the same 10 benchmarks at 5× iteration counts under the full ~220-task concurrent
probe suite. Validates that the benchmark measurements hold under realistic system load
(every other probe service running simultaneously). `adv-ba8` (tight-loop hog) is pinned
to core 3 to avoid starving IPC/yield probes on cores 0–2.

**Command:** `osdev test perf-brutal`  
**Output:** `build/tests/12_PERFORMANCE_BRUTAL/`  
**Timeout ceiling:** 600 s per test (QEMU TCG variance under 220-task load)

### Brutal benchmark results

| ID   | Name                          | Spec ref             | Status |
|------|-------------------------------|----------------------|--------|
| ✅ BP1  | ipc_same_core_roundtrip_1000  | §22 Brutal Perf BP1  | PASS   |
| ✅ BP2  | ipc_cross_core_roundtrip_1000 | §22 Brutal Perf BP2  | PASS   |
| ✅ BP3  | yield_floor_2000              | §22 Brutal Perf BP3  | PASS   |
| ✅ BP4  | cap_validation_50000          | §22 Brutal Perf BP4  | PASS   |
| ✅ BP5  | spawn_cost_50_cycles          | §22 Brutal Perf BP5  | PASS   |
| ✅ BP6  | restart_cost_50_cycles        | §22 Brutal Perf BP6  | PASS   |
| ✅ BP7  | cap_ir_throughput_5000        | §22 Brutal Perf BP7  | PASS   |
| ✅ BP8  | alloc_throughput_to_limit     | §22 Brutal Perf BP8  | PASS   |
| ✅ BP9  | message_copy_4kib_400         | §22 Brutal Perf BP9  | PASS   |
| ✅ BP10 | scheduler_pick_next_2000      | §22 Brutal Perf BP10 | PASS   |

### Brutal probe services

| Service        | Core   | Mode | Notes                                              |
|----------------|--------|------|----------------------------------------------------|
| perf-bp1       | 0      | 120  | BP1 sender; has recv endpoint                      |
| perf-bp1-echo  | 0      | 121  | BP1 echo; SEND cap to perf-bp1                     |
| perf-bp2       | 0      | 122  | BP2 sender; has recv endpoint                      |
| perf-bp2-echo  | 1      | 123  | BP2 echo; SEND cap to perf-bp2                     |
| perf-bp3       | r-robin| 124  | No peers; 2000 yields                              |
| perf-bp4       | r-robin| 125  | Has recv endpoint; 50,000 cap checks               |
| perf-bp5-victim| r-robin| 0    | Passive; killed/respawned 50 times                 |
| perf-bp5       | r-robin| 126  | BP5+BP6 combined; 50 spawn/restart cycles          |
| perf-bp7       | r-robin| 127  | Has recv endpoint; 5,000 cap I/R cycles            |
| perf-bp8       | r-robin| 128  | No peers; allocs to 64 MiB limit                   |
| perf-bp9-recv  | 0      | 130  | BP9 receiver; drains 400 × 4 KiB messages          |
| perf-bp9       | 0      | 129  | BP9 sender; SEND cap to perf-bp9-recv              |
| perf-bp10      | r-robin| 131  | No peers; 2000 yields (scheduler cost)             |

### Implementation checklist

- ✅ `services/probe/src/main.rs` — modes 120–131 (12 modes, BP1–BP10)
- ✅ `kernel/src/task/mod.rs` — 13 brutal perf service configs (probe_mode 120–131)
- ✅ `services/supervisor/src/main.rs` — brutal perf service spawns
- ✅ `osdev/src/validator.rs` — `BRUTAL_PERF_TESTS`, `run_brutal_perf_tests()`, `collect_brutal_perf_baseline()`
- ✅ `osdev/src/main.rs` — `"perf-brutal"` branch in `cmd_test`
- ✅ `build/tests/12_PERFORMANCE_BRUTAL/baseline.json` — written by harness after first passing run

### Brutal baseline results

`build/tests/12_PERFORMANCE_BRUTAL/baseline.json` — QEMU TCG cycle counts under 220-task load.
Values are higher than Phase 1 due to background probe activity; ratio to Phase 1 reflects system load factor.

| ID   | Metric           | Brutal baseline (cycles) | Phase 1 baseline (cycles) | Load factor |
|------|------------------|--------------------------|---------------------------|-------------|
| BP1  | p50 roundtrip    | 52,358,204               | 51,330,536                | 1.02×       |
| BP1  | p99 roundtrip    | 286,059,685              | 104,634,106               | 2.73×       |
| BP1  | p999 roundtrip   | 350,215,046              | —                         | —           |
| BP2  | p50 roundtrip    | 52,394,940               | 28,077,512                | 1.87×       |
| BP2  | p99 roundtrip    | 234,375,226              | 181,409,927               | 1.29×       |
| BP2  | p999 roundtrip   | 418,680,259              | —                         | —           |
| BP3  | mean yield       | 104,792,428              | 3,505,831                 | 29.9×       |
| BP4  | mean cap check   | 94,831                   | 88,611                    | 1.07×       |
| BP5  | mean spawn       | 24,142,019               | 3,446,155                 | 7.0×        |
| BP6  | mean restart     | 35,250,193               | 31,098,700                | 1.13×       |
| BP7  | mean cap I/R     | 204,566                  | 61,935                    | 3.30×       |
| BP8  | mean alloc       | 75,549                   | 57,919                    | 1.30×       |
| BP8  | n allocs         | 16,384                   | 16,384                    | 1.00×       |
| BP9  | mean send        | 24,940,723               | 5,010,740                 | 4.98×       |
| BP10 | mean pick-next   | 47,651,129               | 6,269,961                 | 7.60×       |
