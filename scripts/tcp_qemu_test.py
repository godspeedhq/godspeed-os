#!/usr/bin/env python3
"""End-to-end TCP test in QEMU, verified against the WIRE and not against the stack's own report.

Boots `build/os.img` in QEMU with an e1000 on a user-mode (SLIRP) backend, runs the shell's `tcp`
command against a real echo server on the host, and then decodes `build/net-tx.pcap` to check that the
segments the protocol requires actually went out.

TWO INDEPENDENT INSTRUMENTS, deliberately. The guest's own output can only tell you what the guest
believes; a stack that thinks it sent a segment it never sent agrees with itself perfectly. The pcap is
written by QEMU's `filter-dump` on the NIC backend, outside the guest entirely, so it is the one
witness that cannot be fooled by a bug in the thing under test. This project has been caught twice this
month by an instrument that reported a clean pass while reading nothing, so the assertions below check
that the pcap contains what it must AND that it was read at all.

SLIRP routes the guest's outbound TCP, and the host is reachable from the guest at 10.0.2.2 - so the
peer is a real TCP implementation with real ACK timing and a real window, not a mock that agrees with
us.

Exit: 0 if every assertion holds, 1 otherwise.
"""

import os
import socket
import struct
import subprocess
import sys
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
IMAGE = os.path.join(ROOT, "build", "os.img")
PCAP = os.path.join(ROOT, "build", "net-tx.pcap")
PAYLOAD = b"godspeed-tcp-probe"

FIN, SYN, RST, PSH, ACK = 0x01, 0x02, 0x04, 0x08, 0x10


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Echo(threading.Thread):
    """A real TCP peer: accept, echo what arrives, close when the client closes."""

    def __init__(self, port):
        super().__init__(daemon=True)
        self.port = port
        self.got = b""
        self.connections = 0
        self._stop = False

    def run(self):
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("0.0.0.0", self.port))
        srv.listen(4)
        srv.settimeout(1.0)
        while not self._stop:
            try:
                c, _ = srv.accept()
            except socket.timeout:
                continue
            self.connections += 1
            c.settimeout(5.0)
            try:
                data = c.recv(4096)
                if data:
                    self.got += data
                    c.sendall(b"echo:" + data)
                # Close from this side so the guest sees a FIN and must complete the exchange.
                c.shutdown(socket.SHUT_WR)
                try:
                    c.recv(64)
                except Exception:
                    pass
            except Exception:
                pass
            finally:
                c.close()
        srv.close()

    def stop(self):
        self._stop = True


def decode_pcap(path):
    """Return the list of (flags, seq, ack, payload_len) for every TCP segment in the dump.

    Written out rather than using a library so the test has no dependency the repository does not
    already have, and so a malformed dump is a visible failure here rather than an exception from
    somewhere else.
    """
    with open(path, "rb") as f:
        blob = f.read()
    if len(blob) < 24:
        return None, "pcap is shorter than its own global header"
    magic = struct.unpack("<I", blob[:4])[0]
    if magic == 0xA1B2C3D4:
        endian, nano = "<", False
    elif magic == 0xD4C3B2A1:
        endian, nano = ">", False
    elif magic == 0xA1B23C4D:
        endian, nano = "<", True
    else:
        return None, "unrecognised pcap magic 0x%08x" % magic
    _ = nano

    segs = []
    off = 24
    while off + 16 <= len(blob):
        _ts, _us, caplen, _orig = struct.unpack(endian + "IIII", blob[off:off + 16])
        off += 16
        pkt = blob[off:off + caplen]
        off += caplen
        if len(pkt) < 34:
            continue
        if pkt[12] != 0x08 or pkt[13] != 0x00:
            continue                                    # not IPv4
        ihl = (pkt[14] & 0x0F) * 4
        if ihl < 20 or len(pkt) < 14 + ihl + 20:
            continue
        if pkt[14 + 9] != 6:
            continue                                    # not TCP
        ip_total = struct.unpack(">H", pkt[16:18])[0]
        t = 14 + ihl
        doff = (pkt[t + 12] >> 4) * 4
        seq, ackn = struct.unpack(">II", pkt[t + 4:t + 12])
        flags = pkt[t + 13]
        plen = max(0, (14 + ip_total) - (t + doff))
        segs.append((flags, seq, ackn, plen))
    return segs, None


def main():
    if not os.path.exists(IMAGE):
        print("tcp-qemu: no %s - run `cargo run -p osdev -- image` first" % IMAGE)
        return 1
    if os.path.exists(PCAP):
        os.remove(PCAP)                                 # never assert against a previous run's dump

    echo_port = free_port()
    echo = Echo(echo_port)
    echo.start()
    time.sleep(0.3)

    ser_port = free_port()
    qemu = os.environ.get("QEMU", "qemu-system-x86_64")
    args = [
        qemu,
        "-drive", "format=raw,file=%s,if=ide" % IMAGE,
        "-smp", "2", "-m", "512M",
        "-serial", "tcp::%d,server" % ser_port,
        "-serial", "null",
        "-device", "e1000,netdev=n0",
        "-netdev", "user,id=n0",
        "-object", "filter-dump,id=nicdump,netdev=n0,file=%s" % PCAP,
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]
    print("tcp-qemu: echo server on host port %d, serial on %d" % (echo_port, ser_port))
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
        echo.stop()
        print("tcp-qemu: could not attach to the guest serial port")
        return 1

    sock.settimeout(1.0)
    out = b""
    deadline = time.time() + 180
    sent = False
    while time.time() < deadline:
        try:
            chunk = sock.recv(4096)
            if not chunk:
                break
            out += chunk
        except socket.timeout:
            pass
        # Wait for the network to be configured before asking for a connection, so a failure is
        # about TCP rather than about DHCP not having finished.
        if not sent and (b"lease" in out or b"net-stack: serving" in out) and b"gsh>" in out:
            time.sleep(2.0)
            cmd = "tcp 10.0.2.2 %d %s\r\n" % (echo_port, PAYLOAD.decode())
            sock.sendall(cmd.encode())
            sent = True
            deadline = time.time() + 60
        if sent and (b"byte(s) back" in out or b"connected to nothing" in out):
            # DRAIN, do not just sleep. The byte count is printed before the content line, so
            # breaking on the count and closing the socket loses the very thing being asserted - the
            # first run of this harness reported a stack failure that was entirely its own.
            end = time.time() + 2.0
            while time.time() < end:
                try:
                    chunk = sock.recv(4096)
                    if not chunk:
                        break
                    out += chunk
                except socket.timeout:
                    pass
            break

    try:
        sock.close()
    finally:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
        echo.stop()

    text = out.decode("utf-8", "replace")
    tail = "\n".join(text.splitlines()[-25:])
    print("---- guest tail ----\n%s\n--------------------" % tail)

    ok = True

    def check(cond, what):
        nonlocal ok
        print("  %-4s %s" % ("OK" if cond else "FAIL", what))
        if not cond:
            ok = False

    print("tcp-qemu: guest side")
    check(sent, "the shell reached a prompt and the command was issued")
    check(b"echo:" + PAYLOAD in out, "the guest printed the echo the host sent back")
    print("tcp-qemu: host side")
    check(echo.connections >= 1, "the host echo server accepted a connection")
    check(PAYLOAD in echo.got, "the host received exactly the payload the guest was told to send")

    print("tcp-qemu: the wire (build/net-tx.pcap), decoded independently of the guest")
    segs, err = (None, "pcap missing") if not os.path.exists(PCAP) else decode_pcap(PCAP)
    if err:
        check(False, "pcap readable: %s" % err)
    else:
        # A dump that decodes to nothing must FAIL, not pass quietly: an instrument reading zero is
        # indistinguishable from a system doing nothing wrong, and the flattering reading is the
        # wrong one.
        check(len(segs) > 0, "the dump contains TCP segments at all (%d)" % len(segs))
        syn = [s for s in segs if s[0] & SYN and not s[0] & ACK]
        synack = [s for s in segs if s[0] & SYN and s[0] & ACK]
        data = [s for s in segs if s[3] > 0]
        fins = [s for s in segs if s[0] & FIN]
        rsts = [s for s in segs if s[0] & RST]
        check(len(syn) >= 1, "a SYN opened the connection (%d)" % len(syn))
        check(len(synack) >= 1, "the peer's SYN-ACK is on the wire (%d)" % len(synack))
        check(len(data) >= 2, "data segments in both directions (%d)" % len(data))
        check(len(fins) >= 2, "both sides sent FIN (%d)" % len(fins))
        check(len(rsts) == 0, "no RST anywhere (%d)" % len(rsts))
        if syn and synack:
            check(synack[0][2] == (syn[0][1] + 1) & 0xFFFFFFFF,
                  "the peer acknowledged our ISS+1 (handshake sequencing is correct)")

    print("\ntcp-qemu: %s" % ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
