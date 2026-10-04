# 73. A full send-cap cache dropped a reacquired peer and said it was reacquired

**Status: FIXED 2026-10-04, verified on the card (one residual recorded below). Found from the first Pi 4 storm that survived (`backlog/72`):
after it, the shell could not reach `net-stack` at all, and killing `net-stack` did not help.**

## What was seen

Pi 4, `f587acbe`, after `chaos max-carnage all-services 50 yes` and two kill-storms. `net-stack` was alive
and configured - it took a DHCP lease and pinged the gateway at 10:24:32, and again after a manual
`kill net-stack` at 10:28:21 - but from the shell:

```text
No reply from 8.8.8.8: net-stack not responding      (one a second, not one per 5 s deadline)
SKIP  net - link is up but no lease in 20s
cap::get: ResourceId(104) gen mismatch cap=729 rec=759 liveness=Alive   (dozens, through selfcheck)
```

The same `ping 8.8.8.8` had answered before the storm, from a shell that had not yet been respawned.

## Why

`find_send_slot` resolves a peer from two places: the SDK's send-cap cache (`SEND_CAP_CACHE`, filled by
`reacquire_cap_detail`) and the peers wired at spawn. The shell is wired with `fs` alone, so every other
peer it names lives in the cache - and the cache held **8**.

When the cache was full and the peer had no entry, `reacquire_cap_detail` stored nothing and returned
`Ok` anyway. So:

- the caller believed the peer was reacquired, and the next `find_send_slot` could not find it: a request
  failed before it was sent, which the request helpers report as a timeout - instantly;
- the capability just acquired leaked, one per attempt, every attempt;
- for a peer that WAS wired at spawn, the lookup fell back to the spawn-time capability, which is stale
  after that peer's first restart - the `gen mismatch ... liveness=Alive` stream is the trace path doing
  exactly that to `events`.

Before the storm, the shell's first command was `ping`, so `net-stack` got an entry early. The respawned
shell ran `selfcheck` instead, which named eight other peers before it reached the network part.

## The fix

- `CACHE_SIZE` 8 -> 32, above the supervisor's whole managed roster.
- A full cache EVICTS, round robin: the victim's capability is reclaimed, its peer is reacquired when next
  used, and the eviction is logged. A reacquire never again reports success for a capability it did not
  keep.
- A second cache writer, with the same silent-drop shape and no callers, is deleted.

## How to tell it worked

On the card: a storm, then `selfcheck`, then `ping 8.8.8.8`. The network checks should pass rather than
skip, `ping` should answer, and the `ResourceId(104) gen mismatch ... liveness=Alive` stream should be gone.
If an eviction ever happens it now says so (`sdk: the send-cap cache is full`).

## Verified on the card (2026-10-04)

Pi 4, the fix on `2b142ecf`. `chaos max-carnage all-services 50 yes` (shell respawned 13 times, the last a
second before the storm ended), then `ping 8.8.8.8` from that fresh shell: 11 of 11 answered. Then, in the
same session, the sequence that failed before - `selfcheck` from the respawned shell, then `ping`:

```text
PASS  net - the stack holds a lease (or has no link to need one)
run: ran 526, failed 0, skipped 0
Reply from 8.8.8.8: bytes=32 time=42ms TTL=117
```

No eviction was logged (`sdk: the send-cap cache is full` never appeared), so 32 holds the shell's peers.

**Residual, not this bug.** After `selfcheck`'s own `chaos kill-storm events`, some client went on sending
to the `events` endpoint one generation late for about thirty seconds, in bursts that line up with the
`events` part of `selfcheck` (`cap::get: ResourceId(116) gen mismatch cap=783 rec=803 liveness=Alive`, 31
lines), then stopped. The SDK trace path reacquires after a failed send, so the holder is more likely a
long-lived service that talks to `events` directly - `recorder` and `copier` are never restarted by a
storm and keep their spawn-time capability. Nothing failed because of it; it needs the caller named before
anything is changed.
