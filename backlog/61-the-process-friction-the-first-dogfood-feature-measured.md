# 61 - the process friction the first dogfood feature measured

**Status:** OPEN - six findings, none blocking, four of them cheap. Recorded because this was the first
feature built *after* the rules became findable, so the friction it hit is data about the process rather
than about the feature.
**Found:** 2026-09-27, implementing the `wifi` verb (`1602f3df`). Each item below cost real time in that
one increment, and each is written with what it cost rather than as a preference.

**The headline is not a complaint.** Six gates fired on one small feature and every one was right,
including two nobody would have caught by review: that the verb existed while no `help` listing mentioned
it, and that the surface spec's home is decided by whether the shell answers the verb. The process
worked. What follows is where it cost more than it needed to.

## 1. x86 has no `--cmd`, so the only platform available had no exploratory loop

`scripts/arm_run.py`, `scripts/pi4_run.py` and `scripts/riscv_run.py` all take `--cmd "<line>"` and type
it into the shell in QEMU. **`osdev run` takes `--smp` and nothing else.** So on x86 - the one port that
needs no board - there is no way to try a command by hand.

The capability exists: `osdev/src/shell_test.rs` types commands over serial all day. It is simply not
exposed. The cost here was that the first version of the verb could not be tried at all before being
committed to a suite; the only way to see it run was to write assertions, which is the right END state
but a poor way to find out that a message reads badly.

**Next step:** expose what `shell_test.rs` already does as `osdev run --cmd`, repeatable, matching the
three scripts' flag exactly so muscle memory transfers. This is the "gate the path I actually use" family
of problem, one layer up: the convenience exists on three ports and not on the one used for development.

## 2. A new utility has eight registration sites, and you find the ones you missed by failing

`SUBCMD_FIRST`, `SUBCMD_SECOND`, `NO_PATH_CMDS`, `UTILS`, the command dispatch, the producer dispatch,
the `help_block` arm, and a `Row` in the `help` listing. Eight places in one file for one verb.

Two were missed and both were caught by a checker rather than by a list: `facts_check` reported that no
`help` listing mentioned `wifi` ("an omission ships the feature to nobody who was not watching it being
built"), and the `help_block` arm is policed by `util_help_coverage_problems`. That the gates caught them
is the system working. That there is no checklist to follow in the first place is the friction: the
knowledge of what a utility needs is distributed across the checkers that enforce it and written down
nowhere an author reads.

**Next step:** a short "adding a utility" section in `utilities/0_conventions.md` naming all eight sites.
Better, if it is cheap: derive the list from the checkers that police them, so it cannot rot - the same
argument `foreign_word_check` makes for reading `FOREIGN_HINTS` rather than keeping a copy.

## 3. The `utilities/` versus `docs/` rule is encoded only in an error message

The surface spec moved **three times** - `utilities/` to `docs/` to `utilities/` - because the rule is:
a spec under `utilities/` asserts the shell answers that verb, and once the shell does answer it, a spec
there is *required*. Commandment X enforces both directions, which is exactly right and is what makes the
directory trustworthy.

Nothing states it where an author would look. It is discoverable only by tripping the check and reading
its message, which is a fine way to learn a rule once and a poor way to plan a commit.

**Next step:** one sentence in `utilities/0_conventions.md` and one in `CONTRIBUTING.md`. Cheapest item
here and the one that would have saved the most time.

## 4. The website page count is English prose, so every new page edits a sentence by hand

`website/src/introduction.md` says "Eighty of its eighty-five pages", and `site_check.py` reconciles
those numerals against the filesystem. Adding one page therefore means editing two spelled-out numbers
in a sentence, and the gate can only report the mismatch - `conform` cannot fix it, because prose is on
the judgement side of the line.

This is a restated derived number, which is the Commandment III shape. It is *reconciled*, so it is the
legitimate kind of derived view - but it is reconciled by a human doing arithmetic in words.

**Next step:** either generate the sentence, or describe the counts instead of stating them ("almost
every page here is a rendered view of a file in the repository"), which is what `GS0405`'s own help text
recommends: *"If the number should not be restated at all, describe it instead so it cannot rot again."*

## 5. The gate cascade costs iterations, and some of them were avoidable

Four rounds of edit-run-read for one verb. Round 1: `doc_symbols_check` on a name in prose. Round 2:
Commandment X refusing the spec's home. Round 3: `facts_check` on `help` coverage **and** Commandment X
now firing the other way. Round 4: `doc_refs` on the move's own debris, plus `site_check` page counts.

Some of that cascade is inherent - fixing the home created the dangling reference. But rounds 2 and 3
were knowable at the same moment: the `help`-listing omission had nothing to do with where a document
lived, and `conform --check` reports multiple findings happily. What made it serial is that each fix
changed the tree enough to reveal the next.

**Next step:** probably nothing, and it is recorded so that is a decision rather than an oversight. A
gate that reports everything it can see in one pass is already what `conform` does; a gate that predicts
what its own fix will reveal is a different and much harder thing.

## 6. `conform --check` is always a full sweep

Every run walks the whole tree with all eighteen checkers - the eight documentation ones alone are about
11.5 seconds, measured. It was run roughly eight times in this increment, mostly after editing a single
file that seventeen of the eighteen had nothing to say about.

**Next step:** a path-scoped or `--since <ref>` mode, so the inner loop costs what the change costs.
With one large caveat that must not be lost: **the scoped mode must never be what CI runs**, and a
partial run must say so in its verdict as loudly as the full one names its count. A checker that quietly
examined a subset while printing a clean verdict is the exact failure `conform` was built to prevent -
its own count of how many checks RAN exists for that reason.
