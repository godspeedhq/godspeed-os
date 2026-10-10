#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""One escape marker for every checker to share: `conform-ok`.

WHO HONOURS IT TODAY: `line_ref_check.py` (GS0404) and nothing else - `HONOURED` below. A
`conform-ok` naming any other code suppresses NOTHING, and since 2026-10-10 it is REFUSED and fails
the build, reported by `line_ref_check.py`, which reads every marker in the tree (backlog/80 T8).
Before, it was silently ignored. "One marker" is the design; one consumer is the state.

WHY THIS EXISTS. A document that writes ABOUT a violation contains one, and there was no way to say
so. Writing `docs/conformance.md` failed four gates for exactly that reason, each time correctly in
the gate's own terms and each time on text that was not a defect:

  - a rustc-style example diagnostic carrying a real `path:line`         (line_ref_check)
  - the paragraph added to explain that failure, which QUOTED the citation (line_ref_check again)
  - a placeholder symbol name in a table of test fixtures                (doc_symbols_check)
  - a pasted sample of the tool's own output, holding the line that provoked it (line_ref_check)

Each was paraphrased away, which works for prose and will NOT work for the UI fixtures that document
specifies: a `.expected` file holds a diagnostic VERBATIM, including the dead name and the stale line.
Every one of them would trip a gate the moment `tests/conformance/ui/` exists.

Two gates had already solved this locally and differently - `<!-- doc-command-ok: -->` and
`<!-- foreign-ok: -->` - which is both the precedent and the problem: two of seventeen, twice, in two
shapes.

    <!-- conform-ok: GS0404 - a pasted sample of conform's own output -->

WHAT KEEPS IT FROM BECOMING A DOOR, which is the only interesting part of the design:

1. **It must NAME the rule.** There is no blanket "ignore everything here". A marker suppresses
   exactly the codes it lists and nothing else, so a block exempted from one rule is still checked by
   the other sixteen.
2. **It must carry a REASON.** A marker with nothing after the dash is refused - and refusing it is
   itself reported, so an empty marker fails the build rather than quietly suppressing.
3. **Its SCOPE is narrow and predictable.** It covers the fenced code block that immediately follows
   it, or - if the next non-blank line is not a fence - that one line. It never covers a file, a
   section, or "everything below".
4. **Every honoured suppression is COUNTED and reported.** A checker says how many it allowed. That
   is what makes the escapes visible rather than invisible, and it is the number a ratchet would hold
   if this is ever abused.

The baselines exist for a name that is legitimately unresolvable FOREVER (a hardware register, a Linux
function). This is for a SITE that is legitimately unresolvable because it is quoting something. Two
different problems, which is why this is not just another baseline entry.
"""
import io
import re

MARKER = re.compile(r"<!--\s*conform-ok:\s*([A-Za-z0-9 ,]+?)\s*-\s*(.*?)\s*-->")

# THE CODES SOME CHECKER ACTUALLY HONOURS, and which one. A marker naming any other code suppresses
# nothing, and until 2026-10-10 it was not reported either (backlog/80 T8): a contributor writing
# `conform-ok: GS0402` would believe a site exempt that is still checked - or worse, one that passes for
# another reason and would be read as excused. Such a marker is now REFUSED like one with no reason.
# A checker that starts honouring markers adds its code here.
HONOURED = {"GS0404": "scripts/line_ref_check.py"}
FENCE = re.compile(r"^\s*(```|~~~)")


def scan(path):
    """{lineno: set(codes)} for every line a marker in `path` covers, plus [(lineno, problem)].

    Returns (suppressions, problems). A problem is a marker this module REFUSES - currently one with
    no reason - and a caller must report it rather than treat the marker as honoured.
    """
    try:
        text = io.open(path, encoding="utf-8", errors="replace").read()
    except OSError:
        return {}, []

    lines = text.split("\n")
    supp, problems = {}, []

    i = 0
    while i < len(lines):
        m = MARKER.search(lines[i])
        if not m:
            i += 1
            continue

        codes = {c.strip().upper() for c in m.group(1).replace(",", " ").split() if c.strip()}
        reason = m.group(2).strip()
        here = i + 1                                  # 1-based, the marker's own line

        if not reason:
            problems.append((here, "a `conform-ok` marker with no reason - say WHY, or remove it"))
            i += 1
            continue
        if not codes:
            problems.append((here, "a `conform-ok` marker naming no rule - blanket suppression is "
                                   "not allowed; name the code(s) it covers"))
            i += 1
            continue
        dead = sorted(c for c in codes if c not in HONOURED)
        if dead:
            problems.append((here, "a `conform-ok` marker naming %s, which no checker honours - it "
                                   "suppresses NOTHING. Only %s read(s) this marker; reword the site, "
                                   "or make that checker honour it and add it to HONOURED in "
                                   "scripts/conform_ok.py" % (", ".join(dead), ", ".join(sorted(HONOURED)))))
            i += 1
            continue

        # Scope: the fence that follows, else the next non-blank line. Never more.
        j = i + 1
        while j < len(lines) and not lines[j].strip():
            j += 1
        if j < len(lines) and FENCE.match(lines[j]):
            k = j + 1
            while k < len(lines) and not FENCE.match(lines[k]):
                k += 1
            covered = range(j + 1, min(k + 1, len(lines)) + 1)   # 1-based, inclusive of both fences
        elif j < len(lines):
            covered = range(j + 1, j + 2)
        else:
            covered = range(here, here + 1)

        for ln in covered:
            supp.setdefault(ln, set()).update(codes)
        supp.setdefault(here, set()).update(codes)
        i += 1

    return supp, problems


class Suppressions:
    """Per-file cache, so a checker walking a tree parses each file once.

    `allowed` counts what was honoured. A checker MUST report that count: an escape hatch nobody
    counts is an escape hatch nobody notices, which is how one becomes a door.
    """

    def __init__(self):
        self._cache = {}
        self.allowed = 0
        self.problems = []

    def _for(self, path):
        if path not in self._cache:
            supp, probs = scan(path)
            self._cache[path] = supp
            for ln, why in probs:
                self.problems.append((path, ln, why))
        return self._cache[path]

    def covers(self, path, lineno, code):
        """True if `path:lineno` is exempt from `code`. Counts the suppression when it is."""
        codes = self._for(path).get(int(lineno), set())
        if code.upper() in codes:
            self.allowed += 1
            return True
        return False
