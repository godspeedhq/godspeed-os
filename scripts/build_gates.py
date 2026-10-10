#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""The gates every build runs, for the board builds: the SAME list `osdev build` runs.

WHY THIS EXISTS. `osdev build` runs `EXTRA_CHECKS` and then the commandments self-test
and checks. The three board scripts - `arm_build.py`, `pi4_build.py` and `riscv_build.py`, which build
every Pi and VisionFive image - each ran a hand-copied list of SEVEN, and said so as a virtue: "listed
explicitly, so adding a checker is a decision each build path makes". In practice every checker added
since was added to one path, so a Pi image skipped the documentation, backlog, one-way, stdlib-gap,
python-floor and nonfree gates, among others (backlog/80 T8). This project's own rule is that a
checker on one build path is a checker on none, and it is the reason the list is READ, not copied:
`EXTRA_CHECKS` in `osdev/src/main.rs` is the one list, exactly as `conform.py` reads it.

Run it on its own with `py scripts/build_gates.py`; the board scripts call `run_all()`.
"""
from __future__ import annotations

import io
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OSDEV_MAIN = os.path.join(ROOT, "osdev", "src", "main.rs")


def extra_checks():
    """`EXTRA_CHECKS` from osdev, in order. Refuses rather than guesses when it cannot read it."""
    src = io.open(OSDEV_MAIN, encoding="utf-8", errors="replace").read()
    m = re.search(r"const EXTRA_CHECKS[^=]*=\s*&\[(.*?)\n\];", src, re.S)
    body = re.sub(r"//[^\n]*", "", m.group(1)) if m else ""
    checks = re.findall(r'"(scripts/[a-z_0-9]+\.py)"', body)
    if not checks:
        raise SystemExit("build gates: could not read EXTRA_CHECKS from osdev/src/main.rs - refusing to "
                         "guess which checks a build runs. A gate that cannot find its list has not passed.")
    return checks


def run_all():
    """Every gate `osdev build` runs, in the same order: EXTRA_CHECKS, then the commandments
    self-test and the commandments. Stops the build at the first that fails, with its output."""
    gates = [[c] for c in extra_checks()]
    gates += [["scripts/commandments.py", "--selftest"], ["scripts/commandments.py"]]
    for g in gates:
        r = subprocess.run([sys.executable, os.path.join(ROOT, g[0])] + g[1:],
                           cwd=ROOT, capture_output=True, text=True)
        if r.returncode != 0:
            sys.stdout.write(r.stdout)
            sys.stderr.write(r.stderr)
            raise SystemExit(
                "\nBUILD REFUSED: %s failed. Fix the violation, or amend CLAUDE.md and cite\n"
                "the amendment - those are the only two ways past this, by design." % " ".join(g))
    print("build gates: all %d pass - the same list `osdev build` runs (EXTRA_CHECKS + commandments)"
          % len(gates))
    return len(gates)


if __name__ == "__main__":
    run_all()
