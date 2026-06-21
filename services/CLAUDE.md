# services/

All userspace services. Each service is a separate Rust crate that links against `sdk/rust`.

## TCB members (§6.1) — non-restartable

| Service         | Why non-restartable |
|-----------------|---------------------|
| `supervisor/`   | Holds restart authority + name authority; **spawned directly by the kernel** (init removed, Phase 5); its own death = system reboot |

Failure of `supervisor` causes a kernel panic and system reboot (§6.2). No silent recovery. It is the
**sole** non-restartable service — `init` was removed (Path C / Phase 5, the kernel spawns the
supervisor directly) and the registry service was retired (Phase 4). Path C / Phase 6 will make even
the supervisor restartable, leaving the kernel the only unkillable thing.

## Restartable services

**Directly auto-restarted** — the kernel notifies the supervisor of their death, which respawns them:

| Service      | Notes |
|--------------|-------|
| `block-driver/` | Restartable (Phase D); holds no persistent state; re-inits the controller on respawn |
| `fs/`        | Restartable (Phase D); re-mounts to a consistent state via its crash-consistency journal (§6.8) |
| `shell/`     | The user's interface — a crash or `kill shell` respawns a fresh prompt (in-flight command lost — a re-init, not a resume). "Nothing escapes" |

`block-driver` must respawn before `fs` (fs's send-peer cap to it wires at spawn).

**Revived on a supervisor respawn** — `logger`, `xhci`, `ehci`, `ping`, `pong` are not watched
individually (so probe/app churn never floods the supervisor), but a supervisor respawn re-runs its
boot sequence and re-spawns every service it owns *fresh*. So they come back whenever the supervisor
is restarted (hardware-proven by `chaos max-carnage`, `utilities/38_chaos.md`).

## Supervisor spawn order

The supervisor spawns services in this order:
1. **pong** (core 1) — must be first so ping's SEND cap is wired at ping's spawn time
2. **ping** (core 0)
3. 178 probe services (§22 test infrastructure)
4. Logs `"supervisor: ready"`

Pong and ping start communicating within ~10 s of boot. `"supervisor: ready"` appears after all spawns complete.

## Adding a new service

1. `osdev new <name>` — scaffolds the directory.
2. Write `contracts/<name>.toml` — declare only what the service actually needs.
3. Implement `service_main(ctx: ServiceContext)` — use `ctx.capability()` for every privileged action.
4. Add the crate to the workspace `Cargo.toml`.
5. Run `osdev validate` — must pass before any PR.

## Service rules

- No global mutable state (§3.9). Per-task state is fine; anonymous singletons are not.
- No `unsafe` in service code (§18.2). If you think you need `unsafe`, you need the kernel instead.
- Services must be restartable unless explicitly listed in the TCB (§3.6).
- A service that calls `try_send` in a loop toward another service that also sends back must use `try_send` on both sides — not blocking `send` (§8.9).
