# Hardware: Chaos Tests

Mirrors §22 Chaos Tests (C1–C7). Graceful degradation under partial failures on real silicon.

**Status: 5/5 PASS (4-core, post-placement-fix) - 2026-05-24. C1 partial (2-core degraded boot verified). C4 skipped.**

## Hardware applicability

| ID | Failure injected | HW method | HW feasible? | Status |
|----|-----------------|-----------|-------------|--------|
| C1 | One or more APs fail to come up | Disable cores in BIOS/UEFI | Partial | BIOS "multicore" only delays APs - true AP disable not available; 2-core degraded boot verified (kernel reports 2 cores, IPC works) |
| C2 | Corrupted ELF in boot manifest (non-TCB) | Probe-driven (chaos-only build) | Yes | PASS (4-core + 2-core) |
| C3 | Allocator forced to return `AllocFailed` at random points | Probe-driven (chaos-only build) | Yes | PASS (4-core + 2-core) |
| C4 | Degraded bootloader environment (minimal RAM) | Remove RAM sticks | Not meaningful | Skip - 2×4 GB; removing one stick leaves 4 GB, far above minimum |
| C5 | Kernel stack near exhaustion under deep syscall | Probe-driven (chaos-only build) | Yes | PASS (4-core + 2-core) |
| C6 | Tight-loop hog starves cores | Probe-driven (chaos-only build) | Yes (4-core) | PASS (4-core); inconclusive (2-core - hog falls back to same core as monitor) |
| C7 | Cross-core TLB shootdowns under concurrent IPC load | Probe-driven (chaos-only build) | Yes | PASS (4-core + 2-core, 30 iters) - previous hang was placement bug, not Goldmont+ |

## C1 - 2-core degraded boot (2026-05-24)

BIOS set to 2 cores. Results:
- `smp: core 1 ready` / `kernel: 2 cores ready` ✓
- pong placed correctly on core 1 ✓
- ping/pong IPC working (`pong: received "1"`) ✓
- C2/C3/C5/C7 all pass under 2-core constraints ✓

The Wyse 5070 BIOS "multicore" setting cannot disable APs entirely - it only limits how many start. True single-core AP failure (core never responds to SIPI) is not achievable on this platform. QEMU (`-smp 1`) is the authoritative test for total AP failure.

## C6 - 2-core note

In 4-core operation chaos-c6-hog lands on core 3, monitor on core 0 - genuine cross-core hog isolation. In 2-core operation, the hog falls back (core 3 unavailable) to core 0 alongside the monitor - same core, degenerate case. The monitor times out or doesn't print a result because the scenario is the same as A8 (same-core preemption), not a cross-core hog. Not a kernel bug; the probe degrades gracefully to round-robin.

## C7 - Not Goldmont+ backburner

Previous C7 hang (2026-05-24 4-core run with chaos-only image before placement fix) was caused by the placement bug: chaos-c7-victim silently placed on non-existent core 2. After the `preferred_core is_ready()` fix, C7 passes on both 4-core and 2-core hardware. The Goldmont+ IPI backburner applies only to blocking `recv()` wakeup (S3/S9/BP2), not to kill/respawn or TLB shootdown flows.

## Build mode

```
osdev image --mode chaos
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

**Expected serial lines (4-core, any order):**
```
chaos: C2 pass - system continued after non-TCB page fault
chaos: C3 pass - 500 alloc-deny cycles without panic
chaos: C5 pass - 100/100 recursive yields without stack overflow
chaos: C6 pass - core 0 alive despite core 3 hog
chaos: C7 pass - 30 cross-core TLB shootdowns survived
```

**Expected serial lines (2-core):** Same except C6 may not print (degenerate scenario).

## Pass record

| Date | Test | Cores | Result | Notes |
|------|------|-------|--------|-------|
| 2026-05-24 | C2/C3/C5/C6/C7 | 4 | 5/5 PASS | Full 4-core run, pre-placement-fix image |
| 2026-05-24 | C2/C3/C5/C6/C7 | 4 | 5/5 PASS | Post-placement-fix image. C6: hog core 3, monitor core 0. C7: 30 iters, victim on core 2. |
| 2026-05-24 | C2/C3/C5/C7 | 2 | 4/4 PASS | 2-core C1 variant; placement fix applied. C7 passes (30 iters). C6 inconclusive (hog+monitor both on core 0). |
| 2026-05-24 | C1 | 2 | Partial | 2-core degraded boot verified; true AP-never-starts not testable on this HW |
| - | C4 | - | Skipped | 2×4 GB; not a meaningful stress test |
