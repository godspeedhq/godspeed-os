# `events` - what the sink RECORDED

Six views over the `events` service: the 192-event IPC trace ring, the metric table, the log window,
and capturing any of it to disk. Everything here is history the sink kept - ask twice and you get the
same answer, until the window moves.

For LIVE kernel state - what is stuck right now, who can reach whom - see `utilities/46_trace.md`.

**Status:** **BUILT and QEMU-verified.** The ring, the metric table, the log window, and
`events persist` with its `recorder` service.

```text
  events ipc                    what happened - the ring, paged and pipeable
  events failures               the same ring, only the failures
  events log [n]                the last n log lines the sink kept (default 20)
  events metrics                published samples: owner, metric, value, age
  events status                 ring size, recorded, dropped
  events persist start|stop|status   capture the log to disk
```

---

### `events persist` - keeping a capture when the screen cannot

```text
events persist start /log.txt                  everything, the default budget
events persist start /log.txt 7d               target a week
events persist start /log.txt 64MiB            an exact disk budget
events persist start /log.txt fs 7d sticky     only fs, a week, resumes after a reboot
events persist stop
events persist status
```

**Every budget carries a unit** - `h`/`d`/`w` for a duration, `KiB`/`MiB`/`GiB` for a size. Nothing is
bare, because a bare number would have to mean megabytes or minutes by convention and `16m` cannot be
read as either without guessing. That also removes the last heuristic from the parser: a token without
a unit is unambiguously a service name. `MB` is REFUSED rather than treated as `MiB` - they differ by
4.8%, and quietly giving less than was asked for is a small lie found late.

**A duration is a target, never a promise.** Converting `7d` to bytes needs a fill rate, and that is a
prediction about how chatty the machine will be - which the machine decides. So `status` reports the
MEASURED rate and what the budget actually covers at it:

```text
> events persist status
state      covers  kib_day  capacity_kib  lines  rotations  lost  path
recording  ~2d     11400    16384         842    0          0     /log.txt
```

Ask for a week on a box four times chattier than assumed and this says `~2d`, rather than letting you
discover it when the log you needed had already rotated away. `covers` reports the guaranteed FLOOR,
not the best case, because a rotation discards a whole piece.

**Rotation renames, so `/log.txt` is always the newest.** The previous piece is `/log.txt.1`. An
earlier version alternated between the two names, which meant the current file depended on a rotation
count only `status` could tell you - the file you wanted was a coin toss. Renames are metadata only, so
rotating costs no data movement however big a piece is.

Written by **`recorder`**, not by `events`, and that split is the design rather than tidiness. A file
write BLOCKS on a reply from `fs`; `events` is a single-threaded recv loop, so while it waited it would
stop draining its endpoint and drop the very events worth capturing. On a sick disk that is the full
deadline, repeatedly - `events` would go blind exactly when it matters. `recorder` blocks instead, and
nothing depends on `recorder`, so a stalled disk costs the capture and leaves the volatile window
readable.

**Spawned on demand**, never at boot, and not restarted on death - a respawned recorder would not know
its target and would be alive writing nothing while `status` said "running". The file opens with a
header and closes with a footer, so one without a footer says it died.

**Bounded, and not by counting.** `fs` allocates a file's whole extent up front, so the size is fixed
when the capture starts; two files rotate, so total disk use is twice that, forever. A forgotten
capture cannot fill a disk. It ROTATES rather than stopping because stopping keeps the wrong half - a
fixed file that stops when full preserves the start of a session and discards the crash at the end.

**Reading it back.** The file is written as `owner: text`, one line each, so `read /log.txt` shows a
log. It is NOT records, and that is a deliberate choice rather than an omission: the shell pipe
truncates a producer at 16 KiB (`CAP_MAX`) and every sink clips at 4 KiB, so a multi-megabyte capture
cannot survive a pipeline whatever format it is in. `read` is the tool that will actually consume it,
so the file is shaped for `read`. For records, use the LIVE window - `events log | to json` - which is
small enough to pipe.

**Zero-filled on creation, and it has to be.** `OP_WRITE_NEW` allocates the extent but writes no data
blocks, so everything past the last chunk written carried a stored CRC of zero and `fs` refused the
whole file - `read` returned `storage error` and the capture was unretrievable. Every file is written
through once at creation, which is the pause you see when a capture starts or rotates, and the reason
the default size is modest.

**`sticky`** records the capture in `/persist.conf`, a plain line the shell reads at the next boot and
resumes unattended, announcing it. Plain text on purpose: `read /persist.conf` shows exactly what will
happen, which is the difference between a setting and a surprise. An explicit `stop` deletes it, so the
one command meaning "enough" is not the one that fails to take. Pinned by `osdev test sticky`, which is
the only two-boot test here - a setting that survives a reboot cannot be proved inside one boot.

### `events log` - what was printed, after it has scrolled away

A framebuffer console has no scrollback. Before this, a line that scrolled past was gone unless you
had serial attached, which on a Pi wired to a TV you often do not.

```text
gsh> events log 4
fs: disk capacity = 0 sectors (0 MiB)
fs: serving file API
nic-driver: dwc2 answered op 0x01 while we asked 0x10 - not our reply (3 mismatched...)
block-driver: capacity 0 - the USB host service replied 1 byte(s), not a capacity
events: 13 line(s) logged, none overwritten. Lines printed BEFORE `events` started are on serial only.
```

**This is a COPY, and never the authoritative record.** `ctx.log()` performs its syscall first and
unconditionally - the kernel's 16 KiB ring and serial are written whether or not `events` is alive,
reachable, or has ever existed - and only then offers a duplicate for later querying. Re-pointing logs
AT the service would make observing a failure depend on a service that can fail, which is CLAUDE.md
§15's storage argument one layer up. Adding a copy takes nothing away from the floor.

Three consequences, stated rather than left to be discovered:

- **Nothing printed before `events` started is here.** Those lines are in the kernel ring, which no
  syscall exposes to userspace. Boot output is serial's job and always was.
- **The window is 8 KiB and wraps.** The footer says whether it has, so a truncated view never passes
  for a complete one.
- **A killed `events` loses the scrollback and no log output.** That is the design working: the tier
  beneath it survives anything, including a panic that halts every core.

It prints a screenful by default rather than everything it holds - about 3 KB on a booted machine is
more than anyone reads. `events log 100` asks for more.

**It is a RECORD SOURCE**, like every other view here - `owner` and `text`:

```text
gsh> events log | where owner=fs | to json
[
  {"owner": "fs", "text": "disk capacity = 0 sectors (0 MiB)"},
  {"owner": "fs", "text": "serving file API"}
]
```

The owner is a FIELD, which is what makes `where owner=dwc2` match a service that writes its lines as
`dwc2-svc:` - a text-prefix match could not, and guessing where a name ends inside a string is exactly
what a field removes. The name is stripped from `text` for the same reason: `owner` already holds it,
and stating one fact twice is what fields exist to stop.

Printing is a rendering, not the format. `events log` shows `owner: text` because a log should read
like a log and a 240-byte column would be wider than any screen; piped, it is rows. Same data, two
renderings - what `events ipc` already does.

This is a rule rather than a convenience, and it was ratified after being broken: the log was first
built as text, and filtering it by service then needed a bespoke argument hand-rolled in the shell,
duplicating `where` and getting it wrong on the first run. `docs/observability.md` §9a.

### `events metrics` - what services are counting

The ring answers "what just happened". `events metrics` answers "how much, so far", and it reads the
other half of the same service - the metric table in `events`.

```text
gsh> events metrics
owner   metric           value  age_s
fs      requests         32     1
fs      blk.outages      0      1
events  ring.recorded    139    0
events  ring.dropped     0      0
events  metrics.held     6      0
events  metrics.refused  0      0
```

| column | meaning |
|---|---|
| `owner` | the service that published it, as that service declared itself (`trace_as`). Part of the identity, not a label beside it: two services may both publish `requests` and they are different numbers. |
| `metric` | the name the publisher chose, up to 20 bytes. Longer is truncated, and the SDK says so once. |
| `value` | the LAST value published. A metric is a SET, not an increment - the owning service holds the counter and publishes what it currently reads. |
| `age_s` | seconds since the sink accepted it. **Read this column.** |

**Why `age_s` is not decoration.** `events` keeps a sample after its publisher dies - which is the
point, because the final value of a service that crashed is the one useful thing left to learn from it.
The cost of that is a number frozen at the moment of death being indistinguishable from a number being
maintained right now. The age is what tells them apart, and an instrument that cannot is worse than
none.

A publisher chooses its own interval, and should not publish on every operation: `fs` publishes every
32 requests, because a metric costing an IPC send per request would double the traffic it exists to
measure. So a value can lag by up to one interval on a quiet machine, and the age shows the lag rather
than implying there is none.

**`msgs.received` comes free, from the SDK.** Every service that holds the cap publishes it without a
line of its own code, because it is counted in the SDK's three receive paths - the same place trace
emission lives, and for the same reason: it is the identical counter in every service, and ten
hand-placed copies is ten chances to place it wrong.

It publishes on the FIRST message as well as every 64th, which matters more than it sounds. Publishing
only on the interval left any service under 64 messages with no row at all, and **no row is
indistinguishable from dead** - which is the one question this metric exists to answer. Paired with
`age_s` it separates the two faults nothing else in the system separates: a `block-driver` still
receiving and failing, versus one whose count last moved forty seconds ago.

**Attribution depends on `ctx.trace_as`, and silently failed without it.** A service that never
declares its name publishes under a BLANK owner - and since the key is `(owner, metric)`, *every*
undeclared service collides into one row with its counters interleaving. That is exactly how it was
found: a single `msgs.received 1920` belonging to nobody, which turned out to be `console` plus nine
others. Every internal service now declares itself, an undeclared one renders as `?` rather than
blank, and the view warns underneath when a `?` row exists.

One name is too long and is reported rather than hidden: `PEER_LEN` is 12 bytes (sized for
`block-driver`) and `hw-enumerator` is 13, so it reads `hw-enumerato` and the SDK says so once at
start-up. Widening the field was the alternative and was rejected on cost - the dump is bounded by one
4 KiB message, so a 16-byte field drops this view from 110 events per screen to 95, and trading real
scrollback for four characters of a name is the wrong way round.

**It is a record source**, so it filters and formats like `events ipc`:

```text
events metrics | where owner contains fs
events metrics | select metric,value | to json
```

**What is NOT in here, and cannot be: `events` reporting its own death.** Its own rows are published by
writing straight into the table it already owns, never by sending itself a message - a send is itself a
reportable event, so a self-emit over IPC would feed the ring from the ring. That local write works
right up until the process stops, and then nothing in it can say so. The supervisor's death
notification and the kernel's unconditional serial write are what report that, and both sit beneath
this service rather than inside it (`docs/observability.md` §9).

`trace task <slot>` and `trace service <name>` were folded into `trace chain`: they printed identical
output from identical code while being named after the SUBJECT KIND, which made them look like two
things and made `trace service fs` read oddly beside `trace deps fs`. The argument disambiguates
itself - digits are a slot, anything else is a name.

#### `events ipc` and `events failures` - what happened

```text
seq  sec  caller  peer          op        outcome
9    0    shell   fs            write     REPLY
10   2    shell   net-stack     1         TIMEOUT
6    2    fs      block-driver  capacity  REPLY
```

One row per completed exchange, oldest first, newest last. `events failures` is the same ring filtered
to the failures.

| column | what the NUMBER or word actually is |
|---|---|
| `seq` | the CALLER'S OWN event counter, starting at 0 when that service started. Not global - a mixed dump interleaves several sequences and can look unsorted; it is not, rows are in ring order. A GAP is the one loss nothing else can see: an event whose send failed on a full queue never reached the ring, so `events status` reports 0 dropped while events are being lost |
| `sec` | when the RING recorded the row, counted from the oldest row shown, in whole seconds. NOT a latency, and not when the call was made. It exists to show a HOLE - forty rows in one second, then a four-second gap, and that gap is the stall |
| `caller` | who made the call, as that service declared itself (`ctx.trace_as`). `?` means a service traced without declaring a name |
| `peer` | who was called, by name. The emitter knew it, so no lookup is involved |
| `op` | the OPERATION, by name (`read`, `write`, `capacity`). A bare number is an opcode this shell has no table for - `net-stack`'s protocol, for instance |
| `outcome` | how it ended - see below |

Outcomes, and what each one tells you to do:

| outcome | means |
|---|---|
| `REPLY` | the peer answered |
| `TIMEOUT` | the peer did not answer within the caller's deadline. It is alive as far as the kernel knows |
| `PEER_LOST` | the send failed, or the peer died while the call was outstanding (`ReplyDead`) |
| `QUEUE_FULL` | the peer is ALIVE and its queue is full. Congestion, not absence - answering it by reacquiring a capability that was never stale is a bug this project has already paid for once |
| `ABORTED` | the user pressed `q`. Not a failure of anything |

A request still IN FLIGHT is not here: one row is written per exchange, when it ends. The live view of
something stuck is `trace blocked`.

---

#### `events status` - the ring itself

```text
trace: ring 192 events; 2 recorded; 0 DROPPED (oldest overwritten before being read)
trace: the ring lives in the `events` service - the kernel records nothing.
```

| number | what it counts |
|---|---|
| `ring N events` | capacity. Fixed, no heap |
| `N recorded` | events ever ACCEPTED by the sink since it last started. An `events` restart resets it |
| `N DROPPED` | events overwritten before anyone read them. **This cannot see the other kind of loss** - an event whose `try_send` failed never arrived, so it is invisible here and shows only as a gap in a caller's `seq` |

---

#### Who is traced at all

Only exchanges made through the SDK's request/reply calls, by services whose contract grants
`ipc_send = ["events"]`. Today that is `fs` and `shell`. Tracing is authority: visible in
`caps <service>`, revocable, and absent by default - not a global switch. A service holding no such
capability emits nothing, at a cost of one relaxed atomic load.

---

## 6. Mechanism B: the event ring, if it is built

**Event** - fixed 32 bytes, no allocation, nothing variable-length:

| Field | Width | Note |
|---|---|---|
| `timestamp` | u64 | the monotonic clock `observe` already uses (portable across arches) |
| `sender` | u16 | task slot |
| `receiver` | u16 | task slot, or 0 for none |
| `endpoint` | u32 | endpoint id |
| `generation` | u32 | **the restart-identity the requirement asks for** - `nvme:9` vs `nvme:15` falls out |
| `message_id` | u32 | kernel-assigned, monotonic, wraps loudly |
| `event` | u8 | SEND / RECEIVE / REPLY / BLOCKED / ENDPOINT_DEAD |
| `op` | u8 | byte 0 of the message, opaque - see §7 |

**Bounded** (§26.6): a fixed ring in `.bss`, sized once. Full = **drop the oldest, count the drop**, and
`events status` reports the count. A silent drop is exactly the bug just fixed in the x86 input ring;
this must not repeat it (invariant 12).

**Near-zero when off**: one `Relaxed` load of an `AtomicBool` at the routing point, branch not taken.
**This must be measured, not asserted** (§20 - perf claims require a benchmark). The IPC fast path is
the one place this project pays for cost, so `B1`/`B2` (IPC round-trip latency) must be run with
tracing off and compared to baseline. If it is not free, it does not ship.

**`trace tree <message-id>`** is the expensive one, and it needs saying plainly: linking `shell -> fs`
to the `fs -> block` call it caused requires the **parent id to propagate** - the SDK must carry "the
request I am currently handling" and stamp it on outgoing calls. That is a change to every service's
request path, and it is the same problem `docs/net-tags-design.md` already describes for
net-stack <-> nic-driver. **Proposed: out of v1**, and re-read that design first if it is ever picked
up, because doing it twice differently would be worse than not doing it.

Without propagation, `events ipc` still gives a time-ordered log per endpoint, and the requirement's
failure/recovery example works, because that one is a **time sequence, not a tree**:

```
10:41:02.126 xhci:9   ENDPOINT_DEAD
10:41:02.127 supervisor -> restart xhci
10:41:02.130 xhci:15  READY
10:41:02.131 block-driver:4  req=55 PEER_LOST
```

`xhci:9` vs `xhci:15` is the generation, which the ring records for free.

---

## 7. The one line I am not certain of: recording `op`  *(RESOLVED - and the premise was WRONG)*

> **As built:** the uncertainty below was justified and its factual premise was false. Both busy
> protocols put something else at byte 0: `shell -> fs` and `fs -> block-driver` each PREPEND a
> one-byte correlation tag (added so replies are not matched by arrival order, after `fs` accepted one
> block's data as another's). So a trace recording byte 0 showed REQUEST IDS in a column labelled
> "op" - `183`, `177`, `212` against `block-driver`.
>
> The answer was not to rename the column after the byte. It is that **the service which speaks the
> protocol declares where its opcode lives** - `ctx.trace_op_at("fs", 1)`, once at startup. The SDK is
> generic across peers and cannot know; the kernel may not interpret a payload at all; the service
> can, which is where this design already puts every other piece of protocol knowledge. The shell then
> renders the opcode by NAME (`read`, `write`, `capacity`), because a number needs a lookup table the
> reader does not have.
>
> The section below is kept as the reasoning that led there, per 1 (an amendment is ratified history).

`op` is byte 0 of the message. Every service protocol in this tree happens to put its opcode there
(`fs`, `block-driver`, the block IPC protocol). Recording it is what makes `events ipc` readable rather
than a wall of endpoint numbers.

**The case for:** the kernel stores a byte and attaches no meaning. It is exactly as opaque as the
`ResourceId` badge in §7.10, where the kernel routes something whose meaning only the owning service
knows.

**The case against:** "byte 0 is the opcode" is a *convention*, and the kernel recording it privileges
that convention - which is the thin end of understanding a protocol. A service that puts something else
in byte 0 gets a misleading trace.

**Proposed:** record it, name the field `first_byte` in the kernel and only call it `op` in the
utility, and state in the code comment that the kernel attaches no meaning to it. If that reads as
sophistry on review, drop it - `events ipc` is still useful without it, and the constitution is worth
more than a column.

---

## 8b. As built (mechanism B) - the ring is a SERVICE, and the kernel gained nothing

Section 6 above designed the ring **inside the kernel** and asked whether that was worth it. It was
not, and the answer arrived from the question "why can't the trace be a service?". It can, and the
service version is strictly better - so what shipped is not the design in §6. That section is kept as
ratified history: it is the reasoning that had to be worked through to see why it was wrong.

**What changed, and why each change is an improvement rather than a compromise:**

| §6 proposed (in the kernel) | As built (in userspace) |
|---|---|
| ring in kernel `.bss` | a fixed array in the `events` service |
| `sender`/`receiver` task slots | the peer's **NAME**, because the emitter knows it |
| `endpoint` + `generation` | not recorded - the name is what a reader wanted the endpoint to resolve to |
| kernel-assigned `message_id` | not assigned; see `trace tree`, still out of scope |
| a switch + `AtomicBool` on the IPC fast path | **nothing on the IPC path at all** |
| a control syscall to arm/read it | ordinary IPC to a service |
| `first_byte`, opaque to the kernel (§7's uncertainty) | `op`, interpreted by the service that OWNS the protocol - so §7's dilemma simply does not arise |

**The peer is a name, and that is the whole reason this belongs out here.** The kernel may not know
what a message means (§4.4, §26.10), so a kernel ring could only ever have recorded `endpoint 116,
op 11` and left the reader to resolve it. The emitter called `request_with_reply("fs", ...)`: it
already holds the name, and putting the name in the event is what produces the symbolic output the
requirement asked for. The constraint the constitution imposes and the feature the requirement wanted
point the same way - which is usually the sign the design is right.

(The first attempt was a `tracer` service of its own. The enforcement layer refused it, correctly: the
kernel holds a `service_config` per service, pinned as debt that may only shrink, so even a userspace
ring would have cost ring 0 three lines. `events` is already in every one of those lists and its whole
purpose is diagnostic data, so the ring there costs the kernel **exactly zero**.)

**Layers touched:**

| Layer | Change |
|---|---|
| `sdk/rust/src/trace.rs` | wire format (34 B: seq, at_s, caller[12], peer[12], op, kind), lazy one-time arming, per-peer opcode offset |
| `sdk/rust/src/service_context.rs` | `trace_emit`; three emission points in `request_with_reply` |
| `services/events/src/main.rs` | the 192-event ring, dump + status replies |
| `services/shell/src/main.rs` | `events ipc`, `events failures`, `events status` |
| **kernel** | **nothing** |

**Cost when not tracing: one `Relaxed` load, branch not taken** - and nothing at the routing point, so
`B1`/`B2` cannot move. A service traces only if its contract granted it `ipc_send = ["events"]`, so
tracing is **authority**: visible in `caps <service>`, revocable, absent by default (§3.1). §6 asked
for a benchmark before shipping a switch on the fast path; there is no switch and no fast-path write,
so the thing that needed measuring does not exist.

**Bounded and loud (§26.6, invariant 12):** fixed ring, fixed events, no heap. Full = overwrite the
oldest and **count it**; `events status` reports the count. Emission is `try_send` with the result
discarded, so a full sink queue costs the emitter nothing and loses one event - the right trade on
an observability path, and the opposite of the one made on a correctness path. An observer must never
be able to slow, block or break the thing it observes.

**The reader is not recorded.** A call to `events` itself is never traced: `events ipc` reaches the ring
by asking the service that holds it, so tracing those calls would fill the ring with the reader's own
questions, two per dump, pushing out the traffic the reader came to see. Every other peer is still
recorded and `events status` still counts every accepted event, so nothing is hidden.

**Time is shown relative to the oldest event in the dump.** The stored value is an epoch second; the
absolute number says nothing on its own, and the **gap** between events is what a stall looks like.

Verified in QEMU (`osdev test shell`, 140/0/2 - 10 of them `trace`):

```
gsh> events status
trace: ring 192 events; 2 recorded; 0 DROPPED (oldest overwritten before being read)
trace: the ring lives in the `events` service - the kernel records nothing.
gsh> events ipc
seq  t+s  peer       op  event
0    0    net-stack  2   REQUEST
1    0    net-stack  2   REPLY
```

Real traffic, named, with no kernel involvement of any kind.

### What hardware caught that QEMU did not: the ring was empty

The first cut instrumented `request_with_reply`. On the Wyse, after a `selfcheck` and two `ls`
commands, `events ipc` still said **no events recorded** - while `fs` was demonstrably answering.

The SDK has **eight** request/reply variants, each an independent implementation, and the shell talks
to `fs` through `_abortable` and `_deadline` - never the plain one. So nothing was ever emitted for
the traffic that matters. QEMU had not caught it because the single event it did record came from the
one `net-stack` call site that happens to use the plain variant, and one green row looked like
success.

The fix is a **wrapper per variant** rather than an edit inside each body: eight functions with
several early returns inside a wait loop each, and a wrapper cannot miss an exit because the value
returned IS the outcome. All eight, not the busy ones - **partial instrumentation is worse than
none**, because `events ipc` then shows SOME traffic and silently omits the rest with nothing on screen
saying which, which is a silent gap in the instrument built to prevent silent gaps (26.4,
invariant 12).

Two event kinds fell out of doing it completely, because two of the variants already distinguish
outcomes the original four kinds could not express:

- **`QUEUE_FULL`** - the peer is ALIVE and its queue is full. Congestion, not absence. Recording that
  as a lost peer is a bug this project has already paid for once (`net-stack` reacquiring a capability
  that was never stale, backing DHCP off 34 s on a healthy link), so the ring keeps them apart.
- **`ABORTED`** - the user pressed `q`. Not a failure of anything, and recorded so a gap in a chain is
  explained rather than mysterious.

`events failures` shows `QUEUE_FULL` (a request that never arrived) and not `ABORTED` (a change of
mind).

### What the SUITE caught that neither had: the observer was stealing the shell's core

Instrumenting all eight variants took `osdev test files` from 222/0 to 213/9 - tab completion, `move`
and `find` timing out, and Enter keystrokes being lost so commands ran together. Four experiments, in
order, because the first three theories were all wrong:

| Experiment | Result | Ruled out |
|---|---|---|
| wrappers + tracing | 213/9 | - |
| wrappers marked `#[inline]` | 212/10 | the extra frame and the 4 KiB `Message` move |
| wrappers with the trace calls removed | **222/0** | the wrapper structure itself |
| the clock read removed from emitter AND sink | 213/9 | the CMOS RTC read |
| `events` moved off core 0 | **222/0** | - the answer |

**Every trace event WAKES the sink, and the sink was on core 0 with the shell.** Two events per `fs`
request preempted the shell twice per round trip; the paths that do many round trips blew their
window, and the shell was descheduled often enough to stop draining the console.

Three things changed as a result, and each is a rule worth keeping:

1. **The sink does not live on the interactive core.** ARM had already moved `events` off core 0 for a
   different reason (keeping the serial writer away from the microframe-timed USB driver); x86 now
   needs it for this one. An unavailable preferred core falls back to round-robin rather than failing
   the spawn, so a machine with fewer cores still gets its events.
2. **One event per exchange, not two.** A REQUEST plus an outcome doubled the traffic through the
   sink's single 16-deep endpoint. Every exchange still produces exactly one event carrying its fate,
   so nothing is lost but the duplicate. A request still IN FLIGHT is therefore not in the ring - and
   that is the one question the ring was never the right instrument for: `trace blocked` reads it from
   the kernel, live (mechanism A).
3. **The sink stamps the time, from a cached clock.** `epoch_secs_monotonic` is a CMOS RTC read on
   x86 - `wait_update_clear` can spin ~1 ms before seven port-I/O reads - so per-event it would cap
   the sink near a thousand events a second. `events` reads the cycle counter per event (one
   instruction) and refreshes the seconds only when one has actually elapsed.

The reader also **retries** a busy sink three times before reporting it, because a full queue is
congestion, not absence, and saying "unavailable" about a service that is alive and busy is a lie the
same size as any other silent fallback.

**`eseq` is the emitter's own counter**, not a global one, so a mixed dump interleaves several
sequences and looks unsorted. It is not - rows are in ring order, oldest first. The column earns its
place by making one service's dropped events visible as a gap in its own numbering.

### A row is an EDGE, not a fact: the caller

`peer` alone answers "who was called" and leaves "by whom" to inference - and that inference only
works while exactly two services hold trace authority. So an event carries the CALLER too, and a dump
reads as a call graph:

```
seq  sec  caller  peer          op   outcome
9    0    shell   fs            4    REPLY
10   2    shell   net-stack     1    TIMEOUT
6    2    fs      block-driver  183  REPLY
```

**The caller is self-declared** (`ctx.trace_as("fs")`, once at startup), and that deserves the
argument rather than an apology:

- A service **cannot ask** what it is called. There is no name in its context page and no query for
  one, because identity is not ambient (3.1). This is the capability model being consistent, not a
  gap.
- The kernel **does** know, unforgeably: every syscall send stamps `Message.sender_ep`, the sender's
  primary endpoint. It exists for reply-matching and its comment ends "Kernel-internal: never crosses
  to userspace ... so no ABI change". Surfacing it would grow the syscall surface for a diagnostic,
  which is not a trade this project makes.
- The obvious objection - a self-declared name is a claim, not a fact - **does not survive contact
  with what the event already is.** A service holding `ipc_send = ["events"]` can already write any
  `peer` and any `outcome` it likes, because the whole event is its testimony. `caller` is exactly as
  trustworthy as the two fields beside it: as trustworthy as the service you granted trace authority
  to. It opens nothing.

A service that never declares itself reads `?`, which is the honest answer rather than a guess.

### Reading it: pages, pipes, and a legend

`events ipc` is a **record source**, like `status` or `ls` - one producer feeding three uses, rather
than a printer plus a serialiser that drift apart:

- **Console**: a grid, with a two-line legend above it.
- **Taller than the screen**: it pages, with `help`'s keys (up/down, space, `g`/`G`, `q`). The pager
  was `help`-shaped - it called `help_render_line` directly - and is now given a render closure, so
  the one screenful-at-a-time reader in the system is shared instead of copied.
- **Piped**: `events ipc | to json`, `| to yaml`, `| where caller=fs`, `| where outcome=TIMEOUT`,
  `| count`, `| sort`. The legend is console-only; a pipe carries records and nothing else.

A dump shows the newest **64** events - what a record `Table` holds - while the ring keeps 192.
Asking for more only produced "result exceeded the record bound - truncated" on every run, and a bound
announced every time is noise that trains you to ignore it. `events status` remains where the true ring
size and drop count live.

### The shape of the whole thing

Three questions, three sources, and the kernel is only in one of them.

```text
                    WHAT IS STUCK NOW            WHO CAN REACH WHOM            WHAT HAPPENED
                    trace blocked                trace deps                    events ipc
                    trace chain                  trace endpoint                events failures
                          |                            |                             |
                          v                            v                             v
                  +---------------+           +----------------+          +--------------------+
                  | kernel, LIVE  |           | kernel, LIVE   |          | events, a RING     |
                  | who awaits    |           | capability     |          | of past exchanges  |
                  | which endpoint|           | tables         |          |                    |
                  +---------------+           +----------------+          +--------------------+
                    read-only,                  read-only,                  written by SERVICES,
                    2 queries                   existing syscall            never by the kernel
```

The kernel records nothing. It answers two questions it already knew the answer to (which endpoint a
blocked task awaits, and which endpoint a task owns); everything historical is written by services
into a service.

### How an event gets into the ring

```text
   fs                                     kernel                    events
   |                                        |                          |
   |-- request_with_reply("block-driver") ->|                          |
   |                                        |-- deliver -> block-driver|
   |<------------- reply ------------------ |                          |
   |                                                                   |
   |   ONE event per exchange, describing how it ENDED:                |
   |   seq, caller="fs", peer="block-driver", op, outcome=REPLY        |
   |-- try_send (fire and forget, never blocks) --------------------->|
   |                                                                   |-- stamp the time
   |                                                                   |-- store in a 192 ring
   |                                                                   |   (full = drop oldest,
   |                                                                   |    and COUNT it)
   |                                                                   |
   shell -- TRACE_OP_DUMP ------------------------------------------->|
   shell <- the newest 64 events ------------------------------------ |
```

Three properties fall out of that picture, and each is deliberate:

- **The kernel is not on the path at all.** No ring in ring 0, no retention policy, no control
  syscall, and nothing added to the IPC fast path.
- **The emitter never waits.** `try_send` and the result is discarded: a full sink costs the emitting
  service nothing and loses one event, which the ring counts and `events status` reports.
- **A service that holds no `events` send cap emits nothing**, at the cost of one relaxed load.
  Tracing is authority, visible in `caps <service>` and revocable.

### Why the sink is not on the interactive core

```text
   BEFORE                                  AFTER
   core 0: shell  events                   core 0: shell
           ^^^^^  ^^^^^^                   core 2: events
           every event WAKES events,
           preempting the shell            the wake lands on another core
           twice per fs request

   osdev test files: 222/0 -> 213/9        osdev test files: 222/0
   tab completion timing out,              (nothing changed but the core number)
   Enter keystrokes lost
```

### What the same hardware run says about the ring's limits

`events failures` came back empty after a 100-round `chaos max-carnage` - 559 kills. That is the design
working, and worth stating rather than discovering later: **`events` was itself killed 51 times**, and
a restarted `events` starts with an empty ring. Emission is `try_send`, so events aimed at a dead sink
are lost too.

For the question this tool exists to answer - *why is this task not progressing, right now* - that is
correct, and no ring in a restartable service can behave otherwise. For *post-mortem of a storm that
killed the recorder 51 times*, it cannot help, and only persistence would - which the ring
deliberately is not (see the `events` header on why history that survives nothing is the right call).

**Still out of scope, unchanged:** `trace tree <message-id>` needs parent-id propagation through every
service's request path (§6, and `docs/net-tags-design.md` describes the same problem for
net-stack <-> nic-driver). It is not made easier or harder by this; it is simply not built.

---
