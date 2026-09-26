#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""A backticked name in a Rust COMMENT must name something that exists in the CODE.

WHY THIS EXISTS. `doc_symbols_check.py` asks this of every backticked name in 163 documents, and
that gate is why the 2026-09-26 sweep could be finished at all. Nothing asked it of the **27,126
doc-comment lines** in 238 Rust files, and those are not a smaller or safer surface: they are twice
the volume of the documentation, they sit inside the kernel, and they are what a contributor reads
FIRST - before any document, because they are next to the code being changed.

The sweep proved the rot is identical in both places. `docs/service-ownership.md` was corrected for
citing `pci::XHCI_FOUND` and `pci::XHCI_MMIO_BASE` after step D deleted them; the comments citing
the same dead statics were not looked at, because nothing looks at comments. `examples/e1000` told a
reader to add an arm to `service_hw`, a table that has had no arms since step C. A comment is no
more self-correcting than a document, and it is read more often.

THE RESOLUTION RULE IS STRICTER HERE THAN FOR DOCUMENTS, deliberately, and this is the one design
decision worth understanding before changing anything. `doc_symbols_check` counts a name as
resolved if it appears ANYWHERE in the source text, comments included - correct for a document,
where the question is "does this reference dangle". Applied to comments that rule is CIRCULAR: the
comment saying `service_hw` would satisfy itself, and the single highest-value case of the sweep
would pass silently. So here the corpus a name must appear in is CODE, with line comments stripped.

WHAT IS DELIBERATELY NOT A FINDING, because it is the majority and it is correct. CLAUDE.md 26.14
("borrow the mechanism, never the model") makes it right to name the reference implementation and
the silicon:

  - hardware registers        `CNTP_TVAL`, `CTR_EL0`, `MAIR_EL1`, `MAC_ADDR0`
  - firmware / spec calls     `sbi_remote_fence_i`, `GET_BOARD_MAC_ADDRESS`
  - other systems' functions  `stmmac_mdio_access`, `bcmgenet_probe`, `dwmac4_setup`
  - device-tree clock names   `hdmitx0_pixelclk`, `gmac0_rmii_refin`

None of those is defined in this tree and none should be. They live in the baseline PERMANENTLY, and
that is not debt - it is the checker recording that a citation points outward on purpose. What the
baseline cannot do is grow silently: a NEW unresolved name fails, and the author either fixes the
comment or adds the name with a reason.

RATCHET, the same shape the other gates use. Every unresolved name is listed in
`scripts/COMMENT-SYMBOLS.baseline.txt`. The set may SHRINK freely - a name removed from the baseline
can never come back without a deliberate edit - and may not GROW without one. The count is reported
on every run so a drop is visible, and entries that are no longer needed are named so the ratchet
can be tightened rather than quietly carried.

SCOPE. Rust under `kernel/src`, `services`, `sdk/rust/src`, `stdlib/rust/src`, `osdev/src` and
`examples`. Names are matched only in backticks and only when they carry an underscore, because a
single bare word in a comment is prose far more often than it is an identifier - the same rule
`doc_symbols_check` settled on for the same reason.
"""
import collections
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "COMMENT-SYMBOLS.baseline.txt")

# Scanned for comments AND contributing to the code corpus.
SRC_DIRS = ["kernel/src", "services", "sdk/rust/src", "stdlib/rust/src", "osdev/src", "examples"]

# Contribute to the CODE corpus only. A comment may legitimately name the checker that enforces it
# (`util_help_coverage_problems`), and a driver comment may name a declared CONTRACT field
# (`hw_mmio`, `hw_interrupt`) - which is a real thing that exists, just not a Rust item.
EXTRA_CODE = ["scripts", "contracts"]

TOKEN = re.compile(r"`([a-z][a-z0-9]*(?:_[a-z0-9]+)+|[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)`")

# A PATH-QUALIFIED citation, checked on its FINAL segment: `control::process_pending`,
# `dwc2::hotplug_poll`. Added after the first sweep, because `TOKEN` requires the whole backtick
# content to be one identifier and so could not see either - and `kernel/CLAUDE.md` says in as many
# words that `control::process_pending` "does not exist". A path inside backticks is an identifier by
# construction and never prose, so this is the cheap half of the widening: it found 5 names in 8
# sites, with no false alarms to baseline.
PATH_TOKEN = re.compile(r"`(?:[A-Za-z_][A-Za-z0-9_]*::)+([A-Za-z_][A-Za-z0-9_]*)`")

# STILL A BLIND SPOT, measured and recorded rather than half-enabled (§26.2, §26.7): a CamelCase
# name in backticks - a type or an enum variant - matches neither pattern, because `TOKEN` requires
# an underscore. `UsbExclusive` was dead, cited in a live comment, and invisible for exactly this
# reason. Measured cost of turning it on: 15 names over 23 sites, and roughly half are NOT ours and
# never will be - `AttrIndx`, `DminLine`, `IminLine` are ARM register FIELDS, `HubAddr`, `PrtAddr`,
# `SplEna` are DWC2 ones, `GenuineIntel` is a CPUID vendor string. The other half look like real
# rot (`SetClock`, `CreateEndpoint`, `UnknownSyscall`, `ReclaimBuffer`). So enabling it is a
# triage pass, not a regex change, and doing it in the same commit would mean baselining findings to
# keep the gate green - which is the one thing a ratchet must not be used for. `backlog/58`.
LINE_COMMENT = re.compile(r"^\s*(///|//!|//)\s?(.*)$")


def read(p):
    try:
        return io.open(p, encoding="utf-8", errors="replace").read()
    except OSError:
        return ""


def rust_files():
    for d in SRC_DIRS:
        base = os.path.join(ROOT, d)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, names in os.walk(base):
            dirnames[:] = [x for x in dirnames if x != "target"]
            for n in sorted(names):
                if n.endswith(".rs"):
                    yield os.path.join(dirpath, n)


def python_code_only(text):
    """Strip `#` comments and triple-quoted blocks from Python.

    NOT cosmetic, and the reason is worth keeping: the first version of this gate put `scripts/`
    into the corpus whole, and the scripts in this tree are heavily prose-documented - including
    THIS one, whose docstring names `XHCI_FOUND`, `CNTP_TVAL` and `stmmac_mdio_access` as worked
    examples. Every name the checker mentioned was therefore resolved BY the checker mentioning it,
    so the gate exempted itself from the problem it describes and its silence read as a pass. That
    is the exact failure `doc_symbols_check` records for its own missing `stdlib/rust/src`.
    """
    out, i, n = [], 0, len(text)
    while i < n:
        for q in ('"""', "'''"):
            if text.startswith(q, i):
                j = text.find(q, i + 3)
                i = n if j < 0 else j + 3
                break
        else:
            if text[i] == "#":
                j = text.find("\n", i)
                i = n if j < 0 else j
            else:
                out.append(text[i])
                i += 1
    return "".join(out)


def split_code_and_comments(text):
    """(code_with_line_comments_stripped, [(lineno, comment_text)]).

    Block comments (`/* */`) are left on the CODE side. Stripping them properly needs a lexer, and
    leaving them in errs in the safe direction: it can only make a name look resolved, never make a
    live name look dead - so it costs recall, never precision. There is no false ALARM in it.
    """
    code, comments = [], []
    for i, line in enumerate(text.split("\n"), 1):
        m = LINE_COMMENT.match(line)
        if m:
            comments.append((i, m.group(2)))
            code.append("")
            continue
        idx = line.find("//")
        # Only treat `//` as a comment when it is not inside a string literal. Counting quotes is
        # crude but errs safe: an odd count leaves the whole line as CODE.
        if idx >= 0 and line[:idx].count('"') % 2 == 0:
            comments.append((i, line[idx:]))
            code.append(line[:idx])
        else:
            code.append(line)
    return "\n".join(code), comments


def scan():
    """({name: [(rel, lineno)]}, files_scanned)."""
    code_corpus, per_file = [], []
    for p in rust_files():
        code, comments = split_code_and_comments(read(p))
        code_corpus.append(code)
        per_file.append((p, comments))
    nfiles = len(per_file)

    for d in EXTRA_CODE:
        base = os.path.join(ROOT, d)
        if not os.path.isdir(base):
            continue
        for dirpath, dirnames, names in os.walk(base):
            dirnames[:] = [x for x in dirnames if x != "target"]
            for n in names:
                p = os.path.join(dirpath, n)
                if n.endswith(".py"):
                    code_corpus.append(python_code_only(read(p)))
                elif n.endswith((".toml", ".json")):
                    # A contract field or schema key is a real thing a driver comment may name.
                    # `#` comments in a contract are prose and are stripped for the same reason.
                    code_corpus.append("\n".join(
                        ln.split("#", 1)[0] for ln in read(p).split("\n")))
    code_text = "\n".join(code_corpus)

    hits = collections.defaultdict(list)
    for p, comments in per_file:
        rel = os.path.relpath(p, ROOT).replace(os.sep, "/")
        for lineno, ctext in comments:
            for rx in (TOKEN, PATH_TOKEN):
                for m in rx.finditer(ctext):
                    tok = m.group(1)
                    if tok not in code_text:
                        hits[tok].append((rel, lineno))
    return hits, nfiles


def load_baseline():
    if not os.path.exists(BASELINE):
        return None
    out = set()
    for line in io.open(BASELINE, encoding="utf-8"):
        line = line.split("#", 1)[0].strip()
        if line:
            out.add(line)
    return out


def main():
    hits, nfiles = scan()
    base = load_baseline()

    if base is None:
        print("comment symbols: no baseline at %s - run with --seed to write one"
              % os.path.relpath(BASELINE, ROOT))
        return 1

    new = sorted(set(hits) - base)
    if new:
        print("comment symbols: %d name(s) in Rust comments name nothing in the code:" % len(new))
        for tok in new:
            for rel, lineno in hits[tok][:3]:
                print("    `%s`  %s:%d" % (tok, rel, lineno))
        print()
        print("  A comment is read before any document, so a name that no longer exists sends a")
        print("  contributor after code that is not there. Fix the comment, or - if it names")
        print("  something OUTSIDE this tree on purpose (a hardware register, an SBI call, a Linux")
        print("  function per CLAUDE.md 26.14) - add it to %s with a note saying which."
              % os.path.relpath(BASELINE, ROOT))
        return 1

    stale = sorted(base - set(hits))
    if stale:
        print("comment symbols: %d baseline entr(y/ies) no longer needed - the ratchet can tighten:"
              % len(stale))
        for tok in stale:
            print("    %s" % tok)
        print("  (remove them from %s)" % os.path.relpath(BASELINE, ROOT))

    sites = sum(len(v) for v in hits.values())
    print("comment symbols: every backticked name in the comments of %d Rust files either exists in "
          "the code or is one of %d baselined outward references (%d citation site(s))"
          % (nfiles, len(base), sites))
    return 0


if __name__ == "__main__":
    if "--seed" in sys.argv:
        hits, _ = scan()
        body = ["# Names appearing in Rust COMMENTS that are not defined in this tree's CODE.",
                "# See scripts/comment_symbol_check.py for what belongs here and what does not.",
                "#",
                "# MOST OF THIS LIST IS CORRECT AND PERMANENT: hardware registers, SBI/firmware",
                "# calls, device-tree clock names, and functions in Linux/u-boot that a driver",
                "# comment cites on purpose (CLAUDE.md 26.14, 'borrow the mechanism'). Those are not",
                "# debt. What the file prevents is the OTHER kind growing silently: a name this tree",
                "# once defined and has since deleted, still cited by a comment as if it were live.",
                ""]
        body += sorted(hits)
        io.open(BASELINE, "w", encoding="utf-8", newline="\n").write("\n".join(body) + "\n")
        print("seeded %s with %d name(s)" % (os.path.relpath(BASELINE, ROOT), len(hits)))
        sys.exit(0)
    sys.exit(main())
