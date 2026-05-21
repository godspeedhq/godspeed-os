# Subsystem Property Tests Evidence

Subsystem property tests run as part of `cargo test -p kernel --lib` — no separate command.

## Modules covered

| Module | Properties | Pins |
|--------|-----------|------|
| `capability/table.rs` — `GlobalResourceTable` | 5 | §7.5, §22 P2, P8, P9 |
| `capability/table.rs` — `CapTable` | 4 | §7.8 |
| `memory/bitmap.rs` — `TestBitmapAllocator` | 5 | §10, §22 item 6.1 |

## Running locally

```sh
cargo test -p kernel --lib
```

To run more cases (default 256):

```sh
PROPTEST_CASES=10000 cargo test -p kernel --lib
```

## Test count

50 tests from items 1–5, plus 14 new property tests from item 6 = **64 tests total**.

## Notes

- `GlobalResourceTable` instances in tests are `Box<>`-allocated (~73 KiB struct).
- All tests use LOCAL instances — `GLOBAL_RESOURCES` static is never touched.
- `memory/bitmap.rs` is compiled only in test mode via `#[cfg(test)] mod memory` in `lib.rs`.
