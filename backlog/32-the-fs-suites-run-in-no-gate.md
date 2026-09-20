# 32. The `fs` suites run in no gate, and two of them were red

**Status: `osdev test fs-all` runs 25 suites in ~31 minutes, 25 of 25 green (2026-09-20), and
`.github/workflows/storage.yml` runs it in CI. ONE THING REMAINS OPEN, and it is not technical: that
workflow is `workflow_dispatch` only, because push triggers across this project are deliberately
paused to conserve CI minutes.**

**AND THE THESIS HAS NOW BEEN PROVEN TWICE.** The sweep on 2026-09-20 found TWO MORE suites sitting
red - `fs-tear-detect` and `fs-tear` - neither of which was a filesystem fault, and neither of which
anything would have reported. Details at the end; the short version is that this entry's argument is
no longer a prediction.

## What happened

`feat/gsfs` opened by sweeping every `fs` suite before touching anything, to establish a baseline.
Two of the eleven were failing **on `main`**, at the commit released as v0.18.0:

| suite | failing check | why |
|---|---|---|
| `fs-check` | `free count rebuilt from the tree to the correct value` | the expected count is read from the superblock BEFORE boot, but the running system then creates `/clock.last` and `.gsh_history`, each taking a block |
| `fs-corrupt` | `case2: directory CRC mismatch caught (loud)` | matched the literal phrase `"directory block CRC mismatch"`, which is the NON-ROOT wording; the case corrupts the ROOT block, whose path says `"CRC mismatch on directory block ..."` and then a root-specific refusal |

**Neither was a filesystem bug.** In both cases `fs` behaved correctly and the ASSERTION had gone
stale - `fs-check`'s because two features (the `time` service's persisted clock floor, and shell
history) arrived after it was written, `fs-corrupt`'s because it pinned one of two correct sentences.

Both are fixed, and fixed to assert the BEHAVIOUR rather than a number or a sentence that boot-time
noise and a reworded log can invalidate.

## The part that is not fixed

**No gate that runs before a merge includes any of the eleven `fs` suites.** v0.18.0 was verified with
`osdev test identity` (24/0), `osdev test shell` (174/0), both TCP suites, 15 checker scripts and the
Commandments - and none of those touches `fs-check`, `fs-corrupt`, `fs-frag`, `fs-journal`,
`fs-djournal`, `fs-large`, `fs-ioretry`, `fs-scrub`, `fs-compat`, `fs-restart` or `file-cap`.

So the storage stack - the one subsystem whose failure mode is **losing the user's data** - has the
deepest test coverage in the project and the least automatic attention. The suites exist, they are
good, and they are run by hand when someone is working on storage. Between those times they rot
quietly, which is exactly what happened here.

**A red test nobody looks at is worse than no test**, because it trains a reader to discount red. Two
of eleven is enough to start that.

## Established since: the workflows would NOT have caught it either

Checked rather than assumed. Of the seven workflows, exactly two run any `osdev test` at all:

| workflow | runs | fires on |
| --- | --- | --- |
| `identity.yml` | `test identity` | push, manual |
| `fuzz.yml` | `test fuzz`, `test fuzz-brutal` | push, manual |
| `build`, `coverage`, `mutation`, `pages`, `release` | no `osdev test` | - |

**Not one `fs` suite runs anywhere in CI.** So the two red suites were invisible to every gate the
project has, local or remote, and would have stayed red indefinitely - they were found only because
this branch happened to sweep them before starting work. That is worth stating plainly: the coverage
existed and the reporting did not, which is the whole of this entry.

## Step one is DONE: `osdev test fs-all`

There are **25** suites now, not eleven. `feat/gsfs` added `fs-fuzz`, `fs-hostile`, `fs-time`,
`fs-tear`, `fs-tear-detect`, `fs-model`, `fs-window`, `fs-churn`, `fs-blockchaos`, `fs-blockdeath`,
`fs-dupop`, `fs-cache` and `fs-lyingflush` - which made the problem worse before it made it better.
`osdev test fs-all` runs all 25 and reports one tally, with each suite's full output kept in
`build/tests/fs_all_<name>.log`.

**`fs-tear` roughly doubles the run** (it boots QEMU once per tear point, ~55 of them plus a
recording boot per operation and a control), which sharpens the trigger question below rather than
changing it. It is in the list anyway: a suite sitting outside "every fs suite" is exactly how the
two red ones went unnoticed.

Each runs as a **subprocess**, deliberately: the suites call `std::process::exit` on failure, so
running them in-process would let the first failure kill the run and hide every suite after it -
which is precisely the shape of problem this entry is about. Isolated, one failure costs one line and
the rest still report. The order is cheapest-first, so a broken build or a broken mount shows up in a
minute rather than at the end.

It reports each suite's **own tally** rather than just `ok`, because a number is what reveals a suite
that has quietly stopped asserting - a `PASS 5/0` where it used to be `11/0` is a regression that a
green tick hides.

**This makes nothing automatic, and that is the honest limit of it.** What it removes is the excuse:
running the storage stack is one command now instead of fourteen, and a workflow has a single thing
to call.

## Built: `.github/workflows/storage.yml`

Runs `osdev test fs-all`, and on failure uploads the WHOLE of `build/tests/` - each suite's output
and its serial capture - because a storage failure is diagnosed from the serial log rather than the
tally, and a QEMU run that is gone cannot be re-read.

**It is `workflow_dispatch` only, and that is a decision rather than an omission.** `identity.yml`
carries the note *"paused: re-add push/branches: [main] when CI minutes are available"*. A 12-minute
job on every push would spend precisely what that note is conserving. So this matches the house
state, and the comment at the top of the file says to re-add the push trigger at the same time as
identity's.

Which leaves the honest residual: **until those minutes exist, this still depends on somebody
choosing to run it.** That is better than fourteen commands and worse than a gate, and pretending
otherwise would be the same optimism that let the suites rot.

## What is still open

The trigger above. And, if the minutes stay scarce, which shape to spend them on:

**Measured, not estimated (2026-09-20): the full run is ~31 minutes**, 25 of 25 green:

```
fs-all: [16/25] fs-tear-detect PASS   30s
fs-all: [17/25] fs-tear        PASS  547s
fs-all: 25 of 25 suites passed in ~30 min
fs-all: the storage stack is green
```

**ONE SUITE IS 29% OF THE RUN.** `fs-tear` costs 547s by itself - it boots QEMU once per tear point,
75 of them across four operations, plus a recording boot per operation and a control. The next five
together (`fs-corrupt` 117s, `fs-journal` 106s, `fs-lyingflush` 92s, `fs-cache` 92s, `fs-frag` 84s)
come to 491s.

That single number decides option 3 rather than leaving it to judgement: **dropping `fs-tear` alone
takes the run from 31 minutes to 22**, and dropping the top six leaves 19 suites in about 12 - the
budget this entry was originally written against. A subset gate does not need somebody to choose
honestly between twenty-five suites; it needs one decision about the tear sweep.

1. **Into an existing gate.** At twelve minutes this was arguable. At **thirty-one** it is not: a
   gate nobody can afford to wait for gets skipped, which is the failure mode this entry is already
   about. Struck rather than deleted, because the reason it died is the measurement.
2. **A workflow on push to `main`**, accepting that it reports after the fact rather than before.
   Thirty-one minutes is nothing to a runner and impossible for a person, so the growth has made
   this the strongest of the three by some distance - and `identity.yml` is already exactly this
   shape, so it is a copy rather than a design.
3. **A subset gate**: the pre-merge gate runs everything EXCEPT `fs-tear`, with the full run in CI.
   The measurement above turns this from a judgement call into one: 24 suites in ~22 minutes, or 19
   in ~12 if the next five go too. The honesty risk this option always carried - choosing the subset
   by what is fastest rather than by what breaks - is smaller when the cut is one suite whose cost
   is structural (a boot per tear point) rather than five chosen for convenience.


---

## 2026-09-20: swept again, and it happened AGAIN

The first sweep of the day reported **24 of 25**, and the second found a second failure once the
first was fixed. Neither was a filesystem fault. Neither would have been reported by anything.

| suite | what failed | why |
|---|---|---|
| `fs-tear-detect` | 4 passed, 3 failed | it ran `churn 8` under a comment asserting eight seconds produced a multi-block file. Under the load of 25 back-to-back suites it did not, and `churn tear` had nothing large enough to tear. **A fixed DURATION standing in for a COUNT** |
| `fs-tear` | 17 passed, 14 failed | its `move` case waited for the marker `"entries"` while its own oracle declares `(empty)` legal - and `dir` prints an empty directory with no header and no count. After a torn move the destination is usually empty, so the probe had its answer and timed out anyway. **17 of 17 move tear points reported TIMEOUT, and `delete` never ran at all** |

`fs-tear`'s is the one worth dwelling on. The doc recorded it at **18/0 as of 2026-09-18** while it
was red, which is a document asserting a guarantee that did not hold. It was found only because the
sweep ran it - and it had been red long enough that an earlier session's log (`fstear6.log`) shows it
passing 18/0 across 75 tear points, so `dir`'s empty-directory wording changed at some point and
nothing re-ran the suite that cared.

**Both were caught by the sweep this entry exists to justify, and by nothing else.** The two original
red suites could be read as a one-off; four across two sweeps is a rate. The entry's claim - that the
storage stack has the deepest coverage and the least automatic attention - is now measured rather
than argued.

One correction to the framing above, in fairness to the tests: all four failures were **test faults,
not filesystem faults**. That is not reassuring, it is the point. A suite that fails for its own
reasons is the one nobody investigates, and it trains exactly the reflex the entry names - discount
red, re-run, move on.
