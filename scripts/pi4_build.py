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

usage:  python scripts/pi4_build.py [--debug]
"""
import subprocess, sys, os, pathlib, re

ROOT = pathlib.Path(__file__).resolve().parents[1]
PROFILE = "debug" if "--debug" in sys.argv else "release"
TARGET = "aarch64-unknown-none"
EXPECT_LOAD = 0x80000  # the Pi's 64-bit flat-load address (NOT the 32-bit port's 0x8000)

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

args = ["cargo", "build", "-p", "kernel", "--target", TARGET, "--features", "pi4"]
if PROFILE == "release":
    args.append("--release")
run(args)

elf = ROOT / "target" / TARGET / PROFILE / "kernel"
img = ROOT / "build" / "kernel8.img"
img.parent.mkdir(exist_ok=True)

# Verify the ELF really is the Pi 4 variant BEFORE producing an image from it.
# A wrong-board image is indistinguishable from a code bug once it is on a card.
out = subprocess.run([tool("rust-objdump"), "-h", str(elf)],
                     cwd=ROOT, capture_output=True, text=True).stdout
m = re.search(r"\.text\s+[0-9a-f]+\s+([0-9a-f]{8,16})", out)
if not m:
    sys.exit("could not read the .text address from the ELF - refusing to emit an image")
addr = int(m.group(1), 16)
if addr != EXPECT_LOAD:
    sys.exit(
        f"WRONG BOARD: .text is linked at {addr:#x}, expected {EXPECT_LOAD:#x}.\n"
        f"That is the QEMU `virt` layout, not the Pi 4's. The artifact was probably\n"
        f"overwritten by a non-pi4 build. Re-run this script alone."
    )

run([tool("rust-objcopy"), "-O", "binary", str(elf), str(img)])
print(f"OK  {img.relative_to(ROOT)}  ({img.stat().st_size} bytes, "
      f".text @ {addr:#x}, feature=pi4, profile={PROFILE})")
print("Deploy: copy build/kernel8.img to the SD card as the name config.txt's `kernel=` line points at.")
