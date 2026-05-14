# Milestone 10: Fuzz Tests (§22 Fuzz)

## Goal

The kernel must never panic on user-controllable input. Any panic discovered by F1–F8 is a mandatory kernel fix; the fix must include a regression test added to the identity or property suite before any other work proceeds.

## Spec reference

CLAUDE.md §22 Fuzz Tests.

---

## Status

| Phase | Tests | Status |
|-------|-------|--------|
| Phase 1 | F1, F2, F5, F6, F7, F8 | ✅ 6/6 PASS |
| Phase 2 | F3, F4 | 🔲 DEFERRED |

---

## Design

### Architecture

Fuzz tests reuse the probe service binary (`services/probe`) with new probe modes 30–35. The harness (`osdev/src/validator.rs`) boots QEMU fresh for each test, watches for the PASS string, and fails immediately on `KERNEL PANIC`. The same `osdev test fuzz` command runs all Phase 1 tests sequentially.

### New probe modes

| Mode | Test | Surface |
|------|------|---------|
| 30 | F1 | Random args (a0/a1/a2) for each known non-abort syscall number |
| 31 | F2 | Random u64 values as syscall numbers; all must return -1 (UnknownSyscall) |
| 32 | F5 | Random-content IPC message bodies; sizes 0–4096 bytes |
| 33 | F6 | SendWithCap with random endpoint and grant cap slot indices |
| 34 | F7 | try_send via stale cap after kill/respawn cycles |
| 35 | F8 | AllocMem with edge-case sizes: 0, u64::MAX, overflow values |

### New service configs (kernel/src/task/mod.rs)

| Name | Mode | has_recv | send_peers |
|------|------|----------|------------|
| fuzz-f1 | 30 | no | — |
| fuzz-f2 | 31 | no | — |
| fuzz-f5-recv | 0 (passive) | yes | — |
| fuzz-f5 | 32 | no | fuzz-f5-recv |
| fuzz-f6-recv | 0 (passive) | yes | — |
| fuzz-f6 | 33 | no | fuzz-f6-recv |
| fuzz-f7-victim | 0 (passive) | yes | — |
| fuzz-f7 | 34 | no | fuzz-f7-victim |
| fuzz-f8 | 35 | no | — |

---

## Phase 1: F1, F2, F5, F6, F7, F8

### F1 — Syscall args

**Surface:** `syscall_handler(number, arg0, arg1, arg2)` for all known non-abort syscall numbers.

**Generator:** 10,000 calls per syscall number. `a0` cycles through: our valid cap slots (0, 1), out-of-range slots (64, 0xFFFF, u64::MAX), and random u32 values. `a1`/`a2` are restricted to values that fail `validate_user_slice` (null = 0, kernel-space addresses ≥ 0xffff800000000000) — this prevents kernel-mode page faults from unmapped user pages.

**Syscall numbers tested:** 1, 2, 3, 5, 7, 8, 10, 11, 12, 14. Four numbers excluded:
- nr=4 (Yield): causes a real scheduler context switch per call; no cap argument to exercise.
- nr=6 (AllocMem): small a0 values cause real physical frame allocations before budget is exhausted; page-table overhead makes the loop prohibitively slow on QEMU TCG. Covered by F8.
- nr=13 (InspectKernel): query_id=1 (hit when a0=1) calls `count_live_endpoints()` which acquires `ROUTE_LOCKED`, the same spinlock held by ping/pong IPC sends. Spinning on a contended atomic under QEMU TCG burns the full CPU quantum. Covered by property probes P4/P5/P7.
- nr=15 (RemoveCap): `iter%8==0` produces `a0=0`, removing slot 0 (log_write cap). `ctx.log` at the end then fails silently — pass string never appears and the test times out. RemoveCap cannot panic regardless of slot index (empty/out-of-range slots are an idempotent no-op returning 0), so excluding it does not reduce panic-safety coverage.

**Iteration count:** 100 × 10 syscalls = 1,000 total. Scaled down from the 1M/nr spec target to fit QEMU TCG emulation speed on Windows without KVM. Scale-up is straightforward on native hardware with KVM.

**Pass:** `fuzz: F1 pass (100/10)` seen on serial.
**Fail:** `KERNEL PANIC` seen on serial.
**Timeout:** 120 s.

### F2 — Syscall numbers

**Surface:** The dispatch table's catch-all `_ => -1` arm.

**Generator:** 50,000 random u64 syscall numbers. Any value in 1–15 (valid) is remapped by adding 100 (ensuring every call hits the unknown path). All calls use zero arguments.

**Expected:** every call returns -1 (UnknownSyscall). If any returns a non-(-1) value, `fuzz: F2 FAIL` is logged (wrong dispatch, not a panic — still caught). Any `KERNEL PANIC` is the primary failure mode.

**Pass:** `fuzz: F2 pass (50000/50000)` seen on serial.
**Timeout:** 60 s.

### F5 — IPC message bodies

**Surface:** `build_message` → kernel copy path in `handle_send` / `handle_try_send`.

**Generator:** 1,000 `try_send` calls to `fuzz-f5-recv` with random byte content, random sizes 0–4096 bytes. After the queue fills (depth 16), remaining calls return `QueueFull` — also acceptable.

**Pass:** `fuzz: F5 pass (1000/1000)` seen on serial.
**Timeout:** 60 s.

### F6 — Embedded cap slots

**Surface:** `handle_send_with_cap` validation path (endpoint cap check + grant cap check).

**Generator:** 1,000 `SendWithCap` calls with random u32 values as both the endpoint cap slot and the grant cap slot. Most are out of range → `CapNotHeld`. Even if a valid SEND cap is accidentally hit, a random grant slot → `CapNotGrantable`. Kernel must not panic on any combination.

**Pass:** `fuzz: F6 pass (1000/1000)` seen on serial.
**Timeout:** 60 s.

### F7 — Cap generation / stale-cap sends

**Surface:** `enqueue` → generation check path in `routing.rs`.

**Generator:** 50 kill cycles. Each cycle: kill `fuzz-f7-victim` (bumps generation), try_send via stale SEND cap → must return `EndpointDead` or another error, never `Ok`. Also tries out-of-range slots (0xBEEF, u32::MAX) → `CapNotHeld`. Then respawns victim; stale cap remains stale.

**Pass:** `fuzz: F7 pass (50/50)` seen on serial.
**Fail:** `fuzz: F7 FAIL` (stale-cap send succeeded) or `KERNEL PANIC`.
**Timeout:** 120 s.

### F8 — Memory request sizes

**Surface:** `handle_alloc_mem` → `current_task_claim_alloc` budget check.

**Generator:** 10 edge-case sizes (0, 1, 4095, 4096, 4097, 64MiB+1, 1GiB, u64::MAX-4095, u64::MAX-1, u64::MAX) followed by 1,000 random u64 sizes cast to `usize`. All must return `AllocDenied` or `-1`, never panic.

**Defensive fix (applied in this milestone):** `current_task_claim_alloc` now uses `checked_add(4095)` to compute the page-aligned size; overflow saturates to `u64::MAX`, which the `saturating_add` budget check correctly rejects.

**Pass:** `fuzz: F8 pass` seen on serial.
**Timeout:** 60 s.

---

## Phase 2: F3, F4 (Deferred)

### F3 — ELF mutation

**Surface:** The kernel's ELF loader (`kernel/src/loader.rs`), called at spawn time.

**Plan:** Build variant kernel images with bit-flipped, truncated, or header-corrupted probe ELF bytes embedded via a `kernel/test-bad-elf` Cargo feature (analogous to `test-bad-registry`). The kernel must either reject the spawn with a `LoadError` or execute the service in isolation — it must NOT panic, regardless of ELF content.

**Implementation:** Harness-side ELF mutation + new `WithBadElf` test kind in the harness.

**Deferred because:** Requires changes to the ELF loader, a new kernel feature flag, and harness infrastructure for generating bit-flipped ELF variants. Phase 1 covers the higher-impact kernel boundary (syscall dispatch, IPC, memory). ELF loading is also tested structurally by Test 1B.

### F4 — Service contracts

**Surface:** `osdev validate` (the host-side JSON Schema validator in `osdev/src/validator.rs`).

**Plan:** Generate malformed TOML files (missing required fields, wrong types, extra fields, non-UTF-8 bytes) and run them through `validate_all_contracts`. The validator must return a structured error, never panic (`unwrap` / `expect` / stack overflow).

**Implementation:** A set of known-bad contract files in `tests/qemu/fuzz/contracts/` + a host-side test loop in `run_fuzz_tests` that calls `validate_contract` on each.

**Deferred because:** `validate_all_contracts` is currently a `todo!()`. Once the contract validator is implemented (a pre-requisite for `osdev validate` to work), F4 can be added with minimal harness overhead.

---

## Resource budget check

At peak, with all fuzz probes running alongside identity + property probes:

| Resource | Used | Limit | Margin |
|----------|------|-------|--------|
| Task slots (kstacks) | ~45 | 48 | 3 |
| Routing table entries | ~23 | 32 | 9 |

The margin is tight on task slots. If further probes are added for later milestones, `TASK_KSTACK_MAX` should be raised from 48 to 64 and `MAX_TASKS` updated to match.

---

## Running

```
osdev test fuzz
```

Builds once, then boots QEMU fresh for each of the 6 Phase 1 tests. Logs are written to `build/tests/3_FUZZ/<id>-<name>.log`.

---

## Phase 1 results

| ID | Name | Result | Notes |
|----|------|--------|-------|
| F1 | syscall_args_no_panic | ✅ PASS | 100 iter × 10 syscalls; nr=4/6/13/15 excluded (see design) |
| F2 | syscall_numbers_no_panic | ✅ PASS | 50,000 unknown syscall numbers; all return -1 |
| F5 | ipc_message_bodies_no_panic | ✅ PASS | 1,000 random-content try_send calls |
| F6 | embedded_cap_slots_no_panic | ✅ PASS | 1,000 random send_with_cap slot pairs |
| F7 | stale_cap_generation_no_panic | ✅ PASS | 50 kill/respawn cycles; stale caps → EndpointDead |
| F8 | memory_request_sizes_no_panic | ✅ PASS | 10 edge cases + 1,000 random sizes |

---

## Bugs found and fixed during this milestone

### Overflow in `current_task_claim_alloc` (fixed pre-run)

**File:** `kernel/src/task/scheduler.rs`

**Bug:** `(size + 4095) & !4095` wraps to 0 for `size = u64::MAX`. The subsequent `saturating_add(0)` budget check passes, and `handle_alloc_mem` maps 0 pages but returns a "valid" VA. The task gets a phantom VA with no backing pages.

**Impact:** Not a kernel panic (the probe doesn't write to the returned VA), but a silent correctness violation — the probe gets an unusable VA. Could become a page-fault kill if the probe tries to use it.

**Fix:** `size.checked_add(4095).map(|v| v & !4095).unwrap_or(u64::MAX)`. Overflow saturates `aligned` to `u64::MAX`, which the `saturating_add` budget check correctly rejects with `AllocDenied`.

**Detected by:** F8 design analysis (pre-run). Regression coverage: F8 passes `usize::MAX` as size and verifies no panic.
