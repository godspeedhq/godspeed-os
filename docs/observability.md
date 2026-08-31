# Observability: metrics, logs and traces, without a kernel endpoint

**Status:** design note. Nothing here is built. Recorded because the question came up and the answer
is already half-evidenced by work that shipped.

**The question.** An observability service that serves metrics, logs and traces - does the kernel need
an endpoint to expose that data on?

**The answer: almost certainly not.** Appendix C.2 of `CLAUDE.md` says "the kernel emits structured
events ... on a known endpoint". That line predates the `trace` utility, which shipped with **zero
kernel growth** and is direct evidence against it.

---

## 1. Two thirds of this is already answered, and it needed no kernel

| stream | where it lives today | kernel involvement |
|---|---|---|
| **logs** | the `logger` SERVICE; every service sends to it | none |
| **traces** | a 192-event ring INSIDE `logger`; instrumentation in the SDK | none |
| **metrics** | see below | pull only, already exists |

The `trace` utility answers "what is stuck", "who can reach whom" and "what just happened", and the
kernel gained nothing for it: no ring in ring 0, no retention policy, no message-identity scheme, no
control syscall, no new capability, and nothing on the IPC fast path.

The design question that settled it was **"why can't the trace be a service?"** - and every objection
to putting a ring in the kernel turned out to be an objection to it being in the kernel.

---

## 2. Metrics split in two, and only one half is a question

**Service-level metrics** (a service counting its own work) are not a kernel matter at all. The
service counts, and sends. This is the `logger` shape again.

**Kernel-level metrics** (IPC volume, scheduler statistics, memory accounting, restart counts) are
data only the kernel has - and it **already exposes them, by PULL**:

```
   InspectKernel (syscall 13)     24 queries
   TaskStat                       per-task state
        |
        +-- gated by the INTROSPECT capability (3.1, docs/introspection-capability.md)
        |
        v
   `observe` reads these today and renders a live view
```

So for STATE, there is nothing to build. An observability service holds `INTROSPECT`, polls, and
formats. That is the whole mechanism.

---

## 3. The genuine gap is EVENTS, not state

A poll cannot see what happened between two polls:

```
   poll                                                     poll
    |                                                        |
    |   cap denial   queue overflow   task killed   restart  |
    |       x              x               x           x     |
    +--------------------------------------------------------+
              all four invisible to both samples
```

These are the interesting ones - a denied capability is a security signal, a queue overflow is a
capacity signal, and neither survives sampling.

---

## 4. Why a kernel endpoint is the wrong answer to that gap

Pushing events to an endpoint fails in exactly the ways the trace ring would have, and those
objections were decisive there:

- **A queue needs a bound, and a bound needs a DROP POLICY.** What gets discarded when the consumer
  is behind - the oldest event, the least important one, a whole class? Deciding what to discard is a
  judgement, and judgement is policy (26.10). That is the argument that kept the trace ring out.
- **A slow or dead consumer cannot be waited on.** The kernel must never block on userspace, so it
  must drop - which is the same policy decision again, now on the kernel's critical path.
- **The kernel would hold an endpoint to a RESTARTABLE service.** Every client of a restartable peer
  owes it a reacquire (14.3). In userspace that is a retry; in the kernel there is nobody above to
  recover it, and a stale endpoint in ring 0 is a much worse thing than a stale cap in a service.
- **It grows the kernel for a diagnostic**, which is the shape 4.4 and 26.2 both reject.

---

## 5. The shape that already works: a bounded ring the kernel writes and a SERVICE drains

This is not hypothetical - it is how kernel logs already reach userspace (11.4):

```
   kernel  --writes-->  [ bounded ring, overwrites when full ]  <--drains--  logger
                          16 KiB, no policy, no endpoint,
                          no consumer identity, no cap
```

The properties that make it right are exactly the ones a push model loses:

- **Bounded by construction.** It overwrites. There is no queue to grow and no decision to make.
- **No consumer identity.** The kernel does not know or care who drains it, so there is no endpoint
  to hold, nothing to go stale, and nothing to reacquire.
- **The policy lives in the drainer.** How often to drain, what to keep, what to aggregate, what
  format to emit - all of it in a service, where judgement belongs.
- **Loss is visible, not silent.** A ring that overwrote can say so (a dropped-count), so the
  consumer learns it missed events rather than silently seeing fewer (invariant 12).

Kernel events would ride the same shape: the existing log ring, or a sibling events ring if the
volume or the record type warrants one.

---

## 6. The test for whether anything new is justified

Before adding ANY kernel mechanism for observability, the candidate must be an event class that is:

1. **known only to the kernel** (otherwise a service can emit it itself), AND
2. **lost by polling** (otherwise `InspectKernel` already serves it), AND
3. **unable to ride a drained ring** (otherwise 11.4's shape already fits).

No current candidate meets all three. If one ever does, that is the moment to design a mechanism -
pulled into existence by a real need (26.2), not built ahead of one.

---

## 7. What this costs today, and why it gets cheaper

During the `trace` work the `service_configs` pin **refused a new `tracer` service** - the kernel
holds a config row per service, so adding one was a kernel change. That refusal is what forced the
trace ring into `logger`, which turned out to be the better design anyway.

After step C (`docs/service-ownership.md`) adding a service costs a crate, a contract and a line in
the supervisor. **So an observability service becomes cheap precisely because of the catalogue work**,
and there is no longer any pressure to fold it into an existing service to dodge the pin.

---

## 8. Recommended shape, when it is built

```
   +-------------------+        +------------------+
   |  kernel           |        |  services        |
   |  InspectKernel  <-|--pull--|  observability   |
   |  TaskStat       <-|--pull--|    service       |
   |  log/event ring --|--drain>|                  |
   +-------------------+        |   holds:         |
                                |    INTROSPECT    |
   +-------------------+        |    a log cap     |
   |  logger (ring)  <-|--query-|    a trace cap   |
   +-------------------+        +------------------+
                                        |
                                        v
                                 exposition format
                            (Prometheus / OTel / custom)
```

Everything it needs already exists or is a service-to-service concern. The kernel publishes what only
it knows, by the two mechanisms it already has, and forms no opinion about consumers.

---

## 9. The service should be renamed: `logger` -> `events`

**The name is already wrong.** `logger` holds the 192-event IPC TRACE ring; it stopped being a logger
when `trace` shipped. So this is not a rename in anticipation of a new job - it is the name catching
up with the job it already has.

**It is cheap, and the reason is the capability model working.** `ctx.log()` resolves through
`log_write_slot` - a CAPABILITY, not a name - so essentially every call site in the system is already
insulated from what the receiving service is called. Only ~46 literal `"logger"` occurrences exist in
code (the supervisor's image table and MANAGED list, contracts, the name directory, tests).

**`events` rather than `observability`:**

- **`observe` already exists** - the live top/htop view. `observability` blurs the two;
  `events` and `observe` read as complementary: one HOLDS the stream, the other RENDERS the state.
- **House style is one short word** - `trace`, `drives`, `edit`, `observe`.
- **It names what the thing holds.** Log lines are events. IPC trace records are events. Metric
  samples are events. An honest unifying noun.
- `observability` is a PROPERTY OF A SYSTEM, not a component. A service named after a property tends
  to become a dumping ground, which is precisely what 4.4 and 26.2 exist to prevent.

### The constraint that must survive the rename

`docs/logging.md` makes a load-bearing argument that this service is a **stateless broker, not a
store**: a persisting logger cycles through `fs`, and worse, **makes observing a storage failure
depend on storage.**

Renaming it to `events` invites exactly the violation - "events implies history, history implies
persistence". So state the line precisely:

> `events` may hold **bounded VOLATILE** state (the 192-event ring already does). It must never
> acquire a **durable-storage** dependency.

If retained metrics are wanted later, that is a separate consumer which DRAINS `events` - not
`events` growing an `fs` peer. The service that reports a storage failure must not be downstream of
storage.

### Timing

Do the rename **as part of** the observability work, not before it. Standalone it is 46 sites of
churn with no behaviour change; bundled, the new name is justified by the new job and lands in one
coherent commit.

---

## 10. Amending C.2

Appendix C.2 is explicitly non-normative, so this note does not amend the constitution. But its
"known endpoint" phrasing should be read in light of the `trace` evidence: the kernel does not need
to publish to an endpoint, and the reasons it should not are the same ones that kept the trace ring
out of ring 0.

C.2's actual constraint - *"the kernel publishes; the metrics service interprets"* - is exactly right
and is preserved here. Only the transport changes: a drained ring rather than an endpoint.
