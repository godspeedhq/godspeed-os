# services/events/

The observability sink. **Restartable.** Not a TCB member.

Was `logger`, and the rename is the name catching up with the job: it holds events, and has never held
a log line in its life (see the syscall floor below). Full command surface: `utilities/47_events.md`.

## What it holds - three streams, all VOLATILE

1. **The IPC trace ring** - 192 request/reply events emitted by services whose contract grants
   `ipc_send = ["events"]`. Full = overwrite the oldest and **count** it; `events status` reports the
   count, because a silent loss is the bug (invariant 12). **The kernel records nothing**: the emitter
   knows its own peer's NAME and its own protocol's opcode, and the kernel is forbidden to know either
   (§4.4, §26.10), which is why the instrumentation lives in the SDK and the ring lives here.
2. **The metric table** - 64 fixed slots keyed by `(owner, name)`. A FIXED table, not a map that grows
   with distinct names, because a counter keyed by arbitrary strings is unbounded state wearing a small
   hat (§26.6.1). Full is refused, counted, and said once.
3. **The log window** - an 8 KiB byte ring of copies of what services printed, read by `events log`.
   A byte ring rather than a line array so one long line costs its length instead of a whole slot -
   `fs`'s durability warning is 280 bytes and its journal refusal 356, and both are lines the
   constitution leans on.

It also **drains its recv endpoint** of anything it does not recognise. That is deliberate: a
registered service whose endpoint just parked would let a flood or a stray send fill its 16-deep queue
and sit at 16/16 forever (the flood-endpoint disease). `recv` parks the task between messages, so the
core still idles.

## The floor beneath it: logging does NOT go through this service

`ctx.log()` is a **syscall** that writes the kernel's 16 KiB ring buffer **and** the serial console
**directly**. It does not send IPC here, and never did. So logging does not depend on `events` being
up: when `events` is dead `ctx.log()` still works, and a chaos storm that kills it loses **no log
output**.

What arrives here is a best-effort **copy**, offered after the syscall has already happened. That
ordering is the whole design, and it must not be inverted: re-pointing logs AT this service would make
observing a failure depend on a service that can fail, which is §15's storage argument one layer up.

Consequence, stated rather than discovered: lines printed **before** `events` exists are on serial
only. They live in the kernel ring, which no syscall exposes to userspace.

## Self-observation is a local write, never a message

`events` publishes its own rows (`ring.recorded`, `ring.dropped`, `metrics.held`, `metrics.refused`) by
writing straight into the table it already owns, at read time. It cannot use `ctx.metric()` and needs
no guard against it: it holds no send cap to itself, so the call resolves to `u32::MAX` and returns -
the same cut that stops the sink tracing its own sends.

A self-emit over IPC is the one shape that turns recursion on, because the send is itself a reportable
event. `docs/observability.md` §9.

**Its own death it can never report.** The supervisor's death notification and the kernel's
unconditional serial write do that, and both sit beneath this service rather than inside it.

## Persistence lives in `recorder`, not here

`events` may hold bounded VOLATILE state and must **never** acquire a durable-storage dependency. A
file write blocks on a reply from `fs`, and a blocked single-threaded recv loop stops draining its
endpoint - so it would drop the very events worth capturing, on a sick disk, repeatedly, at the moment
they matter most.

`events persist` is served by `services/recorder/`, which drains this service and writes the file. It
has the identical blocking problem and it does not matter, because nothing depends on it.

## Restartability

Stateless in the sense that matters: the supervisor respawns it on death (§6.2) and it prints
`"events: ready"` again. All three stores are volatile and start empty, which is correct - a restart is
a re-init, not a resume (§14.2), and nothing depends on the history. What survives underneath is
serial and the kernel ring.

**But the PUBLISHERS have work to do when this service restarts, and getting that wrong was invisible.**
A restarted sink has never heard of any service, and every publisher is holding a cap to the dead
instance. Four separate defects lived in that gap - a reacquire that dropped the sample that triggered
it, a missed resolve latching for the life of the service, the kernel dropping the peer DECLARATION
when the sink was down at a service's spawn, and a capless publisher that never ASKED for a cap because
the reacquire sat behind a send failure that a never-sent message cannot produce. All four presented
identically: a row that simply was not there, which is indistinguishable from a dead service.

The publisher side now resolves lazily and retries a miss (bounded), asks the kernel when it holds no
cap, retries the sample after reacquiring, and republishes `msgs.received` on the first message after
the sink comes back. `docs/observability.md` §13 is the full account and the rule it generalises to.

## Still not implemented

`ctx.drain_kernel_ring_buffer()` is a no-op stub, so `events log` begins when this service does.
Tracked, with the reasoning and the cost, in
[`backlog/06-kernel-ring-not-drainable.md`](../../backlog/06-kernel-ring-not-drainable.md).

## Supervisor retry (§11.3)

The **supervisor** spawns `events` (init was removed, Phase 5) and retries once on failure; its output
falls back to the kernel ring buffer meanwhile. It is not TCB, so a spawn failure does not panic the
kernel.

## Kernel log contention

`kprintln!` holds a kernel-side `SpinLock` across a synchronous UART write, which is what makes log
lines splice into each other under load. The problem, its evidence and the proposed
per-core-buffer design are in [`backlog/04-serial-splice.md`](../../backlog/04-serial-splice.md).
