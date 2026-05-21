# Hardware: Stress Tests

Mirrors §22 Stress Tests (S1–S10). No drift, leak, or corruption under sustained load.

**Reference:** `tests/qemu/stress/CLAUDE.md` for full spec.

## Hardware applicability

Hardware stress tests are more meaningful than QEMU equivalents for long-running scenarios — no TCG timing distortion, real cache pressure, real timer jitter. S6 and S8 are the natural starting points.

| ID | Scenario | Duration | HW feasible? | Status |
|----|----------|----------|-------------|--------|
| S1 | IPC saturation: sustained `try_send` on full queue | 1 hour | Yes — self-contained | Pending |
| S2 | Restart storm: 100k kill/respawn cycles | Until complete | Blocked — needs COM2 control | Pending |
| S3 | Cross-core thrash: 4 cores × all-to-all IPC | 10 min | Yes — self-contained | Pending |
| S4 | Cap table churn: 100k random create/destroy | Until complete | Yes — self-contained | Pending |
| S5 | Generation overflow: force counter to wrap | Until wrap + 1k ops | Yes — self-contained (very long) | Pending |
| S6 | Long-running stability: ping/pong + introspection | 24 hours | Yes — bare-metal mode | In progress |
| S7 | Memory pressure: alloc-to-limit + free, 10k cycles | Until complete | Yes — self-contained | Pending |
| S8 | Idle stability: boot, no workload, observe | 24 hours | Yes — bare-metal mode | Pending |
| S9 | Interrupt storm: high-frequency timer + IPI cross-fire | 1 hour | Yes — self-contained | Pending |
| S10 | Cascading revocation: kill service held by many | Until propagated | Blocked — needs COM2 control | Pending |

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
