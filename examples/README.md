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
| `ping` / `pong` | Cross-core IPC + restart/reacquire | **VI** (IPC, not shared memory), **V** (every service is restartable), **VIII** (the generation check settles the restart race, not a sleep), **IX** (reacquire by name + retry on `EndpointDead`) |
| `greet` | Pipe **producer** (text) | **VI**, **VII** (authority granted at composition, not held), **X** (the shell brokers; the producer just produces) |
| `upper` | Pipe **filter** (transform) | **VI**, **VII**, **X** |
| `roster` | Pipe **record producer** (typed `Table`) | **III** (the table is the one truth; JSON/grid are derived views), **VI**, **VII**, **X** |
| `00-hello` *(planned)* | The minimal service | **I** (it is a service, not a kernel change), **IV** (declares its needs via a contract), **VII** (gets only the caps it declares) |
| `cap-grant` *(planned)* | Mint a cap, grant it over IPC | **VII** (authority by capability + the GRANT right), **VI**, **X** |
| `driver-skeleton` *(planned)* | A userspace driver (MMIO/DMA/IRQ), `unsafe`-free | **I** + **X** (a driver is a service; `unsafe` isolated to the SDK), **VII** (only the granted MMIO + IRQ), **VI** (an owned DMA arena), **V** + **IX** (restartable, re-inits on spawn), **VIII** (wait on the interrupt, not a sleep) |
| `e1000` *(planned)* | A real minimal NIC driver that runs in QEMU | same as `driver-skeleton`, proven against actual hardware |

**Cross-cutting: Commandment II (love Chaos).** *Every* service here, before it is "done", must
survive `chaos max-carnage` - kill storms, flood storms, mem pressure, spawn storms. If Chaos finds
a bug, the bug already existed. Each `CLAUDE.md` notes this; it is the universal acceptance test.

## Start here (reading order)

1. **`00-hello`** - the anatomy of a service: `Cargo.toml`, `build.rs`, the contract, `service_main`.
2. **`ping` / `pong`** - IPC and the canonical restart/reacquire pattern (Commandments V, VIII, IX).
3. **`cap-grant`** - how authority moves: minting and granting a capability.
4. **`greet` -> `upper` -> `roster`** - composition: capability-mediated pipes, ending with typed records.
5. **`driver-skeleton` -> `e1000`** - driving hardware as an ordinary, restartable, least-privilege service.

## Planned follow-ups

`00-hello`, `cap-grant`, `driver-skeleton`, and the real `e1000` driver are in progress. A
`resource-server` (delegated resource capabilities - the same mechanism that makes a file a
capability, §7.10) and a `persistent-service` (persist via `fs`, recover on restart - Commandments
V/IX) are the next additions.

## The rules every example obeys (the short version)

- It is a **service**, never a kernel change (**I**). If you reach for the kernel, ask "why isn't this a service?"
- It declares **only** what it needs in its contract, and reaches **only** the caps it was granted (**IV**, **VII**).
- It talks over **IPC**, never shared mutable memory (**VI**).
- It assumes it **will** be killed and restarted, and recovers by reacquiring and retrying (**V**, **IX**).
- It waits for **truth** - acknowledgements, events, generations - never for **time** (**VIII**).
- It keeps complexity in the layer that owns it, and stays bounded and `unsafe`-free in service code (**X**, §18.2, §26.6).

See `../COMMANDMENTS.md` for the full text and `../CLAUDE.md` for the constitution behind it.
