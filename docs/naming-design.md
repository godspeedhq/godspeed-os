# Design Spec: Move Naming Out of the Kernel

> **Status:** Direction **signed off (2026-06-20)**; Phases 0a–3c **built + merged** (the supervisor
> now wires every real service from a `name → cap` map at boot **and** on restart - zero kernel name
> resolution for them). **End-state revised 2026-06-21 to Path C (§3.7)** - supersedes the original
> §3.5 "retire registry, supervisor = sole namer." The constitution amendments (§6) land *with* their
> phases. The spec wins on any conflict; this doc trails it.
>
> **Author intent (2026-06-20):** the kernel currently performs a *policy* job - resolving service
> *names* to *endpoints* - which §26.10 says belongs in a service. Pull it out so the kernel is pure
> mechanism, the supervisor owns naming, and there is no "the kernel already resolves names"
> precedent for future scope creep.
>
> **Revised goal (2026-06-21, Path C, §3.7):** the *deeper* prize is shrinking the **unkillable set to
> its theoretical minimum - just the kernel** (§6.3). Reaching that requires a recovery anchor that
> *cannot* die, which only the kernel is. So the kernel keeps a **minimal name→endpoint recovery
> directory** (one bounded exception), the registry service retires into it, **init is removed**, and
> the **supervisor becomes restartable** (kernel respawns it; it recovers from the directory). This
> *softens* §26.10 (a thin naming facility stays in the kernel) to *better serve* §6.3 (unkillable =
> `{kernel}`). §§1–3.6 below record the original reasoning that got us here; §3.7 is the chosen end.

---

## 1. Motivation

### 1.1 The kernel does naming policy today

Two kernel facilities resolve **names → endpoints**:

- **`kernel/src/ipc/names.rs`** - a `SpinLock<[NameEntry; 128]>` mapping service name → `EndpointId`.
  Populated at spawn (`task/mod.rs`, `names::register(name, ep_id)`), read to wire each service's
  declared `send_peers` SEND caps (`task/mod.rs`, `names::lookup(peer_name)`).
- **Syscall 10 `AcquireSendCap(name, include_grant)`** (`syscall/dispatch.rs`) - **ungated**: any task
  may mint a SEND cap to *any* registered name. Exposed in the SDK as `reacquire_cap` /
  `acquire_send_cap`.

Resolving *another service's* endpoint by name, and minting a cap to it, is **policy** - a decision
about who may talk to whom. Per §26.10 ("the kernel is mechanism, not policy") and §4.4 (kernel
anti-scope), that belongs in a service, not the kernel.

### 1.2 The scope-creep precedent

The concrete danger (the author's argument): once the kernel resolves names, every future feature can
justify itself with *"the kernel already does the registry, so my thing fits too."* That is how a
microkernel rots. Removing naming removes the precedent.

### 1.3 The registry-bootstrap bug made it visible

`chaos kill-storm` exposed that a client could not reacquire its `registry` cap after the registry
restarted - you cannot look the namer up *in* the namer (the bootstrap chicken-and-egg, see
`project_registry_bootstrap`). The shipped fix (merged `ea99322`) is a deliberate **stopgap**: in
`registry_lookup`, on a dead registry cap, fall back to the kernel name table via
`reacquire_cap("registry")` (syscall 10) and retry. That stopgap re-embraces the very kernel path this
spec deletes. **This work removes the stopgap** by giving clients a real, non-kernel bootstrap anchor.

---

## 2. The line: what stays vs what leaves

The single most important clarification - there are **three** distinct "naming" things in the kernel,
and only one leaves:

| Kernel facility | Kind | Verdict |
|---|---|---|
| **Routing table** (`EndpointId → core/gen/liveness`) | mechanism - opaque ids, the kernel routes messages | **STAYS** |
| **Cap mint / validate / install**, the spawn syscall, IPC enqueue/IPI | mechanism | **STAYS** |
| **Task labels** ("this task is `fs`") for death notices, `observe`, logs | mechanism - a task's own identity, not third-party resolution | **STAYS** |
| **name → ELF + `ServiceConfig`** for spawning embedded services (`service_config_by_name`) | v1 *packaging* reality (ELFs are `include_bytes!`'d into the kernel image) - a **separate concern** | **STAYS (out of scope, see §7)** |
| **name → EndpointId resolution for IPC send-peers** (`ipc::names`) | **policy** - who may reach whom | **LEAVES** |
| **Syscall 10 `AcquireSendCap`** (ungated mint-by-name) | **policy** + ambient surface | **LEAVES** |
| **Spawn-time send-peer name wiring** (the `names::lookup` loop in `spawn_service_with_config`) | **policy** | **LEAVES** |

> **Key distinction.** The kernel keeps *task labels* (it may report "the task you named `fs` died" -
> reporting the lifecycle of *its own* tasks, by the label the supervisor gave at spawn). It loses
> *name resolution* (handing a third party a cap to a service it names). The first is mechanism; the
> second is policy. Routing stays opaque: the kernel routes `EndpointId`s and never needs a name.

---

## 3. Target architecture

### 3.1 The bootstrap principle

A name service cannot bootstrap from itself - resolving a name needs *something* already reachable.
The only things that never die in this system are the **kernel** and the **TCB** (`init` +
`supervisor`). The supervisor is the natural anchor: it is already non-restartable, already the
restart authority, and - in this design - already holds a cap to every service it spawned. So:

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
  `peer_label → child_slot` in the child's send-peer metadata - so the child's
  `ctx.capability(peer_label)` / `find_send_slot(peer_label)` resolves exactly as today. The kernel
  never consults a name table; it installs what it is handed.
- **Returned endpoint cap.** If the new task has a recv endpoint, the kernel mints a `SEND|GRANT` cap
  to it and inserts it into the **caller's** (supervisor's) table, returning the slot. This is the
  supervisor's handle to wire that service into future dependents.
- **Still kernel-minted, still mechanism.** The kernel continues to mint caps for resources *it owns*
  - memory, MMIO/DMA/IRQ for drivers (per contract), the endpoint it just created, delegated
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
peer - enforced by spawn order), preserving §13.3 ("the service declares what it needs; the OS decides
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

This is *explicit* reacquisition (§14.3 / §26.5 - no silent rebind), anchored on the one thing that
never dies. It works for **every** name including the registry's role, because the anchor (supervisor)
is reached by a bootstrap cap, not by a lookup. The `registry_lookup` syscall-10 fallback is removed.

### 3.5 Fate of the `registry` service - **DECIDED (2026-06-20): retire it**

Today the userspace `registry` (H11) is a separate restartable name service. In this design the
supervisor *is* the name authority (it holds the authoritative `name → cap` map by construction), so
the registry's job collapses into the supervisor:

- **Recommended:** **retire `registry`.** The supervisor serves `register`/`lookup`/`reacquire`
  directly. One name authority, no separate service, **and the bootstrap chicken-and-egg disappears**
  (there is no registry cap to reacquire - only the supervisor cap, which never goes stale). Net
  fewer moving parts (§26.13). The H11 "registry is restartable" achievement becomes moot, not lost -
  there is simply no registry to restart; the supervisor was always non-restartable TCB.
- **Alternative:** keep `registry` as a restartable **query front-end** seeded by the supervisor, for
  high-volume dynamic discovery, so the supervisor isn't on every lookup path. Clients still reacquire
  the *registry* cap from the supervisor (bootstrap). This keeps the supervisor lean but keeps a
  service and a cache-coherence concern.

**Decision (signed off 2026-06-20): retire `registry`** - _**superseded 2026-06-21 by Path C (§3.7).**_
The original decision retired the registry into the supervisor, leaving `{kernel, supervisor}`
unkillable. Path C keeps the registry's *recovery* role as a minimal **kernel** directory instead, so
the supervisor can also be restarted and the unkillable set shrinks to `{kernel}`. The registry
*service* still retires; what changes is *where its recovery state lives* (kernel directory, not the
supervisor). Read §3.7 for the chosen end-state; §3.1–§3.6 are the reasoning that led there.

### 3.6 Death notifications

Unchanged in spirit. The kernel notifies the supervisor when a restartable service dies. It may report
by **task label** (kept) or by **task_id** (the supervisor holds `task_id → name` from spawn). Either
is mechanism - the kernel reporting the lifecycle of its own tasks - not name resolution. No change
required beyond what the new spawn return already gives the supervisor.

### 3.7 Path C - kernel keeps a minimal recovery directory (the chosen end-state, 2026-06-21)

§3.1–§3.6 make the **supervisor** the bootstrap anchor. That works for *wiring* (done, Phases 0a–3c),
but it leaves the supervisor **unkillable**, and trying to fix *that* exposes a trap.

#### 3.7.1 The trap, and why only the kernel escapes it

To make the supervisor restartable it must **recover its state** after a respawn - above all, its caps
to the already-running services. But **capabilities are not data**: a cap is an unforgeable kernel
token (`ResourceId + Rights + Generation`, §7.3), and a service only ever holds opaque handles. You
**cannot serialise a cap to disk and load it back** - that is cap forgery. So persistence can save
*intent* ("`fs` should be running") but never the *caps*; those must be **re-minted from a live
source**. And the supervisor cannot re-spawn the services to re-derive them (they are alive - the
singleton guard rejects it).

What live source? A separate **registry** could hold the caps and survive the supervisor - but the
registry, if it dies, is respawned *by the supervisor*. Supervisor needs registry to recover; registry
needs supervisor to recover. **Mutual dependency ⇒ both must stay up ⇒ both unkillable.** The trap.

There is exactly one escape: the recovery anchor must be the one thing that **fundamentally cannot
die - the kernel** (a kernel fault *is* the machine faulting; nothing beneath it could respawn it).
So the kernel holds the minimal recovery state. **Everything above the kernel then becomes
restartable, and the unkillable set is `{kernel}` - the theoretical minimum (§6.3).**

#### 3.7.2 The one exception: a *minimal* directory, not the registry

The kernel keeps only what recovery needs - and **it already has it**:

- **`name → EndpointId`** (`ipc::names`, populated at spawn - kept), and
- **mint a SEND cap by name** (`AcquireSendCap`, syscall 10 - kept, but now **GATED** behind a
  recovery capability, closing the ambient surface §1.1 flagged).

That is **not** the full registry - no register-permission policy, no rights-narrowing, no
delegated-cap bookkeeping. The kernel already routes opaque endpoints; a flat name label per endpoint
plus reacquire-by-name is a thin **recovery directory**, not naming *policy*. The registry **service
retires** (the directory replaces it); its richer features aren't needed for recovery.

#### 3.7.3 How the supervisor recovers (mechanism, checked)

| State the supervisor needs | Recovery source |
|---|---|
| caps to running services | `AcquireSendCap` from the kernel directory (re-minted by the kernel) |
| which services *should* run | a **persisted manifest**, reconciled against the kernel's live-task list - so a service that died *during* the supervisor's downtime is noticed and restarted |
| its death-notification endpoint | the kernel **re-points** death notices to the new instance |
| `service_control` + other authority caps | re-minted at spawn (the kernel grants them by name) |
| clients finding the new supervisor | clients reacquire it **by name through the directory** - the directory *is* the bootstrap, so no special stable endpoint is needed |

Capabilities are never persisted; the manifest carries intent and the kernel re-mints the caps. Clients
that were mid-request when the supervisor died get `EndpointDead` and poll until it is back - exactly
§14.3, already proven by the chaos double-storm.

#### 3.7.4 The trade - §26.10 vs §6.3 - and why Path C is chosen

Path C **softens §26.10** (a thin naming facility stays in the kernel) to **better serve §6.3**
(*reduce the TCB over time* - here, to its theoretical floor, `{kernel}`). It chooses
**fault-tolerance over the last increment of kernel-naming purity** - the right priority for an OS:
*availability of everything above the kernel* outweighs the final scrap of minimalism.

The §26.10 scope-creep worry (§1.2) was about *unbounded* "the kernel already does X, so my thing
fits." Path C's exception is the opposite: **one named, documented, frozen exception** - the recovery
directory - with a hard, non-extensible rationale (*the recovery anchor must be unkillable, and only
the kernel is*). That is a defensible boundary, not a slippery slope. The kernel's naming role still
**shrank dramatically**: from "resolve names to wire every service at spawn" (today) to "a flat
recovery directory for re-minting caps after a restart." Phases 0a–3c - moving all *wiring* to the
supervisor - **stand unchanged**; only the endgame's target moves.

> **Path C in one line:** the kernel keeps a minimal, gated name→cap **recovery directory**; the
> registry service retires into it; **init is removed**; the **supervisor is restartable** (kernel
> respawns it, it recovers from the directory). **Unkillable = `{kernel}` only.**

---

## 4. What the kernel deletes - and keeps (revised for Path C)

**Deletes:**

- The `names::lookup` **send-peer wiring loop** in `spawn_service_with_config` - once every service
  is supervisor-wired at boot *and* restart (Phases 3b/3c, done). The kernel no longer resolves names
  to *wire* anyone.
- SDK `reacquire_via_registry` / the userspace registry **lookup path**, and the **registry service**
  itself (Path C, §3.7) - its recovery role moves to the kernel directory.

**Keeps (the Path C exception - was "delete" under the original plan):**

- `kernel/src/ipc/names.rs` - the flat `name → EndpointId` **recovery directory** (`names::register`
  still runs at spawn; `names::lookup` serves recovery, not wiring).
- Syscall 10 `AcquireSendCap` - the **re-mint-a-cap-by-name** recovery primitive, now **GATED** behind
  a recovery capability (closing the ambient surface from §1.1). SDK keeps a thin `reacquire_cap`.
- **InspectKernel query 2** ("endpoint generation by name") - rides on the kept directory; no longer
  needs a name-free rewrite.

Also unchanged: routing, cap machinery, the spawn syscall (new shape), `service_config_by_name`
(ELF lookup - §7), task labels, MMIO/DMA/IRQ + endpoint + delegated-resource minting.

---

## 5. Migration plan (incremental, always-bootable)

The boot/spawn path is the most load-bearing code in the system and is currently hardware-proven
(selfcheck 185/0). **Do not big-bang it.** Each phase keeps the full suite green
(identity 23/23, files 137/0, shell 67/0, script 4/0, and `selfcheck` green) before the next.

| Phase | Change | Status |
|---|---|---|
| **0a** | New `SpawnReturningEndpoint` syscall - spawn returns the new endpoint cap to the caller. Additive. | ✅ merged |
| **1** | Supervisor builds its `name → cap` map (`NameCapMap`) from those caps (shadow). | ✅ merged |
| **0b** | New `SpawnWithCaps` syscall - kernel installs caller-supplied send-peer caps. Wiring becomes a **merge** (install caller caps, then name-wire any peer not provided). | ✅ merged |
| **2** | Flip `fs ← block-driver` (wired from the map). | ✅ merged |
| **3a** | Flip `shell ← fs`; generalize `spawn_wired(name, peers)`. | ✅ merged |
| **3b** | Move `registry` spawn `init → supervisor` (§11); provide `registry` to all services → every real service 100% supervisor-wired **at boot**. | ✅ merged |
| **3c** | Flip the supervisor's **restart** paths to re-wire from the map (map updates in place + frees the dead cap). Boot **and** restart now avoid kernel name resolution. | ✅ merged |

**Endgame - re-scoped for Path C (§3.7).** Each phase still a mergeable, always-bootable, suite-green
increment.

| Phase | Change | Done when |
|---|---|---|
| **4 - Retire the registry service; the kernel directory becomes the namer.** | Clients reacquire via the **gated** kernel directory (`AcquireSendCap`) instead of the registry service; delete the `registry` service + its userspace lookup path. The registry-bootstrap stopgap becomes the *normal* path. **Gate `AcquireSendCap`** behind a recovery cap (close the ambient surface). | No `registry` service in the tree; clients reacquire via the gated directory; chaos double-storm green; suite green. |
| **5 - Remove `init`; the kernel spawns the supervisor directly.** | Retarget `spawn_init` at `supervisor`; move `logger`'s spawn into the supervisor; delete `init`. −1 TCB member. | Boot via kernel→supervisor→all; suite green; §11/§6 amended. |
| **6 - Make the supervisor restartable; unkillable = `{kernel}`.** | Kernel **respawns the supervisor on death** (instead of panic) and **re-points death notices** to the new instance. Supervisor persists a **manifest**, and on respawn rebuilds its `name → cap` map from the kernel directory + reconciles against live tasks. | Kill the supervisor → kernel respawns it → it recovers + the system continues, no reboot; new identity test pins it; §6.2 amended. |

Roll-back is per-phase: each phase is a mergeable, green increment, so a regression reverts one phase,
not the program. Phases 4 and 5 are mechanical; **Phase 6 is the constitutional one** (it amends §6.2:
the supervisor's death is no longer a kernel panic) and the largest - it gets its own design pass.

---

## 6. Constitutional amendments required

To be drafted into `CLAUDE.md` at adoption (each with a commit rationale, §21):

- **§4.4 (Kernel Anti-Scope):** the kernel does **not resolve names to wire services** - the supervisor
  owns wiring and cap distribution. The kernel keeps only a **minimal name→endpoint recovery
  directory** (Path C, §3.7), a single named, frozen exception for re-minting caps after a restart.
  *(Done in spirit by §11 amendment, Phase 3b.)*
- **§11 (Bootstrap):** the new spawn protocol; registry spawned by the supervisor *(✅ amended, 3b)*;
  later, the kernel spawns the supervisor directly (Phase 5).
- **§26.10 (Mechanism, not policy):** record the worked example **and the bounded Path-C exception** -
  the recovery directory is the one place naming stays in the kernel, justified by §6.3 (the recovery
  anchor must be unkillable), not a precedent for further kernel growth.
- **§6.1/§6.2/§6.3 (TCB):** the big one (Phase 6) - **the supervisor becomes restartable**: its death
  is no longer a kernel panic; the kernel respawns it and it recovers from the directory. The
  non-restartable set shrinks to **`{kernel}` only** - §6.3's goal reached at its floor. Retire
  `registry` from §6.1, the Glossary, and `docs/registry.md` (Phase 4).
- **§13 (Contracts) note:** `send_peers` is a *requirement the supervisor fulfils*, not something the
  kernel resolves - sharpening §13.3. *(In effect since Phase 3.)*
- **§22:** extend Test 11 to pin client-resolution-after-restart via the directory (Phase 4); add a new
  identity test for **supervisor-survives-its-own-restart** (Phase 6) - the executable form of
  "unkillable = `{kernel}`."

---

## 7. Out of scope (and why)

- **name → ELF resolution for spawn (`service_config_by_name`).** The kernel spawns embedded services
  by name because their ELFs are `include_bytes!`'d into the kernel image (a v1 packaging reality).
  Moving *that* out means the supervisor carries/loads the binaries (from disk - the Prime/loader
  story, `docs/prime.md`). That is a deeper, separate change; this spec deliberately stops at IPC
  name resolution. The two are independent: the new spawn protocol works whether the ELF is kernel-
  embedded or supplied by the caller.
- **Multi-node / cluster naming** (Appendix C.4) - unaffected; the supervisor-as-name-authority model
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
5. **Retire vs front-end.** ~~Open (§3.5).~~ **Resolved by Path C (§3.7):** the registry *service*
   retires; its *recovery* role moves to the minimal kernel directory.

### Path C-specific risks (Phase 6)

6. **Supervisor recovery completeness.** A respawned supervisor rebuilds caps from the directory and
   reconciles a persisted manifest against live tasks. The gap to watch: a service that dies *and is
   itself mid-restart* exactly as the supervisor dies. Reconciliation (manifest vs live-task list)
   should catch it on the next pass - pin this in the Phase-6 identity test.
7. **Manifest authority + location.** The "should-run" manifest is supervisor-owned intent. Simplest:
   a fixed compiled-in list (the current boot order) - no persistence needed for v1, since the set of
   services is static. Persisting to `fs` would re-introduce a dependency (and `fs` is itself
   restartable); a compiled-in manifest sidesteps it.
8. **Gating `AcquireSendCap` (Phase 4).** It becomes the universal recovery primitive, so it must be
   gated (a `RECOVER`/directory cap) rather than ambient - held by the supervisor and by services that
   legitimately reacquire their own declared peers.
9. **Kernel re-points death notices (Phase 6).** On supervisor respawn the kernel must redirect the
   death-notification endpoint to the new instance - a small, bounded kernel mechanism (it already
   tracks the supervisor specially).

---

## 9. Test strategy

No new test *categories* - the existing suite is the safety net, run green at every phase (§5). Plus:

- **Phases 2/3 (done):** boot + cross-core IPC and the files test (real fs↔block-driver) prove
  wiring-by-cap matches wiring-by-name; the chaos double-storm proves restart re-wiring.
- **Phase 4:** the chaos double-storm regression must still pass with the `registry` service gone,
  through the gated kernel directory. `AcquireSendCap` rejects callers without the recovery cap.
- **Phase 6 (the headline test):** a new identity test - **kill the supervisor, the kernel respawns
  it, it recovers its map + manifest, and the system keeps running with no reboot.** The executable
  proof that the unkillable set is `{kernel}` only. `docs/unsafe-audit.md` essentially unchanged (the
  kernel additions - gated directory, respawn-supervisor, re-point death notices - are small and in
  permitted layers or safe `fn`s).

---

## 10. Summary

The kernel stops **resolving names to wire services** - the supervisor owns all wiring, at boot and on
restart (Phases 0a–3c, done: spawn returns endpoint caps; the supervisor installs caller-supplied caps
and wires dependents from its `name → cap` map). What the kernel **keeps** is one bounded exception: a
minimal, gated `name → endpoint` **recovery directory** (Path C, §3.7) - because the recovery anchor
must be the one thing that cannot die, and only the kernel is. With that anchor the registry service
retires into it (Phase 4), `init` is removed (Phase 5), and the **supervisor itself becomes
restartable** (Phase 6: the kernel respawns it and it recovers from the directory). The result is the
**theoretical-minimum liveness base - `{kernel}` alone** (§6.3 reached at its floor), bought by
*softening* §26.10 (a thin naming facility stays in the kernel): a deliberate, documented,
non-extensible trade of last-scrap minimalism for the availability of everything above the kernel. The
change is large and load-bearing, so it ships incrementally, always-bootable, suite green at every
step - never a big-bang on the boot path.
