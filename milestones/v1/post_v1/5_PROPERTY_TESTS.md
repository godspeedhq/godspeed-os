# Post-v1 Item 5 - Subsystem Property Tests

**Status:** ✅ Complete  
**CI:** runs as part of `cargo test -p kernel --lib` in the existing `build` workflow  
**Evidence:** `build/tests/post_v1/5_PROPERTY_TESTS/`

---

## Overview

Property tests assert *universal* claims over randomised inputs. The existing 32
unit tests prove that specific chosen inputs satisfy the spec. Property tests prove
the spec holds for **any** input the generator can construct - typically 256 cases
per property per CI run, shrunk automatically on failure.

This closes the gap between "correct for the examples I thought of" and "correct
for all inputs", catching a different class of bug than coverage or mutation
testing.

---

## Tool

`proptest = "1"` added as a `[dev-dependencies]` entry in `kernel/Cargo.toml`.
Dev-dependencies are only compiled when running `cargo test`; the bare-metal
`[[bin]]` target has `test = false` so proptest never enters the no_std build.

---

## Properties added

### `capability/generation.rs` - §22 P2 (generation monotonicity)

| Property | What it pins |
|----------|-------------|
| `bump_increments_by_one` | For any `v < u32::MAX`, `Generation(v).bump().0 == v + 1` - strictly monotonic |
| `matches_iff_values_equal` | `a.matches(b)` iff `a == b` - no false equality |
| `stale_cap_always_rejected_after_bump` | A cap at generation `v` is always stale after one bump - cross-core revocation correctness |

### `capability/rights.rs` - §22 P3 (rights non-escalation)

| Property | What it pins |
|----------|-------------|
| `narrow_result_equals_bitwise_and` | `narrow(a, b).0 == a.0 & b.0` - narrow is strictly AND |
| `contains_is_bitwise_subset` | `contains(a, b) iff (a & b) == b` - subset semantics |
| `union_is_superset_of_both_operands` | `union(a, b)` contains both inputs |
| `all_contains_every_valid_right` | `Rights::all()` is the top element |
| `empty_never_contains_nonzero_right` | `Rights::empty()` is the bottom element |

### `capability/cap.rs` - §22 P1, P3, P9

| Property | What it pins |
|----------|-------------|
| `gen_mismatch_always_returns_error` | ANY generation mismatch → `GenerationMismatch` regardless of rights (P9 - all holders invalidated) |
| `matching_gen_and_held_right_passes` | Matching gen + held right always succeeds - positive path (P1) |
| `narrow_for_grant_never_widens` | Result rights are always a strict subset of original (P3 - §7.3 unforgeable) |

### `ipc/message.rs` - §8.5 (message size enforcement)

| Property | What it pins |
|----------|-------------|
| `new_accepts_any_payload_within_limit` | Any `len ≤ 4096` is accepted |
| `new_rejects_oversized_payload` | Any `len > 4096` is rejected |
| `payload_bytes_round_trips` | Arbitrary payloads survive the copy without corruption |

### `ipc/queue.rs` - §22 P6 (queue invariants)

| Property | What it pins |
|----------|-------------|
| `depth_never_exceeds_capacity` | `depth() ≤ QUEUE_DEPTH` after any enqueue/dequeue sequence |
| `full_and_empty_flags_consistent_with_depth` | `is_full() == (depth == 16)` and `is_empty() == (depth == 0)` always |
| `fifo_order_preserved_for_any_fill` | FIFO order holds for any fill level 1-16 |
| `drain_always_yields_empty` | `drain()` always leaves `is_empty() == true` |

---

## Test count impact

| Module | Unit tests (before) | Property tests added | Total |
|--------|--------------------|--------------------|-------|
| `generation.rs` | 6 | 3 | 9 |
| `rights.rs` | 9 | 5 | 14 |
| `cap.rs` | 7 | 3 | 10 |
| `message.rs` | 0 | 3 | 3 |
| `queue.rs` | 10 | 4 | 14 |
| **Total** | **32** | **18** | **50** |

Each property test runs 256 random cases by default (`PROPTEST_CASES` env var
overrides). CI runs 256 × 18 = 4608 additional test iterations - still completes
in seconds on the host.

---

## CI integration

Property tests are regular `#[test]` functions (via the `proptest!` macro) that
run with `cargo test -p kernel --lib`. They appear in the `test_report.py` table
alongside unit tests. No separate workflow or job needed.

Failures produce a human-readable message showing the first failing case and the
shrunk minimal counter-example (printed to stdout even with `panic = "abort"`
because `prop_assert!` uses `Err` return, not `panic!`).

---

## Relationship to other items

| Item | What it adds |
|------|-------------|
| 1 - Coverage | Lines/branches exercised |
| 2 - Unsafe audit | Every unsafe block audited and tested |
| 3 - Static analysis | CVEs, UB, unsafe surface |
| 4 - Mutation testing | Logic holes the test suite misses |
| **5 - Property tests** | **Universal invariants hold for all inputs, not just chosen examples** |
| 6 - Stress tests | Drift, leaks, corruption under sustained load |
