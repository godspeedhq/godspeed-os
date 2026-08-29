# `trace` - observing IPC call chains

**Status:** DESIGN, for review. No code written.
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

Per `0_conventions.md` §1: words not flags, `help` everywhere, `version`, raw facts.

```
trace                       usage (rule 1)
trace help                  usage
trace version               46_trace.md's version

trace blocked               every blocked task, what it awaits, and for how long
trace task <pid>            the blocked-chain rooted at one task
trace service <name>        the blocked-chain rooted at a service, by name

trace ipc                   [B] recent IPC events, newest last
trace tree <message-id>     [B] one request and everything it caused
trace failures              [B] recent ENDPOINT_DEAD / PEER_LOST / drops

trace on | off | status     [B] enable, disable, report the switch + drop count
```

`[B]` = needs the event ring. Everything unmarked works with mechanism A alone.

**`trace` takes service names, pids and numbers - never paths.** It goes in `NO_PATH_CMDS`
(conventions §1.9), so Tab at an argument position does nothing rather than listing the root directory.
Subcommand keywords complete at their position, wired in the same commit.

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

### `trace task 42` / `trace service fs`

The same walk, rooted at one task, printed as the tree the requirement asks for:

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

## 7. The one line I am not certain of: recording `op`

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

## 9. Open questions for review

1. **Is mechanism B wanted at all**, given A answers the stated question? B is where all the kernel
   growth is.
2. **`op`/`first_byte`** (§7) - record it, or refuse it on principle?
3. **Ring size**, if built. The log ring is 16 KiB; 32-byte events make 512 events per 16 KiB. At the
   IPC rates chaos produces, that is a fraction of a second of history - which may be exactly right for
   "what just happened", and useless for anything else. Worth deciding deliberately rather than picking
   a number.
4. **`FOR` (blocked duration)** needs a per-task blocked-since stamp. One `u64` per task, written on
   block and cleared on wake - two stores on a path that already does several. Acceptable, or is the
   duration column not worth it?
