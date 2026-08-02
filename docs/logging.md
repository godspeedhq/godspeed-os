# Logging (design, not built)

> **Status: non-normative.** This records design intent for what the `logger` service is *for*. Nothing
> here amends the constitution; when it and `CLAUDE.md` disagree, `CLAUDE.md` wins. What exists today is
> described under "What runs now" at the end.

## The one-line purpose

**The logger is a broker, not a store.**

The kernel's 16 KiB ring buffer (§11.4) is a *mechanism*: a bounded byte sink that drains to serial. It
deliberately has no opinion about levels, formats, retention, or who may read what. All of that is
policy, and policy belongs in a service (§26.10). The logger exists to be the place where "I have
something to say" becomes "these particular consumers hear it".

That is the whole justification for it being a service at all. If the answer were "make the kernel ring
buffer bigger and cleverer", we would be growing the kernel to hold policy, which §4.4 forbids.

## Logging is a pipe

There is no ambient `stdout` here. A service can emit only because something handed it a send cap at
spawn (§3.1, Appendix B.3). So logging is not a special subsystem: it is a **pipe whose far end happens
to be the logger** - the same primitive as `cmd1 | cmd2`, where the shell mints an endpoint and grants
SEND to one side and RECV to the other (Appendix D.3, `docs/pipes.md`).

This collapse is worth keeping. Logging and piping should not be two mechanisms that happen to look
alike.

## Reading: subscription is a capability, not a filter

The central design choice. Two readings of "show me service X's logs":

**Rejected - merged stream plus filter** (syslog, journald). Everyone writes into one stream tagged by
service; consumers grep for the tag. Simple and familiar, and it quietly reintroduces ambient authority
in the observability plane: anyone who can read the stream reads *everyone's* logs and is trusted to
ignore the rest. Logs carry secrets. A single global stream that any holder of one read cap can scrape
is exactly the hole the rest of the system spends its effort not having.

**Adopted - subscription as a delegated resource capability (§7.10).** A log stream is a resource. The
logger mints a cap to one service's stream and hands it over; the kernel validates and routes it, exactly
as it does for a file cap. Three properties fall out for free:

- **Who can hear whom is kernel-enforced**, not enforced by access-control code inside the logger.
- **Revocation is a generation bump.** Cut a diagnostic tool off mid-stream and its next read fails
  `CapRevoked`.
- **Grants narrow.** Hand a tool a read-only cap to exactly one service's stream and nothing else.

The inversion to hold onto: **the stream is not public with access checks bolted on; it does not exist
for you until someone hands it to you.** That is the opposite of a Unix `/var/log`, where the data lies
there and permissions are a filter on top.

**Subscribing is itself gated.** There is no "open the firehose" call available for the asking - that
would be ambient authority (invariant 1). The authority to subscribe arrives from a contract at spawn or
from a broker. There is precedent in this codebase for treating this seriously: `InspectKernel` and
`TaskStat` sit behind an `INTROSPECT` capability precisely because **read-only is still authority**
(`docs/introspection-capability.md`). Log subscription is the same family and arguably stronger - a log
carries what a service was *doing*, not merely its state. "It is only output" is not a reason to leave it
ungated.

## Attribution is unforgeable

The kernel already knows who called: it validated the caller's capability. So the service identity on a
record does not have to be a string the sender chose and could lie about - it is derived from who holds
the cap. This is a real difference from syslog, where anything can claim any tag, and it is free here
because the capability check has already happened.

## Who actually sees the logs

- **The operator at the serial console sees everything, always.** Serial sits *below* the capability
  system: it is the physical channel, and physical access is root-equivalent here as everywhere. The
  capability model constrains services running on the machine, not the person holding it. This is a
  deliberate exception, not a gap.
- **A human via the shell** sees whatever they ask for, because the shell is the capability broker
  (Appendix B.3). `logs fs` *looks* ambient; the mechanism is that the shell holds the authority and
  mints a narrowed, revocable cap for the tool it spawns. Familiar UX, capability plumbing.
- **Another service** sees a stream only via a grant a reviewer can find and trace (§26.9).

## Stateless is load-bearing

Keeping the logger **stateless** is what makes it a *router* rather than a database, and routers are
always available. It holds a bounded window at most.

The moment it persists, it needs storage; storage means `fs`; and `fs` and `block-driver` both log. That
is a **cycle** - logger to fs to block-driver and back to logger. §8.9 is blunt about the consequence:
the kernel does not detect deadlock, and in any protocol where two services send to each other at least
one direction MUST use `try_send`. A logger that blocks writing to storage while storage blocks logging
is the textbook mutual block.

Worse than the deadlock is the observability inversion: **if the log path runs through storage, a storage
failure destroys your ability to observe the storage failure.** Serial and the kernel ring buffer survive
when everything above them is broken, which is the only reason the framebuffer-console bring-up was
debuggable at all (see `docs/arm32-status.md`).

So persistence belongs in a **separate downstream sink service** that is allowed to be unavailable, while
the router and serial keep working. Serial stays wired underneath everything as the observability floor.

## Shape

```text
services --(send cap)--> logger  (stateless router, unforgeable attribution)
                            |
              +-------------+-------------+
              v             v             v
           serial      fb console    log-store (stateful, may fail)
          (floor)                    network export
```

## Obligations any implementation inherits

- **Bounded queues force a stated loss policy.** Endpoints are 16 deep (§8.5). Under a log storm the
  logger's queue fills, and the sender either blocks - a service hung by its own diagnostics, which is
  unacceptable - or drops. §26.6 wants the saturation behaviour named; §26.7 wants the drop **counted and
  reported**, never silent. "How many did I lose" is part of the protocol, not an afterthought.
- **Formatting is fine; buffering is not.** §26.6.1 rules out a heap. Streaming JSON (or whatever) through
  a fixed stack buffer via `format_args!` is the sanctioned pattern; building a document in memory first
  is not.
- **The producer never decides the format.** Same split as Appendix C.2 (native metrics): the producer
  emits structured records, and a consuming service exports in whatever shape is wanted. A service should
  not know or care that its output ends up as JSON.
- **A persistent sink owns state**, so it inherits the crash-consistency conversation `fs` had to have
  before it could leave the TCB (§6.1 Phase D, `docs/persistence.md`).

## What runs now

`services/logger/src/main.rs` is 29 lines and does almost none of the above:

1. Logs `logger: ready` at startup.
2. Blocks on `recv()` and **drops every message**.

The drop loop is not laziness - it is a real fix. The service owns an endpoint, and an endpoint has a
16-deep queue. A stub that merely parked would let that queue fill (a chaos flood-storm, one stray send)
and sit at 16/16 forever, failing every later sender. Blocking on `recv` and dropping keeps it drained
while still parking the task between messages, so the core idles.

Note what this means: **nothing currently logs *through* the logger.** `ctx.log()` is syscall 5 - it goes
straight to the kernel ring buffer and out to serial, never touching the service. Its practical value
today is as the simplest restartable service: stateless, so it is the trivial restart case (§15), and the
second thing the supervisor spawns, which makes it a useful canary that the spawn path works before
anything complicated starts.
