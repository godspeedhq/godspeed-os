# 12. xHCI hub probes block the input loop - typing lags on a single core

**Severity:** user-visible latency, not a fault. Known, partially mitigated, and the real fix is
already designed for the analogous problem one layer down.
**Observed:** 2026-09-06, T630 single core. Typing on a keyboard behind the **xHCI** is noticeably
slow; move the same keyboard to the **EHCI** and it is fast again. Reproducible by hot-plugging
between the two controllers in either direction.

## MEASURED, and it is NOT the hub probes

The driver's own 60 s heartbeat, with the keyboard on an xHCI port, single-core T630:

```
xhci: alive - t=..., 592 passes (2 fast/592 idle), work 76ms (serve 1 drain 0 hub 73),
      probes 0/0 ok 0 late, 0 MSI, 0 msg, 1 HID, disk no, 0 dropped
```

- **`probes 0/0`** - no hub probe ever completed. The theory this file opened with was wrong.
- **`0 MSI`, across five minutes and 592 passes** - the driver has received NO INTERRUPTS AT ALL,
  though MSI is programmed (`msi: class 0x0c0330 BDF 0x0080 -> vector 0x30`, matching the granted
  vector 48).
- **~120 passes per 60 s ~= 2 Hz.** The loop wakes about twice a second.
- `work 76ms` in five minutes: the driver is not busy, it is asleep.

So the driver is running entirely on its fallback, which it describes as *"polling at the 10ms tick
alongside interrupts (input latency floor)"* - and that floor is not 10 ms. `IDLE_WAIT_MS = 5`
converts to `cycles_to_ticks(...) = 1` tick, and a tick is one `scan_timed_wakes()` call on the BSP
timer. Measured, that tick is arriving at ~2 Hz, so one tick is ~500 ms. **A keyboard serviced twice
a second is the lag.**

`ehci` is unaffected for a reason that now makes sense: it receives real interrupts (`legacy INTx
routed via IOAPIC ... vector=0x29`), so it wakes on the keystroke itself and never depends on the
tick. Same keyboard, same core, different wake source - which is exactly what hot-plugging between
the two controllers demonstrates.

## Two separate defects, either survivable alone

1. **xHCI MSI never fires on this machine.** The destination logic looks right - `usb_irq_dest_lapic`
   falls back to the BSP LAPIC when the driver's contracted core is not ready, which is the
   single-core case - so the fault is further down and NOT yet located.
2. **The "10 ms polling floor" is ~500 ms.** Whatever causes (1), this is independently wrong: the
   BSP tick drives every `ctx.sleep()` wake in the system, and at ~2 Hz every sleeping service in
   the machine is 50x slower to wake than its code says. Fixing this alone would bound typing
   latency to a tick, with or without interrupts.

(2) is the more valuable fix: it is not USB-specific, it affects every service that sleeps, and it is
measurable without hardware once the tick rate is exposed. It may also be the same family as the
recorded BSP idle-wedge (a core halting onto a consumed one-shot deadline) - the idle path is where
a single core differs, and it is the path that re-arms this timer.

## Superseded: the hub-probe theory

Kept because the reasoning is still sound about what hub probes COST, and because it is a fair record
of a theory that measurement refuted - `probes 0/0` says they were not even running.

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
