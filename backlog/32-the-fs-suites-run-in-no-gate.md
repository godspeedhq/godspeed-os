# 32. The eleven `fs` suites run in no gate, and two of them were red

**Status: the two red suites are FIXED. The gap that let them stay red is OPEN.**

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

## What is NOT established

Whether the workflows would have caught it. `.github/workflows/` has `build`, `identity`, `fuzz`,
`coverage`, `mutation`, `release` and `pages`; whether any invokes the fs suites has not been checked,
and the local gates plainly do not.

## Next step

Decide where they belong, and the options differ in cost rather than in value:

1. **A `fs` meta-suite** (`osdev test fs-all`) that runs the eleven and reports one tally, so a person
   has a single thing to run and CI has a single thing to call. Cheap, and it does not make anything
   automatic on its own.
2. **Into an existing gate.** They are slow (each boots QEMU with an AHCI disk, most take 30 to 90
   seconds), so folding them into `osdev test shell` would roughly double the pre-merge wait.
3. **A workflow on push to `main`**, accepting that it reports after the fact rather than before.

The honest constraint is that these are minutes of QEMU, not seconds, and a gate nobody can afford to
wait for gets skipped - which is the failure mode this entry is already about.
