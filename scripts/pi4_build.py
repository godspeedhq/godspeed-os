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

`--xhci-userspace` is separate from `--features` because it has to reach TWO
crates: the kernel (stop driving the VL805) and the `supervisor` (start spawning
the `xhci` service that drives it instead). See where it is handled below.

usage:  python scripts/pi4_build.py [--debug] [--features a,b] [--xhci-userspace]
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
    # Networking. Both are arch-neutral: `nic-driver` reaches the hardware through the NET_DEVICE
    # syscalls, which the aarch64 arch layer now backs with the GENET driver, and `net-stack` sits on
    # top of nic-driver and never touches hardware at all. Neither needed porting - they needed
    # BUILDING, and until they were on this list the kernel embedded an empty placeholder and every
    # boot reported `LoadFailed(TooSmall)`.
    "nic-driver", "net-stack",
    # The userspace USB host-controller driver for the VL805. Built on EVERY invocation, not only
    # under --xhci-userspace, for the reason `kernel/build.rs` gives at its own list: an unspawned ELF
    # in the image costs bytes, while a spawned service the kernel embedded as a placeholder fails
    # with `LoadFailed(TooSmall)` - a diagnostic that points at the binary rather than at the list.
    "xhci",
    # The pipe/IPC example services the shell composes (`roster | shell`, `greet`, `upper`, ...). All
    # arch-neutral SDK users - they needed building, not porting - and selfcheck exercises them, so a
    # missing one is 30 FAIL lines that look like a broken pipe implementation rather than an absent
    # binary.
    "counter", "greet", "upper", "roster", "reply-server", "asker", "resource-server", "holder",
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

# --xhci-userspace: take the USB stack OUT of the kernel and run it in the `xhci` service.
#
# The feature has the same name in two crates and BOTH have to get it. The kernel's copy stops the
# kernel from driving the VL805 (and publishes the BAR its PCIe scan found, so the spawn path can hand
# it over); the supervisor's copy makes it spawn the service that drives it instead. Set only one and
# you get either two drivers fighting over one host controller or no driver at all - a footgun with no
# diagnostic, because both halves build and boot perfectly well on their own. That is exactly why it
# is one flag here rather than two features to remember, and why it is not left to `--features` (which
# reaches only the kernel).
#
# It costs USB MASS STORAGE, and that is not a bug to be found later: the `xhci` service is HID-only,
# so with the in-kernel stack idle nothing implements Bulk-Only Transport and `block-driver` reports
# no disk. The kernel says so on the boot line next to "left to the xhci SERVICE".
XHCI_USERSPACE = "--xhci-userspace" in sys.argv
if XHCI_USERSPACE:
    FEATURES += ",xhci-userspace"

rel = ["--release"] if PROFILE == "release" else []

# 1. The services first - the kernel embeds their ELFs, so a stale service is baked into the image.
for svc in PI4_SERVICES:
    feats = ["--features", "bare-metal"] if svc == "supervisor" else []
    if svc == "supervisor" and XHCI_USERSPACE:
        feats = ["--features", "bare-metal,xhci-userspace"]
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
