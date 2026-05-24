# Hardware: Stress Tests

Mirrors §22 Stress Tests (S1–S10). No drift, leak, or corruption under sustained load.

**Reference:** `tests/qemu/stress/CLAUDE.md` for full spec.

## Hardware applicability

Hardware stress tests are more meaningful than QEMU equivalents for long-running scenarios — no TCG timing distortion, real cache pressure, real timer jitter. S6 and S8 are the natural starting points.

| ID | Scenario | Duration | HW feasible? | Status |
|----|----------|----------|-------------|--------|
| S1 | IPC saturation: sustained `try_send` on full queue | 1 hour | Yes — self-contained | Pending |
| S2 | Restart storm: 50 kill/respawn cycles | Until complete | Yes — self-contained (ctx.kill/spawn) | Pending |
| S3 | Cross-core thrash: 4 cores × all-to-all IPC | 10 min | Yes — self-contained | Pending |
| S4 | Cap table churn: 50 spawn/kill cycles | Until complete | Yes — self-contained | Pending |
| S5 | Generation monotonic: 500 kill/respawn | Until complete | Yes — self-contained (long) | Pending |
| S6 | Long-running stability: ping/pong + introspection | 24 hours | Yes — bare-metal mode | In progress |
| S7 | Memory pressure: alloc-to-limit + free, 100 cycles | Until complete | Yes — self-contained | Pending |
| S8 | Idle stability: boot, no workload, observe | 24 hours | Yes — bare-metal mode | Pending |
| S9 | Interrupt storm: high-frequency timer + IPI cross-fire | 1 hour | Yes — self-contained | Pending |
| S10 | Cascading revocation: 3 caps, cross-core kill | Until propagated | Yes — self-contained (ctx.kill) | Pending |

## Build mode for S1–S10

```
osdev image --mode stress
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

`stress-only` supervisor spawns pong + ping + all 18 stress probe tasks (S1–S10 with their recv/victim partners). No QEMU harness required — all probes are self-contained.

**Expected serial lines (any order):**
```
stress: S1 pass (10000/10000)
stress: S2 pass (50/50)
stress: S3 pass (50/50)
stress: S4 pass (50/50)
stress: S5 pass (500/500)
stress: S6 start
stress: S6 pass (500/500)
stress: S7 pass (100/100)
stress: S8 pass (5 yields)
stress: S9 pass (100/100)
stress: S10 pass (3/3 caps dead)
```

No `KERNEL PANIC` and no line containing `FAIL` allowed.

## S6 — Long-running stability (in progress)

**Build mode:** `osdev image --mode bare-metal`

The machine was left running after the 2026-05-21 bare-metal milestone boot. Continuous ping→pong cross-core IPC (ping core 0, pong core 1). Serial log accumulates in `build/putty_serial_output.log`.

**Pass criteria:**
- No `KERNEL PANIC` in serial log
- `pong: received "N"` counter increments continuously for 24 hours
- No service dies unexpectedly

## S8 — Idle stability

**Build mode:** `osdev image --mode bare-metal`

Boot, leave idle (no deliberate IPC workload beyond initial ping/pong handshake), observe for 24 hours.

**Pass criteria:**
- No `KERNEL PANIC`
- All 4 cores remain alive
- No unexplained output

## Pass record

| Date | Test | Duration | Result | Notes |
|------|------|----------|--------|-------|
| 2026-05-21 | S6 (partial) | ~ongoing | In progress | Machine left running after bare-metal boot milestone |
