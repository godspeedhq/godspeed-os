# 12. xHCI hub probes block the input loop - typing lags on a single core

**Severity:** user-visible latency, not a fault. Known, partially mitigated, and the real fix is
already designed for the analogous problem one layer down.
**Observed:** 2026-09-06, T630 single core. Typing on a keyboard behind the **xHCI** is noticeably
slow; move the same keyboard to the **EHCI** and it is fast again. Reproducible by hot-plugging
between the two controllers in either direction.

## What it is

`services/xhci/src/main.rs` documents it precisely, and the comment predates this observation:

> *That wait BLOCKS THE INPUT LOOP. Auto-repeat is synthesised by this loop, so a held key stutters
> every time a probe runs: a gap of the whole budget, every `HUB_POLL_MS`.*

The xHCI event ring has several consumers and no correlation between a completion and the requester
that wanted it, so a hub port probe cannot tell "my answer has not come yet" from "my answer went to
someone else". It therefore waits out a budget - and that wait sits in the loop that also delivers
keystrokes and synthesises auto-repeat.

Current values: `HUB_POLL_MS = 500`, `PROBE_ANSWER_MS = 10`. So the input loop can stall up to 10 ms
at a time, per hub port, twice a second.

## Why a single core made it obvious

On four cores the probe and everything else overlap: the driver has a core and the stall is absorbed.
On one core that stall is the whole machine's foreground, and it lands in the middle of the path a
person is directly watching - their own typing. `ehci` has no equivalent: it polls its interrupt
endpoint without a competing probe on the same loop, which is exactly why the same keyboard feels
fast there.

## The fix, which is already designed

Correlate a completion with its requester, so a probe gets its own answer and never waits out a
budget for one that already went elsewhere. `docs/net-tags-design.md` designs this exact mechanism
for `net-stack` <-> `nic-driver`; the xHCI event ring is the same problem one layer down, and the
driver's own comment says so:

> *This is mitigation, not the fix. The fix is to correlate a completion with its requester so the
> probe gets its answer at all... Then the budget stops mattering.*

`PROBE_ANSWER_MS` was already cut 50 -> 10, which shrank the gap fivefold. That is as far as tuning
goes: the remaining 10 ms is the honest cost of not knowing whose answer arrived.

## Not urgent, and worth saying why

Nothing is lost or broken - keystrokes are delivered, and the earlier "rare dropped keystroke" was
fixed separately by having BOTH paths that observe a completed report deliver it. What remains is
latency, on a configuration (single core, keyboard behind the xHCI) that no shipping setup uses. It
is recorded because it is a real user-visible symptom with a known cause and a designed fix, not
because it needs doing now.
