# tests/qemu/identity/

The identity test suite (§22). One file per test. **If any test in this directory fails, the system is no longer the system the spec describes.**

## Tests

| File                          | Spec test | Constitutional invariant |
|-------------------------------|-----------|--------------------------|
| `test_01_bootstrap.rs`        | §22 Test 1  | TCB integrity |
| `test_02_cap_enforcement.rs`  | §22 Test 2  | No ambient authority |
| `test_03_ipc_same_core.rs`    | §22 Test 3  | Authority is explicit |
| `test_04_endpoint_death.rs`   | §22 Test 4  | Restartability |
| `test_05_cap_transfer.rs`     | §22 Test 5  | Authority is explicit |
| `test_06_supervisor_restart.rs`| §22 Test 6 | Restartability |
| `test_07_memory_limits.rs`    | §22 Test 7  | Isolation |
| `test_08_preemption.rs`       | §22 Test 8  | No service monopoly |
| `test_09_cross_core_ipc.rs`   | §22 Test 9  | Identity over location |
| `test_10_restart_core_change.rs`| §22 Test 10| Identity over location |

## How tests work (§22.3)

Each test:
1. Builds a test service image via `osdev build`.
2. Boots QEMU with `-smp 4` (or as specified in the test).
3. Streams serial output.
4. Asserts that `TEST:PASS` lines appear within 30 seconds.
5. Fails if `TEST:FAIL` appears, or if the 30 s timeout fires, or if `KERNEL PANIC` appears unexpectedly.

## Test structure (§22.5)

Every test has a positive case (system permits what it should) and a negative case (system refuses what it shouldn't). Both must pass.

## Adding tests

Only add a test here if it pins a constitutional invariant from §3 or §6. Regression tests for specific bugs go in a separate `tests/qemu/regression/` directory (not yet created). If you add a test that invalidates a constitutional invariant, you must amend `CLAUDE.md` first.
