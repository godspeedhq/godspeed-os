# Utility: `spawn`

**Utility:** `spawn` - start a service
**Status:** Built. As-built reference.
**Shape:** shell built-in (see `0_conventions.md` §2).

> Part of the shell's **service-control trio** with `kill` (`11_kill.md`) and
> `restart` (`12_restart.md`). These three are where the shell exercises its
> capability-broker role (Appendix B.3): it holds the authority and acts on the
> user's behalf.

---

## 1. Purpose

`spawn` asks the kernel to start a named service: load its binary, build its cap
table from its contract, place it on a core (§14.1), and add it to the run queue.

## 2. Invocation

| Command | Meaning |
|---|---|
| `spawn <name>` | Start the service named `<name>`. |
| `spawn` (no name) | Prints `usage: spawn <name>`. |

## 3. Behaviour & guards

- The kernel resolves `<name>` to a known service binary + contract. Unknown names
  return a descriptive error rather than a silent no-op (§12 loud failure).
- **Singleton guard:** spawning a service that is already live is refused - there
  is one instance per name (`feat/task` singleton guard).
- Placement follows the contract (or round-robin); a contracted-but-unavailable
  core is rejected with `PlacementInvalid` (§9.2).

## 4. Capabilities

- **`SPAWN`** (WRITE, resource 2) - held by the shell (broker) and supervisor. A
  service without this cap cannot start other services (§3.1).
- **Console output** for the result / error line.

## 5. Non-goals

- **No cap-delegation arguments.** A general "spawn child X with caps A, B" surface
  (for pipes / scripting) is future work (Appendix D); today `spawn` starts a
  service with the caps its own contract declares.

## 6. Conformance

Conforms: own `spawn help` / `spawn version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services** as `spawn <name>`. See `0_conventions.md` §3.
