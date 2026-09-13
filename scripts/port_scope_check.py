#!/usr/bin/env python3
"""THE PORT SCOPE CHECK: a new ISA writes `kernel/src/arch/<isa>/` and eleven named files. Anything
else it edits is a FINDING, and this is what notices.

`docs/porting.md` states the rule plainly - "If you find yourself editing anything else, stop and ask
why" - and draws a tree marking every file write-it, add-to-it, or do-not-touch. Until 2026-09-13 that
tree was advisory: nothing could see an edit to a do-not-touch file at all.

WHY THE EXISTING CHECKERS CANNOT SEE IT, which is the whole reason this file exists:

  - `arch_boundary_check.py` asks "did you BREAK a rule in a neutral file" - inline asm, a named arch
    module, a 64-bit `core` atomic. An ordinary edit to `kernel/src/ipc/routing.rs` breaks none of
    them and passes.
  - `shared_surface_check.py` counts arch-conditional SITES. An edit that adds no `#[cfg(target_arch)]`
    adds no site, so the count does not move and it passes.
  - `arch_seam_check.py` asks the opposite direction: does your arch answer every seam member.
  - `scaffold_check.py` builds a fresh ISA and reports how far it got. A port that got there by
    editing the neutral kernel gets exactly the same green M1.

So five checkers ran green over a tree in which a contributor had edited neutral kernel files, and
that is not hypothetical: it is what happened when a deliberately weak model was set to port riscv32
on 2026-09-13 as an experiment. Its neutral-kernel edits happened to be CORRECT - it was fixing eight
real violations of a documented rule - which is the luckiest possible version of this failure and
still leaves the finding: nothing was watching. The complement to that experiment's other lesson (a
documented rule with no checker, now `arch_boundary_check` rule 3) is this one: a documented SCOPE
with no checker.

WHAT IT DOES. It finds the merge base with main, works out whether this branch adds an ISA, and if it
does, classifies every changed path against the guide's tree. In scope: the new `arch/<isa>/`
directory, its linker script, and the eleven files `docs/porting.md` marks `+`. Out of scope:
everything else, reported with the guide's own reason where it has one.

IT IS SILENT ON EVERY OTHER BRANCH, deliberately. A branch that adds no arch directory is not a port,
and "do not edit the neutral kernel" is not a rule about ordinary work - it is a rule about ports. A
check that fired on every commit would be turned off within a week.

THE ESCAPE HATCH IS A SENTENCE, NOT A FLAG. `docs/porting.md` says an unavoidable edit should be made
and its reason written down. So a commit on the branch may carry a trailer:

    Port-Scope: kernel/src/ipc/routing.rs - RV32 cannot compile the 64-bit atomic here; backlog/NN

and that path stops being a violation. The point was never to forbid the edit - it is that the edit
must be a decision somebody wrote down, rather than a thing that slid through five green checkers.

THE ALLOWED SET IS TIED TO THE GUIDE IN BOTH DIRECTIONS (`_guide_problems`), because a list of paths
in a script and a tree in a document are exactly the pair that drifts apart. Neither can change
without the other.

Exit: 0 if in scope (or not a port branch), 1 otherwise.
"""

import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent
GUIDE = "docs/porting.md"
ARCH_DIR = REPO_ROOT / "kernel" / "src" / "arch"

# The five wiring files. `docs/porting.md`, "The seam: what you write" - a line or a block each, and
# none of them is a neutral kernel file. The linker script is a pattern rather than a path, so it is
# matched by LINKER_SCRIPT below and carried here only so the guide cross-check can see it.
WIRING = {
    "kernel/src/arch/mod.rs": "two cfg lines: `pub mod <isa>;` and `pub use <isa> as imp;`",
    "kernel/kernel-<isa>.ld": "a new linker script for your load address and PHDRS",
    "kernel/build.rs": "one target-matching block, passing -T for that script",
    ".cargo/config.toml": "one `[target.<triple>]` block",
    "rust-toolchain.toml": "your triple, if a shipping build will need it",
}

# The six files ABOVE the kernel that the guide's tree marks `+`. Every one of these is a place the
# guide says to add ONE ARM; none is an invitation to restructure.
SEAM_ABOVE = {
    "sdk/rust/src/syscall.rs": "your `raw_syscall` body: the trap instruction and its register convention",
    "services/supervisor/build.rs": "one arm per table (it sets has_xhci / has_dwc2 / has_hw_enumerator)",
    "services/supervisor/src/main.rs": "one arm IF your storage or NIC sits behind a USB host",
    "services/block-driver/build.rs": "one arm: is the disk on USB, and which service owns the host",
    "services/nic-driver/src/main.rs": "one arm, and the guide says this is the worst one (backlog/21)",
    "services/hw-enumerator/src/main.rs": "one arm IF you have PCI (backlog/25)",
}

ALLOWED = dict(WIRING, **SEAM_ABOVE)

LINKER_SCRIPT = re.compile(r"^kernel/kernel-[a-z0-9_]+\.ld$")

# A port RECORDS things. Writing down what booted, what did not, and what you had to leave open is
# the opposite of the failure this checks for, so none of it is a finding. `SHARED-SURFACE.baseline.txt`
# is here because it is the ratchet's own file: raising it is governed by `shared_surface_check.py`,
# which is a stricter gate than this one and already refuses a rise without a reason.
FREE_PREFIXES = ("docs/", "backlog/", "milestones/", "boot/", "bugs/", "website/")
FREE_FILES = ("README.md", "SHARED-SURFACE.baseline.txt", "CLAUDE.md")

# Files the guide marks `-` with a REASON. Repeating the reason at the moment of the violation is the
# difference between a checker that teaches and one that just says no.
DOCUMENTED_NO = {
    "kernel/src/task/scheduler.rs":
        "the guide says you touch nothing here whether you are 32- or 64-bit: its 2 sites key on "
        "`target_pointer_width`, and the existing arm already answers the 32-bit case",
    "sdk/rust/src/adversarial.rs":
        "not needed to boot - section 22 fault primitives, and backlog/24 records that none of them "
        "runs off x86 anyway",
    "sdk/rust/src/ipc.rs":
        "its one site keys on register width, not on your ISA; if you are 32-bit it already covers you",
    "services/shell/build.rs":
        "derived from CARGO_CFG_TARGET_ARCH - add a line only if your ISA needs a project-specific "
        "name, as arm32 does",
    "services/net-stack/src/main.rs":
        "the default is the wall-clock floor, which every non-x86 port has turned out to need",
}

WAIVER = re.compile(r"^\s*Port-Scope:\s*([^\s]+)\s*-\s*(.+?)\s*$", re.M)


def git(*args):
    return subprocess.run(["git", "-C", str(REPO_ROOT)] + list(args),
                          capture_output=True, text=True)


def _guide_problems():
    """Tie the ALLOWED set to the guide's tree in BOTH directions.

    A list of paths in a script and a tree in a document say the same thing twice, which is the shape
    that rots (the same argument `facts_check.porting_tree_problems` makes about the tree's counts).
    Two assertions, and between them neither side can move alone:

      1. every path this script allows is named somewhere in the guide;
      2. the guide's tree marks exactly as many rows `+` as this script allows, and every one of those
         rows names a file this script allows.

    The row count is what catches a REMOVAL. Matching leaf-by-leaf would not: the tree writes
    `build.rs` three times at three different depths, so a suffix match is ambiguous by construction
    and only the count can tell you a row went missing.
    """
    problems = []
    text = (REPO_ROOT / GUIDE).read_text(encoding="utf-8", errors="replace")

    for path in sorted(ALLOWED):
        if path not in text:
            problems.append(f"{GUIDE} does not mention `{path}`, which this script allows a port to "
                            f"edit. Either the guide lost it or this script invented it.")

    # Tree rows: box art, a name, an optional `[ N ]` count column, then the legend marker.
    rows = re.findall(r"^[│├└─\s]*([A-Za-z0-9_.<>/-]+)\s+(?:\[\s*\d+\s*\]\s+)?\+\s",
                      text, re.M)
    if len(rows) != len(ALLOWED):
        problems.append(f"{GUIDE}'s tree marks {len(rows)} row(s) `+` but this script allows "
                        f"{len(ALLOWED)} path(s). One of them changed without the other: "
                        f"rows={sorted(rows)}")
    for leaf in rows:
        if not any(p.endswith(leaf) for p in ALLOWED):
            problems.append(f"{GUIDE}'s tree marks `{leaf}` as add-to-it, but this script does not "
                            f"allow any path ending in it - a porter would be told yes and then refused")
    return problems


def _base_ref(explicit):
    """The commit this branch grew from. Without one there is nothing to diff and nothing to say."""
    if explicit:
        r = git("rev-parse", "--verify", explicit)
        if r.returncode != 0:
            raise SystemExit(f"port_scope_check: --base {explicit} is not a ref in this repository")
        return explicit
    for cand in ("origin/main", "main"):
        if git("rev-parse", "--verify", cand).returncode == 0:
            mb = git("merge-base", cand, "HEAD")
            if mb.returncode == 0 and mb.stdout.strip():
                return mb.stdout.strip()
    return None


def _changed(base):
    """Every path this branch touches: committed, staged, unstaged, and untracked.

    Untracked matters more here than anywhere else - a brand new `arch/<isa>/` starts life untracked,
    and a checker that only saw commits would tell a porter mid-work that they were not porting.
    """
    paths = set()
    d = git("diff", "--name-only", base)
    if d.returncode == 0:
        paths.update(p for p in d.stdout.split("\n") if p.strip())
    st = git("status", "--porcelain", "--untracked-files=all")
    if st.returncode == 0:
        for line in st.stdout.split("\n"):
            if line[:2].strip() and len(line) > 3:
                paths.add(line[3:].strip().strip('"'))
    return {p.replace("\\", "/") for p in paths if p}


def _new_arches(base):
    """ISA directories that exist now and did not exist at the base. This IS the port."""
    at_base = set()
    r = git("ls-tree", "--name-only", "-d", base, "kernel/src/arch/")
    if r.returncode == 0:
        at_base = {line.rstrip("/").split("/")[-1] for line in r.stdout.split("\n") if line.strip()}
    now = {p.name for p in ARCH_DIR.iterdir() if p.is_dir() and not p.name.startswith(".")}
    return sorted(now - at_base)


def _waivers(base):
    r = git("log", "--format=%B", f"{base}..HEAD")
    if r.returncode != 0:
        return {}
    return {m.group(1).replace("\\", "/"): m.group(2) for m in WAIVER.finditer(r.stdout)}


def main():
    argv = sys.argv[1:]
    force = "--force" in argv
    base = None
    if "--base" in argv:
        base = argv[argv.index("--base") + 1]

    problems = _guide_problems()
    if problems:
        print("Port-scope check - FAILURES (this script and docs/porting.md disagree about the scope):")
        for p in problems:
            print(f"  {p}")
        print()
        print("The allowed set is stated twice on purpose - here and in the guide a porter reads - so "
              "neither can move alone. Fix whichever one is wrong.")
        return 1

    base = _base_ref(base)
    if base is None:
        print("Port-scope check NOT RUN: no merge base against main could be found (a shallow clone, "
              "or a repository with no main). Pass --base <ref> to check anyway. Reporting this "
              "rather than passing quietly, because a pass over an unchecked tree is the failure "
              "this script exists to catch.")
        return 1

    new = _new_arches(base)
    if not new and not force:
        print(f"Port-scope check: not a port branch (no new kernel/src/arch/<isa>/ since "
              f"{base[:12]}), so the scope rule does not apply. It governs ADDING an ISA, not "
              f"ordinary work in the kernel.")
        return 0

    changed = _changed(base)
    waived = _waivers(base)
    in_scope_dirs = tuple(f"kernel/src/arch/{a}/" for a in new)

    violations = []
    for path in sorted(changed):
        if path.startswith(in_scope_dirs):
            continue
        if path in ALLOWED or LINKER_SCRIPT.match(path):
            continue
        if path.startswith(FREE_PREFIXES) or path in FREE_FILES:
            continue
        if path in waived:
            continue
        why, source = DOCUMENTED_NO.get(path), "docs/porting.md marks this do-not-touch"
        if why is None and path.startswith("scripts/"):
            # Not a row in the guide's tree: the guide never contemplates a port editing a checker.
            # Attributing this to it would be a checker misquoting its own source document.
            why, source = ("a port that has to edit a checker in order to pass is the one case where "
                           "the finding is about the CHECKER - say which and why, do not just widen it"),                          "the enforcement layer"
        violations.append((path, why, source))

    isa = ", ".join(new) if new else "(forced)"
    if violations:
        print(f"Port-scope check - FAILURES (porting {isa}, base {base[:12]}):")
        print()
        for path, why, source in violations:
            print(f"  {path}")
            if why:
                print(f"      {source}: {why}")
        print()
        print(f"{len(violations)} file(s) outside the port scope. docs/porting.md: write "
              f"`kernel/src/arch/<isa>/`, touch the five wiring files and the six the tree marks `+`. "
              f"Anything else is a place an earlier port left an assumption behind, and the right "
              f"move is to fix the assumption rather than add your ISA to a list.")
        print()
        print("If the edit is genuinely unavoidable, make it and write down why - a commit on this "
              "branch carrying")
        print()
        print("    Port-Scope: <path> - <reason>")
        print()
        print("clears that path. The rule was never that the edit is forbidden; it is that it has to "
              "be a decision somebody recorded.")
        return 1

    extra = f", {len(waived)} recorded by a Port-Scope trailer" if waived else ""
    print(f"Port-scope check passed - porting {isa} touched {len(changed)} path(s), all inside "
          f"kernel/src/arch/<isa>/, the five wiring files, or the six docs/porting.md marks `+`"
          f"{extra}. The neutral kernel and every other service are untouched, which is what makes "
          f"the port bounded.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
