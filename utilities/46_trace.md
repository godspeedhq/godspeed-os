# `trace` - what the kernel is doing RIGHT NOW

Five views over LIVE kernel state, read through `task_stat` and `InspectKernel`. Nothing here is
recorded anywhere: ask again a second later and the answer may differ, which is the point.

For what was RECORDED - the IPC ring, metrics, logs, and capturing them to disk - see
`utilities/47_events.md`. The two commands read two different sources, which is why they are two
commands.

**Status:** **BUILT and QEMU-verified.** `trace blocked` / `chain` / `deps` / `endpoints` /
`endpoint`, exercised by `osdev test shell` and the selfcheck suite.

```text
  trace blocked                 what is stuck, everywhere
  trace chain <name|slot>       what one task is stuck behind, as a tree
  trace deps <service>          what it can call, as a tree, with what it has called
  trace endpoints               every live endpoint and its owner - names to ids
  trace endpoint <id>           the inverse: who owns an endpoint, and who can reach it
```

`trace deps` and `trace endpoints` are record sources, so they filter and convert like any other:
`trace deps fs | to grid`, `trace endpoints | where name contains fs`.

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
| **B. Event history** (`events ipc`, `trace tree`, `events failures`) | a **bounded ring** written at the IPC routing point | one relaxed atomic load + predicted branch | a fixed ring + counters |

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
`-- events  (trace sink - its own traffic is never recorded)
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
| `(trace sink - its own traffic is never recorded)` | `events` always reads 0 calls: emissions to the ring are deliberately not traced, so this is the MOST used capability here, not an unused one |
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
1     events        102       BlockRecv  0
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

`events ipc` answers "what happened"; `trace chain` answers "what is stuck now". Neither answers the
question the capability model makes most worth asking - **who can reach whom** - so two views do:

```
gsh> trace deps net-stack            gsh> trace endpoint 4
net-stack                            endpoint 4 - NO LIVE OWNER.
|-- nic-driver                       held by:
`-- time                             holder  slot  rights
    `-- fs                           xhci    8     write
        |-- block-driver
        `-- events  (trace sink - its own traffic is never recorded)
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

## 9. Open questions for review

1. **Is mechanism B wanted at all**, given A answers the stated question? B is where all the kernel
   growth is. **ANSWERED: yes, and with zero kernel growth** - it is a service (§8b). The question
   that resolved it was "why can't the trace be a service?", and every objection to B was really an
   objection to B *being in the kernel*.
2. **`op`/`first_byte`** (§7) - record it, or refuse it on principle? **ANSWERED: the dilemma
   dissolved.** The recorder is the service that owns the protocol, and a service is entitled to
   interpret its own messages. Nothing privileges a convention inside the kernel, because the kernel
   is not involved.
3. **Ring size**, if built. **ANSWERED: 192 events (~4 KiB) in `events`.** Sized for "what just
   happened", which is the question a stalled chain asks - deliberately not for "what happened a
   minute ago", which needs either a much larger ring or filtering at the emitter, and
   filtering-in-the-middle is the first step toward putting a programmable VM where it does not
   belong. If longer history is ever wanted, the honest answer is a bigger arena HERE, costing one
   service more memory and the kernel nothing.
4. **`FOR` (blocked duration)** needs a per-task blocked-since stamp. One `u64` per task, written on
   block and cleared on wake - two stores on a path that already does several. Acceptable, or is the
   duration column not worth it?
