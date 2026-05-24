# Hardware: Chaos Tests

Mirrors §22 Chaos Tests (C1–C7). Graceful degradation under partial failures on real silicon.

**Status: 4/5 probe-driven PASS** (C2/C3/C5/C6); C7 backburner (Goldmont+ cross-core IPI); C1/C4 not testable on this hardware.

## Hardware applicability

| ID | Failure injected | HW method | HW feasible? | Status |
|----|-----------------|-----------|-------------|--------|
| C1 | One or more APs fail to come up | Disable cores in BIOS/UEFI | Not on this HW | Not testable — Wyse 5070 BIOS "multicore" setting only delays AP startup; APs always come up |
| C2 | Corrupted ELF in boot manifest (non-TCB) | Probe-driven (chaos-only build) | Yes | PASS 2026-05-24 |
| C3 | Allocator forced to return `AllocFailed` at random points | Probe-driven (chaos-only build) | Yes | PASS 2026-05-24 |
| C4 | Degraded bootloader environment (minimal RAM) | Remove RAM sticks from hardware | Yes (RAM removal) | Pending (physical RAM removal) |
| C5 | Kernel stack near exhaustion under deep syscall | Probe-driven (chaos-only build) | Yes | PASS 2026-05-24 |
| C6 | Tight-loop hog starves cores | Probe-driven (chaos-only build) | Yes | PASS 2026-05-24 |
| C7 | Cross-core TLB shootdowns under concurrent IPC load | Probe-driven (chaos-only build) | Backburner | Hung — cross-core IPC between non-BSP cores hits Goldmont+ IPI quirk (same backburner as S3/S9) |

## C1 — Not testable on Wyse 5070

The Wyse 5070 BIOS "multicore" setting delays AP startup but does not prevent APs from booting. Observed 2026-05-24:
- BIOS set to "multicore = 1"
- `kernel: 1 cores ready` printed (BSP timeout fired before slow APs responded)
- Supervisor spawn calls later: all 4 core placements succeeded — APs had caught up by then
- All tasks ran on cores 0–3 normally

The "kernel: 1 cores ready" message is a timing artifact, not a true single-core run. C1 is authoritative on QEMU (`-smp 2` or `-smp 1`). On hardware it requires a BIOS with genuine per-core disable (e.g. individual core enable/disable, not a count-based "multicore" knob).

## C7 — Goldmont+ backburner

chaos-c7 (controller, core 1) and chaos-c7-victim (core 2) coordinate via cross-core IPC. The Goldmont+ BSP IPI quirk causes WAKE_RECEIVER delivery to stall under concurrent load. Same root cause as stress S3/S9 and perf BP2. Deferred until AMD or later Intel hardware.

## Build mode

```
osdev image --mode chaos
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

**Expected serial lines (C2/C3/C5/C6 — any order):**
```
chaos: C2 pass — system continued after non-TCB page fault
chaos: C3 pass — 500 alloc-deny cycles without panic
chaos: C5 pass — 100/100 recursive yields without stack overflow
chaos: C6 pass — core 0 alive despite core 3 hog
```

## C4 — Minimal RAM

**Method:** Remove one 4 GB RAM stick (leave 1×4 GB), boot `osdev image --mode bare-metal`.

**Expected:** System boots. `memory: frame allocator ready (NNN MiB free)` shows reduced free frames. ping/pong run normally.

## Pass record

| Date | Test | Variant | Result | Notes |
|------|------|---------|--------|-------|
| 2026-05-24 | C2/C3/C5/C6 | chaos-only build | 4/4 PASS | Wyse 5070, J5005, 4 cores. |
| 2026-05-24 | C7 | chaos-only build | Hung | Goldmont+ cross-core IPI backburner (cores 1→2 IPC stalls) |
| 2026-05-24 | C1 | bare-metal + BIOS "multicore=1" | Not testable | APs still boot despite setting; QEMU `-smp 1` is authoritative |
| — | C4 | bare-metal + 1×4 GB RAM | Pending | Physical RAM removal required |
