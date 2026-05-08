# services/

All userspace services. Each service is a separate Rust crate that links against `sdk/rust`.

## TCB members (§6.1) — non-restartable in v1

| Service        | Why non-restartable |
|----------------|---------------------|
| `init/`        | Spawns supervisor; first userspace authority |
| `supervisor/`  | Holds restart authority; its own death = system reboot |
| `registry/`    | Without it, caps cannot be reacquired post-restart |
| `block-driver/`| FS depends on it; restart loses disk state |
| `fs/`          | Owns persistent state for the system |

Failure of any TCB service causes a kernel panic and system reboot (§6.2). No silent recovery.

## Restartable services

| Service   | Notes |
|-----------|-------|
| `logger/` | Stateless; ring buffer preserves recent output across restarts |

## Adding a new service

1. `osdev new <name>` — scaffolds the directory.
2. Write `contracts/<name>.toml` — declare only what the service actually needs.
3. Implement `service_main(ctx: ServiceContext)` — use `ctx.capability()` for every privileged action.
4. Add the crate to the workspace `Cargo.toml`.
5. Run `osdev validate` — must pass before any PR.

## Service rules

- No global mutable state (§3.9). Per-task state is fine; anonymous singletons are not.
- No `unsafe` in service code (§18.2). If you think you need `unsafe`, you need the kernel instead.
- Services must be restartable unless they are explicitly listed in the TCB (§3.6).
- A service that calls `try_send` in a loop toward another service that also sends back to it is a protocol design, not a bug — but both sides MUST use `try_send`, not blocking `send` (§8.9).
