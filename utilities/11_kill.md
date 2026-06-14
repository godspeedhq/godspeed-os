# Utility: `kill`

**Utility:** `kill` — stop a service
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

> Part of the shell's **service-control trio** with `spawn` (`10_spawn.md`) and
> `restart` (`12_restart.md`).

---

## 1. Purpose

`kill` terminates a running service: mark it Dead, bump its endpoint generation
(invalidating outstanding caps), drain its queues, and reclaim its frames (§14.4,
§14.5). It is for misbehaving services and as the first half of `restart` — not a
graceful-shutdown mechanism.

## 2. Invocation

| Command | Meaning |
|---|---|
| `kill <name>` | Stop the running service named `<name>`. |
| `kill` (no name) | Prints `usage: kill <name>`. |

## 3. Behaviour & guards

- **`service_control` capability required.** `kill` (syscall 8) validates the
  `SERVICE_CONTROL` resource before doing anything; without it the call returns
  `CapNotHeld`. This closes the §3.1/§14.4 ambient-authority hole — before it,
  any service could kill any other. Held only by the shell, supervisor, and test
  probes. See `docs/service-control-cap.md`.
- **TCB guard (kernel).** Killing a trusted-root service (init / supervisor /
  registry / block-driver / fs) is refused — their death means a reboot (§6.2), so
  the request is rejected before any kill happens.
- **Session-input guard (shell).** The shell additionally refuses to `kill` the
  services the live session depends on for input — `xhci`, `ehci`, and `shell`
  itself — because killing your own keyboard/console from the prompt would strand
  the session. This is a shell-level UX guard, not a kernel block (those services
  are restartable in principle, just not from the session that needs them).

## 4. Capabilities

- **`SERVICE_CONTROL`** (WRITE, resource 6) — the gating authority.
- **Console output** for the result / error line.

## 5. Non-goals

- **No graceful shutdown.** `kill` is immediate; the target gets no cleanup
  codepath. Clean shutdown, if ever needed, is a separate mechanism.

## 6. Conformance

Conforms: own `kill help` / `kill version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services** as `kill <name>`. See `0_conventions.md` §3.
