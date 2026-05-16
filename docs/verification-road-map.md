# Post-v1 Verification Roadmap

> **Status:** Non-normative. Records the verification work that follows the 130-test suite (§22 of CLAUDE.md). Not a replacement for the constitution; a sequence of concrete activities that raise rigor without bolting on certification overhead.
>
> **Context:** GodspeedOS v1 shipped with all 130 tests passing across seven categories (identity, property, fuzz, stress, performance, adversarial, chaos), each with brutal variants. The kernel is correct against its spec. The work below sharpens what is already there: finding what the tests miss, what QEMU hides, and what only emerges over time.
>
> **Progress (2026-05-16):** Items 1, 3–6 complete. Item 2 and items 7–9 deferred pending hardware arrival.

---

## Ordering

The list is in priority order for a solo developer. Each item compounds with the previous one. Don't skip ahead.

1. [Code coverage](#1-code-coverage) ✅
2. [Real hardware boot](#2-real-hardware-boot) — *deferred pending hardware*
3. [Unsafe audit](#3-unsafe-audit) ✅
4. [Static analysis](#4-static-analysis) ✅
5. [Mutation testing](#5-mutation-testing) ✅
6. [Subsystem-level property tests](#6-subsystem-level-property-tests) ✅
7. [Long-running soak](#7-long-running-soak) — *deferred pending hardware*
8. [External documentation pass](#8-external-documentation-pass) — *deferred*
9. [Formal subset analysis](#9-formal-subset-analysis) — *deferred*

---

## 1. Code Coverage

**Intent.** Measure where the 130 tests do *not* exercise the code. Use the gaps to find dead code (delete it) and underexercised paths (add tests).

**Why first.** Coverage tells you where to look. Every later activity benefits from knowing which lines, branches, and unsafe blocks are actually tested. It also surfaces dead code, which a kernel that prides itself on smallness should not carry.

**Tool.** `cargo llvm-cov`. Works on `no_std` kernels with some configuration. Standard, no other dependencies.

**Steps.**

1. Install: `cargo install cargo-llvm-cov`.
2. Wire it into the `osdev` test runner so each `osdev test <category>` invocation contributes to a merged coverage profile (`LLVM_PROFILE_FILE` per-test, then `cargo llvm-cov report` to merge).
3. Generate the merged report after a full run:
   ```
   cargo llvm-cov report --json > build/coverage.json
   cargo llvm-cov report --html --output-dir build/coverage-html
   ```
4. Inspect the report. Three things to look for:
   - **Dead code.** Lines with zero hits that no test could ever reach. Delete it.
   - **Untested unsafe blocks.** Any unsafe block with no test hit is the scariest unknown in the kernel. List them. Write a test for each before doing anything else in this roadmap.
   - **Underexercised branches.** Functions with line coverage but low branch coverage usually mean the error paths are unrun. Those are the paths that matter under fault injection.
5. Add a CI gate: coverage must not regress against the previous commit. The threshold is whatever your current baseline is. Tighten it over time.

**Done when.** A merged coverage report exists for the full test suite. Dead code has been removed. Every unsafe block has at least one test hit. CI fails on a drop.

**Time.** 1 day to wire up. A few more days to triage the first report and act on it.

**Expected findings.** Most kernel codebases find 10–20% dead code on first measurement. Expect to delete a meaningful amount.

---

## 2. Real Hardware Boot

> **Status:** Deferred. Hardware ordered (2026-05-16); resume when it arrives.

**Intent.** Find the bugs QEMU TCG hides. There will be at least one. Probably two.

**Why it matters.** QEMU TCG models the x86_64 ISA but not the timing or memory ordering of real silicon. Classes of bugs that only real hardware finds:

- **Cache coherence.** Real CPUs have weaker memory ordering than QEMU TCG effectively serializes to. Atomic operations and the cap-table `RwLock` are prime suspects.
- **TLB latency.** QEMU TLB shootdowns are essentially instant; real shootdowns take real time. If the protocol assumed instantaneous IPI completion, it breaks here.
- **APIC delivery.** Real IPIs have measurable latency. Any code path that assumes "IPI delivery is fast" gets tested for the first time.
- **Power management.** Real cores have C-states. The idle loop probably needs `hlt` or `mwait`, not a busy loop, or the CPU cooks.
- **Interrupt priority edge cases.** Real APICs have priority levels and pending vectors that QEMU simulates loosely.

**Hardware shopping list.**

- Used x86_64 machine. Any ThinkPad from the X220 onwards works (~$50–80 on eBay). Intel NUC family is the cheap-but-modern option (~$150 used). A spare desktop is free.
- USB-to-serial adapter (~$15). FTDI-based ones are reliable.
- USB stick (any 4 GB+ will do).

**Steps.**

1. Write the existing `build/os.img` to a USB stick:
   ```
   sudo dd if=build/os.img of=/dev/sdX bs=4M conv=fsync
   ```
2. On the target machine, enter firmware setup. Disable Secure Boot. Set USB as primary boot device.
3. Connect serial. The COM port mapping varies by motherboard. Some servers and mini-PCs route COM1 to a physical DE-9 connector; laptops usually require an internal pin header. Expect to spend an hour finding the right one for your machine.
4. Capture serial on the host side:
   ```
   screen /dev/ttyUSB0 115200
   ```
   (or whatever rate Limine + the kernel are configured for).
5. Boot. Capture the output.
6. Run the brutal identity suite on real hardware. Anything that diverges from QEMU is a finding.

**Done when.** Identity tests pass on at least one real x86_64 machine. Divergences between QEMU and hardware are catalogued in `docs/hardware-findings.md`.

**Time.** One weekend, mostly spent on firmware setup and finding the serial header.

**Expected findings.** At least one timing assumption broken. Possibly a memory ordering bug in the cap table. Almost certainly a power-management issue if the kernel's idle loop is a busy wait.

---

## 3. Unsafe Audit

**Intent.** Verify every unsafe block in the kernel has a SAFETY comment that matches the current code, and that the assumption is exercised by a test.

**Why.** Most kernel CVEs trace to unsafe code with stale assumptions. Code drifts; comments lag. §18 of CLAUDE.md requires SAFETY comments and an audit file, but the audit file is only as good as the last pass through it.

**Steps.**

1. Open `docs/unsafe-audit.md`. Confirm it matches every unsafe block currently in the kernel:
   ```
   grep -rn 'unsafe' kernel/src/ | wc -l
   ```
   Compare against the count in the audit file. Discrepancies are the first finding.

2. For each unsafe block:
   - Read the SAFETY comment out loud. Does it describe what the surrounding code *actually does*, or an older version?
   - Identify the smallest test that would fail if the SAFETY assumption became a lie. If no such test exists, write one.
   - If you cannot articulate what test would catch a violation, the unsafe block is hard to verify. Flag it in the audit file with a `TODO-VERIFY:` marker.

3. Count the total number of unsafe blocks. Track this number commit-to-commit. Every PR that adds an unsafe block needs to justify the increment.

4. Promote `docs/unsafe-audit.md` to a CI-checked artifact: a script verifies the file lists exactly the set of unsafe blocks present in source. Any mismatch fails CI.

**Done when.** Every unsafe block has a SAFETY comment, a corresponding test, and an audit-file entry. The count is tracked. CI rejects PRs that add unsafe without updating the file.

**Time.** 1 week, depending on how many unsafe blocks the kernel has.

**Expected findings.** A handful of SAFETY comments that no longer match the code. One or two unsafe blocks with no test that would catch a violation. Possibly one block that turns out not to need to be unsafe at all.

---

## 4. Static Analysis

**Intent.** Run every available analyzer that the Rust toolchain provides for free. Wire each into CI as a gate.

### 4.1 clippy

Treats warnings as errors. The `pedantic` and `cargo` groups catch idiomatic issues and dependency hygiene.

```
cargo clippy --workspace --all-targets -- \
  -D warnings \
  -W clippy::pedantic \
  -W clippy::cargo
```

**Expected findings.** Dozens of stylistic issues on first run. Fix or `#[allow(...)]` each one explicitly with a comment explaining why.

### 4.2 cargo-geiger

Counts unsafe surface area across the workspace. Useful as a CI signal: if the unsafe count grows, the audit (§3) must grow with it.

```
cargo install cargo-geiger
cargo geiger
```

**Expected findings.** A baseline number. Track over time.

### 4.3 cargo-audit

Checks dependency vulnerabilities against the RustSec advisory database.

```
cargo install cargo-audit
cargo audit
```

Run on every commit. The kernel depends on `limine` and a small number of crates; CVEs in any of them matter.

### 4.4 miri

Catches undefined behavior the compiler doesn't. miri cannot run a full kernel, but it can run *unit* tests of pure-logic subsystems: cap table operations, the bitmap allocator, IPC queue logic, generation arithmetic.

```
rustup +nightly component add miri
cargo +nightly miri test --package <pure-logic-crate>
```

Extract pure-logic subsystems as unit-testable units if they aren't already. Run those under miri.

**Done when.** Each tool runs in CI on every commit. Failures block merge.

**Time.** 1 day to set up all four. Ongoing maintenance is trivial — fix issues as they arise.

**Expected findings.** clippy finds dozens of issues. miri will likely find at least one UB issue in code that "passed all tests" because the compiler happened to handle it benignly. Those are real bugs.

---

## 5. Mutation Testing

**Intent.** Verify the tests actually test what they claim to. Mutation testing introduces tiny changes to the source — flipping `<` to `<=`, replacing `&&` with `||`, removing a line — and reports which mutations the test suite *fails to catch*. Each surviving mutation is a hole.

**Tool.** `cargo-mutants`.

**Steps.**

1. Install: `cargo install cargo-mutants`.
2. Run against a kernel module:
   ```
   cargo mutants --package godspeed-kernel
   ```
3. Triage the report. Each surviving mutation is one of:
   - **A test gap.** The mutation changes behavior, but no test detects the change. Add a test.
   - **A test redundancy.** The mutation does not actually change behavior (e.g., flipping a boundary in code unreachable from any test). Either delete the dead code (§1) or accept it.
   - **A tool false positive.** The mutation is in a macro expansion or generated code. Suppress via `.cargo/mutants.toml`.

4. Set a target: "mutation kill rate ≥ 80% on kernel modules." Track over time.

**Done when.** A mutation report exists for the kernel. Surviving mutations have been triaged and either filled with new tests or documented as accepted.

**Time.** Hours to set up. The full suite runs slowly (each mutation requires a full test pass). Set it to run overnight or weekly.

**Expected findings.** A kernel with 130 tests typically has 10–30 mutation holes on first measurement. This is the closest thing available to MC/DC coverage without paying for avionics-grade tooling.

---

## 6. Subsystem-Level Property Tests

**Intent.** Augment the existing property tests with algebraic assertions about specific subsystems, run in unit-test form rather than through the full kernel.

**Why.** The existing P1–P10 tests are *behavioral*: they spawn services and check outcomes. Algebraic property tests target a subsystem in isolation, run thousands of times faster, and exercise far more states. The bugs they find — order-of-operations issues, boundary conditions, state-explosion paths — are different from what behavioral tests catch.

### 6.1 Bitmap allocator

Properties to assert with `proptest`:

- For any sequence of `alloc`/`free` operations from any valid initial state, no two live allocations overlap.
- For any sequence ending in free-everything, the bitmap returns to its initial state.
- The bitmap is never inconsistent with the in-use frame count.
- `max_valid_frame` is never exceeded by any returned frame. (Regression: the phantom-frame bug fixed during Property Phase 3 would have been caught here.)

### 6.2 Cap table

Properties to assert with `proptest` (single-threaded model first, then a concurrent one):

- After any sequence of insertions, removals, and lookups, every key inserted-and-not-removed is findable.
- Generation values are strictly monotonic per slot.
- The table is never larger than its declared capacity.
- After revocation, no stale cap passes validation.

**Done when.** Each subsystem has a `tests/` directory with proptest assertions. The assertions run as part of `cargo test`. Failures are kernel bugs.

**Time.** A few days per subsystem.

**Expected findings.** Algebraic property tests on the allocator have caught real bugs in real kernels even when behavioral tests passed. Expect at least one finding.

---

## 7. Long-Running Soak

> **Status:** Deferred pending hardware arrival (2026-05-16). Requires real silicon for meaningful results; QEMU soak is not a substitute.

**Intent.** Find leaks, drift, and "works for a week, breaks at three" bugs that no fixed-duration test will catch.

**Setup.**

1. The brutal stress suite already runs for 24 hours (S6, S8). Extend the same harness to run indefinitely with periodic state-capture.
2. Define drift signals:
   - Memory usage over time
   - Cap table size
   - Routing table size
   - Generation counter values
   - Kstack pool usage
   - Per-benchmark perf numbers from a periodic re-run of §22 perf tests
3. Log a snapshot every hour. Drift = monotonic growth in any of these without a corresponding shrink.
4. Run on real hardware (post-§2) for at least one calendar month.
5. Configure the test machine to email or webhook on any kernel panic.

**What constitutes a finding.**

- Any panic during the run.
- Monotonic growth in a resource counter that does not stabilize.
- Drift in a metric that the spec claims is stable (generation should advance; cap table size should stabilize per workload).
- Performance degradation: numbers that drift indicate scheduler or allocator pathology.

**Done when.** The kernel has run for a calendar month without panic and without metric drift. Or: a finding has been recorded, the kernel has been fixed, and the clock has restarted.

**Time.** A month of wall clock. Zero developer time once it's running.

**Expected findings.** The "works for a week, breaks at three" class of bugs is real and surprisingly common in long-lived systems. Even if nothing breaks, the soak run produces the most credible single piece of evidence that the kernel is production-shaped.

---

## 8. External Documentation Pass

> **Status:** Deferred (2026-05-16).

**Intent.** Write the kernel down for someone who has never seen it. Force every invariant out of your head and onto the page.

**Why.** Solo developers compensate for missing peer review by writing for an imaginary collaborator. Every implicit invariant you discover during this exercise is a future bug that didn't happen.

**Deliverables to produce.**

- **Porting guide** (`docs/porting-guide.md`). "How to add a new service." Walk a hypothetical developer through writing a service contract, the service code, the build integration, and the test for it. You will discover undocumented build steps.

- **Driver-writing guide** (`docs/driver-guide.md`). "How to add a driver for a new device." This forces you to confront whatever ad-hoc mechanism currently exists for `hw_interrupt` / `hw_mmio` caps.

- **Subsystem deep-dives** (`docs/subsystems/`). For each of `kernel/capability/`, `kernel/ipc/`, `kernel/task/`, `kernel/smp/`: a short document that explains the invariants the module preserves and the invariants it relies on from other modules. These are your Low-Level Requirements written for humans rather than auditors.

- **Glossary expansion.** CLAUDE.md §24 has the core glossary. Extend it to cover every term that appears in source comments without being defined elsewhere.

**Done when.** A hypothetical new contributor could write a service, debug a kernel panic, and understand a subsystem from the documentation alone, without reading source.

**Time.** A week, possibly two.

**Expected findings.** At least one design inconsistency you had forgotten about. At least one invariant that exists in your head but is not written anywhere. At least one piece of code that you would re-design now that you have to explain it.

---

## 9. Formal Subset Analysis

> **Status:** Deferred (2026-05-16).

**Intent.** Pick one small kernel module and prove its invariants formally, not behaviorally. **Not the whole kernel. One module.**

**Why.** Formal verification of a full kernel is a multi-year research project. Formal verification of *one module* is a weekend exercise that teaches you which assumptions your code actually depends on. The point is the learning, not the certification.

**Candidates, in order of tractability.**

- **Capability generation logic.** Small, pure, no state shared outside the cap table. Properties: monotonicity, bumping invalidates all holders, generation never wraps within service lifetime.
- **IPC queue head/tail arithmetic.** Bounded, ring-buffer logic. Properties: `head ≤ tail ≤ head + capacity`; count is consistent with both pointers.
- **Bitmap allocator.** A bit more state, but pure-logic. Properties as listed in §6.1.

**Tools, in order of effort.**

- **proptest with formal invariants.** The lightest option. Already covered in §6.
- **kani.** A Rust verification tool that bounded-model-checks Rust programs. Genuinely usable on small modules. Install:
  ```
  cargo install --locked kani-verifier
  cargo kani setup
  ```
  Annotate functions with `#[kani::proof]` and let it search for counter-examples.
- **TLA+ or Alloy.** Real formal-method tools. Steep learning curve. Worth it if you want to model the cap-table concurrency or the cross-core IPC protocol abstractly. Not required.

**Done when.** One module has a written specification (in code, in TLA+, in kani, or in dedicated comments) of its invariants, and a proof or model-check that the implementation satisfies them.

**Time.** Open-ended. A weekend for the first attempt; longer if you adopt a full formal-methods tool.

**Expected findings.** Whatever you pick, you will find at least one assumption your code makes implicitly. Making it explicit is the entire value of the exercise.

---

## What This Roadmap Is Not

This document is not a path to DO-178C Level A certification, nor anywhere close. That requires formal requirements decomposition, tool qualification, independent verification by people other than the author, worst-case execution time bounds, and real-hardware testing on the actual target avionics platform — all of which are out of scope for a solo project.

This document *is* a path to "top 5% of OS hobby projects for verification rigor" without any of the cost overhead of formal certification. Most of the activities take days, not months. Most of the findings will be small. The compounding effect is large.

The hard part of building GodspeedOS is done. v1 ships, 130 tests pass, the architecture is sound. The work above sharpens what is already there.