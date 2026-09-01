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

(The former `--xhci-userspace` flag is gone: the in-kernel USB driver was deleted, so the
service is the only driver and there is nothing to switch. It used to have to reach TWO
crates: the kernel (stop driving the VL805) and the `supervisor` (start spawning
the `xhci` service that drives it instead). See where it is handled below.

usage:  python scripts/pi4_build.py [--debug] [--features a,b]
"""
import subprocess, sys, os, pathlib, re, shutil, io

ROOT = pathlib.Path(__file__).resolve().parents[1]

# Quoted lowercase service name, e.g. "net-stack". Built from chr() so this file stays
# free of regex backslashes.
NAME_RE = chr(34) + "([a-z0-9" + chr(45) + "]+)" + chr(34)
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
def _pi4_services():
    """The services to build, DERIVED from `kernel/build.rs` rather than restated here.

    This was a hand-maintained list, and it drifted exactly the way a second copy of a fact always
    does: `console`, `time` and `control` were spawned by the supervisor on this port and missing
    here, so the kernel embedded empty placeholders and the first Pi 4 boot after the arm32 work
    reported three `LoadFailed(TooSmall)` lines - a diagnostic that points at the binaries rather
    than at the list. The Pi 2 had already hit this and already fixed it the right way
    (`_arm_services` in arm_build.py); the Pi 4 kept the duplicate. Commandment III: one truth, and
    everything else derives from it.

    Both arms of the `aarch64_built` conditional are taken, because the asymmetry of being wrong runs
    one way. Building a service the kernel does not embed costs a few seconds; NOT building one it
    does embed costs a placeholder, a failed spawn on the board, and a hunt for a bug in a binary
    that was simply never compiled.
    """
    kb = os.path.join(ROOT, "kernel", "build.rs")
    src = io.open(kb, encoding="utf-8").read()
    head = "let aarch64_built: &[&str] = if aarch64_demo {"
    at = src.find(head)
    end = src.find(chr(10) + "    };", at) if at >= 0 else -1
    if at < 0 or end < 0:
        raise SystemExit(
            "pi4_build: cannot find `aarch64_built` in kernel/build.rs. It is the source of "
            "truth for which services are built + embedded on the Pi 4; refusing to guess, "
            "because guessing wrong produces a placeholder ELF and a service that fails to "
            "spawn at runtime with LoadFailed(TooSmall)."
        )
    # Strip line comments before harvesting names, so a service mentioned only in prose does not
    # get built and, worse, a name REMOVED from the list but still discussed does not linger.
    body = chr(10).join(ln.split("//")[0] for ln in src[at:end].split(chr(10)))
    seen, out = set(), []
    for n in re.findall(NAME_RE, body):
        if n not in seen:
            seen.add(n)
            out.append(n)
    return out


PI4_SERVICES = _pi4_services()

# `pi4` is always present; anything passed is added to it, not substituted for it. `pi4-smp` rides with
# it now that all four cores come up on every boot and survive a carnage run - a Pi 4 running on one of
# its four cores is not the machine.
FEATURES = "pi4,pi4-smp"
if "--features" in sys.argv:
    i = sys.argv.index("--features")
    if i + 1 >= len(sys.argv):
        sys.exit("--features needs a comma-separated list")
    FEATURES = "pi4,pi4-smp," + sys.argv[i + 1]

# USB lives in the `xhci` SERVICE. There is no in-kernel USB driver on aarch64 any more - it was
# deleted, along with the feature flags that used to choose between them, so there is nothing here to
# set and nothing to forget to set. (The old footgun: the flag had to reach the kernel, the supervisor
# AND block-driver, and setting only some of them gave you two drivers fighting over one controller,
# or none, with both halves booting fine on their own.)
#
# arm32 (Pi 2) is unaffected: no PCIe and no device-IRQ routing to userspace, so its USB stack is
# still in the kernel - see arch/arm/CLAUDE.md.
EL0_FAULT_TEST = "--el0-fault-test" in sys.argv

rel = ["--release"] if PROFILE == "release" else []

# Refuse to build an image whose supervisor spawns a service the kernel embeds as a placeholder.
# This is the gate for the failure that produced `LoadFailed(TooSmall)` on this port twice.
sys.path.insert(0, str(ROOT / "scripts"))
import service_embed_check
service_embed_check.enforce(str(ROOT), "aarch64")

# 1. The services first - the kernel embeds their ELFs, so a stale service is baked into the image.
# THE SUPERVISOR IS BUILT LAST - it `include_bytes!`s every other service, so building it earlier
# (it was index 6 of 24) ships the PREVIOUS build of everything after it. See scripts/embed_order_check.py.
_ORDERED = [s for s in PI4_SERVICES if s != "supervisor"] +            (["supervisor"] if "supervisor" in PI4_SERVICES else [])
for svc in _ORDERED:
    feats = ["--features", "bare-metal"] if svc == "supervisor" else []
    if svc == "supervisor":
        feats = ["--features", "bare-metal"]
    # The THIRD crate the one switch has to reach. block-driver must be told to fetch its sectors
    # from the `xhci` SERVICE over IPC instead of from the in-kernel stack by syscall; without it the
    # service drives the disk and block-driver asks a kernel that is no longer driving anything, so
    # storage silently disappears with every individual piece looking correct.
    # Prove the EL0 fault-recovery path actually fires (see mem-pressure's feature doc). Test builds
    # only; it kills mem-pressure on every boot by design.
    if svc == "net-stack" and EL0_FAULT_TEST:
        feats = ["--features", "el0-fault-test"]
    run(["cargo", "build", "-p", svc, "--target", TARGET] + feats + rel)

# Every service's frames must fit the 256 KiB user stack (USER_STACK_PAGES in kernel/src/task/mod.rs).
# The Pi 2 has had this gate since a debug `fs` crash-looped on a 503 KiB frame; the Pi 4 had none, and
# a rule enforced on one build path is enforced on none.
import stack_fit_check
stack_fit_check.enforce(
    tool("rust-objdump"), str(ROOT), TARGET, PROFILE, PI4_SERVICES, 64 * 4096)

sys.path.insert(0, str(ROOT / "scripts"))
import embed_order_check
embed_order_check.enforce(str(ROOT), TARGET, PROFILE, PI4_SERVICES)

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
# Stage the canonical Pi 4 boot config next to the kernel, so deploying is copying TWO files
# from build/ - the config the boot depends on is versioned in the repo (boot/pi4/config.txt),
# never re-typed onto a card by hand. Same rule as the Pi 2 (scripts/arm_build.py).
cfg_src = ROOT / "boot" / "pi4" / "config.txt"
if cfg_src.exists():
    shutil.copyfile(cfg_src, img.parent / "config.txt")
    print("OK  build/config.txt   (canonical copy of boot/pi4/config.txt)")
else:
    # Loud, not silent: a missing config means the card would keep whatever `kernel=` line it
    # already had, and boot SOMEONE ELSE'S kernel while reporting a successful build.
    print(f"WARNING: {cfg_src} is missing - build/config.txt NOT staged")

print("Deploy to Pi 4: copy build/kernel8.img to the card's FAT boot partition AS godspeed8.img,")
print("                and build/config.txt as config.txt. The name differs on purpose: it leaves")
print("                a stock Raspberry Pi OS kernel8.img in place as a known-good fallback.")
