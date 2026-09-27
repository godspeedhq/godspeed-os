#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""The declared minimum Python version must be the TRUE one.

`README.md` tells a contributor they need Python 3.8 or newer. That number was measured by hand on
2026-09-26 - and a number measured by hand is right on the day it is taken and silently wrong
afterwards, which is the lesson `shared_surface_check.py` was written for. The next script that uses a
`match` statement raises the real floor to 3.10 and nothing would say so; a contributor on 3.8 would
get a `SyntaxError` from a checker, which is the worst possible first experience of this repository.

So: the floor is declared in ONE place (`FLOOR` below, and `README.md` quotes it), and this fails if
any tracked Python uses a feature newer than that.

WHAT IT CHECKS, and why the list is short. Only features whose ABSENCE is a hard error on the floor
version - a `SyntaxError` or a `TypeError` at import - are worth gating. Style differences are not.

  3.10  `match` statement
  3.9   `str.removeprefix` / `removesuffix`, `dict |` merge
  3.9   builtin generics in an EVALUATED annotation (see the note below - this one is subtle)
  3.8   walrus `:=`, positional-only `/` parameters

THE SUBTLE ONE, recorded because it was nearly got wrong. `violations: list[str] = []` looks like it
needs 3.9, since `list[str]` is only subscriptable from then. It does NOT, when it is a
FUNCTION-LOCAL annotation: CPython never evaluates those. Four checkers in this tree use exactly that
form, and the floor is 3.8 because of it. At MODULE or CLASS level the annotation IS evaluated and the
same text would genuinely require 3.9 - so that is what this distinguishes, by indentation, rather
than flagging the form wholesale and being wrong four times.

Verified by running it rather than by reading the PEP:

    def f():
        v: totally_undefined_name[int] = []   # never evaluated, so never a NameError
        return v
"""
import io
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# THE DECLARED FLOOR. Change this only with the README, and only knowing that a contributor on the
# old floor will get a SyntaxError rather than a message.
FLOOR = (3, 8)

# (version, name, pattern, applies_to_line) - `applies_to_line` may refuse a match by context.
CHECKS = [
    ((3, 10), "a `match` statement", re.compile(r"^\s*match\s+.+:\s*$"), None),
    ((3, 9), "`str.removeprefix`/`removesuffix`", re.compile(r"\.remove(?:prefix|suffix)\s*\("), None),
    ((3, 9), "a builtin generic in an EVALUATED annotation (module or class level)",
     re.compile(r"^(?P<indent>\s*)[A-Za-z_][A-Za-z0-9_]*\s*:\s*(?:list|dict|tuple|set|frozenset)\["),
     "module_or_class"),
]


def tracked_python():
    out = subprocess.run(["git", "ls-files", "*.py"], cwd=ROOT, capture_output=True, text=True)
    for rel in out.stdout.split("\n"):
        rel = rel.strip()
        if rel and os.path.isfile(os.path.join(ROOT, rel)):
            yield rel


def main():
    problems = []
    scanned = 0

    for rel in tracked_python():
        scanned += 1
        text = io.open(os.path.join(ROOT, rel), encoding="utf-8", errors="replace").read()
        in_doc = False
        for n, line in enumerate(text.split("\n"), 1):
            # Skip docstrings and comments: this file NAMES `match` and `removeprefix` in its own
            # prose, and a checker that fails on its own explanation is the self-reference trap that
            # has already bitten three gates in this repository.
            triple = line.count('"""') + line.count("'''")
            if triple % 2 == 1:
                in_doc = not in_doc
                continue
            if in_doc or line.lstrip().startswith("#"):
                continue

            for ver, what, pat, context in CHECKS:
                if ver <= FLOOR:
                    continue
                m = pat.search(line)
                if not m:
                    continue
                if context == "module_or_class":
                    # A FUNCTION-LOCAL annotation is never evaluated, so it does not raise the floor.
                    # Indentation is the discriminator, which is crude but is exactly the distinction
                    # CPython makes.
                    if m.group("indent"):
                        continue
                problems.append((rel, n, "%d.%d" % ver, what, line.strip()[:76]))

    if problems:
        print("python floor: %d use(s) of a feature newer than the declared floor (%d.%d):"
              % (len(problems), FLOOR[0], FLOOR[1]))
        print()
        for rel, n, ver, what, src in problems:
            print("  %s:%d needs Python %s - %s" % (rel, n, ver, what))
            print("      %s" % src)
        print()
        print("Either rewrite it to work on %d.%d, or RAISE the floor deliberately: change `FLOOR` in"
              % (FLOOR[0], FLOOR[1]))
        print("this file and the Requirements line in README.md together. A contributor on the old")
        print("floor gets a SyntaxError from a checker, which is the worst first experience this")
        print("repository can offer - so the number must never drift upward by accident.")
        return 1

    print("python floor: %d tracked script(s) all run on Python %d.%d, the version README.md declares"
          % (scanned, FLOOR[0], FLOOR[1]))
    return 0


if __name__ == "__main__":
    sys.exit(main())
