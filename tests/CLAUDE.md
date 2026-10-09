# tests/

All tests for the OS. Tests run on two platforms: QEMU (automated harness) and real x86_64 hardware (manual flash-boot-observe).

## Structure

```
tests/
  qemu/
    identity/    # CLAUDE.md only: maps each §22 Test to its entry (the tests live in osdev/src/validator.rs)
    harness/     # CLAUDE.md only: the harness itself is osdev/src/qemu.rs + validator.rs
    perf/        # baseline.json (written by `osdev test perf`) + CLAUDE.md
    post_v1/     # verification-roadmap notes (coverage, unsafe audit, static analysis, mutation, ...)
    1_IDENTITY/ ... 14_CHAOS_BRUTAL/   # committed serial logs from past suite runs - SNAPSHOTS, not
                 # tests, and not current: a run writes its logs to build/tests/<same name>/, and the
                 # committed copies predate later cases (no Test 11/15, no A11-A15, no C1B)
  hardware/
    x86_64/      # Real hardware - 4-core x86_64, ~3 GHz, UEFI USB boot, null modem serial
      1_IDENTITY.md
      2_PROPERTY.md
      3_FUZZ.md
      4_STRESS.md
      5_PERFORMANCE.md
      6_ADVERSARIAL.md
      7_CHAOS.md
      12_PERFORMANCE_BRUTAL.md
  conformance/   # GALLERY.md + ui/*.case - `osdev conform --selftest` checks the rendered output
```

There are no `property/`, `fuzz/`, `stress/`, `adversarial/` or `chaos/` directories: every suite is
a `TestSpec` table in `osdev/src/validator.rs`, or a function in `osdev/src/shell_test.rs` / `main.rs`.

## Test categories (§22.2)

| Category    | Purpose                                           | Status              |
|-------------|---------------------------------------------------|---------------------|
| Identity    | Pin constitutional decisions (§22)                | 24 cases (+ Tests 12-14 as their own subcommands) |
| Property    | Universal invariants under random inputs          | Active              |
| Fuzz        | Crash resistance on adversarial/malformed inputs  | Active              |
| Stress      | No drift, leak, or corruption under sustained load| Active              |
| Performance | Latency / throughput baselines                    | ✅ 10/10 + 10/10 brutal |
| Adversarial | Capability isolation under direct attack          | A1-A15 + BA1-BA10 brutal |
| Chaos       | Graceful degradation under partial failures       | C1-C7 + C1B (8 cases) + BC1-BC7 brutal |

## Philosophy (§22.2)

Identity tests are the minimum set that, if any one fails, means the system is no longer the system `CLAUDE.md` describes. They are a prerequisite for all other categories: do not start property/fuzz/stress work until identity is 24/24.

The bar across every category is identical: **no FAIL, no BLOCKED**. A failure means a real bug - fix it, add a regression test, then move on.

## Running

```bash
osdev test identity          # run §22 identity suite (24 cases: Tests 1-11 + 15, + IR1A/B)
osdev test iommu             # §22 Test 12
osdev test fs-restart        # §22 Test 13
osdev test file-cap          # §22 Test 14
osdev test property          # run property tests (P1-P10)
osdev test fuzz              # run fuzz corpus (F1-F8)
osdev test stress            # run stress scenarios (S1-S10)
osdev test perf              # run benchmarks (B1-B10)
osdev test adv               # run red-team tests (A1-A15)
osdev test chaos             # run chaos scenarios (C1-C7, C1B)
```
