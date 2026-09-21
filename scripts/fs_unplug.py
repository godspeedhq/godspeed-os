#!/usr/bin/env python3
"""Pull the DISK out mid-write - `docs/gsfs-carnage.md` 3.7's last row.

WHAT MAKES THIS DIFFERENT FROM `fs-blockdeath`. That suite kills `block-driver` while requests are
outstanding: the driver dies, the supervisor respawns it, and the disk was there the whole time.
Here the DEVICE vanishes and does not come back. Nothing is restarted, nothing recovers it, and
every layer above has to answer for a disk that is simply gone.

WHY riscv64 AND NOT x86, which is where every other storage suite runs. Asked rather than assumed,
and the answer changed the plan:

    (qemu) device_del thedisk
    Error: Bus 'ahci.0' does not support hotplugging

3.7 records this row as needing "`device_del` over the monitor", and on the machine our storage
suites use that command is REFUSED. QEMU's AHCI cannot hot-unplug. riscv64 carries its disk as a
USB stick behind xHCI (`storage_is_usb`), and USB is built for exactly this - the same `device_del`
is accepted and the device is gone. It is also the more honest unplug: people pull USB sticks, and
nobody hot-pulls a SATA disk.

THE GATE. A device that disappears mid-write must produce a LOUD, BOUNDED failure. Not a hang, not
a panic, and above all not a confident wrong answer - the machine must stay alive and say what
happened.

    py scripts/fs_unplug.py
"""
import os
import re
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU = os.environ.get("QEMU_RISCV64", r"C:\Program Files\qemu\qemu-system-riscv64.exe")
KERNEL = os.path.join(ROOT, "target", "riscv64imac-unknown-none-elf", "release", "kernel")
VOLUME = os.path.join(ROOT, "build", "tests", "unplug.img")
VOLUME_MB = 16
SERIAL, MONITOR = 5613, 5614
LOG = os.path.join(ROOT, "build", "fs_unplug.log")

ANSI = re.compile(r"\x1b\[[0-9;]*[A-Za-z]|\x08")
PASS, FAIL = [], []


def check(ok, label):
    (PASS if ok else FAIL).append(label)
    print("fs-unplug: %s - %s" % ("PASS" if ok else "FAIL", label))


def main():
    if not os.path.exists(KERNEL):
        sys.exit("fs-unplug: no riscv64 kernel - py scripts/riscv_build.py --release")

    os.makedirs(os.path.dirname(VOLUME), exist_ok=True)
    with open(VOLUME, "wb") as fh:
        fh.truncate(VOLUME_MB * 1024 * 1024)

    args = [QEMU, "-M", "virt", "-m", "256M", "-smp", "4", "-nographic",
            "-bios", "default", "-kernel", KERNEL,
            "-device", "qemu-xhci,id=xhci0",
            "-drive", "if=none,id=d0,format=raw,file=%s" % VOLUME.replace("\\", "/"),
            "-device", "usb-storage,bus=xhci0.0,drive=d0,id=thestick",
            "-serial", "tcp::%d,server" % SERIAL,
            "-monitor", "tcp::%d,server,nowait" % MONITOR,
            "-display", "none", "-no-reboot"]

    child = subprocess.Popen(args, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    buf = b""
    try:
        ser = mon = None
        for _ in range(60):
            try:
                ser = socket.create_connection(("127.0.0.1", SERIAL), timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        if ser is None:
            sys.exit("fs-unplug: no guest serial")
        for _ in range(40):
            try:
                mon = socket.create_connection(("127.0.0.1", MONITOR), timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        if mon is None:
            sys.exit("fs-unplug: no monitor")
        ser.settimeout(0.5)
        mon.settimeout(1.0)

        def pump(seconds):
            nonlocal buf
            end = time.time() + seconds
            while time.time() < end:
                try:
                    b = ser.recv(8192)
                    if not b:
                        break
                    buf += b
                except socket.timeout:
                    pass
                except OSError:
                    break

        def typ(line, settle):
            for ch in (line + "\r").encode():
                ser.send(bytes([ch]))
                time.sleep(0.02)
            pump(settle)

        pump(55)

        # ---- a real filesystem, with something on it worth losing ----
        typ("drives flash data", 6)
        typ("y", 25)
        typ("write /before.txt written-before-the-unplug", 12)
        before = len(buf)
        typ("read /before.txt", 10)
        wrote = "written-before-the-unplug" in ANSI.sub("", buf[before:].decode("utf-8", "replace"))
        check(wrote, "a volume was formatted and written before the unplug")

        # ---- SUSTAINED WRITES, then the stick is pulled from under them ----
        # `churn` hammers the filesystem, so the unplug lands with real I/O in flight rather than at
        # an idle prompt - which is the difference between testing the failure path and testing
        # nothing (the lesson `fs-blockdeath`'s precondition records).
        mark = len(buf)
        for ch in b"churn 25\r":
            ser.send(bytes([ch]))
            time.sleep(0.02)
        pump(6)
        writing = "churn" in ANSI.sub("", buf[mark:].decode("utf-8", "replace"))
        check(writing, "churn was writing when the device was pulled")

        print("fs-unplug: >>> device_del thestick <<<")
        unplug_mark = len(buf)
        unplug_at = time.time()
        mon.send(b"device_del thestick\n")

        # WAIT FOR THE EVENT, NOT FOR A CLOCK. The first version slept 45 s and then reported
        # `answered_in` as 45 s every time - it measured its own sleep, which is the mistake this
        # programme keeps catching: a timing assertion that cannot fail is not an assertion. Poll in
        # small steps and stop the moment the system SAYS something about the disk, so the number is
        # the system's latency and not the harness's patience.
        answered_in = None
        deadline = time.time() + 60
        while time.time() < deadline:
            pump(1)
            seen = ANSI.sub("", buf[unplug_mark:].decode("utf-8", "replace"))
            if ("storage unavailable" in seen or "UNAVAILABLE" in seen or "no disk" in seen
                    or "not bound" in seen or "refus" in seen or "I/O error" in seen):
                answered_in = time.time() - unplug_at
                break
        if answered_in is None:
            answered_in = time.time() - unplug_at
        # Let the rest of churn's window drain so the prompt is back before we type again.
        pump(12)

        # ---- the machine must still be here, and still answering ----
        typ("", 8)
        typ("drives", 20)
        typ("dir /", 20)
        typ("status | where name contains fs", 15)

        text = ANSI.sub("", buf.decode("utf-8", "replace"))
        with open(LOG, "w", encoding="utf-8", newline="") as fh:
            fh.write(text)
        after = text[text.index("churn 25"):] if "churn 25" in text else text
    finally:
        child.kill()
        child.wait()

    # ---- THE GATE ----
    check("KERNEL PANIC" not in text, "the kernel did not panic when the disk vanished")
    check("LIVENESS WEDGE" not in text, "no core wedged when the disk vanished")
    check("gsh>" in after, "the shell came back to a prompt after the device vanished")
    # `fs` allows each block request 30 s (`block-driver` legitimately retries a busy device that
    # long), so anything under that means the layers noticed the disappearance rather than sitting
    # out the full deadline. Reported either way - the number is the point, not just the verdict.
    check(answered_in < 30.0,
          "the failure was BOUNDED - the disappearance was reported %.1f s after the unplug, "
          "inside the 30 s a block request is allowed" % answered_in)

    # It must SAY the disk is gone. A truthful refusal is the whole requirement; a confident wrong
    # answer about storage is the thing this programme exists to prevent.
    said = ("storage unavailable" in after or "no disk" in after
            or "UNAVAILABLE" in after or "not bound" in after or "refus" in after)
    check(said, "the disappearance was REPORTED, not silently absorbed")
    check("written-before-the-unplug" not in after.split("dir /")[-1],
          "no stale content is served from a device that is gone")

    print("\nfs-unplug: %d passed, %d failed   (serial -> build/fs_unplug.log)"
          % (len(PASS), len(FAIL)))
    for f in FAIL:
        print("  FAILED: %s" % f)
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
