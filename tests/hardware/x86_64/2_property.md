# Hardware: Property Tests

Mirrors §22 Property Tests (P1–P10). Universal invariants under randomised inputs.

**Reference:** `tests/qemu/property/CLAUDE.md` for full spec.

## Hardware applicability

Property tests require thousands of iterations per property. On QEMU this is a loop inside one boot. On hardware, each boot is a new flash — making per-flash iteration counts the constraint.

The practical approach: bake a large fixed iteration count into the property probe binary (e.g. 10,000 per property per boot), then a single flash run covers one sample. Multiple flash runs accumulate samples.

| Property | Feasible on HW? | Notes |
|----------|----------------|-------|
| P1 — Random bytes never accepted as cap | Yes | Self-contained; high iteration count baked in |
| P2 — Generation strictly monotonic | Yes | Self-contained |
| P3 — Rights never widen on transfer | Yes | Self-contained |
| P4 — Alloc accounting consistent | Yes | Self-contained |
| P5 — Each endpoint has exactly one owner | Yes | Self-contained |
| P6 — Queue head/tail consistent | Yes | Self-contained |
| P7 — Page unreadable after unmap + TLB shootdown | Yes | Reads from every core; cross-core observable |
| P8 — Post-restart name resolves to higher generation | Blocked | Needs COM2 control to trigger restart |
| P9 — Gen bump invalidates ALL holders | Blocked | Needs COM2 |
| P10 — send returns exactly one result | Yes | Self-contained |

**Build mode:** `osdev image --mode identity` (once property probes are added to the identity-only spawn set, or a new `property-only` supervisor feature is added)

## Status

All pending. Property probes on hardware have not been run.

## Pass record

| Date | Properties run | Iterations/property | Passed | Failed | Notes |
|------|---------------|---------------------|--------|--------|-------|
| — | — | — | — | — | No hardware property runs yet |
