#!/usr/bin/env python3
"""Prove the guest ANSWERS a connection it did not start - the passive open, end to end.

Every other networking test on this branch has the guest dialling OUT. This one connects IN, which
is the half `listen`/`accept` exists for and the half nothing could exercise until now.

The trick is one QEMU argument. SLIRP gives the host no route to the guest by default, which is why
`ping <guest>` cannot work under QEMU and why the poll step's ARP and ICMP answering had to be proven
on hardware. But `hostfwd` forwards a host TCP port straight to a guest port, and that is enough to
drive a passive open: the guest listens, this script connects from outside the guest entirely, and
the bytes that come back are the guest's own echo.

Checked on BOTH sides, the same discipline as `tcp_qemu_test.py`: what the guest printed to its
serial console, and what this script actually received over the socket. Either alone is weaker - the
guest can claim to have echoed something this never saw.
"""

import os
import re
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGE = os.path.join(ROOT, "build", "os.img")

GUEST_PORT = 8080
PROBE = b"knock knock"
PROBE2 = b"second run"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def read_until(sock, want, secs, sink):
    """Accumulate serial output until `want` appears, or `secs` elapse."""
    end = time.time() + secs
    sock.settimeout(0.5)
    while time.time() < end:
        try:
            b = sock.recv(4096)
        except socket.timeout:
            continue
        except OSError:
            break
        if not b:
            break
        sink.append(b.decode("utf-8", "replace"))
        if want and want in "".join(sink):
            return True
    return False


def main():
    if not os.path.exists(IMAGE):
        print("tcp-serve: no %s - run `cargo run -p osdev --release -- test shell` first" % IMAGE)
        return 1

    ser_port = free_port()
    fwd_port = free_port()
    qemu = os.environ.get("QEMU", "qemu-system-x86_64")
    args = [
        qemu,
        "-drive", "format=raw,file=%s,if=ide" % IMAGE,
        "-smp", "2", "-m", "512M",
        "-serial", "tcp::%d,server" % ser_port,
        "-serial", "null",
        "-device", "e1000,netdev=n0",
        # THE WHOLE POINT: a host port wired to a guest port, so a connection can come IN.
        "-netdev", "user,id=n0,hostfwd=tcp::%d-:%d" % (fwd_port, GUEST_PORT),
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]
    print("tcp-serve: guest port %d forwarded from host port %d, serial on %d"
          % (GUEST_PORT, fwd_port, ser_port))
    child = subprocess.Popen(args, stdin=subprocess.DEVNULL,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    sock = None
    for _ in range(100):
        try:
            sock = socket.create_connection(("127.0.0.1", ser_port), timeout=2)
            break
        except OSError:
            time.sleep(0.2)
    if sock is None:
        child.kill()
        print("tcp-serve: could not attach to the guest serial port")
        return 1

    out = []
    ok = True

    def check(cond, what):
        nonlocal ok
        print("  %-4s %s" % ("OK" if cond else "FAIL", what))
        if not cond:
            ok = False

    try:
        # Wait for a prompt, then for the stack to take its DHCP lease - `serve` needs a configured
        # stack, and asking before it is up is a different failure from the one being tested.
        booted = read_until(sock, "gsh>", 90, out)
        read_until(sock, "is ours", 40, out)
        time.sleep(1.0)

        sock.sendall(b"serve %d\n" % GUEST_PORT)
        # "listening on <ip>:<port>" when the stack knows its address, "listening on port <n>"
        # when it does not - match the part common to both rather than one spelling of it.
        listening = read_until(sock, "listening on", 20, out)

        # Connect from OUTSIDE the guest and speak first, which is what a client does.
        got = b""
        connected = False
        try:
            c = socket.create_connection(("127.0.0.1", fwd_port), timeout=20)
            connected = True
            c.sendall(PROBE)
            c.settimeout(20)
            while len(got) < len(PROBE):
                b = c.recv(256)
                if not b:
                    break
                got += b
            c.close()
        except OSError as e:
            print("tcp-serve: host-side connection failed: %s" % e)

        read_until(sock, "closed", 20, out)

        # RUN IT AGAIN ON THE SAME PORT. The listener has to have been released, and it is not
        # released by the client dropping its capability - net-stack's listener table is its own
        # state. Found on a Pi 2, where the second `serve 8080` was refused and stayed refused.
        # MAX_LISTEN is small, so this leak is two runs deep.
        out.append("\n---- second run, same port ----\n")
        # A DURATION, not an unbounded wait. `serve <port>` now waits for `q`, and driving that over
        # a serial socket is a race the test does not need to run: the keystroke has to land while
        # the accept loop is between polls, and it flaked PASS/FAIL on consecutive runs. A short
        # bound ends the command deterministically and tests the same thing - that the port could be
        # listened on again at all.
        sock.sendall(b"serve %d 30s\n" % GUEST_PORT)
        again = read_until(sock, "listening on", 20, out)

        # AND CONNECT AGAIN, which is the case hardware broke on. The first connection worked and
        # the second was accepted, received its bytes, and then timed out trying to echo them:
        # net-stack was inside a poll step that outlasted the client's five-second patience. A
        # listener that can be re-created is not the same as a session that works twice.
        got2 = b""
        try:
            c2 = socket.create_connection(("127.0.0.1", fwd_port), timeout=20)
            c2.sendall(PROBE2)
            c2.settimeout(20)
            while len(got2) < len(PROBE2):
                b = c2.recv(256)
                if not b:
                    break
                got2 += b
            c2.close()
        except OSError as e:
            print("tcp-serve: second host-side connection failed: %s" % e)
        read_until(sock, "closed", 25, out)
        text = "".join(out)

        print("---- guest tail ----")
        print("\n".join(text.strip().splitlines()[-40:]))
        print("--------------------")

        print("tcp-serve: guest side")
        check(booted, "the guest booted and reached a prompt")
        check(listening, "the guest reported it is listening")
        check("accepted a connection" in text,
              "the guest ACCEPTED a connection it did not start (passive open)")
        m = re.search(r"received (\d+) byte\(s\)", text)
        check(m is not None and int(m.group(1)) == len(PROBE),
              "the guest received the probe intact (%s)" % (m.group(1) if m else "nothing"))
        check(re.search(r"echoed (\d+) byte\(s\) back", text) is not None,
              "the guest echoed it back")
        check("tcp selftest PASS" in text, "the startup self-test passed")
        check(again and "would not listen" not in text.split("second run")[-1],
              "the SAME port can be listened on again - the listener was released, not leaked")

        print("tcp-serve: host side, decoded outside the guest")
        check(connected, "the host could connect to the guest's listening port")
        check(got == PROBE,
              "the bytes that came back are the bytes sent (%r)" % got[:40])
        check(got2 == PROBE2,
              "a SECOND connection in the same session also echoed (%r)" % got2[:40])
    finally:
        try:
            sock.close()
        except OSError:
            pass
        child.kill()
        child.wait(timeout=10)

    print()
    print("tcp-serve: %s" % ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
