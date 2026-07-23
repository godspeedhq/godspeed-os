#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Boot the ARM32 GodspeedOS kernel in QEMU raspi2b and capture serial output.

Usage:
    python scripts/arm_run.py [--secs N] [--usb] [--cmd "help" --cmd "version"]

By default boots for --secs seconds (headless, serial captured to
build/arm_serial.log) and prints the tail. --usb attaches an emulated usb-kbd to
the root port (note: QEMU's DWC2 does not complete transfers, so this only
exercises detection). --cmd sends a line to the shell (char-by-char, since the
shell echoes with erase sequences) after boot; repeatable.

This runs the kernel ELF directly (QEMU -kernel understands ELF); no objcopy
needed for QEMU. For a real Pi, build/kernel7.img is the flat image to copy.
"""
import argparse, subprocess, threading, time, sys, os, re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU = os.environ.get("QEMU_ARM", r"C:\Program Files\qemu\qemu-system-arm.exe")


def find_kernel(profile):
    p = os.path.join(ROOT, "target", "armv7a-none-eabi", profile, "kernel")
    return p if os.path.exists(p) else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=20.0)
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--usb", action="store_true")
    ap.add_argument("--cmd", action="append", default=[])
    ap.add_argument("--tail", type=int, default=3000)
    args = ap.parse_args()

    profile = "release" if args.release else "debug"
    krn = find_kernel(profile)
    if not krn:
        print("no kernel ELF - run scripts/arm_build.py first", file=sys.stderr)
        sys.exit(1)

    machine = "raspi2b,usb=on" if args.usb else "raspi2b"
    cmd = [QEMU, "-M", machine, "-kernel", krn, "-serial", "stdio", "-display", "none"]
    if args.usb:
        cmd += ["-device", "usb-kbd"]

    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, bufsize=0)
    buf = bytearray()

    def reader():
        while True:
            b = p.stdout.read(1)
            if not b:
                break
            buf.extend(b)
    threading.Thread(target=reader, daemon=True).start()

    t = time.time()
    while time.time() - t < args.secs:
        s = bytes(buf)
        if b"gsh>" in s or b"shell: ready" in s or b"supervisor: ready" in s:
            break
        time.sleep(0.2)
    time.sleep(1.0)

    for c in args.cmd:
        for ch in (c + "\r").encode():
            p.stdin.write(bytes([ch])); p.stdin.flush(); time.sleep(0.06)
        time.sleep(3.0)

    # let any remaining boot output settle
    end = time.time() + max(0.0, args.secs - (time.time() - t))
    while time.time() < end and not args.cmd:
        time.sleep(0.2)

    data = bytes(buf)
    try:
        p.kill()
    except Exception:
        pass

    logp = os.path.join(ROOT, "build", "arm_serial.log")
    os.makedirs(os.path.dirname(logp), exist_ok=True)
    with open(logp, "wb") as f:
        f.write(data)

    txt = data.decode("utf-8", "replace")
    txt = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", txt).replace("\x08", "").replace("[K", "")
    print("=== %d bytes; gsh>=%s supervisor:ready=%s (full log: build/arm_serial.log) ===" %
          (len(data), "gsh>" in txt, "supervisor: ready" in txt))
    print(txt[-args.tail:])


if __name__ == "__main__":
    main()
