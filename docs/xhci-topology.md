# The xhci service needs a topology model

## Why this document exists

Seven fixes over two days, each correct, each revealing another face of the same problem. The pattern:

| Symptom reported | What was actually wrong |
|---|---|
| keyboard dead on the blue port | EP0 max packet, then TT Think Time, then port power settle |
| keyboard dies after a while | unbounded IPC drain; then per-request logging; then a pessimistic probe timeout |
| unplug shows no INFO | the drop path never ran |
| `drives` lies | `fs` cached capacity; then the shell printed a row for zero |
| disk flaps 72 times | one noisy probe treated as removal |
| replug invisible | `hub_tried` never cleared |

Every one was a real defect. None of them was THE defect. The service has no answer to "what is
attached right now" - it has a dozen branches that each infer a piece of that answer from a different
signal, and the branches contradict each other.

The in-kernel driver did not have this problem because it was written as a whole. This port grew
branch by branch under symptom pressure, and each patch added a special case that the next patch had
to work around.

## The evidence that it is structural, not a bug count

The last boot produced ZERO lines from the absence counter, the drop path, and the arrival path. Not
a wrong threshold - none of that code executed. The probe is gated on `disk.is_some()`, so a machine
booted with no stick can never observe a stick arriving; and `hub_tried` is cleared only on a
definite `Some(false)`, so a port whose probe FAILS is never re-examined.

Two independent gates, each reasonable alone, that together make an entire class of event
unobservable. That is what "no model" looks like from the outside.

## The model

One table. One observer. Actions hang off transitions, not off signals.

```
PortState = Empty | Attached { kind, slot } | Unknown
```

For every port the service knows about - every root port, and every downstream port of every hub it
has enumerated - hold:

- `state: PortState`
- `evidence: i8` - a small hysteresis counter, NOT a per-symptom counter

### One observer

A single `observe_topology()` runs on its own cadence and asks every known port its status. It is
NOT gated on anything being bound: a port with nothing in it is exactly the port an arrival will
appear on. This is the gate that broke everything above.

It updates `evidence` and, when evidence crosses a threshold, changes `state` and RETURNS the
transitions. It performs no binding, no dropping, no announcing.

### Evidence, in one place

Today there are three separate rules - `Some(false)` needs 2, `None` needs 20, a failed block
operation needs 1 - living in three functions. They disagree, and two of them can fire for the same
physical event.

One rule: a probe answering "present" is +1 toward Attached, "absent" is -1 toward Empty, and a
FAILED probe is 0 - no evidence either way, because a failed question is not an answer. A failed
block operation is likewise a hint to re-observe, never a removal in itself. Transitions need the
counter to saturate, which is what makes a single bad read harmless without needing a special case.

### Actions on transitions

```
Empty     -> Attached   enumerate, bind, announce "connected"
Attached  -> Empty      drop, announce "disconnected", tell nobody else
Unknown   -> anything   observe again; never act on Unknown
```

Every announce and every drop lives here, once. Today they are scattered across three call sites and
one of them is unreachable.

## What this deletes

- `hub_tried` - the retry mask exists because "connected" was read as "arrived". With a state
  machine, arrival IS the transition, and a port that stays Attached generates nothing.
- `disk_absent_seen` - subsumed by `evidence`.
- the `ndev == 0` special case for scanning the disk's hub - the observer scans every known hub
  regardless of what is bound.
- the "disk stopped answering" removal path in `serve_if_block` - a failed I/O requests an
  observation; it does not decide.
- the boot-pass `announce` flag - a boot device is `Unknown -> Attached`, which can be classified at
  the transition rather than by a global "is this the first pass" flag.

That is five special cases replaced by one table, and every one of them has caused a reported bug.

## What it does not fix

Input latency. That is `docs/xhci-split.md` - the block server and the HID poll sharing a pass - and
it is orthogonal. Do not conflate them; doing both at once is how a rewrite fails.

## Order of work

1. Add the table and `observe_topology()` alongside the existing code, logging transitions only.
   Boot it. The log alone will show whether the model sees what actually happens - including the two
   cases that are currently unobservable (boot-with-no-stick, replug-after-failed-probe).
2. Move announcements onto transitions. Delete the scattered notify calls.
3. Move bind/drop onto transitions. Delete `hub_tried`, `disk_absent_seen`, the `ndev == 0` case, and
   the `serve_if_block` removal path.
4. Only then revisit the latency split.

Step 1 changes no behaviour and is the whole point: it makes the current behaviour OBSERVABLE before
anything is restructured. Every fix in the table at the top of this document was made without that.
