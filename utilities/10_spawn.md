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

`spawn` asks the **supervisor** to start a named service. The supervisor holds every
service image (step C, `docs/service-ownership.md`) and sends the kernel a spawn
request carrying the service's privileges, send peers, memory limit, core and device
class; the kernel builds the cap table from that request (never from the contract,
CLAUDE.md §13.6), places the task on a core (§14.1), and adds it to the run queue.

## 2. Invocation

| Command | Meaning |
|---|---|
| `spawn <name>` | Start the service named `<name>`. |
| `spawn <a>,<b>,...` | Start each named service in turn (comma list, no spaces, at most 16 names; one failure does not stop the rest). |
| `spawn` (no name) | Prints `usage: spawn <svc> \| <svc>,<svc>,...   (e.g. spawn ping,pong)`. |

## 3. Behaviour & guards

- The supervisor resolves `<name>` to an image it holds. A name it cannot spawn
  prints `spawn failed (unknown service?): <name>` and is an `Err`, never a silent
  no-op (invariant 12); success prints `spawned: <name>`.
- **Singleton guard:** spawning a service that is already live is refused by the
  shell (`already running: <name>`) - there is one instance per name.
- **`supervisor` is refused** (`Not applicable. The supervisor is the restart
  authority ...`, `Err(Denied)`), and so are the `observe` variants, which run from
  the `observe` command instead.
- Placement follows the contract (or round-robin); a contracted-but-unavailable
  core is rejected with `PlacementInvalid` (§9.2).

## 4. Capabilities

- **`SPAWN`** (WRITE, resource 2) - held by the shell (broker) and supervisor. A
  service without this cap cannot start other services (§3.1).
- **A send capability to `supervisor`**, which the request travels over. Supplying
  an image is `IMAGE_SPAWN`, which only the supervisor holds; the shell names a
  service, it never hands over code (CLAUDE.md §14.1, step C amendment).
- **Console output** for the result / error line.

## 5. Non-goals

- **No cap-delegation arguments.** A general "spawn child X with caps A, B" surface
  (for pipes / scripting) is future work (Appendix D); today `spawn` starts a
  service with the authority the supervisor's spawn request for it carries.

## 6. Conformance

Conforms: own `spawn help` / `spawn version` (with a real example, per `0_conventions.md`); listed by the shell's top-level
`help` under **Services** as `spawn <name>`. See `0_conventions.md` §3.
