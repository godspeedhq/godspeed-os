#!/usr/bin/env python3
"""Build the riscv64 kernel, gated by the same checks every other build path runs.

WHY THIS EXISTS AT ALL. `kernel/src/arch/riscv64/` has been in the tree for a while and had ROTTED:
35 compile errors, every one a seam member the neutral kernel grew after the stub was written. The
boundary was doing its job - a new `arch::imp` member is a compile error for every arch at once - but
nothing ever COMPILED this target, so the errors were latent instead of loud.

That is the same failure as a test outside the default path, and it has bitten this project before in
both directions: the ARM build ran no checkers for an entire port, and the x86 build still runs no
arch-boundary check. A rule enforced on one build path is enforced on none. So this script exists
before any real RISC-V work does, and it runs the checkers rather than only the compiler.

Usage:
    py scripts/riscv_build.py [--release] [--features F]
"""

import os, subprocess, sys

ROOT   = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "riscv64imac-unknown-none-elf"


def run(cmd, **kw):
    print(">", " ".join(cmd))
    r = subprocess.run(cmd, cwd=ROOT, **kw)
    if r.returncode != 0:
        sys.exit(r.returncode)


def gates():
    """The same set `arm_build.py` runs. Listed explicitly, not discovered, so ADDING a checker is a
    decision each build path makes rather than something that silently changes what a build enforces."""
    for check in ("commandments.py", "dash_check.py", "unsafe_check.py",
                  "arch_boundary_check.py", "arch_seam_check.py", "contract_check.py"):
        r = subprocess.run([sys.executable, os.path.join("scripts", check)],
                           cwd=ROOT, capture_output=True, text=True)
        if r.returncode != 0:
            print(r.stdout + r.stderr)
            sys.exit("BUILD REFUSED: %s failed. Fix the violation, or amend CLAUDE.md and cite\n"
                     "the amendment - those are the only two ways past this, by design." % check)
    print("commandments + dash + unsafe + arch-boundary + arch-seam + contracts: pass")


def main():
    rel  = ["--release"] if "--release" in sys.argv else []
    prof = "release" if rel else "debug"
    feats = []
    if "--features" in sys.argv:
        i = sys.argv.index("--features")
        if i + 1 >= len(sys.argv):
            sys.exit("--features needs a comma-separated list")
        # ADDITIVE by convention (see arm_build.py): anything passed rides WITH the defaults rather
        # than replacing them, because a build with the boot path silently swapped out compiles fine
        # and boots into nothing.
        feats = ["--features", sys.argv[i + 1]]

    gates()
    run(["cargo", "build", "-p", "kernel", "--target", TARGET] + feats + rel)

    elf = os.path.join(ROOT, "target", TARGET, prof, "kernel")
    if not os.path.exists(elf):
        sys.exit("kernel ELF not found at %s" % elf)
    print("OK  %s  (%d bytes, target=%s, profile=%s)" % (elf, os.path.getsize(elf), TARGET, prof))
    print("Boot in QEMU:  py scripts/riscv_run.py%s" % (" --release" if rel else ""))


if __name__ == "__main__":
    main()
