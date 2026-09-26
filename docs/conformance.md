<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Conformance: `osdev conform`, and the Python question

**Status:** BUILT as `scripts/conform.py`; the `osdev conform` shim is not written yet (it needs a
Rust edit, and this branch deliberately touches none). Branch `feat/osdev-conformance`. Trails
`CLAUDE.md`, which wins on any conflict.

    py scripts/conform.py            fix what is decidable, report what needs judgement
    py scripts/conform.py --check    report both, change nothing (what CI wants)
    py scripts/conform.py --explain GS0303
    py scripts/conform.py --list

This proposes a front door for the enforcement layer, and answers - with measurements rather than
taste - whether that layer should stay in Python.

## 1. The problem, which is not the one it looks like

There are **40 Python scripts, 10,425 lines** under `scripts/`, and `osdev`'s `EXTRA_CHECKS` runs 15
of them on every build. The rules they enforce are real, mechanised, and good. What is missing is a
**door**:

- A contributor cannot ask *"am I clear?"* without compiling a kernel, because the checks run as a
  side effect of `osdev build`.
- There is no single verdict. Fifteen subprocesses print fifteen things.
- The rules are enforced but not **discoverable**. You learn them by failing a build, which is
  exactly what §22.7 (the Stranger Test) says the repository should not require: *"a stranger should
  fall into the supported model because it is the path of least resistance."*

Running them as part of the build is RIGHT and must not change - unavoidable beats optional, and the
2026-09-26 audit happened because eight documentation checkers ran only at release. The gap is that
there is no way to run them ALONE, fast, and be told one thing.

## 2. One verb: `osdev conform`

`fmt` was the first guess and it is wrong twice. **It is already taken** - `gsh fmt` is the
shell-script formatter that shipped in v0.4.0 - and **it promises the wrong thing**: `gofmt` never
tells you your design is wrong, whereas half of what this tool reports is judgement.

`conform` is the better name, and it is already this project's word: `utilities/0_conventions.md`
ends every spec with a Conformance section, and this document is `docs/conformance.md`.

The objection to it is real and is the interesting part: **"conform" implies CHANGING, not asking.**
That is true, and the resolution is to make it the design rather than fight it - one verb that fixes
what is decidable and reports what is not, plus the flag CI needs:

| invocation | does | writes? | exit |
|---|---|---|---|
| `osdev conform` | fixes what is DECIDABLE, reports what needs JUDGEMENT | yes, mechanical only | non-zero if anything was reported |
| `osdev conform --check` | reports both, changes nothing | never | non-zero if anything would change OR any rule is violated |

`--check` is the `cargo fmt --check` and `terraform fmt -check` convention, so a contributor already
knows what it means.

### The line that makes one verb honest: decidable vs judgement

This is the whole design, and getting it wrong would reproduce the failure this document keeps citing.

**Decidable** means there is exactly one right answer and no reader is needed: an em-dash must be a
hyphen, a CRLF must be an LF, `cargo fmt` has one output. `conform` fixes these silently-but-audibly
- it changes the file and names it.

**Judgement** means the fix is not determined by the violation. A comment naming a dead symbol might
want the comment corrected, or might be correctly recording a removal and want baselining - and only a
human knows which. Half of what the 2026-09-26 comment sweep found was the second kind. `conform`
must never guess at these, and must never let their existence be implied by silence.

So the output always separates them, and says which it did:

```
conform: fixed 3 (mechanical), 2 need a decision - 19 checks ran, 17 passed
```

A run that fixed everything and reported nothing prints `fixed 3, 0 need a decision`, not a bare
`ok` - because "I changed your files and said nothing" is the surprise that makes people distrust a
formatter.

**Never run `commandments_redteam.py` from this verb.** It plants `static mut SNEAK`, a `sneaky-mode`
feature and `ctx.spawn("probe-recv")` into source and restores with `git checkout`, which would
destroy a contributor's uncommitted work. It proves a checker CAN fail and is a maintainer tool,
reachable only by explicit opt-in.

### What the fixing half may and may not touch

May: the plain ASCII hyphen for an em/en dash; line endings; trailing whitespace; `cargo fmt` on
Rust; `gsh fmt` on `.gsh`.

**May not: prose. Ever.** No reflowing comments, no rewrapping Markdown, no reordering anything. The
comment prose in this tree is where the project keeps its reasoning, and a formatter that reflowed it
would destroy exactly the seams the 2026-09-26 sweep spent a day repairing - three mangled seams were
found where a previous edit had replaced a clause whose sentence tail belonged to the paragraph. An
em-dash is a single-character substitution and is safe. A line wrap is not.

### What `check` must do that the build does not

- **One verdict, and name the commandment.** A violation prints which of the Ten it breaks and the
  section it rests on, not just a script name.
- **Fast.** The 15 documentation-and-code checkers measure ~11.5 s together today, most of it
  interpreter startup. `conform` must not pay for a kernel compile.
- **Say how many checks RAN**, not only how many failed. A run that silently skipped twelve checkers
  and printed a clean verdict is the exact failure this document exists to prevent.

## 3. The Python question, answered with the tree rather than with taste

**Python is already a hard dependency, and declaring it is overdue regardless of anything else
here.** `osdev` refuses to build when it cannot run a checker - deliberately, and the comment says
why: *"a checker that cannot RUN is not a checker that passed."* And it is not only checks:
`scripts/board.py`, `arm_build.py`, `pi4_build.py` and `riscv_build.py` are the ONLY way to build a
Pi 2, Pi 4 or VisionFive image, and `docs/porting.md` tells a porter to run five `python scripts/...`
commands. So a contributor targeting anything but x86 needs Python today, non-optionally, and the
README mentions it once, incidentally. **A required tool that is not declared is discovered by
whoever trusts the document** - the same shape as every finding in the 2026-09-26 audit.

So the first action is free and independent of the port: **declare it.**

**Minimum version: Python 3.8**, and that is measured rather than assumed. The scan, so the next
person can redo it:

| feature | needs | found |
|---|---|---|
| walrus `:=` | 3.8 | `commandments.py:1107` - **this is the floor** |
| `subprocess` `capture_output=` / `text=` | 3.7 | 5 scripts |
| f-strings | 3.6 | 23 scripts |
| `match` statement | 3.10 | none |
| `str.removeprefix` / `removesuffix` | 3.9 | none |
| `list[str]` in a **local** annotation | 3.9 *if evaluated* | 4 scripts, and it does **not** raise the floor |

That last row is the one worth showing the working for. `violations: list[str] = []` looks like it
needs 3.9, because `list[str]` is only subscriptable from 3.9. It does not: CPython **never evaluates
a function-local variable annotation**, verified rather than argued from the PEP -

```python
def f():
    v: totally_undefined_name[int] = []   # never evaluated, so never a NameError
    return v
```

runs clean. Every one of those four sites is inside a function body. Had any been at module or class
level the annotation WOULD be evaluated and the floor would be 3.9.

**CI does not pin a version** - the only `python-version` in the workflows is `'3.x'` in
`pages.yml`. That should become the declared minimum too, so "it works on my machine" and "it works
in CI" cannot diverge silently.

### Decision: Python stays

Settled 2026-09-26 by the repo owner. It is already required, it works, and the remainder of this
section records WHY a port was considered and what the case was, so the question does not get
re-litigated from scratch in six months.

The case FOR was never dogfooding - it is correctness of the checkers themselves.
Every checker bug found on 2026-09-26 was the failure mode of ad-hoc text munging:

- a corpus that included the checker's own prose, so cited dead names resolved themselves;
- a `comm` comparison across two files written with different line endings, silently empty;
- an instrument that counted `impl` methods as free items because indentation was the discriminator;
- (earlier, elsewhere) a cumulative counter declared inside its loop, reading zero forever.

A checker that silently checks nothing is the worst object in this repository - `doc_symbols_check`
says so in its own docstring about the `stdlib/rust/src` it did not scan for months. Compiled code
with types, unit tests under `cargo test`, and one toolchain (`rustup` and nothing else) is a real
reduction in that class. It would also let `osdev` call the checks as a LIBRARY - no subprocess, no
`python` lookup, and most of that 11.5 s back.

The case AGAINST is cost and risk, and it is not small. 10,425 lines, `commandments.py` 1,949 of them.
More importantly, **these scripts are documentation as much as code** - their docstrings are some of
the best writing in the tree and carry the reason each rule exists and each exemption was granted.
A rewrite that loses those has destroyed the more valuable half. And a port that lands one subtle
behaviour change is worse than the status quo, because the change is silent by construction.

**What would make a port safe, recorded in case it is ever wanted:** one checker at a time, behind a
differential harness that runs both implementations on the real tree and asserts identical verdicts
AND identical reported sites - plus the checker's `commandments_redteam.py` probe must still FAIL the
ported version, because a check that passes everything is indistinguishable from a deleted one.

Neither would the port have been total. Only the ~20 conformance checkers are part of a contributor's
build. The build drivers (`board.py` and the three per-board scripts) are a separate and riskier
question, and the test harnesses and the redteam prober are maintainer tools nobody is forced into.
"Dogfood Rust" is a reason to remove a dependency a CONTRIBUTOR cannot avoid, not a reason to rewrite
every tool a maintainer chooses to run.

### So the only work is the seam

`osdev conform`, shelling out to Python, and the dependency declared in `README.md`,
`docs/porting.md` and CI. That was always the part that delivered the developer-facing win; the port
was the optional half, and it is now declined.

**And the seam turned out not to need Rust at all.** `scripts/conform.py` is the whole thing: it reads
`EXTRA_CHECKS` out of `osdev/src/main.rs`, runs each checker, frames the failures, fixes the decidable
class, and answers `--explain` and `--list`. `osdev conform` will be a shim that shells out to it -
three lines - which means the front door exists and works today, and the Rust is a convenience rather
than the feature.

## As built, and the five defects that only appeared when it was RUN

Every one of these was found by running the tool on the real tree, and none of them by reading the
code. That is the argument for UI fixtures arriving a day early.

**1. The fixer invented its own scope.** First run offered to "fix" 562 files. This is a Windows
checkout and `.gitattributes` says `* text=auto`, so CRLF in the working tree is NORMAL; only `*.sh`,
`*.gsh`, `*.gs` and `boot/**` are declared `eol=lf`. It also stripped trailing whitespace, which NO
gate polices - a rule invented by the tool, which is §26.2 backwards. Fixed by DERIVING scope from the
checkers: it imports `dash_check` for its file set and `line_ending_check` for its `.gitattributes`
rules. **That is now the rule: `conform` may only fix what a gate would fail you for, and it takes the
scope from that gate** - which makes it structurally impossible for `conform` and a build to disagree,
the same reason the checker list is read out of osdev rather than copied.

Note the `.gitattributes` match must be LAST-WINS, as git resolves it: `boot/** text eol=lf` is
deliberately followed by `*.dtb binary` so that marking a directory text does not have git "normalise"
a 58 KB device tree blob.

**2. The dash fixer contained two dashes.** The file was written with the literal em-dash and en-dash
in its own substitution table. Writing them as source escapes does not help either - `dash_check`
catches the escaped form on purpose, because "a dash written as a source escape is invisible to a
literal scan". A tool that must NAME these characters has to compute them: `chr(0x2014)`.

**3. A new file violates nothing until it is staged.** `dash_check` reads `git ls-files`, so while
`conform.py` was untracked its dashes were invisible and the gate passed. `git add -N` is how you find
out before you commit. Worth knowing generally: a contributor's brand-new file is unchecked until it
is at least intent-to-added.

**4. The same problem was reported twice, and counted twice.** A fixable violation appeared both in
the "would fix" list and as a full error frame whose help said "`conform` fixes this" - the least
interesting class taking the most space, which trains a reader to skim. Now it collapses to one line
when the fixer claimed every file, and keeps its full frame when it did not, because a fixable RULE
can fail for a reason the fixer cannot reach (an escaped dash). The summary double-counted it too:
"1 would be fixed, 1 need a decision" for one problem, putting a number in the needs-a-human column
that needed no human.

**5. It garbled the output it was quoting.** `subprocess.run(text=True)` decodes with the LOCALE
encoding, not UTF-8, so every `§` a checker printed came through as a replacement character. A tool
that mangles what it quotes is not one to trust about anything else.

### What the render looks like

A judgement finding, unabridged. The marker below is the escape this document argued for, in use: the sample holds the very citation that provoked it, so the site is legitimately unresolvable.

<!-- conform-ok: GS0304 - a pasted sample of conform's own output; the cited line is the one the example was generated from -->

```
error[GS0303]: a Rust comment names something that exists nowhere in the code
   --> examples/00-hello/src/main.rs:53
    |
    = rule: CLAUDE.md 26.7, 26.14
    = why: A comment is read BEFORE any document, because it sits beside the code being
           changed. There are 27,000 doc-comment lines here and until 2026-09-26 nothing
           checked one of them.
    = help: Name what does the job now - or, if it names something OUTSIDE this tree on
            purpose (a hardware register, an SBI call, a Linux function cited per 26.14), add
            it to the baseline with which kind it is. A comment that says "X was deleted" is
            RIGHT to name X: that is a record, and it belongs in the baseline.
    = note: `py scripts/conform.py --explain GS0303` for the long form
```

and a clean tree:

```
conform --check: 0 would be fixed, 0 need a decision - 17 checks ran, 17 passed
nothing to do. Every rule this project enforces is satisfied.
```

The count of checks that RAN is in every line on purpose. A run that silently skipped twelve checkers
and printed a clean verdict is the failure this whole document exists to prevent.

## 4. The output contract: clear, friendly, and it suggests the fix

The bar is `rustc`. Not its cleverness - its **shape**: say what is wrong in one plain line, show
where, then say what to do about it. A checker that reports a violation without a next step has told
the contributor they are wrong and left them to guess, which is the tooling version of the thing
§26.7 forbids.

### Anatomy

```
error[GS0031]: a comment names `probe_authority`, which nothing in the code defines
   --> services/probe/src/table.rs, at the `probe_authority` mention
    |
    = commandment: VII (no ambient authority) - CLAUDE.md 3.1, 13.6
    = why: a comment is read BEFORE any document, because it sits beside the code
           being changed. This one says the KERNEL keys probe authority by name -
           which is the thing step C removed, because a name-keyed authority let a
           spawn cap obtain INTROSPECT by choosing a string.
    = help: name what does the job now (`probes::privileges_of`, in the supervisor's
            spawn request), or - if it names something outside this tree on purpose
            (a register, an SBI call, a Linux function per 26.14) - add it to
            scripts/COMMENT-SYMBOLS.baseline.txt with the reason
    = note: run `osdev conform --explain GS0031` for the long form

check: 1 error, 0 warnings - 19 checks ran, 18 passed
```

**That example cites a NAME and not a line, deliberately, and this document learned it the hard way:**
the first draft put a real file with a line number in that arrow, and `scripts/line_ref_check.py`
failed the build, because a `path:line` in a live document is a claim and that one no longer held. A
doc cannot opt out of being checked by calling itself an example - and note that the paragraph you are
reading cannot QUOTE the offending citation either, for exactly the same reason, which is a small
honest cost of the gate being unable to tell an example from an assertion. It is also what §4's own "no
synthesised caret" rule implies one level up: cite the precision you have.

Five rules behind that layout:

1. **A stable code per rule** (`GS0031`), so it can be searched, cited in a commit, and explained.
   Codes are never reused or renumbered, exactly as rustc's are not.
2. **The commandment is named, not just the script.** The contributor is being held to a rule; tell
   them which one and where it is written. `commandments.py` already knows this mapping.
3. **`= why` before `= help`.** The reason is what makes the rule stick; the fix without the reason
   teaches nothing and gets worked around next time.
4. **`= help` must be actionable and specific.** Both escape routes get stated, including the
   legitimate one. A gate that only says "no" invites a contributor to disable it.
5. **A cargo-style summary line**, including **how many checks RAN.** A run that quietly skipped
   twelve checkers and printed "0 errors" is the failure this whole document is about.

### Most of this text already exists, and the job is not to lose it

The checkers' own messages are unusually good - `doc_refs` prints a paragraph on why a dangling
backlog citation is worse than no citation at all, and `comment_symbol_check` already prints both
escape routes. What is missing is the FRAME: today fifteen subprocesses each print their own format,
so there is no consistent place for a code, a commandment, or a summary.

So `check` is mostly an aggregator with a contract, and the migration is per-checker: each one
gains a machine-readable line (proposal: one `GS<code>\t<path>[:line]\t<summary>` per finding on
stdout, prose unchanged) and `osdev conform` renders it. A checker not yet migrated still runs and its
raw output is passed through verbatim, labelled as unframed - so adoption is incremental and nothing
is silently dropped while it happens.

### What we must NOT copy from rustc

**The caret.** rustc underlines a column because it has a parser and knows the exact span. Most
checkers here report a file and a line, and a few only a file. A caret under the wrong token is worse
than no caret, because it asserts precision the instrument does not have - the same defect as an
instrument that cannot tell a refusing device from an absent one. So: render a column only where the
checker genuinely produced one, and never synthesise it.

**Colour as the only signal.** Severity must be legible when piped to a file, which is where these
are read after a failed CI run.

### The two verbs must reference each other

Where a finding is auto-fixable, `help` says so by name: `= help: `osdev conform` fixes this`. That is
rustc pointing at `cargo fix`, and it is the whole reason the split in §2 is worth having - `fmt`
handles what is mechanical so `check` only ever reports judgement.

## 5. How the OUTPUT gets proven, which is a different question from whether the rule fires

The plan is to break something on purpose, run `check`, and look at what it prints. That is right, and
it is this project's own standing rule - a guard never observed firing is not evidence. But it tests
two different things and they need separating, because only one of them is already covered.

**Whether the RULE fires** is `commandments_redteam.py`'s job and it already does it: plant a
violation, assert the checker catches it. That script is also why breaking files by hand needs care -
it restores with `git checkout`, and doing the same thing manually is how an hour of uncommitted work
gets eaten.

**Whether the MESSAGE is any good** is not tested by anything, and it is the thing being asked for
here. §22.7 already says so in as many words: *"A gate that fires with an unhelpful message is a
finding, not a pass."* That is a claim about rendered text, so it needs the text as an artifact.

### Golden files, which is how rustc does exactly this

rustc keeps UI tests: a small file that is deliberately wrong, beside a `.stderr` file holding the
*exact* diagnostic it should produce. A change to the renderer shows up as a diff in the expected
output, in the pull request, where a human reads it and says whether it got better or worse.

Proposed the same shape:

```
tests/conformance/ui/
  comment-names-nothing.rs          <- one deliberate violation, nothing else wrong
  comment-names-nothing.expected    <- the exact rendered diagnostic
  dangling-backlog-citation.md
  dangling-backlog-citation.expected
  ...
```

`osdev conform --selftest` renders each fixture and diffs against its `.expected`. Three things fall
out of that which ad-hoc breaking does not give:

- **The output format becomes reviewable.** The `.expected` files ARE the spec for §4, concretely,
  and a regression in wording is a diff rather than a thing someone notices later.
- **No file in the real tree is ever broken**, so there is nothing to revert and no `git checkout` in
  the loop. A fixture lives in `tests/` and is broken permanently and on purpose.
- **One violation per fixture**, so the render is unambiguous. Two violations in one file test the
  aggregator, which is a separate fixture and a deliberate one.

### The fixtures must be REALISTIC, and there is a supply of them

A synthetic break renders differently from a real one. A placeholder nobody would ever type is
obviously wrong and produces an obviously fine message; the hard cases are the plausible ones, where
the message has to carry the reason. The 2026-09-26 sweep is a catalogue of them and each makes a good
fixture:

| fixture | the real case it comes from |
|---|---|
| a comment naming a deleted kernel table | `examples/e1000` told a reader to add an arm to `service_hw` |
| a doc asserting a capability is granted by a contract | four documents said so; 13.6 exists because a model acted on it |
| a comment citing a name that is right to cite | a REMOVAL record - must NOT fire, and a fixture proving a rule stays quiet is as valuable as one proving it fires |
| a dangling backlog citation | this happened while writing this document |
| a documented invocation that does not run | `dir long /` lists a directory named `long` |

That third row is the one most likely to be forgotten. Half of what the comment sweep found was
comments correctly naming deleted things, and a checker that fired on those would be unusable. A
false-positive fixture is a first-class test.

### A gate cannot tell an EXAMPLE from an ASSERTION, and the fixtures make that worse

Writing THIS document failed three gates, and every failure was the same shape: a document that writes
ABOUT a violation contains one.

- the rustc-style diagnostic in §4 carried a real `path:line`, and `line_ref_check` failed it -
  correctly, since the line had moved;
- the paragraph added to explain that failure QUOTED the citation, and failed the same gate again;
- the fixture table above named a placeholder symbol, and `doc_symbols_check` failed it.

None was a false positive in the gate's own terms. Each really was an unresolvable reference, and each
was paraphrased away rather than suppressed - which is fine for prose and **will not work for
fixtures.** A `.expected` file holds a diagnostic verbatim, including the dead name and the stale line
that provoked it. Every one of them will trip a gate the moment `tests/conformance/ui/` exists.

**BUILT: `scripts/conform_ok.py`**, one marker honoured by the gates that need it.

    <!-- conform-ok: GS0304 - a pasted sample of the tool own output -->

Two gates had already solved this locally and differently - `doc-command-ok` and `foreign-ok` - which
was the precedent and also the problem: solved twice, in two shapes, for two of seventeen.

Four things keep it from becoming a door, and they are the only interesting part of it:

1. **It must NAME the rule.** No blanket "ignore everything here": a block exempted from one rule is
   still checked by the other sixteen.
2. **It must carry a REASON.** A marker with nothing after the dash is REFUSED, and the refusal fails
   the build rather than silently suppressing.
3. **Its scope is narrow and predictable** - the fenced block that immediately follows, or one line if
   the next thing is not a fence. Never a file, a section, or "everything below".
4. **Every honoured suppression is COUNTED and printed**, e.g. `1 site(s) exempted by a conform-ok
   marker`. An escape nobody counts is one nobody notices, which is how one becomes a door.

Each guard was proved by forcing it: a marker with no reason is refused, one naming no rule is refused,
a marker for one code does not suppress another, and the count increments only on a real suppression.

Wired into `line_ref_check` so far, because that is the gate that actually blocked four times. The
other two keep their local markers until there is a case for folding them in - adding an escape to a
gate that has never been blocked by one would be speculative (26.2).

This document now uses it, once, on the sample of `conform`'s own output - which holds the exact
citation that generated it, so the site is unresolvable by construction. That is the fixture problem
arriving early, on the document that predicted it.

### What to do with the manual break anyway

Keep it, for the first render only. Before there are fixtures there is nothing to diff against, so the
first pass is: break one thing, look at the output, decide whether a stranger could act on it, and
only then freeze that output as the first `.expected`. The judgement has to happen once by a human -
after that the golden file defends it.

## 6. What this branch does not do

No release. This adds a front door and declares an existing dependency; it changes no kernel code, no
service, no capability, and no invariant. Nothing in `CLAUDE.md` moves - §17's workflow table gains
two rows, which is documentation of what the tooling already enforces.

## 7. Open questions worth settling before anything is written

1. **Does `conform` run the SLOW checkers?** `scaffold_check.py` builds a fresh ISA and
   `port_scope_check.py` reads a diff. Both are porting tools. Proposal: `osdev conform` runs the fast
   conformance set; `osdev conform --port` adds those.
2. **Does the fixing half touch `.md`?** Proposal: dashes and line endings yes, nothing else - which is
   precisely what `dash_check.py` and `line_ending_check.py` already police, so `fmt` is their
   auto-fix and cannot disagree with them.
3. **Is `conform` allowed to be a subset of what the build runs?** It must not be a SUPERSET that
   passes while a build fails, and it must not be a subset that passes while a build fails either.
   Proposal: one list, in one place, shared - `EXTRA_CHECKS` becomes that list and `conform` runs it.
