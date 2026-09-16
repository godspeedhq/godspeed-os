# 31. net-stack blocked 48 s waiting on a nic-driver that was ALIVE

**Status: OPEN, measured, not diagnosed.** Found on a Dell Wyse 5070, 2026-09-16, by the slow-pass
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
