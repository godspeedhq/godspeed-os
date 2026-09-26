#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""A `path.rs:NNN` citation in a LIVE document must point at something recognisable.

WHY THIS EXISTS. A line number is the fastest-rotting citation in the repository: every edit above
it moves it, and until this script there were fifteen checkers and not one of them looked. Audit 7
(2026-09-23) found **7 of 11 live citations wrong**, including `CLAUDE.md` §6.4's - which cites
`kernel/src/task/mod.rs:593` for the claim that `ehci` and `block-driver` run in deliberate IOMMU
passthrough, and which by then pointed at a closing brace. Three other documents cited the same dead
line, because the citation had been copied rather than checked.

WHAT IT CHECKS, and deliberately not more. A citation passes if the cited line, or a line within
`WINDOW` of it, still contains an ANCHOR - one of the distinctive words from the citing sentence.
That is weaker than "the line is exactly right" and much stronger than nothing: it catches the file
shrinking, the line drifting out of range, and the target being deleted, while tolerating the
one-or-two-line drift that ordinary editing produces and that does not mislead anybody.

WHAT IS EXCLUDED, for a reason rather than convenience. `audits/`, `milestones/` and `bugs/` are
append-only DATED evidence: a line number correct on the day an audit ran is a true record of what
was seen, and rewriting it later would destroy the evidence. They are history, not claims about now.
"""
import io
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import conform_ok               # noqa: E402  - the shared `conform-ok` escape marker

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SKIP_DIRS = ('target', '.git', 'build', 'node_modules', 'book', 'audits', 'milestones', 'bugs')
CITE = re.compile(r'((?:services|kernel|sdk|osdev|scripts|examples|tests)/[A-Za-z0-9_./-]+\.(?:rs|py)):([0-9]{1,5})\b')
WINDOW = 10          # lines either side the anchor may have drifted: a citation should land you
MIN_ANCHOR = 6       # a word shorter than this is not distinctive enough to anchor on


def anchors(sentence):
    """Distinctive words near the citation, to look for around the cited line."""
    words = re.findall(r'[A-Za-z_][A-Za-z0-9_]{%d,}' % (MIN_ANCHOR - 1), sentence)
    stop = {'kernel', 'service', 'services', 'because', 'through', 'without', 'against',
            'already', 'however', 'therefore', 'something', 'everything', 'anything'}
    return [w for w in words if w.lower() not in stop]


def main():
    bad = []
    checked = 0
    # A document that SHOWS a diagnostic must contain the stale citation that provoked it. Four
    # sites in `docs/conformance.md` hit exactly that, and the UI fixtures that document specifies
    # will hit it by construction - a `.expected` file holds the citation verbatim. `conform-ok`
    # lets a site say so, naming this rule and giving a reason.
    supp = conform_ok.Suppressions()
    for root, dirs, files in os.walk(ROOT):
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS]
        for name in files:
            if not name.endswith(('.md', '.rs', '.py')):
                continue
            path = os.path.join(root, name)
            rel = os.path.relpath(path, ROOT).replace(os.sep, '/')
            if rel.startswith('scripts/line_ref_check.py'):
                continue
            try:
                text = io.open(path, encoding='utf-8', errors='ignore').read()
            except OSError:
                continue
            for line_no, line in enumerate(text.split('\n'), 1):
                for m in CITE.finditer(line):
                    target, num = m.group(1), int(m.group(2))
                    checked += 1
                    tpath = os.path.join(ROOT, target)
                    if supp.covers(path, line_no, 'GS0404'):
                        continue
                    if not os.path.exists(tpath):
                        bad.append((rel, line_no, target, num, 'the file does not exist'))
                        continue
                    body = io.open(tpath, encoding='utf-8', errors='ignore').read().split('\n')
                    if num > len(body):
                        bad.append((rel, line_no, target, num,
                                    'past end of file (%d lines)' % len(body)))
                        continue
                    if supp.covers(path, line_no, 'GS0404'):
                        continue
                    lo, hi = max(0, num - 1 - WINDOW), min(len(body), num + WINDOW)
                    near = '\n'.join(body[lo:hi]).lower()
                    found = [a for a in anchors(line) if a.lower() in near]
                    if not found:
                        bad.append((rel, line_no, target, num,
                                    'nothing within %d lines matches the citing sentence' % WINDOW))
    for p, ln, why in supp.problems:
        rel_p = os.path.relpath(p, ROOT).replace(os.sep, '/')
        print('line refs: %s:%d - %s' % (rel_p, ln, why))
    if supp.problems:
        return 1
    if bad:
        print('line refs: %d citation(s) no longer point at what they claim' % len(bad))
        print()
        for rel, line_no, target, num, why in bad:
            print('  %s:%d' % (rel, line_no))
            print('      cites %s:%d - %s' % (target, num, why))
        print()
        print('A line number rots on the next edit above it. Either re-point it, or cite the')
        print('FUNCTION or the distinctive comment instead - those survive editing and a reader')
        print('can find them with grep. `audits/`, `milestones/` and `bugs/` are exempt: their')
        print('numbers are dated evidence of what was seen, not claims about the code now.')
        return 1
    note = ''
    if supp.allowed:
        note = ', %d site(s) exempted by a `conform-ok` marker' % supp.allowed
    print('line refs: %d `path:line` citation(s) still point at what they claim '
          '(dated evidence in audits/, milestones/, bugs/ exempt%s)' % (checked, note))
    return 0


if __name__ == '__main__':
    sys.exit(main())
