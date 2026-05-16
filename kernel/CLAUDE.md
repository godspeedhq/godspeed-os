# kernel/

The kernel crate. Bare-metal `#![no_std]` binary targeting `x86_64-unknown-none`.

## What lives here

Everything that runs in ring 0. The kernel is the only code that:
- Directly touches hardware (via the arch layer).
- Enforces capability checks.
- Manages physical memory.
- Owns the routing table and IPC queues.
- Issues IPIs.

## What does NOT live here

Filesystem logic, network stack, drivers (beyond minimal arch boot stubs), logging infrastructure, application logic. These belong in `services/`. If you are about to add something to the kernel that isn't on the list in `src/main.rs`, read §4.4 first.

## Build

```
cargo build -p kernel --target x86_64-unknown-none
```

The kernel requires a custom target spec. The binary is a flat ELF loaded by the bootloader.

## Module map

| Module           | Spec section | Unsafe? |
|------------------|-------------|---------|
| `arch/x86_64`    | §11, §12    | Yes — hardware boundary |
| `memory/`        | §10         | Yes — physical addresses |
| `capability/`    | §7          | Yes — global table |
| `smp/`           | §9, §11     | Yes — APIC MMIO |
| `ipc/`           | §8          | No  |
| `task/`          | §9, §14     | No (grandfathered — see `docs/unsafe-audit.md`) |
| `syscall/`       | §8.2        | No (grandfathered — 2 lines, see audit) |
| `interrupt/`     | §12         | No (grandfathered — 1 line, see audit) |
| `invariants/`    | §22         | No  |
| `log.rs`         | §11.4       | No  |

## Unsafe policy (§18)

`unsafe` is permitted **only** in `arch/`, `memory/`, `capability/`, `smp/`. Every `unsafe` block must have a `// SAFETY:` comment. A PR adding an unsafe block without a SAFETY comment is rejected without review.

## Panic behaviour

The panic handler writes to serial console and the crash page, then calls `halt_all_cores()`. There is no recovery. The system reboots on the next power cycle and init reads the stored panic reason.
