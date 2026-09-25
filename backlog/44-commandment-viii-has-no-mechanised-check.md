# 44. Commandment VIII is the only one nothing automated watches

**Status: OPEN, noticed 2026-09-23.** Not a defect, an absence - and one with a same-day example, so
it is written down rather than left as a nice observation.

## The fact

`scripts/commandments.py` carries 21 checks across the ten commandments. They cover **I, II, III, IV,
V, VI, VII, IX and X**. Commandment VIII - *"Thou shalt not rely upon timing for correctness. Wait
for truth, not time"* - has **none**.

That is defensible: it is the hardest of the ten to express mechanically. "This code waits on a
clock where it should wait on a fact" is a judgement about intent, and the other nine are mostly
about the presence or absence of a construct.

## Why it is worth an entry anyway

`backlog/43` was opened the same day, and it is a Commandment VIII problem by its own words. Two full
`fs-all` sweeps returned 31 of 33 and failed DIFFERENT suites; the second pair failed because the
host was starved, not because anything was wrong. Suites were asserting on elapsed time rather than
on the work completing.

So the one commandment with no automated check is the one that produced the only unreproducible test
result on the branch. That is not proof of anything, but it is the kind of coincidence worth
recording before it happens a third time.

## What might actually be catchable

Not the general case. But some of it is a construct after all:

1. **A test that fails on a deadline rather than on an outcome.** `osdev`'s suites already carry
   their timeouts as data; a check could require that a suite distinguishes "the deadline passed"
   from "the assertion was false" in what it REPORTS, which is exactly what `backlog/43` asks for.
2. **A retry driven by a timeout.** `IX-peer-reacquire`'s docs already say this is the bug it cannot
   see: "a reacquire must be driven by `Err` and never by `Ok(None)`". The pattern
   `Ok(None) => ...retry...` is greppable, and `IX-stdlib-delegates` proved that shape of check
   works - it asserts the standard library reacquires on `SendFailed` specifically.
3. **A bound expressed as a COUNT where a duration is meant.** The repository already knows this one:
   "a count is not a duration" appears in `net-stack` (a DNS resolve bounded by the client's patience
   rather than by attempts) and in the `fs` deadlines.

(3) is probably the weakest and (2) the strongest, because (2) is a mechanical shape with a known
correct form sitting next to it.

## Next step

Nothing, deliberately, until someone wants it. This exists so the observation is not lost and so the
next person hitting a timing-shaped flake finds the prior art rather than re-deriving it.
