#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""ONE way to build a bootable image, for any of the four ports.

    py scripts/board.py pi2 | pi4 | visionfive | x86  [--crash-window]

WHY THIS EXISTS. Four ports had four build interfaces, and the differences were not decorative -
they cost two flashes and a bisect on 2026-09-22:

    port        flags style   release          board flag     crash-window   artifact
    x86         osdev CLI     implicit         -              no             build/os-usb.img
    arm32       argparse      --release        -              added that day kernel7.img + config-pi2.txt
    aarch64     sys.argv      DEFAULT ON       -              no             kernel8.img, deployed AS godspeed8.img
    riscv64     sys.argv      --release        --visionfive   added that day godspeed-riscv64-visionfive.img

Three argument styles, four artifact schemes, and `--release` optional on two ports but default on a
third. A VisionFive image built without it embedded a placeholder supervisor, then - once that was
fixed - a DEBUG kernel whose oversized stack frames faulted the board half a second after `xhci`
went interrupt-driven. Both were the build command, not the code.

WHAT THIS FIXES, as rules rather than habits:

  1. A BOARD IMAGE IS ALWAYS RELEASE. There is no flag to ask for debug, because nobody wants a
     debug board image and two ports have now proven what one does.
  2. EVERY PORT TAKES THE SAME FLAGS. `--crash-window` works on all four.
  3. EVERY BUILD ENDS THE SAME WAY: the artifact, its size, and exactly what to copy where -
     including the renames, which are per-port and are what a person gets wrong.
  4. THE ARTIFACT MUST BE NEWER THAN THIS RUN. A build that silently leaves yesterday's image is
     the stale-image trap that has produced false hardware results on this project before.

The per-port scripts stay, and stay callable - this owns the POLICY, not the work.
"""
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Per port: the command, the artifacts it must produce, and what a person does with them. The deploy
# text lives HERE, next to the build, because the renames differ per port and that is precisely what
# gets mistyped - the Pi 2 wants `config-pi2.txt` renamed to `config.txt`, and a card carrying our
# kernel with the Imager's config sits on a rainbow screen saying nothing.
BOARDS = {
    "pi2": {
        "desc": "Raspberry Pi 2 (ARMv7)",
        "cmd": ["arm_build.py", "--release"],
        "artifacts": ["build/kernel7.img", "build/config-pi2.txt"],
        "deploy": [
            "copy build/kernel7.img     -> <card>/kernel7.img",
            "copy build/config-pi2.txt  -> <card>/config.txt   (RENAME; replace the Imager's)",
            "the card also needs the Pi firmware: bootcode.bin, start.elf, fixup.dat",
        ],
    },
    "pi4": {
        "desc": "Raspberry Pi 4 (AArch64)",
        "cmd": ["pi4_build.py"],                      # release is this port's default
        "artifacts": ["build/kernel8.img"],
        "deploy": [
            "copy build/kernel8.img     -> <card>/godspeed8.img   (RENAME)",
            "the card also needs the Pi firmware and a config.txt naming godspeed8.img",
        ],
    },
    "visionfive": {
        "desc": "StarFive VisionFive 2 Lite (RISC-V 64)",
        "cmd": ["riscv_build.py", "--visionfive", "--release"],
        "artifacts": ["build/godspeed-riscv64-visionfive.img"],
        "deploy": [
            "run scripts/deploy_visionfive.ps1 (elevated), or copy the .img to the card's",
            "ESP and point /extlinux/extlinux.conf at it - backlog/14 has the stanza",
        ],
    },
    "x86": {
        "desc": "x86-64 (HP T630, Dell Wyse)",
        "cmd": None,                                   # osdev, not a script - see run_x86
        # `os-usb.img`, NOT `os.img`. Both exist and they are DIFFERENT files: `osdev image` writes
        # the bare-metal UEFI image as `os-usb.img`, while `build/os.img` is what the QEMU suites
        # stage for themselves. Naming the wrong one here pointed at a file that was 56 minutes
        # stale on the first x86 build through this script - caught by the freshness check below,
        # which is the trap `feedback_stale_image_trap` records costing a false "verified in QEMU".
        "artifacts": ["build/os-usb.img"],
        "deploy": ["write build/os-usb.img to a USB stick (Rufus DD mode, or dd), boot it UEFI"],
    },
}


def fail(msg):
    print("board: " + msg)
    sys.exit(1)


def main():
    argv = sys.argv[1:]
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__)
        print("boards:")
        for name, b in BOARDS.items():
            print("  %-11s %s" % (name, b["desc"]))
        return 0
    board = argv[0]
    if board not in BOARDS:
        fail("unknown board %r. One of: %s" % (board, ", ".join(BOARDS)))
    extra = argv[1:]
    for a in extra:
        if a not in ("--crash-window",):
            fail("unknown flag %r. The only flag is --crash-window.\n"
                 "        A board image is ALWAYS release - there is deliberately no way to ask\n"
                 "        for debug, because a debug kernel has faulted a board." % a)

    b = BOARDS[board]
    started = time.time()
    print("board: building %s (%s)%s" % (board, b["desc"],
                                         " [CRASH-WINDOW]" if "--crash-window" in extra else ""))

    if board == "x86":
        # `osdev` owns the x86 path and already defaults to release. The crash-window build is a
        # feature of the `fs` service there, selected through osdev's own feature string.
        if "--crash-window" in extra:
            fail("x86 crash-window images are built by the suites that use them\n"
                 "        (`osdev test fs-window` stages one). Cutting power to a PC under test is\n"
                 "        not the workflow this flag serves; the other three ports are.")
        cmd = ["cargo", "run", "-q", "-p", "osdev", "--release", "--", "image"]
    else:
        cmd = [sys.executable, os.path.join(ROOT, "scripts", b["cmd"][0])] + b["cmd"][1:] + extra

    rc = subprocess.call(cmd, cwd=ROOT)
    if rc != 0:
        fail("the %s build FAILED (exit %d). Nothing was flashed; fix the build first." % (board, rc))

    # THE ARTIFACT MUST BE NEWER THAN THIS RUN. A build that exits 0 having left the previous image
    # in place is how a "verified" hardware result tests code nobody changed.
    print("")
    print("board: %s" % b["desc"])
    for rel in b["artifacts"]:
        p = os.path.join(ROOT, rel)
        if not os.path.exists(p):
            fail("%s was not produced. The build reported success and the artifact is missing." % rel)
        if os.path.getmtime(p) < started:
            fail("%s is OLDER than this build - it was not regenerated.\n"
                 "        Flashing it would test the previous image." % rel)
        print("  %-42s %9d bytes" % (rel, os.path.getsize(p)))
    print("")
    print("  deploy:")
    for line in b["deploy"]:
        print("    " + line)
    if "--crash-window" in extra:
        print("")
        print("  *** CRASH-WINDOW IMAGE - NOT A NORMAL ONE ***")
        print("    `write /cutme.txt hello` holds the commit window open for 10 s. Pull the power")
        print("    inside it, and the next mount must report `journal recovered`.")
        print("    Reflash a normal image afterwards.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
