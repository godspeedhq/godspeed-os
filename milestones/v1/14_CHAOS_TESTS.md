# Milestone 14 — Chaos Tests (§22 Chaos C1–C7)

**Status:** ✅ 7/7 implemented — all pass  
**Command:** `osdev test chaos`  
**Evidence:** `build/tests/7_CHAOS/`

---

## Overview

Chaos tests verify that the system degrades **gracefully** when infrastructure the
kernel depends on is partially unavailable or hostile. Total failures — kernel panic,
TCB death — are covered by identity Test 1B. Chaos tests cover the *between* cases:
one core missing, RAM reduced, an allocator under pressure, a service that faults at
startup, a hog monopolising one core, or a storm of TLB shootdowns while IPC is in
flight.

The bar is the same as every other category: **no FAIL, no BLOCKED with a vague
reason.** Every test must produce a concrete serial log showing the system either
continued with degraded capacity or panicked loudly with a defined reason.

---

## Probe Modes Added (91–96)

New modes complement the existing probe binary (single ELF, many configs).

| Mode | Constant            | Description                                                    | Test |
|------|---------------------|----------------------------------------------------------------|------|
|  91  | `CHAOS_C2`          | Null-deref immediately → page fault → killed by kernel         | C2   |
|  92  | `CHAOS_C2_MON`      | 1,000 yields then log pass — proves system continued post-C2   | C2   |
|  93  | `CHAOS_C3`          | 500 alloc-deny cycles (usize::MAX requests) without panic      | C3   |
|  94  | `CHAOS_C5`          | 100-level recursive `yield_cpu()` — kernel stack depth probe   | C5   |
|  95  | `CHAOS_C6_MON`      | 200 yields then log pass on core 0                             | C6   |
|  96  | `CHAOS_C7`          | 30 cross-core kill/respawn cycles triggering TLB shootdowns    | C7   |
| (7)  | `MODE_HOG` (reused) | Tight loop on core 3 — simulates timer-starved core            | C6   |
| (0)  | `MODE_PASSIVE` (reused) | Idle victim for chaos-c7's kill/respawn cycles            | C7   |

---

## Services Added

8 new probe services. Total service count: 97, below `TASK_KSTACK_MAX = 100`.

| Service           | Mode | Core        | Peers               | Purpose                              |
|-------------------|------|-------------|---------------------|--------------------------------------|
| `chaos-c2`        |  91  | round-robin | —                   | Null-deref → killed (C2 attacker)    |
| `chaos-c2-monitor`|  92  | round-robin | —                   | Witnesses C2 death, logs pass        |
| `chaos-c3`        |  93  | round-robin | —                   | Alloc-deny pressure (4 MiB limit)    |
| `chaos-c5`        |  94  | round-robin | —                   | Recursive syscall depth probe        |
| `chaos-c6-hog`    |   7  | core 3      | —                   | Tight loop simulating starved core   |
| `chaos-c6-monitor`|  95  | core 0      | —                   | Cross-core witness after C6 hog      |
| `chaos-c7-victim` |   0  | core 2      | —                   | Passive recv target for C7 kill loop |
| `chaos-c7`        |  96  | core 1      | `chaos-c7-victim`   | 30-cycle cross-core kill/respawn     |

---

## Tests

### C1 — Degraded SMP Boot

**Spec:** §22 Chaos C1  
**What is injected:** QEMU boots with `-smp 2` (2 of 4 cores available).  
**What is verified:**
- Kernel reports "2 cores ready" (not a panic, not silence).
- Supervisor reaches ready state on core 0.
- Services contracted to cores 2 and 3 fail with `PlacementInvalid` (logged, not panicked).
- Cores 0 and 1 continue operating normally.

**Pass string:** `"kernel: 2 cores ready"` + `"supervisor: ready"`  
**Fail on:** `KERNEL PANIC`  
**Harness variant:** `DegradedSmp { smp: 2 }`

---

### C2 — Non-TCB Fault: System Continues

**Spec:** §22 Chaos C2  
**What is injected:** `chaos-c2` (mode 91) dereferences a null pointer immediately on
startup, simulating a service that begins executing corrupted ELF code. The kernel
delivers a page fault and kills the service.  
**What is verified:**
- The kernel kills `chaos-c2` without panicking (non-TCB fault = kill, not halt).
- `chaos-c2-monitor` (mode 92) is separately scheduled, completes 1,000 yields, and
  logs pass — proving the rest of the system is alive and scheduled normally after the fault.

**Pass string:** `"chaos: C2 pass"`  
**Fail on:** `KERNEL PANIC`

---

### C3 — Allocator Saturation: No Panic

**Spec:** §22 Chaos C3  
*(Approximated: kernel alloc-fault injection is not implemented; this tests the
externally visible rejection path under sustained hostile load.)*  
**What is injected:** `chaos-c3` (mode 93, 4 MiB limit) submits 500 rounds of
impossible allocation requests (`usize::MAX`, 4 GiB, `0`). Every request beyond the
limit must return `AllocDenied`, never a panic.  
**What is verified:**
- All 500 rounds complete without kernel panic.
- Every impossible request returns `Err(AllocDenied)`, not `Ok`.
- Zero-size requests do not panic.

**Pass string:** `"chaos: C3 pass"`
**Fail on:** `KERNEL PANIC`, `"chaos: C3 FAIL"`

---

### C4 — Degraded Boot Environment: Minimal RAM

**Spec:** §22 Chaos C4  
**What is injected:** QEMU boots with `-m 192M` instead of the normal 512M.  
**What is verified:**
- Kernel boots and allocates its structures without overflowing.
- Supervisor reaches ready state.
- No silent OOM — if a service cannot be spawned due to low RAM, the kernel logs
  the failure rather than silently corrupting state.

**Pass string:** `"kernel: 4 cores ready"` + `"supervisor: ready"`  
**Fail on:** `KERNEL PANIC`  
**Harness variant:** `DegradedEnv { smp: 4, ram_mib: 192 }`

---

### C5 — Kernel Stack Probe: Rapid Nested Syscalls

**Spec:** §22 Chaos C5  
**What is injected:** `chaos-c5` (mode 94) makes 100 nested recursive calls each of
which issues a `yield_cpu()` syscall. This probes the kernel's per-syscall stack usage
under 100 pending user-space frames simultaneously active on the return path.  
**What is verified:**
- All 100 recursive rounds complete (depth 100/100 reported).
- No kernel stack overflow, no panic.

**Pass string:** `"chaos: C5 pass"`  
**Fail on:** `KERNEL PANIC`, `"chaos: C5 FAIL"`

---

### C6 — Starved Core: Other Cores Unaffected

**Spec:** §22 Chaos C6  
*(Approximated: QEMU cannot drop timer IRQs; simulated as a tight-loop service
consuming 100% of one core's CPU time.)*  
**What is injected:** `chaos-c6-hog` (mode 7, core 3) runs a tight spin loop between
preemptions — simulating a core whose timer interrupt is suppressed. Core 3 is
effectively starved of meaningful scheduling.  
**What is verified:**
- `chaos-c6-monitor` (mode 95, core 0) completes 200 `yield_cpu()` calls and logs
  pass, proving core 0 receives CPU time normally despite core 3 being maxed out.
- The system does not panic.
- Cross-core isolation holds: one saturated core does not deadlock others.

**Pass string:** `"chaos: C6 pass"`  
**Fail on:** `KERNEL PANIC`, `"chaos: C6 FAIL"`

---

### C7 — TLB Shootdown Under Load: No Corruption

**Spec:** §22 Chaos C7  
**What is injected:** `chaos-c7` (mode 96, core 1) performs 30 kill/respawn cycles of
`chaos-c7-victim` (mode 0, core 2). Each kill triggers a cross-core TLB shootdown:
core 1 kills core 2's service → IPI to core 2 → core 2 acknowledges TLB flush →
physical frames reclaimed. Between cycles, `chaos-c7` issues `try_send` to the victim
endpoint to exercise the generation-check path on a just-killed endpoint.  
**What is verified:**
- All 30 cycles complete without kernel panic or memory corruption.
- No stale TLB entry causes a phantom read/write after a page is reclaimed.
- Cross-core kill → IPI → TLB shootdown → respawn is stable over 30 iterations.

**Pass string:** `"chaos: C7 pass"`  
**Fail on:** `KERNEL PANIC`, `"chaos: C7 FAIL"`

---

## Pass Criteria Summary

| Test | Condition                                            | Bar                                    |
|------|------------------------------------------------------|----------------------------------------|
| C1   | System boots with 2 cores; supervisor reaches ready  | Graceful degradation                   |
| C2   | Non-TCB fault kills service; system continues        | Loud fault, no kernel panic            |
| C3   | 500 impossible alloc requests all return AllocDenied | Allocator rejection path robust        |
| C4   | System boots in 192M RAM; supervisor reaches ready   | Graceful degradation                   |
| C5   | 100 recursive yields complete; kernel stack intact   | No stack overflow                      |
| C6   | Core 0 alive after core 3 hog; C6 monitor logs pass  | Cross-core isolation holds             |
| C7   | 30 cross-core kill/respawn cycles complete; no panic  | TLB shootdowns safe under concurrency  |

Per invariant 12 (§3): **failures are loud, never silent.** Every test validates that
the system's response to degraded conditions is either "continue correctly with
degraded capacity" or "panic loudly with a defined reason." Silent corruption is
never acceptable.

---

## Implementation Checklist

- ✅ `milestones/v1/14_CHAOS_TESTS.md` — this file
- ✅ `services/probe/src/main.rs` — modes 91–96, dispatch arms, implementations
- ✅ `kernel/src/task/mod.rs` — 8 new service configs (chaos-c2 through chaos-c7)
- ✅ `services/supervisor/src/main.rs` — chaos probe spawns (victim-before-controller)
- ✅ `osdev/src/validator.rs` — `CHAOS_TESTS`, `run_chaos_tests()`, `run_chaos_one()`, `DegradedSmp`/`DegradedEnv` test kinds
- ✅ `osdev/src/qemu.rs` — `spawn_for_test_custom()` (custom smp + ram_mib)
- ✅ `osdev/src/main.rs` — `"chaos" => run_chaos_tests()`, docstring
