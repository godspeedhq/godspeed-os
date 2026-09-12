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

Two machines, one arch. QEMU `virt` enters the kernel at 0x8020_0000; the StarFive VisionFive 2 Lite
enters at 0x4020_0000. `--visionfive` selects the board linker script and emits a FLAT BINARY, which
is what U-Boot's `booti` loads - an ELF is fine for QEMU's `-kernel` and useless to U-Boot.

USERSPACE IS BUILT HERE TOO, and in the right order. The kernel embeds ONE image - the supervisor -
and the supervisor embeds every service it spawns, so a supervisor built before the services it
carries embeds STALE ones. That is not a hypothetical: it shipped on both Pi ports and made several
"confirmations" tests of code that was not running. `embed_order_check.py` is the guard, and it only
guards a path that runs it.

RELEASE, for userspace, always. A debug service carries frames many times larger than its user stack
- the arm32 port found this as `fs` crash-looping on a 503 KiB `service_main` frame against a 256 KiB
stack - and a debug supervisor embedding debug services is 40 MB of `.rodata`. `--release` is
therefore forced for the userspace half regardless of what the kernel half is built as.

Usage:
    py scripts/riscv_build.py [--release] [--features F]     QEMU `virt` (ELF)
    py scripts/riscv_build.py --release --visionfive         the board  (flat .img)
    py scripts/riscv_build.py --release --no-userspace       kernel only (placeholder supervisor)
"""

import os, shutil, subprocess, sys

ROOT   = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "riscv64imac-unknown-none-elf"


# Every service the SUPERVISOR embeds, which is what its own `build.rs` demands be on disk before it
# will link. Not a subset: the supervisor's list is the source of truth, and a missing entry there is
# a hard panic rather than a placeholder, so this list either matches it or the build stops.
#
# `probe` is absent deliberately, and for the same reason it is absent on ARM: it is the adversarial
# test service, and the SDK's fault primitives it calls (`fault_noncanonical_read`,
# `fault_divide_by_zero`) are x86 instructions. The supervisor is therefore built with `bare-metal`,
# which is the feature that drops `probe` from its embedded set - the same posture a real board takes
# anyway, since a bare-metal image ships no adversary (§4.4).
SERVICES = [
    "events", "recorder", "console", "shell", "chaos", "observe", "mem-pressure", "time", "control",
    # `hw-enumerator` joins the build now that this arch answers `pci_cfg_read32`. It was absent
    # because the seam member returned None unconditionally, so the service had nothing to enumerate
    # WITH - not because the board has no PCI (QEMU `virt` has a generic host bridge and this port
    # walks it). Its absence was the last thing keeping PCI semantics in ring 0 on this arch.
    "hw-enumerator",
    "ping", "pong", "greet", "upper", "roster", "counter", "reply-server", "asker",
    "resource-server", "holder", "block-driver", "fs", "nic-driver", "net-stack",
    # `xhci` was missing, and that is the whole of "supervisor: spawn xhci FAILED" - the supervisor
    # holds every service's image and cannot spawn one it was never given. The kernel had found the
    # controller, resolved its window and offered it; the driver simply was not in the build.
    "xhci",
]


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
    board = "--visionfive" in sys.argv
    feats = []
    want = ["visionfive"] if board else []
    if "--features" in sys.argv:
        i = sys.argv.index("--features")
        if i + 1 >= len(sys.argv):
            sys.exit("--features needs a comma-separated list")
        # ADDITIVE by convention (see arm_build.py): anything passed rides WITH the defaults rather
        # than replacing them, because a build with the boot path silently swapped out compiles fine
        # and boots into nothing.
        want += [f for f in sys.argv[i + 1].split(",") if f]
    if want:
        feats = ["--features", ",".join(want)]

    gates()

    # Userspace FIRST, and in dependency order: every service, then the supervisor that embeds them,
    # then the kernel that embeds the supervisor. Building the other way round embeds whatever was
    # last on disk, which is the stale-image trap this comment exists to prevent.
    if "--no-userspace" not in sys.argv:
        for svc in SERVICES:
            run(["cargo", "build", "-p", svc, "--target", TARGET, "--release"])
        run(["cargo", "build", "-p", "supervisor", "--target", TARGET, "--release",
             "--features", "bare-metal"])
        sup = os.path.join(ROOT, "target", TARGET, "release", "supervisor")
        if not os.path.exists(sup):
            sys.exit("supervisor did not build; the kernel would silently embed a placeholder")
        print("OK  userspace: %d services + supervisor (%d bytes)" % (len(SERVICES), os.path.getsize(sup)))

    run(["cargo", "build", "-p", "kernel", "--target", TARGET] + feats + rel)

    elf = os.path.join(ROOT, "target", TARGET, prof, "kernel")
    if not os.path.exists(elf):
        sys.exit("kernel ELF not found at %s" % elf)
    # A kernel that embedded the PLACEHOLDER instead of the supervisor links, boots, and fails at
    # `spawn_supervisor` with `LoadFailed(TooSmall)` - which reads like a corrupt binary rather than a
    # build-order mistake. The size difference is unmistakable, so it is stated here rather than
    # discovered there.
    ksize = os.path.getsize(elf)
    print("OK  %s  (%d bytes, target=%s, profile=%s%s)"
          % (elf, ksize, TARGET, prof, ", board=visionfive" if board else ""))
    if "--no-userspace" not in sys.argv and ksize < 1_000_000:
        print("WARNING: the kernel is small enough that it probably embedded the PLACEHOLDER")
        print("         supervisor rather than the real one. Check kernel/build.rs `riscv64_built`.")

    if not board:
        print("Boot in QEMU:  py scripts/riscv_run.py%s" % (" --release" if rel else ""))
        return

    # FLAT BINARY, because U-Boot's `booti` loads an image, not an ELF. Same step the ARM ports take
    # (`rust-objcopy -O binary`); an ELF works for QEMU's `-kernel` only because QEMU parses it.
    objcopy = shutil.which("rust-objcopy") or "rust-objcopy"
    out = os.path.join(ROOT, "build", "godspeed-riscv64-visionfive.img")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    run([objcopy, "-O", "binary", elf, out])
    size = os.path.getsize(out)
    print("OK  %s  (%d bytes, flat, load at 0x40200000)" % (out, size))
    print("")
    print("Deploy: copy it to the card's FAT partition (partition 3, the ESP) and add a label to")
    print("        /extlinux/extlinux.conf pointing at it - see backlog/14 for the exact stanza.")
    print("        Or, faster for first light, load and `booti` it from the U-Boot prompt.")


if __name__ == "__main__":
    main()
