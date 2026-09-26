# Utility: `kill`

**Utility:** `kill` - stop a service
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

> Part of the shell's **service-control trio** with `spawn` (`10_spawn.md`) and
> `restart` (`12_restart.md`).

---

## 1. Purpose

`kill` terminates a running service: mark it Dead, bump its endpoint generation
(invalidating outstanding caps), drain its queues, and reclaim its frames (§14.4,
§14.5). It is for misbehaving services and as the first half of `restart` - not a
graceful-shutdown mechanism.

## 2. Invocation

| Command | Meaning |
|---|---|
| `kill <name>` | Stop the running service named `<name>`. |
| `kill` (no name) | Prints `usage: kill <name>`. |

## 3. Behaviour & guards

- **`service_control` capability required.** `kill` (syscall 8) validates the
  `SERVICE_CONTROL` resource before doing anything; without it the call returns
  `CapNotHeld`. This closes the §3.1/§14.4 ambient-authority hole - before it,
  any service could kill any other. Held only by the shell, supervisor, and test
  probes. See `docs/service-control-cap.md`.
- **There is NO TCB guard, and that is deliberate.** Nothing here is refused on
  trusted-root grounds, because the non-restartable set is `{kernel}` alone (§6.2,
  §6.3 - Path C / Phase 6). `fs` and `block-driver` are freely killable and the
  chaos suite kills them by the thousand; killing the `supervisor` is answered by
  the KERNEL respawning it, which the shell says out loud. `init` and `registry`
  do not exist to protect - `init` was removed in Phase 5 and the `registry`
  service retired in Phase 4.
- **`shell` is the one special case, and it is not a refusal either.** Killing the
  shell from the shell self-kills and the supervisor respawns a fresh prompt (the
  in-flight command is lost - a re-init, not a resume). `xhci` and `ehci` are not
  guarded; the shell's own source notes they USED to be.

  > **Corrected 2026-09-26.** This section promised a kernel TCB guard refusing
  > `init / supervisor / registry / block-driver / fs` and a shell guard refusing
  > `xhci / ehci / shell`. Neither exists. `CORE_SERVICES` holds one name -
  > `"supervisor"` - and the guard reads `is_core_service(name) && name !=
  > "supervisor"`, so the branch can never fire. A document that invents a safety
  > net is worse than one that admits there is none, because it invites the
  > experiment.

## 4. Capabilities

- **`SERVICE_CONTROL`** (WRITE, resource 6) - the gating authority.
- **Console output** for the result / error line.

## 5. Non-goals

- **No graceful shutdown.** `kill` is immediate; the target gets no cleanup
  codepath. Clean shutdown, if ever needed, is a separate mechanism.

## 6. Conformance

Conforms: own `kill help` / `kill version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services** as `kill <name>`. See `0_conventions.md` §3.
