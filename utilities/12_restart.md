# Utility: `restart`

**Utility:** `restart` — restart a service
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

> Part of the shell's **service-control trio** with `spawn` (`10_spawn.md`) and
> `kill` (`11_kill.md`). `restart` = `kill` then `spawn`.

---

## 1. Purpose

`restart` stops a service and starts it again — the v1 update/recovery mechanism
(live code update is permanently rejected, §2.5/§16). A restarted service may come
back on a **different core**: identity is stable, location is not (§11, §14.2).

## 2. Invocation

| Command | Meaning |
|---|---|
| `restart <name>` | Kill `<name>` then spawn it again (placement re-evaluated). |
| `restart <name> <core>` | Restart and place it on the given core (dev-mode override). |
| `restart` (no name) | Prints `usage: restart <name> [core]`. |

## 3. Behaviour & guards

- **Placement is re-evaluated from scratch** (§9.2): a contract-specified core
  re-applies (and may fail with `PlacementInvalid` if unavailable); otherwise a
  fresh round-robin core is chosen. The previous core is **not** remembered — so a
  service can transparently move cores across a restart.
- The optional `<core>` argument is the supervisor's `placement_override` (§14.4),
  exposed for dev-mode use; it is subject to the same strict placement rules.
- **Same guards as `kill`** (it is the kill half): requires `SERVICE_CONTROL`,
  refuses the trusted root (§6.2), and the shell refuses to restart the session's
  own input devices (`xhci`, `ehci`, `shell`) — see `11_kill.md` §3.
- **Client recovery is the client's job** (§14.3): a client of the restarted
  service sees `EndpointDead` / `CapRevoked` on its next call and must reacquire via
  the registry. The kernel does not rebind for it.

## 4. Capabilities

- **`SERVICE_CONTROL`** (WRITE) for the kill half and **`SPAWN`** (WRITE) for the
  spawn half — both held by the shell.
- **Console output** for the result / error line.

## 5. Non-goals

- **No live/in-place update.** Restart-with-new-binary is the only update path
  (§2.5). `restart` does not patch a running service.

## 6. Conformance

Built-in: no `restart help` / `restart version` yet; listed by the shell's
top-level `help` under **Services** as `restart <name> [core]`. See
`0_conventions.md` §3.
