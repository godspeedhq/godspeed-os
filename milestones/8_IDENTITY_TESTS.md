# Milestone 8 — Identity Test Suite (§22)

> Build and run the §22 identity test suite.
> `osdev test identity` reports each test as PASS, FAIL, or BLOCKED.
> No FAIL results. BLOCKED means a required feature is not yet implemented.

---

## Goal

Implement the §22 test harness and run all 10 tests (20 subtests A/B).
Tests that the current kernel can satisfy pass. Tests that require
unimplemented features (test service, AllocMem, cap-in-IPC) are BLOCKED
with an explicit reason.

---

## Phase 1 — Harness + passing tests ✅

Commit `e5abfa2`.

### Harness (`osdev/src/`)

- ✅ `qemu.rs` — `spawn_for_test(image, smp, serial_path, control_port)`:
  spawns QEMU non-blocking, serial to a per-test file, COM2 to a
  configurable TCP port (5556 for restart tests, null otherwise).
- ✅ `validator.rs` — `run_identity_tests()`: kills existing QEMU, builds
  once, boots one QEMU per non-blocked test, polls serial file for expected
  lines, enforces 30 s timeout, prints PASS/FAIL/BLOCK summary.

### Kernel/SDK change — `core_id` in `ServiceContextData`

- ✅ `kernel/src/task/mod.rs` — `ServiceContextData` gains `core_id: u32`
  (replacing one pad slot); written at spawn time.
- ✅ `sdk/rust/src/service_context.rs` — mirrors the field; exposes
  `ctx.core_id() -> u32` to services.
- ✅ `examples/pong/src/main.rs` — logs `"pong: ready on core N"` so
  test 10A can assert the correct core was used after restart.

---

## Phase 2 — Fix 6B / 10B (stale log + kill-all) ✅

Tests 6B and 10B were recorded as PASS in Phase 1 but were actually failing.
Two bugs caused both to report the wrong result:

### Bug 1 — Stale serial log (test harness)

`-serial file:path` in QEMU **appends** to the file; previous runs accumulated
87 k lines. `poll_serial` found `"pong: received"` in old content and
immediately sent the `RESTART` command while the fresh QEMU was still in early
boot (line 34 of 87 k — right after `"kernel: 4 cores ready"`). At that
point `kill_by_name("pong")` found no task, so `spawn_service_by_name` created
a spurious early-boot pong before the normal service stack had started.

**Fix:** `osdev/src/validator.rs` — truncate the per-test log file to zero
bytes before calling `spawn_for_test`, in both `WatchSerial` and `WithRestart`
arms of `run_one`.

### Bug 2 — `kill_by_name` only killed the first match (kernel)

Because of Bug 1, two pong instances were alive simultaneously: the early-boot
spurious one and the supervisor-spawned one. The test's actual `RESTART` command
killed only the first task found by `find_task_by_name`, leaving the
supervisor-spawned pong (the one ping held a cap to) alive. Ping's `try_send`
kept succeeding; `EndpointDead` was never seen.

**Fix:** `kernel/src/task/mod.rs` — `kill_by_name` now loops until
`find_task_by_name` returns `None`, killing every live task with the given
name before returning.

### Test results (Phase 2 target)

| ID  | Test name                           | Result  | Blocked reason (if any) |
|-----|-------------------------------------|---------|--------------------------|
| 1A  | bootstrap_steady_state_positive     | ✅ PASS  | — |
| 1B  | bootstrap_tcb_failure_panics        | BLOCKED  | needs corrupted TCB binary |
| 2A  | cap_enforcement_positive            | ✅ PASS  | — |
| 2B  | cap_enforcement_negative            | ✅ PASS  | — |
| 3A  | ipc_same_core_positive              | BLOCKED  | needs test service |
| 3B  | ipc_no_send_right                   | BLOCKED  | needs test service |
| 4A  | endpoint_death_send_returns_dead    | BLOCKED  | needs test service |
| 4B  | blocked_sender_wakes_on_death       | BLOCKED  | needs test service |
| 5A  | cap_transfer_positive               | BLOCKED  | cap embedding in IPC not implemented |
| 5B  | cap_transfer_negative               | BLOCKED  | cap embedding in IPC not implemented |
| 6A  | supervisor_restart_positive         | ✅ PASS  | — |
| 6B  | stale_cap_revoked_after_restart     | ✅ PASS  | — |
| 7A  | memory_alloc_within_limit           | BLOCKED  | AllocMem syscall (6) not implemented |
| 7B  | memory_beyond_limit                 | BLOCKED  | AllocMem syscall (6) not implemented |
| 8A  | yield_advisory_works                | BLOCKED  | needs test service |
| 8B  | non_yielding_service_preempted      | BLOCKED  | needs test service |
| 9A  | cross_core_ipc_positive             | ✅ PASS  | — |
| 9B  | cross_core_no_authority_leak        | BLOCKED  | needs test service |
| 10A | restart_changes_core_transparently  | ✅ PASS  | — |
| 10B | client_reacquires_after_core_change | ✅ PASS  | — |

**Result: 8 PASS, 12 BLOCKED, 0 FAIL.**

All blocked items: "not implemented; will be implemented when a test requires it."

---

## Acceptance

```
osdev test identity
  8 passed  0 failed  12 blocked
```

No FAIL results. A FAIL means the kernel regressed on a constitutional
invariant. BLOCKED tests become FAIL if the feature they require is
implemented without making the test pass.
