# 2. GodspeedOS on ONE core - does it actually work, and what has been hiding behind SMP?

**Severity:** feature, and an audit of everything "put it on another core" ever settled.
**Status:** first experiment run 2026-09-06. It BOOTS. What that proves is narrower than it looks.

## Why this is worth doing

Several times, a scheduling or liveness problem has been answered by moving a service to a
different core. That is not a fix; it is a workaround that depends on a second core existing to
absorb the fault. A single-core boot removes the absorber, so anything that was only ever
"solved" by spreading load has to fail honestly instead.

The constitution already claims this configuration works. 11.3:

> AP startup: kernel logs warning, continues with available cores; **if zero APs come up, system
> runs as single-core**

So this is not a new feature request. It is checking a promise the document already makes.

## What the first run showed

`osdev run --smp 1`, identity build, ran for the full 180 s window:

- It boots. `supervisor`, `events`, `console`, `control`, `shell`, `block-driver`, `nic-driver`,
  `net-stack`, `hw-enumerator`, `xhci` all start, all on core 0.
- Cross-core IPC still works, because there is nothing to cross: `pong: received "1224"` by the
  end of the window, so ping/pong sustained ~1200 exchanges on the one run queue.
- No kernel panic, no liveness wedge in the window.
- Every task landed on core 0, including the ones that asked for another core - see item 1, which
  this experiment is what found.

## What it did NOT show, and must not be read as showing

- **`fs` never started, and that is the BUILD, not single-core.** This was the identity build,
  which is probe-heavy and carries no `fs`. A single-core test of the storage stack has not been
  run at all yet.
- 180 s is a window, not a soak. Starvation and priority inversion are exactly the faults that need
  longer than that to show.
- No chaos was run single-core. `chaos max-carnage` on one core is the interesting case, because
  every kill and respawn now contends with the shell for the same quantum.

## What the contracts currently pin

```
supervisor, console, shell, chaos, dwc2  -> core 0
fs                                       -> core 1
xhci                                     -> core 2
ehci                                     -> core 3
```

Each of these needs the same question asked of it: **is this a property of the work, or a
workaround that happened to help?** 9.2's own guidance is "contract authors should specify
placement only when they have a real reason". Whatever the answers, they should be written into
the contracts as comments, because right now the reason for `ehci -> core 3` is not recorded
anywhere.

## Next steps, in order

1. Settle item 1 first. Until `PlacementInvalid` is enforced or 9.2 is amended, a single-core run
   silently ignores every pin, so nothing learned about placement is trustworthy.
2. Run the STORAGE build single-core (`osdev test script --smp 1` equivalent) and get a selfcheck
   tally. That is the real test: `fs` and `block-driver` on one queue with the shell.
3. Run `chaos max-carnage 100` single-core. Expect this to be the one that finds something.
4. For each pinned contract, record the reason or remove the pin.
