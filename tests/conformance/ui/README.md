<!-- SPDX-License-Identifier: GPL-2.0-only -->
# `tests/conformance/ui/` - what a violation LOOKS like

`commandments_redteam.py` proves a rule can FAIL. Nothing proved the MESSAGE was any good, and
`CLAUDE.md` 22.7 says in terms: **"A gate that fires with an unhelpful message is a finding, not a
pass."** That is a claim about rendered text, so it needs the text as an artifact.

This is rustc's UI-test shape. One file per case, each holding a deliberate violation and the EXACT
diagnostic `conform` should print for it:

```
py scripts/conform.py --selftest
```

## Why a case is ONE file with a `.case` extension

Two constraints, and they point the same way.

**The fixtures must not be scanned by the checkers they provoke.** A file containing a dead symbol and
a stale line number, sitting in the tree, is a violation - and would fail the very gates these cases
exist to exercise. Exempting the directory in all seventeen checkers would mean seventeen edits and
seventeen chances to widen an exemption by accident. Naming the file `.case` instead means almost no
checker looks at it, because almost none of them scans that extension.

The exception is `line_ending_check.py`, which scans every tracked text file whatever it is called -
and that is right, not a leak. A `.case` file holding CRLF is a real defect: the fixture would plant
whatever endings the checkout happened to give it rather than the bytes it declares. `.gitattributes`
pins `*.case` to `eol=lf` for the same reason it pins `*.gsh` and `boot/**` - a file whose bytes are
asserted must not depend on who checked the repository out.

**A reviewer must be able to read the whole case at once.** The violation and its expected output in
one file, in a diff, is the thing that makes a wording regression visible in a pull request rather than
discovered later. That is most of the value here.

So `--selftest` PLANTS each case at its target path, runs the one checker it names, renders it, restores
the file, and diffs the render against `expected`.

## The safety rules, which are not optional

`commandments_redteam.py` restores with `git checkout`, and that is how an hour of uncommitted work
gets eaten. This does not do that:

- **`--selftest` refuses to plant into a file you have uncommitted work in.** It compares the case
  TARGETS against `git status` and stops, naming them, if any overlap - and only then: everything else
  in the tree may be dirty, so the harness stays usable while you are working on it.
- **Restore is byte-for-byte from an in-memory copy**, not from git. The plant never touches the index.
- **Every case restores in a `finally`**, so a crash mid-case cannot leave a planted violation behind.

## Bytes a plant needs are DECLARED, never written literally

A plant that needs a character the surrounding tooling normalises away spells it `\uXXXX`, and the
loader decodes it: an em-dash is a codepoint escape because the dash gate would otherwise fail this
very file, and a carriage return is `\u000d` because git rewrites line endings on checkout.

The CRLF case learned this the hard way. Its plant body was plain text, so the carriage returns it
planted were the case FILE's own line endings - CRLF on a Windows checkout, LF everywhere else. The
fixture was measuring the checkout: it fired on one machine and would have found nothing in CI. The
loader now normalises the file's own endings, which are structure, and decodes what the plant declares.

## The case format

```
# target: examples/00-hello/src/main.rs
# mode: append
# checker: scripts/comment_symbol_check.py
# why: ...one line on what this case is testing and why it is realistic
--- plant ---
<the text to append, or the whole file for `mode: write`>
--- expect ---
<the exact rendered diagnostic, or the single word NOTHING>
```

`expect: NOTHING` is a case that must produce **no** finding. Those matter as much as the others: half
of what the 2026-09-26 comment sweep found was comments correctly RECORDING a removal, and a checker
that fired on those would be unusable. A gate is only trustworthy if it is quiet in the right places
too.

## Writing a new case

Make it REALISTIC. A placeholder nobody would type produces an obviously fine message; the hard cases
are the plausible ones, where the message has to carry the reason. The first break attempted while
building this used `paint_view` - which is in the baseline as a legitimate record, so nothing fired.
Correct behaviour, wrong test. **A break has to be chosen as carefully as a fix.**

Generate the `expect` block by running the case and pasting what you get, but only after reading it and
deciding a stranger could act on it. The golden file defends a judgement; it cannot make one.
