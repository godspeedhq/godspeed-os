# GodspeedOS Examples

Working, minimal services that teach how to build on GodspeedOS - and, just as much, *why* the
architecture forces each pattern the way it does. Think of this folder as a **guided tour of the
Ten Commandments** (`../COMMANDMENTS.md`) through real code.

## How to use these

Each example is one concept in its own folder, with a `CLAUDE.md` written to be **pointed at an AI**:
a contributor adapting an example for their own service can hand its `CLAUDE.md` to their assistant
as the pattern primer. Every `CLAUDE.md` follows the same shape:

> Purpose - What it demonstrates - **Why it is built this way (the Commandments)** - The contract,
> annotated - **What you must NOT do** (each anti-pattern tagged with the Commandment it breaks) -
> How to adapt this - See also.

The two bold sections are the point: they tie each design choice to the constitutional discipline,
so you learn the *rule*, see it enforced in code, and learn the failure it prevents.

## The examples, and the Commandments each teaches

| Example | What it is | Commandments it teaches |
|---|---|---|
| `00-hello` | The minimal service | **I** (it is a service, not a kernel change), **IV** (declares its needs via a contract), **VII** (gets only the caps it declares) |
| `ping` / `pong` | Cross-core one-way IPC + restart/reacquire | **VI** (IPC, not shared memory), **V** (every service is restartable), **VIII** (the generation check settles the restart race, not a sleep), **IX** (reacquire by name + retry on `EndpointDead`) |
| `reply-server` / `asker` | Request/reply (RPC) + the deadlock rule - server (`reply-server`) and its client (`asker`), paired like `pong`/`ping` | **VII** (the server replies only via the client's embedded reply cap), **VIII** (a send is queued, not processed; the reply uses non-blocking `try_send`, §8.9), **IX** (the client reacquires the server by name + retries), **X** (request/reply is service policy; the kernel only routes) |
| `cap-grant` | Transfer a capability over IPC (the GRANT right) | **VII** (authority by capability + the GRANT right), **VI**, **IX**, **X** |
| `resource-server` | Mint a delegated resource cap ("a file is a capability", §7.10) | **VII** (minting is gated, never ambient), **III** (the service owns the resource's meaning; the kernel tracks only an opaque id), **X** (kernel mints/routes/revokes; the service defines meaning) |
| `greet` | Pipe **producer** (text) | **VI**, **VII** (authority granted at composition, not held), **X** (the shell brokers; the producer just produces) |
| `upper` | Pipe **filter** (transform) | **VI**, **VII**, **X** |
| `roster` | Pipe **record producer** (typed `Table`) | **III** (the table is the one truth; JSON/grid are derived views), **VI**, **VII**, **X** |
| `counter` | Restart-with-state: persist to `fs`, recover on spawn | **V** (restartable like any service), **IX** (persist externally, reconstruct on startup), **VIII** (load the persisted truth), **III** (`fs` owns the durable copy) |
| `driver-skeleton` | A userspace driver (MMIO/DMA/IRQ), `unsafe`-free | **I** + **X** (a driver is a service; `unsafe` isolated to the SDK), **VII** (only the granted MMIO + IRQ), **VI** (an owned DMA arena), **V** + **IX** (restartable, re-inits on spawn), **VIII** (wait on the interrupt, not a sleep) |
| `e1000` | A real minimal NIC driver that runs in QEMU | same as `driver-skeleton`, proven against actual hardware |

**Cross-cutting: Commandment II (love Chaos).** *Every* service here, before it is "done", must
survive `chaos max-carnage` - kill storms, flood storms, mem pressure, spawn storms. If Chaos finds
a bug, the bug already existed. Each `CLAUDE.md` notes this; it is the universal acceptance test.

## Start here (reading order)

1. **`00-hello`** - the anatomy of a service: `Cargo.toml`, `build.rs`, the contract, `service_main`.
2. **`ping` / `pong`** - one-way IPC and the canonical restart/reacquire pattern (Commandments V, VIII, IX).
3. **`reply-server`** (+ its client **`asker`**) - the other IPC direction: request/reply (RPC) and the §8.9 deadlock rule. `osdev test reply-server` boots the pair and proves the round-trip.
4. **`cap-grant`** - how authority *moves*: transferring a capability over IPC (the GRANT right).
5. **`resource-server`** - how authority is *born*: minting a delegated resource cap ("a file is a capability", §7.10).
6. **`greet` -> `upper` -> `roster`** - composition: capability-mediated pipes, ending with typed records.
7. **`counter`** - state that survives restart: persist via `fs`, reconstruct on spawn (Commandments V, IX).
8. **`driver-skeleton` -> `e1000`** - driving hardware as an ordinary, restartable, least-privilege service.

## The set is complete

These examples cover the full *vocabulary* of GodspeedOS, not a sample of cases:

- a **service** (`00-hello`);
- both **IPC directions** - one-way (`ping`/`pong`) and request/reply (`reply-server` + its client `asker`);
- all three **capability operations** - *use* (`hello`/`ping`), *transfer* (`cap-grant`), *mint* (`resource-server`);
- **composition** (`greet` -> `upper` -> `roster`);
- **state across restart** (`counter`);
- and **hardware** (`driver-skeleton` -> `e1000`).

Because the system is small on purpose, there is no fourth IPC direction or fifth capability
operation waiting to be shown. Past this point you are writing *variations*, not new lessons - which
is exactly the point: a small, complete set beats a large one.

## The rules every example obeys (the short version)

- It is a **service**, never a kernel change (**I**). If you reach for the kernel, ask "why isn't this a service?"
- It declares **only** what it needs in its contract, and reaches **only** the caps it was granted (**IV**, **VII**).
- It talks over **IPC**, never shared mutable memory (**VI**).
- It assumes it **will** be killed and restarted, and recovers by reacquiring and retrying (**V**, **IX**).
- It waits for **truth** - acknowledgements, events, generations - never for **time** (**VIII**).
- It keeps complexity in the layer that owns it, and stays bounded and `unsafe`-free in service code (**X**, §18.2, §26.6).

See `../COMMANDMENTS.md` for the full text and `../CLAUDE.md` for the constitution behind it.
