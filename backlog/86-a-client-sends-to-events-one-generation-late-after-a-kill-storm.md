# 86. Some client sends to `events` one generation late for about 30 s after a kill storm

**Status: OPEN, 2026-10-13.** Moved out of `backlog/fixed/73` when that entry was closed: it was the
residual recorded there as "not this bug", and recorded nowhere else.

## Evidence

Pi 4, 2026-10-04: after `selfcheck`'s own `chaos kill-storm events`, some client went on sending to
the `events` endpoint one generation late for about thirty seconds, in bursts that line up with the
`events` part of `selfcheck`, then stopped:

```
cap::get: ResourceId(116) gen mismatch cap=783 rec=803 liveness=Alive      (31 lines)
```

Nothing failed because of it.

## What is ruled out

- Not the SDK trace path: it reacquires after a failed send.
- Not a lost log line: logs never go to `events` (CLAUDE.md 11.4); only traces and metrics do.

## Candidates, not checked

A long-lived service that talks to `events` directly and is never restarted by the storm, so it keeps
its spawn-time capability: `recorder` and `copier` were the two named at the time.

## Next step

Name the caller before changing anything. The kernel's mismatch line names the resource and both
generations but not the sender; the cheapest discriminator is to log the calling task's name on that
line (one argument at the call site), run `chaos kill-storm events` in QEMU, and read which service it
is. Then that service reacquires on `EndpointDead` the way the SDK trace path does (CLAUDE.md 14.3).
