#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""ONLY `gs`: which crates may depend on the SDK at all, each one named with its reason.

`backlog/71`, step 2: "done when no service or driver crate depends on the SDK directly - only `gs` and the
domain libraries do, and the SDK is an internal layer ... Each is either moved behind `gs`, or named as an
exception with its reason where the gate can see it." `one_way_check.py` counts the raw SDK CALLS a crate
makes that `gs` covers; this asks the question underneath it, whether a crate LISTS `godspeed-sdk` in its
`Cargo.toml` at all. A crate that does not cannot reach the SDK, so its whole surface is `gs`.

THE RULE. A crate under `services/` or `examples/` may list `godspeed-sdk` only if
`scripts/SDK-DEPENDENCY.baseline.txt` names it, with a reason. So:
  - a NEW crate starts without it, and is written on `gs` from its first line;
  - a crate that stops depending on the SDK must leave the list, so the list never claims more than is true
    and can only shrink;
  - adding a crate to the list is a decision with its reason in the file, said again in the commit
    (CLAUDE.md 21: a gate is not weakened silently).

The exceptions are crates whose work IS what the SDK exposes and `gs` deliberately does not: the supervisor's
spawn ABI, the trace wire format's two ends, the USB hosts' protocols, the test-only `adversarial` module
(CLAUDE.md 18.1). `gs` answers "how is a Godspeed program written", and these are not ordinary programs.
A reason that names something `gs` merely LACKS yet (not something it should never have) marks a gap to
close, and the crate leaves the list when it is closed.

`--list` prints the exceptions and their reasons.
"""
import io
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "scripts", "SDK-DEPENDENCY.baseline.txt")
TOPS = ("services", "examples")
DEP = re.compile(r'^\s*godspeed-sdk\s*=', re.M)
# A reason is a sentence, not a placeholder: a one-word entry would satisfy the letter of the rule and none
# of its point, which is that a reviewer can read WHY.
MIN_REASON = 20


def depending_crates():
    """{crate name: Cargo.toml path} for every crate under the tops that lists `godspeed-sdk`."""
    found = {}
    for top in TOPS:
        base = os.path.join(ROOT, top)
        if not os.path.isdir(base):
            continue
        for name in sorted(os.listdir(base)):
            toml = os.path.join(base, name, "Cargo.toml")
            if not os.path.isfile(toml):
                continue
            with io.open(toml, encoding="utf-8") as fh:
                body = re.sub(r"#[^\n]*", "", fh.read())   # a comment naming the SDK is not a dependency
            if DEP.search(body):
                found[name] = os.path.relpath(toml, ROOT).replace(os.sep, "/")
    return found


def load_baseline():
    """{crate: reason}, and the problems with the file itself."""
    entries, problems = {}, []
    if not os.path.isfile(BASELINE):
        return entries, [f"{os.path.relpath(BASELINE, ROOT)} is missing"]
    for n, line in enumerate(io.open(BASELINE, encoding="utf-8"), 1):
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        crate, sep, reason = line.partition("=")
        crate, reason = crate.strip(), reason.strip()
        if not sep or not crate:
            problems.append(f"line {n}: not `crate = reason`")
        elif len(reason) < MIN_REASON:
            problems.append(f"line {n}: `{crate}` has no reason worth the name - say why it needs the SDK")
        elif crate in entries:
            problems.append(f"line {n}: `{crate}` is listed twice")
        else:
            entries[crate] = reason
    return entries, problems


def main():
    deps = depending_crates()
    allowed, problems = load_baseline()

    if "--list" in sys.argv:
        print(f"sdk dependency: {len(allowed)} crate(s) may depend on the SDK:")
        for crate in sorted(allowed):
            print(f"  {crate:14} {allowed[crate]}")
        return 0

    unlisted = sorted(set(deps) - set(allowed))
    stale = sorted(set(allowed) - set(deps))

    if problems or unlisted or stale:
        print("sdk dependency - FAILURES:")
        for p in problems:
            print(f"  FAIL  {os.path.relpath(BASELINE, ROOT)} {p}")
        for crate in unlisted:
            print(f"  FAIL  {deps[crate]} lists godspeed-sdk, and `{crate}` is not a named exception. Write it on")
            print(f"        `gs` (the SDK's ServiceContext is `godspeed::ServiceContext`, its Message is")
            print(f"        `godspeed::ipc::Message`). If it genuinely needs what only the SDK has, add")
            print(f"        `{crate} = <why>` to scripts/SDK-DEPENDENCY.baseline.txt and say why in the commit.")
        for crate in stale:
            print(f"  FAIL  `{crate}` is named as an exception but no longer depends on the SDK (or no longer")
            print(f"        exists). Remove its line from scripts/SDK-DEPENDENCY.baseline.txt: the list only shrinks.")
        print("")
        print("backlog/71 step 2: a service or driver depends on `gs`; the SDK is an internal layer.")
        return 1

    crates = sum(1 for top in TOPS for d in (os.listdir(os.path.join(ROOT, top)) if os.path.isdir(os.path.join(ROOT, top)) else [])
                 if os.path.isfile(os.path.join(ROOT, top, d, "Cargo.toml")))
    print(f"sdk dependency: {crates - len(deps)} of {crates} crates depend on `gs` alone; the other {len(deps)} are "
          f"named exceptions, each with its reason (--list)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
