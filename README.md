# GodspeedOS

A deliberately small, capability-based microkernel operating system written in Rust — built to be fully understood by one engineer.

---

## What it is

GodspeedOS is a from-scratch OS kernel. It runs on bare-metal x86-64 hardware and supports multiple CPU cores out of the box. Every part of it — from how processes communicate to how access to hardware is controlled — is designed to be explainable in a 30-minute whiteboard session.

The goal is not to build the next Linux. It is to build something small enough to completely understand, and rigorous enough to trust.

---

## Core philosophy

**Explicit authority over ambient trust.**
Every privileged action in the system requires a capability — an unforgeable token that grants specific rights to a specific resource. There is no concept of "run as root." If you don't hold the token, you can't perform the action.

**Isolated services over a monolithic kernel.**
Application logic lives in user-space services with isolated memory. Services communicate by passing messages — they never share memory directly. The kernel's only job is to enforce those boundaries, route messages, and manage hardware. Policy belongs to services; mechanism belongs to the kernel.

**Loud failures over silent fallbacks.**
When something goes wrong, the system says so immediately. There are no hidden retries, no silent error-swallowing, no graceful degradation that masks a bug. A failure at the kernel boundary is always visible.

**Identity over location.**
A service has a stable name. Where it runs — which CPU core it is assigned to — is an implementation detail. If a service crashes and restarts on a different core, clients notice the death, look up the new address, and resume. The name stays constant; the location does not.

**Restartability as a first-class property.**
Non-critical services are designed to be killed and restarted at any time without corrupting the system. The supervisor — a trusted user-space service — holds restart authority and handles recovery. The kernel does not need to know what "recovery" means.

---

## What it is not

- A POSIX-compatible OS
- A production-grade kernel
- A research novelty
- A competitor to Linux, macOS, or Windows

It is a learning-by-building project that takes the design discipline of capability-based systems seriously.

---

## How it works (briefly)

The kernel provides exactly six things: memory isolation, scheduling, inter-process communication (IPC), capability enforcement, interrupt routing, and multi-core support. Nothing else.

Services run in separate address spaces and talk to each other by sending messages through the kernel. Before the kernel delivers a message, it checks whether the sender holds a valid capability to the destination. If not, the call is rejected — no exceptions.

When a service is restarted, all outstanding capabilities pointing to it become stale. Callers find out on their next message attempt and can look up the new address through a registry service. The system continues running.

---

## Repository layout

```
kernel/      — the kernel itself (ring 0, bare-metal)
services/    — system services: init, supervisor, registry, logger
sdk/rust/    — Rust library for writing services
osdev/       — host-side CLI: build, run in QEMU, run tests
contracts/   — JSON Schema for service declarations
docs/        — design documents and architecture notes
tests/       — identity tests, run in QEMU
```

---

## Getting started

You need a nightly Rust toolchain and QEMU. The workspace `rust-toolchain.toml` pins the exact toolchain version automatically.

```sh
# Build everything
cargo build --target x86_64-unknown-none -p kernel

# Boot in QEMU with 4 cores
cargo run -p osdev -- run --smp 4

# Run the identity test suite
cargo run -p osdev -- test identity
```

---

## CI

| Workflow | Trigger | What it checks |
|---|---|---|
| `build` | Every push / PR | Compile, clippy, contract validation, unit tests |
| `identity` | Push to `main` | All 10 constitutional invariants in QEMU |
| `coverage` | Push to `main` | Code coverage for pure-logic kernel modules |

---

## Design reference

The full architecture — capability model, IPC semantics, scheduler, memory model, bootstrap sequence, service lifecycle, and test suite — is documented in [`CLAUDE.md`](CLAUDE.md).
