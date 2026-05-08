# CLAUDE.md
## Capability-Based Microkernel OS

**Status:** v3.6 — SMP-Integrated Spec, Reviewed (Restart Placement Clarified)
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

```mermaid
flowchart TB
    subgraph App["Application Services (replaceable)"]
        ping
        pong
    end
    subgraph Sys["System Services"]
        logger
        block["block-driver"]
        fs
    end
    subgraph TCB["Trusted Root (non-restartable)"]
        init
        supervisor
        registry
    end
    subgraph Kernel["Kernel (mechanism, not policy)"]
        memory
        scheduler
        ipc
        capability
        syscall
        interrupts
        smp["smp / routing"]
    end
    subgraph Arch["Architecture Layer (unsafe boundary)"]
        x86["arch/x86_64"]
    end
    HW[Hardware - multi-core]

    App --> Sys
    Sys --> TCB
    TCB --> Kernel
    Kernel --> Arch
    Arch --> HW
```

### 4.2 SMP View (Per-Core)

```mermaid
flowchart LR
    subgraph K["Kernel - shared, concurrent"]
        RT["Routing Table<br/>(EndpointId → CoreId)"]
        CT["Capability Table"]
    end
    subgraph C0["Core 0"]
        RQ0["Run Queue"]
        S0["Services pinned to Core 0<br/>(e.g. init, supervisor, ping)"]
    end
    subgraph C1["Core 1"]
        RQ1["Run Queue"]
        S1["Services pinned to Core 1<br/>(e.g. pong)"]
    end
    subgraph C2["Core 2"]
        RQ2["Run Queue"]
        S2["Services pinned to Core 2"]
    end

    S0 -->|syscall| K
    S1 -->|syscall| K
    S2 -->|syscall| K
    K -.->|IPI| RQ0
    K -.->|IPI| RQ1
    K -.->|IPI| RQ2
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
    registry/              # name → endpoint resolution (TCB)
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
    unsafe-audit.md

  tests/
    qemu/
      identity/            # identity test suite (§22)
      harness/             # shared test infrastructure
      perf/                # deferred
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
| `registry`        | Without it, caps cannot be reacquired post-restart  |
| `block-driver`    | (v1 only) FS depends on it; restart loses disk state |
| `fs`              | (v1 only) Owns persistent state for the system      |

### 6.2 Failure Semantics

> **Failure of any TCB service (`init`, `supervisor`, `registry`, `block-driver`, `fs`) results in kernel panic and immediate system reboot. No automatic recovery is attempted in v1.**

> **Failure on any core that corrupts shared kernel state (capability table, routing table) results in kernel panic on all cores.**

Silent recovery of TCB state risks undefined system state. Loud failure plus clean restart is the only safe v1 option.

### 6.3 Reducing TCB Over Time

The block-driver and FS being trusted is a v1 simplification. v2 goal: only `init`, `supervisor`, `registry`, and the kernel remain non-restartable.

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

The generation check is one atomic comparison. It is the v1 mechanism for cross-core revocation: bumping the generation on one core makes every cap on every other core stale, with no synchronous notification required.

### 7.6 Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Created: kernel mints with current gen
    Created --> Held: inserted into cap table
    Held --> Used: syscall + rights + gen check
    Used --> Held: action complete
    Held --> Transferred: send w/ GRANT
    Transferred --> Held: in receiver's table
    Held --> Stale: kernel bumps resource gen
    Stale --> [*]: next use returns CapRevoked / EndpointDead
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

```mermaid
sequenceDiagram
    participant A as Service A (Core 0)
    participant K as Kernel
    participant B as Service B (Core 1)

    A->>K: send(endpoint_cap, message)
    K->>K: validate cap (rights + generation)
    alt cap invalid
        K-->>A: CapNotHeld / CapInsufficientRights
    else generation mismatch (revoked)
        K-->>A: CapRevoked
    else generation mismatch (endpoint dead)
        K-->>A: EndpointDead
    else queue full (blocking send)
        K->>K: block A in routing table
        Note over A,B: A waits for queue space on Core 1
    else queue has space
        K->>K: copy message into B's queue (on Core 1)
        K-->>A: Ok
        K->>B: IPI Core 1, wake B if blocked on recv
    end
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

```mermaid
flowchart TD
    Start[Service memory action]
    Start --> Type{Action type}
    Type -->|alloc within limit| Ok[Memory returned]
    Type -->|alloc beyond limit| AD[AllocDenied returned]
    Type -->|access outside mapped region| Fault[Page fault]
    AD --> Cont[Service continues, may degrade]
    Fault --> Kill[Service killed]
    Kill --> Sup[Supervisor decides restart]
```

### 10.4 Two Failure Modes

`AllocDenied` is **recoverable**. The service knows it asked for too much and can degrade.

A protection violation is **unrecoverable**. The service is in undefined state and the only safe response is termination.

### 10.5 TLB Coherence

When a page is unmapped (service killed, memory reclaimed), the kernel issues a TLB shootdown via IPI to every core. Operations resume only after acknowledgment from all cores. v1 minimizes unmap frequency by reclaiming memory only at service death.

---

## 11. Bootstrap Sequence

### 11.1 Sequence

```mermaid
sequenceDiagram
    participant BL as Bootloader
    participant K as Kernel (BSP)
    participant APs as APs (Cores 1..N)
    participant I as init (Core 0)
    participant S as supervisor (Core 0)
    participant R as registry (Core 0)
    participant Svc as Other services

    BL->>K: load image, jump (BSP only)
    K->>K: paging, IDT, GDT
    K->>K: frame allocator, cap subsystem
    K->>APs: start APs (real-mode trampoline → long mode)
    APs->>APs: enter idle scheduler loop
    K->>K: mark all available cores ready
    K->>I: spawn init on Core 0
    I->>S: spawn supervisor on Core 0
    I->>R: spawn registry on Core 0
    I->>I: spawn logger on Core 0
    S->>S: read boot manifest
    S->>Svc: spawn services per placement policy (§9.2)
    Note over BL,Svc: System reaches multi-core steady state
```

### 11.2 BSP and APs

- **BSP (Bootstrap Processor)** — the first core to execute kernel code. Responsible for kernel init and bringing APs online.
- **APs (Application Processors)** — secondary cores. Brought up via real-mode trampoline (`arch/x86_64/ap_boot.rs`), then jump to long-mode kernel code, then enter idle.

### 11.3 Failure During Bootstrap

| Failing component | Effect                                |
|-------------------|---------------------------------------|
| Bootloader        | Hardware reset                         |
| Kernel BSP init   | Kernel panic, halt                     |
| AP startup        | Kernel logs warning, continues with available cores; if zero APs come up, system runs as single-core |
| init spawn        | Kernel panic, halt                     |
| supervisor spawn  | Kernel panic, halt (TCB)               |
| registry spawn    | Kernel panic, halt (TCB)               |
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

```mermaid
sequenceDiagram
    participant HW as Hardware
    participant K as Kernel IDT
    participant Route as Interrupt Router
    participant Drv as Driver Service

    HW->>K: IRQ N (on some core)
    K->>Route: dispatch by IRQ number
    Route->>Drv: IPC message to interrupt endpoint
    Drv->>Drv: recv() returns interrupt event
    Drv->>HW: handle device via MMIO cap
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

### 14.2 Restart and Cap Rebinding (Possibly Cross-Core)

```mermaid
sequenceDiagram
    participant A as Service A (Core 0)
    participant Sup as Supervisor (Core 0)
    participant K as Kernel
    participant B_old as Service B gen=4 (Core 1)
    participant B_new as Service B gen=5 (Core 2)
    participant R as Registry

    Note over A,B_old: Steady state — A holds cap to B (gen=4)
    Sup->>K: kill(B)
    K->>K: close B's endpoints, drop queued
    K->>K: bump B's resource generation (4 → 5)
    K->>K: reclaim B's memory (TLB shootdown)
    A->>K: send via gen=4 cap
    K-->>A: EndpointDead (gen mismatch)
    Sup->>K: spawn B on Core 2 (round-robin or override)
    K->>B_new: start, cap table populated with gen=5
    B_new->>R: register endpoint
    A->>R: lookup("B")
    R-->>A: fresh cap (gen=5, points to Core 2)
    A->>K: send via fresh cap
    K-->>A: Ok (routes to Core 2)
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

The filesystem service is the externalization mechanism for everyone else and cannot persist *to itself*. Resolution: the block driver holds a direct hardware capability and stores fs metadata. In v1, both block-driver and fs are non-restartable. v2 will give fs transactional metadata recovery.

Stateless services (logger in v1, ping, pong) restart trivially.

---

## 16. Update Model

```mermaid
flowchart LR
    Pkg[Update package] --> Sig{Signature valid?}
    Sig -->|no| Reject[Reject]
    Sig -->|yes| Schema{Contract valid?}
    Schema -->|no| Reject
    Schema -->|yes| Policy{Policy allows?}
    Policy -->|no| Reject
    Policy -->|yes| Restart[Supervisor restarts service with new binary]
```

Push vs pull is irrelevant — verification is the security property. Live code update is permanently rejected (§2.5); the only update mechanism is restart-with-new-binary.

---

## 17. Developer Workflow

```bash
osdev new <service-name>            # scaffold
osdev build                         # build kernel + services
osdev run --smp <N>                 # boot in QEMU with N cores
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

### 18.2 Forbidden

All userspace services, `sdk/`, `osdev/`, all kernel code outside the four layers above.

### 18.3 Documentation

Every `unsafe` block carries a SAFETY comment:

```rust
// SAFETY: <argument for why this is sound>
unsafe { ... }
```

A PR with an unsafe block lacking a SAFETY comment is rejected without review.

### 18.4 Audit Trail

`docs/unsafe-audit.md` lists every unsafe block. CI checks the file matches source.

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

| Category   | Purpose                                  | Status     |
|------------|------------------------------------------|------------|
| Identity   | Pin constitutional decisions             | **§22**    |
| Property   | Invariants under random inputs           | Deferred   |
| Fuzz       | Crash resistance on malformed inputs     | Deferred   |
| Performance| Benchmarks for IPC and syscall paths     | Deferred   |

Identity tests live in `tests/qemu/identity/`. The harness boots the OS with a configurable `-smp N` value; multi-core tests assert on N ≥ 2.

### 22.3 Test Harness

```mermaid
flowchart TD
    A[osdev test identity] --> B[Build kernel + test service]
    B --> C[Boot OS in QEMU with -smp N]
    C --> D[Test service runs scenario]
    D --> E[Harness reads serial console]
    E --> F{Match expected output?}
    F -->|yes| Pass[PASS]
    F -->|no| Fail[FAIL with diff]
    C --> G{Timeout 30s?}
    G -->|yes| Fail
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

If any cell becomes obsolete, the corresponding spec section is being changed and the change requires a CLAUDE.md amendment.

---

## 23. First Milestone

### 23.1 Goal

```
Boot the OS multi-core.
Start two services (ping on core 0, pong on core 1).
Send a message between them via cross-core IPC.
Kill pong.
The supervisor restarts it (possibly on a different core).
The system continues running.
```

### 23.2 Acceptance Criteria

1. `osdev run --smp 4` boots the OS with 4 cores; init, supervisor, registry, logger, ping, and pong reach steady state.
2. ping placed on core 0; pong placed on core 1.
3. `osdev logs ping` shows ping sending a message every second.
4. `osdev logs pong` shows pong receiving each message (cross-core IPC).
5. `osdev restart pong --core 2` kills pong on core 1 and respawns it on core 2.
6. ping observes `EndpointDead` and reacquires via the registry; the new cap routes to core 2.
7. After reacquisition, ping and pong continue communicating across the new core boundary.
8. The kernel does not panic on any core.
9. **All ten identity tests in §22 pass.**

### 23.3 Out of Scope for v1

Filesystem persistence beyond the trusted block driver, network stack, work-stealing scheduler, service migration, zero-copy IPC, live code updates, restartable block driver / fs, update model in production mode, core hotplug, per-endpoint queue depth in contract.

---

## 24. Glossary

- **AP** — Application Processor. Any core other than the BSP.
- **BSP** — Bootstrap Processor. The first core to execute kernel code.
- **Capability** — Unforgeable token: ResourceId + Rights + Generation.
- **Endpoint** — IPC destination owned by a service. Bounded queue, depth 16 in v1.
- **Generation** — Monotonic counter on resources; mismatch on cap use indicates the resource was destroyed or replaced.
- **Grant** — The right to transfer a capability via IPC.
- **Identity test** — A test that pins a constitutional decision (§22).
- **IPI** — Inter-Processor Interrupt. Mechanism for a core to wake another core.
- **Placement** — Strict assignment of a service to a specific core at spawn time. Re-evaluated from scratch on every spawn, including post-restart spawns; the previous core is not remembered.
- **PlacementInvalid** — Error returned when a contracted core is unavailable; spawn rejected, supervisor logs and skips.
- **Quantum** — The 10 ms time slice after which the per-core scheduler preempts.
- **Routing table** — Kernel structure mapping `EndpointId → (CoreId, Generation, Liveness)`.
- **TCB** — Trusted Computing Base. Kernel + arch + smp + init + supervisor + registry (+ block-driver, fs in v1).
- **Trusted root** — `init`, `supervisor`, `registry`. Failure of any reboots the system.
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