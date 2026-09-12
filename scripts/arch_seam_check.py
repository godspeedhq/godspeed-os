#!/usr/bin/env python3
"""Every arch must provide every `arch::imp` member the NEUTRAL kernel actually calls.

WHY. `arch_boundary_check.py` enforces the boundary in one direction: neutral code may only reach
hardware through `arch::imp`. This enforces the other direction - that each arch actually ANSWERS
that seam.

The gap was real and it cost a rotted port. `kernel/src/arch/riscv64/` sat in the tree accumulating
35 compile errors, every one a seam member the neutral kernel had grown since the stub was written.
The compiler would have said so instantly - but nothing ever built that target, so the errors were
latent rather than loud. Same shape as a test that only runs when named: a rule enforced on one build
path is enforced on none.

Compiling every arch would also catch it, and costs minutes per target plus a toolchain for each.
This costs a grep, needs no toolchain, and can therefore run on every build path - which is the
property that actually decides whether a check protects anything.

THE SEAM IS DEFINED BY USE, not by a list somebody maintains. A list would drift from the code it
describes, which is the failure being fixed. So the members are discovered by scanning the neutral
kernel for what it calls, and any arch missing one is named.

Exit 0 when every arch answers the seam, 1 otherwise.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KSRC = os.path.join(ROOT, "kernel", "src")
ARCHES = os.path.join(KSRC, "arch")

# A call under `#[cfg(target_arch = "...")]` is required only of THAT arch. Ignoring this produced
# three false alarms on aarch64 and arm - both of which compile - and a check with false alarms gets
# switched off, which is worse than no check at all (the lesson `site_check.py` records).
CFG_ARCH = re.compile(r'#\[cfg\([^\]]*target_arch\s*=\s*"[^"]+"[^\]]*\)\]')
NEWLINE = chr(10)


# SCAFFOLDS, not ports. These exist to prove the seam generalises to a fourth and fifth ISA; nothing
# builds them and no board is targeted, so they are reported as drift-to-be-expected rather than
# failing the check. Removing a name from here is how a scaffold BECOMES a port - a deliberate act,
# the same shape as pinning a feature in COMMANDMENTS.baseline.toml.
#
# `riscv64` was on this list and has come off it: it now compiles and boots under QEMU `virt`, so it
# is held to the seam like any other port.
SCAFFOLDS = {
    "riscv32":     "no board targeted; rv32 is a size study, not a port",
    "loongarch64": "no toolchain target installed, no hardware",
    "s390x":       "no toolchain target installed, no hardware",
}


def arch_dirs():
    """An arch dir with no `mod.rs` is not yet a port."""
    out = []
    for n in sorted(os.listdir(ARCHES)):
        d = os.path.join(ARCHES, n)
        if os.path.isdir(d) and os.path.exists(os.path.join(d, "mod.rs")):
            out.append(n)
    return out


def read(p):
    try:
        with open(p, encoding="utf-8", errors="ignore") as f:
            return f.read()
    except OSError:
        return ""


def neutral_files():
    """Everything under kernel/src EXCEPT kernel/src/arch - the layers held to the boundary."""
    for base, dirs, files in os.walk(KSRC):
        if os.path.abspath(base).startswith(os.path.abspath(ARCHES)):
            continue
        dirs[:] = [d for d in dirs if os.path.join(base, d) != ARCHES]
        for f in files:
            if f.endswith(".rs"):
                yield os.path.join(base, f)


def strip_arch_gated(text):
    """Drop what a `#[cfg(target_arch = ...)]` attribute guards: the braced item that follows, or the
    rest of the statement when it guards a single expression."""
    out, i = [], 0
    for m in CFG_ARCH.finditer(text):
        if m.start() < i:
            continue                      # already inside a region an earlier gate swallowed
        out.append(text[i:m.start()])
        j = m.end()
        while j < len(text) and text[j].isspace():
            j += 1
        brace = text.find("{", j)
        semi = text.find(";", j)
        nl = text.find(NEWLINE, j)
        candidates = [c for c in (brace, semi, nl) if c != -1]
        if not candidates:
            i = len(text)
            continue
        first = min(candidates)
        if first == brace:
            depth, k = 0, brace
            while k < len(text):
                if text[k] == "{":
                    depth += 1
                elif text[k] == "}":
                    depth -= 1
                    if depth == 0:
                        k += 1
                        break
                k += 1
            i = k
        else:
            i = first + 1
    out.append(text[i:])
    return "".join(out)


def wanted():
    """`arch::imp::NAME` and `arch::imp::MOD::NAME` as the neutral kernel writes them, excluding
    anything reached only under a `target_arch` gate."""
    pat = re.compile(r"arch::imp::([a-z_][a-z0-9_]*)(?:::([A-Za-z_][A-Za-z0-9_]*))?")
    top, moded = set(), {}
    for f in neutral_files():
        for m in pat.finditer(strip_arch_gated(read(f))):
            a, b = m.group(1), m.group(2)
            if b is None:
                top.add(a)
            else:
                moded.setdefault(a, set()).add(b)
    # A name used BOTH ways (`imp::pci` alone and `imp::pci::init`) is a module, not a member.
    top -= set(moded)
    return top, moded


def arch_text(arch):
    """An arch's whole source - members may live in submodules the arch re-exports."""
    d = os.path.join(ARCHES, arch)
    out = []
    for base, _, files in os.walk(d):
        for f in files:
            if f.endswith(".rs"):
                out.append(read(os.path.join(base, f)))
    return NEWLINE.join(out)


def defines(text, name):
    """Is `name` defined as a public item, or brought in by a re-export?"""
    if re.search(r"pub\s+(?:unsafe\s+)?(?:fn|const|static|struct|enum|type|mod)\s+%s\b"
                 % re.escape(name), text):
        return True
    for m in re.finditer(r"pub\s+use\s+[^;]+;", text):
        if re.search(r"\b%s\b" % re.escape(name), m.group(0)):
            return True
    return False


def main():
    top, moded = wanted()
    arches = arch_dirs()
    problems = []

    scaffold_gaps = {}
    for arch in arches:
        text = arch_text(arch)
        sink = scaffold_gaps.setdefault(arch, []) if arch in SCAFFOLDS else problems
        for name in sorted(top):
            if not defines(text, name):
                sink.append("%s: missing `arch::imp::%s`" % (arch, name))
        for mod, members in sorted(moded.items()):
            if not defines(text, mod):
                sink.append("%s: missing module `arch::imp::%s`" % (arch, mod))
                continue
            for name in sorted(members):
                if not defines(text, name):
                    sink.append("%s: missing `arch::imp::%s::%s`" % (arch, mod, name))

    total = len(top) + sum(len(v) for v in moded.values())
    ports = [a for a in arches if a not in SCAFFOLDS]

    if not problems:
        print("arch seam: all %d port(s) answer every one of the %d `arch::imp` members the neutral "
              "kernel calls" % (len(ports), total))
        print("  ports:     %s" % ", ".join(ports))
        for a, gaps in sorted(scaffold_gaps.items()):
            if gaps:
                # Said out loud, every run. A scaffold that is quietly behind is how `riscv64` reached
                # 35 compile errors without anyone knowing.
                print("  scaffold:  %-12s %d member(s) behind - %s" % (a, len(gaps), SCAFFOLDS[a]))
        return 0

    print("arch seam: %d member(s) missing - an arch that does not answer the seam cannot compile,"
          % len(problems))
    print("and will not be noticed until somebody builds that target." + NEWLINE)
    for p in problems:
        print("  %s" % p)
    print(NEWLINE + "Add a body (a stub is fine, and honest) to the arch named, or - if the member is")
    print("genuinely x86-only - stop calling it from the neutral layers.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
