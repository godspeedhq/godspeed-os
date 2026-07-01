# Milestone - Fault Tolerance: Unkillable = `{kernel}` ✅

**Status:** ✅ Complete - "Move Naming Out of the Kernel" (Path C, `docs/naming-design.md` §3.7) built
and merged; all phases done. The non-restartable set has reached its theoretical floor: **the kernel is
the only thing that cannot die.**

---

## What this milestone is

v1 left a non-restartable set of `{init, supervisor, registry, kernel}` (CLAUDE.md §6.3 v2 goal). This
milestone shrinks it to **`{kernel}` alone** - and, in doing so, pulls service-name *policy* out of the
kernel so the kernel is pure mechanism again (§26.10, §4.4).

The lever is the realisation in `docs/naming-design.md` §3.7: reaching "unkillable = just the kernel"
requires a recovery anchor that *cannot* die, which only the kernel is. So the kernel keeps a **minimal,
bounded name→endpoint recovery directory** (one deliberate exception), every other naming/identity job
moves to the supervisor, and the supervisor itself becomes restartable with the kernel as its anchor.
This *softens* §26.10 (a thin naming facility stays in the kernel) precisely to *serve* §6.3.

---

## Achievements

- ✅ **Naming wiring moved out of the kernel (Path C).** The supervisor wires every real service from a
  `name → cap` map at boot **and** on restart - zero kernel name resolution for them
  (`docs/naming-design.md`, Phases 0a-3c). The kernel retains only a **minimal gated recovery
  directory**: `ipc::names` (a bounded `name → EndpointId` map) plus the gated `AcquireSendCap`. Clients
  reacquire a name through the directory after a restart.
- ✅ **The `registry` service was retired entirely (Phase 4).** It had already left the TCB via H11 (a
  restartable userspace name service); Phase 4 deletes the service outright - the kernel directory is the
  namer now. Closes the registry-bootstrap chicken-and-egg (you cannot look the namer up *in* the namer)
  that `chaos kill-storm` exposed. **§22 Test 11** moved off the registry to pin
  *name-resolves-after-restart-via-the-kernel-directory* (CLAUDE.md §6.1, Test 11).
- ✅ **`init` was removed (Phase 5).** The kernel spawns the supervisor **directly** -
  `task::spawn_supervisor`, the kernel's *one* direct spawn (CLAUDE.md §11.1, §11.3). The supervisor, not
  init, spawns the logger. A corrupt supervisor ELF now fails the kernel's own spawn → `KERNEL PANIC`,
  `"supervisor spawn failed"` (§22 Test 1B was re-pointed onto the supervisor).
- ✅ **The supervisor itself is restartable (Phase 6).** When it dies - a fault, or a deliberate
  `chaos kill-storm supervisor` - the **kernel respawns it**, the last-resort recovery anchor. The respawn
  is **unconditional and unbounded by design**: a cap-then-panic bound would re-introduce the very reboot
  this eliminates and hand an attacker a trivial DoS (kill it N times). Each respawn first reclaims the
  dead instance's frames/kstack/caps then allocates fresh, so the footprint is constant - only a *count*
  grows, and a count is not a resource (CLAUDE.md §6.2, §26.6). **§22 Test 15** pins
  *supervisor-survives-its-own-restart*.
- ✅ **The respawned supervisor reconciles, it does not duplicate.** It re-registers its
  death-notification endpoint, then **adopts the still-running services** (reacquiring each by name from
  the kernel directory) and respawns only those that died. A dropped death notification no longer strands
  a service - the supervisor reconciles to the desired state (`560ee2c`), and a restart storm recovers via
  a name-wire fallback without flooding the shell (`8e7d837`).
- ✅ **The respawn is un-starvable (yield-driven, hardware-found).** The kernel respawns the dead
  supervisor from `poll_supervisor_respawn`, which runs at the scheduler loop's top - an `IF=1` point
  Core 0 reaches by *blocking* or via the *timer ISR*. Under `chaos max-carnage` the foreground task
  never blocks (it paces with `yield`), so recovery hinged solely on the timer ISR - which a storm
  starves on real hardware, draining the live set to 0 with no respawn. Fix (`675082c`): drive the
  pending respawn from the **yield path** too (`yield_current` mirrors the timer ISR's pending-respawn
  routing), so the storm's own hot path drives recovery. Zero cost when healthy (one relaxed load),
  guarded by the same PENDING/IN-PROGRESS handshake, and no new `unsafe` (inside `yield_current`'s
  existing block). Confirmed on hardware: a 5000-round max-carnage soak sustains the live set instead of
  draining to 0.
- ✅ **Interdependent services wait on truth without hanging (`Call` / `ReplyDead`).** A dependent that
  issues a synchronous request (the SDK `request_with_reply`, on which `fs`'s `block_rpc` rides) blocks
  for the reply via a kernel `Call` (syscall 41). If the replier dies *after* receiving the request but
  *before* replying, the kernel wakes the caller with `ReplyDead` - the reply-side twin of `EndpointDead`
  (CLAUDE.md §8.6), on the same generation/liveness mechanism - instead of hanging it on a reply that
  will never come. The caller then reacquires the peer by name and retries. This closes the last hang in
  the recovery loop: `fs ← block-driver`, `shell ← fs`, any client ← any server now wait on the *truth*
  of the peer's liveness (Commandment VIII, "the truth must include failure"), never on a timer.
  Mechanism, not policy: the kernel learns only a **reply cap**, never "RPC" (§26.10). Pinned by
  `osdev test reply-dead` (the reply-side twin of §22 Test 4).
- ✅ **The shell survives killing its own spawner.** In the chaos kill-storm the shell prompt keeps
  answering across every supervisor respawn - the system stays *alive* rather than rebooting.
- ✅ **Floor reached and exceeded.** `block-driver`, `fs`, and `shell` are restartable; `registry` is
  retired; `init` is removed; the `supervisor` is restartable. The **only** non-restartable component is
  the **kernel** (DMA drivers aside, and only on a machine with no IOMMU - see `5_IOMMU_DMA_CONFINEMENT`).
  Supported by endpoint-id reclaim and self-heal via unregister-on-death.

---

## Evidence

| Claim | Pinned / recorded by |
|-------|----------------------|
| Naming out of the kernel; supervisor wires `name → cap` | `docs/naming-design.md` §3.7 (Path C); `ipc::names` + gated `AcquireSendCap` |
| Registry service retired; name resolves after restart via the kernel directory | CLAUDE.md §6.1 amendment + **§22 Test 11**; H11 (TCB drop) precursor |
| `init` removed; kernel spawns supervisor directly; boot-spawn failure fatal | CLAUDE.md §11.1/§11.3; **§22 Test 1B** |
| Supervisor restartable; kernel respawns unconditionally; reconciles not duplicates | CLAUDE.md §6.2/§6.3 amendments + **§22 Test 15**; commits `560ee2c`, `8e7d837` |
| Respawn un-starvable (yield-driven, not only timer ISR) | commit `675082c`; `yield_current` mirrors the timer-ISR pending-respawn routing; 5000-round hardware soak |
| Interdependent services wait on truth without hanging (`Call` / `ReplyDead`) | CLAUDE.md §7.7/§8.2/§8.6 (reply-side death-wake); `osdev test reply-dead`; COMMANDMENTS.md VIII |
| Unkillable set = `{kernel}` | CLAUDE.md §6.3 ("goal reached, at the floor"); §24 glossary (Name directory, Trusted root) |
| Shell survives killing its spawner; storm recovery | chaos kill-storm `supervisor` (4× recovered, kernel alive, no bound) |

---

> **Identity over location, at its limit.** Every service is now an identity the system can re-place,
> re-wire, and resurrect; the kernel is the single fixed point that makes that possible. *The only
> unkillable component is the kernel itself.*
