#!/usr/bin/env python3
"""Files a BOOTLOADER reads must be LF. Enforce it on the working tree, not just on the repository.

WHY THIS EXISTS. On 2026-09-13 the VisionFive 2 Lite would not boot GodspeedOS. U-Boot read the card,
parsed `extlinux.conf`, printed a menu whose labels looked perfect, selected the right entry, and then:

    Retrieving file: /godspeed-riscv64-visionfive.img
    Failed to load '/godspeed-riscv64-visionfive.img'

The config was CRLF. U-Boot's extlinux parser takes the trailing `\\r` as part of the FILENAME, so it
tried to open a file whose name ended in a carriage return. The MENU rendered correctly throughout,
because a stray `\\r` in a display string only returns the cursor and is invisible - which is exactly
why this read as a load failure rather than a config fault, and why it survived two card reflashes and
two wrong theories (`backlog/26`).

`.gitattributes` ALREADY HELD THE RULE and it was not enough. The repository stored those files as LF
the whole time; `boot/** text eol=lf` now keeps a Windows CHECKOUT at LF too. But a rule in
`.gitattributes` governs what git does, and nothing else: an editor that "helpfully" converts a file,
a copy through a tool that rewrites line endings, a patch applied with the wrong settings, or simply a
tree checked out before the rule existed all land CRLF in the working tree with git none the wiser
until commit. The bytes that reach the card come from the working tree, so the working tree is what
has to be checked.

THE PATTERN LIST IS DERIVED, NOT RESTATED. It is read from `.gitattributes` - every pattern marked
`eol=lf` - so a new rule there is enforced here the moment it is written and nobody has to remember
this file. That is the same reason `arch_boundary_check.py` derives its arch list from the directory
listing: the previous version of that check restated a list, the manual step was missed, and it
printed an all-clear it could not back.

Exit: 0 if every file under an `eol=lf` rule is free of carriage returns, 1 otherwise.
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ATTRS = os.path.join(ROOT, ".gitattributes")

# Where a pattern like `boot/**` is rooted. Anything outside the repository is not ours to police.
SKIP_DIRS = {".git", "target", "build", "node_modules"}


def rules():
    """Every `eol=lf` and `binary` pattern, IN FILE ORDER, as (pattern, kind, line_no).

    Both kinds, because `.gitattributes` is LAST-MATCH-WINS and reading only half of it gets the
    answer wrong. `boot/** text eol=lf` matches a device tree blob; the `*.dtb binary` line below it
    is what actually governs. A checker that saw only the first rule flagged a 58 KB binary for
    containing carriage returns, which is what binaries do.
    """
    out = []
    if not os.path.exists(ATTRS):
        raise SystemExit("line_ending_check: no .gitattributes at %s - nothing defines the policy this\n"
                         "check enforces, so it cannot report a pass." % ATTRS)
    with open(ATTRS, encoding="utf-8") as fh:
        for n, line in enumerate(fh, 1):
            body = line.split("#", 1)[0].strip()
            if not body:
                continue
            parts = body.split()
            if "eol=lf" in parts[1:]:
                out.append((parts[0], "lf", n))
            elif "binary" in parts[1:]:
                out.append((parts[0], "binary", n))
    if not any(k == "lf" for _, k, _ in out):
        raise SystemExit("line_ending_check: .gitattributes declares no `eol=lf` rule. Either the policy\n"
                         "was removed - in which case this check is dead and should be deleted - or the\n"
                         "file is malformed. Refusing to report a pass against an empty rule set.")
    return out


def matches(rel, pattern):
    """Does `rel` fall under this .gitattributes pattern?

    Only the two shapes actually used are handled - a directory glob (`boot/**`) and a bare extension
    (`*.sh`). An unhandled shape is reported rather than silently matching nothing, because a pattern
    this cannot read is a rule that is not being enforced.
    """
    if pattern.endswith("/**"):
        return rel.startswith(pattern[:-2])
    if pattern.startswith("*."):
        return rel.endswith(pattern[1:])
    if "*" not in pattern:
        return rel == pattern
    return None  # unreadable shape


def main():
    ruleset = rules()
    unreadable = []
    offenders = []
    checked = 0

    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for fn in filenames:
            full = os.path.join(dirpath, fn)
            rel = os.path.relpath(full, ROOT).replace(os.sep, "/")
            # LAST match wins, which is git's rule. Scan them all and keep the final verdict.
            verdict = None
            for pat, kind, line_no in ruleset:
                m = matches(rel, pat)
                if m is None:
                    if (pat, line_no) not in unreadable:
                        unreadable.append((pat, line_no))
                    continue
                if m:
                    verdict = kind
            if verdict != "lf":
                continue
            checked += 1
            with open(full, "rb") as fh:
                data = fh.read()
            crs = data.count(b"\r")
            if crs:
                offenders.append((rel, crs))

    if unreadable:
        print("LINE-ENDING CHECK cannot read these `.gitattributes` patterns, so it is NOT enforcing them:")
        for pat, line_no in unreadable:
            print("  .gitattributes:%d  %s" % (line_no, pat))
        print()
        print("Teach `matches()` the shape, or narrow the rule. A pattern this cannot read is a rule")
        print("nobody is enforcing, which is worse than not having written it.")
        return 1

    if offenders:
        print("CARRIAGE RETURNS in files that must be LF:")
        print()
        for rel, crs in offenders:
            print("  %-52s %d CR byte(s)" % (rel, crs))
        print()
        print("These are read by a BOOTLOADER or a shell, not by Windows. U-Boot's extlinux parser")
        print("takes a trailing CR as part of the FILENAME, so every entry fails to load while the")
        print("menu still renders perfectly - a stray CR in a display string only moves the cursor.")
        print("That cost two card reflashes and two wrong theories on 2026-09-13 (backlog/26).")
        print()
        print("Fix: convert to LF and commit. `.gitattributes` keeps a fresh checkout right; this")
        print("catches the tree you actually have.")
        return 1

    print("line endings: %d file(s) under an `eol=lf` rule, none contains a carriage return "
          "(%d rule(s) read from .gitattributes, last match wins)" % (checked, len(ruleset)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
