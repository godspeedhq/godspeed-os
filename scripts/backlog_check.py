#!/usr/bin/env python3
"""Every backlog entry is reachable from the backlog index, and no number is used twice.

`doc_refs.py` already checks one direction - a citation names an entry that exists. This checks
the OTHER direction, which is the one that actually failed: entries 33 through 38 existed as files
and appeared in no index, so the folder's own README promised "this is the index" while six items
were invisible to anyone reading it. A record nobody can find is not a record (backlog/README.md).

Also ENFORCES a status line on every entry. This started as a number to watch rather than a gate,
because 14 of 38 entries predated the rule and failing them all at once would only have got the
check disabled. They were worked through on 2026-09-20 and coverage reached 38 of 38, so the
ratchet closed the same day it could.

What the line must say: `**Status:` within the first 12 lines, carrying CLOSED, OPEN, RESOLVED or
FIXED in capitals. Capitals because two entries said "open" in lowercase, which reads perfectly to a
person and is invisible to every survey - and an unsurveyable backlog is how entries 33 to 38 came
to be linked from nowhere while the index claimed to be the index.
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BACKLOG = os.path.join(ROOT, 'backlog')
README = os.path.join(BACKLOG, 'README.md')

STATUS = re.compile(r'\*\*Status[:*]')
VERDICT = re.compile(r'\b(CLOSED|OPEN|RESOLVED|FIXED)\b')


def main():
    if not os.path.isdir(BACKLOG):
        print('backlog check: no backlog/ directory')
        return 0

    entries = sorted(f for f in os.listdir(BACKLOG)
                     if f.endswith('.md') and f != 'README.md')
    if not entries:
        print('backlog check: no entries')
        return 0

    with open(README, encoding='utf-8') as fh:
        index = fh.read()

    failures = []

    # 1. every entry is linked from the index
    for name in entries:
        if '(%s)' % name not in index:
            failures.append('NOT INDEXED: backlog/%s is not linked from backlog/README.md' % name)

    # 2. no number used twice
    seen = {}
    for name in entries:
        num = name.split('-', 1)[0]
        if not num.isdigit():
            failures.append('BAD NAME: backlog/%s does not begin with a number' % name)
            continue
        key = str(int(num))
        if key in seen:
            failures.append('DUPLICATE NUMBER %s: %s and %s' % (key, seen[key], name))
        seen[key] = name

    # 3. every entry carries a status line a survey can read
    withstatus = 0
    for name in entries:
        with open(os.path.join(BACKLOG, name), encoding='utf-8') as fh:
            head = ''.join(fh.readlines()[:12])
        if STATUS.search(head) and VERDICT.search(head):
            withstatus += 1
        else:
            failures.append(
                'NO STATUS: backlog/%s has no "**Status:" line carrying CLOSED/OPEN/RESOLVED/FIXED '
                '(capitals) in its first 12 lines' % name)

    if failures:
        for f in failures:
            print('backlog check: %s' % f)
        print('\nbacklog check: FAILED (%d problem(s) across %d entries)' % (len(failures), len(entries)))
        return 1

    print('backlog check: %d entries, all indexed, no duplicate numbers, '
          '%d of %d with a readable status line.' % (len(entries), withstatus, len(entries)))
    return 0


if __name__ == '__main__':
    sys.exit(main())
