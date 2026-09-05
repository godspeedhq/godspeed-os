# Observability: metrics, logs and traces, without a kernel endpoint

**Status:** design note, now PARTLY BUILT. Section 9's rename shipped on 2026-09-04 (`logger` ->
`events`, commit 7dc157b0), together with the section 1 correction that prompted it. Everything from
section 10's phase 1 onward is still design. Recorded because the question came up and the answer was
already half-evidenced by work that had shipped.

**The question.** An observability service that serves metrics, logs and traces - does the kernel need
an endpoint to expose that data on?

**The answer: almost certainly not.** Appendix C.2 of `CLAUDE.md` says "the kernel emits structured
events ... on a known endpoint". That line predates the `trace` utility, which shipped with **zero
kernel growth** and is direct evidence against it.

---

## 1. Two thirds of this is already answered, and it needed no NEW kernel

| stream | where it lives today | kernel involvement |
|---|---|---|
| **logs** | syscall 5, straight to the kernel's 16 KiB ring + serial. **Not the service** | the EXISTING 11.4 floor; no new growth |
| **traces** | a 192-event ring INSIDE `events`; instrumentation in the SDK | none |
| **metrics** | see below | pull only, already exists |

The `trace` utility answers "what is stuck", "who can reach whom" and "what just happened", and the
kernel gained nothing for it: no ring in ring 0, no retention policy, no message-identity scheme, no
control syscall, no new capability, and nothing on the IPC fast path.

The design question that settled it was **"why can't the trace be a service?"** - and every objection
to putting a ring in the kernel turned out to be an objection to it being in the kernel.

### Correction 2026-09-04: logs never reach the `events` service at all

The logs row above used to read *"the `events` SERVICE; every service sends to it"*, with no kernel
involvement. **Both halves were wrong**, and the error survived a long time because it describes the
arrangement the NAME implies - which is the same reason section 9 wants the name changed.

What actually happens: `ctx.log()` is **syscall 5**, gated by the `log_write` capability slot, and it
writes the kernel's 16 KiB ring buffer **and serial, directly**. The service's contract has no
`log_write` at all; it declares only `ipc_receive`. Its endpoint is drained and unrecognised messages
are dropped (`services/events/CLAUDE.md`).

```
   TODAY - and the arrow that does NOT exist

   any service
   +----------------------+      syscall 5 (Log), gated by log_write
   |  ctx.log("ready")    |------------------------+
   |                      |                        |
   |  SDK trace record    |------ IPC ------+      |
   +----------------------+                 |      v
                                            |   +--------------------------------+
                                            |   |  kernel: 16 KiB ring + serial  |
                                            |   |  written UNCONDITIONALLY       |
                                            |   +--------------------------------+
                                            |                  |
                                            |                  |  drained ONCE,
                                            v                  v  at events start
                                     +------------------------------+
                                     |  events  (-> `events`)       |
                                     |    192-event trace ring      |
                                     |    endpoint: drained,        |
                                     |    unrecognised = dropped    |
                                     +------------------------------+

   There is no   ctx.log() ---> events   arrow. There never was one.
```

**Three consequences, and the first is the one that matters most:**

1. **Logging does not depend on the service being up.** When `events` is dead `ctx.log()` still
   works: it never blocks on it and never returns `EndpointDead` from it, so a chaos storm that kills
   `events` loses **no log output**. That is a property to protect, not an accident to tidy up.
2. **So logs must NOT be re-pointed at `events` when it is built.** Routing them through the service
   would make observing a failure depend on that service being alive - the same argument as section
   9's "must never depend on storage", one layer further up. The instinct to "wire our existing
   services to `events`" is right for metrics, right for traces, and **wrong for logs.**
3. **What genuinely wires to the service** is traces (`ipc_send = ["events"]` in a contract, which is
   a real IPC path that exists today) and metrics (new). Logs stay on the syscall floor.

This also sharpens section 9's tier table: the serial-and-kernel-ring floor is not a FALLBACK that
`events` degrades to, it is a **separate path `events` was never on**. That is why "the kernel keeps
writing to serial unconditionally" is load-bearing rather than reassurance.

**Two docs disagreed, and this one was wrong.** `docs/logging.md` has recorded the truth all along -
*"nothing currently logs THROUGH `events`. `ctx.log()` is syscall 5"* - while this note asserted the
opposite in its opening table. A reader who started here would have built the wrong model of the
system and had no reason to doubt it. Both are now consistent; if the log path ever changes, it
changes in both.

---

## 2. Metrics split in two, and only one half is a question

**Service-level metrics** (a service counting its own work) are not a kernel matter at all. The
service counts, and sends. This is the `events` shape again.

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
   kernel  --writes-->  [ bounded ring, overwrites when full ]  <--drains--  events
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
                                          ->  drained ONCE, when `events` starts
```

`kernel/src/log.rs` is a **byte ring of formatted, human-readable lines** - not typed records - and
`drain_to_events` has exactly one caller, `events`'s startup. After that everything reaches serial
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
trace ring into `events`, which turned out to be the better design anyway.

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
   |  events (ring)  <-|--query-|    a trace cap   |
   +-------------------+        +------------------+
                                        |
                                        v
                                 exposition format
                            (Prometheus / OTel / custom)
```

Everything it needs already exists or is a service-to-service concern. The kernel publishes what only
it knows, by the two mechanisms it already has, and forms no opinion about consumers.

---

## 9. The service WAS renamed: `logger` -> `events` (done 2026-09-04)

**The name was already wrong.** The service holds the 192-event IPC TRACE ring; it stopped being a
logger when `trace` shipped. So this was not a rename in anticipation of a new job - it was the name
catching up with the job it already had.

**Shipped in commit 7dc157b0**, verified in QEMU: identity 24/0, shell 165/0, commandments 15 pass,
redteam restored 0. Runtime evidence rather than a clean compile - `events: ready (drains its
endpoint; holds the IPC trace ring)`, `trace: ring 192 events; 82 recorded; 0 DROPPED`, and zero
occurrences of the old name anywhere in the serial log. The rest of this section is why, kept in
place because the reasoning is what a future reader needs.

**It is cheap, and the reason is the capability model working.** `ctx.log()` resolves through
`log_write_slot` - a CAPABILITY, not a name - so essentially every call site in the system is already
insulated from what the receiving service is called. Only ~46 literal `"events"` occurrences exist in
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
store**: a persisting events cycles through `fs`, and worse, **makes observing a storage failure
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

### The second constraint: self-observation must never take an IPC hop

**Can `events` observe itself?** Three ways yes, one way never, and the shape that would look most
natural to write is the one that turns recursion on.

```
   +---------------------------------------------------------------------+
   |  `events`, observing itself                                          |
   +---------------------------------------------------------------------+
   |                                                                      |
   |  its own logs     -- syscall 5 --> kernel ring + serial       OK     |
   |                      leaves the building entirely; never              |
   |                      touches its own endpoint, so no loop            |
   |                                                                      |
   |  its own metrics  -- local write --> its own table            OK     |
   |                      no IPC hop, therefore no recursion              |
   |                                                                      |
   |  its own traces   ----------------> its own ring              NOISY  |
   |                      it is the endpoint EVERYONE sends to, so        |
   |                      tracing its own recvs costs one record per      |
   |                      real event: self-noise at 1:1 with signal       |
   |                                                                      |
   |  its own DEATH    ------------------------------------------  NEVER  |
   |                      structurally impossible                         |
   +---------------------------------------------------------------------+
```

**The rule, stated before anyone codes it:**

> A service reporting on ITSELF writes locally or uses the syscall floor. It must never do so by
> sending itself a message.

A self-emit that takes an IPC hop is the one construction that feeds the ring from the ring: the
send is itself a traceable event, which produces a record, which is a send. Nothing else in the
design has this property, and it is invisible until the ring is full of itself. Self-emit is a
function call.

The logs row above is the pattern to copy, and the 2026-09-04 correction in section 1 is why it
already works: `ctx.log()` leaves for the kernel floor, so `events` logging about `events` never
touches its own endpoint. It gets self-observation for free by not being clever.

### Its own death is the one event it cannot report

Worth stating as a property rather than discovering it during a storm. The single most useful thing
`events` could tell you about `events` is the one thing it structurally cannot:

```
                        events dies
                             |
              +--------------+--------------+
              v                             v
      +----------------+        +---------------------------+
      |  supervisor    |        |  kernel ring + serial     |
      |  gets the      |        |  written unconditionally, |
      |  death notif,  |        |  survives a panic that    |
      |  respawns it   |        |  halts every core         |
      +----------------+        +---------------------------+

      Both sit BENEATH it. Neither routes through it. That is the
      whole reason the floor is not allowed to move.
```

So the watchman is watched by the two components that cannot die, and neither of them depends on the
one that can. This is not a gap to close - closing it (by persisting, or by having `events` guard
itself) is precisely the violation section 9 opens by forbidding.

### Timing - and what was actually done instead

The advice here was to do the rename **as part of** the observability work rather than before it,
since standalone it is churn with no behaviour change.

**It shipped first anyway, and the reason is worth recording.** The section 1 correction changed what
the rename MEANS: once it was established that logs never reached the service, the name was not merely
imprecise, it was actively misleading about the system's structure - it implied an arrow that does not
exist. That makes it a correctness fix to the codebase's own vocabulary, not cosmetic churn, and those
land on their own so the diff stays reviewable. The measured cost was 56 sites, not 46.

---

## 9a. The rule: every stream the sink serves is RECORDS, never free text

**Ratified 2026-09-05, after breaking it once.**

> A view served by `events` returns rows with named fields. It is queryable with the same `where` as
> every other view, and it converts with `to json` / `to yaml`. A view that can only be printed is not
> finished.

Metrics and IPC traces were built this way from the start - `owner, metric, value, age_s` and
`seq, sec, caller, peer, op, outcome`. The LOG was not: it was built as a byte ring of formatted
lines, because that is what a log looks like.

**The cost arrived immediately, which is why this is a rule and not a preference.** Filtering the log
by service needed a bespoke argument parsed in the shell (`events log 4 fs`), hand-rolling what `where`
already does - and the hand-rolled version was wrong on its first run, returning lines belonging to
other services. Two mechanisms for one operation, one of them broken, and the broken one written
because the data had the wrong shape.

As records the whole thing disappears:

```text
events log | where owner=fs | to json
[
  {"owner": "fs", "text": "disk capacity = 0 sectors (0 MiB)"},
  {"owner": "fs", "text": "serving file API"}
]
```

Three properties follow that the text form could not have had:

- **The owner is a FIELD, not a prefix.** `where owner=dwc2` matches even though that service writes
  its lines as `dwc2-svc:`. A text-prefix match could not, and deciding where a name ends inside a
  string is exactly the guesswork a field removes.
- **No fact is stated twice.** The service name lives in `owner`, so it is stripped from `text` -
  `{"owner": "fs", "text": "fs: serving file API"}` was the first output and is noise.
- **Printing is a RENDERING, not the format.** `events log` prints `owner: text` because a log should
  read like a log, and a 240-byte `text` column would be wider than any screen. Piped, it is rows.
  Same data, two renderings, exactly as `trace ipc` already does.

The rule binds anything added later. A new stream that can only be printed has picked the wrong shape,
and the filter someone writes for it will be the second mechanism this section exists to prevent.

---

## 9b. Persisting a capture, and where it may live

**Built 2026-09-05.** `events persist start|stop|status`, written by a separate `recorder` service.

The rule this obeys is the one `docs/logging.md` set: **`events` may hold bounded VOLATILE state and
must never acquire a durable-storage dependency.** The concrete reason turned out to be sharper than
the dependency diagram suggests - a file write BLOCKS on a reply, and a blocked `events` is not
draining its endpoint, so it drops the log copies, traces and metrics arriving while it waits. On a
sick disk that is the full deadline, repeatedly. It would go blind at the moment it is most needed.

`recorder` has the identical blocking problem and it does not matter, because nothing depends on it.
That is the whole argument for a separate service, and it is worth keeping in that form: the question
is never "does this component block" but "who else stops when it does".

**A future remote sink** (`events persist start <url>`) was raised and is not built. One thing to decide
deliberately before it is: credentials on a command line land in shell history AND in the event log
itself, which the recorder is at that moment writing to disk. Credentials looping through the thing
that captures them is a bad shape - it wants a credential file or a capability, not an argument.

---

## 10. The plan

Phased so that the kernel change is **pulled into existence** by a service that has demonstrably hit
the wall (26.2), not built ahead of one. Each phase is independently useful and independently
shippable.

### Phase 0 - build the service against what already exists - **DONE 2026-09-04**

Both halves shipped: the rename (section 9, `7dc157b0`) and the metrics stream (`b91cea82`).

Correct the headline first, because it was wrong: this was **not** "no kernel change at all". The
kernel carries hard-coded service-name lists (two in `task/scheduler.rs`, four in `build.rs`), so the
rename edited the kernel. No new responsibility and no growth - but a kernel SOURCE edit, and that is
the expensive kind, because it reopens the question of whether the kernel is still correct. What is
genuinely true is the claim worth making: **no new kernel responsibility, no new syscall, no new
capability, nothing on the IPC fast path.**

What a service now has, and the shape of each:

| stream | how | costs the emitter |
|---|---|---|
| **logs** | `ctx.log()`, syscall 5 to the kernel floor | works even when `events` is dead |
| **traces** | automatic in the SDK; needs only `ipc_send = ["events"]` | one relaxed load when not tracing |
| **metrics** | `ctx.metric(name, value)`, `try_send` and discard | nothing it can be blocked or slowed by |

Read back with `events metrics` (a record source, so it filters and formats like any other);
`trace` remains as the older name for every view.

**And logs became queryable without a kernel change**, which is worth recording because the obvious
route needed one. `drain_kernel_ring_buffer()` is a no-op stub and no syscall exposes the kernel's
16 KiB ring to userspace, so `events log` could not be built by draining it. What works instead:
`ctx.log()` performs its syscall FIRST and unconditionally, then offers a best-effort COPY to
`events`. That is not the re-pointing section 1 forbids - the floor still fires first and a dead
sink still loses no log output - it is a duplicate kept for querying. The limitation that follows is
stated in the view itself: lines printed before `events` exists are on serial only.

**Two things this phase settled that the plan above did not anticipate.**

*The self-observation rule needed no mechanism.* `events` publishes its own four rows by writing into
the table it already owns, and it cannot accidentally do otherwise: it holds no send cap to itself, so
`ctx.metric` resolves to `u32::MAX` and returns - the same cut that already stopped the sink tracing
its own sends. The rule in section 9 turned out to be a generalisation of something the SDK was
already doing deliberately, not a new constraint.

*Bounds bite immediately, and that is the system working.* The metric name field was first sized at
`PEER_LEN`, and the sink's own first metric (`ring.recorded`, 13 bytes) was silently truncated to
`ring.recorde`. Two fixes, both in the spirit of 26.6.1: give the field its own size (20) rather than
borrowing one meant for service names, and REPORT the truncation once, because a name quietly becoming
a different name would merge two metrics into one row with the values interleaving.

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
