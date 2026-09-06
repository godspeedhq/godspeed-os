# 5. Pi 2 never writes `/clock.last`, so every boot starts at 1970

**Severity:** correctness, degrading gracefully. The failure is LOUD and the clock refuses to lie,
which is the right behaviour - what is missing is the recovery.

## Evidence (Pi 2, 2026-09-05)

```
time: no persisted clock floor on disk - the clock stays unset until the network sets it
time: cannot reach fs to persist the clock floor - the next boot starts with no floor
```

Successful floor writes that boot: **zero**. Failures: one, and then no further attempts.

## Why it matters

The Pi has no RTC, so the only things that can establish the time are the network and the floor
persisted from a previous boot. With the floor never written, every boot begins at the epoch and
stays there until SNTP lands - which on a machine with no cable is forever. `date` correctly
reports the clock as unset rather than inventing a value, so nothing lies; the machine is just
blind to the date longer than it needs to be.

It also makes the selfcheck statement count vary (449 vs 450) depending on whether SNTP beat the
`date` section, which is harmless but confusing until explained.

## What is known

- The Pi 4, same code, **does** persist it: `time: adopted clock floor 1788643817 from /clock.last`.
  So the mechanism works and this is specific to the Pi 2's conditions.
- The failing call is `time` -> `fs`, and it happens once, early, during a window when `fs` may not
  be serving yet (Pi 2 storage is a USB stick behind `dwc2`, which enumerates slowly).
- There is no retry: the attempt is made, it fails, it is reported, and nothing tries again.

## Next step

Cheapest correct fix: `time` re-attempts the floor write when it next has both a real clock reading
and a reachable `fs`, rather than once at startup. That is the same "reacquire AND re-establish"
shape as docs/observability.md 13, and needs no kernel change - it is service-local.

Worth confirming first whether the single attempt is at `time`'s startup or at first clock set; if
the latter, the retry has an obvious hook.
