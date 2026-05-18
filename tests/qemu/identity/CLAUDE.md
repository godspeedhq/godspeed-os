# tests/qemu/identity/

The identity test suite (§22). **20/20 tests passing — no regressions allowed.**

If any test in this directory fails, the system is no longer the system the spec describes.

## Tests

| File                           | Spec test  | Constitutional invariant  | Timeout      |
|--------------------------------|------------|---------------------------|--------------|
| `test_01_bootstrap.rs`         | §22 Test 1 | TCB integrity             | 30s / 120s   |
| `test_02_cap_enforcement.rs`   | §22 Test 2 | No ambient authority      | 30s          |
| `test_03_ipc_same_core.rs`     | §22 Test 3 | Authority is explicit     | 30s          |
| `test_04_endpoint_death.rs`    | §22 Test 4 | Restartability            | 30s / 60s    |
| `test_05_cap_transfer.rs`      | §22 Test 5 | Authority is explicit     | 30s          |
| `test_06_supervisor_restart.rs`| §22 Test 6 | Restartability            | 30s / 180s   |
| `test_07_memory_limits.rs`     | §22 Test 7 | Isolation                 | 30s / 60s    |
| `test_08_preemption.rs`        | §22 Test 8 | No service monopoly       | 120s / 120s  |
| `test_09_cross_core_ipc.rs`    | §22 Test 9 | Identity over location    | 60s          |
| `test_10_restart_core_change.rs`| §22 Test 10| Identity over location   | 30s / 180s   |

Timeout column: positive test / negative test. Single value = both cases share the timeout.

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

`WithRestart` tests use `"supervisor: ready"` as the `wait_for` guard on all tests that restart pong/ping. This ensures the restart fires only after the supervisor's spawn loop is complete — no risk of restart-mid-spawn on the timer ISR.

## Timeout rationale

Timeouts are chosen to cover Windows TCG (software emulation) where the supervisor spawns 178+ probe services before logging `"supervisor: ready"` — this loop takes 18–120 s depending on system load. KVM (CI) always completes well inside 30 s per test.

| Test  | Timeout | Reason |
|-------|---------|--------|
| 1A    | 120s    | 178 probes before "supervisor: ready"; loop ≤120s on loaded TCG |
| 7A/7B | 60s     | Probe services compete under 100+ concurrent tasks |
| 8A    | 120s    | Yielder competes with 100+ probe services |
| 8B    | 120s    | Pong/ping spawn first; no queue-full stall |
| 9A    | 60s     | Pong/ping spawn first; "pong: received" at t≈5–10s |
| 6A/6B | 180s    | WithRestart: supervisor ready ≤120s + restart phase ≤30s |
| 10A/10B| 180s   | Same as 6A/6B |

## Spawn order (affects timing)

The supervisor spawns pong and ping **first** (before all 178 probe services). This ensures cross-core IPC between ping and pong is established within ~10 s of boot, regardless of how long the probe spawn loop takes. Tests that previously timed out waiting for `"pong: received"` at t≈175 s now see it at t≈5 s.

## Test structure (§22.5)

Every test has a positive case (system permits what it should) and a negative case (system refuses what it shouldn't). Both must pass.

## Adding tests

Only add a test here if it pins a constitutional invariant from §3 or §6. Regression tests for specific bugs go in `tests/qemu/regression/`. If you add a test that invalidates a constitutional invariant, you must amend `CLAUDE.md` first.
