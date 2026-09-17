# 32. The `fs` suites run in no gate, and two of them were red

**Status: the two red suites are FIXED, `osdev test fs-all` runs all fourteen in ~12 minutes, and
`.github/workflows/storage.yml` runs it in CI. ONE THING REMAINS OPEN, and it is not technical: that
workflow is `workflow_dispatch` only, because push triggers across this project are deliberately
paused to conserve CI minutes.**

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

There are **fifteen** suites now, not eleven - `feat/gsfs` added `fs-fuzz`, `fs-hostile`, `fs-time`
and `fs-tear`, which made the problem worse before it made it better. `osdev test fs-all` runs all
fifteen and reports one tally, with each suite's full output kept in
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

**Measured, not estimated: the full run is 11 to 12 minutes**, 14 of 14 green:

```
fs-all: [ 1/14] fs-restart   PASS   32s    ...   [ 4/14] fs-corrupt   PASS  118s
fs-all: [ 6/14] fs-journal   PASS  106s    ...   [ 9/14] fs-frag      PASS   84s
fs-all: 14 of 14 suites passed in ~12 min
```

Four suites account for most of it (`fs-corrupt` 118s, `fs-journal` 106s, `fs-frag` 84s, `fs-large`
and `fs-djournal` 67s each) because each boots QEMU more than once. That shape matters for option 3:
a subset gate does not have to guess, it can just drop the five slowest and keep nine suites in about
four minutes.

1. **Into an existing gate.** Twelve minutes on top of `osdev test shell` roughly triples the
   pre-merge wait, and a gate nobody can afford to wait for gets skipped - which is the failure mode
   this entry is already about.
2. **A workflow on push to `main`**, accepting that it reports after the fact rather than before.
   Twelve minutes is nothing to a runner and a lot to a person, which is the strongest argument of
   the three - and `identity.yml` is already exactly this shape, so it is a copy rather than a
   design.
3. **A subset gate**: the four or five suites that cover the paths most likely to break, in the
   pre-merge gate, with the full run in CI. Needs somebody to choose the subset honestly rather than
   by what is fastest.
