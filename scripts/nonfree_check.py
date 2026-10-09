#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Every vendor blob under `nonfree/` carries its LICENCE and a verified PROVENANCE, or the build fails.

WHY THIS EXISTS. `docs/wifi.md` section 8 decides that redistributable device firmware IS committed to
this repository, in `nonfree/`, rather than fetched at setup. That decision is defensible - the licence
permits redistribution with the notice attached, and a fetch script has to reimplement somebody else's
packaging logic and re-breaks every time they change it. But it creates two obligations that a document
cannot hold on its own:

  1. **The notice must travel with every copy.** That is the licence's own requirement, and a repository
     is a copy. A blob with no `LICENCE` beside it is a licence violation sitting in git history, and
     nobody notices a missing file.
  2. **A binary nobody can check is a binary nobody should trust.** A reader cannot read it, cannot
     diff it usefully, and cannot tell a vendor's file from something a contributor built. Without a
     recorded origin and digest, "where did this come from" has no answer.

So the rule is mechanical: a directory under `nonfree/` must contain `LICENCE` and `PROVENANCE`, and
every payload file in it must appear in `PROVENANCE` with a SHA-256 that MATCHES ITS CONTENT. A
contributor adding a driver that needs a blob gets an obvious place to put it and is made to declare
these two things, or the build refuses the change.

WHAT THIS DOES NOT DO, deliberately: it does not judge whether a licence permits redistribution. It
cannot - that is a reading, not a computation. What it guarantees is that the terms are PRESENT and the
bytes are ACCOUNTED FOR, so a human reviewing a pull request has both in front of them. Some licences
forbid redistribution outright; for those the answer is `scripts/get_firmware.py`, fetched on the owner's
machine, and this gate is what keeps the two cases from being confused.

THE DIGEST IS THE LOAD-BEARING PART. A size would catch truncation; a digest catches substitution, and it
lets anyone verify our copy against upstream without trusting this project. Recording a fact about a
binary is the opposite of recording the binary and hoping.
"""
import hashlib
import io
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NONFREE = os.path.join(ROOT, "nonfree")

# The two files every vendor directory owes, and which are not themselves payload.
META = {"LICENCE", "LICENSE", "PROVENANCE"}

SHA_RE = re.compile(r"\b([0-9a-f]{64})\b")


def tracked_files():
    """The set of paths git actually tracks under `nonfree/`, as forward-slash relatives.

    WHY THIS EXISTS, because it was learned by this check passing when it should not have. The guarantee
    wanted here is a property of the REPOSITORY - that a blob this project DISTRIBUTES carries its notice -
    and the first version of this file read only the filesystem. A pre-existing `*.bin` rule in
    `.gitignore` kept the firmware out of its own commit while its licence and provenance went in, and this
    check reported three blobs all correctly licensed. A file nobody clones distributes nothing, and the
    author is the one person who cannot notice, because it works on their machine.
    """
    try:
        out = subprocess.run(["git", "ls-files", "nonfree"], cwd=ROOT,
                             capture_output=True, text=True, check=True).stdout
    except Exception:  # noqa: BLE001 - no git, or not a checkout
        # Said rather than assumed: without git this check is weaker, and a reader should know which
        # guarantee they are getting.
        print("nonfree: WARNING - cannot ask git what is tracked; licence and digest are still checked")
        return None
    return {line.strip().replace("\\", "/") for line in out.splitlines() if line.strip()}


def read(path):
    with io.open(path, "rb") as fh:
        return fh.read()


def check_dir(rel, path, out, tracked):
    names = sorted(os.listdir(path))
    files = [n for n in names if os.path.isfile(os.path.join(path, n))]
    payload = [n for n in files if n not in META]

    have_licence = any(n in ("LICENCE", "LICENSE") for n in files)
    prov_path = os.path.join(path, "PROVENANCE")

    if not payload:
        out.append("%s: no payload files - an empty vendor directory is debt, not policy" % rel)
        return

    if not have_licence:
        out.append("%s: %d blob(s) and NO LICENCE. The notice must travel with every copy, and a "
                   "repository is a copy - this is a licence violation in git history, not an "
                   "oversight." % (rel, len(payload)))
    if not os.path.isfile(prov_path):
        out.append("%s: no PROVENANCE. A binary nobody can check is a binary nobody should trust: "
                   "record where each file came from and its SHA-256." % rel)
        return

    prov = io.open(prov_path, encoding="utf-8", errors="replace").read()
    listed = set(SHA_RE.findall(prov))

    for n in payload:
        # TRACKED FIRST, because an untracked file makes every other check here meaningless: the licence
        # it is paired with ships to nobody, and the digest describes bytes no clone receives. Named
        # separately because "not committed" and "no licence" need different fixes.
        if tracked is not None and "%s/%s" % (rel, n) not in tracked:
            out.append("%s/%s: present on disk but NOT TRACKED BY GIT. Its licence and digest then "
                       "guarantee nothing - a file nobody clones distributes nothing. Check .gitignore."
                       % (rel, n))
            continue
        digest = hashlib.sha256(read(os.path.join(path, n))).hexdigest()
        if digest not in listed:
            # Named separately from "absent from PROVENANCE" because the two mean different things: a
            # file nobody recorded, versus a file whose bytes have CHANGED since somebody did.
            if n in prov:
                out.append("%s/%s: PROVENANCE mentions it but no recorded digest matches its content "
                           "(sha256 %s). The file changed, or the record did." % (rel, n, digest))
            else:
                out.append("%s/%s: not in PROVENANCE (sha256 %s). Add it with where it came from."
                           % (rel, n, digest))


def main():
    if not os.path.isdir(NONFREE):
        # Not a failure. The tree exists when something needs it (26.2), and saying so beats silence.
        print("nonfree: no `nonfree/` directory - nothing to check")
        return 0

    tracked = tracked_files()
    out = []
    dirs = 0
    payloads = 0
    for entry in sorted(os.listdir(NONFREE)):
        path = os.path.join(NONFREE, entry)
        if not os.path.isdir(path):
            # A loose file at the top level has no licence or provenance of its own and never can.
            out.append("nonfree/%s: a file at the top level - every blob belongs in a directory with "
                       "its own LICENCE and PROVENANCE" % entry)
            continue
        dirs += 1
        before = len(out)
        check_dir("nonfree/" + entry, path, out, tracked)
        if len(out) == before:
            payloads += len([n for n in sorted(os.listdir(path))
                             if os.path.isfile(os.path.join(path, n)) and n not in META])

    if out:
        print("Nonfree blob check - FAILURES:")
        for line in out:
            print("  FAIL  %s" % line)
        print("%d violation(s). See docs/wifi.md 8 and docs/licensing.md for the policy." % len(out))
        return 1

    print("nonfree: %d vendor director(y/ies), %d blob(s), every one with a LICENCE and a digest that "
          "matches" % (dirs, payloads))
    return 0


if __name__ == "__main__":
    sys.exit(main())
