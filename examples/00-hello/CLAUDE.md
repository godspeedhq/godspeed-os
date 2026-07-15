# Example: hello

The minimal GodspeedOS service. If you are writing your first service, start here: copy this folder
and grow it.

## Purpose

Show the anatomy of a service and the single most important rule: a service can do **only** what its
contract granted. `hello` asks for one capability (`log_write`) and does one thing (log).

## What it demonstrates

The four files every service has, and nothing more:

| File | Role |
|------|------|
| `Cargo.toml` | the crate; depends on `godspeed-sdk` (the only way to reach the OS) |
| `build.rs` | links against `services/user.ld` and sets the entry point to `service_main` |
| `contracts/hello.toml` | declares what the service may do (here: only `log_write`) |
| `src/main.rs` | `service_main(ctx: ServiceContext) -> !`, the function the kernel calls at spawn |

`service_main` receives a `ServiceContext`: the one gateway to every OS operation. There is no other
way in (no raw syscalls, no ambient globals). If it is not reachable through `ctx`, and not granted by
the contract, the service cannot do it.

> **Gotcha - keep `#[no_mangle]` directly on `service_main`.** `build.rs` wires the ELF entry by name
> (`--entry=service_main`), so the entry resolves to a symbol *literally* named `service_main`. Rust
> mangles names by default; `#[no_mangle]` must sit *directly* on `service_main` to preserve it. Miss it
> (or let other code separate the attribute from the function) and the entry address becomes `0` - the
> service page-faults at `rip=0` the instant it spawns, prints nothing, and dies in under a second. If a
> service you spawn never logs its first line, check this first.

## Why it is built this way (the Commandments)

- **Commandment I (do not expand the kernel; use a service).** `hello` is a service, not a kernel
  feature. The kernel's job is memory, scheduling, IPC, capabilities, interrupts, and routing, and
  nothing else. Everything a contributor builds lives out here, in userspace. *(COMMANDMENTS.md I;
  CLAUDE.md §4.3-§4.4, Invariant 4.)*
- **Commandment IV (honor service contracts).** `hello` states its needs in `contracts/hello.toml`.
  The contract is the only channel through which it asks for authority; there is no hidden back door.
  When a future `hello` needs to log AND talk to a peer, you add that to the contract; you do not
  invent a side path. *(COMMANDMENTS.md IV; CLAUDE.md §13.)*
- **Commandment VII (no ambient authority).** At spawn the kernel mints `hello` exactly the caps its
  contract named, here just `log_write`, and nothing else. `hello` cannot send IPC, touch hardware,
  or read another task's state, because it never asked for those rights. Authority comes only from a
  held capability, never from identity or ancestry. *(COMMANDMENTS.md VII; CLAUDE.md §7, Invariant 1.)*

## The contract, annotated

```toml
name    = "hello"
version = "0.1.0"

[resources.memory]
request = "32MiB"   # minimum the service needs to start
limit   = "64MiB"   # maximum it may ever allocate (AllocDenied past this)

[capabilities]
log_write = true    # the ONLY authority hello holds: write to the logger

# No [placement] section: omitting it (as hello does) makes the supervisor ROUND-ROBIN the service
# across the ready cores - the right default. (Omitting does NOT mean "core 0".) Pin a core only with
# a real reason: cross-core parallelism, a driver's interrupt locality, or isolation.
```

Read the contract as a request, not a grant: the developer says what the service needs; the OS decides
whether to grant it (CLAUDE.md §13.3).

## What you must NOT do

- **Do not call an operation you did not declare.** `ctx.try_send(...)` from here would return
  `CapNotHeld`, correctly. Reaching for authority you did not ask for breaks **Commandment VII**. The
  fix is always to add the capability to the contract, never to bypass the check.
- **Do not add a `static mut` to keep state.** A service owns its state on its own stack; an unowned
  global mutable is forbidden (Invariant 9) and it breaks isolation. If state must be shared, expose
  it *through a service* (**Commandment VI**), not a global.
- **Do not write `unsafe`.** Service code is `unsafe`-free by rule (§18.2). If you think you need it,
  you need the kernel, or the SDK's audited `Mmio`/`Dma` layer, instead.

## How to adapt this

Your next service starts by copying this folder and editing two files:

1. Add what you need to `contracts/<name>.toml` (`ipc_send`, `ipc_receive`, `log_write`, `hw_mmio`,
   and so on). Declare the minimum; that is the whole point.
2. In `src/main.rs`, reach each granted capability through `ctx` (for example
   `ctx.try_send("peer", &msg)`), and handle the failures the OS can return (`EndpointDead`,
   `QueueFull`, `CapNotHeld`).

For IPC, see `examples/ping` and `examples/pong`. For capabilities you mint and hand out, see
`examples/cap-grant` and `examples/resource-server`. For hardware, see `examples/driver-skeleton`.

## See also

- **Commandments I, IV, VII** in `COMMANDMENTS.md`.
- **CLAUDE.md** §13 (service contracts), §7 (the capability system), §4.3-§4.4 (kernel scope).
- `examples/ping` - the same skeleton, plus IPC and the restart/reacquire pattern.
