# tests/qemu/

All QEMU-based tests. Tests in this tree boot the OS in QEMU; they are integration tests, not unit tests.

## Subdirectories

| Directory     | Purpose                                        |
|---------------|------------------------------------------------|
| `identity/`   | CLAUDE.md mapping each §22 Test to its entry in `osdev/src/validator.rs` / `main.rs` |
| `harness/`    | CLAUDE.md describing the harness (the code is `osdev/src/qemu.rs` + `validator.rs`) |
| `perf/`       | `baseline.json`, rewritten by every `osdev test perf` run, + CLAUDE.md |
| `post_v1/`    | Verification-roadmap notes (coverage, unsafe audit, static analysis, mutation, property) |
| `1_IDENTITY/` ... `14_CHAOS_BRUTAL/` | Committed serial logs from past runs - snapshots, not tests. A run writes to `build/tests/<same name>/`; the committed copies predate later cases (no Test 11/15, A11-A15 or C1B) |

There are no per-category test directories: the property, fuzz, stress, adversarial and chaos suites
(P1-P10, F1-F8, S1-S10, A1-A15, C1-C7 + C1B, and their brutal variants) are `TestSpec` tables in
`osdev/src/validator.rs`.

## Running the identity suite

```bash
osdev test identity
```

Builds the kernel + test service images, boots each test in QEMU with `-smp 4`, reads serial output, and reports PASS/FAIL. Each test has its own `timeout_secs` (30 to 120 s for the identity cases, calibrated for KVM); under TCG (no `/dev/kvm`, e.g. Windows) every budget is multiplied by 4, so the longest identity case gets 480 s.

## Test sequencing (§22.4)

1. Write test specifications (§22 is the spec).
2. Build minimum kernel + harness.
3. See tests fail for the right reasons (missing features, not harness bugs).
4. Implement until they pass.

A test failing due to a compile error or harness bug is a test failure, not a kernel failure.

## KVM vs TCG

The harness detects `/dev/kvm` at runtime. When KVM is available it passes `-enable-kvm` to QEMU; otherwise it runs under TCG and multiplies every timeout by 4 (`qemu::timeout_scale`, overridable with `GODSPEED_TIMEOUT_SCALE`). GitHub Actions `ubuntu-latest` runners have KVM, but the workflows that run these suites (`identity.yml`, `storage.yml`, ...) are `workflow_dispatch`-only today, so nothing runs them on a push. On Windows, TCG is the only option.
