# tests/qemu/

All QEMU-based tests. Tests in this tree boot the OS in QEMU; they are integration tests, not unit tests.

## Subdirectories

| Directory     | Purpose                                        | Status          |
|---------------|------------------------------------------------|-----------------|
| `identity/`   | Constitutional identity tests (§22) - 20/20 ✅ | Complete        |
| `harness/`    | Shared QEMU launcher, serial reader, runner    | -               |
| `perf/`       | Performance benchmarks (B1-B10, BP1-BP10)      | ✅ 10/10 + 10/10 brutal |
| `property/`   | Property tests (P1-P10, §22)                   | Active          |
| `fuzz/`       | Fuzz tests (F1-F8, §22)                        | Active          |
| `stress/`     | Stress scenarios (S1-S10, §22)                 | Active          |
| `adversarial/`| Red-team / cap isolation tests (A1-A10, §22)   | ✅ 10/10 + 10/10 brutal |
| `chaos/`      | Chaos / partial-failure tests (C1-C7, §22)     | ✅ 7/7 + 7/7 brutal |

## Running the identity suite

```bash
osdev test identity
```

Builds the kernel + test service images, boots each test in QEMU with `-smp 4`, reads serial output, and reports PASS/FAIL. On Linux CI runners with KVM (`/dev/kvm` accessible), each test completes in <30 s. On Windows TCG (software emulation), timeouts scale up to 300 s for the most complex tests.

## Test sequencing (§22.4)

1. Write test specifications (§22 is the spec).
2. Build minimum kernel + harness.
3. See tests fail for the right reasons (missing features, not harness bugs).
4. Implement until they pass.

A test failing due to a compile error or harness bug is a test failure, not a kernel failure.

## KVM vs TCG

The harness detects `/dev/kvm` at runtime. When KVM is available it passes `-enable-kvm -cpu host` to QEMU; otherwise it falls back silently to TCG. On GitHub Actions `ubuntu-latest` runners KVM has been available since 2023 - CI always runs KVM. On Windows, TCG is the only option; per-test timeouts are generous enough to cover worst-case TCG timing.
