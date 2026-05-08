# tests/qemu/

All QEMU-based tests. Tests in this tree boot the OS in QEMU; they are integration tests, not unit tests.

## Subdirectories

| Directory   | Purpose |
|-------------|---------|
| `identity/` | Constitutional identity tests (§22) — must all pass for v1 |
| `harness/`  | Shared QEMU launcher, serial reader, assertion library |
| `perf/`     | Performance benchmarks (deferred) |

## Running

```
osdev test identity
```

This builds the kernel + test service images, boots each test in QEMU with `-smp 4`, and reports PASS/FAIL.

## Test sequencing (§22.4)

Tests cannot run until the kernel boots and IPC works. The correct sequence is:
1. Write test specifications (§22 is the spec).
2. Build minimum kernel + harness.
3. See tests fail for the right reasons (missing features, not harness bugs).
4. Implement until they pass.

A test failing due to a compile error or harness bug is a test failure, not a kernel failure.
