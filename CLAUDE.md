# CLAUDE.md
## Capability-Based Microkernel OS

**Status:** v3.7 — SMP-Integrated Spec, Reviewed, with Forward-Looking Appendices
**Scope:** v1 milestone (multi-core kernel, two services, cross-core IPC, supervisor restart with core reassignment)
**Audience:** Contributors, reviewers, future maintainers

---

## Table of Contents

1. [Purpose of This Document](#1-purpose-of-this-document)
2. [Project Identity](#2-project-identity)
3. [Constitution: Non-Negotiable Invariants](#3-constitution-non-negotiable-invariants)
4. [Architecture Overview](#4-architecture-overview)
5. [Repository Structure](#5-repository-structure)
6. [Trusted Computing Base](#6-trusted-computing-base)
7. [Capability System](#7-capability-system)
8. [IPC](#8-ipc)
9. [Scheduler and SMP](#9-scheduler-and-smp)
10. [Memory Model](#10-memory-model)
11. [Bootstrap Sequence](#11-bootstrap-sequence)
12. [Drivers and Interrupts](#12-drivers-and-interrupts)
13. [Service Contracts](#13-service-contracts)
14. [Service Lifecycle](#14-service-lifecycle)
15. [State and Persistence](#15-state-and-persistence)
16. [Update Model](#16-update-model)
17. [Developer Workflow](#17-developer-workflow)
18. [Unsafe Policy](#18-unsafe-policy)
19. [Debugging Model](#19-debugging-model)
20. [Performance Philosophy](#20-performance-philosophy)
21. [Contribution Rules](#21-contribution-rules)
22. [Identity Test Suite](#22-identity-test-suite)
23. [First Milestone](#23-first-milestone)
24. [Glossary](#24-glossary)
25. [Final Principles](#25-final-principles)
26. [Architectural Discipline](#26-architectural-discipline)

**Appendices (forward-looking, mostly non-normative):**

- [Appendix A: Bootloader Choice (Limine)](#appendix-a-bootloader-choice-limine)
- [Appendix B: Userspace Posture](#appendix-b-userspace-posture)
- [Appendix C: Forward-Looking Vision (Non-Normative)](#appendix-c-forward-looking-vision-non-normative)
- [Appendix D: Shell, Scripting, and Utility Ecosystem (Non-Normative, Far Future)](#appendix-d-shell-scripting-and-utility-ecosystem-non-normative-far-future)

---

## 1. Purpose of This Document

This document is the constitution of the project. It defines the enforceable laws and concrete decisions of the OS.

It is **not aspirational**. It is a commitment to specific trade-offs.

It exists to:

- Ensure the system remains small, correct, and enforceable.
- Prevent contributors from introducing hidden complexity.
- Make every architectural decision traceable to a stated rationale.
- Resolve future arguments by appeal to written law rather than memory.

When this document and the code disagree, the document wins and the code is wrong. When this document and reality disagree, the document is amended and the change is recorded.

The appendices at the end of this document collect forward-looking design notes. Appendix A documents a v1 commitment (the bootloader choice). Appendices B, C, and D are explicitly non-normative — they record design intent and discussion, not commitments. Their content does not amend the constitution.

---

## 2. Project Identity

### 2.1 What This Project Is

A deliberately small, fully-understood capability microkernel, built to internalize the design discipline of capability-based systems by writing one.

### 2.2 What This Project Is Not

- A novel research system.
- A POSIX-compatible OS.
- A production-grade kernel.
- A general-purpose application platform.
- A competitor to seL4, Fuchsia, Hubris, Redox, or Genode.

### 2.3 Goals, Ranked

1. **Clarity over novelty.** Every part of the system should be explainable in a paragraph.
2. **Correctness over cleverness.** A boring, verifiable design beats a clever, fragile one.
3. **Execution over theory.** Shipping a working ping/pong beats refining a perfect spec.

### 2.4 v1 Architectural Principles

- Isolation over performance.
- Explicit transitions over implicit mutation.
- Identity over location.
- Simplicity over cleverness.
- **Loud failures over silent fallbacks.**

### 2.5 v1 Permanent Decisions

| Capability         | Status                  | Rationale                                       |
|--------------------|-------------------------|-------------------------------------------------|
| SMP                | **Supported (static)**  | Cross-core parallelism; no migration            |
| Zero-copy IPC      | **Rejected, permanent** | Violates "no shared mutable memory"             |
| Live code updates  | **Rejected, permanent** | Restart-with-reacquisition is sufficient        |

### 2.6 Reference Systems

| System  | What we borrow                                          |
|---------|---------------------------------------------------------|
| seL4    | Capability model, synchronous IPC discipline             |
| Hubris  | Declarative task contracts, supervisor-driven restart    |
| Fuchsia | Userspace drivers, capability handles in IPC messages    |
| Genode  | Composition through capability delegation                |
| Redox   | Rust microkernel patterns                                |

---

## 3. Constitution: Non-Negotiable Invariants

These are the laws that bound every design choice. Any change that violates an invariant must first amend the invariant — and amendments require a written rationale.

1. **No ambient authority.** Every privileged action requires an explicit capability.
   > **Amendment 2026-06-12 (H1):** DMA-capable devices were an unstated exception
   > to this invariant — a driver could direct its controller's DMA engine at any
   > physical address, kernel-equivalent reach the capability model never granted.
   > H1 (IOMMU confinement) closes the gap: a confined device reaches only its
   > granted arena. See §6.4.
2. **No shared mutable memory by default.** Services have isolated address spaces.
3. **All authority is explicit.** Capabilities, not identity.
4. **Kernel remains tiny.** Memory, scheduling, IPC, capabilities, interrupts, cross-core routing. Nothing else.
5. **Unsafe is isolated and audited.** Permitted only in `arch/`, `memory/`, `capability/`, `smp/`.
6. **Services must be restartable** (with stated TCB exceptions).
7. **Contracts are enforced, not interpreted.**
8. **State must be explicit and owned.** No anonymous singletons.
9. **No unowned global mutable state.** Immutable globals are fine.
10. **System must remain understandable.** 30-minute whiteboard rule.
11. **Identity is stable; location is not.** Services have stable names; their core assignment may change across restarts.
12. **Failures are loud, never silent.** No silent fallbacks at the kernel boundary.

---

## 4. Architecture Overview

### 4.1 Layered View

```text
  ┌──────────────────────────────────────────────────┐
  │  Application Services  (replaceable)             │
  ├──────────────────────────────────────────────────┤
  │  System Services                                 │
  │  logger  ·  block-driver  ·  fs                  │
  ├──────────────────────────────────────────────────┤
  │  Trusted Root  (non-restartable)                 │
  │  init  ·  supervisor  ·  registry                │
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

### 4.2 SMP View (Per-Core)

```text
  Kernel (shared, concurrent)
  ┌────────────────────────────────────────────────┐
  │  Routing Table: EndpointId → CoreId            │
  │  Capability Table                              │
  └─────────────┬──────────────┬───────────────┬───┘
            IPI │          IPI │           IPI │
  ┌─────────────▼──┐  ┌────────▼───┐  ┌────────▼───┐
  │    Core 0      │  │   Core 1   │  │   Core 2   │
  │   Run Queue    │  │  Run Queue │  │  Run Queue │
  │   Services     │  │  Services  │  │  Services  │
  └────────────────┘  └────────────┘  └────────────┘
    ▲ syscall             ▲ syscall       ▲ syscall
```

### 4.3 Kernel Scope (Strict)

- Memory isolation (per-service address spaces, page tables)
- Scheduling (per-core run queues, round-robin with timer preemption)
- IPC (synchronous message passing, bounded queues, cross-core routing)
- Capability enforcement (validation on every privileged syscall, generation check)
- Interrupt routing (delivery to userspace driver services)
- SMP routing (EndpointId → CoreId map, IPI wakeup)

### 4.4 Kernel Anti-Scope

The kernel does **not** contain filesystem logic, network stack, drivers (beyond minimal arch boot), logging infrastructure, application logic, developer tooling, work-stealing scheduler, service migration, or load balancing.

> **Amendment 2026-06-18 (P2, file-as-capability): delegated resource capabilities do not add
> file logic to the kernel.** The kernel gains the ability to mint, route, and revoke capabilities
> for resources whose *meaning is defined by a service* (§7.10). It still contains **no filesystem
> logic**: it tracks an opaque `ResourceId` and its owning endpoint and nothing more; the owning
> service (`fs`) alone maps `ResourceId → file`. "A file is a capability" thus becomes literally
> true while this anti-scope holds — the kernel never learns what a file *is*.

---

## 5. Repository Structure

```
os/
  CLAUDE.md
  README.md
  Cargo.toml

  kernel/
    src/
      main.rs
      arch/x86_64/         # boot.rs, interrupts.rs, context_switch.rs, page_tables.rs, ap_boot.rs
      memory/              # frame.rs, page.rs, allocator.rs, ownership.rs
      task/                # task.rs, state.rs, scheduler.rs (per-core)
      ipc/                 # message.rs, endpoint.rs, queue.rs, routing.rs
      capability/          # cap.rs, table.rs, rights.rs, generation.rs, revoke.rs
      smp/                 # core.rs, ipi.rs, placement.rs
      syscall/dispatch.rs
      interrupt/route.rs
      invariants/assertions.rs
      log.rs               # kernel ring buffer

  services/
    init/                  # PID 1 equivalent (TCB)
    supervisor/            # restart authority (TCB)
    registry/              # name → endpoint resolution (restartable, H11; docs/registry.md)
    logger/
    block-driver/          # v1: trusted
    fs/                    # v1: trusted, depends on block-driver

  sdk/rust/                # service_context.rs, capability.rs, ipc.rs

  osdev/
    src/main.rs
    src/validator.rs       # JSON Schema validation

  examples/
    ping/
    pong/

  contracts/
    schema/service.schema.json

  docs/
    bootstrap.md
    ipc.md
    capability.md
    restart.md
    smp.md
    bootloader.md          # Limine integration notes
    unsafe-audit.md

  tests/
    qemu/
      identity/            # identity test suite (§22)
      harness/             # shared test infrastructure
      perf/                # performance benchmarks (§22 B1–B10) — 10/10 ✅
```

---

## 6. Trusted Computing Base

### 6.1 TCB Members

| Component         | Trusted because                                     |
|-------------------|-----------------------------------------------------|
| Kernel            | Enforces all isolation                              |
| `arch/x86_64`     | Direct hardware access                              |
| `kernel/smp`      | Concurrent-correctness primitives                   |
| `init` service    | Spawns supervisor; first userspace authority        |
| `supervisor`      | Holds restart authority over all other services     |
| `xhci`, `ehci` (DMA drivers) | **Machine-dependent (H1, §6.4):** in the TCB only on a machine with no IOMMU to confine them (DMA-anywhere = kernel-equivalent reach); **dropped** from it — least-privilege and restartable — wherever an IOMMU confines them to their arena. The case is reported loudly at boot (invariant 12). |

> **Amendment 2026-06-12 (H1): DMA-capable drivers are no longer an unconditional TCB
> member.** Before H1 they were an *implicit, unstated* member: with no IOMMU a driver
> directs its controller's DMA engine at any physical address — kernel-equivalent power
> the capability model never granted (see the invariant 1 amendment). H1 makes their
> trust **machine-dependent**: confined by an IOMMU, a compromise is bounded to the
> granted arena, so the driver is genuinely least-privilege and restartable and **leaves
> the TCB**; on a machine with no IOMMU it stays trust-critical by necessity. The same
> binary, different posture, with the difference printed at boot. §6.4 is the full
> treatment; §22 Test 12 pins the confined case.

> **Amendment 2026-06-17 (Phase D / FS robustness): `block-driver` and `fs` are no longer
> TCB members.** They were a v1 simplification (§6.3): `fs` owned persistent state it could
> not recover after a crash, so its death — and that of the `block-driver` it depends on —
> was a panic+reboot. The filesystem robustness program closes that gap: `fs` now commits
> every metadata mutation through a **crash-consistent redo-journal** and **recovers to a
> consistent state on mount** (Phase C; `docs/persistence.md` §6.8). With recovery in hand,
> both services are **restartable** like any other: the kernel notifies the supervisor of
> their death, which respawns them; `fs` re-mounts (recovering via the journal) and
> re-registers, `block-driver` re-initialises the controller, and clients reacquire via the
> registry and retry (§14.3, §6.2). `block-driver` holds no persistent state and was already
> operationally restartable. Their *boot-time* spawn must still succeed to bootstrap
> persistence (§11.3); only their *runtime* death is now recovered, not fatal. Worked example
> of invariant 11 and the §6.3 goal reached. §22 Test 13 pins it (`fs` survives its own
> restart). The remaining non-restartable set is now just `init` + `supervisor` + kernel.

> **Amendment 2026-06-09 (H11): `registry` is no longer a TCB member.** It became a
> real userspace name service (register/lookup over IPC, holding only delegated caps
> and deriving copies — `docs/registry.md`). It owns no kernel-critical state, so its
> *runtime* death degrades name resolution temporarily rather than corrupting the
> system, and it is now **restartable** (see §6.2, §6.3). Its *boot-time* spawn must
> still succeed (the name service must come up to bootstrap), which remains fatal
> (§11.3). Worked example of invariant 11 — identity (the name) is stable; the
> registry instance/location is not.

### 6.2 Failure Semantics

> **Failure of any TCB service (`init`, `supervisor`) results in kernel panic and immediate system reboot. No automatic recovery is attempted.**

> **`registry`, `block-driver`, and `fs` are restartable, not in this set.** Their runtime
> death is recovered by the supervisor's death-notification restart loop (the kernel notifies
> the supervisor, which respawns them); clients see a temporary `EndpointDead` / lookup miss,
> reacquire via the registry, and retry (§14.3). `fs` re-mounts to a consistent state via its
> crash-consistency journal (Phase C; `docs/persistence.md` §6.8). `registry` left the TCB via
> H11; `block-driver` + `fs` via the Phase D amendment (§6.1). Only their *boot-time* spawn
> failure is fatal (§11.3).

> **Failure on any core that corrupts shared kernel state (capability table, routing table) results in kernel panic on all cores.**

Silent recovery of TCB state risks undefined system state. Loud failure plus clean restart is the only safe v1 option.

### 6.3 Reducing TCB Over Time — **goal reached**

The v2 goal was: only `init`, `supervisor`, and the kernel remain non-restartable. That goal
is now **met**. `registry` was dropped early via H11 (a restartable userspace name service);
`block-driver` and `fs` were dropped via the **Phase D amendment (§6.1, 2026-06-17)** once the
filesystem gained crash-consistent recovery (the redo-journal, `docs/persistence.md` §6.8) —
so an `fs` restart re-mounts to a consistent state rather than corrupting it. The
non-restartable set is now just **`init` + `supervisor` + kernel** (plus DMA drivers only on a
machine without an IOMMU, §6.4). Further shrinking would require making `init`/`supervisor`
themselves recoverable — out of scope; they are the recovery authority.

### 6.4 DMA-Capable Drivers and the IOMMU (H1)

> **Amendment 2026-06-12 (H1).** Adopts IOMMU DMA-confinement as the mechanism that
> brings DMA-capable userspace drivers into the least-privilege model (invariant 1).

DMA-capable userspace drivers (`xhci`, `ehci`) are **not** trusted root. Their trust
status is **machine-dependent**, and which case holds is reported loudly at boot
(invariant 12):

- **Without an IOMMU**, a DMA-capable driver holds implicit kernel-equivalent power:
  it programs its controller's DMA engine with physical addresses and can therefore
  read or write *anywhere* in RAM, regardless of the capabilities it holds. Its
  compromise is unbounded, so it is **trust-critical by necessity**. (Boot reports
  `iommu: no IVRS table ... drivers stay in TCB`.)
- **With an IOMMU confining it (H1)**, the device can reach only the driver's granted
  DMA arena; a DMA outside it faults rather than corrupting memory. A compromise is
  then bounded to the arena the capability model already granted, so the driver is
  **genuinely least-privilege and restartable** — exactly as ordinary services are.
  (Boot reports `iommu: ... confined BDF ...`; out-of-arena DMA is pinned by §22
  Test 12.)

This machine-dependent posture is deliberate: the same driver binary is least-privilege
on a machine whose IOMMU confines it and trust-critical on one without, and the
difference is a printed boot fact rather than a hidden assumption. Confinement is applied
**per driver** and only where it fits the driver's DMA shape — `xhci` is confined to its
arena; `ehci` (which legitimately reaches firmware/hub regions and a low-speed device only
through a hub's transaction translator) runs in transparent IOMMU passthrough. See
`docs/iommu.md`.

Operationally these drivers were already restartable (their death reclaims their IOMMU
resources and the supervisor respawns them); this amendment makes the *trust* claim
official, not the runtime behaviour.

---

## 7. Capability System

### 7.1 Concept

A capability is an unforgeable token granting specific rights to a specific resource at a specific generation. Holding a cap is necessary and sufficient authority for the actions it permits — provided its generation still matches the kernel's record.

### 7.2 Capability Structure

```
Capability = ResourceId + Rights + Generation
```

- **ResourceId** — identifies the target resource (endpoint, memory region, MMIO range).
- **Rights** — bitfield of permitted actions (READ, WRITE, SEND, RECV, GRANT, REVOKE).
- **Generation** — monotonic counter assigned by the kernel when the resource is created or replaced.

### 7.3 Properties

- **Unforgeable.** Only the kernel constructs valid capabilities.
- **Explicit.** Authority comes from holding a cap, not from identity.
- **Non-escalating.** Rights cannot be widened.
- **Scoped.** Targets a specific resource, not a class.
- **Revocable.** Kernel can invalidate at any time.
- **Transferable** (with `GRANT` right).
- **Generationed.** Stale caps fail their generation check on use.

### 7.4 Rights Model

| Right      | Meaning                                              |
|------------|------------------------------------------------------|
| `READ`     | Read from a resource                                 |
| `WRITE`    | Write to a resource                                  |
| `SEND`     | Send messages to an IPC endpoint                     |
| `RECV`     | Receive messages from an IPC endpoint                |
| `GRANT`    | Transfer this capability via IPC                     |
| `REVOKE`   | Revoke this capability (held by supervisor only)     |

### 7.5 Generations

Every capability carries a generation number. Every resource in the kernel tracks its current generation.

**Bumping rules:**

- **Restartable / destroyable resources** bump their generation when they are destroyed or replaced. This invalidates all outstanding caps targeting them.
- **Stable resources** (kernel-owned, never reclaimed) keep generation 0 forever.

**Validation on use:**

```
syscall(cap, action, args):
    if cap.resource_id not in kernel_table:
        return CapNotHeld
    if cap.generation != kernel_table[cap.resource_id].generation:
        return CapRevoked or EndpointDead   # depending on cause
    if action not in cap.rights:
        return CapInsufficientRights
    perform action
```

```text
  syscall(cap, action, args)
    │
    ├─ cap.resource_id in kernel table?
    │       No  ──▶  CapNotHeld
    │       Yes ↓
    ├─ cap.generation == table generation?
    │       No (destroyed)  ──▶  EndpointDead
    │       No (revoked)    ──▶  CapRevoked
    │       Yes ↓
    └─ action in cap.rights?
            No  ──▶  CapInsufficientRights
            Yes ──▶  perform action  ──▶  Ok
```

The generation check is one atomic comparison. It is the v1 mechanism for cross-core revocation: bumping the generation on one core makes every cap on every other core stale, with no synchronous notification required.

### 7.6 Lifecycle

```text
  [kernel mints cap with current generation]
           │
        Created
           │  inserted into cap table
         Held ◀────── action complete ──────┐
           │  syscall + rights + gen check   │
          Used ────────────────────────────▶─┘
           │
           ├── send with GRANT ──▶  Transferred ──▶  Held (receiver's table)
           │
           └── kernel bumps resource gen ──▶  Stale
                                                 │
                                    next use: CapRevoked / EndpointDead
```

### 7.7 Error Codes

Both error codes are kept; they share a mechanism but communicate different intent:

| Error                    | Meaning                                              |
|--------------------------|------------------------------------------------------|
| `CapNotHeld`             | Cap not in calling task's table                      |
| `CapInsufficientRights`  | Cap held but lacks required right                    |
| `CapNotGrantable`        | Send embedded a cap without GRANT                    |
| `CapWrongScope`          | Cap targets a different resource than the action     |
| `CapRevoked`             | Authority explicitly invalidated                     |
| `EndpointDead`           | Endpoint/service lifecycle terminated                |

Same generation-mismatch mechanism underlies `CapRevoked` and `EndpointDead`; the kernel returns the more specific code based on whether the resource was destroyed (endpoint dead) or had its cap explicitly revoked.

### 7.8 Capability Table Concurrency

Multiple cores may execute syscalls touching the same capability table simultaneously. v1 uses a locking discipline that guarantees:

- Reads (cap lookup, generation check) are wait-free under common cases.
- Writes (cap insertion on spawn, removal on death) are serialized.
- A revocation in flight on one core is visible to a syscall in flight on another core within bounded time (next memory barrier).

The exact primitive is an implementation choice, not a spec choice.

> **v1 implementation note:** A single global `RwLock` is acceptable for the v1 milestone. Syscall-path serialization is a known performance cost; sharded or RCU-based designs are explicit v2 work and require benchmarks before adoption.

### 7.9 Example

```rust
fn main(ctx: ServiceContext) -> Result<()> {
    let logger    = ctx.capability("log_write")?;
    let pong_send = ctx.capability("ipc_send.pong")?;

    logger.info("ping starting")?;
    pong_send.send(Message::text("hello"))?;
    Ok(())
}
```

### 7.10 Delegated Resource Capabilities (P2 — file-as-capability)

> **Amendment 2026-06-18 (P2).** Extends the capability model so a resource's *meaning* can be
> owned by a service while the kernel still mints, validates, routes, and revokes its caps. This is
> the mechanism that makes "a file is a capability" literally true (`docs/persistence.md` §7.2).

The kernel mints unforgeable caps (§7.3) but has no concept of a file (§4.4). Delegated resource
capabilities bridge the two: a service **owns a band of `ResourceId`s** whose meaning only it knows,
and the kernel treats each as an **opaque** resource — identical machinery to an endpoint cap.

- **Mint.** A service asks the kernel (`resource_mint`) to allocate a fresh `ResourceId` in its
  band, register it (generation 0, Alive), record the service's endpoint as its **owner**, and mint
  a cap with chosen `Rights` (READ/WRITE/GRANT). The service hands a (narrowed) copy to a client by
  the existing embedded-cap transfer (§8.5). `fs` does this on `Open`, recording `ResourceId → file`.
- **Use = send.** A holder uses the cap by `send`ing on it. The kernel validates it (generation +
  required right) and routes the message to the **owning service's endpoint, badged with the
  `ResourceId`** — so the owner knows which resource without the kernel knowing what it means. A
  read/write to a file is then a first-class capability operation: validated, denied, or revoked by
  the same code path as any cap.
- **Revoke = generation bump (§7.5).** The owner revokes a resource it owns (`resource_revoke`);
  every outstanding cap to it goes stale and the next use returns `CapRevoked`. `fs` revokes on
  delete/close.
- **Minting is gated (§3.1).** `resource_mint` requires a `RESOURCE_MINT` capability, granted only
  to services that legitimately issue resources (e.g. `fs`). Delegated minting is explicit
  authority, never ambient.

Every §7.3 property holds for a delegated resource cap exactly as for an endpoint cap: unforgeable,
non-escalating (rights narrow on transfer), scoped (to one `ResourceId`), revocable, generationed.
The kernel learns nothing about files; it routes opaque resources. Pinned by §22 Test 14.

---

## 8. IPC

### 8.1 Model

Synchronous message passing with bounded per-endpoint queues. Endpoints are owned by services; services are pinned to cores; sending across cores goes through the kernel routing table.

### 8.2 Syscalls

```rust
send(endpoint_cap, message)     -> Result<(), IpcError>   // blocks if full
recv(endpoint_cap)              -> Result<Message, IpcError>  // blocks until msg
try_send(endpoint_cap, message) -> Result<(), IpcError>   // non-blocking
```

### 8.3 Routing

The kernel maintains a routing table:

```
EndpointId → (CoreId, Generation, Liveness)
```

On every send, the kernel:

1. Validates the cap (rights + generation).
2. Looks up the target endpoint in the routing table.
3. Enqueues into the target endpoint's queue (which lives on the target's core).
4. If the receiver is blocked on `recv`, sends an IPI to its core to wake it.

### 8.4 Send Flow

```text
  Service A (Core 0)           Kernel                  Service B (Core 1)
       │                          │                            │
       │── send(endpoint_cap, ───▶│                            │
       │        message)          │ validate cap               │
       │                          │ (rights + generation)      │
       │◀── CapNotHeld ───────────│  cap not in table          │
       │◀── CapRevoked ───────────│  gen mismatch (revoked)    │
       │◀── EndpointDead ─────────│  gen mismatch (dead)       │
       │                          │                            │
       │   [queue full]           │                            │
       │   A blocked ─────────────│────────────────────────────│
       │   in routing table       │  (waiting for queue space) │
       │                          │                            │
       │   [queue has space]      │                            │
       │                          │── copy msg into B's queue ─│
       │◀── Ok ───────────────────│                            │
       │                          │── IPI Core 1 ─────────────▶│
       │                          │   wake B if blocked        │
```

### 8.5 Message and Queue Format

- **Maximum message size:** 4 KiB (one page).
- **Queue depth:** **16 messages per endpoint, fixed in v1.** Worst-case 64 KiB per endpoint queue.
- **Copy semantics:** kernel copies sender → receiver. Zero-copy is permanently rejected (§2.5).
- **Embedded capabilities:** caps inside a message are transferred (with `GRANT`) and removed from sender's table.

Queue depth is not configurable per endpoint in v1. Per-endpoint depth is a v2 concern.

### 8.6 Failure Semantics

| Event                                       | Effect                       |
|---------------------------------------------|------------------------------|
| Service dies                                | Endpoint generation bumped; queue drained |
| `send` to closed endpoint                   | Returns `EndpointDead`       |
| `recv` on closed endpoint                   | Returns `EndpointDead`       |
| Sender already blocked when endpoint closes | Wakes with `EndpointDead` (cross-core IPI) |
| Send-during-restart race                    | Generation check catches it; returns `EndpointDead` |

> **No delivery guarantee.** A successful `send` means the message was queued, not processed. Protocols requiring acknowledgment must build it explicitly.

### 8.7 Send-During-Restart Race

Without explicit handling, this race is real:

- Core 0: A reads cap to B (generation 4, alive).
- Core 1: B is killed, generation bumped to 5.
- Core 0: A's send syscall executes.

Resolution: the kernel checks A's cap generation against the routing table's current generation atomically inside the send syscall. The generation has been bumped to 5; A's cap is generation 4; send returns `EndpointDead`. The race is invisible to the developer.

### 8.8 Cross-Core IPC Cost

Cross-core sends pay one IPI to wake a blocked receiver, one memory barrier on enqueue, and cache-line bouncing on the queue's head/tail indices. These are real costs but they are bounded and predictable. v1 does not optimize cross-core IPC beyond getting it correct.

### 8.9 Deadlock and Mutual-Blocking Avoidance

The kernel does not detect or break deadlocks.

> **Design rule:** In any protocol where A and B both send to each other, at least one direction MUST use `try_send`. Mutual blocking sends are an anti-pattern the kernel will not detect and will not recover from. The supervisor's quantum-starvation watchdog is a last resort, not a primary mitigation.

The classic deadlock — A and B both call `send` to a full queue — is the developer's responsibility to prevent at the protocol level. Use `try_send`, structure as request/response, or apply explicit timeouts.

---

## 9. Scheduler and SMP

### 9.1 Model

- **Multi-core in v1.** Number of cores discovered at boot.
- **Per-core run queues.** Each core runs round-robin over its own queue.
- **Static placement.** Services are pinned at spawn; they never migrate.
- **10 ms preemption quantum**, per core, enforced by each core's local timer.

```text
  spawn ──▶  Ready
               │  scheduler selects
               ▼
            Running ◀──────────────────────────────────────────┐
               │                                                │
               ├── timer preemption (10 ms) / yield() ─────────┘
               │
               ├── recv() on empty queue ──▶  BlockedOnRecv
               │                                   │
               │                   message arrives │
               │                   (IPI wake) ─────┘
               │
               ├── send() on full queue ──▶  BlockedOnSend
               │                                   │
               │               queue drains (IPI) ─┤
               │               or EndpointDead ─────┘
               │
               └── page fault / supervisor kill ──▶  Dead
                                                        │
                                            frames reclaimed, generation bumped
```

### 9.2 Placement (Strict)

When the supervisor spawns a service:

```
1. If the contract specifies [placement] core = N:
   - If core N exists and is ready, spawn on N.
   - Otherwise, spawn rejected with PlacementInvalid.
     The supervisor logs the rejection and skips the service.
     The system continues with the services that did start.
2. If no placement specified:
   - Round-robin across ready cores.
3. Once placed, the service stays on that core for its lifetime.
```

> **Rationale (strict):** Contracts are enforced, not interpreted (§3.7). A contract that names a core means the developer expressed deployment intent. Silently rerouting to a different core would be exactly the kind of reinterpretation a capability-based system is designed to forbid. Contracted placement is deployment-coupled by design.

**On restart:** the placement decision is re-evaluated from scratch using the rules above. The supervisor does not remember the previous core.

- If the contract specified a core, that core is required again. If it is unavailable, the restart fails with `PlacementInvalid` — the same strict rule as initial spawn.
- If the contract did not specify a core, round-robin selects a fresh core, which may differ from the previous one.

This is consistent with invariant 11 (identity is stable; location is not). Sticky placement would make location stable across restart, contradicting the principle. Mid-execution migration remains forbidden.

### 9.3 Yield

```rust
yield() -> ()
```

Yield is **advisory**. Preemption remains enforced regardless. The scheduler does not rely on cooperative behavior for correctness or fairness.

### 9.4 Cross-Core Wakeups

When a syscall on core 0 needs to wake a task on core 1 (e.g., `send` woke a blocked `recv`), the kernel sends an IPI to core 1. The receiving core's IPI handler re-enters the scheduler.

### 9.5 What v1 Does Not Have

- Work stealing
- Load balancing
- Priority scheduling
- Service migration
- Core hotplug (cores discovered at boot are fixed for system lifetime)

---

## 10. Memory Model

### 10.1 Isolation

Each service has a separate virtual address space established at spawn time via per-task page tables.

### 10.2 Limits

```toml
[resources.memory]
request = "32MiB"   # minimum needed to start
limit   = "64MiB"   # maximum permitted
```

### 10.3 Enforcement

```text
  Service memory action
    │
    ├── alloc within limit ─────────────▶  Memory returned  ──▶  Continue
    │
    ├── alloc beyond limit ─────────────▶  AllocDenied  ──▶  Service continues (may degrade)
    │
    └── access outside mapped region ──▶  Page fault  ──▶  Service killed
                                                                  │
                                                    Supervisor decides restart
```

### 10.4 Two Failure Modes

`AllocDenied` is **recoverable**. The service knows it asked for too much and can degrade.

A protection violation is **unrecoverable**. The service is in undefined state and the only safe response is termination.

### 10.5 TLB Coherence

When a page is unmapped (service killed, memory reclaimed), the kernel issues a TLB shootdown via IPI to every core. Operations resume only after acknowledgment from all cores. v1 minimizes unmap frequency by reclaiming memory only at service death.

---

## 11. Bootstrap Sequence

### 11.1 Sequence

```text
  Limine (Bootloader)
    ── loads kernel image, hands off to BSP
    ── supplies: physical memory map, framebuffer, SMP topology, HHDM

  Kernel BSP
    ── paging, IDT, GDT
    ── frame allocator, capability subsystem
    ── start APs (real-mode trampoline → long mode)
         APs: enter idle scheduler loop
    ── mark all available cores ready
    ── spawn init on Core 0

  init (Core 0)
    ── spawn supervisor on Core 0
    ── spawn logger on Core 0

  supervisor (Core 0)
    ── spawn registry on Core 0   (name service — FIRST; holds its cap)
    ── read boot manifest
    ── spawn services per placement policy (§9.2),
       wiring each from its name→cap map (no kernel name resolution)

  ── System reaches multi-core steady state ──
```

> **Amendment 2026-06-21 (naming Phase 3b): `registry` is spawned by the `supervisor`, not
> `init`.** Moving name→endpoint resolution out of the kernel (`docs/naming-design.md`, §4.4/§26.10)
> makes the supervisor the name authority. For it to wire every service's `registry` peer from a
> capability it holds — rather than the kernel resolving the name — the supervisor must hold
> registry's cap, so it now spawns registry **first**, before any service that registers with it.
> `init` spawns only `supervisor` + `logger`. Registry's *boot-time* spawn failure is still **fatal**
> (the supervisor aborts → kernel panic, same reason string), preserving §11.3; its *runtime* death is
> still a supervisor restart (H11, §6.2). The kernel still records the name→endpoint mapping at spawn
> (used by the restart path + the registry-bootstrap stopgap) until that path is removed in Phase 5.

The bootloader is **Limine**, accessed via the Limine Boot Protocol. Limine is responsible for loading the kernel image, supplying the physical memory map, the framebuffer descriptor, kernel relocation info, and the SMP topology (APIC IDs of all available cores). See Appendix A for the bootloader rationale and installation story.

### 11.2 BSP and APs

- **BSP (Bootstrap Processor)** — the first core to execute kernel code. Responsible for kernel init and bringing APs online.
- **APs (Application Processors)** — secondary cores. Brought up via real-mode trampoline (`arch/x86_64/ap_boot.rs`), then jump to long-mode kernel code, then enter idle.

Because Limine supplies APIC IDs directly, the kernel does not need to probe ACPI/MADT for SMP topology. This removes a non-trivial subsystem from `arch/x86_64/ap_boot.rs`.

### 11.3 Failure During Bootstrap

| Failing component | Effect                                |
|-------------------|---------------------------------------|
| Bootloader        | Hardware reset                         |
| Kernel BSP init   | Kernel panic, halt                     |
| AP startup        | Kernel logs warning, continues with available cores; if zero APs come up, system runs as single-core |
| init spawn        | Kernel panic, halt                     |
| supervisor spawn  | Kernel panic, halt (TCB)               |
| registry spawn (boot-time, by the **supervisor** — Phase 3b) | Kernel panic, halt — the supervisor aborts (the name service must come up to bootstrap). *Runtime* death is recovered by the supervisor (H11; §6.2), not a reboot. |
| logger spawn      | Init logs to kernel ring buffer, retry |
| Application svc   | Supervisor logs, may retry per policy  |
| Service contracted to unavailable core | Spawn rejected with `PlacementInvalid`; supervisor logs and skips; system runs without that service |

### 11.4 Logging Before Logger Exists

The kernel maintains a 16 KiB ring buffer (per-core view, single shared sink). Anything logged before the logger is up writes to the ring buffer and the serial console. When the logger starts, it drains the buffer.

---

## 12. Drivers and Interrupts

### 12.1 Model

Kernel routes interrupts. Drivers are user-space services. Essential drivers (block-driver) are trusted in v1.

### 12.2 Routing

```text
  Hardware IRQ N (arrives on some core)
    │
    ▼
  Kernel IDT handles interrupt
    │
    ▼
  Interrupt Router dispatches by IRQ number
    │
    ▼  IPC message to driver's interrupt endpoint
  Driver Service
    ── recv() returns interrupt event
    ── handles device via MMIO capability
```

If the driver runs on a different core than the one receiving the IRQ, routing crosses cores via the same IPC mechanism as any other cross-core send.

### 12.3 Driver Capabilities

```toml
[capabilities]
hw_interrupt = [12]                       # IRQ line
hw_mmio      = ["0xfee00000+0x1000"]      # MMIO region
```

The kernel validates these at spawn time and grants caps only for the specified resources.

---

## 13. Service Contracts

### 13.1 Format

```toml
name    = "ping"
version = "0.1.0"

[resources.memory]
request = "32MiB"
limit   = "64MiB"

[capabilities]
ipc_send    = ["pong"]
ipc_receive = ["ping"]
log_write   = true

[placement]
core = 0    # optional; if omitted, supervisor uses round-robin
```

### 13.2 Placement Field (Strict)

- **Omitted** → supervisor places via round-robin across available cores.
- **Specified** → supervisor requires that exact core. If unavailable, spawn is rejected with `PlacementInvalid`.

> **Strict semantics:** A contract that names a core is a deployment-intent statement. The supervisor will not silently reroute to a different core. If `placement.core = 2` and core 2 didn't come up, the service does not start; the supervisor logs the rejection and continues. Contract authors should specify placement only when they have a real reason.

### 13.3 Request, Not Permission

The developer declares **what the service needs**. The OS decides **whether to grant it**.

### 13.4 Build-Time Validation

`osdev` validates contracts using JSON Schema applied to the TOML structure.

**Guarantees:** correct structure, valid capability names, valid resource declarations, valid core IDs (range check only), required fields present.

**Non-guarantees:** behavioral correctness, that the binary uses only declared caps, that limits are reasonable, that the requested core will be available at spawn time.

> Build-time validation is structural, not behavioral. Behavioral enforcement is runtime-only in v1.

### 13.5 Schema

`contracts/schema/service.schema.json`. Versioned. Breaking changes require a major version bump and a documented migration.

### 13.6 Runtime Enforcement

Every syscall checks the calling task's capability table, populated *from* the contract at spawn time. The kernel does not consult the contract at runtime.

---

## 14. Service Lifecycle

### 14.1 Spawn

1. Supervisor reads service binary + contract.
2. Validates contract against schema.
3. Determines core placement (round-robin or contract override per §9.2; rejected if contracted core unavailable).
4. Asks kernel to create a new task on that core with declared resources.
5. Kernel mints capabilities per contract (each tagged with current resource generation).
6. Kernel maps service binary into new address space.
7. Service registers any owned endpoints with registry.
8. Service enters main loop on its assigned core.

```text
  Supervisor ─── spawn(name, contract) ──▶  Kernel
                                               │
                              resolve placement (§9.2):
                              contract core or round-robin
                                               │
                        Err(PlacementInvalid) ◀─┤  (core unavailable: stop here)
                                               │
                              allocate Task + CapTable from contract
                              map binary into new address space
                              enqueue task on target core run queue
                                               │
  Supervisor ◀── Ok(task_id) ────────────────┘
                                               │
  New Service:                      enters execution
    ── registers endpoint with registry
    ── service_main() ── enters work loop
```

### 14.2 Restart and Cap Rebinding (Possibly Cross-Core)

```text
  Steady state: Service A (Core 0) holds cap to Service B gen=4 (Core 1)

  Step 1 — Supervisor kills B:
    Kernel closes B's endpoints, drains queues
    Kernel bumps B's generation: 4 → 5
    Kernel reclaims B's memory (TLB shootdown on all cores)

  Step 2 — A tries to send via stale cap:
    A ── send(gen=4 cap) ──▶ Kernel ── EndpointDead ──▶ A
                              (gen mismatch: held=4, current=5)

  Step 3 — Supervisor spawns B on Core 2:
    Kernel starts B with fresh cap table (gen=5)
    B registers endpoint with registry

  Step 4 — A reacquires:
    A ── registry.lookup("B") ──▶ fresh cap (gen=5, Core 2)
    A ── send(gen=5 cap) ──▶ Ok  (routes to Core 2)
```

**Key principle:** the new instance may run on a different core than the old one. Clients never know — they see only `EndpointDead`, look up via registry, and resume.

### 14.3 Cascading Failure

Cascading failure is **the client's responsibility**. There is no implicit recovery. If A depends on B and B restarts, A's next syscall returns `CapRevoked` or `EndpointDead`, and A must retry, degrade, or fail.

### 14.4 Supervisor API

```rust
supervisor.kill(service_name) -> Result<()>
supervisor.restart(service_name, placement_override?: CoreId) -> Result<()>
```

Required capability: `service_control` (held only by supervisor).

- **`kill`** — immediate. The killed service does not get a chance to clean up. Clean shutdown is a separate codepath.
- **`restart`** — kill followed by spawn. Placement is determined as follows:
  - If `placement_override` is provided, the supervisor requires that core (subject to the strict rules of §9.2; rejected if the core is unavailable).
  - If `placement_override` is omitted, the placement decision is re-evaluated from scratch per §9.2 — contract-specified placement re-applies (and may fail with `PlacementInvalid` if unavailable); unspecified placement gets a fresh round-robin choice.
  - **The previous core is not remembered.** A service that ran on core 1 before the restart has no implicit affinity for core 1 after.

### 14.5 Kill Semantics

Kill is for misbehaving services and for restart. It is not a graceful shutdown mechanism.

---

## 15. State and Persistence

State belongs to services, not the kernel. Services that need to survive restart must persist externally and reconstruct on startup.

The filesystem service is the externalization mechanism for everyone else and cannot persist *to itself*. Resolution: the block driver holds a direct hardware capability and stores fs metadata. `fs` gives itself **transactional metadata recovery** — every mutation commits through a crash-consistent redo-journal and `fs` recovers to a consistent state on mount (`docs/persistence.md` §6.8). With that, `block-driver` and `fs` are **restartable** and no longer in the TCB (§6.1 Phase D amendment, 2026-06-17); their death is a supervisor restart, not a reboot.

Stateless services (logger in v1) restart trivially. Example application services (ping, pong) are also stateless and restart trivially — but they are demonstration services in `examples/`, not permanent architectural components.

---

## 16. Update Model

```text
  Update package
    │
    ├── Signature valid?   No  ──▶  Reject
    │       Yes ↓
    ├── Contract valid?    No  ──▶  Reject
    │       Yes ↓
    └── Policy allows?     No  ──▶  Reject
            Yes ↓
    Supervisor restarts service with new binary
```

Push vs pull is irrelevant — verification is the security property. Live code update is permanently rejected (§2.5); the only update mechanism is restart-with-new-binary.

---

## 17. Developer Workflow

```bash
osdev new <service-name>            # scaffold
osdev build                         # build kernel + services
osdev run --smp <N>                 # boot in QEMU with N cores
osdev image                         # build bare-metal image → build/os.img (UEFI GPT)
osdev publish                       # package + serve a service
osdev restart <service> [--core N]  # restart in running OS; --core is dev-mode only
osdev logs <service>                # tail logs
osdev status <service>              # show service state + assigned core
osdev caps <service>                # show held capabilities
osdev test identity                 # run §22 test suite
```

**`osdev restart --core N`** is the CLI surface for the supervisor's `placement_override` (§14.4). It is rejected outside dev mode and subject to the same strict placement rules as a contract-specified core.

Iteration loop: edit → `build` → `publish` → `restart` → `logs`. Only the changed service restarts.

---

## 18. Unsafe Policy

### 18.1 Permitted

`kernel/src/arch/`, `kernel/src/memory/`, `kernel/src/capability/`, `kernel/src/smp/`.

**Plus the SDK's audited hardware/ABI layer:** the syscall ABI (`raw_syscall`,
inline `asm!`) and the MMIO/DMA accessor modules (`sdk/rust/src/mmio.rs` and
`sdk/rust/src/dma.rs`). Userspace drivers (§12) cannot touch device registers or
DMA memory without `unsafe`; isolating it to these designated SDK modules — each
block carrying a `// SAFETY:` comment — keeps the *driver services themselves*
`unsafe`-free behind safe wrappers (`Mmio::read32`, `Dma::write32`, etc.), exactly
as the kernel isolates its `unsafe` to the four layers above. This is a recognition
of what the syscall ABI already required, extended to the hardware access §12 needs.

### 18.2 Forbidden

All userspace services, `osdev/`, and all of `sdk/` **except** the audited
hardware/ABI layer named in §18.1 — and all kernel code outside the four
permitted layers. A driver service that writes `unsafe` directly (rather than
going through the SDK's safe `Mmio`/`Dma` wrappers) is rejected.

### 18.3 Documentation

Every `unsafe` block carries a SAFETY comment:

```rust
// SAFETY: <argument for why this is sound>
unsafe { ... }
```

A PR with an unsafe block lacking a SAFETY comment is rejected without review.

### 18.4 Audit Trail

`docs/unsafe-audit.md` lists every unsafe block. CI checks the file matches source.

### 18.5 Grandfathered Floors

`unsafe` outside the four permitted layers (§18.1) is tolerated only as
**grandfathered** lines in `task/`, `syscall/`, and `interrupt/`, frozen at the
counts in `docs/unsafe-audit.md`. Those counts may **decrease** freely but may
**increase** only by an amendment recorded here and in the audit, with a written
safety + necessity rationale.

New `unsafe` that a feature or hardening needs must first try to live in a permitted
layer (`arch/`, `memory/`, `capability/`, `smp/`) rather than grow a grandfathered
file. A precondition that is merely *boot-ordering* (violating it wedges boot — a
liveness bug, not UB) does **not** justify an `unsafe fn`; make it a safe `fn` with a
documented contract, like `memory::init` / `smp::init`. Worked example: the H4
kstack-guard / W^X hardening (2026-06-08) was structured so its page-table `unsafe`
lives in `arch/` and the boot call sites are safe `fn`s — `main.rs` and `task/mod.rs`
stayed at their floors with **no amendment needed**. There are currently no
amendments to the grandfathered floors.

---

## 19. Debugging Model

Bugs are classified as one of: `kernel`, `arch`, `memory`, `ipc`, `capability`, `smp`, `service`, `cli`.

Bug reports include: logs (kernel ring buffer + service), contract(s), QEMU repro steps including `-smp N` value, expected behavior, actual behavior, suspected classification.

Kernel panics write to serial console **and** a reserved memory page that survives reboot. On next boot, init logs the stored panic reason. Panics on any core halt the system.

---

## 20. Performance Philosophy

Ranked: correctness, observability, performance.

The IPC fast path is the exception. In a microkernel, every meaningful operation crosses an IPC boundary. v1 IPC uses simple predictable structures: one memcpy, constant-time cap + generation check, one IPI on cross-core wake.

No vibes-based optimization. Perf claims require benchmarks in `tests/qemu/perf/`.

---

## 21. Contribution Rules

A PR is rejected without further review if it:

- Introduces global mutable state outside a single owning service.
- Introduces ambient authority.
- Bypasses the capability system or generation check.
- Adds service migration or work stealing.
- Adds zero-copy IPC.
- Adds live code update.
- Introduces silent fallbacks at the kernel boundary.
- Breaks restartability of a non-TCB service.
- Adds unsafe code without a SAFETY comment, or outside permitted layers.
- Adds a syscall that does not validate a capability.
- Changes the IPC fast path without a benchmark.
- Edits CLAUDE.md without a rationale in the commit message.

Reviewers ask: does this respect the constitution, leave the kernel small, present a convincing unsafe argument, include a test, make the system more or less understandable?

---

## 22. Identity Test Suite

### 22.1 Purpose

The identity tests are the minimum set of tests that, **if any one fails, the system is no longer the system this document describes**.

They are the executable form of the constitution. If you change the spec in a way that invalidates one of these tests, you have changed system identity. That requires a CLAUDE.md amendment.

### 22.2 Categorization

The test suite is layered. Each layer answers a different question about kernel correctness. A pass on one layer is necessary but not sufficient. Battle-hardening requires passing all layers.

| Category    | Purpose                                              | Bar (what failure means)                          | Status |
|-------------|------------------------------------------------------|---------------------------------------------------|--------|
| Identity    | Pin constitutional decisions; existence proof        | Constitutional invariant violated                 | §22 (Test 11 added, H11) |
| Property    | Universal invariants under random inputs             | A claim the spec makes does not hold              | Active |
| Fuzz        | Crash resistance under adversarial inputs            | Kernel panics on user-controllable input          | Active |
| Stress      | Survival under sustained load                        | Drift, leaks, or corruption appear over time      | Active |
| Performance | Latency / throughput benchmarks                      | A measured number regressed                       | §22 (10/10) |
| Adversarial | Capability isolation under attack                    | An attack succeeds where the spec says it fails   | Active |
| Chaos       | Graceful degradation under partial failures          | A defined failure mode is not handled cleanly     | Active |

```text
  ┌─────────────────────────────────────────────────────────────────────┐
  │  Foundation — must pass before anything else                        │
  │  Identity (§22) — Tests 1–11 ✅ — Constitutional invariants        │
  └──────────────────────┬──────────────────────────────────────────────┘
          ┌──────────────┴─────────────────┐
          ▼                                ▼
  ┌───────────────────┐          ┌───────────────────┐
  │  Property         │          │  Fuzz             │
  │  Universal invs.  │          │  Crash resistance │
  │  under random     │          │  on adversarial   │
  │  inputs           │          │  inputs           │
  └───────┬───────────┘          └──────────┬────────┘
          ▼                                 ▼
  ┌───────────────────┐          ┌───────────────────┐
  │  Stress           │          │  Adversarial      │
  │  No drift, leak,  │          │  Cap isolation    │
  │  or corruption    │          │  under attack     │
  └───────┬───────────┘          └──────────┬────────┘
          └─────────────┬────────────────────┘
                ┌───────┴────────┐
                ▼                ▼
      ┌──────────────────┐  ┌──────────────────┐
      │  Performance     │  │  Chaos           │
      │  Latency /       │  │  Graceful        │
      │  throughput      │  │  degradation     │
      │  baselines       │  │  under failures  │
      └──────────────────┘  └──────────────────┘
```

Tests live under `tests/qemu/`, organized by category:

```
tests/qemu/
  identity/      # §22.5–§22.6 (complete)
  property/      # see Property Tests below
  fuzz/          # see Fuzz Tests below
  stress/        # see Stress Tests below
  perf/          # see Performance Benchmarks below
  adversarial/   # see Adversarial / Red-Team Tests below
  chaos/         # see Chaos Tests below
  harness/       # shared infrastructure
```

The base harness (§22.3) boots the OS with a configurable `-smp N` value; multi-core tests assert on N ≥ 2. The battle-hardening categories extend it with longer timeouts (stress, perf), metric collection (perf), and fault injection (chaos).

The bar across every category is the same as identity: **no FAIL, no BLOCKED with a vague reason**. A failure means a real bug — fix it, add a regression test to the appropriate suite, then move on.

---

#### Identity (§22) — Complete

All passing (Tests 1–11; harness reports 23 cases incl. A/B + IR + Test 11). No regressions allowed. Any failure here is a constitutional violation that requires either a kernel fix or a CLAUDE.md amendment. **Test 11 (registry survives restart) was added by the H11 amendment (§6.1/§6.2).**

---

#### Property Tests

Property tests assert *universal* claims over randomized inputs. Identity tests prove the system *can* satisfy each invariant; property tests prove it *always* does. Each test runs thousands of iterations with QuickCheck-style generators.

| ID  | Property                                                                       | Pins                  |
|-----|--------------------------------------------------------------------------------|-----------------------|
| P1  | Random bytes → `CapNotHeld` or `CapInvalid`; never accepted as a cap           | §7.3 (unforgeable)    |
| P2  | Generation per service is strictly monotonic across its lifetime               | §7.5                  |
| P3  | Cap rights never widen during transfer                                         | §7.3 (non-escalating) |
| P4  | ∑ `task_alloc_bytes` ≡ pages mapped, after any sequence of alloc/free          | §10.3                 |
| P5  | Every live endpoint has exactly one owning task                                | §8.3                  |
| P6  | Queue head ≤ tail ≤ head + 16; count consistent with both                      | §8.5                  |
| P7  | After unmap + TLB shootdown, the page is unreadable from every core            | §10.5                 |
| P8  | After restart, name resolves to a task with the same name and higher generation | §14.2                 |
| P9  | Generation bump invalidates ALL holders, not just some                         | §7.5                  |
| P10 | Every `send` returns exactly one of {Ok, defined error} — never both, never neither | §8.6              |

Bar: any property failure is a logic bug. Kernel must be fixed before any other work proceeds.

---

#### Fuzz Tests

Fuzz tests find the inputs that crash the kernel that no one would write by hand. The bar is binary and absolute: **the kernel must never panic on user-controllable input.**

| ID  | Surface                          | Generator                                                                         |
|-----|----------------------------------|-----------------------------------------------------------------------------------|
| F1  | Syscall args (each × 1M iters)   | Random u64 in `a0/a1/a2`; including kernel addresses, unmapped pointers, misaligned values |
| F2  | Syscall numbers                  | Random u64 as `nr`; must return `UnknownSyscall`, never crash                     |
| F3  | ELF binaries                     | Bit-flip mutations of known-good ELFs handed to the spawner                       |
| F4  | Service contracts                | Malformed TOML; JSON Schema-invalid structures                                    |
| F5  | IPC message bodies               | Random bytes, random sizes up to 4 KiB                                            |
| F6  | Embedded caps in messages        | Messages claiming to carry caps with random structure                             |
| F7  | Cap generation field             | Random u64 as generation in cap usage; must return `CapRevoked` or `EndpointDead` |
| F8  | Memory request values            | Random sizes including > total RAM, `0`, and `u64::MAX`                           |

Any panic discovered by F1–F8 is a kernel bug. The fix is mandatory and includes a regression test added to the relevant identity or property suite.

---

#### Stress Tests

Stress tests find the bugs that only appear under sustained load. Identity tests prove correctness for individual operations; stress tests prove the system does not drift, leak, or corrupt over hours of operation.

| ID  | Scenario                                                | Duration       |
|-----|---------------------------------------------------------|----------------|
| S1  | IPC saturation: sustained `try_send` on a full queue    | 1 hour         |
| S2  | Restart storm: 100k kill/respawn cycles of one service  | until complete |
| S3  | Cross-core thrash: 4 cores × all-to-all IPC             | 10 min         |
| S4  | Cap table churn: 100k random create/destroy             | until complete |
| S5  | Generation overflow: force counter to wrap, observe wraparound semantics | until wrap + 1k operations |
| S6  | Long-running stability: ping/pong + introspection       | 24 hours       |
| S7  | Memory pressure: alloc-to-limit + free, 10k cycles      | until complete |
| S8  | Idle stability: boot, no workload, observe              | 24 hours       |
| S9  | Interrupt storm: high-frequency timer + IPI cross-fire  | 1 hour         |
| S10 | Cascading revocation: kill a service held by many; observe propagation | until propagated |

Bar: at the end of each test, the kernel has not panicked, memory accounting is consistent, all services are in a defined state, and no resource is leaked.

---

#### Performance Benchmarks

Performance benchmarks lock in numbers so regressions are detected commit-to-commit. Absolute values matter less than the deltas.

| ID  | Metric                                                |
|-----|-------------------------------------------------------|
| B1  | IPC same-core round-trip latency: p50, p99, p99.9     |
| B2  | IPC cross-core round-trip latency: p50, p99, p99.9    |
| B3  | Syscall floor: `yield` round trip                     |
| B4  | Cap validation cost: one cap + generation check       |
| B5  | Spawn cost: `supervisor.spawn` → service "ready"      |
| B6  | Restart cost: kill + spawn                            |
| B7  | Cap table contention: throughput at 1, 2, 4 cores     |
| B8  | Allocator throughput: pages/sec under contention      |
| B9  | Message copy cost: 4 KiB upper-bound copy             |
| B10 | Scheduler decision cost: time to pick next task       |

Results are committed to `tests/qemu/perf/baseline.json`. CI compares each run against baseline and flags regressions ≥ 10%. The §7.8 single global `RwLock` will surface most visibly in B7 — record the number now so the v2 sharded/RCU migration has a target.

---

#### Adversarial / Red-Team Tests

Adversarial tests verify capability isolation holds under direct attack. The system claims a capability model with no ambient authority; these tests run services that try to break that claim.

| ID  | Attack                                                                              |
|-----|-------------------------------------------------------------------------------------|
| A1  | Service crafts random u64 values and tries to use them as caps                      |
| A2  | Service brute-forces endpoint IDs across the u32 space                              |
| A3  | Service attempts to allocate beyond its contract memory limit through every syscall path |
| A4  | Service receives a cap with limited rights and attempts to use rights it lacks      |
| A5  | TOCTOU: service races a syscall with revocation of the cap it is about to use       |
| A6  | Service tries to fill the cap table to denial-of-service the kernel                 |
| A7  | Service tries to detect IPC partner identity via timing                             |
| A8  | Service tries to monopolize a core via a tight loop without yielding                |
| A9  | Service tries to spawn another service directly, bypassing the supervisor           |
| A10 | Service passes kernel addresses as syscall arguments                                |

Bar: every attack returns a defined error. Any attack that succeeds is a security hole; any attack that panics the kernel is a kernel bug. Both are mandatory fixes.

---

#### Chaos Tests

Chaos tests verify graceful degradation when something the kernel depends on fails partially. Total failures (kernel panic, TCB death) are covered by §6.2 and Test 1B. Chaos tests cover the *between* cases.

| ID  | Failure injected                                                          |
|-----|---------------------------------------------------------------------------|
| C1  | One or more APs fail to come up during boot                               |
| C2  | A service in the boot manifest has a corrupted ELF (non-TCB)              |
| C3  | Allocator forced to return `AllocFailed` at random syscall entry points    |
| C4  | Bootloader provides degraded environment (minimal RAM, no framebuffer)    |
| C5  | Kernel stack approaches exhaustion under deeply nested syscall            |
| C6  | One core's timer interrupt is dropped for an extended period              |
| C7  | TLB shootdown IPI delivery is delayed across cores                        |

Bar: the system either continues correctly with degraded capacity, or panics loudly with a defined reason. Silent corruption is never acceptable. Per invariant 12 (§3): failures are loud, never silent.

---

### 22.3 Test Harness

```text
  osdev test identity
    │
    ├── build: identity-only supervisor + kernel
    ├── create disk image, install bootloader
    │
    └── for each test:
           │
           ├── boot QEMU (-smp N; + -enable-kvm when /dev/kvm available)
           ├── test service runs scenario
           ├── harness reads serial console line by line
           │
           ├── expected output matched?  ──▶  PASS
           ├── fail_on string seen?      ──▶  FAIL
           ├── timeout fires?            ──▶  FAIL
           │
           └── 500 ms isolation pause (OS reclaims QEMU pages)
```

### 22.4 Sequencing Note

"Tests before code" is aspirational. You cannot run any test until the kernel boots and IPC works. Honest sequence: write test specs (this section); build minimum kernel + harness; see tests fail for the right reasons; implement until they pass.

A test failing for the wrong reason (compile error, harness bug, missing cap not declared in test contract) is a failure of the test, not the kernel.

### 22.5 Conventions

- Each test has a **positive** case (the system permits what it should) and a **negative** case (the system refuses what it shouldn't).
- Pseudocode is illustrative, not literal Rust.
- Each test names the spec section it pins.
- Multi-core tests run with `-smp 4` minimum unless otherwise stated.

---

### Test 1: Bootstrap to Steady State

**Pins:** §11 (Bootstrap), §6 (TCB).

#### 1.A — Positive: Healthy Multi-Core Boot

```
test bootstrap_steady_state_positive:
    image = build_kernel(boot_manifest=[init, supervisor, registry, logger])
    qemu  = boot(image, smp=4)

    assert serial_contains("init: ready",       within=5s)
    assert serial_contains("supervisor: ready", within=5s)
    assert serial_contains("registry: ready",   within=5s)
    assert serial_contains("logger: ready",     within=5s)
    assert serial_contains("smp: 4 cores ready", within=5s)
    assert kernel_did_not_panic()
```

#### 1.B — Negative: TCB Failure Panics

```
test bootstrap_tcb_failure_panics:
    image = build_kernel(boot_manifest=[
        init, supervisor, logger,
        corrupt_binary("registry")
    ])
    qemu = boot(image, smp=4)

    assert serial_contains("KERNEL PANIC", within=5s)
    assert serial_contains("reason: registry spawn failed")
    assert qemu_state == "halted"
    assert no_app_services_started()
```

---

### Test 2: Capability Enforcement

**Pins:** §3.1 (no ambient authority), §7 (use validation).

#### 2.A — Positive

```
test cap_enforcement_positive:
    s = spawn_test_service(contract="[capabilities] log_write = true")
    assert s.invoke(Log("hello")) == Ok
    assert logger_received("hello")
```

#### 2.B — Negative

```
test cap_enforcement_negative:
    s = spawn_test_service(contract="[capabilities] # no log_write")
    assert s.invoke(Log("hello")) == Err(CapNotHeld)
    assert logger_did_not_receive("hello")
    assert s.is_alive()
```

---

### Test 3: IPC Send / Receive (Same Core)

**Pins:** §8 (IPC), §7.4 (`SEND` right).

#### 3.A — Positive

```
test ipc_send_recv_positive:
    a = spawn(contract="ipc_send=['b'], placement.core=0")
    b = spawn(contract="ipc_receive=['b'], placement.core=0")

    a.send(target="b", payload="ping")
    msg = b.recv()
    assert msg.payload == "ping"
```

#### 3.B — Negative

```
test ipc_send_without_send_right:
    cap_no_send = mint_endpoint_cap(target=b, rights=[])
    a.install_cap("b_endpoint", cap_no_send)
    assert a.try_send(target="b", payload="ping") == Err(CapInsufficientRights)
    assert b.queue_depth() == 0
```

---

### Test 4: Endpoint Death Semantics

**Pins:** §8.6 (failure semantics), §14.2 (restart drops endpoints), §8.5 (queue depth = 16).

#### 4.A — Positive: Send-After-Death Returns EndpointDead

```
test endpoint_death_send_returns_dead:
    b = spawn_simple_recv_service()
    a = spawn_with_cap_to(b.endpoint)

    supervisor.kill(b)
    wait_for_revocation()

    assert a.try_send(b.endpoint, "after death") == Err(EndpointDead)
```

#### 4.B — Negative: Blocked Sender Wakes With EndpointDead

The queue depth is 16 (§8.5), so this test fills it deterministically.

```
test blocked_sender_wakes_on_endpoint_death:
    b = spawn(non_consuming_receiver)            # never calls recv
    a = spawn_with_cap_to(b.endpoint)

    for i in 0..16:
        a.send("fill", index=i)                  # fills the 16-deep queue

    handle = a.send_async("blocked")             # 17th: must block
    assert handle.is_pending()

    supervisor.kill(b)
    wait_for_revocation()

    assert handle.poll(timeout=1s) == Err(EndpointDead)
    assert a.is_alive()
```

---

### Test 5: Capability Transfer Requires GRANT

**Pins:** §7.6 (transfer rule), §7.4 (`GRANT` right).

#### 5.A — Positive

```
test grant_positive:
    cap = mint_cap(target=resource_X, rights=[READ, GRANT])
    sender.install_cap("X", cap)

    msg = Message::with_cap(cap)
    assert sender.send(receiver.endpoint, msg) == Ok

    received = receiver.recv()
    assert received.embedded_cap.use(READ) == Ok
    assert sender.has_cap("X") == false
```

#### 5.B — Negative

```
test grant_negative:
    cap = mint_cap(target=resource_X, rights=[READ])     # no GRANT
    sender.install_cap("X", cap)

    msg = Message::with_cap(cap)
    assert sender.send(receiver.endpoint, msg) == Err(CapNotGrantable)
    assert sender.has_cap("X") == true
    assert receiver.queue_depth() == 0
```

---

### Test 6: Supervisor Restart and Cap Rebinding

**Pins:** §14.2 (restart flow), §14.3 (cascading failure).

#### 6.A — Positive

```
test supervisor_restart_positive:
    pong         = spawn("pong")
    original_pid = pong.pid

    supervisor.restart("pong")
    wait_for_serial("pong: ready", within=5s)

    assert pong.pid != original_pid
    assert kernel_did_not_panic()
    assert all_other_services_alive()
```

#### 6.B — Negative

```
test stale_cap_revoked_after_restart:
    ping = spawn("ping")
    pong = spawn("pong")

    supervisor.restart("pong")
    wait_for_serial("pong: ready")

    assert ping.send_via_stale_cap("hello") in [Err(CapRevoked), Err(EndpointDead)]

    fresh = ping.lookup_via_registry("pong")
    assert ping.send_via(fresh, "hello") == Ok
```

---

### Test 7: Memory Limit Enforcement

**Pins:** §10.3 (enforcement), §10.4 (two failure modes).

#### 7.A — Positive

```
test memory_alloc_within_limit:
    s = spawn(contract="memory.limit = 64MiB")
    assert s.alloc(32 * MiB) == Ok
    assert s.alloc(20 * MiB) == Ok
    assert s.is_alive()
```

#### 7.B — Negative (Two Modes)

```
test memory_alloc_beyond_limit_recoverable:
    s = spawn(contract="memory.limit = 64MiB")
    s.alloc(60 * MiB)
    assert s.alloc(20 * MiB) == Err(AllocDenied)
    assert s.is_alive()
    assert s.alloc(2 * MiB) == Ok

test memory_protection_violation_kills_service:
    s = spawn(contract="memory.limit = 64MiB")
    s.alloc(32 * MiB)
    s.write_to_unmapped_address(0xdead_0000)
    wait_for_kill(within=1s)
    assert s.is_dead()
    assert kernel_did_not_panic()
```

---

### Test 8: Preemption of Non-Yielding Service

**Pins:** §3.6 (no service can monopolize), §9.1 (10 ms quantum), §9.3 (yield is advisory).

#### 8.A — Positive

```
test yield_advisory_works:
    yielder = spawn_yielding_service(core=0)
    other   = spawn_logging_service(core=0)

    run_for(1s)
    assert other.log_count    >= 90
    assert yielder.tick_count >= 50
```

#### 8.B — Negative

```
test non_yielding_service_is_preempted:
    hog   = spawn_tight_loop_service(core=0)
    other = spawn_logging_service(core=0)

    run_for(1s)
    assert other.log_count >= 1     # minimum bar
    assert other.log_count >= 40    # near fair share
    assert hog.is_alive()
    assert kernel_did_not_panic()
```

---

### Test 9: Cross-Core IPC

**Pins:** §8.3 (routing), §8.4 (send flow), §9 (per-core scheduler).

#### 9.A — Positive

```
test cross_core_ipc_positive:
    a = spawn(contract="ipc_send=['b'], placement.core=0")
    b = spawn(contract="ipc_receive=['b'], placement.core=1")

    assert a.assigned_core == 0
    assert b.assigned_core == 1

    a.send(target="b", payload="hello from core 0")
    msg = b.recv()
    assert msg.payload == "hello from core 0"
```

#### 9.B — Negative

```
test cross_core_no_authority_leak:
    a = spawn(contract="placement.core=0")          # no caps to b
    b = spawn(contract="ipc_receive=['b'], placement.core=1")

    fake_cap = a.try_construct_cap(target=b.endpoint, rights=[SEND])
    assert fake_cap == Err(CapForgeryAttempted)
    assert b.queue_depth() == 0
```

---

### Test 10: Restart With Core Reassignment

**Pins:** §9.2 (placement, strict), §14.2 (restart can change core), §14.4 (placement override), §11 (identity stable; location not).

#### 10.A — Positive

```
test restart_changes_core_transparently:
    pong = spawn(contract="placement.core=1")
    assert pong.assigned_core == 1
    pong_gen_old = pong.generation

    supervisor.restart("pong", placement_override=2)
    wait_for_serial("pong: ready on core 2", within=5s)

    assert pong.assigned_core == 2
    assert pong.generation     > pong_gen_old
```

#### 10.B — Negative

```
test client_reacquires_after_core_change:
    ping = spawn(contract="placement.core=0")
    pong = spawn(contract="placement.core=1")

    supervisor.restart("pong", placement_override=2)
    wait_for_serial("pong: ready on core 2")

    assert ping.send_via_stale_cap("hello") == Err(EndpointDead)

    fresh = ping.lookup_via_registry("pong")
    assert fresh.target_core == 2
    assert ping.send_via(fresh, "hello") == Ok
    assert pong.received("hello")
```

---

### Test 11: Registry Survives Its Own Restart (H11)

**Pins:** §6.1/§6.2 (registry is restartable, not trusted root), §14 (supervisor restart authority), §3.11 (identity over location).

The registry is a restartable userspace name service (amendment 2026-06-09). Killing
it must NOT panic the kernel; the supervisor must observe its death (via the kernel's
death-notification) and respawn it, and name resolution must recover.

```
test registry_survives_own_restart:
    wait_for_serial("supervisor: ready")
    control.kill("registry")                       # was rejected pre-H11 (trusted root)

    assert serial_contains("supervisor: registry died, restarting")
    assert serial_contains("supervisor: registry restarted")
    assert serial_contains("registry: ready")      # fresh instance up
    assert kernel_did_not_panic()
    # Names re-populate via push re-registration (services re-announce); clients that
    # looked up during the gap retry (§14.3).
```

---

### Test 12: Confined Driver Cannot DMA Outside Its Arena (H1)

**Pins:** §3.1 / invariant 1 (no ambient authority — the DMA gap), §6.4 (DMA-capable
drivers are least-privilege when IOMMU-confined), §12 (drivers).

The point of H1: a confined DMA-capable driver's device can reach only its granted
arena. A DMA outside it must be **blocked at the IOMMU** (a logged `IO_PAGE_FAULT` on
real hardware), never silently read/write other memory. This is the executable form of
the §6.4 trust claim — without it, "confined" is a word, not a guarantee.

Runs only where an IOMMU is present (`osdev test iommu` launches QEMU with
`-device amd-iommu`); on a machine with no IVRS the driver is trusted (§6.4) and the
test is not applicable.

**Verification is structural, not a live fault, and the reason is a QEMU limitation.**
QEMU's emulated `amd-iommu` installs the device tables and page tables but does **not**
enforce translation faults on unmapped I/O addresses — a device DMA to an unmapped page
is silently allowed through. So a live `IO_PAGE_FAULT` cannot be observed under QEMU.
The harness therefore asserts the property QEMU *can* be made to prove: the kernel's
confinement **selftest** — a CPU-side walk of the device's I/O page table — confirms the
arena translates identity and the page one past it is **unmapped** (so a DMA there has no
translation and *would* fault on conforming silicon). That selftest is exactly the
structure an `IO_PAGE_FAULT` is raised from; pinning it pins the guarantee. (The live
fault itself is hardware-verified on the T630 and reproducible anywhere via the
`iommu-fault-test` build feature, which confines the driver to an *empty* domain so its
normal init DMA lands out-of-arena.)

```
test confined_driver_dma_faults:           # osdev test iommu
    boot(smp=2, iommu=on)                                     # q35 + amd-iommu + qemu-xhci + usb-kbd
    assert serial_contains("iommu: ... confined BDF")         # driver is confined to its arena

    # The kernel's confinement selftest walks the device's I/O page table: the arena
    # maps identity, and the page one past arena_end has NO mapping — the structural
    # form of "out-of-arena DMA would fault" (QEMU can't raise the fault itself).
    assert serial_contains("iommu: selftest PASS")
    assert serial_contains("(outside) unmapped")

    assert serial_contains("keyboard found")                  # driver still operates THROUGH the domain
    assert kernel_did_not_panic()                             # confinement is not fatal to a well-behaved driver

# Live-fault form (hardware / `--features iommu-fault-test`, not QEMU's lenient model):
#   confine the driver to an EMPTY domain → its first init DMA is out-of-arena →
#   IOMMU raises IO_PAGE_FAULT, kernel logs it, memory outside the arena is unchanged.
```

---

### Test 13: Filesystem Survives Its Own Restart (Phase D)

**Pins:** §6.1/§6.2 (`fs` + `block-driver` are restartable, not trusted root), §6.3 (TCB-shrink
goal reached), §15 (transactional recovery), §14 (supervisor restart authority), §3.11.

`fs` and `block-driver` are restartable userspace services (Phase D amendment 2026-06-17), made
safe by `fs`'s crash-consistent recovery (Phase C). Killing `fs` must NOT panic the kernel; the
supervisor must observe its death and respawn it; `fs` must re-mount to a consistent state
(persisted data intact) and re-register; and a client must reacquire it via the registry and
keep working.

```
test fs_survives_own_restart:                     # osdev test fs-restart
    boot(bare-metal + AHCI disk; shell on COM1, control on COM2)
    shell("drives flash data" → y)                # format
    shell("write /t.txt survives-restart")
    assert shell("read /t.txt") contains "survives-restart"

    control.kill("fs")                            # was a panic+reboot pre-Phase-D

    assert serial_contains("supervisor: fs died, restarting")
    assert serial_contains("supervisor: fs restarted")
    assert serial_contains("fs: serving file API")   # re-mounted + re-registered

    # The shell reacquires a fresh fs cap via the registry (§14.3); the file persisted.
    assert shell("read /t.txt") contains "survives-restart"
    assert kernel_did_not_panic()
```

---

### Test 14: File Is a Capability (P2)

**Pins:** §7.3 (cap properties), §7.10 (delegated resource capabilities), §3.1 (no ambient
authority), §3.3 (authority by capability, not identity).

The P2 amendment (2026-06-18) makes a file a real, kernel-minted capability via delegated resource
caps. This test pins that the file cap is a *genuine* capability — not a service-level token — by
exercising the three properties that distinguish one: unforgeable, revocable, non-escalating.

```
test file_is_a_capability:
    # fs hands out a real cap on open (delegated resource cap, §7.10)
    rw = fs.open("/doc.txt", rights=[READ, WRITE])     # returns a file capability
    assert rw.write("hello") == Ok
    assert rw.read()         == "hello"

    # Unforgeable (§7.3): a fabricated handle is not a cap
    assert use_random_handle_as_file_cap() in [Err(CapNotHeld), Err(CapInvalid)]

    # Non-escalating (§7.3): a READ-only file cap cannot write
    ro = fs.open("/doc.txt", rights=[READ])
    assert ro.read()          == "hello"
    assert ro.write("nope")   == Err(CapInsufficientRights)

    # Revocable (§7.5/§7.10): deleting the file revokes every cap to it
    fs.delete("/doc.txt")                               # fs revokes the resource (gen bump)
    assert rw.read()  == Err(CapRevoked)
    assert ro.read()  == Err(CapRevoked)
    assert kernel_did_not_panic()
```

**Implemented (2026-06-18) as `osdev test file-cap` (9/9 ✅).** The shell `fcap <file>` command opens a
file as a real kernel capability and exercises every property above end-to-end: read/write *through*
the cap; non-escalation at **both** layers (the kernel rejects a READ-only cap's WRITE invocation with
`CapInsufficientRights`, and `fs` refuses a write op carried under a read-validated badge — `op ≤ right`);
a fabricated handle is rejected (unforgeable); and the cap is `CapRevoked` after close/delete (revocable).
The badge that carries the validated `(resource_id, right)` to `fs` is an unforgeable kernel-set `Message`
field (`LastRecvBadge` syscall) — a client cannot fake a file-cap invocation over its ordinary `fs` send cap.

---

### 22.6 Test Coverage Matrix

| Test                              | Spec sections pinned       | Constitutional invariant      |
|-----------------------------------|----------------------------|-------------------------------|
| 1. Bootstrap                      | §11, §6                    | TCB integrity                 |
| 2. Capability enforcement         | §3.1, §7                   | No ambient authority          |
| 3. IPC send/receive (same core)   | §8, §7.4                   | Authority is explicit         |
| 4. Endpoint death                 | §8.5, §8.6, §14.2          | Restartability                |
| 5. Capability transfer            | §7.6, §7.4                 | Authority is explicit         |
| 6. Supervisor restart             | §14.2, §14.3               | Restartability                |
| 7. Memory limits                  | §10.3, §10.4               | Isolation                     |
| 8. Preemption                     | §3.6, §9.1, §9.3           | No service monopoly           |
| 9. Cross-core IPC                 | §8.3, §8.4, §9             | Identity over location        |
| 10. Restart with core change      | §9.2, §14.2, §14.4, §11    | Identity over location        |
| 11. Registry survives restart     | §6.1, §6.2, §14, §3.11     | Restartability / TCB shrink   |
| 12. Confined driver DMA faults    | §3.1, §6.4, §12            | No ambient authority (DMA)    |
| 13. fs survives own restart       | §6.1, §6.2, §6.3, §15, §14 | Restartability / TCB shrink   |
| 14. File is a capability          | §7.3, §7.10, §3.1, §3.3    | Authority is explicit (files) |

If any cell becomes obsolete, the corresponding spec section is being changed and the change requires a CLAUDE.md amendment.

---

## 23. First Milestone ✅ Complete

### 23.1 Goal

```
Boot the OS multi-core.
Start two services (ping on core 0, pong on core 1).
Send a message between them via cross-core IPC.
Kill pong.
The supervisor restarts it (possibly on a different core).
The system continues running.
```

### 23.2 Acceptance Criteria ✅

1. `osdev run --smp 4` boots the OS with 4 cores; init, supervisor, registry, logger, ping, and pong reach steady state. ✅
2. ping placed on core 0; pong placed on core 1. ✅
3. `osdev logs ping` shows ping sending a message every second. ✅
4. `osdev logs pong` shows pong receiving each message (cross-core IPC). ✅
5. `osdev restart pong --core 2` kills pong on core 1 and respawns it on core 2. ✅
6. ping observes `EndpointDead` and reacquires via the registry; the new cap routes to core 2. ✅
7. After reacquisition, ping and pong continue communicating across the new core boundary. ✅
8. The kernel does not panic on any core. ✅
9. **All ten identity tests in §22 pass.** ✅

### 23.3 Bare-Metal Achievement ✅

GodspeedOS has booted on real x86_64 hardware (4-core CPU, 4 GB RAM) via UEFI USB boot (2026-05-21).

- `osdev image` produces a UEFI GPT disk image at `build/os.img`.
- Image written to USB with Cygwin `dd`; boots via `BOOTX64.EFI` (Limine 12.x).
- All 4 cores come up; cross-core IPC (ping core 0 → pong core 1) runs continuously on hardware.
- Null modem serial (115200 8N1, PuTTY) confirms boot output; log appended to `build/putty_serial_output.log`.
- `bare-metal` supervisor feature excludes harness-driven probe services that require QEMU's control port.

**Persistence + file-as-capability hardware-proven on the HP T630 (2026-06-18).** Booted the bare-metal
`os.img` on the T630 (AMD GX-420GI, real AHCI SSD) and ran the shell `selfcheck` suite: **`ran 163,
failed 0`**. That run includes the full persistence stack (GSFS0008 format, journal, fsck/`drives check`,
read-only `drives scrub`) and the **file-as-capability** self-check (`fcap`, §22 Test 14): a file opened
as a real kernel capability, read/written *through* the cap, non-escalation enforced at both the kernel
and fs layers, a forged handle rejected, and the cap revoked on close and on rename — all green on real
hardware, no panic. The §7 "north star" (a file *is* a capability, true not approximate) is hardware-
validated, not just QEMU. Serial in `build/putty_serial_output.log`.

Hardware performance data (perf-brutal-only build, ~3 GHz CPU, 2026-05-21):

| Benchmark | Result |
|-----------|--------|
| BP1 IPC same-core p50 | 55,320 cycles (~18.4 µs) |
| BP3 yield floor | 39,903 cycles (~13.3 µs) |
| BP4 cap validation | 495 cycles (~165 ns) |
| BP5 spawn cost | 8,121,378 cycles (~2.7 ms) |
| BP6 restart cost | 14,462,309 cycles (~4.8 ms) |
| BP7 cap table | 1,168 cycles (~389 ns) |
| BP8 allocator | 616 cycles/4KiB page (~205 ns) |
| BP9 message copy 4KiB | 20,073 cycles (~6.7 µs) |
| BP10 scheduler decision | 2,323 cycles (~774 ns) |
| BP2 IPC cross-core | Not measured on J5005 (Goldmont+ stalled cross-core under load) — measured on the T630, see table below |

HP T630 (AMD GX-420GI, Jaguar/Puma+, ~2 GHz) — per-probe **isolated** measurements
(one benchmark alone: no ping/pong, no competing probes; 2026-05-31, build `ed8a151`).
µs/ms at ~2 GHz:

| Benchmark | T630 isolated | J5005 (above, perf-brutal) |
|-----------|---------------|-----------------------------|
| BP1 IPC same-core p50 | ~102,600 cycles (~51 µs) † | 55,320 |
| BP2 IPC cross-core p50 | 1,433,087 cycles (~0.72 ms); p99/p999 16,409,799 | not measured |
| BP3 yield floor | 19,281 cycles (~9.6 µs) | 39,903 |
| BP4 cap validation | ~1,258 cycles (~0.63 µs) † | 495 |
| BP5 spawn cost | 45,406,292 cycles (~22.7 ms) | 8,121,378 |
| BP6 restart cost | 54,231,139 cycles (~27.1 ms) | 14,462,309 |
| BP7 cap table | 2,932 cycles (~1.5 µs) | 1,168 |
| BP8 allocator | ~1,472 cycles/4KiB (~0.74 µs) † | 616 |
| BP9 message copy 4KiB | 21,796 cycles (~10.9 µs) | 20,073 |
| BP10 scheduler decision | 20,726 cycles (~10.4 µs) | 2,323 |

All cycle counts. † = perf-brutal in-suite p50 (already low-variance); the rest are
single-probe isolation builds (`osdev image --mode iso-bp{3,5,7,9,10}`; bp5 covers BP5+BP6).
The two columns are **not** a clean head-to-head: J5005 was perf-brutal (contended), the T630
column is isolated (uncontended), so they compare fairly only where both were clean —
BP1/BP4/BP8 at ~1.9–2.5×, genuine Jaguar-vs-Goldmont IPC. Where the T630 number is *lower*
(BP3) the J5005 figure was itself contention-inflated. BP5/BP6 (spawn/restart) are
memory-bandwidth-bound — the low-power thin client lags most there. The full investigation
behind BP2 (the COM2 timer-ISR wedge, fixed `a306fd3`) is in `bugs/1_FINDINGS_AP_TO_BSP_IPI.md`.

### 23.4 Out of Scope for v1

Filesystem persistence beyond the trusted block driver, network stack, work-stealing scheduler, service migration, zero-copy IPC, live code updates, restartable block driver / fs, update model in production mode, core hotplug, per-endpoint queue depth in contract.

---

## 24. Glossary

- **AP** — Application Processor. Any core other than the BSP.
- **BSP** — Bootstrap Processor. The first core to execute kernel code.
- **Capability** — Unforgeable token: ResourceId + Rights + Generation.
- **Delegated resource capability** — A capability for a resource whose *meaning* is defined by a service (e.g. a file owned by `fs`), not the kernel. The owning service mints and revokes it (`resource_mint`/`resource_revoke`, gated by a `RESOURCE_MINT` cap); the kernel validates and routes it as for any cap, badging a send with the opaque `ResourceId` so the owner knows which resource. The mechanism behind file-as-capability (§7.10, P2).
- **Endpoint** — IPC destination owned by a service. Bounded queue, depth 16 in v1.
- **Generation** — Monotonic counter on resources; mismatch on cap use indicates the resource was destroyed or replaced.
- **Grant** — The right to transfer a capability via IPC.
- **Identity test** — A test that pins a constitutional decision (§22).
- **IPI** — Inter-Processor Interrupt. Mechanism for a core to wake another core.
- **Limine** — The bootloader used in v1. See Appendix A.
- **Placement** — Strict assignment of a service to a specific core at spawn time. Re-evaluated from scratch on every spawn, including post-restart spawns; the previous core is not remembered.
- **PlacementInvalid** — Error returned when a contracted core is unavailable; spawn rejected, supervisor logs and skips.
- **Quantum** — The 10 ms time slice after which the per-core scheduler preempts.
- **Routing table** — Kernel structure mapping `EndpointId → (CoreId, Generation, Liveness)`.
- **TCB** — Trusted Computing Base. Kernel + arch + smp + init + supervisor. `registry` left the TCB via H11; `block-driver` + `fs` left via the Phase D amendment (§6.1, once `fs` gained crash-consistent recovery). DMA drivers (`xhci`/`ehci`) are in the TCB only on a machine without an IOMMU (§6.4).
- **Trusted root** — `init`, `supervisor`. Failure of either reboots the system; they are the only remaining non-restartable services. (`registry`, `block-driver`, `fs` are all restartable name/storage services now.)
- **Registry** — Restartable userspace name service: maps stable names → current capabilities so services can find and re-find each other across restarts (H11; `docs/registry.md`).
- **Service** — Userspace component with a contract, capability table, and isolated address space.
- **Contract** — `service.toml` declaring resource, capability, and placement requirements.
- **Supervisor** — User-space service holding restart authority over other services.
- **`osdev`** — Host-side CLI for building, publishing, and controlling the OS in QEMU.

---

## 25. Final Principles

> **Undefined behavior in spec becomes bugs in system.**

> **If it violates the model, it does not belong.**

> **If an identity test fails, the system is no longer this system.**

> **Identity is stable. Location is not. Movement is invisible. Death is visible.**

> **Failures are loud, never silent.**

---
---

# Appendix A: Bootloader Choice (Limine)

> **Status:** v1 commitment. This appendix documents and justifies a concrete decision; it is part of the v1 spec, not aspirational.

## A.1 Decision

The v1 bootloader is **Limine**, accessed by the kernel via the **Limine Boot Protocol**.

## A.2 What Limine Provides to the Kernel

At handoff, Limine supplies the kernel with:

- **Physical memory map** — usable, reserved, ACPI, framebuffer regions all classified.
- **Framebuffer descriptor** — base address, dimensions, pixel format. Avoids ever needing VGA text mode.
- **Kernel relocation info** — physical and virtual base addresses of where the kernel was loaded.
- **SMP topology** — APIC IDs of all available processors, used by `kernel/smp` and `arch/x86_64/ap_boot.rs`. This removes the need to probe ACPI/MADT in v1.
- **Higher-half direct map** — physical memory pre-mapped at a known high-half virtual address, available immediately.
- **Boot-time module list** — ability to load additional binaries (e.g., the initial service manifest) alongside the kernel.

## A.3 Why Limine for v1

| Reason | Detail |
|--------|--------|
| **Tight Rust integration** | The `limine` crate (crates.io) provides type-safe bindings to the boot protocol. Request structures are declared as Rust statics; responses are read back with strong typing. No hand-rolled parsing of bootloader-supplied tables. |
| **SMP topology supplied** | APIC IDs come pre-discovered. `arch/x86_64/ap_boot.rs` does not need an ACPI parser to enumerate cores. |
| **BIOS + UEFI from one bootloader** | Limine supports both firmware modes with the same protocol. The kernel does not need to care which firmware booted it. |
| **Good QEMU support** | Critical for `osdev run` (§17). The whole identity test suite runs under QEMU; the bootloader must work there reliably. |
| **Active maintenance** | Limine is currently maintained and the protocol is stable but evolving. Pin a specific protocol version in `Cargo.toml` and treat updates as deliberate. |

## A.4 Installation Story

GodspeedOS ships with Limine. Users do not install or pre-install Limine separately.

**UEFI systems:**

- The OS image places `BOOTX64.EFI` (the Limine UEFI loader) at `/EFI/BOOT/BOOTX64.EFI` on the EFI System Partition.
- The kernel binary is placed where `limine.conf` references it.
- Firmware → `BOOTX64.EFI` → Limine → kernel.

**BIOS systems:**

- The installer runs `limine bios-install` to write Limine's stage-1 to the MBR (or VBR).
- Stages 2 and 3 of Limine live on disk alongside the kernel; loaded by the stage-1.
- Firmware → MBR → Limine → kernel.

In both cases the Limine binaries are part of the OS image. The end user installs the OS; Limine arrives with it.

## A.5 Dual-Boot Support

Out of scope for v1. If pursued later, Limine supports chainloading other bootloaders, which is the cleanest path. v1 assumes a dedicated machine or VM.

## A.6 Alternatives Considered

| Option | Why Not |
|--------|---------|
| GRUB / Multiboot2 | Older protocol, clunkier Rust integration, no built-in SMP topology surface, much larger surface area to depend on. |
| Custom bootloader | A project unto itself. Distracts from the actual goal (a capability microkernel). |
| Bootboot, stivale (predecessor to Limine) | Stivale is deprecated in favor of Limine Boot Protocol. Bootboot is reasonable but has a smaller community and worse Rust tooling than Limine today. |

## A.7 Implementation Notes

- The integration code lives in `kernel/src/arch/x86_64/boot.rs`.
- The Limine protocol version used is pinned in the workspace `Cargo.toml`. Protocol version bumps are PR-reviewed.
- A short narrative of the boot handoff (Limine → kernel main) lives in `docs/bootloader.md`.

---

# Appendix B: Userspace Posture

> **Status:** Non-normative. Records the v1 stance on userspace languages and the design intent for early services. Does not amend the constitution.

## B.1 Primary Language: Rust

All v1 userspace services are written in Rust against `sdk/rust/`.

Reasons:

- The kernel is Rust; sharing a toolchain dramatically simplifies the build and CI pipeline.
- Capability handles in `sdk/rust/capability.rs` are typed Rust structures. C or other-language equivalents would need parallel SDKs.
- Contract validation in `osdev` is Rust; reusing the same JSON Schema crate end-to-end avoids drift.
- Borrow-check discipline meshes naturally with the no-shared-mutable-state invariant.

## B.2 Other Languages

The syscall ABI follows System V AMD64. Any language that can produce a freestanding ELF binary and call via that ABI can run as a service.

Plausible second-fits:

- **C** — would need a `sdk/c/` of thin syscall wrappers. Capability handles become opaque integers; the type-safety of the Rust SDK is lost.
- **Zig** — bare-metal-capable, no runtime, comparable ergonomics to C with better safety. A `sdk/zig/` is a future possibility.

Out of scope:

- **Languages with substantial runtimes** (Go, Python, JavaScript, Java) — would require porting their runtime as a GodspeedOS service. Each is a multi-month project on its own.

## B.3 Shell ≠ Unix Shell

The traditional Unix shell relies on fork, exec, file-descriptor inheritance, ambient stdin/stdout/stderr, environment-variable inheritance, and anonymous pipe sharing. None of those primitives exist in GodspeedOS.

A "shell" in this system is a capability-broker service. It:

- Holds a capability to a console service (keyboard input + display output).
- Holds the authority to ask the supervisor to spawn other services (via a cap to invoke `supervisor.spawn`).
- Reads commands and constructs spawn requests, including the explicit caps each child should receive.

This means there is no `stdin`, only an IPC endpoint to a console service. There is no `fork`, only an authenticated request to the supervisor. There is no inherited environment, only the caps the parent explicitly granted.

The full mechanics of how Unix-style scripting maps onto this model — pipes as capability-mediated endpoints, redirection as cap minting, etc. — are explored in Appendix D.

## B.4 Open Question for Later

When real userspace work begins, the first user-facing design decision is **how Unix-flavored the interface should feel** — Genode-style superficial familiarity (`ls /data` works, even though `ls` is a capability-bearing service) versus a fully fresh vocabulary. Either is defensible. Picking deliberately matters because retrofitting later is painful.

Not a v1 decision.

---

# Appendix C: Forward-Looking Vision (Non-Normative)

> **Status:** Non-normative. Records design intent for post-v1 directions. Items may be deferred indefinitely, redesigned, or rejected when their time comes. This appendix does not amend the constitution.

## C.1 `observe` — Native Top/Htop Equivalent

**Tentative timeline:** v2 candidate.

A native introspection tool, distinct from any third-party utility, that surfaces what the system is doing.

The architecture makes this easier than the equivalent on Unix. Per-service state is already structured: caps held, assigned core, memory request/limit, current allocation, IPC queue depths, generation, liveness. The supervisor already knows about every service. The kernel already tracks per-task state.

`observe` would be a service holding capabilities to query the supervisor and a kernel-provided introspection endpoint, formatting the result for display. There is no need to parse a `/proc`-style text interface — the data is already structured.

## C.2 Native Metrics Emission

**Tentative timeline:** v2 / v3 candidate.

The system should emit metrics by default, without requiring third-party agents.

Architectural intent: the kernel emits structured events (IPC volume, cap denials, scheduler statistics, restart events) on a known endpoint. This is a publication, not a feature baked into kernel logic — the kernel does not know what format consumers want. A separate metrics service holds the consuming capability and exports in whatever format is appropriate (Prometheus exposition, OpenTelemetry, statsd, custom).

Constraint: this must not pollute kernel scope. The kernel publishes; the metrics service interprets.

## C.3 Cluster Mode (Single-System-Image)

**Tentative timeline:** multi-year research direction. Not a milestone commitment for any specific version.

The ambition: multiple GodspeedOS instances, possibly identified by a shared token, joining together to act as a single system. Conceptually similar to what k8s does for containers, but at the OS layer: services on any node, IPC across nodes, single namespace.

**Why this is unusually feasible architecturally:**

- Invariant 11 ("identity is stable; location is not") is the design primitive distributed systems are built on. The routing table generalizes cleanly: `EndpointId → CoreId` becomes `EndpointId → (NodeId, CoreId)`. SMP already separated identity from execution location; cluster mode extends "location" from a core to a (node, core) pair without changing the philosophy.
- The generation-number mechanism for cross-core revocation extends to cross-node revocation.
- The absence of POSIX, fork, shared memory, and ambient authority means none of the impossible-to-distribute primitives are baked in. Linux clustering attempts (OpenSSI, MOSIX) all foundered on `mmap`, signals, and inherited file descriptors.

**Why it is still a multi-year project, not a v2 weekend:**

- Synchronous IPC does not survive network latency. Either remote IPC must be a different primitive (loses transparency), or "blocking send" must mean something different cross-node (semantic fork in the API).
- Cluster membership, leader election, and consensus are real distributed-systems problems (Raft / Paxos territory).
- Cryptographic capability transfer over a network — caps must remain unforgeable when crossing the wire.
- Cross-node TCB definition: does the supervisor on node A have authority over services on node B? If yes, a compromised supervisor compromises the cluster. If no, what coordinates restarts?
- Failure semantics expand: not just node crashes, but network partitions, split-brain, and partial failures.

**Reference points:**

- Plan 9 from Bell Labs achieved network-transparent files and processes 30+ years ago.
- Barrelfish (Microsoft Research) explored "the OS as a distributed system" via the multikernel model.
- MOSIX, OpenSSI, LOCUS — Linux/Unix attempts at SSI; useful for understanding what didn't work and why.

The architectural primitives in this constitution are unusually well-suited for revisiting these ideas. Whether and when to attempt it is open.

## C.4 Cluster Routing: Architecture and API

> Full design notes, failure semantics, flow control, ordering, registry scope, transport options, and TCB authority model live in `docs/cluster-design.md`. This section records the headline conclusions.

The routing table generalizes from `EndpointId → CoreId` to `EndpointId → (NodeId, CoreId)`. The invariant "identity is stable; location is not" already encodes this — cluster mode extends the definition of location without changing the philosophy. The generation mechanism handles cross-node service mobility identically to how it handles cross-core restarts today.

The remote IPC API uses a distinct call surface (`send_remote` with explicit timeout) rather than transparent routing. The existing constitution invariants settle this: a successful local `send` guarantees queue delivery on this machine; a successful remote `send` guarantees handoff to a transport. These are different contracts with different durability and failure obligations, and pretending otherwise is the architectural mistake transparent-clustering systems have historically made. The network boundary is visible at three layers: contract (`ipc_send_remote`), type system (`RemoteSendCap` vs `LocalSendCap`), and call site. Applications that never declare `ipc_send_remote` are entirely unaffected by cluster membership.

Three questions must be resolved before cluster mode can ship: the transport protocol (which shapes failure semantics and ordering guarantees), the registry consistency model (distributed name resolution is the largest single piece of work, comparable in scope to most of v1), and cross-node TCB authority (whether a supervisor on node A governs services on node B — the central security question for clustering).

---

# Appendix D: Shell, Scripting, and Utility Ecosystem (Non-Normative, Far Future)

> **Status:** Non-normative, far-future. Records the design intent for what shell scripting could look like on GodspeedOS, and what porting Unix-style utilities would actually require. This appendix does not amend the constitution.

## D.1 Why This Is Possible

The capability constraints in the constitution do not forbid shell scripting. They reshape what scripting *is* under the hood.

The fundamental abstraction of a shell script is "compose a sequence of commands and feed data between them." Nothing in the constitution forbids that. What changes is how composition happens.

## D.2 Walking Through Bash Primitives

| Bash primitive               | GodspeedOS mapping                                                  |
|------------------------------|---------------------------------------------------------------------|
| Sequential (`cmd1; cmd2`)    | Shell calls `supervisor.spawn(cmd1)`, waits, then spawns `cmd2`. Unchanged. |
| Variables, `if`, `while`     | Entirely shell-internal; never touch the OS. Unchanged.             |
| Pipes (`cmd1 \| cmd2`)        | Capability-mediated. See D.3.                                       |
| Redirection (`cmd > file`)   | Shell creates the file via FS, mints a `WRITE` cap, grants to `cmd`. Same UX, different mechanism. |
| Substitution (`x=$(cmd)`)    | Shell sets up `cmd` with a `SEND` cap to a shell-owned endpoint, captures the output, binds the variable. |
| Background (`cmd &`)         | Spawn without waiting. Trivial.                                     |

## D.3 The Pipe Pattern

The interesting case is the pipe, because Unix pipes rely on fork and inherited file descriptors — neither of which exists.

In GodspeedOS, `cmd1 | cmd2` becomes:

1. Shell creates a fresh IPC endpoint.
2. Shell grants `cmd1` a `SEND` cap to that endpoint, via its spawn contract.
3. Shell grants `cmd2` a `RECV` cap to the same endpoint, via its spawn contract.
4. `cmd1`'s "stdout" is "send to the cap I was granted."
5. `cmd2`'s "stdin" is "recv from the cap I was granted."

Pipes still exist. They are capability-mediated rather than fd-mediated.

The user-facing syntax can look exactly familiar:

```
ls /data | grep .txt > files.list
```

The shell does more capability brokerage than bash; the user sees the same thing.

## D.4 The Security Upside

Capability-mediated scripting is meaningfully safer than POSIX shell *by construction*.

`rm -rf /` cannot exist on GodspeedOS unless the user (via the shell) explicitly grants the `rm` service a `WRITE` cap to `/`. The shell becomes the deliberate place where authority is decided. It can refuse dangerous compositions, ask for confirmation, or apply policy.

There is no equivalent of "ambient authority leaked through fork-exec and now the wrong process deleted everything."

## D.5 Language Design — Open Question

The script language syntax does not fall out of the architecture. Three plausible directions:

| Direction              | Trade-offs                                                          |
|------------------------|---------------------------------------------------------------------|
| POSIX-flavored         | Familiar muscle memory; inherits 50 years of warts (word-splitting, quoting rules, `[` vs `[[`). |
| Modern shell           | (fish, nushell, oil-inspired) Structured data, better composition, fewer footguns. |
| Embedded language      | Shell hosts a real language (Lua, Rhai, custom DSL); scripts are programs in that language. |

Likely lean: modern-shell-inspired. POSIX-shell quirks exist for historical reasons that don't apply here.

## D.6 What "Porting" GNU Utilities Would Actually Mean

A common assumption is that supporting Unix-style utilities means "port GNU coreutils." This is misleading.

**It is not a port. It is a rewrite.** GNU coreutils is hundreds of thousands of lines of POSIX-specific C. You cannot recompile it against a new libc and expect it to work, because the underlying primitives don't exist:

- No `fork` / `exec`.
- No `errno`.
- No POSIX signals (no `SIGPIPE`, `SIGINT`, `SIGTERM` handlers).
- No inherited file descriptors.
- No environment-variable inheritance.
- No process groups, sessions, or terminal control as POSIX defines them.

Each utility has to be **rewritten** in Rust (or whatever) against the GodspeedOS SDK. Contract per utility. Capabilities for everything it touches. IPC for what it produces and consumes.

**The rewrite is genuinely simpler per-utility, however,** because the OS handles the plumbing that POSIX utilities have to handle themselves:

| GNU `cat` worries about              | GodspeedOS equivalent              |
|--------------------------------------|------------------------------------|
| `errno` translation                  | Typed `Result<>` from the SDK      |
| Signal handlers                      | None needed                        |
| Permission checks                    | Either you have the cap or you don't |
| Symlinks, special files              | FS service handles; utility doesn't see them |
| getopt edge cases                    | Rust argument-parsing crate       |
| Cross-platform abstraction layers    | Single platform                    |
| Memory safety                        | Free with Rust                     |

A `cat`-equivalent in GodspeedOS is on the order of 30 lines of Rust. GNU `cat` is hundreds of lines, mostly handling POSIX corner cases that GodspeedOS doesn't have.

## D.7 Realistic Expectations

The GNU community is heavily invested in POSIX. Existing GNU code is unlikely to be ported by its existing maintainers; the architectural mismatch is too large.

A more realistic scenario is a fresh community building a coreutils-equivalent for GodspeedOS from scratch in Rust, motivated by the architecture rather than by carrying forward existing code. The friendliness this appendix describes is friendliness *to the rewrite*, not friendliness to existing C source.

Bash compatibility is explicitly **not on offer**. A shell language that resembles bash superficially but is its own thing underneath is fine; pretending bash scripts will run unmodified would be a lie that bites users later.

## D.8 Why This Belongs in Far-Future Scope

None of this is v1, v2, or v3 work. It is what becomes possible once:

- The kernel and TCB are stable.
- A real userspace exists (logger, fs, network).
- A supervisor API for service-initiated spawn (with cap delegation) exists — currently `supervisor.spawn` is supervisor-internal.
- The userspace community has formed enough to have opinions on what shell language to design.

This appendix exists so that, when that time comes, the design intent is on record and the architectural reasoning is preserved.

---

# 26. Architectural Discipline

> The architecture survives only if the discipline survives.

The greatest long-term risk to this system is not memory corruption, race conditions, or hardware failure.

The greatest risk is gradual architectural erosion:
- hidden complexity;
- convenience abstractions;
- implicit behavior;
- speculative extensibility;
- silent fallback paths;
- and features that weaken the model in exchange for short-term ergonomics.

This section exists to preserve the architectural mindset that produced the system.

---

## 26.1 The Model Is The Product

GodspeedOS is not merely a collection of kernel features.

The value of the system is the coherence of the model:
- explicit authority;
- bounded behavior;
- visible failure;
- typed capabilities;
- restartability;
- and identity separated from execution location.

A feature that weakens the model weakens the system, even if the feature appears useful.

The architecture itself is the product.

---

## 26.2 Features Must Be Pulled Into Existence

Features are added because:
- a constitutional invariant requires them;
- an identity test requires them;
- a real operational problem requires them;
- or a demonstrated implementation limitation requires them.

Features are not added because:
- they may be useful later;
- other systems have them;
- they feel incomplete without them;
- or they might support hypothetical future flexibility.

Speculative abstraction is architectural debt.

The preferred state of an unneeded feature is:

```text
not implemented; will be implemented when a test requires it
```

Deferral is a design decision, not a weakness.

---

## 26.3 Identity Tests Define System Identity

Identity tests are not implementation tests.

They are executable constitutional invariants.

A passing identity test means:
- the implementation still matches the model;
- the guarantees in this document still hold;
- the architectural invariants still compose correctly;
- and the system is still the system this document describes.

A failing identity test is evidence of architectural drift.

If an identity test fails:
- either the implementation is wrong;
- or the constitution has changed.

There is no third category.

---

## 26.4 No Silent Complexity

Complexity must remain visible.

Reject:
- hidden retries;
- invisible caching layers;
- implicit authority escalation;
- silent transport substitution;
- transparent distributed semantics;
- automatic fallback behavior;
- convenience APIs that weaken guarantees;
- or abstractions that obscure ownership or failure.

The system should always make it possible to answer:
- where authority came from;
- where state lives;
- who owns the state;
- what failed;
- what guarantees exist;
- what the timeout boundary is;
- and what recovery obligations now exist.

If those questions become difficult to answer, the abstraction is too opaque.

---

## 26.5 Explicitness Over Magic

The system intentionally prefers explicit operations over implicit convenience.

Examples:
- explicit capability passing instead of ambient authority;
- explicit restart instead of live mutation;
- explicit remote IPC instead of transparent clustering;
- explicit reacquisition instead of silent rebinding;
- explicit queue limits instead of elastic growth;
- explicit contracts instead of inferred permissions.

This is not accidental minimalism.

It is how the system preserves mechanical honesty.

---

## 26.6 Bounded Behavior Over Optimistic Behavior

Every subsystem should have:
- bounded memory usage;
- bounded queue growth;
- bounded retry behavior;
- bounded authority;
- bounded execution scope;
- and bounded failure semantics.

Unbounded behavior eventually becomes undefined behavior under load.

If a subsystem cannot explain:
- its limits;
- saturation behavior;
- failure mode;
- and recovery strategy;

then the subsystem is incomplete.

### 26.6.1 Bounded memory means stack and arenas, not heap (settled direction)

The **default** mechanism for bounded memory is **no heap**. State lives in fixed stack arrays,
in bounded reusable arenas (a fixed region + a bump pointer, reset between operations), or in
immutable rodata — never behind a general allocator. The heap reflex is resisted by default.

This is not asceticism; it is how the bound stays *visible*. A fixed footprint can be read off
the source — the maximum a subsystem can use is right there. There is no allocator in the trusted
base to fragment, to fail *in the middle* of an operation, or to turn memory use into a runtime
mystery. And overflow fails the GodspeedOS way: loud, into a guard page, killing one service —
never a silent slide into thrashing. A heap erases each of those properties, which is why it is
the exception (declared, scoped) and not the default.

When a working set feels too big for the stack, the move is **not** to reach for a heap — it is to
**change the representation so the working set is small**:

- **stream in fixed chunks** instead of buffering the whole thing (the `edit` piece table holds
  one window of an arbitrarily large file, never the file; `read`/`write`/`copy` stream in
  `IO_CHUNK` pieces);
- **refer to data by `(offset, len)` spans** instead of copying it (piece spans; interned string
  ids in the records `Table`);
- **give a subsystem its own named, bounded arena** instead of a shared allocator (the records
  arena), reset between uses;
- **iterate with an explicit bounded stack** instead of call-stack recursion (`tree`'s walk).

A hard ceiling reached *loudly* is therefore a **feature**, not a missing heap: it says "rethink
the working set." (The `selfcheck | write` stack overflow was exactly this lesson — the fix was to
forbid the unbounded nesting, not to add a heap.)

This is **simplicity** as much as fault tolerance (§2.3). The bounded representation is usually
*also the clearer one*, because the constraint forces you to name the working set instead of
hand-waving it onto a heap. Per §26.13 the bar is **simple-and-bounded, not clever-and-cramped**:
a stack-only design that is *harder to read* than a heap one is a regression, not a win. The goal
is the right data shape — a piece table, a ring buffer, an arena — over which boring code is
small by construction.

---

## 26.7 Loud Failure Over Hidden Recovery

The system must prefer visible failure over ambiguous state.

A loud crash is preferable to silent corruption.

A rejected operation is preferable to undefined semantics.

A restart is preferable to hidden mutation.

A visible timeout is preferable to indefinite waiting.

The developer must never be forced to guess whether:
- an operation succeeded;
- a message was delivered;
- authority still exists;
- or the system silently degraded behavior.

The kernel boundary must remain semantically honest.

---

## 26.8 Identity Over Location

Services are identities, not placements.

Core assignment is an execution detail.

Node assignment is an execution detail.

The location of execution may change across restart boundaries without changing system identity.

The architecture must continue to separate:
- who a service is;
- from where a service currently executes.

Location is mutable.
Identity is stable.

---

## 26.9 Authority Must Remain Visible

Authority should always be inspectable and traceable.

A reviewer should be able to determine:
- what a service can do;
- where the authority came from;
- which capability granted it;
- and whether the authority can be revoked.

No subsystem may gain authority through:
- process ancestry;
- ambient inheritance;
- hidden globals;
- implicit trust;
- or side effects.

Authority is granted deliberately or not at all.

---

## 26.10 The Kernel Is Mechanism, Not Policy

The kernel exists to enforce invariants, not interpret intent.

The kernel provides:
- isolation;
- scheduling;
- capability enforcement;
- IPC routing;
- memory protection;
- interrupt routing;
- and bounded primitives.

The kernel does not:
- guess developer intent;
- silently reinterpret contracts;
- optimize correctness away;
- or make policy decisions on behalf of services.

Policy belongs in services.

Mechanism belongs in the kernel.

---

## 26.11 Understandability Is A Hard Requirement

The system must remain understandable by a single engineer.

The "30-minute whiteboard rule" is mandatory:
- a contributor should be able to explain the architecture,
- the capability flow,
- the restart model,
- the IPC semantics,
- and the failure model

on a whiteboard in roughly 30 minutes.

If understanding requires:
- tribal knowledge;
- hidden runtime behavior;
- undocumented interactions;
- or layered abstractions across dozens of components;

then the system has exceeded its acceptable complexity budget.

---

## 26.12 Correctness Before Performance

Performance matters.

Correctness matters more.

Predictability matters more than peak throughput.

The system intentionally accepts:
- extra copies;
- extra validation;
- explicit synchronization;
- and visible restart boundaries

when they preserve architectural clarity and bounded correctness.

Optimization is permitted only when:
- invariants remain visible;
- failure semantics remain unchanged;
- observability is preserved;
- and the resulting system is no harder to reason about.

---

## 26.13 Discipline Over Cleverness

Clever code is not an achievement if it weakens the model.

The preferred implementation is:
- boring;
- explicit;
- inspectable;
- testable;
- restartable;
- and mechanically honest.

The system should resist:
- abstraction for its own sake;
- framework-style indirection;
- meta-programming layers;
- hidden runtime behavior;
- and architecture driven by novelty.

A smaller coherent system is preferred over a larger impressive one.

---

## 26.14 Preserve The Invariants

Every contributor is responsible for preserving:
- identity over location;
- explicit authority;
- bounded behavior;
- typed capabilities;
- failure visibility;
- restartability;
- and no silent fallback.

If a proposal weakens those principles, the burden of proof is on the proposal.

The default answer to architectural uncertainty is:
- simplify;
- reduce scope;
- make the behavior explicit;
- and preserve the invariants.

---

## 26.15 Final Reminder

The system is allowed to evolve.

The model is not allowed to rot.

Every feature, abstraction, optimization, and subsystem must justify its existence against the constitution.

If the implementation becomes easier to extend but harder to reason about, the system has regressed.

Correctness is not enough.

The architecture must remain coherent.