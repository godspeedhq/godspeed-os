#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Compare a cargo-mutants run of the kernel against the committed survivor baseline (backlog/90 E3).

    python scripts/mutants_check.py mutants.out            check a run's missed.txt against the baseline
    python scripts/mutants_check.py mutants.out --write    rewrite the baseline from the run (deliberate)

WHY. `mutation.yml` ended `|| true`, so a mutation run could never fail: a test deleted, a check
weakened, a new function nobody tests - all green. Requiring zero survivors instead would fail on the
day it is switched on, because the 2026-10-27 run left 65 (backlog/90 E5 is the work list). So the
survivors are a RATCHET, the shape this repository uses for every debt it cannot pay at once: a NEW
survivor fails (something got weaker, or arrived untested), and a baseline survivor that is now
CAUGHT fails too, until it is deleted from the baseline - so the list only shrinks.

Names are compared without their `line:col`, so an edit above a function does not churn the baseline;
two identical mutations in one function count once.

The run takes about 50 minutes on four jobs, so it lives in `mutation.yml`, not in osdev's build gate:
    cargo mutants --package kernel --config kernel/mutants.toml --jobs 4 -- --lib
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "kernel", "mutants-survivors.txt")


def key(line):
    # `kernel/src/ipc/queue.rs:117:9: replace MessageQueue::reset with ()` -> drop the `:117:9`.
    return re.sub(r"^([^:]+):\d+:\d+:", r"\1:", line.strip())


def read_keys(path):
    with open(path, encoding="utf-8") as f:
        return {key(l) for l in f if l.strip() and not l.startswith("#")}


def main(argv):
    if not argv:
        print(__doc__)
        return 2
    missed = os.path.join(argv[0], "missed.txt")
    if not os.path.exists(missed):
        print(f"mutants: {missed} does not exist - no run to check, which is a failure, never a pass")
        return 1
    run = read_keys(missed)
    if "--write" in argv:
        with open(BASELINE, "w", encoding="utf-8", newline="\n") as f:
            f.write("# cargo-mutants survivors of `cargo test -p kernel --lib`, by scripts/mutants_check.py.\n"
                    "# May only shrink: a new survivor fails mutation.yml, and so does an entry that is now caught.\n"
                    "# Each is a mutation of the kernel no host test notices - backlog/90 E5's work list.\n")
            for k in sorted(run):
                f.write(k + "\n")
        print(f"mutants: baseline written, {len(run)} survivor(s)")
        return 0
    base = read_keys(BASELINE)
    new, gone = sorted(run - base), sorted(base - run)
    for k in new:
        print(f"mutants: NEW SURVIVOR {k} - a test no longer notices this, or the code is new and untested")
    for k in gone:
        print(f"mutants: now CAUGHT {k} - delete it from kernel/mutants-survivors.txt so the list stays true")
    print(f"mutants: {len(run)} survivor(s) in the run, {len(base)} in the baseline")
    return 1 if new or gone else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
