#!/usr/bin/env python3
"""Build the Raspberry Pi 4 (aarch64) kernel image: build/kernel8.img.

WHY THIS SCRIPT EXISTS
----------------------
The aarch64 target has two board variants that write to the SAME cargo artifact
path (`target/aarch64-unknown-none/<profile>/kernel`):

  * default        -> QEMU `virt`,  linked at 0x4008_0000, PL011 at 0x0900_0000
  * --features pi4 -> Raspberry Pi 4, linked at 0x0008_0000, PL011 at 0xFE20_1000

So building one and then the other silently replaces the artifact, and an
objcopy afterwards produces an image for the wrong board. It boots to nothing:
every absolute address is wrong and the UART writes go to an address the board
does not decode. That is not hypothetical - it is exactly how the first Pi 4
image came to be a QEMU image, after a "check the virt build still compiles" step
ran between the build and the objcopy.

This script builds ONLY the pi4 variant and objcopies immediately, so the two
cannot be interleaved. It then VERIFIES the link address rather than trusting it,
because the failure mode is silent.

Extra cargo features may be appended with --features (comma separated). They are
added ALONGSIDE `pi4`, never instead of it, so no invocation of this script can
accidentally produce a non-Pi image:

    python scripts/pi4_build.py --features pi4-sched-demo

usage:  python scripts/pi4_build.py [--debug] [--features a,b]
"""
import subprocess, sys, os, pathlib, re

ROOT = pathlib.Path(__file__).resolve().parents[1]
PROFILE = "debug" if "--debug" in sys.argv else "release"
TARGET = "aarch64-unknown-none"
# The kernel is LINKED high (TTBR1) and LOADED low. objcopy -O binary emits by LMA,
# so the flat image still starts at the Pi's 0x80000 flat-load address, while every
# symbol is a high virtual address. Both are checked: the LMA because a wrong one
# produces an image the firmware drops at the wrong place, and the VMA because it is
# what distinguishes the Pi 4 build from the QEMU `virt` build (which is linked at
# 0x4008_0000 and would otherwise pass an LMA-only check by accident).
EXPECT_LOAD = 0x80000               # LMA: where the firmware puts the flat binary
EXPECT_VMA  = 0xFFFFFF8000080000    # VMA: KERNEL_VA + 0x80000, see kernel-aarch64-pi4.ld

def run(cmd, **kw):
    print(">", " ".join(str(c) for c in cmd))
    r = subprocess.run(cmd, cwd=ROOT, **kw)
    if r.returncode != 0:
        sys.exit(f"FAILED: {' '.join(str(c) for c in cmd)}")
    return r

def tool(name):
    """Prefer the rustup-provided llvm tools; fall back to PATH."""
    cargo_bin = pathlib.Path.home() / ".cargo" / "bin" / f"{name}.exe"
    return str(cargo_bin) if cargo_bin.exists() else name

# The services embedded in the Pi 4 image. The kernel's `build.rs` picks up each one from
# `target/aarch64-unknown-none/<profile>/` if it is in the matching list there, and falls back to the
# empty placeholder otherwise (which is what a `LoadFailed(TooSmall)` at boot means: not ported yet).
# Building them HERE rather than by hand is what keeps the two lists honest - a service added to
# `build.rs` but never built silently stays a placeholder, and the boot log says so in a line that is
# easy to read past.
PI4_SERVICES = [
    "logger", "supervisor", "shell",
    "ping", "pong",
    # The chaos service is what the carnage gate runs; `observe` is how the machine is watched while it
    # runs. Both are arch-neutral - they needed building, not porting.
    "chaos", "observe", "mem-pressure",
    # Storage: the USB stick, never the SD card (which is the boot medium - see block-driver's
    # `backend_run`). `fs` is arch-neutral and rides on whatever block-driver serves.
    "block-driver", "fs",
]

# `pi4` is always present; anything passed is added to it, not substituted for it. `pi4-smp` rides with
# it now that all four cores come up on every boot and survive a carnage run - a Pi 4 running on one of
# its four cores is not the machine.
FEATURES = "pi4,pi4-smp"
if "--features" in sys.argv:
    i = sys.argv.index("--features")
    if i + 1 >= len(sys.argv):
        sys.exit("--features needs a comma-separated list")
    FEATURES = "pi4,pi4-smp," + sys.argv[i + 1]

rel = ["--release"] if PROFILE == "release" else []

# 1. The services first - the kernel embeds their ELFs, so a stale service is baked into the image.
for svc in PI4_SERVICES:
    feats = ["--features", "bare-metal"] if svc == "supervisor" else []
    run(["cargo", "build", "-p", svc, "--target", TARGET] + feats + rel)

# 2. The kernel.
args = ["cargo", "build", "-p", "kernel", "--target", TARGET, "--features", FEATURES] + rel
run(args)

elf = ROOT / "target" / TARGET / PROFILE / "kernel"
img = ROOT / "build" / "kernel8.img"
img.parent.mkdir(exist_ok=True)

# Verify the ELF really is the Pi 4 variant BEFORE producing an image from it.
# A wrong-board image is indistinguishable from a code bug once it is on a card.
out = subprocess.run([tool("rust-objdump"), "-h", str(elf)],
                     cwd=ROOT, capture_output=True, text=True).stdout
m = re.search(r"\.text\s+[0-9a-f]+\s+([0-9a-f]{16})\s+([0-9a-f]{16})", out)
if not m:
    sys.exit("could not read the .text VMA/LMA from the ELF - refusing to emit an image")
vma, lma = int(m.group(1), 16), int(m.group(2), 16)
if lma != EXPECT_LOAD or vma != EXPECT_VMA:
    sys.exit(
        f"WRONG BOARD: .text VMA={vma:#x} LMA={lma:#x}, "
        f"expected VMA={EXPECT_VMA:#x} LMA={EXPECT_LOAD:#x}.\n"
        f"A low VMA means the QEMU `virt` build overwrote the artifact; a wrong LMA means\n"
        f"the firmware would drop the image somewhere it cannot run. Re-run this script alone."
    )
addr = lma

run([tool("rust-objcopy"), "-O", "binary", str(elf), str(img)])
print(f"OK  {img.relative_to(ROOT)}  ({img.stat().st_size} bytes, "
      f".text VMA {vma:#x} / LMA {addr:#x}, feature={FEATURES}, profile={PROFILE})")
print("Deploy: copy build/kernel8.img to the SD card as the name config.txt's `kernel=` line points at.")
