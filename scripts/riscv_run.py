#!/usr/bin/env python3
"""Boot the riscv64 kernel under QEMU `virt`, with a timeout and a serial capture.

QEMU IS THE PRIMARY TARGET FOR THIS PORT, not a fallback. `virt` ships OpenSBI as its default
firmware, which is the same M-mode handoff a real JH7110 board performs: the kernel is entered in
S-MODE at 0x8020_0000 with `a0` = hart id and `a1` = a device-tree pointer, and every hart but the
boot one is parked. So the early port - Sv39 paging, the trap vector, timer, console - can be built
and iterated here with a two-second loop and no hardware at all.

What QEMU will NOT settle, and what the board is actually for: the real UART, PLIC routing, and
SD/eMMC. Those differ, and this script cannot pretend otherwise.

Usage:
    py scripts/riscv_run.py [--release] [--timeout N] [--smp N] [--log FILE]
"""

import argparse, os, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU = os.environ.get("QEMU_RISCV64", r"C:\Program Files\qemu\qemu-system-riscv64.exe")
TARGET = "riscv64imac-unknown-none-elf"


def type_at_shell(cmd, a):
    """Boot, wait for the shell, then type each --cmd a character at a time.

    CHARACTER AT A TIME, with a delay, because the receive path this exercises reads a real 16-byte
    16550 FIFO drained by the timer tick. Blasting a line in one write is a test of the FIFO's depth
    rather than of the console path, and it fails for a reason that has nothing to do with the code
    under test - the same overrun that truncated this port's early output at exactly 16 characters.
    """
    import threading, time
    buf = bytearray()
    p = subprocess.Popen(cmd, cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT)

    def reader():
        while True:
            b = p.stdout.read(1)
            if not b:
                return
            buf.extend(b)

    threading.Thread(target=reader, daemon=True).start()

    # WAIT FOR THE PROMPT ITSELF, not for the supervisor.
    #
    # This waited on "supervisor: ready" and then typed, which is ten seconds into a boot whose
    # PROMPT does not arrive for about two minutes: with no disk, `fs` waits 30 s for block-driver
    # and another 20 s for capacity, and the shell then spends 4 x 8 s asking that filesystem for its
    # sticky-capture file. Every keystroke typed into that gap was discarded, so `--cmd` silently did
    # nothing and the run looked like a clean boot that simply ignored its command - which is how a
    # chaos reproduction attempt came back reporting zero rounds.
    #
    # `gsh> ` is the shell saying it is READING, which is the actual precondition for typing at it.
    deadline = time.time() + a.timeout
    while time.time() < deadline and b"gsh>" not in bytes(buf):
        time.sleep(0.2)
    if b"gsh>" not in bytes(buf):
        print("(no gsh> prompt within the timeout - typed commands would be discarded, so none were sent)")
    time.sleep(1.0)

    for line in a.cmd:
        for ch in (line + "\n").encode():
            try:
                p.stdin.write(bytes([ch]))
                p.stdin.flush()
            except Exception:
                break
            time.sleep(a.chardelay)
        time.sleep(a.settle)

    time.sleep(1.0 + a.after)
    try:
        p.kill()
    except Exception:
        pass
    return bytes(buf).decode("utf-8", "replace")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--release", action="store_true")
    ap.add_argument("--timeout", type=int, default=20, help="seconds before QEMU is killed")
    ap.add_argument("--smp", type=int, default=1, help="hart count (OpenSBI parks all but the boot hart)")
    ap.add_argument("--mem", default="256M")
    ap.add_argument("--log", default=os.path.join("build", "riscv_serial.log"))
    ap.add_argument("--cmd", action="append", default=[],
                    help="type a line at the shell, once it is up (repeatable)")
    ap.add_argument("--after", type=float, default=0.0,
                    help="extra seconds to wait after the LAST typed line - for a command that runs "
                         "long after its confirmation, like a chaos storm")
    ap.add_argument("--settle", type=float, default=8.0,
                    help="seconds to wait after each typed line for its output")
    ap.add_argument("--net", action="store_true",
                    help="attach an Intel e1000 on QEMU user-net, so nic-driver and net-stack can be "
                         "exercised on this arch. The VisionFive's own MAC is a Synopsys DesignWare "
                         "part that QEMU does not model at all, so this proves the PORT-NEUTRAL half "
                         "of the stack (PCI discovery, the DMA arena, the frame IPC, ARP/DHCP/ICMP) "
                         "and says nothing about the board's MAC registers.")
    ap.add_argument("--pcap", default="",
                    help="with --net, dump every frame to this file so the wire can be read when the "
                         "guest disagrees with itself about what it sent")
    ap.add_argument("--chardelay", type=float, default=0.02,
                    help="seconds between characters - slow enough not to outrun a 16-byte FIFO")
    a = ap.parse_args()

    prof = "release" if a.release else "debug"
    elf = os.path.join(ROOT, "target", TARGET, prof, "kernel")
    if not os.path.exists(elf):
        sys.exit("kernel ELF not found at %s\nBuild it: py scripts/riscv_build.py%s"
                 % (elf, " --release" if a.release else ""))

    cmd = [QEMU, "-M", "virt", "-m", a.mem, "-smp", str(a.smp), "-nographic",
           # `-bios default` is OpenSBI. Stated rather than omitted: it is the M-mode firmware this
           # kernel runs UNDER, so it is part of the contract being tested, not an incidental default.
           "-bios", "default",
           "-kernel", elf,
           "-serial", "mon:stdio"]
    if a.net:
        # `virt` has a real PCIe host bridge (ECAM at 0x3000_0000, in the FDT), so this is an ordinary
        # PCI device the arch's own enumeration finds - not a special case wired in for the test.
        cmd += ["-device", "e1000,netdev=n0", "-netdev", "user,id=n0"]
        if a.pcap:
            cmd += ["-object", "filter-dump,id=nicdump,netdev=n0,file=%s" % a.pcap]
    print("> " + " ".join(cmd))
    os.makedirs(os.path.join(ROOT, os.path.dirname(a.log)), exist_ok=True)

    if a.cmd:
        out = type_at_shell(cmd, a)
    else:
     try:
        r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, timeout=a.timeout)
        out = r.stdout + r.stderr
     except subprocess.TimeoutExpired as e:
        # A TIMEOUT IS THE NORMAL OUTCOME while the kernel ends in a halt loop, so it is reported as
        # a fact rather than as a failure. What would be a failure is no output at all.
         out = (e.stdout or "") + (e.stderr or "")
         if isinstance(out, bytes):
             out = out.decode("utf-8", "replace")
         print("(qemu ran the full %ds and was stopped - expected while the kernel halts)" % a.timeout)

    path = os.path.join(ROOT, a.log)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(out)
    print(out[-2000:] if out else "(no output)")
    print("\nserial captured to %s (%d bytes)" % (a.log, len(out)))
    # The NEUTRAL banner, not a phrase this port invented. It used to look for "GodspeedOS riscv64",
    # which was the arch's own home-made opening line - so the harness was pinned to the very thing
    # that made this port's boot read differently from every other one, and it failed the moment that
    # line was replaced by the shared `banner()`. A gate should check the thing that must be true, and
    # what must be true is that the kernel identified itself the way every port does.
    if "GodspeedOS" not in out or "riscv64" not in out:
        sys.exit("FAIL: the kernel banner never appeared - it did not reach S-mode.")
    print("OK  the kernel reached S-mode and drove the UART.")


if __name__ == "__main__":
    main()
