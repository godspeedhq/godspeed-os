# tests/

All tests for the OS. Tests run on two platforms: QEMU (automated harness) and real x86_64 hardware (manual flash-boot-observe).

## Structure

```
tests/
  qemu/
    identity/    # Constitutional identity tests (§22) - 20/20 complete ✅
    harness/     # QEMU launcher, serial parser, test runner
    perf/        # Performance benchmarks - ✅ 10/10 complete (§22 B1–B10), ✅ 10/10 brutal (BP1–BP10)
    property/    # Property tests - Active (§22)
    fuzz/        # Fuzz tests - Active (§22)
    stress/      # Stress tests - Active (§22)
    adversarial/ # Red-team / capability isolation tests - ✅ 10/10 complete + 10/10 brutal (§22)
    chaos/       # Chaos / partial-failure tests - ✅ 7/7 complete + 7/7 brutal (§22)
  hardware/
    x86_64/      # Real hardware - 4-core x86_64, ~3 GHz, UEFI USB boot, null modem serial
      1_identity.md
      2_property.md
      3_fuzz.md
      4_stress.md
      5_perf.md      # 5/10 brutal complete (BP3/BP4/BP7/BP8/BP10)
      6_adversarial.md
      7_chaos.md
```

## Test categories (§22.2)

| Category    | Purpose                                           | Status              |
|-------------|---------------------------------------------------|---------------------|
| Identity    | Pin constitutional decisions (§22)                | ✅ 20/20 complete   |
| Property    | Universal invariants under random inputs          | Active              |
| Fuzz        | Crash resistance on adversarial/malformed inputs  | Active              |
| Stress      | No drift, leak, or corruption under sustained load| Active              |
| Performance | Latency / throughput baselines                    | ✅ 10/10 + 10/10 brutal |
| Adversarial | Capability isolation under direct attack          | ✅ 10/10 + 10/10 brutal |
| Chaos       | Graceful degradation under partial failures       | ✅ 7/7 + 7/7 brutal  |

## Philosophy (§22.2)

Identity tests are the minimum set that, if any one fails, means the system is no longer the system `CLAUDE.md` describes. They are a prerequisite for all other categories: do not start property/fuzz/stress work until identity is 20/20.

The bar across every category is identical: **no FAIL, no BLOCKED**. A failure means a real bug - fix it, add a regression test, then move on.

## Running

```bash
osdev test identity          # run §22 identity suite (20 tests)
osdev test property          # run property tests (P1–P10)
osdev test fuzz              # run fuzz corpus (F1–F8)
osdev test stress            # run stress scenarios (S1–S10)
osdev test perf              # run benchmarks (B1–B10)
osdev test adversarial       # run red-team tests (A1–A10)
osdev test chaos             # run chaos scenarios (C1–C7)
```
