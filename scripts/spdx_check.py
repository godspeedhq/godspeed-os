#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Every tracked Rust file says which licence it is under, and it is the licence of its tree.

WHY. `docs/licensing.md` puts the OS under GPL-2.0-only and the SDK (`sdk/rust`), the standard library
(`stdlib/rust`) and `examples/` under Apache-2.0, and says per-file SPDX tags mark every file. Thirteen
did not carry one - nine in `services/dwc2`, `net-stack`'s `tcp.rs`, `probe`'s `table.rs`,
`wifi-driver`'s `main.rs`, `osdev`'s `fs_model.rs` - and nothing would have noticed a fourteenth
(backlog/80 R1). A file with no tag is a file whose terms a reader must guess, and the guess matters
most where the two licences meet: the Apache crates are what a service links, so a GPL file inside one
is the case the split exists to prevent.

WHAT IT CHECKS. In every `.rs` file git tracks: an `SPDX-License-Identifier:` in the first five lines,
naming a licence this project uses; and, under a tree `docs/licensing.md` gives a licence, that licence.

THE EXCEPTIONS ARE NAMED, with the reason, and may only shrink. Two `sdk/rust` files are tagged
GPL-2.0-only inside the Apache-2.0 SDK. Changing a file's licence is the copyright holder's decision,
not a checker's, so they are recorded here rather than retagged - and a new mismatch fails.

Exit 0 when every file passes, 1 otherwise.
"""
from __future__ import annotations

import io
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TAG = re.compile(r"SPDX-License-Identifier:\s*([A-Za-z0-9.+-]+)")
KNOWN = {"GPL-2.0-only", "Apache-2.0"}

# docs/licensing.md, "Licensing intent": the permissive trees. Everything else in the repository is the OS.
TREE_LICENCE = [("sdk/rust/", "Apache-2.0"), ("stdlib/rust/", "Apache-2.0"), ("examples/", "Apache-2.0")]

MISMATCH_RECORDED = {
    "sdk/rust/src/churn.rs": "GPL-2.0-only in the Apache-2.0 SDK; relicensing is the copyright holder's call",
    "sdk/rust/src/trace.rs": "GPL-2.0-only in the Apache-2.0 SDK; relicensing is the copyright holder's call",
}


def tracked_rust():
    out = subprocess.run(["git", "ls-files", "*.rs"], cwd=ROOT, capture_output=True, text=True)
    if out.returncode != 0:
        raise SystemExit("spdx: `git ls-files` failed - refusing to pass a check that listed nothing")
    return [p for p in out.stdout.split("\n") if p.strip()]


def main():
    files = tracked_rust()
    if not files:
        raise SystemExit("spdx: no tracked .rs files found - refusing to pass a check that read nothing")
    bad = []
    seen_recorded = set()
    for rel in files:
        path = os.path.join(ROOT, rel)
        if not os.path.isfile(path):
            continue
        head = "\n".join(io.open(path, encoding="utf-8", errors="replace").read().split("\n")[:5])
        m = TAG.search(head)
        if not m:
            bad.append("%s: no `SPDX-License-Identifier:` in its first five lines" % rel)
            continue
        lic = m.group(1)
        if lic not in KNOWN:
            bad.append("%s: `%s` is not a licence this project uses (%s)" % (rel, lic, ", ".join(sorted(KNOWN))))
            continue
        want = next((l for prefix, l in TREE_LICENCE if rel.startswith(prefix)), "GPL-2.0-only")
        if lic != want:
            if rel in MISMATCH_RECORDED:
                seen_recorded.add(rel)
            else:
                bad.append("%s: tagged %s, but its tree is %s (docs/licensing.md)" % (rel, lic, want))
    for rel in sorted(set(MISMATCH_RECORDED) - seen_recorded):
        bad.append("%s: a recorded mismatch that no longer mismatches - delete it from "
                   "MISMATCH_RECORDED (the list may only shrink)" % rel)
    if bad:
        print("spdx: %d file(s) do not say their licence, or say the wrong one:" % len(bad))
        for b in bad:
            print("  " + b)
        print("\nPut `// SPDX-License-Identifier: <licence>` on the first line: GPL-2.0-only for the OS,")
        print("Apache-2.0 under sdk/rust/, stdlib/rust/ and examples/ (docs/licensing.md).")
        return 1
    print("spdx: all %d tracked .rs files carry a licence tag, each the licence of its tree "
          "(%d recorded exception(s))" % (len(files), len(MISMATCH_RECORDED)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
