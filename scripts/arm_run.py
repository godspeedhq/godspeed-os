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


def announce_kernel(path, profile):
    """Say WHICH kernel is booting, and shout if the other profile is newer.

    This default (`debug`) silently booted a stale kernel through several "verified in QEMU" runs whose
    builds had all been `--release`. Every result was wrong in the same believable direction: the fix
    under test appeared not to work, because the image under test predated it. Nothing was broken - the
    runner answered a question about a different binary and said nothing about which.

    A test harness that can run the wrong artifact must say which artifact it ran. Printing the profile
    and its age costs one line; the mismatch warning costs one more and is the one that matters, because
    the failure mode is not "no kernel" (that already errors) but "a kernel, just not yours".
    """
    age = time.time() - os.path.getmtime(path)
    print("booting %s kernel (built %s ago): %s" % (profile, _ago(age), path))
    other = "release" if profile == "debug" else "debug"
    op = find_kernel(other)
    if op and os.path.getmtime(op) > os.path.getmtime(path) + 1:
        print("WARNING: the %s kernel is NEWER than the %s one you are booting (by %s)."
              % (other, profile, _ago(os.path.getmtime(op) - os.path.getmtime(path))))
        print("WARNING: if you just built with --%s, pass --%s here or you are testing an old binary."
              % (other, other))


def _ago(secs):
    secs = int(secs)
    if secs < 60:
        return "%ds" % secs
    if secs < 3600:
        return "%dm%02ds" % (secs // 60, secs % 60)
    return "%dh%02dm" % (secs // 3600, (secs % 3600) // 60)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=20.0)
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--usb", action="store_true")
    ap.add_argument("--usbnet", action="store_true", help="attach a CDC-ECM USB-Ethernet device (user-net)")
    ap.add_argument("--sd", default=None, help="path to a raw SD-card image to attach (if=sd)")
    ap.add_argument("--usbdisk", default=None,
                    help="path to a raw image to attach as a USB mass-storage stick (block-driver "
                         "prefers this over the SD card, matching the Pi: boot from SD, store on USB)")
    ap.add_argument("--cmd", action="append", default=[])
    ap.add_argument("--chardelay", type=float, default=0.08, help="seconds between injected characters (raise if the shell drops/garbles input under TCG load)")
    ap.add_argument("--tail", type=int, default=3000)
    args = ap.parse_args()

    profile = "release" if args.release else "debug"
    krn = find_kernel(profile)
    if not krn:
        print("no kernel ELF - run scripts/arm_build.py first", file=sys.stderr)
        sys.exit(1)
    announce_kernel(krn, profile)

    machine = "raspi2b,usb=on" if (args.usb or args.usbnet or args.usbdisk) else "raspi2b"
    cmd = [QEMU, "-M", machine, "-kernel", krn, "-serial", "stdio", "-display", "none"]
    if args.usb:
        cmd += ["-device", "usb-kbd"]
    if args.usbnet:
        # Attach a CDC-ECM USB-Ethernet device on QEMU's user-net so net-stack can DHCP/ARP/ping.
        cmd += ["-netdev", "user,id=n0", "-device", "usb-net,netdev=n0"]
    if args.usbdisk:
        # A USB mass-storage stick: the storage target on a real Pi (the SD card holds the boot files).
        cmd += ["-drive", f"if=none,id=usbstick,format=raw,file={args.usbdisk}",
                "-device", "usb-storage,drive=usbstick"]
    if args.sd:
        # Attach an SD-card image so the block-driver (BCM2835 EMMC) has a disk to serve to fs.
        cmd += ["-drive", f"if=sd,format=raw,file={args.sd}"]

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

    # Prime the shell input: QEMU -serial stdio drops the FIRST byte after boot, so send a lone
    # newline before the real commands (an empty line the shell ignores) to absorb the drop.
    if args.cmd:
        p.stdin.write(b"\n"); p.stdin.flush(); time.sleep(0.5)
    for c in args.cmd:
        # Newline (not CR) is the Enter the shell acts on under QEMU -serial stdio.
        for ch in (c + "\n").encode():
            p.stdin.write(bytes([ch])); p.stdin.flush(); time.sleep(args.chardelay)
        time.sleep(6.0)

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
