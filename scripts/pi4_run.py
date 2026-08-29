#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Boot the AArch64 (Pi 4) GodspeedOS kernel in QEMU raspi4b and capture serial output.

Usage:
    python scripts/pi4_run.py [--secs N] [--cmd "version" --cmd "help"] [--shot out.ppm]

Boots headless, captures serial to build/pi4_serial.log, and prints the tail. --cmd sends a line to
the shell after boot (character by character, since the shell echoes with erase sequences);
repeatable. --shot writes a framebuffer screendump through the QEMU monitor, which is the only way to
check what the DISPLAY shows without a TV attached - serial cannot answer that question.

Runs the kernel ELF directly (QEMU -kernel understands ELF). For a real Pi, build/kernel8.img is the
flat image to copy to the SD card.

Note on what QEMU can and cannot prove here: raspi4b emulates the VideoCore property mailbox and the
framebuffer, so the allocation path and the rendering are exercised. It does NOT reproduce the real
firmware's memory carve or a real display's reported size, so a green QEMU run is a necessary check,
not a sufficient one.
"""
import argparse, subprocess, threading, time, sys, os, re, socket

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU = os.environ.get("QEMU_AARCH64", r"C:\Program Files\qemu\qemu-system-aarch64.exe")
MONITOR_PORT = 55571


def find_kernel(profile):
    p = os.path.join(ROOT, "target", "aarch64-unknown-none", profile, "kernel")
    return p if os.path.exists(p) else None


def monitor_cmd(line):
    """Send one command to the QEMU monitor over TCP. Returns whatever came back."""
    s = socket.create_connection(("127.0.0.1", MONITOR_PORT), timeout=5)
    try:
        time.sleep(0.3)
        s.recv(65536)  # banner
        s.sendall((line + "\n").encode())
        time.sleep(1.0)
        return s.recv(65536).decode("utf-8", "replace")
    finally:
        s.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=float, default=25.0)
    ap.add_argument("--debug", action="store_true", help="use the debug profile ELF")
    ap.add_argument("--cmd", action="append", default=[])
    ap.add_argument("--chardelay", type=float, default=0.08)
    ap.add_argument("--shot", default=None, help="write a framebuffer screendump (PPM) here")
    ap.add_argument("--tail", type=int, default=4000)
    # A USB mass-storage device, so the suite's file tests have something to run against.
    #
    # QEMU's raspi4b accepts `-device usb-storage` (there is a usb bus), but it emulates no PCIe
    # VL805, so the kernel's probe finds no xHCI controller and the `xhci` service comes up with
    # "no controller MMIO granted - idling". The disk is therefore NOT visible to the OS today and
    # `fs` comes up storage-unavailable - which it does cleanly, without hanging, so everything that
    # does not touch the filesystem still runs. The flag is here because attaching the drive is the
    # half we control; the missing half is emulation, and that is recorded rather than worked around.
    ap.add_argument("--drive", default=None, help="raw disk image to attach as USB mass storage")
    args = ap.parse_args()

    profile = "debug" if args.debug else "release"
    krn = find_kernel(profile)
    if not krn:
        print("no kernel ELF - run scripts/pi4_build.py first", file=sys.stderr)
        sys.exit(1)

    cmd = [QEMU, "-M", "raspi4b", "-kernel", krn, "-serial", "stdio", "-display", "none"]
    if args.drive:
        cmd += ["-device", "usb-storage,drive=d0",
                "-drive", f"if=none,id=d0,format=raw,file={args.drive}"]
    if args.shot:
        cmd += ["-monitor", f"tcp:127.0.0.1:{MONITOR_PORT},server,nowait"]

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
        if b"gsh>" in s or b"shell: ready" in s:
            break
        time.sleep(0.2)
    time.sleep(1.5)

    # Prime the shell input: QEMU -serial stdio drops the FIRST byte after boot, so send a lone
    # newline before the real commands (an empty line the shell ignores) to absorb the drop.
    if args.cmd:
        p.stdin.write(b"\n"); p.stdin.flush(); time.sleep(0.5)
    for c in args.cmd:
        for ch in (c + "\n").encode():
            p.stdin.write(bytes([ch])); p.stdin.flush(); time.sleep(args.chardelay)
        time.sleep(4.0)

    if args.shot:
        out = os.path.abspath(args.shot).replace("\\", "/")
        try:
            monitor_cmd(f"screendump {out}")
            time.sleep(1.0)
            print(f"screendump -> {out} ({os.path.getsize(out)} bytes)"
                  if os.path.exists(out) else f"screendump FAILED - no {out}")
        except Exception as e:
            print(f"screendump FAILED: {e}")

    end = time.time() + max(0.0, args.secs - (time.time() - t))
    while time.time() < end and not args.cmd:
        time.sleep(0.2)

    data = bytes(buf)
    try:
        p.kill()
    except Exception:
        pass

    logp = os.path.join(ROOT, "build", "pi4_serial.log")
    os.makedirs(os.path.dirname(logp), exist_ok=True)
    with open(logp, "wb") as f:
        f.write(data)

    txt = data.decode("utf-8", "replace")
    txt = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", txt).replace("\x08", "").replace("[K", "")
    print("=== %d bytes; gsh>=%s framebuffer=%s panic=%s (full log: build/pi4_serial.log) ===" %
          (len(data), "gsh>" in txt, "framebuffer console up" in txt, "PANIC" in txt))
    print(txt[-args.tail:])


if __name__ == "__main__":
    main()
