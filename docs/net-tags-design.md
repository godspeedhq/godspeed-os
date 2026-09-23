# Design Spec: Correlation Tags Between net-stack and nic-driver

> **Status:** Direction agreed (2026-08-11). **Partly addressed 2026-08-18 WITHOUT the wire change;
> the tag itself is still not built.** See §6 for exactly what was done and what that leaves.
>
> Original status: not built. Written after the idle-link tick was
> reverted (`aa569bcc`) for the bug this fixes. Three phases, each independently testable; do them in
> order and verify on hardware between each.
>
> **Author intent:** net-stack serves clients and receives nic-driver replies on **one untagged
> endpoint**, so anything it asks the driver outside of serving a request can consume a client's
> message instead. That is not a tuning problem; it is a missing correlation, and until it is fixed
> net-stack cannot do ANY background work - no link watching, no periodic re-sync, no staying
> responsive during the DHCP dance.

---

## 1. The bug this fixes

net-stack owns exactly one endpoint. Two unrelated kinds of message arrive on it:

- **client requests** - the shell asking for `net` status, `ping`, DNS, sockets;
- **replies from nic-driver** - answers to net-stack's own questions (link state, TX, RX).

Nothing distinguishes them. When net-stack asks the driver something it calls `nic_req`, which
delegates to the SDK's `request_with_reply_deadline_outcome`, whose wait loop is:

```rust
if let Some(r) = self.try_recv() { return DeadlineOutcome::Reply(r); }
```

It returns **whatever lands next**. So a client request arriving in that window is:

1. consumed as though it were the driver's reply, and misparsed (a `net` request read as a link
   status answers "link up" from `[0] != 0`);
2. never served - the client waits out its own deadline and reports "net-stack not responding";
3. worse, its **reply capability stays on the kernel's pending FIFO**, so the *next* reply net-stack
   sends goes to the wrong requester.

Found independently by two audits (userspace A10-1, documentation A5-2) after an idle tick made the
window open once a second, forever. The tick was reverted; the window still exists whenever net-stack
talks to the driver while a client is active.

**Precedent:** `fs` had exactly this and fixed it exactly this way. Its replies were matched by arrival
order, which produced the "run `dir` twice and it is out of step" desync; the fix was a correlation byte
at offset 0 of both request and reply (see the `tag` handling in
`services/fs/src/main.rs`). This spec is that pattern applied one layer down.

> **Amendment 2026-08-22: the second endpoint EXISTS now, and the rejection below was right about the
> wrong problem.** Tags solve CORRELATION - telling your reply apart from other traffic - and they do
> that well; the shell's "discarded an fs reply for tag 68 while awaiting 69" is the mechanism working.
> What they cannot solve is a reply that never ARRIVES: a service blocked awaiting a reply cannot drain
> the endpoint it also SERVES on, sixteen client requests fill the queue, and the reply is dropped by a
> peer that correctly uses `try_send` rather than deadlocking. The wait then runs to its full deadline -
> 30 s per block operation, which on x86 made `write append` take 73 seconds. A tag on an undelivered
> message does nothing.
>
> The stated blocker also turned out not to hold. "There is no `CreateEndpoint` syscall" is true and
> irrelevant: the FIRST endpoint is minted during spawn, and the second is the same mint a few lines
> later. No new syscall, no contract change, no way for a service to ask for it wrongly - every service
> that can receive gets one, costing one endpoint and two cap slots.
>
> Tags stay. They are still what separates one reply from another once both are in the mailbox, and the
> net-stack/nic-driver protocol below is unchanged. The endpoint fixes delivery; the tag fixes identity.

**Rejected alternative - a second endpoint.** [SUPERSEDED - see the amendment above.] Structurally cleaner (client traffic and driver traffic
in different mailboxes, impossible to confuse) but **not available**: there is no `CreateEndpoint`
syscall, a service's receive endpoint is minted at spawn from its contract, and `ServiceContext`
carries a single `recv_slot`. It would take a new kernel primitive, an SDK change to select a mailbox,
and a contract change, to solve one service's problem. Tags need none of that and there is a working
example in the tree.

---

## 2. Wire format

One byte at **offset 0** of every net-stack -> nic-driver request and every nic-driver -> net-stack
reply. Every existing field shifts up by one.

```
request : [tag, op, ...args]          (was [op, ...args])
reply   : [tag, ...payload]           (was [...payload])
```

- The tag is **echoed**, never interpreted: nic-driver copies request[0] into reply[0] and does
  nothing else with it. The driver needs no state and no memory of outstanding requests.
- `tag = 0` is **reserved** and means "untagged". A reply carrying 0 is from an instance that predates
  this change, or from something that is not nic-driver; treat it as unmatched.
- The counter is per net-stack instance, incremented per request, wrapping. Wrapping is safe: the
  window that matters is one outstanding request, so only the current tag is ever compared.

Both services must ship together. There is no compatibility mode - they are spawned by the same
supervisor from the same image, so a mixed pair cannot occur outside a partial build.

---

## 3. Phases

### Phase 1 - tag the protocol

Add the byte on both sides, one op at a time, keeping the two in step.

- `services/nic-driver/src/genet.rs` (`serve`) and the other backends' serve loops: read the tag from
  `p[0]`, shift every existing parse by one, echo the tag into `out[0]`.
- `services/net-stack/src/main.rs`: every place that builds a request for `nic_req` and every place
  that parses its reply.

Ops to convert (grep `nic_req(` for the full list): status/link, TX frame, RX frame, and the ARM
USB-net variants if that backend is in the build.

**Verify after each op**, not at the end: `net`, `ping`, and a DHCP configure must still work. A wrong
shift shows up as a plausible-looking wrong value, not a crash - the exact failure that is cheap to
find one op at a time and expensive to find after six.

### Phase 2 - a tag-aware await in net-stack

`nic_req` stops using `request_with_reply_deadline_outcome` (it cannot know about tags; it is generic
and shared with every other caller). net-stack grows its own send-and-await:

```
send the request carrying tag T
loop until deadline:
    m = try_recv()
    if m is None: yield/sleep, continue
    if m[0] == T: return Some(m)          // our reply
    else:         stash(m)                // NOT ours - see phase 3
return None                                // deadline; caller already handles this
```

After phase 2 alone, `stash` may simply **drop** the message. That is already a strict improvement:
a mis-served client with a corrupt reply becomes a client that times out and retries - a defined,
loud, recoverable outcome. Ship it here if phase 3 has to wait.

### Phase 3 - the bounded stash

Dropping loses work, so keep what is not ours and serve it after.

- A small fixed array (4-8 entries) of pending client messages, owned by the serve loop, **not** a
  heap or a growable buffer (§26.6.1).
- The serve loop drains the stash **before** calling `recv()`.
- **On overflow, drop the OLDEST and say so once.** A bound that is silently exceeded is the
  unbounded-behaviour case §26.6 forbids; a bound that is loud is a bound.
- A stashed message carries a reply cap. Reclaim it if the message is dropped, or the slot leaks
  (§8.5, and the class behind `1ecfd98e`).

---

## 4. What this unblocks

- **The idle link tick** (cable INFO on plug/unplug without being asked) - reverted for exactly this
  bug; see the note at the revert site in `net-stack/src/main.rs`.
- **Staying responsive during the DHCP dance** - net-stack currently cannot serve a client while
  configuring, because the dance runs inline in the request path.
- **Any periodic work at all**, including the re-sync a future time service would ask for.

---

## 5. Test plan

Per phase, on hardware (QEMU cannot reproduce the Pi 4's NIC):

1. `net` and `ping` with the cable in - the ordinary path still works.
2. Boot cable-out, plug in, confirm DHCP configures (this is the path that regressed twice already).
3. **The bug itself:** run a continuous `ping` and, while it is running, make net-stack talk to the
   driver (a second `net` from another prompt, or plug/unplug the cable). Before the fix this can
   misparse; after it, both complete correctly.
4. `chaos max-carnage` - net-stack and nic-driver are both restartable, and a tag must not survive a
   restart in a way that matches a stale reply. A respawned net-stack starts its counter fresh; a
   reply from before the restart carries a tag it will not match, which is the correct outcome.

---

## 6. Notes for whoever builds it

- Do **not** add a tick, a background poll, or any other unsolicited driver traffic before phase 2
  lands. That is the change that turned a latent race into a once-a-second one.
- `fs` is the reference for the pattern, including what it does with a message that is not its reply.
  Read `services/fs/src/main.rs` and the shell's `drain_stale_fs_replies` before designing the stash.
- The tag proves a reply is *for this request*. It does not prove the reply is *correct* - keep the
  existing length and shape checks on every parse.

---

## 6. What was done on 2026-08-18, and what it does not cover

**The failure that forced it.** `learn_our_mac` read a link-status reply, found zeros where a MAC should
be, and net-stack reported "no NIC MAC yet (driver absent/not ready)" for two minutes while the driver
was up and had logged the MAC at boot. Every request after the first timeout was being answered with the
PREVIOUS request's answer - the same one-out-of-step desync `fs` had, arriving here exactly as §1
predicted.

**What was implemented.** `nic_req` now DRAINS the receive queue before it sends. net-stack has at most
one driver request outstanding, so clearing the channel first makes the next capless message
unambiguously the answer to the question just asked. Messages found during the drain are separated by a
distinction the protocol already makes rather than a new one: a **client request carries a reply cap, a
driver reply does not**. A stale driver reply is discarded; a client request is dropped WITH ITS CAP
RECLAIMED, which is this spec's own phase-2 behaviour ("a mis-served client with a corrupt reply becomes
a client that times out and retries").

**Why not the tag, given the spec says to build it.** The tag is 40 edit points across
`nic-driver/src/main.rs` (3 receive loops, 14 payload reads, 18 reply sends) and `genet.rs` (5 more).
This spec says a wrong shift "shows up as a plausible-looking wrong value, not a crash - the exact
failure that is cheap to find one op at a time and expensive to find after six", and says to verify on
hardware between each. Doing all 40 in one pass, on a branch under active hardware test, is the thing
this document warns against. The drain fixes the observed defect with no wire change and no off-by-one
surface.

**What the drain does NOT cover, and why the tag is still owed:**
- It assumes ONE outstanding driver request. That is true today and nothing enforces it - the tag would.
- A reply that arrives DURING our await, belonging to a request whose deadline has already passed, is
  still taken as ours. The drain closes the window between requests, not inside one.
- It cannot support net-stack doing background work while a client is active (§4), because a client
  request met during a driver await is dropped rather than stashed. That is phase 3 and it needs the
  stash to be owned by the serve loop, which means threading it through the sixteen `nic_req` call
  sites - the reason it is not done here. **(Superseded: phase 3 was later built, withdrawn, and
  built again once the client hop carried a tag - see §7.2 and §7.4. A displaced request is STASHED
  today, not dropped.)**

So §4's list is still blocked, and the phases below are still the plan. This is a narrowing of the bug,
not its removal.

### 6.1 The same desync, one hop lower - found and fixed with exactly this design (2026-08-19)

This document is about the **net-stack <-> nic-driver** hop. The identical fault was then found on the
**nic-driver <-> dwc2** hop and fixed there (`7f678e00`), which is worth recording because it is
evidence about this design rather than a separate story.

That hop carried three ops (INFO, TX, RX) on one channel with untagged replies. `nic-driver` bounds its
wait, so a reply arriving after its deadline was still queued when the next request went out, and every
answer afterwards was one behind - permanently. Being one behind there is not a late answer, it is
destruction: an RX reply read as an INFO reply is a FRAME consumed as a status word. Worse, the retry
re-sent on timeout, and dwc2 POPS a frame to build each RX reply, so a resend cost a frame every time.

The fix is phase 1 of this document applied to that hop: one byte of op tag on every reply, and a reply
whose tag does not match is consumed and reported as "nothing this time" rather than believed.
Deliberately NOT re-asked on mismatch - re-asking is the destructive move above. Consuming the stale
reply shortens the queue by one, so alignment repairs itself.

Two things follow for THIS hop:
- the approach is validated on hardware, at a cost of one byte per reply
- the argument that "one outstanding request makes a tag unnecessary" is now known to be wrong in
  practice, because that is exactly the assumption the lower hop was making when it lost frames

---

## 7. What was done on 2026-09-14: phase 2 everywhere, and phase 3 BUILT AND WITHDRAWN

Two things happened, and the second is the more useful.

### 7.1 Phase 2 now covers every conversation, not one of them

§6 records the 2026-08-18 drain, which fixed the observed defect by clearing the channel before a
STATUS query. It says plainly what it did not cover, and the biggest gap was this: the drain only
guarded `nic_status_req`. **Every other `nic_req` still returned whatever landed next**, so a client
that spoke during an ordinary frame send or drain was CONSUMED as the driver's reply, parsed as a link
status or a frame batch, and silently mis-served - the original bug, still present in fifteen of the
sixteen call sites.

That is closed. The SDK grew `request_with_reply_deadline_sifted` (and a millisecond twin), a bounded
wait that asks the caller about each message as it arrives instead of believing the first one. Every
conversation with `nic-driver` goes through it. A displaced client request is now **identified**, and
dropped with its capability reclaimed and counted - phase-2 behaviour, which this document explicitly
sanctions ("ship it here if phase 3 has to wait"). **(Superseded by §7.4: once the client hop carried
a tag, phase 3 came back and the request is STASHED rather than dropped.)**

**The discriminator is the one §6 already found**: a client request carries a reply capability, a
driver reply does not. So none of this needed the wire tag, and the forty-edit-point change this
document warns about was not attempted. The tag is still owed for the case the discriminator cannot
see - two driver requests outstanding at once, which nothing currently makes - and that is unchanged.

### 7.2 Phase 3 was built, measured, and taken back out

The bounded stash of §3 was implemented in full: a fixed ring of four displaced requests, each
carrying the payload, the badge and the reply capability captured at arrival, drained by the serve
loop before it blocked, dropping the oldest on overflow and saying so once. It is what this document
asks for, and it does not work.

**A request answered LATE is worse than one never answered.** A client that gives up RE-SENDS - the
shell reacquires net-stack and asks again, with deadlines from 3 to 30 seconds depending on the
command. Answer the held copy as well and the client has two replies to one question: it reads the
first as the answer to THIS request and the second as the answer to the NEXT one, and every exchange
afterwards is permanently one behind. That is the same desync this document was written to remove,
reintroduced from the other end.

It is not a theory. The shell log says it directly:

```
DIAG sift: kept a client request op=1 badge=0 len=12
DIAG sift: kept a client request op=1 badge=0 len=12     <- the same lookup, re-sent
DIAG serve: from the stash, op=1 len=12
example.com is 172.66.147.243                            <- one copy answered
...
DIAG serve: from the stash, op=1 len=12                  <- the other copy, two commands later
net: net-stack gave a short reply                        <- a `net` status answered with a hostname
```

**A hold bound does not close it.** Expiring entries after half a second was tried and measured next;
it turned a reproducible failure into an intermittent one, which is worse to own and no better to
rely on. The reason is that the hold and the SERVE are separate: take a displaced lookup after 400 ms,
spend three seconds resolving it, and the reply still lands after the client's deadline. net-stack
cannot bound its own serve time, and it does not know the client's deadline.

### 7.3 What phase 3 actually needs, stated so it is not rediscovered

**Correlation on the CLIENT hop** - net-stack <-> its clients - which is a different hop from the one
this whole document is about. The client tags its request, net-stack echoes the tag, and the client
discards a reply to a question it is no longer asking. That is exactly what `fs` carries
(the shell's `drain_stale_fs_replies` / `reclaim_late_fs_reply`),
and exactly what this hop does not.

Until then, deferral is unsafe and dropping is correct: the client times out, retries, and exactly
one request is ever outstanding. Recorded rather than half-built (§26.7).

**A note for whoever builds it:** the hold is safe in one regime, and it is the regime the stash was
wanted for in the first place - SHORT background work, where the displaced request is served within
milliseconds and no client is anywhere near its deadline. It is long, client-initiated operations
(a DHCP dance, a DNS lookup that times out) that make a held request stale. So the correlation tag and
the stash are worth building together with the background poll of §4, not before it, and the stash
should hold only while net-stack is doing its OWN work.

### 7.4 And then phase 3 came back, scoped to where it is safe

The withdrawal above is right about the hazard and was too broad about the remedy. Measuring the
cost showed what the right scope is.

**The measurement.** `scripts/tcp_qemu_test.py` failed about one run in three, on this host, BOTH
before and after net-stack learned to sift - so the sifting did not cause it - and every failing run
correlated exactly with one event: `time` nudges net-stack for the network clock (op 11, one-way,
carrying no reply cap), net-stack runs an SNTP exchange inline, and the shell's `tcp` request lands
inside it and is lost. Before sifting it was consumed and misparsed; after, it was dropped and said
so. Either way the shell then waited out its whole deadline.

**The distinction that makes deferral safe.** A request is unsafe to hold while net-stack is SERVING
somebody, because that client may give up and re-send, and then two replies answer one question. It
is safe to hold while net-stack is working for ITSELF, because nobody is waiting on that work and
there is no re-sent copy in flight. The SNTP nudge is the only such work in the service, and it is
exactly where the losses were.

So: **one held slot, armed only around the nudge, expiring after 500 ms.** One, because the situation
is one client speaking into one bounded moment. Armed only there, because off is the safe default and
that is what you get by forgetting. Expiring, because the hold must end well inside the shortest
client deadline in the tree - three seconds, the shell's status query - and because the work being
waited on does not always go well.

**What remains, recorded rather than smoothed over.** When the SNTP server does not answer, the
exchange costs net-stack its whole query budget, which is seconds - far longer than the hold. The
held request then expires, is dropped, and says so in one line naming the reason. The QEMU test still
fails on those runs, and it should: the defect is real and it is not this one. It is that the DANCE
BLOCKS THE SERVE LOOP, which `docs/tcp-design.md` already quotes this service's own comment about
("making the dance incremental so net-stack answers THROUGHOUT it is the real fix, and that is a
rework of the state machine rather than a constant"). The hold covers the common case and reports the
uncommon one; it does not pretend to have fixed the loop.

---

## 8. The CLIENT hop is tagged now (2026-09-14)

§7.3 named correlation on the net-stack <-> client hop as the real prerequisite for deferring a
request at all. It is built.

**One byte at offset 0 of every name-addressed request, echoed back and never interpreted.** It is
the same wire change this document describes for the driver hop, applied to the other side of the
service - and the reason it could be done in one pass, where that one cannot, is the shape `fs`
found:

- **net-stack strips the tag in ONE place** (the serve loop, right after the badge decision) and
  **echoes it in ONE place** (`Reply::send`). Not one of the thirteen op arms knows the tag exists.
- **the shell strips it in ONE place** (`ns_take_tagged`), so every call site still reads
  `r.payload_bytes()` with byte 0 meaning exactly what it always meant.

That is why the forty-edit-point warning in §3 does not apply here. There is no per-op shift to get
wrong, because no op moved. `fs`'s own comment says it plainly: *"the tag is handled here and nowhere
else, which is why adding it did not touch a single arm."*

### What is deliberately NOT tagged, and why

- **Badged socket invocations.** A capability invoking its owner; the badge already names the
  socket, so there is nothing to correlate. `fs` makes the identical exception for file caps.
- **The capless clock nudge (op 11).** One-way, from `time`, with no reply cap and no reply. It is
  identified precisely BY having no reply cap, and it is handled before the strip.
- **The nic-driver hop.** A different channel with a different problem; `net_query` takes the tag as
  an `Option` and its nic-driver callers pass `None`.

### Why the type, not the byte

Thirteen places in net-stack answer a client. Hand-writing the tag at each is the "plausible-looking
wrong value, not a crash" failure this document warns about: a missed site still compiles, still
sends, and is one byte out forever. So the reply capability was given a TYPE - `Reply { cap, tag }` -
and every one of those sites became a build error until converted. The compiler enumerated them
instead of a person. On the shell side the same job was done by promoting the request helpers to
`&ShellCtx`, which the compiler then propagated up the call tree through six more functions.

The tag counter lives in `ShellCtx` beside `fs_tag`, NOT in a `static`: audit C6-1 had to undo
exactly that mistake on the fs channel, and Invariant 9 forbids the unowned global it would be.

### What this unblocks, and what it does not

It removes the blocker §7.3 records. A request can now be deferred and answered late, because the
client can tell a late answer from its own. **The stash, the background poll and listen/accept are
now unblocked work rather than blocked work** - none of them is built by this change.

**It is proven wired, and not proven to fire.** Matching is mandatory - a reply is returned only if
its tag matches - so if net-stack were not echoing correctly, every network command would time out
rather than quietly work; 174/0 with `net`, `net dns`, `net arp`, `net renew`, `sock`, `tcp`, `ping`
and `date sync` all passing is therefore positive evidence the byte makes the round trip. What has
NOT been observed is the discard path actually firing, because that needs a real desync to provoke
and nothing provokes one on demand. Recorded rather than claimed (§26.7).

### Hardware-verified on the Pi 2 (2026-09-14)

Same board, same host, straight after the change. TCP 15 ms and 16 ms for the two transactions,
against 47 ms and 15 ms before it - the same band, with the variation living in ARP (158-222 ms)
rather than in the protocol. **A correlation tag that changed any observable behaviour on a healthy
channel would mean it was wrong**, so the result being dull is the result.

What the run proves beyond "nothing broke": `net` answered with a lease, a resolved gateway and a
successful ping, so the byte makes the round trip in both directions; and the wall clock was set
from the network, which is the CAPLESS op-11 nudge from `time` and therefore proof that the
deliberately-untagged path was not shifted by one.

Not covered by that run: the badged socket path (`sock`), which is exercised by the x86 shell suite
but was not typed on this board.

### One thing found on the way

`net_query`'s pre-send drain discarded messages without reclaiming their embedded capabilities. That
is SEC-35 one channel over: the kernel has already installed the cap and queued its slot, so
dropping the message leaves an entry that the next socket `open` reads as its own - the `fcap` bug.
Fixed here. **Two more blind `while ctx.try_recv().is_some() {}` drains remain in the shell with the
same hole**; they are on other paths and are left recorded rather than swept up in a networking
change.

---

## 9. Phase 3 is built (2026-09-14), and this time it is measured both ways

The bounded stash of §3 is in, unconditional, four slots. What makes it safe is §8: the client hop
carries a tag, so a held request answered late arrives with a tag the client is not waiting for and
is discarded instead of being read as the answer to its next question. That was the entire hazard
§7.2 records, and it is gone.

**The tag's discard path is now OBSERVED FIRING**, which closes the gap §8 left open. The shell log,
during an ordinary suite run:

```
shell: discarded a net-stack reply for tag 3 while awaiting 4 (overtaken)
net-stack: a held client request waited more than 500 ms and was dropped
```

So the correlation is not merely wired: it is doing the job, on the exact configuration that
desynchronised without it. Zero occurrences of `gave a short reply`, the symptom §7.2 was killed by.

### The bound is set by LATENCY, and that was got wrong first

The first version after the tag widened `HOLD_MS` to 3 s - the shortest client deadline - reasoning
that anything inside it was now safe. It is safe, and it is slower, and only a before/after
comparison showed it:

| | baseline (tag, no stash) | stash, hold 3 s | stash, hold 500 ms |
|---|---|---|---|
| `net: net-stack unavailable` | 0 | **1** | 0 |
| tag discard fired | 0 | 2 | 1 |
| `gave a short reply` | 0 | 0 | 0 |
| shell suite | 174/0 | 174/0 | 174/0 |

A request held for 2.9 s is still served, by which time the client gave up at 3.0 s and re-sent -
net-stack then does the work twice and the duplicate delays the copy that is actually wanted. **The
suite passes at 174/0 in all three columns**, so nothing but the comparison would have caught it.

The rule, stated so it is not widened again: **the hold must be well UNDER the shortest client
deadline, not equal to it**, so a held request is either served promptly or abandoned early enough
that only the re-send is served.

### What this does and does not unblock

The background poll step is now unblocked in the sense that matters: a client met during unsolicited
driver traffic is kept and served rather than lost. §4's list - the idle link tick, staying
responsive during the DHCP dance, any periodic work at all - is buildable. None of it is built here.

## 10. The second header byte: how long the client will wait (2026-09-16)

The tag solved correlation. It did not solve the other thing net-stack cannot know about its client,
and that gap cost twenty seconds a command on a Dell Wyse.

A request that arrives while net-stack is mid-conversation with `nic-driver` is held in a four-slot
stash. A held request used to be dropped after a FIXED `HOLD_MS` of 1.5 s, and the constant carried a
compile-time assertion saying why: `HOLD_MS < 3_000`, "a held request must be served before the
shortest client deadline". That was correct when every client on this hop waited 3 s.

Then the transaction path (`tcp`, `sock`, `serve`'s listen) was given a 20 s bound, and the assertion
quietly became a defect. net-stack threw a request away at 1.5 s while its client sat patiently for
another 18.5, the client timed out, reacquired, re-sent, and the re-send was answered in
milliseconds. Measured as 19.67 s and 19.99 s to dispatch, twice, with the arrival receipt showing the
served copy arriving "from the queue" - the retry, not the original (`backlog/29`).

**A constant cannot know a client's deadline, so it stops guessing: the client says.** Byte 1 of a
tagged request is how many seconds that client will wait, saturating into one byte. `Displaced::note`
stores it per entry and `take` expires each against its own, so a 3 s status query is still dropped
promptly and a 20 s transaction is held until it can be served.

Three properties are deliberately preserved from the tag:

- **Stripped in ONE place**, with the tag, so not a single op arm knows either byte exists.
- **Badged invocations carry no header** - a capability invocation names its resource and needs no
  correlation - so those keep `HOLD_MS` as the default.
- **`time`'s one-way `[11]` nudge is untouched**: it is capless and handled before the strip.

The latches went with it. Both drop reports were "said once", which is exactly what hid this for three
debugging sessions: the first drop was reported at boot and every later one - each costing a client
its whole deadline - was silent, so a board looked healthy while its commands took twenty seconds.
They are bounded by RATE now (every one of the first eight, then every eighth), which is a bound that
still reports the twentieth occurrence.
