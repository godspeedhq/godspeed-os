#!/usr/bin/env python3
"""Fail on documentation that points at a file which does not exist.

WHY THIS EXISTS. Three stale claims were found by hand in one sitting: the `logger`-era statement that
logs flowed through that service, a header saying the event ring was "deliberately not built" while the
same file's section 8b described it built, and a service doc listing shipped work as "future work".
Each was written true and went quietly false underneath. A dead PATH is the mechanically checkable half
of that class, so it is checked.

WHAT IS DELIBERATELY EXEMPT, and why it is not a loophole:

  audits/      Append-only EVIDENCE, not documentation. An audit that found something in
               `arch/arm/dwc2.rs` must keep saying so after that file is deleted - rewriting it would
               falsify the record. `docs/CLAUDE.md` states this split: docs are what you read, audits
               are evidence, CLAUDE.md is law.

  CLAUDE.md    Its amendment blocks are ratified history and name files that were REMOVED by the very
               amendment recording the removal. The body outside them is still checked.

Exit 0 when every referenced path resolves, 1 otherwise.
"""

import glob
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Docs a reader is expected to trust as CURRENT.
PATTERNS = [
    "docs/*.md",
    "utilities/*.md",
    "services/*/CLAUDE.md",
    "kernel/src/CLAUDE.md",
    "kernel/src/*/CLAUDE.md",
    "kernel/src/*/*/CLAUDE.md",
    "sdk/rust/CLAUDE.md",
    "website/src/*.md",
    "README.md",
    "GETTING_STARTED.md",
]

# See the module docstring: evidence and ratified history, not stale documentation.
EXEMPT_PREFIXES = ("audits/", "website/book/", "build/", "target/")

PATHISH = re.compile(r"`([A-Za-z0-9_./-]+/[A-Za-z0-9_./-]+\.(?:rs|md|toml|py|json|gsh))`")

# Paths named by prose that is ABOUT their removal. Listed one by one rather than matched by a
# heuristic on the surrounding words, so admitting one is a deliberate act that shows up in a diff -
# the same reason PLANNED is a list. Each entry is a file that was deleted and whose absence is the
# POINT of the sentence naming it.
HISTORICAL = {
    # The ARM USB stack moved to `services/dwc2`; these three docs are the record of that move, and
    # `kernel/src/arch/arm/CLAUDE.md` says outright "That file no longer exists".
    "arch/arm/dwc2.rs",
    "kernel/src/arch/arm/dwc2.rs",
    # The PIO storage path, superseded by AHCI + DMA. `docs/persistence.md` and `docs/ahci.md` record
    # why the no-DMA design was chosen and then replaced.
    "capability/hw_pio.rs",
    "sdk/rust/src/pio.rs",
    # A refactor `docs/aarch64.md` PROPOSED (a neutral console module). The kernel kept `bootcon`
    # instead, so the file was never created - the doc is a design note, not a description.
    "kernel/src/console.rs",
}

# Forward-looking references: a roadmap naming a document it proposes to write is not stale, and a
# config file the reader is told to CREATE is not missing.
PLANNED = {
    "docs/driver-guide.md",
    "docs/porting-guide.md",
    "docs/hardware-findings.md",
    ".cargo/mutants.toml",
    "src/main.rs",  # the scaffolding template path in GETTING_STARTED.md, not a repo file
}


def resolves(ref: str, doc_dir: str) -> bool:
    """True if `ref` names a real file under any of the roots docs write paths relative to."""
    candidates = [
        ref,
        os.path.join(doc_dir, ref),
        os.path.join("kernel/src", ref),
        os.path.join("sdk/rust/src", ref),
    ]
    parts = ref.split("/")
    if len(parts) >= 2:
        # `dwc2/net.rs` -> services/dwc2/src/net.rs
        candidates.append(os.path.join("services", parts[0], "src", *parts[1:]))
        candidates.append(os.path.join("services", parts[0], *parts[1:]))
    if ref.startswith("sdk/rust/"):
        candidates.append(os.path.join("sdk/rust/src", ref[len("sdk/rust/"):]))
    if ref.startswith("services/") and len(parts) >= 3:
        candidates.append(os.path.join("services", parts[1], "src", *parts[2:]))
    return any(os.path.exists(os.path.join(ROOT, c)) for c in candidates)


def main() -> int:
    docs = []
    for pat in PATTERNS:
        docs += glob.glob(os.path.join(ROOT, pat), recursive=True)

    dead = {}
    for path in sorted(set(docs)):
        rel = os.path.relpath(path, ROOT).replace(os.sep, "/")
        if rel.startswith(EXEMPT_PREFIXES):
            continue
        try:
            text = io.open(path, encoding="utf-8").read()
        except OSError:
            continue
        doc_dir = os.path.dirname(rel)
        for ref in sorted(set(PATHISH.findall(text))):
            if ref.startswith(("http", "build/", "target/", "os/")) or ref in PLANNED or ref in HISTORICAL:
                continue
            if not resolves(ref, doc_dir):
                dead.setdefault(rel, set()).add(ref)

    if not dead:
        n = len(set(docs))
        print(f"doc refs: every referenced path resolves ({n} docs scanned)")
        return 0

    total = sum(len(v) for v in dead.values())
    print(f"doc refs: {total} reference(s) point at files that do not exist\n")
    for doc, refs in sorted(dead.items()):
        print(f"  {doc}")
        for r in sorted(refs):
            print(f"      -> {r}")
    print(
        "\nA doc that points at a deleted file sends a reader after code that is not there.\n"
        "Fix the reference, or - if the file was deliberately removed - say so in prose\n"
        "instead of naming a path, as CLAUDE.md's amendments do."
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
