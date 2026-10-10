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

AND A FUNCTION SIGNATURE (`def f() -> list[str]:`, `def f(x: dict[str, int])`), which this did not
check until 2026-10-10 (`backlog/80` T1). Those annotations ARE evaluated, at `def` time, so on 3.8
they raise `TypeError` at import. `arch_boundary_check.py`, `dash_check.py` and `unsafe_check.py` each
carried one while this reported the floor true. A module with `from __future__ import annotations`
evaluates none of its annotations (PEP 563), so in such a module neither annotation check applies -
which is how those three were fixed, and why the import is looked for rather than assumed.

Reasoned from PEP 585 and PEP 563 rather than run: no 3.8 interpreter is on the machine this was
written on. The module-level case was run, as the note above says.
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
    ((3, 9), "a builtin generic in a function signature (evaluated at `def` time)",
     re.compile(r"^\s*(?:async\s+)?def\s+\w+\s*\(.*(?:\blist|\bdict|\btuple|\bset|\bfrozenset|\btype)\["),
     "signature"),
]

# STANDARD-LIBRARY MODULES newer than the floor. `import tomllib` is a ModuleNotFoundError on 3.10,
# and `scripts/commandments.py` - the first checker of every build - did exactly that while this
# file reported the floor true, because it looked at syntax and never at imports (backlog/80 T1).
# An import is fine inside `try:` with an `except ImportError` fallback, which is how
# `scripts/toml_compat.py` does it; the guard is "the line before it is `try:`".
NEW_MODULES = {"tomllib": (3, 11), "zoneinfo": (3, 9), "graphlib": (3, 9)}
NEW_IMPORT = re.compile(r"^\s*(?:import|from)\s+(%s)\b" % "|".join(NEW_MODULES))

# PEP 563: with this import no annotation in the module is evaluated, so the two annotation checks
# above do not apply to it.
FUTURE_ANNOTATIONS = re.compile(r"^from __future__ import (?:[\w, ]*\b)?annotations\b", re.M)


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
        postponed = bool(FUTURE_ANNOTATIONS.search(text))
        in_doc = False
        prev = ""
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

            m = NEW_IMPORT.match(line)
            if m and NEW_MODULES[m.group(1)] > FLOOR and prev.split("#", 1)[0].strip() != "try:":
                ver = NEW_MODULES[m.group(1)]
                problems.append((rel, n, "%d.%d" % ver,
                                 "`%s` is not in the standard library before %d.%d (guard it with "
                                 "`try:` / `except ImportError`)" % (m.group(1), ver[0], ver[1]),
                                 line.strip()[:76]))
            if line.strip():
                prev = line

            for ver, what, pat, context in CHECKS:
                if ver <= FLOOR:
                    continue
                m = pat.search(line)
                if not m:
                    continue
                if context in ("module_or_class", "signature") and postponed:
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
