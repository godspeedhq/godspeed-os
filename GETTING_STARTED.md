<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Getting Started with GodspeedOS

The fast path from zero to your first running service. This page is the on-ramp; the rules behind the
architecture live in [`COMMANDMENTS.md`](COMMANDMENTS.md) (5 minutes) and the full constitution
[`CLAUDE.md`](CLAUDE.md). You do not need to read those first to get something running.

## 1. Boot the OS (about a minute)

You need Rust nightly, QEMU on your PATH, an x86-64 host, and the Limine binaries - the one-time setup
is in the [README](README.md#getting-started). Then:

```bash
cargo run -p osdev -- build          # build the kernel + all services
cargo run -p osdev -- run --smp 4    # boot in QEMU with 4 cores
```

You should see `kernel: 4 cores ready`, then `supervisor: ready`, then ping and pong exchanging
messages across cores. For an interactive prompt instead, run `cargo run -p osdev -- shell` and type
`help` at the `gsh>` prompt.

## 2. Write your first service (about 5 minutes)

Every service is a tiny `no_std` crate with **four files**. The minimal one is
[`examples/00-hello`](examples/00-hello) - copy that folder and grow it:

| File | Role |
|------|------|
| `Cargo.toml` | the crate; depends on `godspeed-sdk` (the only way to reach the OS) |
| `build.rs` | links `services/user.ld` and sets the ELF entry point to `service_main` |
| `contracts/<name>.toml` | declares what the service may do - it gets **only** these capabilities |
| `src/main.rs` | `service_main(ctx: ServiceContext) -> !`, the function the kernel calls at spawn |

The entire `src/main.rs` for `hello`:

```rust
#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

#[no_mangle]                                    // REQUIRED, directly on the entry - see the gotcha below
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("hello: starting");                // ctx is the ONE gateway to the OS
    loop { ctx.yield_cpu(); }                   // a real service would block on recv here
}
```

To make it *your* service, edit just two files:

1. **`contracts/<name>.toml`** - declare the minimum you need (`ipc_send`, `ipc_receive`, `log_write`,
   `hw_mmio`, ...). That minimum *is* your security boundary; there is no ambient authority, so anything
   you do not declare, you cannot do.
2. **`src/main.rs`** - reach each granted capability through `ctx` (for example
   `ctx.try_send("peer", &msg)`), and handle the errors the OS can return (`EndpointDead`, `CapNotHeld`,
   ...).

Add your new crate to the workspace `Cargo.toml` members list, then `cargo run -p osdev -- build` and
boot. (`cargo run -p osdev -- new <name>` scaffolds these four files for you.)

## 3. The one gotcha that will bite you: `#[no_mangle]` on `service_main`

A service's entry point is wired **by name**: `build.rs` passes `--entry=service_main` to the linker,
so the ELF entry resolves to a symbol *literally* named `service_main`. Rust mangles symbol names by
default, so **`#[no_mangle]` must sit directly on `service_main`** to keep the name intact. If it is
missing - or separated from the function so the attribute no longer applies to it - the symbol is
mangled, the linker sets the entry address to `0`, and at spawn the service **page-faults at `rip=0`,
prints nothing, and is killed in under a second.**

The tell: a service the supervisor spawns but that never prints its own first log line, dying almost
immediately. If you see that, check that `#[no_mangle]` is directly above `pub extern "C" fn
service_main`. After any refactor of a service's entry point, boot it once and confirm it reaches its
own "starting"/"ready" log.

## Where to go next

- **[`examples/`](examples/README.md)** - the guided tour, in reading order: `00-hello` ->
  `ping`/`pong` (IPC + restart/reacquire) -> `reply-server` (request/reply) -> `cap-grant` ->
  `resource-server` -> the `greet`/`upper`/`roster` pipe trio -> `counter` (state across restart) ->
  `driver-skeleton`/`e1000` (hardware). Each folder's `CLAUDE.md` ties its pattern to a Commandment.
- **[`sdk/rust/CLAUDE.md`](sdk/rust/CLAUDE.md)** - the `ServiceContext` API you call.
- **[`CONTRIBUTING.md`](CONTRIBUTING.md)** - the "Where do I start?" task map and the instant-reject
  rules a pull request is held to.
- **[`COMMANDMENTS.md`](COMMANDMENTS.md)** then **[`CLAUDE.md`](CLAUDE.md)** - the ten rules, then the
  full law behind them.

Welcome aboard. Build something that survives the fire.
