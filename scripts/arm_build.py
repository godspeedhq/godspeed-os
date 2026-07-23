#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Reproducible ARM32 (Raspberry Pi 2) build for GodspeedOS.

Cross-compiles the SDK + every arm-ported userspace service to armv7a-none-eabi,
then builds the kernel (which embeds those service ELFs via kernel/build.rs) and
objcopies it to a flat kernel7.img the Pi firmware / QEMU raspi2b can boot.

`osdev` is x86-only; this is the ARM equivalent of `osdev build` until ARM is a
first-class osdev target. Usage:

    python scripts/arm_build.py [--feature arm-shell|arm-supervisor] [--release]

The default feature is arm-supervisor (the full stack: supervisor -> logger +
ping/pong). Pass --feature arm-shell for the logger+shell-only prompt build.
"""
import argparse, subprocess, sys, os, shutil

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TARGET = "armv7a-none-eabi"

# Services that build for ARM (arch-neutral: SDK + syscalls only, no x86 hardware
# probe). Must stay in sync with `arm_built` in kernel/build.rs. Hardware drivers
# (block-driver, fs, nic-driver, net-stack, xhci, ehci) are omitted - they hunt for
# x86 hardware absent on the Pi 2 and stay placeholders until real Pi drivers exist.
ARM_SERVICES = [
    "logger", "ping", "pong", "supervisor", "shell",
    "observe", "chaos", "mem-pressure",
    "counter", "greet", "upper", "roster",
    "reply-server", "asker", "resource-server", "holder",
    # Persistence on the Pi 2: block-driver (BCM2835 EMMC/SDHCI PIO backend) + arch-neutral fs.
    "block-driver", "fs",
    # Networking on the Pi 2: nic-driver (USB-net bridge backend) + arch-neutral net-stack.
    "nic-driver", "net-stack",
]


def run(cmd):
    print(">", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=ROOT)
    if r.returncode != 0:
        print("FAILED:", " ".join(cmd), file=sys.stderr)
        sys.exit(r.returncode)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--feature", default="arm-supervisor",
                    help="kernel boot-path feature (arm-supervisor | arm-shell | ...)")
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

    size = os.path.getsize(img)
    print(f"\nOK  build/kernel7.img  ({size} bytes, feature={kfeatures}, profile={profile})")
    print("Boot in QEMU:  python scripts/arm_run.py")
    print("Deploy to Pi:  copy build/kernel7.img to the SD card's FAT32 partition")


if __name__ == "__main__":
    main()
