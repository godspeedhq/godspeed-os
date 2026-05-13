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

---

## Phase 3 — Probe service + Group A identity tests ✅

Commit `d958402`.

### Probe service (`services/probe/`)

One binary, multiple modes selected by `probe_mode` written into
`ServiceContextData` at spawn time. Supervisor spawns each probe variant by
name; kernel wires send-peer SEND/GRANT caps at spawn time.

Modes implemented:
- 0 `PASSIVE` — idle kill target
- 1 `ECHO_RECV` — recv one message (Test 3A receiver)
- 2 `ECHO_SEND` — send to probe-recv (Test 3A sender)
- 3 `NO_SEND_RIGHT` — try_send via recv-slot cap → CapInsufficientRights (Test 3B)
- 4 `SEND_AFTER_KILL` — kill probe-victim, try_send → EndpointDead (Test 4A)
- 5 `FILL_AND_BLOCK` — fill 16-slot queue + blocking send; woken by KILL (Test 4B)
- 6 `YIELD_LOGGER` — yield × 10, log sentinel (Test 8A)
- 7 `HOG` — tight loop (Test 8B via ping output)
- 8 `CAP_FORGE` — try_send on slot 99 → CapNotHeld (Test 9B)

### Kernel changes

- ✅ `kernel/src/task/mod.rs` — `probe_mode` field in `ServiceContextData`;
  `service_config()` match arms for each probe variant; send-peer GRANT flag
  for cap-transfer probes.
- ✅ `kernel/src/task/scheduler.rs` — kstack freeing on task death (BSS
  collision root cause fixed; `KSTACK_USED` magic-marker approach).
- ✅ `sdk/rust/src/ipc.rs` — `try_send`, `try_send_by_handle` wrappers.
- ✅ `sdk/rust/src/service_context.rs` — `probe_mode()`, `recv_handle()`,
  `try_send_by_handle()`, `kill()`.

### Test results (Phase 3 target)

| ID  | Test name                           | Result  |
|-----|-------------------------------------|---------|
| 3A  | ipc_same_core_positive              | ✅ PASS  |
| 3B  | ipc_no_send_right                   | ✅ PASS  |
| 4A  | endpoint_death_send_returns_dead    | ✅ PASS  |
| 4B  | blocked_sender_wakes_on_death       | ✅ PASS  |
| 8A  | yield_advisory_works                | ✅ PASS  |
| 8B  | non_yielding_service_preempted      | ✅ PASS  |
| 9B  | cross_core_no_authority_leak        | ✅ PASS  |

**Running total: 15 PASS, 5 BLOCKED (1B, 5A, 5B, 7A, 7B), 0 FAIL.**

---

## Phase 4 — Cap transfer (Tests 5A / 5B) ✅

Commit `e06e4e8`.

### New capability right: `GRANT`

The kernel gained a `GRANT` right bit. A cap carrying `SEND | GRANT` may be
transferred via IPC; a `SEND`-only cap returns `CapNotGrantable` if embedded
in a message.

### Kernel / SDK changes

- ✅ `kernel/src/syscall/dispatch.rs` — syscall 11 `SendWithCap`: validates
  GRANT right, transfers cap to receiver, removes from sender's table.
- ✅ `kernel/src/syscall/dispatch.rs` — syscall 12 `TakePendingCap`: pops the
  next received cap from the task's pending-cap queue.
- ✅ `kernel/src/task/mod.rs` — `send_peers_grant` flag on `ServiceConfig`;
  caps wired with `GRANT` bit when flag is set.
- ✅ `sdk/rust/src/service_context.rs` — `send_with_cap()`, `take_pending_cap()`.
- ✅ `services/probe/src/main.rs` — modes 9 `GRANT_RECV`, 10 `GRANT_SEND`,
  11 `NO_GRANT_SEND`.
- ✅ Kstack BSS-collision fix applied (arrays split to avoid overlapping statics).

### Test results (Phase 4 target)

| ID  | Test name                           | Result  |
|-----|-------------------------------------|---------|
| 5A  | cap_transfer_positive               | ✅ PASS  |
| 5B  | cap_transfer_negative               | ✅ PASS  |

**Running total: 17 PASS, 3 BLOCKED (1B, 7A, 7B), 0 FAIL.**

---

## Phase 5 — TCB failure injection + AllocMem (Tests 1B, 7A, 7B) ✅

### Test 1B — `bootstrap_tcb_failure_panics`

**Approach:** Cargo feature `test-bad-registry` replaces the registry ELF with
`b"\xDE\xAD"` (invalid). Init calls Abort syscall (9) when registry spawn
fails, which triggers a kernel panic with a specific reason string.

- ✅ `kernel/Cargo.toml` — `[features] test-bad-registry = []`
- ✅ `kernel/src/task/mod.rs` — `#[cfg(feature = "test-bad-registry")]` gate
  selects invalid ELF bytes; all other service configs unchanged.
- ✅ `kernel/src/syscall/dispatch.rs` — syscall 9 `Abort`: reads reason string
  from user space, emits `"KERNEL PANIC"` to serial, then `panic!("reason: {}")`.
- ✅ `sdk/rust/src/service_context.rs` — `abort(reason: &str) -> !` wrapper.
- ✅ `services/init/src/main.rs` — TCB failure handlers call `ctx.abort(…)`
  instead of looping.
- ✅ `osdev/src/disk_image.rs` — `create_at(kernel_elf, limine_dir, image_path)`
  extracted from `create`; `create` delegates to it. Allows test 1B to write
  to `build/tests/1B-bad-tcb.img` without overwriting `build/os.img`.
- ✅ `osdev/src/validator.rs` — `TestKind::WithBadTcb { expect, fail_on, … }`:
  builds kernel with feature, creates separate image, runs QEMU, watches serial.
  Test 1B promoted from `Blocked` to `WithBadTcb`.

### Tests 7A / 7B — Memory limit enforcement

**Approach:** AllocMem syscall (6) tracks per-task budget via three new
`[u64; MAX_TASKS]` statics in `scheduler.rs`; maps pages into the task's
active CR3 (which remains set during a syscall on x86_64).

- ✅ `kernel/src/task/scheduler.rs` — `TASK_HEAP_VA_START = 0x1_0000_0000`;
  `TASK_ALLOC_BYTES`, `TASK_LIMIT_BYTES`, `TASK_NEXT_ALLOC_VA` arrays;
  `set_task_memory_budget(slot, limit)` and `current_task_claim_alloc(size)`.
- ✅ `kernel/src/task/mod.rs` — `memory_limit: u64` in `ServiceConfig`;
  `set_task_memory_budget` called after `commit_task`; `probe-7a` (mode 12,
  64 MiB limit) and `probe-7b` (mode 13, 64 MiB limit) entries added.
- ✅ `kernel/src/syscall/dispatch.rs` — `AllocMem = 6`: validates budget via
  `current_task_claim_alloc`, maps pages with `map_in_active_tables`, returns
  base VA or -11 (`AllocDenied`).
- ✅ `sdk/rust/src/service_context.rs` — `AllocError` enum; `alloc_mem(size)`.
- ✅ `services/probe/src/main.rs` — modes 12 `ALLOC_OK` and 13 `ALLOC_LIMIT`.
- ✅ `services/supervisor/src/main.rs` — spawns `probe-7a` and `probe-7b`.
- ✅ `osdev/src/validator.rs` — tests 7A and 7B promoted from `Blocked` to
  `WatchSerial`.

### Final test table

| ID  | Test name                           | Result  |
|-----|-------------------------------------|---------|
| 1A  | bootstrap_steady_state_positive     | ✅ PASS  |
| 1B  | bootstrap_tcb_failure_panics        | ✅ PASS  |
| 2A  | cap_enforcement_positive            | ✅ PASS  |
| 2B  | cap_enforcement_negative            | ✅ PASS  |
| 3A  | ipc_same_core_positive              | ✅ PASS  |
| 3B  | ipc_no_send_right                   | ✅ PASS  |
| 4A  | endpoint_death_send_returns_dead    | ✅ PASS  |
| 4B  | blocked_sender_wakes_on_death       | ✅ PASS  |
| 5A  | cap_transfer_positive               | ✅ PASS  |
| 5B  | cap_transfer_negative               | ✅ PASS  |
| 6A  | supervisor_restart_positive         | ✅ PASS  |
| 6B  | stale_cap_revoked_after_restart     | ✅ PASS  |
| 7A  | memory_alloc_within_limit           | ✅ PASS  |
| 7B  | memory_beyond_limit                 | ✅ PASS  |
| 8A  | yield_advisory_works                | ✅ PASS  |
| 8B  | non_yielding_service_preempted      | ✅ PASS  |
| 9A  | cross_core_ipc_positive             | ✅ PASS  |
| 9B  | cross_core_no_authority_leak        | ✅ PASS  |
| 10A | restart_changes_core_transparently  | ✅ PASS  |
| 10B | client_reacquires_after_core_change | ✅ PASS  |

**Result: 20 PASS, 0 BLOCKED, 0 FAIL.**

---

## Acceptance

```
osdev test identity
  20 passed  0 failed  0 blocked
```

No FAIL results. A FAIL means the kernel regressed on a constitutional
invariant. All §22 identity tests pass; the system is the system this
document describes.
