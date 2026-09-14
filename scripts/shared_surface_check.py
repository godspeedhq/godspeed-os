#!/usr/bin/env python3
"""The SHARED SURFACE: everything OUTSIDE `arch/` that still knows which ISA it was built for.

The kernel's arch seam is enforced from two directions already - `arch_boundary_check.py` proves the
neutral layers reach hardware only through `arch::imp`, and `arch_seam_check.py` proves every arch
answers every member of it. Neither counts `#[cfg(target_arch)]`, and that is the gap this closes.

A port that edits a shared SERVICE invalidates every other board's testing, silently, and the operator
finds out either by re-testing everything or by shipping a regression.

THE NEUTRAL KERNEL IS SCANNED TOO, AND IT WAS NOT BEFORE. `arch_boundary_check.py` forbids exactly two
things outside `arch/`: inline assembly, and naming an arch MODULE (`arch::aarch64::`). A line like

    #[cfg(target_arch = "arm")]
    HwClass::Dwc2 => &[crate::arch::imp::irq::USB_VECTOR],      // kernel/src/task/mod.rs

breaks neither rule and passes - while being precisely what that script's own header says must not
exist: "implement `arch/<new>/` to the same surface, touch zero neutral files". CLAUDE.md 4.1 names
this debt and counts it BY HAND ("neutral code still names `arm` in 8 places, `aarch64` in 4 and
`x86_64` in 3"), which is the shape that drifts: a hand count is right on the day it is written and
silently wrong afterwards. Measured here instead, so it can only fall.

The two halves are the same property asked of two layers, which is why they share a ratchet rather
than getting a second script: code outside `arch/` that knows the ISA is what makes a port unbounded,
whether it sits in `kernel/src/task/` or in `services/`. What differs is the DIAGNOSIS, and only the
services half gets the "which BOARD am I on" reading below - in the neutral kernel the ISA usually is
the real question, and the fix is a new `arch::imp` member rather than a board fact.

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

# A SECOND baseline, in its own file, counting `#[cfg` ATTRIBUTES rather than arch NAMES.
#
# WHY TWO UNITS. The name count above is the better headline - it is what CLAUDE.md 4.1 quotes and
# what `docs/porting.md` puts in its tree - and it has one blind spot that an experiment walked
# straight into. On 2026-09-14 a model was asked to support a second NIC on the Pi 4. It rewrote
#     #[cfg(not(any(target_arch = "arm", target_arch = "aarch64", target_arch = "riscv64")))]
# as
#     #[cfg(target_arch = "x86_64")]
# which is the SAME BRANCH spelled with fewer arch names. The name count fell 11 -> 9 and this script
# reported an improvement. Nothing about the file's portability changed: the number of places that
# decide something by instruction set was 10 before and 10 after.
#
# So a fall in the name count is not by itself evidence that an arch-conditional DECISION went away.
# Ratcheting both closes it: a genuine removal lowers both, a re-spelling lowers neither, and a
# negative cfg rewritten as a positive one can no longer bank a win it did not earn.
#
# It is a separate FILE rather than a second column because `SHARED-SURFACE.baseline.txt`'s format is
# parsed by `facts_check.porting_tree_problems` and drives the tree in `docs/porting.md`. Widening
# that line would have to be done in three places at once, and getting it wrong makes the tree
# cross-check skip lines rather than fail - the exact silent-pass shape this layer exists to prevent.
SITES_BASELINE = os.path.join(ROOT, "SHARED-SURFACE-SITES.baseline.txt")
# Both forms are a compile-time branch on the ISA and both count once, however many names they
# list: the `#[cfg(..)]` ATTRIBUTE and the `cfg!(..)` EXPRESSION (`net-stack` and the SDK's
# `ipc.rs` each use the latter, and an attribute-only scan reported them as zero).
#
# A `build.rs` matching `CARGO_CFG_TARGET_ARCH` scores zero here, and that is CORRECT rather
# than a blind spot: `docs/porting.md` names those tables as the designed place to answer, one
# question asked ONCE, and this unit is meant to reward exactly that over a scatter of cfgs.
def _cfg_site_re():
    """One compile-time ISA branch, in either spelling, counted once however many names it lists:
    the `#[cfg(..)]` attribute and the `cfg!(..)` expression."""
    return re.compile(r'#\[\s*cfg' + '|' + r'cfg!\s*\(')


CFG_SITE = _cfg_site_re()
if not CFG_SITE.search('x = cfg!(target_arch = "x86_64");'):
    # LOUD, never a silent pass. This alternation was first written with a `\b` inside a NON-RAW
    # string, which Python reads as the BACKSPACE character (0x08) - so the `cfg!` branch matched
    # nothing, every `cfg!` site counted as zero, and this script printed a clean pass over a regex
    # that was reading nothing. `print()` of the pattern looked right because a backspace is
    # invisible; only `repr()` showed it. A regex that has stopped matching is the same defect as a
    # counter nobody increments, so it is asserted against a known-matching line rather than trusted.
    raise SystemExit('shared_surface_check: CFG_SITE no longer matches a `cfg!` site - refusing to '
                     'report a count it cannot measure')

# Where shared code lives. `sdk/` is the seam and is EXPECTED to carry arch cfgs - counted so a reader
# can see the shape, but it is the one place they are the right answer rather than a smell.
# The neutral kernel is `kernel/src` MINUS `arch/`, which is the one directory allowed to know.
SHARED_ROOTS = ("services", "sdk", "kernel/src")
EXCLUDED_DIRS = ("target", ".git", "arch")
# Two spellings of the SAME question, because a build script asks it differently and the answer is
# just as arch-conditional. `CARGO_CFG_TARGET_ARCH` is the environment variable cargo sets for a
# `build.rs`, and matching on it there is how a crate maps the ISA to a board fact once instead of
# repeating a `#[cfg(any(...))]` list at every site (`services/block-driver/build.rs`).
#
# It is counted, and that is the point: concentrating seven lists into one table should read as
# 21 -> 1, not as 21 -> 0. A reduction that is really the instrument going blind is the exact failure
# this branch keeps finding elsewhere - `arch_boundary_check` could not see two arches, `stack_fit`
# censused zero frames on riscv64 and called it a pass. A ruler you can step off is not a ruler.
# `target_pointer_width` counts too. It is a BETTER axis than `target_arch` for anything that turns on
# register width (the SDK's syscall-argument clamp), and it must still be counted, or moving to the
# better question would read as the site disappearing - the third time on this branch that the ruler
# could have been stepped off by improving the code. Counted, not exempt.
ARCH_CFG = re.compile(r'target_arch\s*=\s*"([a-z0-9_]+)"|target_pointer_width\s*=\s*"\d+"'
                      r'|CARGO_CFG_TARGET_ARCH')

# In a BUILD SCRIPT the arch is read once into a variable and then compared - `match arch.as_str()`,
# `arch == "x86_64"` - so `CARGO_CFG_TARGET_ARCH` alone counts a fifty-arm table as ONE. That is the
# ruler going blind in the slower way: not missing the file, but reporting a number that cannot grow
# no matter how much arch-conditional code the file accumulates.
#
# So a build script ALSO counts each arch it names. A new ISA's real cost there is one arm per
# question the file asks, and that is what this measures. Only in `build.rs`, where an arch name in a
# string is a decision; in ordinary source it would match prose and paths.
ARCH_NAME = re.compile(r'"(x86_64|x86|aarch64|arm|riscv64|riscv32|loongarch64|s390x)"')


def rel(path):
    return os.path.relpath(path, ROOT).replace(os.sep, "/")


def _strip_comments(text):
    """Drop `//` line comments before counting.

    This counted RAW FILE TEXT, so a comment that MENTIONED `target_arch` counted as an
    arch-conditional site. Two ways that bites, and the second is the bad one:

      * prose alone could trip the ratchet and refuse a change that added no conditional code
        (this file's own fix did exactly that - a comment quoting the cfg it had just DELETED kept
        the count level and hid a genuine reduction);
      * and the reverse - deleting a conditional while describing it in a comment leaves the number
        unmoved, so the ratchet reports no progress for real progress, which is the slower poison.

    `arch_boundary_check.py`, the sibling that enforces the same boundary inside the kernel, has
    stripped comments since it was written. This is that, applied to the other side of the seam.
    Block comments and string literals are rare enough here that a line-comment strip suffices; a
    miss is a count that is too HIGH, which fails loudly rather than passing quietly.
    """
    return chr(10).join(line.split("//", 1)[0] for line in text.splitlines())


def scan_counts():
    """Occurrences of an arch cfg per shared file, as {path: count}. Comments do not count."""
    counts = {}
    for root_name in SHARED_ROOTS:
        root = os.path.join(ROOT, root_name)
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in EXCLUDED_DIRS]
            for fn in filenames:
                if not fn.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, fn)
                with open(full, encoding="utf-8", errors="replace") as fh:
                    text = _strip_comments(fh.read())
                n = len(ARCH_CFG.findall(text))
                if fn == "build.rs":
                    n += len(ARCH_NAME.findall(text))
                if n:
                    counts[rel(full)] = n
    return counts


def scan_sites():
    """`#[cfg` attributes that mention an arch, per shared file, as {path: count}.

    An attribute is counted once no matter how many arch names it lists, which is the whole point:
    this unit asks HOW MANY PLACES decide by instruction set, not how many names they spell.
    """
    counts = {}
    for root_name in SHARED_ROOTS:
        root = os.path.join(ROOT, root_name)
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in EXCLUDED_DIRS]
            for fn in filenames:
                if not fn.endswith(".rs"):
                    continue
                full = os.path.join(dirpath, fn)
                with open(full, encoding="utf-8", errors="replace") as fh:
                    text = _strip_comments(fh.read())
                n = sum(1 for line in text.splitlines()
                        if CFG_SITE.search(line) and ARCH_CFG.search(line))
                if n:
                    counts[rel(full)] = n
    return counts


def read_sites_baseline():
    if not os.path.exists(SITES_BASELINE):
        return None
    out = {}
    with open(SITES_BASELINE, encoding="utf-8") as fh:
        for line in fh:
            line = line.split("#", 1)[0].strip()
            parts = line.split(None, 1)
            if len(parts) == 2 and parts[0].isdigit():
                out[parts[1].strip()] = int(parts[0])
    return out


def write_sites_baseline(counts):
    with open(SITES_BASELINE, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("# Arch-conditional `#[cfg` ATTRIBUTES outside kernel/src/arch, frozen. The companion\n"
                 "# to SHARED-SURFACE.baseline.txt, which counts arch NAMES.\n"
                 "#\n"
                 "# This unit asks HOW MANY PLACES decide by instruction set. The name count can fall\n"
                 "# without this one moving - rewrite `not(any(arm, aarch64, riscv64))` as `x86_64` and\n"
                 "# you have spelled the same branch with two fewer names. That really happened, and\n"
                 "# this script reported it as an improvement, which is why this file exists.\n"
                 "#\n"
                 "# SHRINK ONLY, on the same terms as its companion.\n\n")
        for path in sorted(counts):
            fh.write("%4d  %s\n" % (counts[path], path))


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
        write_sites_baseline(scan_sites())
        print("shared-surface: BOTH baselines rewritten from the working tree - say why in the commit.")
        return 0

    counts = scan_counts()
    baseline = read_baseline()
    if baseline is None:
        write_baseline(counts)
        print("shared-surface: no baseline; wrote one from the working tree.")
        return 0

    sites = scan_sites()
    sites_base = read_sites_baseline()
    if sites_base is None:
        write_sites_baseline(sites)
        print("shared-surface: no SITES baseline; wrote one from the working tree.")
        sites_base = sites

    sites_grew = [(p, sites_base.get(p, 0), n) for p, n in sorted(sites.items())
                  if n > sites_base.get(p, 0)]
    if sites_grew:
        print("SHARED SURFACE GREW - more PLACES now decide by instruction set, outside `arch/`:")
        print()
        for path, was, now in sites_grew:
            print("  %s: %d -> %d arch `#[cfg` attribute(s)" % (path, was, now))
        print()
        print("This is the SITE count, not the name count: how many places branch on the ISA at all.")
        print("It is checked separately because the name count has a blind spot - rewriting")
        print("`not(any(arm, aarch64, riscv64))` as `x86_64` drops two names while leaving the same")
        print("branch exactly where it was, and this script used to call that an improvement.")
        print()
        print("If the port really needs it, run `python scripts/shared_surface_check.py --bless` and")
        print("say why in the commit.")
        return 1

    grew = [(p, baseline.get(p, 0), n) for p, n in sorted(counts.items()) if n > baseline.get(p, 0)]
    if grew:
        print("SHARED SURFACE GREW - arch-conditional code was ADDED outside `arch/`:")
        print()
        for path, was, now in grew:
            print("  %s: %d -> %d" % (path, was, now))
        print()
        if any(p.startswith("kernel/src") for p, _, _ in grew):
            print("In the NEUTRAL KERNEL the ISA usually IS the real question - and the answer is a new")
            print("`arch::imp` member, not a cfg here. A neutral file that knows the ISA is a file the")
            print("NEXT port has to edit, which is the whole of what `bounded to arch/<isa>/` means")
            print("(CLAUDE.md 4.1). `arch_boundary_check.py` will not catch this: a `#[cfg]` is neither")
            print("inline asm nor a named arch module, so it passes both of that script's rules.")
            print()
        if any(not p.startswith("kernel/src") for p, _, _ in grew):
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
    kern = sum(n for p, n in counts.items() if p.startswith("kernel/src"))
    print("  (%d in the neutral kernel, %d above it)" % (kern, total - kern))
    print("Shared-surface check passed - %d arch-conditional site(s) outside `arch/`, none added%s."
          % (total, (", %d file(s) shrank" % shrank) if shrank else ""))
    if "--report" in sys.argv:
        print()
        report_against_main()
    return 0


if __name__ == "__main__":
    sys.exit(main())
