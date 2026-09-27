<!-- SPDX-License-Identifier: GPL-2.0-only -->
# Conformance: one front door, and the output held to a standard

**Status:** built, `feat/osdev-conformance`, merged 2026-09-27. No release: nothing shipping changed.
Design note: [`docs/conformance.md`](../../docs/conformance.md). Catalogue:
[`tests/conformance/GALLERY.md`](../../tests/conformance/GALLERY.md).

## What was missing

Not a rule. By 2026-09-14 all ten Commandments had a mechanical check and eighteen checkers ran on
every build. What there was no way to do was **ask**:

- a contributor could not find out whether they were clear without compiling a kernel, because the
  checks ran as a side effect of `osdev build`;
- eighteen subprocesses printed eighteen formats, so there was no single verdict;
- and the rules were enforced without being **discoverable**. You learned them by failing a build -
  which is exactly what CLAUDE.md 22.7 says the repository must not require of a stranger.

## What was built

`osdev conform`, a shim over `scripts/conform.py`.

```
$ osdev conform --check
conform --check: 0 would be fixed, 0 need a decision - 18 checks ran, 18 passed
nothing to do. Every rule this project enforces is satisfied.
```

| invocation | does |
|---|---|
| `osdev conform` | fixes what is DECIDABLE, reports what needs JUDGEMENT |
| `osdev conform --check` | reports both, changes nothing (what CI wants) |
| `osdev conform --explain GS0004` | the long form for one rule, including what is NOT mechanised |
| `osdev conform --list` | every rule, its code, its Commandment, and the code legend |
| `osdev conform --selftest` | prove the rendered OUTPUT is right, against the case corpus |
| `osdev conform --gallery` | regenerate `tests/conformance/GALLERY.md` |

### The one design decision: decidable versus judgement

**Decidable** means one right answer and no reader needed - an em-dash must be a hyphen, a CRLF in a
boot config must be an LF. `conform` fixes these and names every file it touched.

**Judgement** means the fix is not determined by the violation. A comment naming a dead symbol might
want correcting, or might be correctly RECORDING a removal and want baselining - and only a human
knows which. Half of what the 2026-09-26 comment sweep found was the second kind. So a clean run
prints `fixed 3, 0 need a decision` rather than a bare `ok`, because "I changed your files and said
nothing" is what makes people distrust a formatter.

The rule that fell out of it: **`conform` may only fix what a gate would fail you for, and it takes
the scope FROM that gate.** It imports `dash_check` for its file set and `line_ending_check` for its
`.gitattributes` rules; it reads the checker list out of `osdev/src/main.rs`. Nothing is restated, so
`conform` and a build cannot disagree.

### The output contract, modelled on rustc

```
error[GS0004]: a contract's claim of authority matches what is actually granted
   --> examples/counter/contracts/counter.toml
    |
    = commandment: IV - Thou shalt honor service contracts.
    = why: declares `service_control = true`, and NOTHING GRANTS IT. The contract is not read
           at runtime (13.6): authority comes from the supervisor's spawn row and the
           kernel's `service_config`...
    = help: ...
    = note: `py scripts/conform.py --explain GS0004` for the long form
```

A stable code per rule, the Commandment named, `= why` before `= help`, both escape routes stated
including the legitimate one, and a count of how many checks RAN - because a run that silently skipped
twelve and printed a clean verdict is the failure this exists to prevent.

**Codes are readable.** `GS0001`..`GS0010` are the Ten Commandments and the number IS the numeral, so
`GS0004` is IV. Above that: `GS01xx` house conventions, `GS02xx` the kernel boundary and unsafe,
`GS03xx` contracts and authority, `GS04xx` documentation and comments.

**What is NOT copied from rustc: the caret.** rustc underlines a column because it has a parser. These
checkers report a file and usually a line; several report only a file. A caret under the wrong token
asserts precision the instrument does not have.

## The gallery: 24 entries, 23 of 28 codes

`tests/conformance/GALLERY.md` is what a developer actually SEES, one entry per rule, produced by
planting a real violation and capturing the output. Generated from the same `tests/conformance/ui/*.case`
corpus `--selftest` verifies, so the catalogue cannot drift from the tested behaviour: one corpus, two
views.

It exists because 22.7 says **a gate that fires with an unhelpful message is a finding, not a pass** -
a claim about rendered text, holdable only by reading the text.

Three plant modes, each earned by a rule that needed it:

| mode | for |
|---|---|
| `append` | most rules - add a line and the gate fires |
| `create` | rules only a NEW file can trip: an unindexed doc, a statusless backlog entry, a kernel module claiming no MISCIS responsibility |
| `replace` | rules tripped by CHANGING or REMOVING an existing construct - Commandment II's `is_transient()`, VII's grant table, IX's missing reacquire |

Multi-file plants also work (`--- plant: <path> <mode> ---`, repeatable, unwound in reverse in a
`finally`) - but they were **not** what the last three rules needed, which is recorded in
`docs/conformance.md` because it was a wrong guess worth keeping.

**A plant DECLARES any byte the surrounding tooling would normalise**, as `\uXXXX`: an em-dash by
codepoint because the dash gate would otherwise fail the fixture itself, and a carriage return as
`\u000d` because git rewrites line endings on checkout. The case file's own endings are structure and
are normalised. That distinction is not decoration - the CRLF case spent a day planting whatever
endings the checkout happened to give it, so it fired on Windows and would have found nothing in CI.
`.gitattributes` pins `*.case` to `eol=lf` as well, the same argument it already makes for `*.gsh`.

**The five codes without an entry are absent for stated reasons**, and the coverage list is COMPUTED
from the rule set minus what the cases rendered - so an absence nobody explained is reported as a
defect rather than mistaken for coverage. `GS0008` can never fire at all: Commandment VIII has no
mechanical check, and `--list` says so.

## Also built

- **`scripts/conform_ok.py`** - one escape marker, `<!-- conform-ok: GS0404 - reason -->`. A document
  that writes ABOUT a violation contains one, and `docs/conformance.md` failed four gates while being
  written. It must NAME the rule, must carry a REASON (a marker without one is refused and fails the
  build), scopes to one fence or line, and every honoured suppression is COUNTED and printed.
- **`scripts/python_floor_check.py`** - the declared Python floor (3.8) held mechanically, because a
  hand-measured number is right on the day it is taken. A contributor on the floor would otherwise
  meet the drift as a SyntaxError from a CHECKER.
- **Python declared** in `README.md` and `docs/porting.md`, and pinned in `build.yml`. It was always
  required - `osdev build` refuses without it, and the three per-board image scripts are Python - and
  never stated.

## Evidence

`conform --check`: 18 checks ran, 18 passed. `--selftest`: 24 of 24 cases render as expected - and
that now holds on a CRLF checkout as well as an LF one, which it did not on the day it was first
claimed. All 18 checkers pass individually. `cargo check -p osdev` clean. No shipping binary
changed.
