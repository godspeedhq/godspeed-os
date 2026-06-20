# Design Spec: Move Naming Out of the Kernel

> **Status:** Direction **signed off (2026-06-20)** — implementation proceeds in phases (§5). The
> §3.5 open question is **DECIDED: retire `registry`** (the supervisor is the sole name authority).
> Still non-normative as a document: the constitution amendments (§6) land *with* their phases. The
> spec wins on any conflict; this doc trails it.
>
> **Author intent (2026-06-20):** the kernel currently performs a *policy* job — resolving service
> *names* to *endpoints* — which §26.10 says belongs in a service. Pull it out so the kernel is pure
> mechanism, the supervisor owns naming, and there is no "the kernel already resolves names"
> precedent for future scope creep.

---

## 1. Motivation

### 1.1 The kernel does naming policy today

Two kernel facilities resolve **names → endpoints**:

- **`kernel/src/ipc/names.rs`** — a `SpinLock<[NameEntry; 128]>` mapping service name → `EndpointId`.
  Populated at spawn (`task/mod.rs`, `names::register(name, ep_id)`), read to wire each service's
  declared `send_peers` SEND caps (`task/mod.rs`, `names::lookup(peer_name)`).
- **Syscall 10 `AcquireSendCap(name, include_grant)`** (`syscall/dispatch.rs`) — **ungated**: any task
  may mint a SEND cap to *any* registered name. Exposed in the SDK as `reacquire_cap` /
  `acquire_send_cap`.

Resolving *another service's* endpoint by name, and minting a cap to it, is **policy** — a decision
about who may talk to whom. Per §26.10 ("the kernel is mechanism, not policy") and §4.4 (kernel
anti-scope), that belongs in a service, not the kernel.

### 1.2 The scope-creep precedent

The concrete danger (the author's argument): once the kernel resolves names, every future feature can
justify itself with *"the kernel already does the registry, so my thing fits too."* That is how a
microkernel rots. Removing naming removes the precedent.

### 1.3 The registry-bootstrap bug made it visible

`chaos kill-storm` exposed that a client could not reacquire its `registry` cap after the registry
restarted — you cannot look the namer up *in* the namer (the bootstrap chicken-and-egg, see
`project_registry_bootstrap`). The shipped fix (merged `ea99322`) is a deliberate **stopgap**: in
`registry_lookup`, on a dead registry cap, fall back to the kernel name table via
`reacquire_cap("registry")` (syscall 10) and retry. That stopgap re-embraces the very kernel path this
spec deletes. **This work removes the stopgap** by giving clients a real, non-kernel bootstrap anchor.

---

## 2. The line: what stays vs what leaves

The single most important clarification — there are **three** distinct "naming" things in the kernel,
and only one leaves:

| Kernel facility | Kind | Verdict |
|---|---|---|
| **Routing table** (`EndpointId → core/gen/liveness`) | mechanism — opaque ids, the kernel routes messages | **STAYS** |
| **Cap mint / validate / install**, the spawn syscall, IPC enqueue/IPI | mechanism | **STAYS** |
| **Task labels** ("this task is `fs`") for death notices, `observe`, logs | mechanism — a task's own identity, not third-party resolution | **STAYS** |
| **name → ELF + `ServiceConfig`** for spawning embedded services (`service_config_by_name`) | v1 *packaging* reality (ELFs are `include_bytes!`'d into the kernel image) — a **separate concern** | **STAYS (out of scope, see §7)** |
| **name → EndpointId resolution for IPC send-peers** (`ipc::names`) | **policy** — who may reach whom | **LEAVES** |
| **Syscall 10 `AcquireSendCap`** (ungated mint-by-name) | **policy** + ambient surface | **LEAVES** |
| **Spawn-time send-peer name wiring** (the `names::lookup` loop in `spawn_service_with_config`) | **policy** | **LEAVES** |

> **Key distinction.** The kernel keeps *task labels* (it may report "the task you named `fs` died" —
> reporting the lifecycle of *its own* tasks, by the label the supervisor gave at spawn). It loses
> *name resolution* (handing a third party a cap to a service it names). The first is mechanism; the
> second is policy. Routing stays opaque: the kernel routes `EndpointId`s and never needs a name.

---

## 3. Target architecture

### 3.1 The bootstrap principle

A name service cannot bootstrap from itself — resolving a name needs *something* already reachable.
The only things that never die in this system are the **kernel** and the **TCB** (`init` +
`supervisor`). The supervisor is the natural anchor: it is already non-restartable, already the
restart authority, and — in this design — already holds a cap to every service it spawned. So:

> **The supervisor is the name authority and the bootstrap anchor.** Every service holds a stable
> cap to the supervisor (wired at its own spawn). Naming and reacquisition flow through it.

### 3.2 New spawn protocol (kernel = mechanism)

The spawn syscall changes from *"resolve names, wire caps"* to *"install the caps I hand you, and give
me back a cap to the new endpoint."*

```
spawn(name, core, install: [(peer_label, cap_slot)]) -> (task_id, endpoint_cap_slot)
```

- **Input caps.** For each `(peer_label, cap_slot)`, the kernel validates the caller holds
  `cap_slot` **with GRANT**, derives a copy into the new task's cap table, and records
  `peer_label → child_slot` in the child's send-peer metadata — so the child's
  `ctx.capability(peer_label)` / `find_send_slot(peer_label)` resolves exactly as today. The kernel
  never consults a name table; it installs what it is handed.
- **Returned endpoint cap.** If the new task has a recv endpoint, the kernel mints a `SEND|GRANT` cap
  to it and inserts it into the **caller's** (supervisor's) table, returning the slot. This is the
  supervisor's handle to wire that service into future dependents.
- **Still kernel-minted, still mechanism.** The kernel continues to mint caps for resources *it owns*
  — memory, MMIO/DMA/IRQ for drivers (per contract), the endpoint it just created, delegated
  resource caps (`RESOURCE_MINT`, §7.10). What it stops doing is *resolving one service's name to
  another service's endpoint.*

### 3.3 The supervisor as name authority

The supervisor already spawns every service in dependency order (`pong` before `ping`, `block-driver`
before `fs`, …). In the new model it **collects each child's returned endpoint cap into a userspace
`name → cap` map**, and wires dependents from that map:

```
ep_pong = spawn("pong", 1, [])                    # leaf: no peers
ep_ping = spawn("ping", 0, [("pong", ep_pong)])   # ping gets a SEND cap to pong, by being handed it
```

The map lives in the supervisor (TCB userspace), built by construction. The contract's `send_peers`
becomes a **requirement the supervisor reads and fulfils** (it must already hold a cap to each named
peer — enforced by spawn order), preserving §13.3 ("the service declares what it needs; the OS decides
whether to grant it"). A contracted peer the supervisor lacks is a **loud spawn failure**, not a
silent skip.

### 3.4 Reacquisition (deletes the stopgap)

A client whose cap to `fs` (or any service) goes stale after a restart **asks the supervisor** for a
fresh one, over its stable supervisor cap:

```
client: reacquire("fs")  ──▶  supervisor
supervisor: derive a SEND copy of its held fs cap, grant it back   (DeriveCap + SendWithCap, §7)
client: cache it, retry
```

This is *explicit* reacquisition (§14.3 / §26.5 — no silent rebind), anchored on the one thing that
never dies. It works for **every** name including the registry's role, because the anchor (supervisor)
is reached by a bootstrap cap, not by a lookup. The `registry_lookup` syscall-10 fallback is removed.

### 3.5 Fate of the `registry` service — **DECIDED (2026-06-20): retire it**

Today the userspace `registry` (H11) is a separate restartable name service. In this design the
supervisor *is* the name authority (it holds the authoritative `name → cap` map by construction), so
the registry's job collapses into the supervisor:

- **Recommended:** **retire `registry`.** The supervisor serves `register`/`lookup`/`reacquire`
  directly. One name authority, no separate service, **and the bootstrap chicken-and-egg disappears**
  (there is no registry cap to reacquire — only the supervisor cap, which never goes stale). Net
  fewer moving parts (§26.13). The H11 "registry is restartable" achievement becomes moot, not lost —
  there is simply no registry to restart; the supervisor was always non-restartable TCB.
- **Alternative:** keep `registry` as a restartable **query front-end** seeded by the supervisor, for
  high-volume dynamic discovery, so the supervisor isn't on every lookup path. Clients still reacquire
  the *registry* cap from the supervisor (bootstrap). This keeps the supervisor lean but keeps a
  service and a cache-coherence concern.

**Decision (signed off 2026-06-20): retire `registry`.** The supervisor is the sole name authority;
the bootstrap chicken-and-egg disappears (no registry cap to reacquire — only the never-stale
supervisor cap). The alternative (front-end) is recorded above for history. `init` + `supervisor`
remain the only non-restartable services, as intended.

### 3.6 Death notifications

Unchanged in spirit. The kernel notifies the supervisor when a restartable service dies. It may report
by **task label** (kept) or by **task_id** (the supervisor holds `task_id → name` from spawn). Either
is mechanism — the kernel reporting the lifecycle of its own tasks — not name resolution. No change
required beyond what the new spawn return already gives the supervisor.

---

## 4. What the kernel deletes

- `kernel/src/ipc/names.rs` (the whole module) — *after* the migration.
- Syscall 10 `AcquireSendCap` + handler `handle_acquire_send_cap`.
- The `names::lookup` send-peer wiring loop in `spawn_service_with_config`.
- `names::register` calls at spawn.
- SDK `reacquire_cap` / `acquire_send_cap` (syscall-10 wrappers).
- **InspectKernel query 2** ("endpoint generation by name") rides on `ipc::names::lookup` — used by a
  test/introspection path. It must move to a name-free form (e.g. generation by `EndpointId`, or via
  the supervisor) or be removed. Flagged as a migration sub-task, not a blocker.

The kernel keeps: routing, cap machinery, the spawn syscall (new shape), `service_config_by_name`
(ELF lookup — §7), task labels, MMIO/DMA/IRQ + endpoint + delegated-resource minting.

---

## 5. Migration plan (incremental, always-bootable)

The boot/spawn path is the most load-bearing code in the system and is currently hardware-proven
(selfcheck 185/0). **Do not big-bang it.** Each phase keeps the full suite green
(identity 23/23, files 137/0, shell 67/0, script 4/0, and `selfcheck` green) before the next.

| Phase | Change | Done when |
|---|---|---|
| **0** | Add the new spawn syscall shape (accept `install` caps, return endpoint cap) **alongside** the existing name-wiring path. `ipc::names` untouched. | New syscall works in a unit/probe; old path unchanged; suite green. |
| **1** | Supervisor builds its `name → cap` map by collecting returned endpoint caps (shadow — not yet used to wire). | Supervisor logs it holds a cap for every spawned service; suite green. |
| **2** | Flip **one leaf** service (e.g. `pong`) to be wired by the supervisor (caps passed in) instead of kernel name-resolution. | That service boots + does IPC via supervisor-passed caps; suite green. |
| **3** | Flip **all** services (incl. `fs`/`block-driver`/`shell` dependency chains). Kernel spawn-time name wiring becomes dead code. | Full boot + IPC with zero `names::lookup` calls on the spawn path; suite green. |
| **4** | Move reacquisition to the supervisor; **remove the `registry_lookup` syscall-10 stopgap**. Re-run the chaos double-storm regression (it must still pass via the supervisor path). | `chaos kill-storm registry N` → working `ls`, now through the supervisor; suite green. |
| **5** | Retire `registry` (or convert to front-end per §3.5); **delete `ipc::names` + syscall 10** + SDK wrappers; resolve query 2. Update §22 Test 11 to pin client-resolution-after-restart through the supervisor. | No `ipc::names`, no syscall 10 in the tree; suite green; audit clean. |

Roll-back is per-phase: each phase is a mergeable, green increment, so a regression reverts one phase,
not the program.

---

## 6. Constitutional amendments required

To be drafted into `CLAUDE.md` at adoption (each with a commit rationale, §21):

- **§4.4 (Kernel Anti-Scope):** add that **name → endpoint resolution is not in the kernel** — the
  supervisor owns naming and cap distribution. The kernel routes opaque `EndpointId`s and installs
  caps it is handed; it does not resolve names for third parties.
- **§11 (Bootstrap):** document the new spawn protocol (kernel returns an endpoint cap and installs
  handed caps; the supervisor wires dependents from its `name → cap` map in dependency order).
- **§26.10 (Mechanism, not policy):** record this as the worked example.
- **§6 (TCB) note:** the supervisor's role is clarified to include name authority + reacquisition
  broker (it was already TCB and the spawn authority — no new TCB member). If `registry` is retired,
  update §6.1/§6.2, the Glossary, and `docs/registry.md`.
- **§13 (Contracts) note:** `send_peers` is a *requirement the supervisor fulfils*, not something the
  kernel resolves — sharpening §13.3 (request, not permission).
- **§22 Test 11:** extend to pin "a pre-existing client resolves a name after a service restart"
  through the supervisor path (the property the stopgap's regression currently pins via the files
  test).

---

## 7. Out of scope (and why)

- **name → ELF resolution for spawn (`service_config_by_name`).** The kernel spawns embedded services
  by name because their ELFs are `include_bytes!`'d into the kernel image (a v1 packaging reality).
  Moving *that* out means the supervisor carries/loads the binaries (from disk — the Prime/loader
  story, `docs/prime.md`). That is a deeper, separate change; this spec deliberately stops at IPC
  name resolution. The two are independent: the new spawn protocol works whether the ELF is kernel-
  embedded or supplied by the caller.
- **Multi-node / cluster naming** (Appendix C.4) — unaffected; the supervisor-as-name-authority model
  generalizes cleanly but is far-future.

---

## 8. Risks and open questions

1. **Supervisor endpoint load.** If `registry` is retired, every reacquisition (and any
   `register`/`lookup`) hits the supervisor's endpoint, which also carries kernel death notices.
   Replies **must** be request/response (no blocking send back into a client), so a slow client can't
   wedge the restart loop (§8.9). Volume in v1 is tiny (reacquisition is rare); the front-end
   alternative (§3.5) exists if it ever isn't.
2. **Spawn syscall ABI.** Passing a variable-length `install` list through the register-based syscall
   ABI needs a small in-memory descriptor (like `handle_spawn_pipe` already does for its sink string).
   Bounded, fixed-max `install` entries (§26.6).
3. **Cap-table growth on the supervisor.** It now holds an endpoint cap per service. Bounded by the
   service count; well within the cap table. Worth a number in Phase 1.
4. **Query 2 consumers.** Confirm exactly who reads "endpoint generation by name" before removing it
   (Phase 5) so a test doesn't silently lose coverage.
5. **Retire vs front-end.** The one genuinely open design choice (§3.5) — wants a decision at sign-off.

---

## 9. Test strategy

No new test *categories* — the existing suite is the safety net, run green at every phase (§5). Plus:

- **Phase 2/3:** boot + cross-core IPC (the §23 ping/pong demo) with supervisor-passed caps — proves
  wiring-by-cap matches wiring-by-name.
- **Phase 4:** the chaos double-storm regression (`project_registry_bootstrap`) must pass through the
  supervisor reacquisition path, with the syscall-10 stopgap gone.
- **Phase 5:** `selfcheck` green on hardware; `docs/unsafe-audit.md` unchanged (this is a logic move,
  not new `unsafe`); grep proves `ipc::names` / syscall 10 are gone.

---

## 10. Summary

The kernel stops resolving service names to endpoints — pure mechanism, no policy, no ambient
mint-by-name, no scope-creep precedent. The supervisor (already TCB, already the spawn authority)
becomes the name authority by construction: spawn returns endpoint caps, the supervisor wires
dependents and brokers reacquisition. The registry service most likely retires, taking the bootstrap
chicken-and-egg with it. The change is large and load-bearing, so it ships as an incremental,
always-bootable migration with the full suite green at every step — never a big-bang on the boot path.
