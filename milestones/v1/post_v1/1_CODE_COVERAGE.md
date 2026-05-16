# Post-v1 Item 1 — Code Coverage

**Status:** ✅ Complete  
**Commands:**
```sh
cargo test -p kernel                          # run unit tests locally
cargo llvm-cov --package kernel --summary-only  # print coverage summary
python3 scripts/test_report.py                # rendered table (used by CI)
```
**CI workflow:** `.github/workflows/coverage.yml` (push to `main`)  
**Evidence:** `build/tests/post_v1/1_CODE_COVERAGE/` — local output directory; HTML report uploaded as `coverage-html` artifact in GitHub Actions

---

## Overview

Code coverage for GodspeedOS targets the **pure-logic kernel modules** only — the
subset of the kernel that has no dependency on hardware, architecture, or bare-metal
primitives and can therefore be compiled and executed on the host machine.

Coverage of the remaining kernel (arch, memory, SMP, syscall dispatch) requires a
running x86-64 machine or QEMU. That is measured indirectly by the identity test suite
(§22), which exercises those paths under a real boot. Tracking line coverage across a
QEMU serial session is out of scope for this item.

The boundary is enforced by the kernel's lib target: `kernel/src/lib.rs` re-exports
only the modules with no `unsafe`, no arch imports, and no hardware dependencies.
Anything that fails to compile on the host does not belong in the lib target.

---

## Phase 1 — Kernel lib target ✅

**Commit:** `e602794`

To run `cargo test -p kernel` on the host, the kernel crate needs a lib target compiled
under the host toolchain. The binary target (`kernel_main`) is `no_std no_main` and
cannot run on the host at all.

### Changes

- ✅ `kernel/src/lib.rs` — created; `#![cfg_attr(not(test), no_std)]`; re-exports
  `capability::{cap, generation, rights}` and `ipc::{message, queue}`
- ✅ `kernel/Cargo.toml` — added `[lib]` section (`name = "kernel"`, `path = "src/lib.rs"`);
  added `test = false` to `[[bin]]` so `cargo test` targets the lib, not the binary
- ✅ `kernel/build.rs` — linker script emission (`-Tkernel.ld`) made conditional on
  `TARGET == "x86_64-unknown-none"`; prevents a linker warning when `cargo test` runs
  on the host with the MSVC or GNU linker

### Modules in scope (pure logic — no hardware deps)

| Module | Path | Unsafe? |
|---|---|---|
| Capability rights | `kernel/src/capability/rights.rs` | No |
| Capability generation | `kernel/src/capability/generation.rs` | No |
| Capability core | `kernel/src/capability/cap.rs` | No |
| IPC message | `kernel/src/ipc/message.rs` | No |
| IPC queue | `kernel/src/ipc/queue.rs` | No |

### Modules intentionally excluded from lib target

| Module | Reason |
|---|---|
| `arch/x86_64` | Direct hardware access — APIC, GDT, IDT, page tables |
| `memory/` | Physical addresses, frame allocator — x86-64 bare metal |
| `capability/table.rs` | Uses a global `RwLock` backed by `smp/` spinlock |
| `smp/` | APIC MMIO, IPI — no-op outside bare metal |
| `syscall/` | Dispatches into arch and memory layers |
| `task/` | Embeds service ELF bytes via `include_bytes!` at compile time |
| `interrupt/` | IDT registration — arch-only |

---

## Phase 2 — Unit tests ✅

**Commit:** `e602794`

32 unit tests written across the four testable modules. Every test lives in a
`#[cfg(test)] mod tests` block inside its module file. Tests target both the
positive path (the system permits what it should) and the negative path (the system
refuses what it shouldn't), matching the convention of the §22 identity tests.

### `capability::rights` — 9 tests

| Test | What it pins |
|---|---|
| `contains_single_right` | A right bitfield contains its own bit |
| `contains_subset` | A superset contains a strict subset |
| `contains_all_is_superset_of_everything` | `ALL` contains every defined right |
| `narrow_never_widens` | `narrow()` cannot produce rights the source lacks |
| `narrow_to_empty_yields_empty` | Narrowing to `EMPTY` always produces `EMPTY` |
| `narrow_is_idempotent` | Applying `narrow` twice produces the same result |
| `union_is_superset` | `union()` produces a superset of both operands |
| `bitor_operator_matches_union` | `|` operator is equivalent to `union()` |
| `empty_contains_nothing` | `EMPTY` contains no right |

### `capability::generation` — 6 tests

| Test | What it pins |
|---|---|
| `initial_is_zero` | `Generation::INITIAL` has value zero |
| `bump_is_monotonic` | Each `bump()` strictly increases the counter |
| `matches_same_value` | A generation matches itself |
| `does_not_match_different_value` | Two different generations do not match |
| `stale_cap_detected_after_bump` | A cap at old generation fails after a bump |
| `many_bumps_stay_monotonic` | 1,000 bumps remain strictly increasing |

### `capability::cap` — 7 tests

| Test | What it pins |
|---|---|
| `validate_ok_with_matching_gen_and_right` | Valid cap + matching generation + held right → `Ok` |
| `validate_fails_on_generation_mismatch` | Stale generation → `CapRevoked` |
| `validate_fails_on_insufficient_rights` | Right not held → `CapInsufficientRights` |
| `validate_checks_gen_before_rights` | Generation check takes priority over rights check |
| `narrow_for_grant_reduces_rights` | Narrowing removes the GRANT bit as expected |
| `narrow_for_grant_preserves_resource_and_gen` | Narrowing does not alter `ResourceId` or generation |
| `validate_subset_right_passes` | A cap with a superset of rights satisfies a subset check |

### `ipc::queue` — 10 tests

| Test | What it pins |
|---|---|
| `new_queue_is_empty` | Fresh queue has depth 0 |
| `enqueue_dequeue_single` | Single message round-trips correctly |
| `fifo_order_preserved` | N messages dequeue in the same order they were enqueued |
| `full_queue_rejects_enqueue` | 17th enqueue on a depth-16 queue returns `QueueFull` |
| `empty_queue_dequeue_returns_none` | Dequeue on empty queue returns `None` |
| `depth_tracks_enqueue_dequeue` | `depth()` is consistent with every enqueue and dequeue |
| `drain_empties_queue` | `drain()` removes all messages and leaves depth 0 |
| `wraparound_preserves_fifo` | FIFO order holds across a head/tail index wraparound |
| `queue_head_tail_invariant_depth_le_capacity` | `depth ≤ 16` holds after any sequence of operations |
| `reset_clears_without_drain` | `reset()` zeroes depth without iterating messages |

---

## Phase 3 — CI workflows ✅

**Commits:** `e602794`, `12d6e20`

### `coverage.yml` — coverage on push to main

- ✅ Installs `cargo-llvm-cov` (locked version)
- ✅ Generates LCOV report → `build/coverage.lcov`
- ✅ Generates HTML report → `build/coverage-html/`
- ✅ Uploads HTML report as `coverage-html` artifact in GitHub Actions
- ✅ Prints summary table to the CI log via `--summary-only`
- ✅ Trigger: push to `main` and `workflow_dispatch`

### `build.yml` — unit tests on every push

- ✅ Unit test step replaced with `python3 scripts/test_report.py`
- ✅ Renders a Unicode-bordered table in the CI log:
  Phase | Test | Result | ms — one row per test, grouped by module
- ✅ Exit 0 on all-pass, exit 1 on any failure — CI step fails correctly

### `scripts/test_report.py`

- ✅ Runs `cargo test -p kernel -- -Z unstable-options --format json`
- ✅ Parses JSON test events (`type: test`, `event: ok/failed`)
- ✅ Maps module paths to human labels (`capability::cap` → `Capability`, etc.)
- ✅ Renders grouped Unicode table with pass/fail summary footer
- ✅ Column widths computed dynamically from actual test names

---

## Phase 4 — README ✅

**Commit:** `7ce6108`

- ✅ `README.md` — replaced two-line stub with full project introduction:
  what GodspeedOS is, five core principles, what it is not, how it works,
  repo layout, getting started commands, CI table, pointer to `CLAUDE.md`

---

## Coverage scope summary

| Layer | Covered by unit tests | Covered by identity tests |
|---|---|---|
| `capability::rights` | ✅ 9 tests | ✅ exercised by cap enforcement (§22 Test 2) |
| `capability::generation` | ✅ 6 tests | ✅ generation bump on every restart (§22 Tests 6, 10) |
| `capability::cap` | ✅ 7 tests | ✅ cap validation on every syscall |
| `ipc::message` | (lib target; no dedicated tests yet) | ✅ IPC send/recv (§22 Tests 3, 4, 9) |
| `ipc::queue` | ✅ 10 tests | ✅ queue depth, full, drain (§22 Tests 4, 9) |
| `arch/x86_64` | — not host-testable | ✅ entire boot sequence |
| `memory/` | — not host-testable | ✅ alloc limit (§22 Tests 7A, 7B) |
| `smp/` | — not host-testable | ✅ 4-core boot (§22 Test 1A) |
| `syscall/` | — not host-testable | ✅ every kernel call path |
| `task/` | — not host-testable | ✅ spawn, kill, restart (§22 Tests 1, 6, 10) |

---

## Implementation checklist

- ✅ `kernel/src/lib.rs` — pure-logic lib target
- ✅ `kernel/Cargo.toml` — `[lib]` section; `test = false` on `[[bin]]`
- ✅ `kernel/build.rs` — linker arg conditional on `TARGET`
- ✅ `kernel/src/capability/rights.rs` — 9 unit tests
- ✅ `kernel/src/capability/generation.rs` — 6 unit tests
- ✅ `kernel/src/capability/cap.rs` — 7 unit tests
- ✅ `kernel/src/ipc/queue.rs` — 10 unit tests
- ✅ `.github/workflows/coverage.yml` — `cargo-llvm-cov` CI workflow
- ✅ `.github/workflows/build.yml` — unit test step calls `scripts/test_report.py`
- ✅ `scripts/test_report.py` — Unicode table renderer
- ✅ `README.md` — project identity and philosophy
- ✅ `build/tests/post_v1/1_CODE_COVERAGE/` — output directory for local reports
