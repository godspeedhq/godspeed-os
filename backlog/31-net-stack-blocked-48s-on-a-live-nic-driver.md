# 31. net-stack blocked 48 s waiting on a nic-driver that was ALIVE

**Status: OPEN, and the CAUSE IS NOW CONFIRMED** (2026-09-16, see the section at the end).
The fix is not: a correlation tag was built, proved the fault, and was REVERTED because rejecting a
stale reply is not the same as recovering from one. Found on a Dell Wyse 5070, 2026-09-16, by the slow-pass
instrument added in `backlog/29`. A restart of `net-stack` cleared it.

## What happened

`selfcheck` reported `ran 462, failed 1`:

```
FAIL  fail 'dns: ICMP to 8.8.8.8 works but no name resolves - the UDP request/reply path is broken'
```

and `ping 8.8.8.8` answered `No reply from 8.8.8.8: net-stack not responding`. `kill net-stack` fixed
both: the next run was `ran 461, failed 0` and ping worked.

## What the log shows

```
20:50:21-28  a client request met mid-question to nic-driver was dropped because the stash was full  (x8)
20:50:58.592 dropped a held client request (op 1) after its client's own 8000 ms of patience  (drop #16)
20:51:00.591 nic-driver did not answer the link query - treating as no link, but this is a TIMEOUT,
             not a reading (the cable may be fine)
20:51:02.578 nic-driver did not answer the link query - ...
20:51:03.571 a serve pass took 47985 ms (over 1000) - not asking for client requests during it
20:51:08.653 net-stack: starting                                      <- the operator's kill
```

**One serve pass lasted 48 seconds**, so it began around 20:50:15 and net-stack was inside a single
client request for all of it, doing `nic-driver` round trips that timed out.

**`nic-driver` never died.** It started once at 20:47:06 and was still `Ready` at 20:51:23 with four
minutes of uptime, so this is NOT the stale-peer-cap case (`project_stale_peer_cap_reacquire`), where
the peer respawned and the cached cap went stale. The driver was alive and not answering.

## What is NOT established

Why an alive `nic-driver` stopped answering, and why killing `net-stack` - the OTHER side - cured it.
Those two facts together are the whole puzzle: if the driver were wedged, restarting its client should
not help. Candidates, none separated:

- the request/reply channel desynchronised, so every reply was matched to the wrong request. The
  driver logs `reply send FAILED (caller is gone, or its queue is full)` elsewhere in this session,
  which is what an abandoned deadline leaves behind;
- `net-stack` abandoning a `nic_req` at its deadline and the late reply being consumed as the NEXT
  request's answer, one behind forever - the same shape the client-hop tag was introduced to fix on
  the other hop, and which the net-stack/nic-driver hop still does NOT carry
  (`docs/net-tags-design.md` says the tag is "still owed for the case this cannot see");
- something in `nic-driver` itself that a restart of its client happens to clear.

## A trade-off from `backlog/29` that this exposes, and it may be mine

The eight `stash was full` drops are new pressure. Before the patience byte, a displaced request was
dropped after a fixed 1.5 s, which freed its stash slot quickly. Now a transaction-path request
declares 20 s of patience and can occupy one of only `STASH_N` = 4 slots for that whole time, so the
stash fills far more easily and the eviction path (drop the OLDEST) runs where it previously did not.

That is a real consequence of holding longer and it is recorded rather than explained away. It did not
CAUSE the 48 s block - the driver not answering did - but a fuller stash is a worse place to be when
one arrives. Raising `STASH_N` is the obvious lever and is deliberately NOT pulled: `backlog/28`
already argues that the queue behind it is 16 deep and a multi-second block fills any bound worth
having, so a bigger stash hides the condition rather than fixing it.

## Next step

Instrument the net-stack/nic-driver hop the way the client hop was instrumented, since that is what
worked twice: a correlation tag on that hop would make "the reply matched the wrong request" a
reported fact instead of a candidate. It is already named as owed in `docs/net-tags-design.md`.

## Not to be confused with

`backlog/28` (the in-loop dance blocking for seconds) and `backlog/29` (a held request never taken).
Both are net-stack blocking ITSELF. This is net-stack blocked on a peer that was alive.

## CONFIRMED: the reply stream runs chronically behind (2026-09-16)

The correlation tag this entry asked for was implemented on the net-stack/nic-driver hop and run in
QEMU. It found the fault immediately:

```
net-stack: discarded a nic-driver reply for tag 11 while awaiting 39 - an abandoned request was
           answered late (stale #1)
net: resolving ... cannot reach nic-driver (no answer) - link state unknown
net-stack: ARP for 10.0.2.2 found nothing - 6 sent 0 SEND-FAILED, 0 frames scanned
```

**The driver's replies are running about 28 requests behind.** Every abandoned deadline leaves an
orphan reply in net-stack's queue, and nothing ever removes it, so each wait finds the orphan of a
long-dead request sitting in front of the answer it wants. Untagged, net-stack READ that orphan as its
answer - reply N-28 served as the answer to request N - and the channel appeared to work while every
exchange was tens of requests out of step. That is the 48 second stall at the top of this entry, and
it is now a measured fact rather than a candidate.

## Why the tag was reverted, and what the real fix has to be

The tag does exactly what it was built to do: it refuses the stale reply. **Refusing is not
recovering.** With the tag in and no resynchronisation, all four direct `net` ops (`dns`, `stats`,
`arp`, `renew`) fail outright - net-stack correctly declines every answer it is offered and times out,
where before it accepted a wrong one and limped. TCP kept passing, because its own retransmission
covers a lost exchange.

So the tag is necessary and **not sufficient**, and the missing half is a design question rather than
a constant:

- drain the channel to empty before issuing a request (bounded how? the orphans arrive asynchronously);
- or resynchronise on the first mismatch - keep taking replies until the tag matches, within the
  deadline already held;
- or stop creating orphans at all, by not abandoning a driver request whose reply is still coming.

The third is the root: an orphan exists only because a deadline expired on a request the driver later
answered. That is the same shape as `backlog/29` one hop down.

**Reverted at `87f1f358`'s state**, which is what all five boards passed, so the branch carries no
half-built protocol change. The work is described here in enough detail to be redone deliberately.

**Verified after the revert:** `osdev test shell` 174/0/2, and `git diff 87f1f358 -- services/ sdk/`
empty - the services are byte-identical to the five-board-verified state.
