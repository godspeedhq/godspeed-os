# tests/qemu/harness/

Shared test infrastructure (§22.3). Used by all tests in `tests/qemu/identity/` and `tests/qemu/perf/`.

## Responsibilities

| Component          | What it does |
|--------------------|-------------|
| QEMU launcher      | Spawns `qemu-system-x86_64` with configurable `-smp N`, serial to stdout, timeout enforced |
| Serial reader      | Streams stdout line-by-line; parses structured `TEST:` lines |
| Assertion library  | `assert_serial_contains(text, within_secs)`, `assert_no_panic()`, `assert_halted()` |
| Timeout enforcer   | 30 s hard limit per test boot; kills QEMU and marks test FAIL |

## `TEST:` log format

Test services emit structured lines on the serial console:
```
TEST:PASS bootstrap_steady_state_positive
TEST:FAIL cap_enforcement_negative expected=CapNotHeld got=Ok
TEST:INFO bootstrap: 4 cores ready
```

The harness parses the first word after `TEST:` to determine test outcome.

## QEMU binary path

The harness looks for `qemu-system-x86_64` on PATH. On this machine QEMU is at `C:\Program Files\qemu\qemu-system-x86_64.exe`. Ensure this is on PATH or configure the path in `harness/config.toml`.

## Failure modes

A test FAILS if:
- `TEST:FAIL` appears on serial.
- `KERNEL PANIC` appears when not expected.
- The 30 s timeout fires without `TEST:PASS`.
- QEMU exits with a non-zero code before the timeout.
