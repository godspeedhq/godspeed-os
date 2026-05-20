# osdev/

Host-side developer CLI (§17). Builds for the developer's machine, not the kernel target.

## Commands

| Command                     | What it does |
|-----------------------------|-------------|
| `osdev new <name>`          | Scaffold a new service (dir, Cargo.toml, src/main.rs, contract) |
| `osdev build`               | Build kernel + all services for the bare-metal target |
| `osdev run [--smp N]`       | Boot in QEMU with N cores (default 4) |
| `osdev publish [service]`   | Package a service update |
| `osdev restart <service>`   | Restart a service in the running OS |
| `osdev logs <service>`      | Tail service log output |
| `osdev status <service>`    | Show service state + assigned core |
| `osdev caps <service>`      | Show held capabilities |
| `osdev test identity`       | Run §22 identity test suite (20 tests) |
| `osdev test property`       | Run property tests (P1–P10) |
| `osdev test fuzz`           | Run fuzz tests (F1–F8) |
| `osdev test stress`         | Run stress tests (S1–S10) |
| `osdev test perf`           | Run performance benchmarks (B1–B10) ✅ 10/10 |
| `osdev test perf:<ID>`      | Run a single benchmark (e.g. `perf:B2`) |
| `osdev test adv`            | Run adversarial / red-team tests (A1–A10) |
| `osdev test chaos`          | Run chaos / partial-failure tests (C1–C7) |
| `osdev validate`            | Validate all contracts against the JSON schema |

## Files

| File             | Responsibility |
|------------------|---------------|
| `src/main.rs`    | CLI parsing (`clap`), dispatch to handlers |
| `src/validator.rs`| Contract validation + all test suite runners (identity, property, fuzz, stress, perf, adversarial, chaos, and their brutal variants) |
| `src/qemu.rs`    | QEMU launch helpers (`spawn_for_test`, `spawn_for_test_custom`) — file-based serial (`-serial file:`) on all platforms |

## Build

```
cargo build -p osdev
```

No `--target` flag — this is a host binary.

## QEMU path

osdev expects `qemu-system-x86_64` to be on PATH (or at the configured path). On Windows: `C:\Program Files\qemu\qemu-system-x86_64.exe`. Serial output is captured from stdio and parsed for test assertions and log streaming.

## Iteration loop (§17)

```
edit → osdev build → osdev publish → osdev restart <service> → osdev logs <service>
```

Only the changed service restarts; the kernel and other services keep running.
