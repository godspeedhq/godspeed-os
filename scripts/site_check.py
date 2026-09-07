#!/usr/bin/env python3
"""Hold the four HAND-WRITTEN site pages to the repository they describe.

WHY THIS EXISTS. 70 of the site's 74 pages are `{{#include}}` views of real files, so they cannot
drift - that is the whole design, and it works. Four pages have no source to be a view of:

    introduction.md   the front door
    gallery.md        captures of the running system
    services.md       the service catalogue, with diagrams
    utilities.md      the index of every utility

Those four are prose someone wrote, and prose someone wrote is prose that goes stale. The question
asked was whether they could be GENERATED instead. Mostly they should not be: the spec files carry
inconsistent headers (`# Utility Spec: observe` in one, `# trace - what the kernel is doing` in the
next), so a generated table would have to invent its own descriptions, and generated prose would be
worse writing than what is there.

What CAN be mechanised is the part that actually rots: whether the pages still describe the system
that exists. A missing row for a new utility, a service that gained a peer, a page that quietly
became hand-written. So this checks facts and completeness, and leaves the writing alone.

Exit 0 when the four pages match the repository, 1 otherwise.
"""

import glob
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SITE = os.path.join(ROOT, "website", "src")


def read(path):
    try:
        return io.open(path, encoding="utf-8", errors="ignore").read()
    except OSError:
        return ""


def check_standalone_inventory():
    """A NEW hand-written page is a decision, not an accident - so it has to be admitted here.

    Without this, someone adds a page, writes it by hand because that is easiest, and the site
    quietly grows a second copy of something. The four below were each a deliberate choice.
    """
    expected = {"introduction.md", "gallery.md", "services.md", "utilities.md"}
    found = set()
    for p in glob.glob(os.path.join(SITE, "**", "*.md"), recursive=True):
        rel = os.path.relpath(p, SITE).replace(os.sep, "/")
        if rel == "SUMMARY.md":
            continue
        if "{{#include" not in read(p):
            found.add(rel)
    out = []
    for extra in sorted(found - expected):
        out.append("new HAND-WRITTEN page '%s' - make it an `{{#include}}` of a real file, or add it "
                   "to the expected set in this script with a reason" % extra)
    for gone in sorted(expected - found):
        out.append("'%s' is no longer hand-written (good) - drop it from the expected set here" % gone)
    return out


def check_intro_counts():
    """The introduction states how many pages are views and how many are hand-written.

    It states them because a reader deserves to know which pages can drift - but a stated count is
    itself a thing that drifts, so it is checked here rather than trusted. Written as words
    ("Seventy of its seventy-four"), which reads better and is no harder to verify.
    """
    WORDS = {"sixty": 60, "seventy": 70, "seventy-four": 74, "seventy-three": 73,
             "seventy-five": 75, "sixty-nine": 69, "seventy-one": 71, "three": 3,
             "four": 4, "five": 5}
    page = read(os.path.join(SITE, "introduction.md"))
    total = inc = 0
    for p in glob.glob(os.path.join(SITE, "**", "*.md"), recursive=True):
        if os.path.basename(p) == "SUMMARY.md":
            continue
        total += 1
        if "{{#include" in read(p):
            inc += 1

    # READ THE SENTENCE, do not substring-search for the words. The first version of this check
    # asked `"seventy" in page`, which is TRUE even when the page says "Sixty of its seventy-four" -
    # because "seventy-four" contains "seventy". It passed on a deliberately broken claim, which is
    # the one failure mode a check must not have.
    m = re.search(r"([A-Za-z-]+) of its ([A-Za-z-]+) pages", page, re.I)
    if not m:
        return ["introduction.md no longer states the page split ('N of its M pages') - either "
                "restore it or drop this check; it must not silently pass"]
    said_inc = WORDS.get(m.group(1).lower())
    said_total = WORDS.get(m.group(2).lower())
    out = []
    if said_inc is None or said_total is None:
        out.append("introduction.md says '%s of its %s pages' and this check cannot read one of "
                   "those numerals - add it to WORDS here" % (m.group(1), m.group(2)))
        return out
    if said_inc != inc:
        out.append("introduction.md says %d pages are views; %d actually are" % (said_inc, inc))
    if said_total != total:
        out.append("introduction.md says there are %d pages; there are %d" % (said_total, total))
    hand = total - inc
    if not re.search(r"\bfour exceptions\b", page, re.I) or hand != 4:
        out.append("introduction.md calls out 'four exceptions'; %d pages are hand-written" % hand)
    return out


def check_utilities_index():
    """Every utility spec has a row, and every row has a spec."""
    page = read(os.path.join(SITE, "utilities.md"))
    if not page:
        return ["website/src/utilities.md is missing"]
    linked = set(re.findall(r"\]\(utilities/([a-z0-9_-]+)\.md\)", page))
    specs = set()
    for p in glob.glob(os.path.join(ROOT, "utilities", "[0-9]*.md")):
        specs.add(re.sub(r"^[0-9]+_", "", os.path.basename(p)[:-3]))
    out = []
    for missing in sorted(specs - linked):
        out.append("utilities.md has NO ROW for `%s` (utilities/*_%s.md exists)" % (missing, missing))
    for extra in sorted(linked - specs):
        out.append("utilities.md links `%s`, which has no spec under utilities/" % extra)
    return out


def check_services_peers():
    """`**Peers:**` on the services page must match the contract's `ipc_send`.

    Checked in one direction only: every name the PAGE claims must really be a declared peer. The
    reverse would fire on prose that legitimately summarises (`net-stack` -> `nic-driver`), and a
    check with false alarms gets switched off.
    """
    page = read(os.path.join(SITE, "services.md"))
    out = []
    section = []
    for line in page.split("\n"):
        if line.startswith("### "):
            # A heading may cover SEVERAL services - `### `nic-driver` and `net-stack`` is one
            # section about the network, and its Peers line describes the pair. Reading only the
            # first name made this script's own first run produce three false alarms, all of them
            # correct prose. Take every name in the heading and accept a peer declared by any of
            # them; a check with false alarms gets switched off, which is worse than no check.
            section = re.findall(r"`([a-z0-9-]+)`", line)
            continue
        m = re.match(r"^\*\*Peers:\*\* (.+)$", line)
        if not m or not section:
            continue
        claimed = set(re.findall(r"`([a-z0-9-]+)`", m.group(1)))
        declared = set()
        seen_contract = False
        for svc in section:
            contract = read(os.path.join(ROOT, "services", svc, "contracts", svc + ".toml"))
            if not contract:
                continue
            seen_contract = True
            cm = re.search(r"ipc_send\s*=\s*\[([^\]]*)\]", contract)
            if cm:
                declared |= set(re.findall(r'"([a-z0-9-]+)"', cm.group(1)))
            declared.add(svc)   # a combined section naming its own subjects is not a peer claim
        if not seen_contract:
            continue
        # `supervisor` reaches every service by spawning it, and a driver's own hardware peer may be
        # named in prose; neither is an `ipc_send` entry, and neither is wrong.
        for name in sorted(claimed - declared - {"supervisor"}):
            if os.path.isdir(os.path.join(ROOT, "services", name)):
                out.append("services.md says `%s` has peer `%s`, but no contract in that section "
                           "declares it" % ("/".join(section), name))
    return out


def check_services_covered():
    """A service the supervisor manages should be on the page describing the services."""
    page = read(os.path.join(SITE, "services.md"))
    sched = read(os.path.join(ROOT, "kernel", "src", "task", "scheduler.rs"))
    m = re.search(r'if matches!\(task_name,(.{0,600}?)\)\s*\{', sched, re.S)
    managed = set(re.findall(r'"([a-z0-9-]+)"', m.group(1))) if m else set()
    managed -= {"counter", "supervisor"}          # a test service, and the page's own subject
    return ["services.md does not mention `%s`, which the kernel manages (scheduler.rs restart set)"
            % n for n in sorted(managed) if "`%s`" % n not in page]


def main():
    problems = []
    for fn in (check_standalone_inventory, check_intro_counts, check_utilities_index,
               check_services_peers, check_services_covered):
        problems += fn()

    if not problems:
        print("site: the 4 hand-written pages match the repository "
              "(utility index complete, service peers and coverage agree)")
        return 0

    print("site: %d hand-written page(s) no longer match the repository\n" % len(problems))
    for p in problems:
        print("  %s" % p)
    print("\nThese four pages have no source to be a view of, so nothing else catches them.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
