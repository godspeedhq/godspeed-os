#!/usr/bin/env python3
"""The SHARED SURFACE above the kernel: what a port may touch, and what it must not grow.

The kernel's arch seam is enforced from both directions already - `arch_boundary_check.py` proves the
neutral layers reach hardware only through `arch::imp`, and `arch_seam_check.py` proves every arch
answers every member of it. Above the kernel there was nothing, and that is where the cost actually
lands: a port that edits a shared SERVICE invalidates every other board's testing, silently, and the
operator finds out either by re-testing everything or by shipping a regression.

This does two separate jobs, and they answer different questions.

1. THE RATCHET (the part that can fail a build).

   `target_arch` in a service is a smell with a specific name: above the kernel it is almost always
   standing in for "which BOARD am I on", not "which instruction set". `nic-driver` picks dwmac /
   genet / e1000 / smsc95xx by instruction set - put a different NIC on a RISC-V board and it breaks,
   because the axis is wrong. Genuinely ISA-dependent code in a service belongs in the SDK, which is
   the seam services already have (CLAUDE.md 18.1 designates `sdk/mmio.rs` and `sdk/dma.rs` for
   exactly this).

   So the counts are FROZEN, in the same instrument as the grandfathered `unsafe` floors of 18.5 and
   `COMMANDMENTS.baseline.toml`: they may DECREASE freely and may increase only deliberately. A port
   that needs a new one has to say so out loud rather than add it in passing.

2. THE REPORT (`--report`, never fails).

   Against `main`, list every shared file this branch touched and say whether its changes are
   arch-gated or unconditional. That is the difference between "I have to test everything" and "these
   two files, these two boards".

   UNCONDITIONAL is deliberately NOT a failure. When a new board exposes a latent driver bug, the fix
   belongs in the shared driver where every port gets it - fencing it behind an arch gate would leave
   the others carrying a bug their own hardware has, which is the mistake the Pi 4 GENET work already
   paid for ("when a compliant path is gated behind a flag, the flag is the bug"). This exists to make
   the choice visible and to name the testing it obliges, not to prevent it.
"""

import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE = os.path.join(ROOT, "SHARED-SURFACE.baseline.txt")

# Where shared code lives. `sdk/` is the seam and is EXPECTED to carry arch cfgs - counted so a reader
# can see the shape, but it is the one place they are the right answer rather than a smell.
SHARED_ROOTS = ("services", "sdk")
ARCH_CFG = re.compile(r'target_arch\s*=\s*"([a-z0-9_]+)"')


def rel(path):
    return os.path.relpath(path, ROOT).replace(os.sep, "/")


def scan_counts():
    """Occurrences of an arch cfg per shared file, as {path: count}."""
    counts = {}
    for root_name in SHARED_ROOTS:
        root = os.path.join(ROOT, root_name)
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
            for fn in filenames:
                if not fn.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, fn)
                with open(full, encoding="utf-8", errors="replace") as fh:
                    n = len(ARCH_CFG.findall(fh.read()))
                if n:
                    counts[rel(full)] = n
    return counts


def read_baseline():
    if not os.path.exists(BASELINE):
        return None
    out = {}
    with open(BASELINE, encoding="utf-8") as fh:
        for line in fh:
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            count, path = line.split(None, 1)
            out[path.strip()] = int(count)
    return out


HEADER = (
    "# Arch-conditional code ABOVE the kernel, frozen. See scripts/shared_surface_check.py.\n"
    "#\n"
    "# THIS FILE MAY ONLY EVER SHRINK, on the same terms as the grandfathered unsafe floors of\n"
    "# CLAUDE.md 18.5: a count may DECREASE freely, and may increase only deliberately - by\n"
    "# regenerating this file in a commit that says why the port needed it.\n"
    "#\n"
    "# An `sdk/` entry is not a smell. The SDK is the seam services are supposed to reach hardware\n"
    "# through, so an arch cfg there is the right answer. Every `services/` entry is a claim that a\n"
    "# SERVICE cares about the instruction set - which above the kernel is usually standing in for\n"
    "# 'which board am I on', a different question on a wrong axis.\n"
    "#\n"
    "# count  path\n"
)


def write_baseline(counts):
    with open(BASELINE, "w", encoding="utf-8", newline="") as fh:
        fh.write(HEADER)
        for path in sorted(counts):
            fh.write("%5d  %s\n" % (counts[path], path))


def git(*args):
    try:
        # EXPLICIT utf-8, because the default is the console codepage: on Windows that is cp1252 and
        # the first source file containing a box-drawing character kills the whole report.
        r = subprocess.run(["git"] + list(args), cwd=ROOT, capture_output=True,
                           text=True, encoding="utf-8", errors="replace")
        return r.stdout.strip() if r.returncode == 0 else None
    except OSError:
        return None


def module_gate_of(path):
    """The arch this whole FILE is gated behind, if its `mod` declaration carries a cfg.

    A file can be entirely arch-specific without containing a single `target_arch` itself:
    `services/nic-driver/src/dwmac.rs` is 900 lines with none, because its parent writes
    `#[cfg(target_arch = "riscv64")] mod dwmac;`. Counting occurrences INSIDE the file therefore
    reads it as unconditional and claims it endangers every board, which is the opposite of true.
    """
    stem = os.path.basename(path)[:-3]
    crate_src = os.path.join(ROOT, os.path.dirname(path))
    if not os.path.isdir(crate_src):
        return None
    decl = re.compile(r"^\s*(pub\s+)?mod\s+%s\s*;" % re.escape(stem))
    for fn in os.listdir(crate_src):
        if not fn.endswith(".rs"):
            continue
        with open(os.path.join(crate_src, fn), encoding="utf-8", errors="replace") as fh:
            lines = fh.readlines()
        for i, line in enumerate(lines):
            if not decl.match(line):
                continue
            # Walk back over doc comments and other attributes to find a governing cfg.
            j = i - 1
            while j >= 0:
                prev = lines[j].strip()
                if prev.startswith("///") or prev.startswith("//"):
                    j -= 1
                    continue
                m = ARCH_CFG.search(prev)
                if prev.startswith("#[") and m:
                    return m.group(1)
                if prev.startswith("#["):
                    j -= 1
                    continue
                break
            return None
    return None


def arch_gated_lines(text):
    """Line numbers (1-based) covered by an arch cfg attribute, by tracking the item it governs.

    An attribute applies to the NEXT item, so its reach is that item's extent - a one-line
    `#[cfg(target_arch = "riscv64")]` above a 26-line function governs all 26. Counting attribute
    lines against body lines, which is what this did first, cannot see that and calls the SDK's
    riscv64 syscall stub unconditional.
    """
    lines = text.splitlines()
    covered = set()
    i = 0
    while i < len(lines):
        if not ARCH_CFG.search(lines[i]) or "#[" not in lines[i]:
            i += 1
            continue
        # Skip any further attributes and doc comments to reach the item itself.
        j = i + 1
        while j < len(lines) and (lines[j].lstrip().startswith("#[")
                                  or lines[j].lstrip().startswith("///")
                                  or not lines[j].strip()):
            j += 1
        if j >= len(lines):
            break
        # Extend to the item's end: a braced body to its matching close, otherwise to the `;`.
        depth = 0
        seen_brace = False
        k = j
        while k < len(lines):
            depth += lines[k].count("{") - lines[k].count("}")
            if "{" in lines[k]:
                seen_brace = True
            if seen_brace and depth <= 0:
                break
            if not seen_brace and lines[k].rstrip().endswith(";"):
                break
            k += 1
        for n in range(i + 1, min(k, len(lines) - 1) + 2):
            covered.add(n)
        i = k + 1
    return covered


def added_line_numbers(diff):
    """New-file line numbers of added lines, from the hunk headers."""
    out = []
    new_ln = 0
    for line in diff.splitlines():
        m = re.match(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
        if m:
            new_ln = int(m.group(1))
            continue
        if line.startswith("+++"):
            continue
        if line.startswith("+"):
            out.append((new_ln, line[1:]))
            new_ln += 1
        elif not line.startswith("-"):
            new_ln += 1
    return out


def report_against_main():
    """Which shared files this branch touched, and whether the changes are arch-gated."""
    base = git("merge-base", "main", "HEAD")
    if not base:
        print("shared-surface: no merge-base with `main` (detached, or main absent) - report skipped.")
        return
    changed = git("diff", "--name-only", base, "HEAD")
    if changed is None:
        print("shared-surface: could not diff against main - report skipped.")
        return
    files = [f for f in changed.splitlines() if f.startswith(SHARED_ROOTS) and f.endswith(".rs")]
    if not files:
        print("shared-surface: this branch touches NO shared service or SDK file, so no other board's")
        print("                testing is invalidated by it.")
        return
    print("shared-surface: %d shared file(s) changed against main. Each invalidates the other boards'"
          % len(files))
    print("                testing to the extent its changes are NOT arch-gated:")
    print()
    shared_total = 0
    for f in files:
        gate = module_gate_of(f)
        diff = git("diff", base, "HEAD", "--", f) or ""
        added = [(n, t) for n, t in added_line_numbers(diff)
                 if t.strip() and not t.lstrip().startswith("//")]
        if gate:
            print("  %-14s %-42s whole file is `#[cfg(target_arch = \"%s\")] mod`" % ("PORT-LOCAL", f, gate))
            continue
        if not added:
            print("  %-14s %-42s" % ("comments-only", f))
            continue
        text = git("show", "HEAD:" + f) or ""
        covered = arch_gated_lines(text)
        loose = [n for n, _ in added if n not in covered]
        if not loose:
            print("  %-14s %-42s %4d added code line(s), all inside an arch cfg"
                  % ("ARCH-GATED", f, len(added)))
        else:
            shared_total += len(loose)
            print("  %-14s %-42s %4d added code line(s), %d OUTSIDE any arch cfg"
                  % ("SHARED", f, len(added), len(loose)))
    print()
    print("                %d added line(s) run on EVERY board." % shared_total)
    print()
    print("                SHARED is not a violation. A latent driver bug that a new board exposes")
    print("                SHOULD be fixed for every port; gating it would leave the others carrying")
    print("                a bug their own hardware has. It is a TESTING obligation, not a fault:")
    print("                those lines run on every board, so every board is where they are proven.")


def main():
    if "--bless" in sys.argv:
        write_baseline(scan_counts())
        print("shared-surface: baseline rewritten from the working tree - say why in the commit.")
        return 0

    counts = scan_counts()
    baseline = read_baseline()
    if baseline is None:
        write_baseline(counts)
        print("shared-surface: no baseline; wrote one from the working tree.")
        return 0

    grew = [(p, baseline.get(p, 0), n) for p, n in sorted(counts.items()) if n > baseline.get(p, 0)]
    if grew:
        print("SHARED SURFACE GREW - arch-conditional code was ADDED above the kernel:")
        print()
        for path, was, now in grew:
            print("  %s: %d -> %d" % (path, was, now))
        print()
        print("Above the kernel, `target_arch` is usually standing in for 'which BOARD am I on' - a")
        print("different question on a wrong axis, since a board with different hardware on the SAME")
        print("instruction set breaks it. Genuinely ISA-dependent code belongs in the SDK, which is")
        print("the seam services already have (CLAUDE.md 18.1).")
        print()
        print("If the port really needs it, run `python scripts/shared_surface_check.py --bless` and")
        print("say why in the commit. Refusing quietly is the point: this is the surface that makes")
        print("every other board's testing uncertain.")
        return 1

    shrank = sum(1 for p, n in counts.items() if n < baseline.get(p, 0))
    total = sum(counts.values())
    print("Shared-surface check passed - %d arch-conditional site(s) above the kernel, none added%s."
          % (total, (", %d file(s) shrank" % shrank) if shrank else ""))
    if "--report" in sys.argv:
        print()
        report_against_main()
    return 0


if __name__ == "__main__":
    sys.exit(main())
