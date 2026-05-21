# Hardware: Chaos Tests

Mirrors §22 Chaos Tests (C1–C7). Graceful degradation under partial failures on real silicon.

**Reference:** `tests/qemu/chaos/CLAUDE.md` for full spec.

## Hardware applicability

Chaos tests inject failures. Some failures are more naturally injected on real hardware than in QEMU; others require QEMU-specific fault injection mechanisms. Hardware is the authoritative platform for C1 (AP failure) and C4 (degraded bootloader environment).

| ID | Failure injected | HW method | HW feasible? | Status |
|----|-----------------|-----------|-------------|--------|
| C1 | One or more APs fail to come up | Disable cores in BIOS/UEFI settings | Yes — first target | Pending |
| C2 | Corrupted ELF in boot manifest (non-TCB) | Bake a corrupt ELF into the image at build | Yes | Pending |
| C3 | Allocator forced to return `AllocFailed` at random points | Probe-driven — inject via service | Yes — self-contained | Pending |
| C4 | Degraded bootloader environment (minimal RAM) | Remove RAM sticks from hardware | Yes — hardware configuration | Pending |
| C5 | Kernel stack near exhaustion under deep syscall | Probe-driven — self-contained | Yes | Pending |
| C6 | One core's timer interrupt dropped for extended period | Not achievable without QEMU fault injection | No (QEMU only) | N/A |
| C7 | TLB shootdown IPI delayed across cores | Not achievable without QEMU fault injection | No (QEMU only) | N/A |

## C1 — AP failure (first target)

**Why hardware is the authoritative test:** QEMU simulates AP failure by not starting the AP thread. Real hardware AP failure involves actual APIC, real-mode trampoline, and actual BIOS interaction. These differ.

**Method:** Enter BIOS/UEFI setup and disable 1–3 cores. Boot `osdev image --mode bare-metal`.

**Expected serial output:**
```
smp: core 1 ready       ← (missing if core 1 disabled)
smp: core 2 ready       ← (missing if core 2 disabled)
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
- ping and pong communicate successfully across whatever cores are available

**Fail criteria:**
- `KERNEL PANIC` with missing-core reason
- System hangs during AP startup
- ping/pong fail to communicate

**Variants to test:**
- 3 cores (disable core 3): expected `kernel: 3 cores ready`
- 2 cores (disable cores 2+3): expected `kernel: 2 cores ready`
- 1 core (disable cores 1+2+3): pong must land on core 0 (same core as ping); verify same-core IPC still works

## C2 — Corrupted ELF in boot manifest

**Method:** `osdev image --mode identity` after manually corrupting a non-TCB probe ELF in the build output. Supervisor should log `PlacementInvalid` or spawn-failure and continue with remaining services.

**Build mode:** Custom (corrupt probe ELF, then `osdev image --mode identity`).

**Status:** Pending — needs build tooling to inject the corrupt binary.

## C4 — Minimal RAM

**Method:** Remove RAM from hardware to reduce to minimal amount (e.g. 1 GB). Boot bare-metal image.

**Expected:** System boots within reduced memory budget. Frame allocator reports reduced free pages. Services operate normally within constraints.

**Status:** Pending — requires hardware reconfiguration.

## Pass record

| Date | Test | Variant | Result | Notes |
|------|------|---------|--------|-------|
| — | — | — | — | No hardware chaos runs yet |
