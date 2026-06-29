# Post-v1 Item 10 - IPC Routing Property Tests (P5, P8, P10)

**Status:** ✅ Complete  
**CI:** runs as part of `cargo test -p kernel --lib` in the existing `build` workflow  
**Evidence:** 76/76 host-side tests pass (was 64 before this item)

---

## Overview

Items 5 and 6 added property tests for the primitive types (`Generation`, `Rights`,
`Capability`, `Message`, `Queue`) and composite structures (`CapTable`,
`GlobalResourceTable`, `TestBitmapAllocator`).

This item closes the remaining gaps in the P1–P10 property matrix by adding model-based
property tests for the IPC routing table (P5, P8, P10) and the name registry (P8).

| Property | Previously covered? | This item |
|----------|--------------------|-----------
| P1 | ✅ items 5/6 | - |
| P2 | ✅ items 5/6 | - |
| P3 | ✅ items 5/6 | - |
| P4 | ✅ item 6 | - |
| P5 | ❌ | ✅ `routing_model.rs` |
| P6 | ✅ item 5 | - |
| P7 | ❌ | deferred (see §P7 note) |
| P8 | ❌ | ✅ `routing_model.rs` + `names_model.rs` |
| P9 | ✅ items 5/6 | - |
| P10 | ❌ | ✅ `routing_model.rs` |

---

## New modules

### `ipc/routing_model.rs` - §8.3, §22 P5, P8, P10

`TestRoutingModel` mirrors the algorithmic invariants of `ipc/routing.rs` without
`SpinLock`, global statics, or hardware dependencies. Pattern mirrors
`memory/bitmap.rs` (item 6): a heap-backed model that exercises the same logic at
millisecond speed on the host.

**Important model constraint:** `run_ops` guards against registering an already-alive
endpoint. This mirrors the kernel's actual usage protocol - `spawn_service_with_config`
always calls `kill_endpoint` before re-registering (task/mod.rs:2314). Proptest
confirmed this guard is necessary: without it, `[Register(1), Register(1)]` would
produce duplicate alive entries - a real protocol violation that the test correctly
rejects.

#### P5 properties

| Property | What it pins |
|----------|-------------|
| `no_duplicate_alive_endpoint_ids` | After any spawn/kill sequence matching the kernel's protocol, no endpoint ID appears twice in the alive set - §8.3 |
| `count_live_consistent_with_iteration` | `count_live()` always equals `alive_ids().len()` - count function is consistent with iteration |

#### P8 properties

| Property | What it pins |
|----------|-------------|
| `kill_reregister_strictly_increases_generation` | Any number of kill+reregister cycles produce strictly increasing generations - §7.5, §14.2 |
| `stale_cap_rejected_fresh_cap_accepted_after_restart` | Old-gen cap fails; new-gen cap succeeds after kill+reregister - §7.5, §14.2 |

#### P10 properties

| Property | What it pins |
|----------|-------------|
| `enqueue_dead_endpoint_returns_endpoint_dead` | Enqueue on a dead endpoint always returns `EndpointDead`, never `Ok` - §8.6 |
| `enqueue_full_queue_returns_queue_full` | Enqueue on a full queue always returns `QueueFull`, never `EndpointDead` - §8.6 |
| `enqueue_alive_non_full_returns_ok` | Enqueue on alive, non-full queue always returns `Ok` - §8.6 |
| `enqueue_result_always_one_of_defined_outcomes` | After any mixed op sequence, enqueue always returns one of `{Ok, EndpointDead, QueueFull}` - never panics, never an unexpected variant - §22 P10 |

---

### `ipc/names_model.rs` - §14.2, §22 P8

`TestNameModel` mirrors `ipc/names.rs` register/lookup logic without `SpinLock` or
global statics.

| Property | What it pins |
|----------|-------------|
| `lookup_returns_most_recent_registration` | After `register(name, ep1)` + `register(name, ep2)`, lookup returns `ep2` - restart updates the mapping - §14.2, P8 |
| `registered_name_always_found` | A name registered once is always found - no phantom miss - §14.2 |
| `unregistered_name_returns_none` | A name never registered returns `None` - §14.2 |
| `distinct_names_each_get_own_entry` | N distinct names each get their own slot - no merge or loss - §14.2 |

---

## P7 note - deferred

**P7:** "After unmap + TLB shootdown, the page is unreadable from every core" (§10.5).

P7 requires verifying, from the kernel, that a specific physical page is not
accessible on a different core after the TLB shootdown completes. This cannot be
verified cleanly from userspace (the page fault handler kills the accessing service;
there is no "try-read-and-catch" from user code). A complete test would require
either kernel instrumentation (a kernel-side probe that attempts access and reports
the fault) or a QEMU memory-introspection plugin.

P7 is noted but deferred. It does not block this item or any other milestone.
The identity test suite already verifies the observable consequence (§10.3 test 7A/7B:
allocating beyond limit is denied; protection violations kill the service); the
missing piece is the cross-core TLB coherence verification.

---

## Test count impact

| Module | Before item 10 | Property tests added | Total |
|--------|---------------|---------------------|-------|
| `ipc/routing_model.rs` | 0 (new file) | 10 | 10 |
| `ipc/names_model.rs` | 0 (new file) | 4 | 4 |
| **Cumulative total** | **64** | **14** | **76** |  

Each property test runs 256 random cases per CI run (proptest default).

---

## Architecture notes

Both model files live in `kernel/src/ipc/` and are exposed in `kernel/src/lib.rs`
under `#[cfg(test)]` - the same mechanism as `memory/bitmap.rs`. The models use a
local `EndpointId(u64)` newtype rather than importing from `ipc::endpoint` because
`endpoint.rs` depends on `crate::task` which has hardware dependencies not available
in the host test binary.

---

## CI integration

Both model files' tests run with `cargo test -p kernel --lib` - no new workflow step.
They appear in the `test_report.py` table alongside all other library tests.
