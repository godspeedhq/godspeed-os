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

Commit `TBD`.

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

### Test results (Phase 1 target)

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
