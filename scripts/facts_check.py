#!/usr/bin/env python3
"""Fail when a doc states a number the CODE owns, and the two disagree.

WHY THIS EXISTS. `doc_refs.py` catches a doc pointing at a file that does not exist. This catches the
other half: a doc stating a FACT that used to be true. Both are the same disease - a second copy of
something whose truth lives elsewhere - and the second copy is the one that goes quietly stale,
because nothing breaks when it does.

Found by hand in one sitting, which is the argument for mechanising it:
  - CLAUDE.md's own test-category diagram said "Tests 1-11" while its prose said "Tests 1-15".
  - An osdev comment said "all 22 services" when there were 19 crates.
  - Three doc STATUS lines named branches that had been deleted.

THE RULE THIS ENFORCES. A number that comes from code is owned by the code. A doc may restate it -
prose is allowed to be readable - but the restatement is CHECKED. Where a doc does not need the
number at all, the better fix is to delete it rather than pin it, and that is not this script's
call to make.

WHAT IS DELIBERATELY NOT CHECKED. Dated evidence. `docs/ahci.md` recording "identity 23/23" is a true
statement about a run in the past, and rewriting it would falsify the record - the same exemption
`doc_refs.py` grants `audits/`. History states what WAS; only present-tense claims are checked.

Exit 0 when every checked fact agrees with its source, 1 otherwise.
"""

import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel):
    try:
        return io.open(os.path.join(ROOT, rel), encoding="utf-8", errors="ignore").read()
    except OSError:
        return ""


def const(rel, name):
    """The value of a `const NAME: ty = N;` in one file - the authoritative number."""
    m = re.search(r"const\s+%s\s*:\s*\w+\s*=\s*([0-9_]+)" % re.escape(name), read(rel))
    return int(m.group(1).replace("_", "")) if m else None


def enum_value(rel, name):
    """The value of `Name = N,` in a Rust enum."""
    m = re.search(r"^\s*%s\s*=\s*([0-9]+)\s*," % re.escape(name), read(rel), re.M)
    return int(m.group(1)) if m else None


def count_files(pattern_dir, pattern):
    import glob
    return len(glob.glob(os.path.join(ROOT, pattern_dir, pattern)))


# Each fact: a human name, the authoritative value, and the patterns that ASSERT it in prose.
# A pattern must capture the number, so a wrong number is what fails - not a missing mention.
def facts():
    out = []

    qd = const("kernel/src/ipc/queue.rs", "QUEUE_DEPTH")
    if qd:
        out.append(("IPC queue depth", qd, "kernel/src/ipc/queue.rs QUEUE_DEPTH",
                    [r"([0-9]+)-deep queue", r"queue depth (?:of|is|=)\s+([0-9]+)",
                     r"([0-9]+) messages per endpoint", r"queue depth, 0-([0-9]+)"]))

    mm = const("kernel/src/ipc/message.rs", "MAX_MESSAGE_SIZE")
    mp = const("sdk/rust/src/ipc.rs", "MAX_PAYLOAD")
    if mm and mp and mm != mp:
        out.append(("SDK MAX_PAYLOAD vs kernel MAX_MESSAGE_SIZE", mm,
                    "these two MUST be equal - a smaller SDK buffer is a stack smash", []))

    ring = const("services/events/src/main.rs", "RING")
    if ring:
        out.append(("events trace ring", ring, "services/events/src/main.rs RING",
                    [r"([0-9]+)-event (?:trace )?ring", r"ring of ([0-9]+) events"]))

    logb = const("services/events/src/main.rs", "LOG_BYTES")
    if logb:
        out.append(("events log window (KiB)", logb // 1024, "services/events/src/main.rs LOG_BYTES",
                    [r"([0-9]+) KiB log window"]))

    pieces = const("services/recorder/src/main.rs", "PIECES")
    if pieces:
        out.append(("recorder rotation pieces", pieces, "services/recorder/src/main.rs PIECES",
                    [r"`?PIECES`? files rotate \(([0-9]+) today", r"rotates? between ([0-9]+) pieces"]))

    for nm in ("Log", "Call", "CallDeadline", "AcquireSendCap", "InspectKernel", "Kill"):
        v = enum_value("kernel/src/syscall/dispatch.rs", nm)
        if v:
            out.append(("syscall %s" % nm, v, "kernel/src/syscall/dispatch.rs enum",
                        [r"syscall\s+([0-9]+)\s*[-(]?\s*%s" % nm, r"%s\s*\(syscall\s+([0-9]+)\)" % nm]))

    n_util = count_files("utilities", "[0-9]*.md")
    out.append(("utility specs", n_util, "utilities/*.md on disk",
                [r"([0-9]+) utility specs", r"([0-9]+) utilities\b"]))

    return [f for f in out if f[3]]


# Present-tense docs only. `audits/` and `milestones/` record what WAS true and are exempt, as are
# CLAUDE.md's dated amendment blocks (handled by the historical-line filter below).
DOCS = ["CLAUDE.md", "README.md", "CONTRIBUTING.md", "GETTING_STARTED.md"]
DOC_GLOBS = ["docs/*.md", "utilities/*.md", "services/*/CLAUDE.md", "kernel/src/**/CLAUDE.md",
             "sdk/rust/CLAUDE.md", "osdev/CLAUDE.md", "tests/**/CLAUDE.md", "backlog/*.md"]

# A SECTION REFERENCE IS NOT A VALUE. On this script's first run `queue depth (§8.5)` captured "8"
# and a `0-16` range captured "0" - two false alarms out of two findings. A checker that cries wolf
# gets disabled, which is worse than never having written it, so references are stripped before any
# pattern sees the line rather than being fought inside each pattern.
SECTION_REF = re.compile(r"\(?§\s*[0-9]+(\.[0-9]+)*\)?|\bsection\s+[0-9]+(\.[0-9]+)*", re.I)

# A line that reports a past run or names a date is evidence, not a claim about now.
HISTORICAL = re.compile(r"20[0-9]{2}-[0-9]{2}-[0-9]{2}|Verified:|verified on|Amendment|was\b.*\bnow\b")


def main():
    import glob
    docs = [d for d in DOCS]
    for g in DOC_GLOBS:
        docs += [os.path.relpath(p, ROOT).replace(os.sep, "/")
                 for p in glob.glob(os.path.join(ROOT, g), recursive=True)]

    checked = 0
    bad = []
    for name, truth, source, pats in facts():
        if not pats:
            bad.append((name, source, "", "", ""))
            continue
        for rel in sorted(set(docs)):
            text = read(rel)
            if not text:
                continue
            for line_no, line in enumerate(text.split("\n"), 1):
                if HISTORICAL.search(line):
                    continue
                line = SECTION_REF.sub(" ", line)
                for pat in pats:
                    for m in re.finditer(pat, line, re.I):
                        checked += 1
                        got = int(m.group(1))
                        if got != truth:
                            bad.append((name, source, rel, line_no, "says %d, code says %d" % (got, truth)))

    if not bad:
        print("facts: %d doc statement(s) agree with the code they restate" % checked)
        return 0

    print("facts: %d doc statement(s) disagree with the code\n" % len(bad))
    for name, source, rel, line_no, what in bad:
        print("  %s" % name)
        print("      source of truth: %s" % source)
        if rel:
            print("      %s:%s  %s" % (rel, line_no, what))
    print("\nThe code owns the number. Update the doc, or - better - delete the number from the\n"
          "doc if the sentence reads fine without it. A fact restated in two places drifts.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
