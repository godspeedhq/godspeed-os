# tests/

All tests for the OS. v1 contains only QEMU-based integration tests.

## Structure

```
tests/
  qemu/
    identity/   # Constitutional identity tests (§22) — 10 tests, all required for v1
    harness/    # QEMU launcher, serial parser, assertion library
    perf/       # Performance benchmarks (deferred)
```

## Philosophy (§22.2)

| Category   | Purpose                                  | Status     |
|------------|------------------------------------------|------------|
| Identity   | Pin constitutional decisions (§22)       | v1 required |
| Property   | Invariants under random inputs           | Deferred   |
| Fuzz       | Crash resistance on malformed inputs     | Deferred   |
| Performance| Benchmarks for IPC and syscall paths     | Deferred   |

Identity tests are the minimum set that, if any one fails, means the system is no longer the system `CLAUDE.md` describes. They are prioritised above all other test categories.
