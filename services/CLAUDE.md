# services/

All userspace services. Each service is a separate Rust crate that links against `sdk/rust`.

## TCB members (§6.1) — non-restartable in v1

| Service         | Why non-restartable |
|-----------------|---------------------|
| `init/`         | Spawns supervisor; first userspace authority |
| `supervisor/`   | Holds restart authority; its own death = system reboot |
| `registry/`     | Without it, caps cannot be reacquired post-restart |
| `block-driver/` | FS depends on it; restart loses disk state |
| `fs/`           | Owns persistent state for the system |

Failure of any TCB service causes a kernel panic and system reboot (§6.2). No silent recovery.

## Restartable services

| Service      | Notes |
|--------------|-------|
| `logger/`    | Stateless; ring buffer preserves recent output across restarts |
| `ping/`      | Stateless; canonical client restart pattern (§14.2) |
| `pong/`      | Stateless; spawned first by supervisor (before probe services) |

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
