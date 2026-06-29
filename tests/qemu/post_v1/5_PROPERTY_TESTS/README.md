# Property Tests Evidence

Property tests run as part of `cargo test -p kernel --lib` - no separate command.

## Modules covered

| Module | Properties |
|--------|-----------|
| `capability/generation.rs` | 3 (P2 - monotonicity) |
| `capability/rights.rs` | 5 (P3 - non-escalation) |
| `capability/cap.rs` | 3 (P1, P3, P9) |
| `ipc/message.rs` | 3 (§8.5 size enforcement) |
| `ipc/queue.rs` | 4 (P6 - ring-buffer invariants) |

## Running locally

```sh
cargo test -p kernel --lib
```

To run more cases (default 256):

```sh
PROPTEST_CASES=10000 cargo test -p kernel --lib
```

## Failure output

When a property fails, proptest prints:
```
thread '...' panicked at 'Test failed: ...
Minimal failing input: <shrunk values>
```
The minimal input is automatically shrunk for easy reproduction.
