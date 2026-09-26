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
| `00-hello` | The minimal service, on the standard library | **I** (it is a service, not a kernel change), **IV** (declares its needs via a contract), **VII** (gets only the caps it declares) |
| `stdlib-hello` | Doing real WORK with the standard library: read a file and print it, with the opcodes, framing, reply tags and streaming all behind `gs::fs` | **VII** (the contract's `ipc_send = ["fs"]` is why the read can work at all), **IX** (a failure is reported, not retried until something looks fine) |
| `ping` / `pong` | Cross-core one-way IPC + restart/reacquire | **VI** (IPC, not shared memory), **V** (every service is restartable), **VIII** (the generation check settles the restart race, not a sleep), **IX** (reacquire by name + retry on `EndpointDead`) |
| `reply-server` / `asker` | Request/reply (RPC) + the deadlock rule - server (`reply-server`) and its client (`asker`), paired like `pong`/`ping` | **VII** (the server replies only via the client's embedded reply cap), **VIII** (a send is queued, not processed; the reply uses non-blocking `try_send`, §8.9), **IX** (the client reacquires the server by name + retries), **X** (request/reply is service policy; the kernel only routes) |
| `cap-grant` | Transfer a capability over IPC (the GRANT right) | **VII** (authority by capability + the GRANT right), **VI**, **IX**, **X** |
| `resource-server` / `holder` | Mint a delegated resource cap ("a file is a capability", §7.10) - owner (`resource-server`) and its client (`holder`), paired like `pong`/`ping`; holder proves use / non-escalation / revoke | **VII** (minting is gated, never ambient; a granted cap cannot widen its rights), **III** (the service owns the resource's meaning; the kernel tracks only an opaque id), **IX** (a revoked cap fails loud, never silently succeeds), **X** (kernel mints/routes/revokes; the service defines meaning) |
| `greet` | Pipe **producer** (text) | **VI**, **VII** (authority granted at composition, not held), **X** (the shell brokers; the producer just produces) |
| `upper` | Pipe **filter** (transform) | **VI**, **VII**, **X** |
| `roster` | Pipe **record producer** (typed `Table`) | **III** (the table is the one truth; JSON/grid are derived views), **VI**, **VII**, **X** |
| `counter` | Restart-with-state: persist to `fs`, recover on spawn | **V** (restartable like any service), **IX** (persist externally, reconstruct on startup), **VIII** (load the persisted truth), **III** (`fs` owns the durable copy) |
| `driver-skeleton` | A userspace driver (MMIO/DMA/IRQ), `unsafe`-free | **I** + **X** (a driver is a service; `unsafe` isolated to the SDK), **VII** (only the granted MMIO + IRQ), **VI** (an owned DMA arena), **V** + **IX** (restartable, re-inits on spawn), **VIII** (wait on the interrupt, not a sleep) |
| `e1000` | A real minimal NIC driver, read-only: reports link state and the MAC | same as `driver-skeleton`. Its DEGRADE path is proven by `osdev test examples`; its MMIO path is not (see the table below) |

## How each example is PROVEN to run

An example that has never been executed is a claim this folder cannot back. Every one of the fifteen
now runs somewhere, and this is where:

| Example | What runs it | What that proves |
|---|---|---|
| `ping` / `pong` | `osdev test identity` (Tests 3, 6, 9, 10) | cross-core IPC, restart, cap rebinding |
| `counter` | `osdev test counter` | persisted a count, was killed, recovered it on respawn |
| `reply-server` / `asker` | `osdev test reply-server` | the round trip closed and the reply echoed the request |
| `resource-server` / `holder` | `osdev test resource-server` | mint, use, non-escalation refused, `CapRevoked` after revoke |
| `greet` / `upper` / `roster` | the shell, ON DEMAND, whenever a pipe names them (`spawn_via_supervisor`) - so `osdev test shell` and any `selfcheck` run exercise them | they spawn, reach `ready`, and survive repeated chaos respawns. Observed: `greet` + `upper` in the x86 shell suite; all three on the VisionFive 2, `roster` twenty times across a 1000-round chaos run |
| `00-hello` | `osdev test examples` | it starts, holds one capability, and yields through `gs::task` |
| `stdlib-hello` | `osdev test examples` | the `gs::fs` + `gs::io` path reaches a definite outcome |
| `cap-grant` | `osdev test examples` | `gs::cap::self_grant` and `gs::cap::duplicate` really succeed |
| `e1000` | `osdev test examples` | its DEGRADE path: no device, so it logs and idles |
| `driver-skeleton` | `osdev test examples` | the same, which is the discipline it exists to teach |

**Those five are proven on all four ISAs, not just x86.** A claim about one instruction set is not a
claim about the others here - the RISC-V port found four bugs that QEMU on x86 structurally could not
reach. Each port builds them with `--examples`, which adds the same `examples-test` supervisor feature
`osdev test examples` uses, so it is the same proof rather than a similar one:

| ISA | Build | Boot |
|---|---|---|
| x86-64 | (built by the test) | `osdev test examples` - 11 assertions |
| ARMv7 | `py scripts/arm_build.py --release --examples` | `py scripts/arm_run.py --release` |
| AArch64 | `py scripts/pi4_build.py --release --examples` | `py scripts/pi4_run.py` |
| RISC-V 64 | `py scripts/riscv_build.py --release --examples` | `py scripts/riscv_run.py --release` |

On all four, every one of the five logged its startup line AND its documented outcome, with zero
kernel panics. The flag is opt-in and changes nothing without it: the default `kernel7.img`,
`kernel8.img` and VisionFive images rebuild to their exact previous byte sizes.

**Two gaps, stated rather than implied.** `e1000` and `driver-skeleton` are drivers, and in
`osdev test examples` they are granted no device - so their MMIO paths are NOT exercised, only their
degrade paths. Granting `e1000` the NIC would put two drivers on one controller (`nic-driver` takes
it by PCI class, unconditionally), which is the footgun the 2026-08-09 amendment in CLAUDE.md 6.4
records. The register-read path is covered instead by `nic-driver`, which uses the same
`Mmio::read32` wrapper on every boot and is hardware-verified on the T630 and the Wyse. Likewise
`cap-grant`'s actual TRANSFER has no `receiver` to land on; that path is covered by
`resource-server` granting to `holder`.

**Cross-cutting: Commandment II (love Chaos).** *Every* service here, before it is "done", must
survive `chaos max-carnage` - kill storms, flood storms, mem pressure, spawn storms. If Chaos finds
a bug, the bug already existed. Each `CLAUDE.md` notes this; it is the universal acceptance test.

## Start here (reading order)

1. **`00-hello`** - the anatomy of a service: `Cargo.toml`, `build.rs`, the contract, `service_main`.
2. **`stdlib-hello`** - the same anatomy doing actual work: read a file with `gs::fs`. Read its
   header for what you no longer need to know (opcodes, framing, reply tags, streaming) and what
   you still do (authority is granted, and a failure is a fact).
3. **`ping` / `pong`** - one-way IPC and the canonical restart/reacquire pattern (Commandments V, VIII, IX).
4. **`reply-server`** (+ its client **`asker`**) - the other IPC direction: request/reply (RPC) and the §8.9 deadlock rule. `osdev test reply-server` boots the pair and proves the round-trip.
5. **`cap-grant`** - how authority *moves*: transferring a capability over IPC (the GRANT right).
6. **`resource-server`** (+ its client **`holder`**) - how authority is *born*: minting a delegated resource cap ("a file is a capability", §7.10). `osdev test resource-server` boots the pair and proves use / non-escalation / revoke.
7. **`greet` -> `upper` -> `roster`** - composition: capability-mediated pipes, ending with typed records.
8. **`counter`** - state that survives restart: persist via `fs`, reconstruct on spawn (Commandments V, IX).
9. **`driver-skeleton` -> `e1000`** - driving hardware as an ordinary, restartable, least-privilege service.

## The set is complete

These examples cover the full *vocabulary* of GodspeedOS, not a sample of cases:

- a **service** (`00-hello`);
- both **IPC directions** - one-way (`ping`/`pong`) and request/reply (`reply-server` + its client `asker`);
- all three **capability operations** - *use* (`hello`/`ping`), *transfer* (`cap-grant`), *mint* (`resource-server` + its client `holder`);
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
