# Design Spec: Completion Correlation in the xhci Driver

> **Status:** Root cause **proven by measurement** (2026-08-11), fix **not built**. This is the
> driver-layer twin of `docs/net-tags-design.md`: several consumers, one queue, no correlation.
>
> **OUTCOME (2026-08-11, hardware): fixed.** Hub probes went from **151/328 (46%) to 312/313 (99.7%)**
> and hot-plug works in both directions for both devices, with console INFO every time. The residual
> and its correct handling are in §8.
>
> **Symptom it explains:** USB hot-plug does nothing on the Pi 4. Unplug the keyboard or the stick
> and no INFO appears, until unrelated IPC (typing `ls` over serial) drives a re-enumeration that
> rediscovers everything.

---

## 1. The measurement that settled it

The driver's heartbeat was instrumented (`37cccb9c`) with `probes ok/posted late`, specifically to
separate two hypotheses that fit the evidence equally well and want opposite fixes:

- **(a)** the answer arrives too late and the drain discards it -> correlate;
- **(b)** the answer never comes (halted EP0, doorbell not landing) -> fix the endpoint.

One minute on hardware:

```
xhci: alive - t=61s, 1693 passes, work 2962ms (serve 116 drain 0 hub 2845),
      probes 151/328 ok 0 late, 151 MSI, 98 msg, 1 HID, disk yes
```

- **328 probes posted, 151 answered.** 177 failures, and the hub segment holds 2845 ms of the
  2962 ms of work - the failures are 10 ms timeouts, one per probe.
- **`late = 0`.** No hub completion is ever found by the drain.

`late = 0` reads as (b) - and that reading is **wrong**, which is exactly why the counter was worth
building rather than reasoning from. There is a **third** consumer of the event ring that the counter
did not watch.

## 2. The actual mechanism

`msc::await_on_slot` (`services/xhci/src/msc.rs:263`) waits for the disk's completion and, on a
transfer event belonging to any other slot, **consumes and discards** it:

```rust
Some((TRB_TRANSFER_EVENT, _, sid)) => {
    if sid < 32 { *eaten |= 1 << sid; }   // records THAT it happened, not the completion itself
    unrelated += 1;
    ...
}
```

`eaten` is a bitmap used to re-arm HID endpoints. A **hub** completion filed into it is dropped: the
bit is never read for a hub slot, and the completion code is never stored at all.

The pass is ordered `serve -> drain -> hub probes`, so:

1. Pass N: a probe posts its GET_STATUS TD and gives up after `PROBE_ANSWER_MS`.
2. The TD retires a millisecond or so later - during pass N+1's **serve** segment.
3. `await_on_slot`, waiting on the disk, eats it as "unrelated" and discards it.
4. The drain never sees it. `late` stays 0. The probe that asked never gets an answer.

It is **self-sustaining**: with steady disk traffic (98 messages in 61 s at an idle prompt) there is
almost always a serve segment to swallow the previous probe's answer, so probes keep timing out.

**And the 151 "ok" are not necessarily correct.** A timed-out probe leaves an **unretired TD** on the
hub's EP0 ring. The next probe posts a second TD behind it, then accepts the first completion whose
`sid` matches the hub - which is the **previous** TD, answering about a **possibly different port**,
with `DATA_BUF_OFF` holding whatever that transfer wrote. This is a one-behind lockstep desync: the
identical failure `fs` had ("run `ls` twice and it is out of step", `project_fs_reply_correlation`),
fixed there the same way this must be.

## 3. Why hot-plug specifically dies

Everything on the Pi 4 is behind the internal hub, so arrival and removal are visible **only** through
these probes. The driver is otherwise behaving correctly and loudly:

```
xhci: disk port 1 unreachable 20x - NOT concluding removal (a failed probe is not an answer)
```

That guard is right (`9af9ab4b`) and must stay: a failed question is not an answer, and concluding
removal from one would spuriously unbind a working device. The fault is upstream - the question never
gets answered.

## 4. The fix

**A completion must be delivered to the consumer that asked for it, not to whoever reads the ring
next.** Three parts; all bounded, no heap (§26.6.1).

### 4.1 Widen `eaten` into a completion mailbox

`eaten: &mut u32` is already threaded through every consumer (`hub_port_status`, `await_on_slot`,
`bot`, `read10`/`write10`/`sync_cache`, `serve_if_block`), so the plumbing exists.

```rust
pub(crate) struct EvMail {
    pub have: u32,        // existing re-arm bitmap semantics, unchanged
    pub cc:   [u32; 32],  // the completion code, per slot - fixed, indexed by slot id
}
```

- Every consumer that takes an event for a slot it is not waiting on calls `mail.put(sid, cc)`
  instead of dropping it.
- Every waiter calls `mail.take(slot)` **before** polling the ring, and returns that if present.
- `have` keeps its current meaning for the HID re-arm loop. HID slots are never `take`n (nothing
  waits on them), so their bits still persist for that loop - the existing behaviour is unaffected.

### 4.2 Handle the stale TD, do not just collect it

A mailed completion may be **stale**: it answers the previous probe, and `DATA_BUF_OFF` may since
have been overwritten. Collecting it blindly swaps one wrong answer for another.

Track, per hub, whether a posted TD is still unretired. Before posting a new probe:

- if a TD is outstanding, **collect its completion and discard it explicitly** (from the mailbox
  first, then a short ring poll), then post fresh;
- only then trust the next completion as this probe's answer.

This resynchronises the lockstep rather than letting it accumulate, and is what `fs` does with a
stale reply (`drain_stale_fs_replies` in the shell is the same idea from the client side).

### 4.3 Give the probe its own data buffer

`hub_port_status` reuses `DATA_BUF_OFF` with the comment "unused by the poll loop, so safe to reuse
here". That stopped being true the moment a completion could be collected a pass later. Give the
probe a dedicated 4-byte slot in the DMA arena so an in-flight probe's data cannot be clobbered by
anything else.

## 5. What this buys

- **Hot-plug works** - arrival and removal are noticed within `HUB_POLL_MS` instead of never.
- **The probes stop lying** - no more one-behind answers about the wrong port.
- **~2845 ms per minute of wasted waiting disappears**, because probes get answers instead of
  burning their budget. `PROBE_ANSWER_MS` stops mattering, and with it the last of the typing
  stutter (the probe budget is dead air in the loop that synthesises auto-repeat).

## 6. Test plan

On hardware (QEMU emulates no PCIe on raspi4b, so none of this reproduces there):

1. **The counter is the acceptance test.** `probes ok/posted` must go to ~1.0, and the hub segment
   must fall from ~2800 ms/min to near zero. Do not accept the change without it.
2. Unplug and replug the keyboard - INFO both ways, and the keyboard works after.
3. Unplug and replug the stick - `drives` reflects it both ways.
4. Hold a key down during 2 and 3; the repeat stream must not hitch.
5. `chaos max-carnage` 100 rounds plus `selfcheck` at 0 failures - the merge gate.

## 7. Notes for whoever builds it

- **Do not widen `PROBE_ANSWER_MS` as a shortcut.** The wait is dead air in the input path; that is
  the stutter, and the budget is not the bug.
- **Do not remove the "a failed probe is not an answer" guard.** It is the only reason a spurious
  unbind is not already happening.
- The 42 disk requests/second at an idle prompt (98 messages in 61 s) are unexplained and are what
  keeps the serve segment busy enough to steal every answer. Worth chasing separately - the bug
  above is real regardless, but that traffic is what makes it fire constantly.
- `services/fs/src/main.rs` (the `tag` handling) is the in-tree reference for correlation, and
  `docs/net-tags-design.md` is the same problem one layer up. Read both first.


---

## 8. Residual: the endpoint reaches EP State 4 (Error), and re-enumeration is the right answer

Occasionally - roughly once a minute under deliberate hot-plug hammering, never in quiet running - a
probe's completion comes back `cc 5` (TRB Error) and the hub's EP0 lands in **EP State 4 (Error)**.

Getting to that sentence took three wrong guesses, each costing a hardware round-trip:

1. "The endpoint is halted" -> Reset Endpoint -> `cc 19` Context State Error. Reset Endpoint is legal
   only from Halted, so it was not halted. The log had been *asserting* "endpoint stays halted".
2. "Then it is running" -> Stop Endpoint -> `cc 19` again. Not running either.
3. Stop guessing. The state is a FIELD - three bits at the bottom of the endpoint context's first
   dword (xHCI 6.2.3), in memory the driver already owns. One load: **state 4, Error**.

**No endpoint command repairs the Error state.** Reset Endpoint and Stop Endpoint both refuse it by
design; the defined recovery is to rebuild the endpoint, which for this driver means re-enumeration.
So the fallback that had been running all along - and which was being logged as
`endpoint reset FAILED - falling back to a full re-enumeration` - **was the correct repair**, reported
as a failure. It recovers cleanly every time; hot-plug kept working across all four occurrences in one
run. The messages now say what is actually happening (§26.7 cuts both ways: a recovery that works must
not be reported as a failure).

The repair is now state-driven rather than hypothesis-driven:

| EP state | Repair |
|---|---|
| 2 Halted | Reset Endpoint, then Set TR Dequeue |
| 1 Running | Stop Endpoint, then Set TR Dequeue |
| 3 Stopped | nothing first - this is already what Set TR Dequeue requires |
| 0 Disabled / 4 Error | not repairable in place; re-enumerate (the defined recovery) |

**What is still open** is why `cc 5` happens at all - a TRB the controller judges malformed, so ours.
The prime suspect remains the hub's EP0 ring being shared between two producers with different
disciplines: enumeration's `hoff` sweeps 176..0xF00 writing a hardcoded cycle bit of 1, while the poll
loop's probe walks its own cursor with a `pcs` that toggles on wrap. Seeding the probe cursor from
`hub_off` (both paths now do this) removed the common case; the rare one likely needs the two
producers separated outright - a dedicated probe region the enumeration path never touches.

Worth doing, but it is a latent-fault hunt with a working recovery underneath it, not an outage.

## 9. Method note

Three times in this investigation, measuring beat reasoning in a single step, after reasoning had
already failed: the probe counter (which mechanism), the segment timing (where the time went), and the
endpoint-state read (what state it was actually in). Each followed a wrong hypothesis that had sounded
convincing.

The rule earned here: **when a hypothesis about hardware state turns out wrong, do not form a second
one - find where the hardware records the answer and read it.** The controller knows its own endpoint
state; there was never a need to infer it from which commands it rejected.
