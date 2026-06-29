# Post-v1 Item 6 - Subsystem-Level Property Tests

**Status:** ✅ Complete  
**CI:** runs as part of `cargo test -p kernel --lib` in the existing `build` workflow  
**Evidence:** `build/tests/post_v1/6_SUBSYSTEM_PROPERTY_TESTS/`

---

## Overview

Item 5 added property tests at the primitive level (individual types: `Generation`,
`Rights`, `Capability`, `Message`, `Queue`). Item 6 extends coverage to the
*subsystem* level - testing the algorithmic invariants of complete subsystems under
arbitrary operation sequences:

- **`capability/table.rs`**: `CapTable` (per-task slot store) and `GlobalResourceTable`
  (kernel-wide resource registry) - the two structures that actually enforce the
  capability model at runtime.
- **`memory/bitmap.rs`**: `TestBitmapAllocator` - a host-compilable model of the
  physical frame allocator, exercising alloc/free invariants without hardware
  dependencies.

Subsystem-level tests catch bugs that primitive tests miss: state corruption across
sequences of operations, interaction between structures, invariants that only surface
after multiple operations.

---

## New modules

### `capability/table.rs` - §7.8, §22 P2, P8, P9

`GlobalResourceTable` and `CapTable` are tested with LOCAL instances (no global
`GLOBAL_RESOURCES` state touched). `GlobalResourceTable` is `Box<>`-allocated to
avoid placing its ~73 KiB body on the test stack.

#### `GlobalResourceTable` properties

| Property | What it pins |
|----------|-------------|
| `registered_resource_is_findable_at_gen_zero` | After `register(id)`, `get_record(id)` returns `Some` with gen=0, liveness=Alive (§7.5) |
| `unregistered_resource_not_found` | Before `register`, `get_record` returns `None` - no phantom lookups |
| `bump_generation_is_strictly_monotonic` | 1–8 consecutive bumps are strictly increasing per resource (§7.5 P2) |
| `bump_to_dead_sets_liveness_dead` | After `bump_generation(Dead)`, liveness is `Dead` → `EndpointDead` on next cap use |
| `bump_to_revoked_sets_liveness_revoked` | After `bump_generation(Revoked)`, liveness is `Revoked` → `CapRevoked` on next cap use |

#### `CapTable` properties

| Property | What it pins |
|----------|-------------|
| `cap_table_insert_within_capacity` | Inserting n ≤ 64 caps always succeeds (§7.8) |
| `cap_table_full_rejects_next_insert` | After 64 inserts the table is full; 65th insert fails |
| `remove_is_idempotent_after_first_call` | First `remove` returns `Some`; second returns `None` - no double-removal |
| `inserted_slot_is_within_bounds` | Every slot returned by `insert` is `< MAX_CAPS_PER_TASK` |

---

### `memory/bitmap.rs` - §10, §22 item 6.1

`TestBitmapAllocator` is a heap-backed model of `memory/allocator.rs`'s
`BitmapAllocator`. It exercises the same algorithmic invariants with a
`Vec<bool>`-backed bitmap so property tests complete in milliseconds on the host.
This module is only compiled in test mode (gated via `#[cfg(test)] mod memory`
in `lib.rs`).

| Property | What it pins |
|----------|-------------|
| `count_always_sums_to_total` | `live_count + free_count == max_frames` after any alloc/free sequence (§10.3) |
| `live_allocations_never_overlap` | No two outstanding allocations ever return the same frame index - uniqueness |
| `alloc_never_exceeds_max_valid_frame` | `alloc()` never returns a frame index ≥ `max_frames` - bounds safety |
| `free_all_returns_to_initial_state` | After freeing all live frames the allocator is fully free - recovery |
| `phantom_frame_free_is_silently_rejected` | `free(idx >= max_frames)` returns `false` and leaves state unchanged - guard rail |

---

## Test count impact

| Module | Before item 6 | Property tests added | Total |
|--------|--------------|---------------------|-------|
| `capability/table.rs` | 0 | 9 | 9 |
| `memory/bitmap.rs` | 0 (new file) | 5 | 5 |
| **Cumulative total** | **50** | **14** | **64** |

Each property test runs 256 random cases per CI run. The bitmap tests use operation
sequences up to length 128 against a 64-frame allocator; the table tests vary IDs
across the full `DIRECT_CAP` (8192) range and bump counts 1–8.

---

## Architecture notes

### Why `TestBitmapAllocator` instead of the real allocator

`memory/allocator.rs` contains `unsafe` code, hardware-dependent types (`BootInfo`),
serial write calls, and global statics - none of which compile on the host. The
model allocator is algorithmically equivalent (first-fit over a boolean bitmap) and
exercises exactly the invariants that property tests care about, without any hardware
dependency.

### Why `GlobalResourceTable` is boxed

The struct contains `[ResourceRecord; 8192]` + `[bool; 8192]` ≈ 73 KiB. Placing
this on the stack inside a proptest lambda would overflow. Boxing moves it to the
heap, which is correct and expected for host-side tests.

### No global state in these tests

All `GlobalResourceTable` property tests create fresh local instances. The actual
`GLOBAL_RESOURCES` static in `table.rs` is never touched. This keeps the tests
hermetic and avoids inter-test state leakage.

---

## CI integration

Property tests registered in `table.rs` and `bitmap.rs` run with `cargo test -p
kernel --lib` - no new workflow or job. They appear in the `test_report.py` Unicode
table alongside all other tests.

---

## Relationship to other items

| Item | What it adds |
|------|-------------|
| 1 - Coverage | Lines/branches exercised |
| 2 - Unsafe audit | Every unsafe block audited |
| 3 - Static analysis | CVEs, UB, unsafe surface |
| 4 - Mutation testing | Logic holes in primitive operations |
| 5 - Property tests | Universal invariants on primitives |
| **6 - Subsystem property tests** | **Universal invariants on composite structures and operation sequences** |
| 7 - Soak / stress tests | Drift, leaks, corruption under sustained load |
