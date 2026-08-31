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
- ~~**The kernel would hold an endpoint to a RESTARTABLE service.**~~ **CORRECTION: this objection is
  not decisive, and was wrong to present as such.** The kernel ALREADY does this - it sends death
  notifications to the supervisor's endpoint, and the staleness is solved: the respawned supervisor
  re-registers in `ipc::names` and notifications re-point to it (6.2). The pattern exists and works.
  What distinguishes that case is NECESSITY - recovery has no alternative, since the kernel is the
  last-resort anchor - whereas observability has one. A relationship the kernel is forced into is not
  a licence for relationships it merely finds convenient.
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

### What the kernel actually emits today: TEXT, not events

Checked rather than assumed, because it changes the answer:

```
   kprintln!  ->  [u8; 16 KiB] BYTE ring  ->  serial (always)
                                          ->  drained ONCE, when `logger` starts
```

`kernel/src/log.rs` is a **byte ring of formatted, human-readable lines** - not typed records - and
`drain_to_logger` has exactly one caller, the logger's startup. After that everything reaches serial
and the ring only matters again on the next boot.

**So "kernel events ride the existing ring" would mean PARSING TEXT**, which is fragile and creates a
second truth: the parser's idea of an event versus what the kernel meant (26.4). A format change in a
`kprintln!` would silently break a consumer, with nothing to catch it.

If structured kernel events are ever genuinely wanted, that is a **sibling ring of typed records** -
same drained shape, same bounded-and-overwriting properties, but records instead of bytes. It must
clear the three-part test below first. Nothing does today.

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

**A few minutes of scrollback in RAM, gone on restart, is the RIGHT design** - not a limitation to
apologise for. The trace ring already works this way (192 events), and losing it on restart is
consistent with 14.2: a restart is a RE-INIT, not a resume.

The consequence, stated plainly rather than discovered later: **if `events` dies, the window dies
with it**, and under `chaos max-carnage` it is a target like anything else. That is acceptable only
because of what sits beneath it:

| tier | survives | holds |
|---|---|---|
| **serial + the kernel ring** | anything, including a panic that halts every core | the floor - boot, panics, kernel lines |
| **the `events` ring** | its own process lifetime | recent history, convenient, fine to lose |
| **durable storage** | - | **never, for either** |

So `events` is a convenience layer over a floor that outlives it, not the system's only memory. That
floor must not be weakened to make `events` look better: the kernel keeps writing to serial
unconditionally, exactly as it does now.

### Timing

Do the rename **as part of** the observability work, not before it. Standalone it is 46 sites of
churn with no behaviour change; bundled, the new name is justified by the new job and lands in one
coherent commit.

---

## 10. The plan

Phased so that the kernel change is **pulled into existence** by a service that has demonstrably hit
the wall (26.2), not built ahead of one. Each phase is independently useful and independently
shippable.

### Phase 0 - build the service against what already exists

No kernel change at all. Rename `logger` -> `events` (section 9) and give it the observability job:

```
   InspectKernel (24 queries) --pull--+
   TaskStat                   --pull--+--> events --> exposition
   kernel text ring           --drain-+      |
   its own 192-event trace ring ------+      +-- holds INTROSPECT + a log cap
```

Deliverable: metrics and logs and traces served by one service, with **zero** kernel growth. This is
most of the value, and it is reachable today.

### Phase 1 - find out what is actually missing

Run it. Use it. Record specifically what could not be answered and why. The PREDICTION, written down
so it can be wrong:

> **Capability denials will be the gap.** They are never logged (verified: no `kprintln!` on any
> `CapNotHeld` / `CapInsufficientRights` path), so a denial returns an error to the caller and
> vanishes. The adversarial suite proves denials HAPPEN; an operator cannot see that they DID.

If phase 1 finds the text ring and the pull queries are enough, **stop** - the kernel gains nothing
and that is the best outcome, not a disappointment.

### Phase 2 - a typed event ring, only if phase 1 justifies it

| piece | size |
|---|---|
| typed record: timestamp, core, kind, subject, two payload words. **NO STRINGS** | ~30 lines |
| bounded ring + a DROPPED-COUNT, so loss is visible rather than silent (invariant 12) | ~60 lines, mirrors `log.rs` |
| emit macro | ~10 lines |
| emit sites | a handful, see the rule below |
| drain, as a new `InspectKernel` query - **no new syscall** | ~30 lines |
| gating | reuse INTROSPECT; no new capability |

Roughly **150-200 lines of kernel**. That is real 4.3 growth and should be argued for, not waved
through.

### The three rules that keep phase 2 honest

**1. Emit only where the path is ALREADY exceptional.** A capability denial is already an error
return - rare, and off the success path. A success-path emit is not acceptable at any size: 20 makes
the IPC fast path the one place where performance is a first-class constraint, and 21 rejects a
change to it without a benchmark. B1 (IPC round-trip) and B4 (cap validation) must show no regression.

**2. No strings, ever.** A string means formatting inside the kernel: unbounded work on a hot path,
and a judgement about wording. Fixed-size records only; the DRAINER renders them. This is also what
removes the fragile text-parsing problem in section 5.

**3. The record layout is a frozen ABI.** Versioned like `SpawnRequest`, and a mismatched consumer is
refused loudly rather than misreading a shorter record.

### What it costs at the enforcement layer

Both surfaces this touches are PINNED, and deliberately so:

- **`InspectKernel` queries are pinned individually** (`scripts/commandments.py::check_introspect_queries`):
  *"a new query is a new kernel responsibility"*. The drain query must be added to
  `introspect_queries` with a written reason.
- **`kernel/src/` unsafe** is audited per file (18.4). A ring using the existing `SpinLock` pattern
  should need none; if it does, it belongs in a permitted layer (18.5), as `smp::names` did.

Neither is a formality. The pin refusing a `tracer` service is what produced the current design.

### The objection, and the answer

> *"Deciding what to record is judgement, and judgement is policy (26.10) - the argument that kept
> the trace ring out of the kernel."*

The answer is that **the kernel already makes that judgement, about 60 times, in every `kprintln!`**
it contains. It already decides that a failed spawn, an IOMMU fault and an SMP bring-up are worth
reporting. Structured events do not ADD judgement to the kernel; they change the ENCODING of
judgement it already exercises, from text a consumer must parse into records a consumer can read.

That is a genuinely different claim from the trace ring, which asked the kernel to make a NEW
judgement (what to retain, what to discard, for whom) that it was not making anywhere else.

---

## 10a. Endpoint or ring? A ring is a PLACE; an endpoint is a RELATIONSHIP

Worth settling explicitly, because "the kernel exposes a reporting endpoint that `events` subscribes
to" is the natural first design - and the kernel already does exactly that for death notifications,
so it is not obviously wrong.

| | **ring** (kernel writes, service drains) | **endpoint** (kernel sends, service subscribes) |
|---|---|---|
| kernel knows its consumer | no | **yes** - registration, identity |
| consumer dies | nothing happens | kernel must handle it and re-point |
| queue full | overwrites; there is no choice to make | `try_send` fails, so: drop |
| cost at the EMIT SITE | append ~32 bytes | routing lookup, target queue, locks |

**The last row decides it.** A capability denial lives on the cap-validation path - 20's
"constant-time cap + generation check", which 21 forbids changing without a benchmark. Appending a
fixed record there is affordable; performing an IPC SEND there is not.

The others matter too, and in the same direction: with a ring the kernel does not know who reads,
when, or whether anyone does. That ignorance IS the property being bought. "Not a kernel
responsibility" is strongest when the kernel cannot even name its consumer.

### And "subscribe" still works, from the service's side

Nothing about the user-facing model changes. `events` drains on a timer, keeps a bounded RAM buffer,
serves it, and loses it on restart - exactly the stateless shape section 9 requires. The difference is
invisible from userspace and total from the kernel's: **pull, so the kernel never learns that `events`
exists.**

---

## 11. What would the new kernel responsibility be CALLED? (MISCIS)

The right answer is that **it must not need a name - and if it does, that is the signal to stop.**

And reporting should not BECOME a responsibility. It cannot be removed either - invariant 12 requires
loud failure and 11.4 mandates the floor precisely because a panic cannot ask a service - so the
achievable goal is to keep it **mechanism-only**: the kernel writes facts to a place and forms no
opinion about who reads them, when, or what they mean.

4.3 enumerates six kernel responsibilities: **M**emory, **I**PC, **S**cheduling, **C**apability,
**I**nterrupt, **S**MP. Every event proposed here is an event OF one of them:

| event | responsibility |
|---|---|
| capability denial | **C**apability |
| queue overflow, endpoint death | **I**PC |
| task killed, restarted, preempted | **S**cheduling |
| alloc denied | **M**emory |

None is a new THING THE KERNEL DOES. The ring changes how the kernel REPORTS what it already does.
`MISCISO` would be ugly, and the ugliness is diagnostic: **needing a seventh letter means the wrong
thing is being built.** That is a usable test for any future proposal, not just this one.

### But the kernel already HAS a responsibility outside MISCIS

Worth stating, because it was a surprise on checking: 4.3 enumerates six, and 11.4 separately
mandates a 16 KiB log ring, unconditional serial output, and the panic/boot framebuffer blit. None of
that is memory, IPC, scheduling, capability, interrupt or SMP. It is **REPORTING** - and the two
sections do not reference each other, so the kernel's reporting floor sits outside the enumeration
that is supposed to bound kernel scope.

11.4 justifies it, and the justification is strict - **IMPOSSIBILITY**:

> a panic halts every core, INCLUDING the `console` service, so it cannot ask a service to report it.
> Boot output has the same shape: it precedes every service, including the one that would render it.

That is the bar the console blit cleared and the operator control channel failed.

### So events are not a new responsibility - but the bar is higher than "useful"

Events would be the existing 11.4 reporting floor **encoded as records instead of text**. No new
letter, because reporting was never a letter; it is a floor justified separately and narrowly.

And it must clear 11.4's bar, not a lower one. Being honest about the gap:

> A capability denial is **"only the kernel KNOWS"**, not **"only the kernel CAN report"**. The denied
> service could say so itself - it simply will not, if it is the hostile one. That is weaker than a
> panic that halts every core.

So phase 2's justification is **weaker than the three-part test in section 6 makes it sound**, and
phase 1 has to do real work: demonstrate the blind spot MATTERS IN PRACTICE, not merely that it
exists. "An operator could not diagnose X without this" is the standard; "this would be nice to have"
is not.

If phase 1 cannot produce that evidence, the correct outcome is that the kernel gains nothing and
`events` reports what it can see. Recorded here so the bar is set BEFORE anyone is invested in
clearing it.

---

## 12. Amending C.2

Appendix C.2 is explicitly non-normative, so this note does not amend the constitution. But its
"known endpoint" phrasing should be read in light of the `trace` evidence: the kernel does not need
to publish to an endpoint, and the reasons it should not are the same ones that kept the trace ring
out of ring 0.

C.2's actual constraint - *"the kernel publishes; the metrics service interprets"* - is exactly right
and is preserved here. Only the transport changes: a drained ring rather than an endpoint.
