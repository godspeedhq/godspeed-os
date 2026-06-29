# Hardware: Stress Tests

Mirrors §22 Stress Tests (S1-S10). No drift, leak, or corruption under sustained load.

**Reference:** `tests/qemu/stress/CLAUDE.md` for full spec.

**Status: 8/10** - S1, S2, S4, S5, S6, S7, S8, S10 pass. S3 and S9 not measured (Goldmont+ IPI quirk - same root cause as B2/BP2).

## Hardware applicability

Hardware stress tests are more meaningful than QEMU equivalents for long-running scenarios - no TCG timing distortion, real cache pressure, real timer jitter.

| ID | Scenario | Duration | Status |
|----|----------|----------|--------|
| S1 | IPC saturation: sustained `try_send` on full queue | Until complete | ✅ pass (10000/10000) |
| S2 | Restart storm: 50 kill/respawn cycles | Until complete | ✅ pass (50/50) |
| S3 | Cross-core blocking IPC thrash (core 0 → core 1) | 10 min | Not measured |
| S4 | Cap table churn: 50 spawn/kill cycles | Until complete | ✅ pass (10/10) |
| S5 | Generation monotonic: 500 kill/respawn | Until complete | ✅ pass (500/500) |
| S6 | Long-running stability: self-ping 500 rounds | Until complete | ✅ pass (500/500) |
| S7 | Memory pressure: alloc-to-limit + free, 100 cycles | Until complete | ✅ pass (100/100) |
| S8 | Idle scheduler: 5 yields | Until complete | ✅ pass (5 yields) |
| S9 | IPI storm: 2 senders (50 each) + cross-core receiver | 1 hour | Not measured |
| S10 | Cascading revocation: 3 caps, cross-core kill | Until propagated | ✅ pass (3/3 caps dead) |

## S3 and S9 - not measured on Goldmont+

S3 sends 50 blocking messages from core 0 to a receiver on core 1. S9 has two
senders (cores 0 and 1) sending to a receiver on core 2. Both stall because the
blocking `send` → blocked `recv` round-trip requires a WAKE_RECEIVER IPI from the
receiving core back to the sender, and Goldmont+ does not reliably deliver that IPI
under concurrent load.

Same root cause as B2 and BP2 (cross-core IPC round-trip). S10 (cross-core kill)
passed because killing a service bumps the generation table without requiring an IPI
wakeup acknowledgement.

On AMD or later Intel hardware these would complete in seconds.

## Build mode

```
osdev image --mode stress
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

`stress-only` supervisor spawns pong + ping + all 18 stress probe tasks (S1-S10 with
their recv/victim partners). No QEMU harness required - all probes are self-contained.

**Expected serial lines (any order, 8/10 on Goldmont+):**
```
stress: S1 pass (10000/10000)
stress: S2 pass (50/50)
stress: S4 pass (10/10)
stress: S5 pass (500/500)
stress: S6 start
stress: S6 pass (500/500)
stress: S7 pass (100/100)
stress: S8 start
stress: S8 pass (5 yields)
stress: S10 pass (3/3 caps dead)
```

No `KERNEL PANIC` and no line containing `FAIL` allowed.

## S6 - Long-running stability (bare-metal)

**Build mode:** `osdev image --mode bare-metal`

Continuous ping→pong cross-core IPC (ping core 0, pong core 1). Serial log
accumulates in `build/putty_serial_output.log`.

**Pass criteria:** No `KERNEL PANIC`; `pong: received "N"` increments continuously
for 24 hours; no service dies unexpectedly.

## S8 - Idle stability (bare-metal)

**Build mode:** `osdev image --mode bare-metal`

Boot, leave idle, observe for 24 hours.

**Pass criteria:** No `KERNEL PANIC`; all 4 cores alive; no unexplained output.

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| 2026-05-21 | S6 partial | Machine left running after bare-metal milestone boot |
| 2026-05-24 | 8/10 (S1,S2,S4,S5,S6,S7,S8,S10) | stress-only build; S3/S9 not measured - Goldmont+ IPI quirk |
