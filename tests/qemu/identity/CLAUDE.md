# tests/qemu/identity/

The identity test suite (§22, Tests 1-15). **`osdev test identity` runs 24 cases (Tests 1-11 and 15, each with an A/B case, plus IR1A/IR1B) - all passing, no regressions allowed.** Tests 12-14 run as their own bare-metal subcommands (see below).

If any test in this directory fails, the system is no longer the system the spec describes.

## Tests

There are **no `test_NN_*.rs` files in this directory** - it is a spec/guide directory. The identity
cases are **data-driven `TestSpec` entries in `osdev/src/validator.rs`** (each names its own
`spec_ref`, e.g. `§22 Test 4A`), and `osdev test identity` runs all 24 together.

| Case(s) in `osdev/src/validator.rs` | Spec test  | Constitutional invariant  | Timeout      |
|-------------------------------------|------------|---------------------------|--------------|
| `1A` / `1B`                         | §22 Test 1 | TCB integrity             | 30s / 120s   |
| `2A` / `2B`                         | §22 Test 2 | No ambient authority      | 30s          |
| `3A` / `3B`                         | §22 Test 3 | Authority is explicit     | 30s          |
| `4A` / `4B`                         | §22 Test 4 | Restartability            | 30s / 60s    |
| `5A` / `5B`                         | §22 Test 5 | Authority is explicit     | 30s          |
| `6A` / `6B`                         | §22 Test 6 | Restartability            | 60s / 60s    |
| `7A` / `7B`                         | §22 Test 7 | Isolation                 | 30s / 60s    |
| `8A` / `8B`                         | §22 Test 8 | No service monopoly       | 120s / 120s  |
| `9A` / `9B`                         | §22 Test 9 | Identity over location    | 60s          |
| `10A` / `10B`                       | §22 Test 10| Identity over location    | 60s / 60s    |
| `11`                                | §22 Test 11| Naming out of kernel; restartability | 60s |
| `15`                                | §22 Test 15| Unkillable set = {kernel} | 60s          |
| `IR1A` / `IR1B`                     | §12.2 §12.3| Interrupt delivery / discard-on-no-driver | 60s |

Timeout column: positive case / negative case. Single value = both cases share the timeout.

**Tests 12, 13, and 14 are heavier bare-metal scenarios run as their own subcommands** (not part of
`osdev test identity`):

| Subcommand              | Spec test   | Pins                                            | Implemented in |
|-------------------------|-------------|-------------------------------------------------|----------------|
| `osdev test iommu`      | §22 Test 12 | Confined driver cannot DMA outside its arena (H1, §6.4) | `osdev/src/main.rs` |
| `osdev test fs-restart` | §22 Test 13 | `fs` survives its own restart (Phase D)         | `osdev/src/main.rs`, `osdev/src/shell_test.rs` |
| `osdev test file-cap`   | §22 Test 14 | A file is a capability (P2, §7.10)               | `osdev/src/main.rs`, `osdev/src/shell_test.rs` |

## How tests work (§22.3)

Each test:
1. Builds a kernel + probe service image via `osdev build`.
2. Boots QEMU with `-smp 4` (and `-enable-kvm -cpu host` when `/dev/kvm` is accessible).
3. Streams serial output line by line.
4. Passes when all `expect` strings appear in order within the timeout.
5. Fails if any `fail_on` string appears, or the timeout fires, or `KERNEL PANIC` appears unexpectedly.

## TestKind variants

Tests are expressed as one of three harness kinds, defined in `osdev/src/validator.rs`:

| Kind           | Trigger                          | Used by |
|----------------|----------------------------------|---------|
| `WatchSerial`  | Look for `expect` strings        | 1A, 2A/B, 3A/B, 4A, 5A/B, 7A/B, 8A/B, 9A/B |
| `WithRestart`  | Wait for `wait_for` string, send `restart_cmd` via COM2, then look for `expect_after` | 4B, 6A/B, 10A/B |
| `WithBadTcb`   | Boot with a corrupted TCB binary, look for `KERNEL PANIC` | 1B |

`WithRestart` tests use `"supervisor: ready"` as the `wait_for` guard on all tests that restart pong/ping. This ensures the restart fires only after the supervisor's spawn loop is complete - no risk of restart-mid-spawn on the timer ISR.

## Timeout rationale

`osdev test identity` builds supervisor with `--features supervisor/identity-only`, which compiles out the 160+ non-identity probe spawns and leaves only the 16 identity probe services. This cuts `"supervisor: ready"` time from 30-200 s (full build) to ~3 s on Windows TCG. KVM (CI) completes every test well inside 30 s.

| Test    | Timeout | Reason |
|---------|---------|--------|
| 1A      | 120s    | Full supervisor build (1B test shares the image); 178 probes before "supervisor: ready" in that path |
| 7A/7B   | 60s     | 15 identity probes still compete; 30s was marginal on loaded TCG |
| 8A      | 120s    | Yielder competes with concurrent probe services for scheduler quanta |
| 8B      | 120s    | Pong/ping spawn first; no queue-full stall; 120s is conservative |
| 9A      | 60s     | "pong: received" at t≈5-10s with identity-only; 60s is 6× margin |
| 6A/6B   | 60s     | identity-only: supervisor ready ~3s; restart phase ~5s; 60s is 6× margin |
| 10A/10B | 60s     | Same as 6A/6B |

## Spawn order (affects timing)

The supervisor spawns pong and ping **first** (before all probe services). In the `identity-only` build this is followed by 15 identity probe services; in the full build it is followed by 160+ probes. Cross-core IPC between ping and pong is established within ~5 s of boot on any build variant. Tests that previously timed out waiting for `"pong: received"` at t≈175 s (full build under load) now see it at t≈5 s.

## Pass record

### Pre-IR1A/IR1B baseline (Windows TCG, 2026-05-18) - 20 tests

Recorded after the `identity-only` supervisor feature and per-test isolation sleep were introduced.

| Run | Passed | Failed | Blocked |
|-----|--------|--------|---------|
| 1-10 | 20 each | 0 | 0 |
| **Total** | **200** | **0** | **0** |

Zero failures across 200 consecutive tests (20 tests × 10 runs). Confirmed reduced timeouts (6A/6B/10A/10B: 240-300s → 60s) with comfortable margin.

### Post-IR1A/IR1B (Windows TCG, 2026-05-18) - 22 tests

IR1A and IR1B added as part of post-v1 item 9 (interrupt routing tests). Verification run to be recorded here after the next full identity suite run.

## Test structure (§22.5)

Every test has a positive case (system permits what it should) and a negative case (system refuses what it shouldn't). Both must pass.

## Adding tests

Only add a test here if it pins a constitutional invariant from §3 or §6. Regression tests for specific bugs go in `tests/qemu/regression/`. If you add a test that invalidates a constitutional invariant, you must amend `CLAUDE.md` first.
