#!/usr/bin/env python3
"""Carry ONE GSFS volume between architectures and back - `docs/gsfs-carnage.md` 3.11.

WHY THIS GATE EXISTS. Every storage guarantee in the carnage document is verified on one
architecture: fifteen suites, 75 tear points and the recovery measurements, all x86-64 and all
AHCI. The format is little-endian by construction and the code is architecture-neutral, so the
expectation is that a volume travels. An expectation is not a result.

It also hides a whole category of bug - anything where the BLOCK TRANSPORT changes the picture.
AHCI hands `fs` a sector; USB mass storage hands it one through BOT/SCSI over a split transaction.
The filesystem should not care, and "should not" is the phrase this programme exists to remove.

WHY IT COULD NOT BE RUN UNTIL NOW. It was recorded as NOT REACHABLE rather than merely not done:
no non-x86 port could attach a usable disk in QEMU (`backlog/34`). riscv64 had no drive option at
all, and it turned out the option it needed was a USB stick behind `qemu-xhci` rather than the AHCI
controller tried first - `build.rs` maps riscv64 to `storage_is_usb`, so `mod ahci` is not compiled
on that port and an AHCI controller is a device nothing there looks at.

THE SAME RAW FILE IS THE POINT. x86 sees it as an AHCI disk, riscv64 sees it as a USB mass-storage
stick behind xHCI. One image, two transports, two instruction sets - which is the whole test.

    py scripts/cross_isa.py                  # the full round trip

The volume is left on disk either way (`build/tests/cross_isa.img`): a run that failed IS the
evidence, and a fresh one is written at the start of every run so a pass can never be inherited.
"""
import argparse
import os
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU_X86 = os.environ.get("QEMU_X86", r"C:\Program Files\qemu\qemu-system-x86_64.exe")
QEMU_RV = os.environ.get("QEMU_RISCV64", r"C:\Program Files\qemu\qemu-system-riscv64.exe")

X86_IMAGE = os.path.join(ROOT, "build", "os.img")
RV_KERNEL = os.path.join(ROOT, "target", "riscv64imac-unknown-none-elf", "release", "kernel")
VOLUME = os.path.join(ROOT, "build", "tests", "cross_isa.img")
VOLUME_MB = 16

PASS, FAIL = [], []


def check(ok, label):
    (PASS if ok else FAIL).append(label)
    print("cross-isa: %s - %s" % ("PASS" if ok else "FAIL", label))


def drive(qemu_args, cmds, port, settle, boot_wait, log_path):
    """Boot a machine, wait for a prompt, type each line, and return everything seen.

    Typed CHARACTER BY CHARACTER with a delay, for the reason `riscv_run.py` records: the UART has
    a 16-byte FIFO and a pasted line outruns it.
    """
    child = subprocess.Popen(qemu_args, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    buf = b""
    try:
        sock = None
        for _ in range(60):
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        if sock is None:
            raise RuntimeError("could not connect to the guest serial on port %d" % port)
        sock.settimeout(0.5)

        def pump(seconds):
            nonlocal buf
            end = time.time() + seconds
            while time.time() < end:
                try:
                    b = sock.recv(8192)
                    if not b:
                        break
                    buf += b
                except socket.timeout:
                    pass
                except OSError:
                    break

        pump(boot_wait)
        for c in cmds:
            for ch in (c + "\r").encode():
                sock.send(bytes([ch]))
                time.sleep(0.02)
            pump(settle)
    finally:
        child.kill()
        child.wait()
    text = buf.decode("utf-8", errors="replace")
    with open(log_path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    return text


def x86(cmds, settle=6, boot_wait=40, log="build/cross_isa_x86.log"):
    """x86-64 sees the volume as an AHCI disk - the same wiring `osdev test files` uses."""
    vol = VOLUME.replace("\\", "/")
    img = X86_IMAGE.replace("\\", "/")
    args = [QEMU_X86,
            "-drive", "format=raw,file=%s,if=ide" % img,
            "-device", "ich9-ahci,id=ahci",
            "-drive", "id=data,format=raw,file=%s,if=none" % vol,
            "-device", "ide-hd,drive=data,bus=ahci.0",
            "-smp", "4", "-m", "512M",
            "-serial", "tcp::5591,server",
            "-serial", "null",
            "-display", "none", "-no-reboot", "-no-shutdown"]
    return drive(args, cmds, 5591, settle, boot_wait, os.path.join(ROOT, log))


def riscv(cmds, settle=10, boot_wait=55, log="build/cross_isa_riscv.log"):
    """riscv64 sees the SAME FILE as a USB stick behind xHCI - `storage_is_usb` on this port."""
    vol = VOLUME.replace("\\", "/")
    args = [QEMU_RV, "-M", "virt", "-m", "256M", "-smp", "4", "-nographic",
            "-bios", "default", "-kernel", RV_KERNEL,
            "-device", "qemu-xhci,id=xhci0",
            "-drive", "if=none,id=d0,format=raw,file=%s" % vol,
            "-device", "usb-storage,bus=xhci0.0,drive=d0",
            "-serial", "tcp::5592,server",
            "-display", "none", "-no-reboot"]
    return drive(args, cmds, 5592, settle, boot_wait, os.path.join(ROOT, log))


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.parse_args()

    if not os.path.exists(X86_IMAGE):
        sys.exit("cross-isa: missing %s\n  build it: cargo run -p osdev -- image" % X86_IMAGE)

    # BUILD THE riscv64 KERNEL HERE, EVERY RUN, and a bug this gate already hit is the reason.
    # `riscv_build.py --visionfive` links at 0x40200000 for the board; QEMU's `virt` loads at
    # 0x80200000. Run the gate after a board build and OpenSBI comes up, our kernel prints
    # nothing at all, and every riscv64 assertion fails - which reads exactly like "riscv64
    # cannot mount an x86 volume" and is instead "the kernel was linked for another machine".
    # It cost one full red run to notice. A test that silently uses the wrong artefact is worse
    # than one that refuses to start, so this builds what it needs rather than trusting what is
    # lying in `target/`.
    print("cross-isa: building the riscv64 kernel for QEMU (not the board load address)")
    r = subprocess.run([sys.executable, os.path.join(ROOT, "scripts", "riscv_build.py"),
                        "--release"], cwd=ROOT, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit("cross-isa: riscv64 build FAILED\n" + (r.stdout or "") + (r.stderr or ""))

    os.makedirs(os.path.dirname(VOLUME), exist_ok=True)
    # A FRESH volume every run. A pass inherited from a previous run is not a pass.
    with open(VOLUME, "wb") as fh:
        fh.truncate(VOLUME_MB * 1024 * 1024)
    print("cross-isa: volume %s (%d MiB, zeroed)" % (VOLUME, VOLUME_MB))

    # ---- LEG 1: x86-64 formats it and writes -------------------------------------------------
    print("\n=== leg 1: x86-64 (AHCI) formats and writes ===")
    out = x86(["drives flash data", "y",
               "write /from-x86.txt written-on-x86",
               "mkdir /shared",
               "write /shared/note.txt shared-note-from-x86",
               "read /from-x86.txt",
               "dir /"], settle=8)
    check("formatted as GSFS" in out, "x86 formatted the volume")
    check("written-on-x86" in out, "x86 read back its own file")
    check("from-x86.txt" in out, "x86 lists the file it wrote")

    # ---- LEG 2: riscv64 reads what x86 wrote, and writes its own -----------------------------
    print("\n=== leg 2: riscv64 (USB/BOT-SCSI) reads it, and writes back ===")
    out = riscv(["dir /",
                 "read /from-x86.txt",
                 "read /shared/note.txt",
                 "write /from-riscv.txt written-on-riscv64",
                 "read /from-riscv.txt",
                 "drives check"])
    check("mounted GSFS" in out or "from-x86.txt" in out,
          "riscv64 MOUNTED a volume formatted by x86-64")
    check("written-on-x86" in out, "riscv64 read the file x86 wrote")
    check("shared-note-from-x86" in out, "riscv64 read through a directory x86 created")
    check("written-on-riscv64" in out, "riscv64 wrote and read back its own file")
    check("0 bad" in out, "riscv64 fsck finds no corrupt blocks in an x86 volume")

    # ---- LEG 3: back to x86-64 ----------------------------------------------------------------
    print("\n=== leg 3: back on x86-64 - did riscv64's writes survive? ===")
    out = x86(["dir /",
               "read /from-riscv.txt",
               "read /from-x86.txt",
               "drives check"], settle=8)
    check("written-on-riscv64" in out, "x86 read the file riscv64 wrote")
    check("written-on-x86" in out, "x86's own file survived the round trip")
    check("0 bad" in out, "x86 fsck finds no corrupt blocks after riscv64 wrote")
    check("consistent" in out or "ok" in out, "the volume is consistent after both")

    print("\ncross-isa: %d passed, %d failed" % (len(PASS), len(FAIL)))
    for f in FAIL:
        print("  FAILED: %s" % f)
    print("logs -> build/cross_isa_x86.log, build/cross_isa_riscv.log")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
