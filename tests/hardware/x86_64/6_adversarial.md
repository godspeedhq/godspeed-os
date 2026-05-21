# Hardware: Adversarial Tests

Mirrors §22 Adversarial Tests (A1–A10). Capability isolation under direct attack on real silicon.

**Reference:** `tests/qemu/adversarial/CLAUDE.md` for full spec.

## Hardware applicability

Adversarial tests are self-contained attack probes — they attempt the attack and report success or failure via serial. No COM2 control needed. The main blocker is that an `adversarial-only` supervisor feature does not yet exist; adding one follows the same pattern as `perf-brutal-only`.

Once the supervisor feature is built, all 10 adversarial tests should run unmodified on hardware.

**Build mode:** Will need `osdev image --mode adversarial` (new supervisor feature required).

## Tests

| ID | Attack | Self-contained? | HW status |
|----|--------|----------------|-----------|
| A1 | Random u64 values used as caps | Yes | Pending — needs `adversarial-only` feature |
| A2 | Brute-force endpoint IDs across u32 space | Yes | Pending |
| A3 | Alloc beyond contract limit through every syscall path | Yes | Pending |
| A4 | Use cap with rights not held | Yes | Pending |
| A5 | TOCTOU: race syscall with revocation | Yes | Pending |
| A6 | Fill cap table to DoS kernel | Yes | Pending |
| A7 | Detect IPC partner identity via timing | Yes — timing observable on HW | Pending |
| A8 | Monopolize core via tight loop | Yes | Pending |
| A9 | Spawn service directly, bypassing supervisor | Yes | Pending |
| A10 | Pass kernel addresses as syscall args | Yes | Pending |

## A7 timing side-channel note

A7 is more meaningful on real hardware than on QEMU TCG, because hardware has genuine cache timing variation. A QEMU pass is necessary but not sufficient — a hardware pass is the authoritative result for timing side-channel resistance.

## Pass record

| Date | Tests run | Passed | Failed | Notes |
|------|-----------|--------|--------|-------|
| — | — | — | — | No hardware adversarial runs yet |

## To unblock

1. Add `adversarial-only = []` feature to `services/supervisor/Cargo.toml`
2. Wrap adversarial probe spawns in `#[cfg(feature = "adversarial-only")]` in `supervisor/src/main.rs`
3. Add `cmd_build_adversarial()` to `osdev/src/main.rs`
4. Add `"adversarial"` to the `--mode` match in `cmd_image()`
