# Hardware: Chaos Tests

Mirrors §22 Chaos Tests (C1–C7). Graceful degradation under partial failures on real silicon.

**Status: Pending** — `chaos-only` build mode ready; awaiting hardware run.

## Hardware applicability

| ID | Failure injected | HW method | HW feasible? | Status |
|----|-----------------|-----------|-------------|--------|
| C1 | One or more APs fail to come up | Disable cores in BIOS/UEFI settings | Yes | Pending |
| C2 | Corrupted ELF in boot manifest (non-TCB) | Probe-driven (chaos-only build) | Yes | Pending |
| C3 | Allocator forced to return `AllocFailed` at random points | Probe-driven (chaos-only build) | Yes | Pending |
| C4 | Degraded bootloader environment (minimal RAM) | Remove RAM sticks from hardware | Yes | Pending |
| C5 | Kernel stack near exhaustion under deep syscall | Probe-driven (chaos-only build) | Yes | Pending |
| C6 | Tight-loop hog starves cores | Probe-driven (chaos-only build) | Yes | Pending |
| C7 | Cross-core TLB shootdowns under concurrent IPC load | Probe-driven (chaos-only build) | Yes | Pending |

Note: C6 and C7 were previously labelled "QEMU only" but the probes test preemption (C6) and TLB shootdown survival (C7) — both run fine on hardware. "QEMU fault injection" refers to a different injection vector not used by the probes.

## Build mode

```
osdev image --mode chaos
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

Spawns pong + ping + chaos-c2/c2-monitor/c3/c5/c6-hog/c6-monitor/c7-victim/c7. All probes are self-contained — no COM2 control port needed.

**Expected serial lines (any order):**
```
chaos: C2 pass — system continued after non-TCB page fault
chaos: C3 pass — 500 alloc-deny cycles without panic
chaos: C5 pass — 100/100 recursive yields without stack overflow
chaos: C6 pass — core 0 alive despite core 3 hog
chaos: C7 pass — 30 cross-core TLB shootdowns survived
```

No `KERNEL PANIC` and no line containing `FAIL` allowed.

## C1 — AP failure

**Method:** Enter BIOS/UEFI setup and disable 1–3 cores. Boot `osdev image --mode bare-metal`.

**Expected serial output:**
```
kernel: N cores ready   ← N < 4, matching the enabled count
supervisor: ready
ping: starting
pong: ready on core N   ← pong placed on an available core
pong: received "1"
...
```

**Pass criteria:**
- System boots and reaches steady state with the available cores
- No `KERNEL PANIC`
- `kernel: N cores ready` reflects the actual enabled core count
- ping and pong communicate successfully

**Variants to test:**
- 3 cores (disable core 3): expected `kernel: 3 cores ready`
- 2 cores (disable cores 2+3): expected `kernel: 2 cores ready`
- 1 core (disable cores 1+2+3): pong lands on core 0; same-core IPC verified

## C4 — Minimal RAM

**Method:** Remove RAM from hardware. Boot `osdev image --mode bare-metal`.

**Expected:** System boots within reduced memory budget. Frame allocator reports reduced free pages. Services operate normally within constraints.

## Pass record

| Date | Test | Variant | Result | Notes |
|------|------|---------|--------|-------|
| — | — | — | — | No hardware chaos runs yet |
