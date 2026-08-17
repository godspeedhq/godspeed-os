#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Reproducible ARM32 (Raspberry Pi 2) build for GodspeedOS.

Cross-compiles the SDK + every arm-ported userspace service to armv7a-none-eabi,
then builds the kernel (which embeds those service ELFs via kernel/build.rs) and
objcopies it to a flat kernel7.img the Pi firmware / QEMU raspi2b can boot.

`osdev` is x86-only; this is the ARM equivalent of `osdev build` until ARM is a
first-class osdev target. Usage:

    python scripts/arm_build.py [--feature arm-supervisor] [--release]

The default feature is arm-supervisor (the full stack: supervisor -> logger +
ping/pong). The kernel spawns only the supervisor (C1-1), so there is no kernel-spawned
bring-up build any more - the supervisor path IS the bring-up path.
"""
import argparse, subprocess, sys, os, shutil
import io
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "armv7a-none-eabi"

# Services that build for ARM (arch-neutral: SDK + syscalls only, no x86 hardware
# probe). Must stay in sync with `arm_built` in kernel/build.rs. Hardware drivers
# DERIVED from `kernel/build.rs`, not declared here. Two lists of one fact is the shape that keeps
# biting: `kernel/build.rs` decides which services are embedded as REAL ELFs on ARM (`arm_built`), and
# this script decides which are CROSS-COMPILED - and you cannot embed what was never built. They are the
# same set, and keeping a second copy here means a service added to one and not the other silently
# becomes a PLACEHOLDER: the kernel builds, the image builds, and the failure is a runtime
# `LoadFailed(TooSmall)` at spawn.
#
# That is not hypothetical. It is the THIRD time this exact drift has landed (the scheduler's
# death-notification set vs the supervisor's watched set was the second, and is now derived too), and it
# cost the `console` service its first boot on hardware: everything reported success, the display just
# quietly stayed on the kernel's boot floor.
#
# So parse the kernel's list and use it. `kernel/build.rs` is the single source of truth because it is
# the one that must be right for the IMAGE to be right - this script only feeds it.
def _arm_services():
    kb = os.path.join(ROOT, "kernel", "build.rs")
    src = io.open(kb, encoding="utf-8").read()
    m = re.search(r"let arm_built: &\[&str\] = &\[(.*?)\n    \];", src, re.S)
    if not m:
        raise SystemExit(
            "arm_build: cannot find `arm_built` in kernel/build.rs. It is the source of truth for which\n"
            "services are built + embedded on ARM; refusing to guess, because guessing wrong produces a\n"
            "placeholder ELF and a service that fails to spawn at runtime with LoadFailed(TooSmall)."
        )
    # Strip comments, then take every quoted name.
    body = re.sub(r"//[^\n]*", "", m.group(1))
    return re.findall(r'"([a-z0-9-]+)"', body)


ARM_SERVICES = _arm_services()


def run(cmd):
    print(">", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=ROOT)
    if r.returncode != 0:
        print("FAILED:", " ".join(cmd), file=sys.stderr)
        sys.exit(r.returncode)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--feature", default="arm-supervisor",
                    help="kernel boot-path feature (arm-supervisor)")
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--qemu", action="store_true",
                    help="target QEMU emulation (identity DWC2 DMA); default is real-Pi hardware")
    args = ap.parse_args()
    profile = "release" if args.release else "debug"
    rel = ["--release"] if args.release else []
    kfeatures = args.feature + (",qemu" if args.qemu else "")

    # 1. Cross-compile every ARM-ported service to armv7 so build.rs can embed them.
    #    The Pi 2 is a bare-metal target (no QEMU control port), so the supervisor is built with its
    #    `bare-metal` feature - the designated "usable OS, quiet gsh> prompt" spawn set (logger + shell,
    #    no 178 harness probes, no ping/pong flood). ping/pong are spawnable on demand from the shell.
    for svc in ARM_SERVICES:
        feats = ["--features", "bare-metal"] if svc == "supervisor" else []
        run(["cargo", "build", "-p", svc, "--target", TARGET] + feats + rel)

    # 2. Build the kernel (embeds the service ELFs) with the chosen boot path.
    run(["cargo", "build", "-p", "kernel", "--target", TARGET,
         "--features", kfeatures] + rel)

    # 3. Flatten to a raw image the Pi firmware / QEMU loads at 0x8000.
    kelf = os.path.join(ROOT, "target", TARGET, profile, "kernel")
    out_dir = os.path.join(ROOT, "build")
    os.makedirs(out_dir, exist_ok=True)
    img = os.path.join(out_dir, "kernel7.img")
    objcopy = shutil.which("rust-objcopy") or "rust-objcopy"
    run([objcopy, "-O", "binary", kelf, img])

    # Stage the canonical Pi 2 boot config next to the kernel, so deploying is copying TWO files
    # (kernel7.img + config.txt) from build/ - the config the boot depends on is versioned in the repo
    # (boot/pi2/config.txt), never re-typed onto a card by hand (docs/pi2-deploy.md).
    cfg_src = os.path.join(ROOT, "boot", "pi2", "config.txt")
    if os.path.exists(cfg_src):
        shutil.copyfile(cfg_src, os.path.join(out_dir, "config.txt"))

    size = os.path.getsize(img)
    print(f"\nOK  build/kernel7.img  ({size} bytes, feature={kfeatures}, profile={profile})")
    print("Boot in QEMU:  python scripts/arm_run.py")
    print("Deploy to Pi:  copy build/kernel7.img + build/config.txt to the SD card's FAT boot partition")
    print("               (full procedure incl. the storage USB stick: docs/pi2-deploy.md)")


if __name__ == "__main__":
    main()
