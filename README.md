# GodspeedOS

> Small enough to understand. Rigorous enough to trust.

A capability-based microkernel OS written in Rust. Every privileged action requires an explicit capability. Services are isolated. Failures are visible. Authority is never inherited or ambient.

---

## Architecture

```
  ┌──────────────────────────────────────────────────┐
  │  Application Services  (replaceable)             │
  ├──────────────────────────────────────────────────┤
  │  System Services                                 │
  │  logger  ·  block-driver  ·  fs                  │
  ├──────────────────────────────────────────────────┤
  │  Trusted Root  (restartable; kernel respawns it) │
  │  supervisor                                      │
  ├──────────────────────────────────────────────────┤
  │  Kernel  (mechanism, not policy)                 │
  │  memory · scheduler · ipc · capability           │
  │  syscall · interrupts · smp/routing              │
  ├──────────────────────────────────────────────────┤
  │  Architecture Layer  (unsafe boundary)           │
  │  arch/x86_64                                     │
  ├──────────────────────────────────────────────────┤
  │  Hardware  (multi-core)                          │
  └──────────────────────────────────────────────────┘
```

The kernel is strictly bounded: memory isolation, scheduling, IPC routing, capability enforcement, interrupt routing, and multi-core coordination. Nothing else. Policy belongs to services.

---

## How it works

**Capabilities** — every privileged action requires an explicit, unforgeable token. A capability carries a resource ID, a rights set, and a generation number. Stale capabilities return `EndpointDead`. Forged ones return `CapNotHeld`. There is no ambient authority.

**IPC** — synchronous message passing with bounded queues (16 messages per endpoint). Services are pinned to CPU cores. Cross-core sends route through the kernel's routing table and wake the receiver via IPI. Zero-copy is permanently rejected — isolation is more important.

**Supervisor:** the service with restart authority. When a service is killed, its endpoint generation is bumped. All outstanding capabilities immediately become stale. Clients detect `EndpointDead`, look up the new instance by name via the kernel's name directory, and resume. The new instance may be on a different core, which is invisible to callers. The supervisor is itself restartable: if it dies, the **kernel respawns it** and it reconciles with the still-running services. The only unkillable component is the kernel.

**Scheduler** — per-core run queues, round-robin, 10 ms preemption quantum enforced by the local timer. Services are placed at spawn and never migrate. Yield is advisory; preemption is not.

---

## Design principles

| Principle | What it means |
|-----------|---------------|
| No ambient authority | Every privileged action requires a capability |
| Explicit authority | Authority comes from holding a cap, not from identity or ancestry |
| Bounded behavior | Queues, tables, memory, and messages all have fixed limits |
| Loud failures | `EndpointDead`, `CapRevoked`, `AllocDenied` — never silent fallback |
| Identity over location | Service names are stable; core assignments are not |
| One irreducible truth | Store the minimal source; derive (and reconcile) every cache, index, or count |
| Restartability | Every service survives kill + respawn, even the supervisor; only the kernel is unkillable |

These distil into the **[Ten Commandments of Godspeed](COMMANDMENTS.md)**, the human-readable form of the constitution.

---

## Test suite

GodspeedOS treats testing as architecture. The suite is layered — each layer must pass before the next is meaningful.

| Suite | Purpose | Status |
|-------|---------|--------|
| Identity (15 tests, 24 cases) | Pin constitutional invariants | 24/24 ✅ |
| Property (P1–P10) | Universal correctness under random inputs | Active |
| Fuzz (F1–F8) | Kernel never panics on user-controllable input | Active |
| Stress (S1–S10) | No drift, leaks, or corruption over time | Active |
| Performance (B1–B10) | Latency / throughput baselines | Active |
| Adversarial (A1–A10) | Capability isolation under direct attack | Active |
| Chaos (C1–C7) | Graceful degradation under partial failures | Active |

### Static analysis & unsafe audit

Every `unsafe` block is inventoried in `docs/unsafe-audit.md` and enforced by
`scripts/unsafe_check.py` — counts may not grow without a written SAFETY argument.
Latest pass (2026-05-31, boot-verified on AMD T630; `milestones/v2/STATIC_ANALYSIS_AUDIT.md`):

| Check | Result |
|-------|--------|
| Unsafe confined to permitted layers (§18.1) | ✅ `ipc/` violation fixed; audit passes (302 lines / 23 files) |
| Safety / correctness lints (static-mut refs, fn-casts, redundant `unsafe`) | ✅ 0 |
| Kernel build warnings | 104 → 57 (remaining are intentional unwired architecture) |
| Hardware boot regression | ✅ clean — 4 cores, cross-core ping/pong to 83k+ msgs, zero faults |

---

## Getting started

**Requirements:** Rust nightly (pinned in `rust-toolchain.toml`), QEMU, x86_64 host.

```bash
# Build
cargo run -p osdev -- build

# Boot in QEMU with 4 cores
cargo run -p osdev -- run --smp 4

# Run identity test suite
cargo run -p osdev -- test identity

# Run property tests
cargo run -p osdev -- test property
```

The full `osdev` CLI reference is in `CLAUDE.md §17`.

---

## Repository layout

```
kernel/       bare-metal microkernel
services/     system services (supervisor, logger, block-driver, fs, shell, ...)
sdk/rust/     Rust SDK for service development
osdev/        build / test / run tooling
contracts/    service contracts and JSON schema
examples/     demonstration services (ping, pong, greet, upper, roster)
tests/        identity, property, fuzz, stress, chaos suites
docs/         architecture notes and design docs
```

---

## Design reference

The full specification (capability model, IPC semantics, scheduler rules, memory enforcement, bootstrap sequence, unsafe policy, and constitutional invariants) is in `CLAUDE.md`. Its human-readable distillation is **[`COMMANDMENTS.md`](COMMANDMENTS.md)**: ten laws that bound every design choice.

The system is defined there first. The implementation exists to satisfy it.
