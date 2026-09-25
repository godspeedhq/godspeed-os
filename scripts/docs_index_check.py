#!/usr/bin/env python3
"""Every file in `docs/` is reachable from the docs index, which CLAUDE.md 5 says is `docs/CLAUDE.md`.

`doc_refs.py` already checks one direction - a citation names a file that exists. This checks the
OTHER direction, and it is the one that actually failed: `gsfs-next.md`, `ipc-efficiency.md`,
`tcp-design.md` and `x-residue.md` sat in `docs/` unlisted while the index called itself the index,
and were cited from two to five other places each. `x-residue.md` is where `scripts/commandments.py`
says the un-mechanised half of Commandment X is written down, so the enforcement layer was pointing
at a document the index did not admit existed.

This is `backlog_check.py` applied to a second folder that makes the same promise. That check exists
because backlog entries 33 to 38 "existed as files and appeared in no index, so the folder's own
README promised 'this is the index' while six items were invisible to anyone reading it. A record
nobody can find is not a record." The lesson was learned once and enforced in one place; `docs/`
drifted the same way within weeks. Found by Audit 7 (2026-09-25).

A file may be listed anywhere in the index - the table, or prose naming it - because the point is
that a reader can FIND it, not that it occupies a particular row.
"""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DOCS = os.path.join(ROOT, 'docs')
INDEX = os.path.join(DOCS, 'CLAUDE.md')


def main():
    if not os.path.isdir(DOCS):
        print('docs index: no docs/ directory')
        return 0
    if not os.path.isfile(INDEX):
        print('DOCS INDEX CHECK FAILED: docs/CLAUDE.md does not exist, but CLAUDE.md 5 names it '
              'as the index')
        return 1

    with open(INDEX, encoding='utf-8') as fh:
        index = fh.read()

    entries = sorted(f for f in os.listdir(DOCS)
                     if f.endswith('.md') and f != 'CLAUDE.md')
    missing = [f for f in entries if f not in index]

    if missing:
        print('DOCS INDEX CHECK FAILED: %d file(s) in docs/ are named nowhere in docs/CLAUDE.md\n'
              % len(missing))
        for f in missing:
            print('  docs/%s' % f)
        print('\nCLAUDE.md 5 designates `docs/CLAUDE.md` as the index for this directory. A document')
        print('that the index does not name is one a reader cannot find by reading the index, which')
        print('is the whole job it claims to do. Add a row to its Files table (or name it in the')
        print('prose, which also counts) - or delete the file if it is genuinely dead.')
        return 1

    print('docs index: all %d file(s) under docs/ are reachable from docs/CLAUDE.md' % len(entries))
    return 0


if __name__ == '__main__':
    sys.exit(main())
