# `trace` - observing IPC call chains

**Status:** **Mechanism A BUILT** (`trace blocked` / `task` / `service`), QEMU-verified, 6 checks in
`osdev test shell`. Mechanism B (the event ring) is DESIGNED AND DELIBERATELY NOT BUILT - see §8.
**Version:** 0.1.0
**Pins:** §4.3 (kernel scope = the six), §4.4 (anti-scope), §8 (IPC), §8.6 (failure semantics),
§26.4 (no silent complexity), §26.6 (bounded), §26.10 (mechanism not policy), invariant 12 (loud).
**Conventions:** `utilities/0_conventions.md` §1.

---

## 1. The question

> **Why is this task not progressing?**

Everything below is judged against that sentence. A feature that does not help answer it is not in
this utility.

---

## 2. The finding that shapes the design

**The kernel already knows why a task is stuck.** It is not inferred, not reconstructed, not sampled -
it is recorded, right now, in the structures the reply-side death-wake (§8.6) needed:

| Fact | Where it already lives |
|------|------------------------|
| Which endpoint a task awaits a reply from | `ipc::routing::CALL_AWAIT_EP[slot]` - one entry per task, "bounded ... it never grows" |
| Which task is blocked on a full queue | `RoutingEntry::blocked_sender` |
| Whether a task is blocked, and how | `TaskState::BlockedOnRecv` / `BlockedOnSend` |
| Endpoint core, **generation**, liveness | the routing table (§8.3) |
| Endpoint -> name | the kernel name directory (`ipc::names`) |
| Task name, core, state | `task_stat` |

So the chain in the requirement's own example -

```
42 shell  -> fs:7 / req:88
7  fs     -> block:4 / req:901
4  block  -> nvme:9 / req:55
```

- is a **walk of `CALL_AWAIT_EP`**: shell awaits fs's endpoint; look up which task owns that endpoint;
ask what *it* awaits; repeat. No trace buffer. No event history. No message ids. No propagation.

**This splits the utility into two mechanisms with radically different costs**, and the split is the
main proposal in this document:

| | Mechanism | Cost when unused | New kernel state |
|---|---|---|---|
| **A. Blocked-chain** (`trace blocked`, `trace task`, `trace service`) | a **state query** | **zero** - nothing runs until asked | one `u64` per task (blocked-since), for the `FOR` column |
| **B. Event history** (`trace ipc`, `trace tree`, `trace failures`) | a **bounded ring** written at the IPC routing point | one relaxed atomic load + predicted branch | a fixed ring + counters |

**A answers the question. B explains what led up to it.** A is cheap enough to always be available; B
is the part that needs a switch. Building A first is not a staging convenience - it is where the value
is, and it may be that B is never needed.

---

## 3. Where this sits against the constitution

The requirement says trace collection must not add a kernel responsibility outside **MISCIS** (Memory,
IPC, Scheduling, Capability, Interrupt, SMP). Taking that seriously:

**Mechanism A adds no responsibility at all.** It exposes state the kernel already maintains *for
correctness* - `CALL_AWAIT_EP` exists so a dead replier wakes its caller with `ReplyDead`. Reading it
is introspection, which already exists and is capability-gated (`INTROSPECT`,
`docs/introspection-capability.md`). This is the same move as `observe`: the kernel publishes, a
service interprets (§26.10).

**Mechanism B is a genuine addition, and should be argued rather than assumed.** The honest case:

- The kernel is *already* at the routing point holding exactly these values (§8.3: validate cap, look
  up endpoint, enqueue, IPI). Recording them adds no new knowledge - it writes down what it just
  computed.
- The precedent is the **kernel log ring** (§11.4): a bounded in-kernel buffer whose contents are
  observability, not correctness, drained by userspace. A trace ring is that shape exactly.
- It stays mechanism: `(sender, receiver, endpoint, generation, event)` are IPC facts. The kernel
  never learns what a message *means*.

The honest cost: it is more kernel code and more kernel state, in a project whose first rule is that
the kernel does not grow. **That is a real trade and it should be made deliberately, with A shipped and
in use first**, so the decision is informed by whether A turned out to be enough.

---

## 4. What the requirement's example asks for that the kernel MUST NOT provide

The example output is:

```
└─ ipc → fs.read("/etc/config")
   └─ ipc → block.read(lba=1824, count=4)
```

`fs.read`, `"/etc/config"`, `lba=1824`, `count=4` are **protocol interpretation**. The requirement
itself forbids the kernel from doing that, and it is right to - it is §4.4 and §26.10 both. The kernel
sees an opaque byte array.

What the kernel can honestly produce:

```
└─ ipc → fs:7 gen=3 op=11
   └─ ipc → block-driver:4 gen=1 op=2
```

Endpoint names come from the name directory (already there). `op` is **byte 0 of the message**, which
this document proposes the kernel records as an opaque `u8` and never interprets - see §7 for why that
is a defensible line and where it could still be argued.

Getting from `op=11` to `read` requires a **service-published decoder** - the requirement's own
"services may optionally expose symbolic names". That is real work in every service and it is the
right place for it: `fs` knows what 11 means; the kernel must not. **Proposed: not in v1.** `op=11` is
already a large step up from nothing, and the decoder can be added per service later without touching
the kernel.

---

## 5. Command surface

As shipped - seven views, each named for WHAT IT SHOWS rather than what you give it:

```text
  trace blocked                 what is stuck, everywhere
  trace chain <name|slot>       what one task is stuck behind, as a tree
  trace deps <service>          what it can call, as a tree, with what it has called
  trace endpoints               every live endpoint and its owner - the map from names to ids
  trace endpoint <id>           the inverse: who owns an endpoint, and who can reach it
  trace ipc                     what happened - the ring, paged and pipeable
  trace failures                the same ring, only the failures
  trace status                  ring size, recorded, dropped
```

`trace task <slot>` and `trace service <name>` were folded into `trace chain`: they printed identical
output from identical code while being named after the SUBJECT KIND, which made them look like two
things and made `trace service fs` read oddly beside `trace deps fs`. The argument disambiguates
itself - digits are a slot, anything else is a name.

### Reading the output: every command, every column, every number

The rest of this document is the reasoning. This section is the reference: what each command prints,
what each column holds, and what a number in it actually counts.

---

#### `trace blocked` - who is stuck on someone else

```text
gsh> trace blocked
no task is blocked on another task.
```

That sentence is the healthy answer, not an empty result. A service parked on its OWN endpoint is
idle - waiting for work - and is deliberately not listed. Only a task waiting on ANOTHER task appears:

```text
slot  name   blocked  awaiting  held_by
9     asker  call     116       reply-server
```

| column | what it holds |
|---|---|
| `slot` | the kernel scheduler slot of the blocked task (the same number `status` shows) |
| `name` | that task's service name |
| `blocked` | HOW it is waiting: `call` (a synchronous request awaiting its reply) or `recv` |
| `awaiting` | the ENDPOINT ID it is waiting on - an opaque kernel id, resolvable with `trace endpoint` |
| `held_by` | the service that OWNS that endpoint, i.e. the one that owes the answer |

`held_by` is the payoff: it turns "task 9 is blocked" into "task 9 is waiting on reply-server".

---

#### `trace chain <name|slot>` - the same, as a tree from one task

```text
gsh> trace chain asker
task 9 "asker" BlockRecv (call)
   awaiting endpoint 116
   `- task 8 "reply-server" BlockRecv (recv)
      root: awaits no task - blocked on its own endpoint, waiting for work
```

Read it downward: each line is who the line above is waiting for. The walk stops at a ROOT - a task
waiting on nobody - and says which kind of root it is:

| root line | means |
|---|---|
| `awaits no task - blocked on its own endpoint, waiting for work` | idle. The chain ends here because this service is fine |
| `awaits no task - it is runnable, so the chain is not stuck here` | running right now |

The argument is a service NAME or a task SLOT; digits are read as a slot. `trace chain 7` and
`trace chain shell` are the same query.

---

#### `trace deps <service>` - what it can call, and what it has called

```text
gsh> trace deps fs
fs
|-- block-driver  27 calls  (capacity)
`-- logger  (trace sink - its own traffic is never recorded)
(2 reply address(es) hidden - `trace deps fs | where peer contains reply` lists them)
```

Indentation is the call direction: a child is a service its parent **holds a SEND capability to**.
This is read from the live capability table, so it is what the service can do right now - not what a
contract declared.

| what you see | what it means |
|---|---|
| `27 calls` | how many exchanges with that peer are STILL IN THE RING. A recent window, never a lifetime total. `0` means "not in the last 64 events", never "never" |
| `(capacity)` | the distinct operations seen, by name. A bare number is an opcode this shell has no name for |
| `4 FAILED` | of those calls, how many ended in anything other than a reply |
| `(trace sink - its own traffic is never recorded)` | `logger` always reads 0 calls: emissions to the ring are deliberately not traced, so this is the MOST used capability here, not an unused one |
| `reply#119` | a send capability whose endpoint no live task owns - a return address, not a dependency. Counted below the tree rather than drawn in it |
| `stopped at N edges` | the walk hit its bound; the graph is larger than the view |

As records (`| to grid`), one row per EDGE:

| column | what it holds |
|---|---|
| `depth` | 1 for a direct peer, 2 for a peer of that peer, and so on |
| `parent` | the service that holds the capability |
| `peer` | the service it points at |
| `grant` | `grantable` if the cap carries GRANT. That means EITHER a supervisor-wired peer OR a reply address - the two are indistinguishable from userspace, so the ambiguity is reported rather than guessed |
| `calls` / `failed` | as above |
| `ops` | space-separated distinct operation names, or `-` |

---

#### `trace endpoints` - the map from names to ids

```text
gsh> trace endpoints
slot  name          endpoint  state      queue
0     supervisor    100       BlockRecv  0
1     logger        102       BlockRecv  0
6     fs            112       BlockRecv  0
```

The inventory the other views assume you have. Ids reach you one at a time from `caps <service>` (as
`endpoint#N`), from `trace blocked`'s `awaiting` column and from `trace deps`' reply list - none of
which answers "what endpoints exist", so `trace endpoint <id>` could not be used deliberately without
first assembling this by hand.

| column | what it holds |
|---|---|
| `slot` | the task's scheduler slot, as `status` shows it |
| `name` | the service |
| `endpoint` | the id to pass to `trace endpoint <id>` |
| `state` | the task state - `BlockRecv` here means idle, waiting for work |
| `queue` | messages WAITING in that endpoint. Non-zero on an idle service means work is arriving faster than it drains |

Only PRIMARY endpoints appear, because that is what the kernel reports per task. A reply-only mailbox
has no name here - which is exactly why an unresolvable capability shows as `reply#NNN` in a
dependency tree rather than as a service.

A record source, so it filters and pipes: `trace endpoints | where name contains fs | to json`.

---

#### `trace endpoint <id>` - what an endpoint is, and who can reach it

```text
gsh> trace endpoint 116
endpoint 116 - owned by task 8 "reply-server" (BlockRecv)
held by:
holder     slot  rights
asker      9     send
supervisor 0     send grant
```

The inverse of `deps`: that one says who a service calls, this says who holds authority over a given
endpoint. Three shapes of answer:

| first line | means |
|---|---|
| `owned by task N "name" (state)` | a live service's endpoint |
| `console_push (id 4) - a kernel resource, not an endpoint` | ids 1-5 are stable kernel resources (`log_write`, `spawn`, `console_read`, `console_push`, `introspect`). They have no owning task by design |
| `NO LIVE OWNER` | either the owning task died, or it is a REPLY-only endpoint. A task's reply mailbox is not its primary one, and only primaries are named here |

| column | what it holds |
|---|---|
| `holder` | a live service holding a capability to this resource |
| `slot` | that holder's scheduler slot |
| `rights` | the rights on ITS copy, spelled out: `read write send recv grant revoke` |

`no live task holds a capability to it` is a complete answer, not a failure.

---

#### `trace ipc` and `trace failures` - what happened

```text
seq  sec  caller  peer          op        outcome
9    0    shell   fs            write     REPLY
10   2    shell   net-stack     1         TIMEOUT
6    2    fs      block-driver  capacity  REPLY
```

One row per completed exchange, oldest first, newest last. `trace failures` is the same ring filtered
to the failures.

| column | what the NUMBER or word actually is |
|---|---|
| `seq` | the CALLER'S OWN event counter, starting at 0 when that service started. Not global - a mixed dump interleaves several sequences and can look unsorted; it is not, rows are in ring order. A GAP is the one loss nothing else can see: an event whose send failed on a full queue never reached the ring, so `trace status` reports 0 dropped while events are being lost |
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

#### `trace status` - the ring itself

```text
trace: ring 192 events; 2 recorded; 0 DROPPED (oldest overwritten before being read)
trace: the ring lives in the `logger` service - the kernel records nothing.
```

| number | what it counts |
|---|---|
| `ring N events` | capacity. Fixed, no heap |
| `N recorded` | events ever ACCEPTED by the sink since it last started. A logger restart resets it |
| `N DROPPED` | events overwritten before anyone read them. **This cannot see the other kind of loss** - an event whose `try_send` failed never arrived, so it is invisible here and shows only as a gap in a caller's `seq` |

---

#### Who is traced at all

Only exchanges made through the SDK's request/reply calls, by services whose contract grants
`ipc_send = ["logger"]`. Today that is `fs` and `shell`. Tracing is authority: visible in
`caps <service>`, revocable, and absent by default - not a global switch. A service holding no such
capability emits nothing, at a cost of one relaxed atomic load.

---

### `trace blocked` - the whole point, in one screen

```
gsh> trace blocked
PID  NAME          STATE        WAITING ON              FOR
42   shell         BlockedCall  fs:7 gen=3              12ms
7    fs            BlockedCall  block-driver:4 gen=1     11ms
4    block-driver  BlockedCall  xhci:9 gen=2             10ms
9    xhci          BlockedRecv  (its own endpoint)       10ms

4 blocked. Longest chain: shell -> fs -> block-driver -> xhci (root: xhci, waiting on its own endpoint)
```

The last line is the answer to the question, and it is **derived, not editorialised** (rule 7): the
chain is a fact about `CALL_AWAIT_EP`, and "root" is simply the end of the walk.

Two failure shapes it must name rather than hide:

- **A cycle** (A awaits B, B awaits A - §8.9 says the kernel does not detect deadlock, so the *utility*
  reporting one is exactly right): print the cycle and stop, never loop.
- **A dead endpoint** - the awaited endpoint's generation no longer matches, or liveness is Dead. That
  is a task about to be woken with `ReplyDead`, and saying so distinguishes "stuck" from "already
  losing".

### `trace task 42` / `trace service fs`  *(as PROPOSED; shipped as `trace chain`)*

The same walk, rooted at one task, printed as the tree the requirement asks for. Two subcommands were
proposed here, one per subject kind - they shipped as a single `trace chain <name|slot>`, because they
printed identical output from identical code and naming them after the SUBJECT rather than the VIEW
made `trace service fs` read oddly beside `trace deps fs`. The sketch below also shows a per-hop
duration, which was not built: the kernel does not stamp when a task blocked, and adding that stamp is
kernel growth for a diagnostic. See the reference section above for what it actually prints.

```
gsh> trace task 42
task 42 "shell"  BlockedCall 12ms
└─ awaiting fs:7 gen=3          (task 7 "fs", BlockedCall 11ms)
   └─ awaiting block-driver:4 gen=1   (task 4 "block-driver", BlockedCall 10ms)
      └─ awaiting xhci:9 gen=2        (task 9 "xhci", BlockedRecv 10ms)
```

A task that is **not** blocked prints one line saying so. That is a real answer, not an empty result:
"it is running" is the correct response to "why is it not progressing".

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
`trace status` reports the count. A silent drop is exactly the bug just fixed in the x86 input ring;
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

Without propagation, `trace ipc` still gives a time-ordered log per endpoint, and the requirement's
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
(`fs`, `block-driver`, the block IPC protocol). Recording it is what makes `trace ipc` readable rather
than a wall of endpoint numbers.

**The case for:** the kernel stores a byte and attaches no meaning. It is exactly as opaque as the
`ResourceId` badge in §7.10, where the kernel routes something whose meaning only the owning service
knows.

**The case against:** "byte 0 is the opcode" is a *convention*, and the kernel recording it privileges
that convention - which is the thin end of understanding a protocol. A service that puts something else
in byte 0 gets a misleading trace.

**Proposed:** record it, name the field `first_byte` in the kernel and only call it `op` in the
utility, and state in the code comment that the kernel attaches no meaning to it. If that reads as
sophistry on review, drop it - `trace ipc` is still useful without it, and the constitution is worth
more than a column.

---

## 8. Proposed order

1. **Mechanism A** - `trace blocked`, `trace task`, `trace service`. New kernel surface: two
   `InspectKernel` queries (awaited endpoint per slot; blocked-since per slot) plus one `u64` per task.
   No ring, no switch, no cost. Answers the question.
2. **Use it.** Find out whether B is needed. §26.2 - features are pulled into existence.
3. **Mechanism B**, only if A proved insufficient, and only with the IPC benchmark showing the
   disabled path unchanged.

---

## 8a. As built (mechanism A)

What it cost, end to end:

| Layer | Change |
|---|---|
| `ipc/routing.rs` | one safe reader, `call_await_endpoint` - no `unsafe`, `ipc/` stays unsafe-free |
| `task/scheduler.rs` | `TaskStatRaw.endpoint`, populated from a value `task_stat` **already loaded** for `queue_depth` |
| `syscall/dispatch.rs` | two match arms (queries 24, 25), INTROSPECT-gated by falling outside the ungated list |
| `sdk` | two wrappers |
| `shell` | `cmd_trace` + completion + `NO_PATH_CMDS` |

**No new kernel state, no new kernel behaviour, no switch, and nothing on the IPC fast path.**

The enforcement layer caught the queries and refused the build until they were pinned with a written
answer to "why isn't this a service?" - which is the guard working as intended, and the answer is now
in `COMMANDMENTS.baseline.toml` beside the pin. The short form: query 25 passes on **impossibility**
(a task that is stuck is precisely the one that cannot tell a service it is stuck, so the kernel is
the only possible source), and 24 is disclosure of a read `task_stat` already performs.

Verified in QEMU (`osdev test shell`, 136/0/2):

```
gsh> trace blocked
no task is blocked on another task.
gsh> trace service shell
task 7 "shell" Running (-)
   root: awaits no task - it is runnable, so the chain is not stuck here
gsh> trace service nosuchsvc
trace service: no live task named 'nosuchsvc'
```

**The multi-hop walk is proven** - `osdev test trace`, 10/10. A healthy machine has nothing blocked, so
that test builds a chain that IS stuck: the reply-test build already has `asker` send `reply-server` a
request it is built never to answer, then block in a synchronous `Call`. No new service and no new
build feature were needed - the situation already existed, and had only ever been used to prove the
death-wake. Read live, mid-block:

```
gsh> trace blocked
slot  name   blocked  awaiting  held_by
9     asker  call     116       reply-server

gsh> trace service asker
task 9 "asker" BlockRecv (call)
   awaiting endpoint 116
   `- task 8 "reply-server" BlockRecv (recv)
      root: awaits no task - blocked on its own endpoint, waiting for work
```

That is the question answered in three lines: asker is stuck in a call on endpoint 116, reply-server
holds it, and reply-server is not waiting on anyone - it is sitting on its own endpoint. The endpoint
-> owner resolution across tasks is the part one hop can never demonstrate, and it is what this test
exists for. `reply-dead` still passes 5/5, so reading a stuck chain changes nothing about it.

## 8b. As built (mechanism B) - the ring is a SERVICE, and the kernel gained nothing

Section 6 above designed the ring **inside the kernel** and asked whether that was worth it. It was
not, and the answer arrived from the question "why can't the trace be a service?". It can, and the
service version is strictly better - so what shipped is not the design in §6. That section is kept as
ratified history: it is the reasoning that had to be worked through to see why it was wrong.

**What changed, and why each change is an improvement rather than a compromise:**

| §6 proposed (in the kernel) | As built (in userspace) |
|---|---|
| ring in kernel `.bss` | a fixed array in the `logger` service |
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
ring would have cost ring 0 three lines. `logger` is already in every one of those lists and its whole
purpose is diagnostic data, so the ring there costs the kernel **exactly zero**.)

**Layers touched:**

| Layer | Change |
|---|---|
| `sdk/rust/src/trace.rs` | wire format (34 B: seq, at_s, caller[12], peer[12], op, kind), lazy one-time arming, per-peer opcode offset |
| `sdk/rust/src/service_context.rs` | `trace_emit`; three emission points in `request_with_reply` |
| `services/logger/src/main.rs` | the 192-event ring, dump + status replies |
| `services/shell/src/main.rs` | `trace ipc`, `trace failures`, `trace status` |
| **kernel** | **nothing** |

**Cost when not tracing: one `Relaxed` load, branch not taken** - and nothing at the routing point, so
`B1`/`B2` cannot move. A service traces only if its contract granted it `ipc_send = ["logger"]`, so
tracing is **authority**: visible in `caps <service>`, revocable, absent by default (§3.1). §6 asked
for a benchmark before shipping a switch on the fast path; there is no switch and no fast-path write,
so the thing that needed measuring does not exist.

**Bounded and loud (§26.6, invariant 12):** fixed ring, fixed events, no heap. Full = overwrite the
oldest and **count it**; `trace status` reports the count. Emission is `try_send` with the result
discarded, so a full logger queue costs the emitter nothing and loses one event - the right trade on
an observability path, and the opposite of the one made on a correctness path. An observer must never
be able to slow, block or break the thing it observes.

**The reader is not recorded.** A call to `logger` itself is never traced: `trace ipc` reaches the ring
by asking the service that holds it, so tracing those calls would fill the ring with the reader's own
questions, two per dump, pushing out the traffic the reader came to see. Every other peer is still
recorded and `trace status` still counts every accepted event, so nothing is hidden.

**Time is shown relative to the oldest event in the dump.** The stored value is an epoch second; the
absolute number says nothing on its own, and the **gap** between events is what a stall looks like.

Verified in QEMU (`osdev test shell`, 140/0/2 - 10 of them `trace`):

```
gsh> trace status
trace: ring 192 events; 2 recorded; 0 DROPPED (oldest overwritten before being read)
trace: the ring lives in the `logger` service - the kernel records nothing.
gsh> trace ipc
seq  t+s  peer       op  event
0    0    net-stack  2   REQUEST
1    0    net-stack  2   REPLY
```

Real traffic, named, with no kernel involvement of any kind.

### What hardware caught that QEMU did not: the ring was empty

The first cut instrumented `request_with_reply`. On the Wyse, after a `selfcheck` and two `ls`
commands, `trace ipc` still said **no events recorded** - while `fs` was demonstrably answering.

The SDK has **eight** request/reply variants, each an independent implementation, and the shell talks
to `fs` through `_abortable` and `_deadline` - never the plain one. So nothing was ever emitted for
the traffic that matters. QEMU had not caught it because the single event it did record came from the
one `net-stack` call site that happens to use the plain variant, and one green row looked like
success.

The fix is a **wrapper per variant** rather than an edit inside each body: eight functions with
several early returns inside a wait loop each, and a wrapper cannot miss an exit because the value
returned IS the outcome. All eight, not the busy ones - **partial instrumentation is worse than
none**, because `trace ipc` then shows SOME traffic and silently omits the rest with nothing on screen
saying which, which is a silent gap in the instrument built to prevent silent gaps (26.4,
invariant 12).

Two event kinds fell out of doing it completely, because two of the variants already distinguish
outcomes the original four kinds could not express:

- **`QUEUE_FULL`** - the peer is ALIVE and its queue is full. Congestion, not absence. Recording that
  as a lost peer is a bug this project has already paid for once (`net-stack` reacquiring a capability
  that was never stale, backing DHCP off 34 s on a healthy link), so the ring keeps them apart.
- **`ABORTED`** - the user pressed `q`. Not a failure of anything, and recorded so a gap in a chain is
  explained rather than mysterious.

`trace failures` shows `QUEUE_FULL` (a request that never arrived) and not `ABORTED` (a change of
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
| `logger` moved off core 0 | **222/0** | - the answer |

**Every trace event WAKES the sink, and the sink was on core 0 with the shell.** Two events per `fs`
request preempted the shell twice per round trip; the paths that do many round trips blew their
window, and the shell was descheduled often enough to stop draining the console.

Three things changed as a result, and each is a rule worth keeping:

1. **The sink does not live on the interactive core.** ARM had already moved `logger` off core 0 for a
   different reason (keeping the serial writer away from the microframe-timed USB driver); x86 now
   needs it for this one. An unavailable preferred core falls back to round-robin rather than failing
   the spawn, so a machine with fewer cores still gets its logger.
2. **One event per exchange, not two.** A REQUEST plus an outcome doubled the traffic through the
   sink's single 16-deep endpoint. Every exchange still produces exactly one event carrying its fate,
   so nothing is lost but the duplicate. A request still IN FLIGHT is therefore not in the ring - and
   that is the one question the ring was never the right instrument for: `trace blocked` reads it from
   the kernel, live (mechanism A).
3. **The sink stamps the time, from a cached clock.** `epoch_secs_monotonic` is a CMOS RTC read on
   x86 - `wait_update_clear` can spin ~1 ms before seven port-I/O reads - so per-event it would cap
   the sink near a thousand events a second. `logger` reads the cycle counter per event (one
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
  with what the event already is.** A service holding `ipc_send = ["logger"]` can already write any
  `peer` and any `outcome` it likes, because the whole event is its testimony. `caller` is exactly as
  trustworthy as the two fields beside it: as trustworthy as the service you granted trace authority
  to. It opens nothing.

A service that never declares itself reads `?`, which is the honest answer rather than a guess.

### Reading it: pages, pipes, and a legend

`trace ipc` is a **record source**, like `status` or `ls` - one producer feeding three uses, rather
than a printer plus a serialiser that drift apart:

- **Console**: a grid, with a two-line legend above it.
- **Taller than the screen**: it pages, with `help`'s keys (up/down, space, `g`/`G`, `q`). The pager
  was `help`-shaped - it called `help_render_line` directly - and is now given a render closure, so
  the one screenful-at-a-time reader in the system is shared instead of copied.
- **Piped**: `trace ipc | to json`, `| to yaml`, `| where caller=fs`, `| where outcome=TIMEOUT`,
  `| count`, `| sort`. The legend is console-only; a pipe carries records and nothing else.

A dump shows the newest **64** events - what a record `Table` holds - while the ring keeps 192.
Asking for more only produced "result exceeded the record bound - truncated" on every run, and a bound
announced every time is noise that trains you to ignore it. `trace status` remains where the true ring
size and drop count live.

### The shape of the whole thing

Three questions, three sources, and the kernel is only in one of them.

```text
                    WHAT IS STUCK NOW            WHO CAN REACH WHOM            WHAT HAPPENED
                    trace blocked                trace deps                    trace ipc
                    trace chain                  trace endpoint                trace failures
                          |                            |                             |
                          v                            v                             v
                  +---------------+           +----------------+          +--------------------+
                  | kernel, LIVE  |           | kernel, LIVE   |          | logger, a RING     |
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
   fs                                     kernel                    logger
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
  service nothing and loses one event, which the ring counts and `trace status` reports.
- **A service that holds no `logger` send cap emits nothing**, at the cost of one relaxed load.
  Tracing is authority, visible in `caps <service>` and revocable.

### Why the sink is not on the interactive core

```text
   BEFORE                                  AFTER
   core 0: shell  logger                   core 0: shell
           ^^^^^  ^^^^^^                   core 2: logger
           every event WAKES logger,
           preempting the shell            the wake lands on another core
           twice per fs request

   osdev test files: 222/0 -> 213/9        osdev test files: 222/0
   tab completion timing out,              (nothing changed but the core number)
   Enter keystrokes lost
```

### What `trace deps` reads, and what it cannot

```text
   contract (fs.toml)          kernel service_config          fs's LIVE cap table
   ipc_send = [...]     --->   send_peers = [...]      --->   [ SEND -> ep 42 ]
        |                            |                        [ SEND -> ep 51 ]
        |                            |                               |
   host-only:                  inside the kernel:               task_caps(slot)
   reconciled at build         not readable from a                    |
   time, never ships           service                                v
                                                              trace deps  <-- reads THIS
```

`trace deps` walks the right-hand column: capabilities a service is holding right now, each endpoint
resolved back to its owning task. The left-hand columns are what "declared" would mean, and neither is
reachable without new kernel surface - which is why that view waits for the supervisor to own service
policy.

### `trace deps` and `trace endpoint`: authority, in both directions

`trace ipc` answers "what happened"; `trace chain` answers "what is stuck now". Neither answers the
question the capability model makes most worth asking - **who can reach whom** - so two views do:

```
gsh> trace deps net-stack            gsh> trace endpoint 4
net-stack                            endpoint 4 - NO LIVE OWNER.
|-- nic-driver                       held by:
`-- time                             holder  slot  rights
    `-- fs                           xhci    8     write
        |-- block-driver
        `-- logger  (trace sink - its own traffic is never recorded)
(3 reply address(es) not shown: reply#119 reply#107 - `trace endpoint <id>` resolves one)
```

Both are built from the LIVE capability table (`task_caps`), not from a contract: a row is not "the
toml says it may call block-driver", it is "this service is holding, right now, a SEND capability
whose endpoint block-driver owns". That is 26.9 - authority inspectable as it actually stands.

**A tree and a record stream are the same data.** The table holds one row per EDGE
(`depth, parent, peer, grant, calls, failed, ops`), which is what a tree IS, so the console draws it
and a pipe gets the rows - `trace deps shell | where peer=fs`, `| to json`, `| to grid`. `to grid`
was added for this: the grid was every record source's console rendering but could not be ASKED for,
so a producer that draws something else had no way to offer the table.

Four bugs surfaced only because the tree draws structure a table hides, and each is a rule:

1. **A global "seen" set flattened it.** `block-driver` is a direct peer of the shell AND a peer of
   `fs`, so once drawn it was never drawn again - `fs 24 calls` had nothing beneath it. Distinctness
   of children and absence of cycles are DIFFERENT problems: dedupe the edge, guard the path.
2. **Edges were duplicated.** `time` is reached twice (directly and via `net-stack`) and each visit
   added its own `time -> fs` row; the renderer, which finds children by parent NAME, then drew both
   under every occurrence. The edge itself must be unique.
3. **The cycle guard silently failed.** A short name written over a longer one in an ancestor slot
   left `"stack"` trailing, so the match missed and the cycle expanded one level further.
4. **Filtering `SEND|GRANT` deleted real wiring.** A reply capability carries GRANT, so filtering it
   hid return addresses - and also hid every peer the SUPERVISOR provides at spawn, which carries
   GRANT because the supervisor must be able to re-delegate it. `net-stack` showed no `nic-driver`
   dependency at all, immediately after a ping that demonstrably used it. The two are
   indistinguishable from userspace, so they are included and the ambiguity is REPORTED (26.7):
   dropping them hides real dependencies, including them silently shows return addresses as
   dependencies, and an honest ambiguity beats a confident wrong answer in either direction.

**What it deliberately does NOT show: DECLARED dependencies.** A contract's `ipc_send` list lives in
`service_config` inside the kernel (the `.toml` never ships - `contract_check.py` reconciles it at
build time), so `trace deps <svc> declared` would need a new `InspectKernel` query: kernel-surface
growth for a diagnostic, which this project does not do. It is deferred to the work that moves the
service catalogue into the supervisor, after which the supervisor owns that policy and the query is
service-to-service with the kernel uninvolved.

### What the same hardware run says about the ring's limits

`trace failures` came back empty after a 100-round `chaos max-carnage` - 559 kills. That is the design
working, and worth stating rather than discovering later: **`logger` was itself killed 51 times**, and
a restarted logger starts with an empty ring. Emission is `try_send`, so events aimed at a dead sink
are lost too.

For the question this tool exists to answer - *why is this task not progressing, right now* - that is
correct, and no ring in a restartable service can behave otherwise. For *post-mortem of a storm that
killed the recorder 51 times*, it cannot help, and only persistence would - which the ring
deliberately is not (see the `logger` header on why history that survives nothing is the right call).

**Still out of scope, unchanged:** `trace tree <message-id>` needs parent-id propagation through every
service's request path (§6, and `docs/net-tags-design.md` describes the same problem for
net-stack <-> nic-driver). It is not made easier or harder by this; it is simply not built.

---

## 9. Open questions for review

1. **Is mechanism B wanted at all**, given A answers the stated question? B is where all the kernel
   growth is. **ANSWERED: yes, and with zero kernel growth** - it is a service (§8b). The question
   that resolved it was "why can't the trace be a service?", and every objection to B was really an
   objection to B *being in the kernel*.
2. **`op`/`first_byte`** (§7) - record it, or refuse it on principle? **ANSWERED: the dilemma
   dissolved.** The recorder is the service that owns the protocol, and a service is entitled to
   interpret its own messages. Nothing privileges a convention inside the kernel, because the kernel
   is not involved.
3. **Ring size**, if built. **ANSWERED: 192 events (~4 KiB) in `logger`.** Sized for "what just
   happened", which is the question a stalled chain asks - deliberately not for "what happened a
   minute ago", which needs either a much larger ring or filtering at the emitter, and
   filtering-in-the-middle is the first step toward putting a programmable VM where it does not
   belong. If longer history is ever wanted, the honest answer is a bigger arena HERE, costing one
   service more memory and the kernel nothing.
4. **`FOR` (blocked duration)** needs a per-task blocked-since stamp. One `u64` per task, written on
   block and cleared on wake - two stores on a path that already does several. Acceptable, or is the
   duration column not worth it?
