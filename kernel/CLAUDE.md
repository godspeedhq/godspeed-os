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

```bash
cargo build -p kernel --target x86_64-unknown-none
```

The kernel requires a custom target spec. The binary is a flat ELF loaded by Limine.

## Module map

| Module           | Spec section | Unsafe permitted? |
|------------------|-------------|-------------------|
| `arch/x86_64`    | §11, §12    | Yes - hardware boundary |
| `memory/`        | §10         | Yes - physical addresses |
| `capability/`    | §7          | Yes - global table |
| `smp/`           | §9, §11     | Yes - APIC MMIO |
| `ipc/`           | §8          | No  |
| `task/`          | §9, §14     | grandfathered: `mod.rs` 7 (kstack pool + spawn), `scheduler.rs` 37 - see `docs/unsafe-audit.md` |
| `syscall/`       | §8.2        | 2 grandfathered lines (syscall entry - see audit) |
| `interrupt/`     | §12         | 1 grandfathered line (IDT delivery - see audit) |
| `invariants/`    | §22         | No  |
| `log.rs`         | §11.4       | No  |
| `control.rs`     | §17         | No  |

## Unsafe policy (§18)

`unsafe` is permitted **only** in `arch/`, `memory/`, `capability/`, `smp/`. Every `unsafe` block must have a `// SAFETY:` comment. The grandfathered lines in `task/`, `syscall/`, and `interrupt/` are documented in `docs/unsafe-audit.md` and frozen - they may decrease but increase only by a recorded §18.5 amendment with rationale. There are no such amendments: hardening that needs `unsafe` (e.g. the H4 W^X / kstack-guard work) puts it in a permitted layer (`arch/`) and uses safe `fn`s for boot-ordering call sites, so the grandfathered floors hold.

A PR adding an unsafe block without a SAFETY comment is rejected without review.

## Control channel (`control.rs`)

`control.rs` implements the COM2 serial control channel used by the test harness to inject `RESTART`/`KILL` commands at runtime (§17). `process_pending()` is called from Core 0's timer ISR on every tick - not only in the scheduler idle branch - so commands are processed even under full task load.

## Panic behaviour

The panic handler prints `KERNEL PANIC: {info}` to the serial console (and the log ring buffer), then calls `halt_all_cores()` - which broadcasts an NMI so **every** core halts, not just the panicking one (SEC-18). There is no recovery. A reserved crash-page that persists the panic reason across reboot was once described here but was **never implemented**, and `init` (which would have re-read it) is removed (Phase 5); the panic reason lives on the serial console only (SEC-20 doc-drift correction).
