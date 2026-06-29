# Post-v1 Item 4 - Mutation Testing

**Status:** ✅ Complete  
**CI workflow:** `.github/workflows/mutation.yml` - `mutation` job (push to main + weekly)  
**Evidence:** `build/tests/post_v1/4_MUTATION_TESTING/`

---

## Overview

Mutation testing asks: "does the test suite actually detect logic errors?"

The tool (`cargo-mutants`) introduces tiny changes to source code - flipping `<`
to `<=`, replacing `&&` with `||`, deleting a `return` arm - and reruns the test
suite. A mutation that the tests *fail to catch* is called a **survivor**. Each
survivor is a code path the tests do not adequately cover.

This catches a different class of problem than coverage: a line can be executed
by many tests and still have a surviving mutation (if all tests happen to work
around the broken version).

---

## Scope

Only the five pure-logic modules in the lib target are mutated. Everything else
either requires the `no_std` / hardware environment or is the binary entry point
that cannot compile on the host.

| File | Tests that kill mutations |
|------|--------------------------|
| `capability/cap.rs` | 7 tests in `cap::tests` |
| `capability/generation.rs` | 6 tests in `generation::tests` |
| `capability/rights.rs` | 9 tests in `rights::tests` |
| `ipc/message.rs` | covered by `queue::tests` (all 10 use `Message`) |
| `ipc/queue.rs` | 10 tests in `queue::tests` |

Configuration: `kernel/mutants.toml` - `exclude_globs` removes every other file
in the kernel source tree so cargo-mutants only generates mutations from these five.

---

## Implementation

### `kernel/mutants.toml`

Scopes mutations to the five pure-logic files via `exclude_globs`. Sets a
`timeout_multiplier = 5.0` so slow compile cycles don't time out prematurely.

### `.github/workflows/mutation.yml`

Separate workflow from `build` (not triggered on every PR - mutations are slow).
Runs on:
- Every push to `main` - catches regressions as they land.
- Weekly schedule (Sunday 02:00 UTC) - catches drift from dependency upgrades.

Steps:
1. `rustup show` - activate pinned nightly toolchain.
2. Cache `~/.cargo/bin` with a dedicated `mutants-v1` key (separate from
   the `static-analysis` tool cache so they don't evict each other).
3. `cargo install cargo-mutants --locked` - install or skip if cached.
4. `cargo mutants --package kernel -- --lib` - run mutations; `--lib` routes the
   underlying `cargo test` invocations to the lib target only.
5. Upload `mutation-report.txt` and `mutants.out/` as a named artefact per commit.

The step uses `|| true` so the workflow does not hard-fail while the baseline
kill rate is being established. **Once the first run is triaged, remove `|| true`
to make CI strict.** The triage categories are:

| Category | Action |
|----------|--------|
| Surviving mutation changes observable behavior | Add a test that catches it |
| Surviving mutation is in unreachable code | Delete the dead code (item 1 done this) |
| False positive (macro expansion, generated code) | Add to `mutants.toml` exclude_globs |

---

## Kill rate target

Per the verification roadmap (§5): **≥ 80% kill rate on kernel modules.**

Expected approximate mutation counts on first run:

| File | Estimated mutations | Why |
|------|--------------------|----|
| `cap.rs` | ~15 | validate(), narrow_for_grant() - 2 logic branches each |
| `generation.rs` | ~10 | bump(), matches() - arithmetic and comparison |
| `rights.rs` | ~20 | contains(), narrow(), union(), BitOr - 3+ bit ops |
| `message.rs` | ~8 | new() size check, payload copy |
| `queue.rs` | ~25 | enqueue/dequeue/drain ring buffer arithmetic |

Total: ~80 mutations. Expected survivors on first run: 0–16 (80% kill rate floor).
A clean run (100%) is the real goal; 80% is the "fix before merging" threshold.

---

## Relationship to other items

| Item | Catches |
|------|---------|
| 1 - Coverage | Which lines are never reached |
| 2 - Unsafe audit | Unsafe blocks not justified or tested |
| 3 - Static analysis | CVEs, UB, unsafe surface |
| **4 - Mutation testing** | **Logic errors the test suite does not detect** |
| 5 - Property tests | Universal invariants (thousands of random inputs) |
| 6 - Stress tests | Drift, leaks, or corruption under sustained load |
