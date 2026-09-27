# 62 - `osdev test shell` varies between 0 and 6 failures on the same binary

**Status:** OPEN - measured, not diagnosed. Not caused by the change that found it, and it is NOT
established whether it predates 2026-09-27; that is the next step rather than a claim.
**Found:** 2026-09-27, verifying an unrelated `xhci` logging change. Six failures appeared, which is too
many to wave at as one timeout.

## The measurement

Four consecutive runs, **identical binary, nothing rebuilt between them**:

| run | result |
|---|---|
| 1 | 209 passed, 6 failed, 2 skipped |
| 2 | 215 passed, 0 failed, 2 skipped |
| 3 | 209 passed, 6 failed, 2 skipped |
| 4 | 214 passed, 1 failed, 2 skipped |

A stray QEMU was killed before runs 3 and 4 and `tasklist` confirmed none was left, so a leftover guest
is not the explanation for at least those two.

**That variance IS the control, and it is why this is not the change's fault.** A deterministic regression
fails the same assertions every run. The code was constant across all four; the results were not.

## What fails

Only one failure was captured by name before the count dropped again:

```
shell-test: FAIL - sock: opened + invoked a UDP socket capability (socket = capability, 7.10)
```

`sock` opens a UDP socket through `net-stack` and sends a datagram, so it depends on the whole network
dance - DHCP through QEMU's user-mode backend, ARP, then a round trip. That is the slowest and most
timing-dependent thing in the suite, and the six-failure runs are consistent with a cluster of
network-dependent assertions timing out together rather than with six unrelated defects.

**Consistent with, not proven.** The other five were not captured, and guessing them from the shape is
exactly the reasoning this project distrusts. The next run that fails six should have every `FAIL` line
saved before anything else is done.

## What this is NOT

- **Not the `wifi` verb.** Its nine assertions passed in every run that printed them, and they touch no
  hardware and no network.
- **Not the `xhci` change that found it.** Same-binary variance rules it out, and that change adds log
  lines on a path QEMU's shell test never reaches (no unclaimed USB device in that guest).
- **Not established as new.** The run before any of today's changes was 206 passed / 0 failed, ONCE. One
  clean observation is not a baseline, and treating it as one would be the same error as reading a second
  failure in a session as an independent control.

## Why it matters more than a flaky test usually would

A suite that reports between 0 and 6 failures teaches its reader to discount failures, and this file
already has a worked example of what that costs: `80-network.gsh` carries a long comment about a check
that was made to REPORT rather than fail precisely because "a failure that gets discounted protects
nothing - which is worse than not asserting at all, because it also costs the reader's trust in the 467
results beside it."

That reasoning applies to the harness as much as to one assertion. Right now this suite cannot be used as
a merge gate, because a red run means nothing until it is run again.

## Next step

1. **Establish whether it predates today.** Four runs at the commit before `1602f3df`, same machine, same
   conditions. That is the measurement missing here, and nothing should be concluded before it exists.
2. **Capture every `FAIL` line** on the next six-failure run, so the cluster is known rather than inferred.
3. If it is the network-dependent group, the fix is the one `80-network.gsh` already models: wait on the
   truth with a bound, and where the truth is somebody else's DHCP server, report rather than fail.
