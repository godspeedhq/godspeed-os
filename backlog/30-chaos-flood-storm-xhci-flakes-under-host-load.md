# 30. `chaos: flood-storm xhci` fails intermittently in `osdev test shell`

**Status: OPEN, observed and not diagnosed.** Recorded because it was seen repeatedly in one day and
dismissed as host load each time, which is exactly how a real intermittent bug stays invisible.

## What happens

Two assertions in `osdev test shell` fail together:

```
shell-test: FAIL - chaos: flood-storm xhci - no-controller idle drains, PASS (the gap the sweep missed)
shell-test: FAIL - chaos: flood-storm xhci - survived all 5 (drained, not clogged)
```

## How often

**Twice in four runs on 2026-09-16**, on the same build, interleaved with clean 174/0/2 runs. Each time
it did NOT reproduce on an immediate re-run. Nothing else in the suite failed with it.

## What is NOT established

Whether it is host load or a real intermittent defect. The machine was running QEMU suites and release
builds back to back all day, so load is plausible - but "plausible" is the whole problem. Two facts
argue for looking harder rather than writing it off:

- The pair fails TOGETHER, which is a shape, not noise.
- A re-run passing is consistent with a timing-sensitive bug as much as with host load, and those are
  the ones that reach hardware.

Related, and a reason to be suspicious of a "flake" verdict here specifically: the kernel is known to
SPLICE one log line into another under load (`project_serial_splice`), which produces both false FAILs
and false PASSes in harness assertions that match on serial text. Whether these two assertions are
matching text that a splice could corrupt has not been checked.

## Next step

Run the suite in a loop on an idle machine and count. If it never fails idle and fails under load, it
is a harness timing bound to widen (and to state as a bound). If it fails idle, it is a bug in the
xhci flood-drain path. Either way the answer is cheap; the dismissal was what cost nothing and proved
nothing.

## Not to be confused with

`backlog/28` and `backlog/29`, which are net-stack. This is the USB host driver under a deliberate
message flood, and no change on `feat/tcp` touches it.
