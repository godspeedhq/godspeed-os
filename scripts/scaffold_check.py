#!/usr/bin/env python3
"""The BOUNDED-PORT TEST: how far does a fresh ISA get with only `arch/<isa>/` written?

CLAUDE.md 4.1 makes a claim about what this kernel is for: "a port is bounded to `arch/<isa>/`: you
write that directory and nothing else in the kernel changes". Two checkers defend it -
`arch_boundary_check.py` (neutral code may not name an arch) and `arch_seam_check.py` (every arch
answers every seam member) - and both are NECESSARY-but-not-sufficient. They prove no rule is broken.
They cannot prove a new ISA actually works, because nothing ever tries one.

That gap was not theoretical. `arch_seam_check.py` EXEMPTS the scaffold arches -

    SCAFFOLDS = {"riscv32": ..., "loongarch64": ..., "s390x": ...}
    "...reported as drift-to-be-expected rather than failing the check"

- so "the seam generalises to a fifth ISA" was ASSERTED for as long as the scaffolds existed, and
never once tested. The first time anyone ran `cargo build -p kernel --target
loongarch64-unknown-none-softfloat` it produced 37 errors across 13 unanswered seam members.

This script closes that. It is the difference between COUNTING arch-conditional code (a proxy: the
number can fall without the bound improving) and EXERCISING the bound (the proposition itself). And
its failures are the enumeration - you do not have to guess which of the shared surface matters,
because anything a fresh ISA cannot get past without editing above `arch/` is named by the compiler.

THE RULE THAT MAKES IT A TEST
    To advance a milestone you may write ONLY `arch/<isa>/`.
    Anything else you must touch is a FINDING - record it; it is the work.

THE LADDER, and why it stops where it does:

    M1  compiles     the kernel builds for the target: every seam member answered
    M2  boots        reaches its UART under QEMU and prints
    M3  kernel up    neutral kernel steady state - memory, scheduler, IPC, capabilities
    M4  userspace    SPAWNS THE SUPERVISOR

M4 is the real bar, because bringing up userspace is the kernel's whole job - and it is where the
ABOVE-kernel surface first bites (the SDK's `hwclass` list, the supervisor's per-arch arms). Past M4
is driver work, which is genuinely board-specific and does not belong in a boundedness claim.

WHAT THIS DOES NOT TEST, stated plainly so nobody reads more into a pass than it means: it exercises
the ISA axis only. It would NOT have caught the riscv64 spawn-order split, where two boards with the
SAME disk topology and DIFFERENT instruction sets needed different handling - that is the "which
BOARD am I on" axis, which is what most of the shared surface above the kernel actually is. Different
failure mode, different instrument (`shared_surface_check.py`).

Only M1 is MEASURED today. M2-M4 need a bootable image per scaffold (linker script, load address,
QEMU machine), which is real port work; they are listed so the ladder is visible and are reported as
"not measured" rather than assumed failed - a milestone nobody has attempted is not a regression.

Exit: 0 if no scaffold went BACKWARDS, 1 otherwise. Advancing is reported and needs `--bless`.
"""

import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "SCAFFOLD.baseline.txt")

# The scaffolds, and the rustc target each is built for. These are deliberately three DIFFERENT
# probes rather than three of the same thing, which is why all three are kept:
#   riscv32      a 32-bit target - breaks the kernel's 64-bit assumptions (it found `AtomicU64`)
#   loongarch64  a clean 64-bit little-endian ISA - the control, so a failure is about BOUNDEDNESS
#                rather than about word size or byte order
#   s390x        BIG-ENDIAN - the only probe for byte-order assumptions anywhere in this repo
SCAFFOLDS = {
    "riscv32":     "riscv32imac-unknown-none-elf",
    "loongarch64": "loongarch64-unknown-none-softfloat",
    "s390x":       "s390x-unknown-none-softfloat",
}

MILESTONES = {
    0: "nothing yet",
    1: "compiles      (the kernel builds: every seam member answered)",
    2: "boots         (reaches its UART under QEMU and prints)",
    3: "kernel up     (neutral kernel steady state)",
    4: "userspace     (spawns the supervisor)",
}
MEASURED_UP_TO = 1  # everything above this is reported, never asserted

# `can't find crate for core` means the target's `rust-std` is not installable on this toolchain -
# a TOOLCHAIN fact, not a seam failure. Conflating the two would report a port as broken when nobody
# can build it at all, which is the "instrument that prints a zero it did not earn" shape.
NO_STD = re.compile(r"can't find crate for `core`")


def measure(arch, target):
    """(milestone, note). `None` milestone = could not be measured here."""
    r = subprocess.run(["cargo", "build", "-q", "-p", "kernel", "--target", target],
                       cwd=ROOT, capture_output=True, text=True)
    out = r.stdout + r.stderr
    if NO_STD.search(out):
        return None, "toolchain: no rust-std for this target on the pinned nightly"
    errors = [l for l in out.splitlines() if l.startswith("error")]
    if not errors:
        return 1, "compiles"
    # The most common unanswered seam members, so the report says WHAT to write next.
    missing = sorted(set(re.findall(r"cannot find \w+ `([A-Za-z_0-9]+)`", out)))
    note = "%d error(s)" % len(errors)
    if missing:
        note += "; unanswered: " + ", ".join(missing[:6]) + ("..." if len(missing) > 6 else "")
    return 0, note


def read_baseline():
    if not os.path.exists(BASELINE):
        return {}
    out = {}
    with open(BASELINE, encoding="utf-8") as fh:
        for line in fh:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = line.split()
            if len(parts) == 2:
                out[parts[0]] = int(parts[1])
    return out


def write_baseline(reached):
    with open(BASELINE, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("# The bounded-port test: how far each scaffold ISA gets with ONLY arch/<isa>/\n")
        fh.write("# written. See scripts/scaffold_check.py. A milestone may ADVANCE (bless it) and\n")
        fh.write("# may never go backwards - a regression means a fresh ISA lost ground, which is\n")
        fh.write("# the bound eroding.\n")
        for a in sorted(reached):
            fh.write("%-14s %d\n" % (a, reached[a]))


def main():
    bless = "--bless" in sys.argv
    base = read_baseline()
    reached, notes, unavailable = {}, {}, []

    for arch in sorted(SCAFFOLDS):
        m, note = measure(arch, SCAFFOLDS[arch])
        notes[arch] = note
        if m is None:
            unavailable.append(arch)
            continue
        reached[arch] = m

    print("Bounded-port test - how far a fresh ISA gets with only arch/<isa>/ written")
    print("  (measured up to M%d; M%d+ are listed, not asserted)" % (MEASURED_UP_TO, MEASURED_UP_TO + 1))
    print()
    for arch in sorted(SCAFFOLDS):
        if arch in unavailable:
            print("    %-14s  --   %s" % (arch, notes[arch]))
            continue
        m = reached[arch]
        was = base.get(arch)
        mark = ""
        if was is not None and m > was:
            mark = "   ADVANCED from M%d" % was
        elif was is not None and m < was:
            mark = "   *** REGRESSED from M%d ***" % was
        print("    %-14s  M%d  %s%s" % (arch, m, MILESTONES[m], mark))
        if m < MEASURED_UP_TO:
            print("                       %s" % notes[arch])
    print()

    regressed = [a for a, m in reached.items() if a in base and m < base[a]]
    advanced = [a for a, m in reached.items() if a in base and m > base[a]]
    new = [a for a in reached if a not in base]

    if bless:
        keep = dict(base)
        keep.update(reached)
        write_baseline(keep)
        print("scaffold: baseline rewritten from the working tree - say why in the commit.")
        return 0

    if regressed:
        for a in regressed:
            print("  %s went BACKWARDS: M%d -> M%d" % (a, base[a], reached[a]))
        print()
        print("A scaffold losing ground means a fresh ISA can no longer get as far as it could -")
        print("the bound eroding, usually because neutral code grew an assumption. Fix it, or if the")
        print("loss is deliberate run `python scripts/scaffold_check.py --bless` and say why.")
        return 1

    if advanced or new:
        for a in advanced:
            print("  %s ADVANCED M%d -> M%d - run --bless to lock it in." % (a, base[a], reached[a]))
        for a in new:
            print("  %s is new at M%d - run --bless to record it." % (a, reached[a]))
        return 0

    print("scaffold check passed - no scaffold lost ground.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
