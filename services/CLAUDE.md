# services/

All userspace services. Each service is a separate Rust crate that links against `sdk/rust`.

## TCB members (§6.1) - trusted root

| Service         | Role |
|-----------------|------|
| `supervisor/`   | Holds restart authority + name authority; **spawned directly by the kernel** (init removed, Phase 5). Trusted, but **restartable** (Phase 6) |

The supervisor is trusted root, but **it is restartable** (Path C / Phase 6, §6.2): when it dies (a
fault, or `chaos kill-storm supervisor`) the **kernel respawns it** - unconditionally and forever (no
bound; a bound would re-introduce the reboot and hand an attacker a DoS) - and the respawned supervisor
**reconciles**, adopting the still-running services (reacquiring each by name from the kernel directory)
instead of duplicating them. So its death is *recovered, not a reboot*. The **only unkillable component
is the kernel itself** (`{kernel}`). Pinned by §22 Test 15.

## Restartable services

**Directly auto-restarted** - the kernel notifies the supervisor of their death, which respawns them:

| Service      | Notes |
|--------------|-------|
| `block-driver/` | Restartable (Phase D); holds no persistent state; re-inits the controller on respawn |
| `fs/`        | Restartable (Phase D); re-mounts to a consistent state via its crash-consistency journal (§6.8) |
| `shell/`     | The user's interface - a crash or `kill shell` respawns a fresh prompt (in-flight command lost - a re-init, not a resume). "Nothing escapes" |
| `xhci/` `ehci/` | USB host drivers - own-death respawn re-grants MMIO/DMA/IRQ caps + re-inits the controller + re-enumerates devices. Without this, a `chaos max-carnage` that kills them in its last rounds left the keyboard dead until a lucky supervisor respawn |
| `logger/`    | Stateless; respawn drains the kernel ring buffer afresh |

`block-driver` must respawn before `fs` (fs's send-peer cap to it wires at spawn). The kernel notifies
the supervisor only for this **named set** (not probes), so ordinary probe/app churn never floods it.

A respawn is always a **fresh instance**: the supervisor spawns a new task with a *new* endpoint
(generation bumped) and *fresh* caps minted from the contract - never the dead instance's. The dead
generation goes stale, so clients get `EndpointDead` and reacquire by name (§14.3). The service never
restarts *itself* (a dead task can't); the kernel is the messenger, the supervisor the actor.

**Revived on a supervisor respawn (only)** - `ping`, `pong` (demo services, bare-metal skips them) are
not individually watched; a supervisor respawn re-runs its boot sequence and re-spawns them fresh.

## Supervisor spawn order

The supervisor spawns services in this order:
1. **pong** (core 1) - must be first so ping's SEND cap is wired at ping's spawn time
2. **ping** (core 0)
3. 178 probe services (§22 test infrastructure)
4. Logs `"supervisor: ready"`

Pong and ping start communicating within ~10 s of boot. `"supervisor: ready"` appears after all spawns complete.

## Adding a new service

1. `osdev new <name>` - scaffolds the directory.
2. Write `contracts/<name>.toml` - declare only what the service actually needs.
3. Implement `service_main(ctx: ServiceContext)` - use `ctx.capability()` for every privileged action.
4. Add the crate to the workspace `Cargo.toml`.
5. Run `osdev validate` - must pass before any PR.

## Service rules

- No global mutable state (§3.9). Per-task state is fine; anonymous singletons are not.
- No `unsafe` in service code (§18.2). If you think you need `unsafe`, you need the kernel instead.
- Services must be restartable unless explicitly listed in the TCB (§3.6).
- A service that calls `try_send` in a loop toward another service that also sends back must use `try_send` on both sides - not blocking `send` (§8.9).
