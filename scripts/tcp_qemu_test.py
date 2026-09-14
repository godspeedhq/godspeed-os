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

# The multi-segment reply, defined ONCE. Restating its length as a literal is how this test spent a
# run reporting a 4-byte shortfall that was its own arithmetic: 16 x 180 is 2880, and "BIG:" makes
# 2884, not the 2888 the assertion claimed. Derive, do not restate.
BIG_REPLY = b"BIG:" + (b"0123456789abcdef" * 180)


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
                    # A payload asking for BIG gets a reply that cannot fit in one segment, so the
                    # guest's receive path has to reassemble across segments, advance its window as
                    # the arena drains, and acknowledge each one. A single-segment echo proves none
                    # of that.
                    if b"big" in data:
                        c.sendall(BIG_REPLY)
                    else:
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
        # The OPTIONS, not just their length. A stack can advertise a maximum segment size or fail to,
        # and from the outside those look identical in every other field - so the only way to check it
        # is to read the bytes off the wire.
        opts = bytes(pkt[t + 20:t + doff])
        segs.append((flags, seq, ackn, plen, opts))
    return segs, None


def mss_option(opts):
    """The MSS from a TCP option field, or None. Walks the list, as a receiver must."""
    i = 0
    while i < len(opts):
        kind = opts[i]
        if kind == 0:                      # End of Option List
            return None
        if kind == 1:                      # No-Operation
            i += 1
            continue
        if i + 1 >= len(opts):
            return None
        ln = opts[i + 1]
        if ln < 2 or i + ln > len(opts):
            return None
        if kind == 2 and ln == 4:
            return struct.unpack(">H", opts[i + 2:i + 4])[0]
        i += ln
    return None


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
    out = bytearray()

    def pump(seconds):
        """Read for `seconds`, appending to `out`. Returns what arrived in this window."""
        before = len(out)
        stop = time.time() + seconds
        while time.time() < stop:
            try:
                chunk = sock.recv(4096)
                if not chunk:
                    break
                out.extend(chunk)
            except socket.timeout:
                pass
        return bytes(out[before:])

    def wait_for(needle, seconds):
        stop = time.time() + seconds
        while time.time() < stop:
            if needle in out:
                return True
            pump(0.5)
        return needle in out

    def run(cmd, seconds):
        """Send one shell command and read until it has clearly finished.

        Reading until the NEXT prompt rather than until a keyword: a command that fails prints
        something this harness did not predict, and waiting for a keyword would then sit until its
        deadline and report the wrong thing. The prompt is the one marker every outcome shares.
        """
        mark = len(out)
        sock.sendall((cmd + "\r\n").encode())
        stop = time.time() + seconds
        while time.time() < stop:
            pump(0.5)
            after = bytes(out[mark:])
            # The echoed command line ends with a prompt of its own, so look for a prompt AFTER
            # some output has followed it.
            if after.count(b"gsh>") >= 1 and len(after) > len(cmd) + 12:
                pump(1.0)
                break
        return bytes(out[mark:])

    # The network must be configured first, or a TCP failure would really be a DHCP failure.
    # WAIT FOR THE DANCE TO FINISH, not for the lease to be offered. `10.0.2.15` appears in the DHCP
    # OFFER, which is early: ARP and the ICMP check still follow, net-stack is single-threaded, and a
    # request issued in that window finds it busy. The ICMP reply is the line that ends the dance, so
    # it is the only honest "ready" marker. Getting this wrong reported a total TCP failure - zero
    # connections, zero segments - for a stack that was working.
    ok_boot = (wait_for(b"gsh>", 180)
               and wait_for(b"ICMP - 10.0.2.2 echo reply", 150))
    time.sleep(3.0)

    r1 = run("tcp 10.0.2.2 %d %s" % (echo_port, PAYLOAD.decode()), 45)
    time.sleep(1.0)
    r2 = run("tcp 10.0.2.2 %d big" % echo_port, 45)
    out_b = bytes(out)

    try:
        sock.close()
    finally:
        child.terminate()
        try:
            child.wait(timeout=10)
        except subprocess.TimeoutExpired:
            child.kill()
        echo.stop()

    text = out_b.decode("utf-8", "replace")
    tail = "\n".join(text.splitlines()[-25:])
    print("---- guest tail ----\n%s\n--------------------" % tail)

    ok = True

    def check(cond, what):
        nonlocal ok
        print("  %-4s %s" % ("OK" if cond else "FAIL", what))
        if not cond:
            ok = False

    # ---- second scenario: a reply that spans several segments -----------------------------------
    #
    # DIAGNOSE A WRONG IMAGE BEFORE REPORTING SEVENTEEN FAILURES.
    #
    # `build/os.img` is written by several osdev subcommands and the last one wins. `osdev test
    # identity` writes an IDENTITY-ONLY image, which boots perfectly and has no shell - so this
    # script then reports every assertion failing, including ones about the wire, and none of them
    # says why. It has already cost one debugging session on this branch. The file's existence is not
    # the question; which build wrote it is, and the guest answers that by reaching a prompt or not.
    if not ok_boot:
        print("tcp-qemu: the guest never reached a shell prompt.")
        print("tcp-qemu: build/os.img is written by SEVERAL osdev commands and the last one wins -")
        print("tcp-qemu:   `osdev test identity` writes an identity-only image, which has no shell.")
        print("tcp-qemu: run `cargo run -p osdev --release -- test shell` (which writes the full")
        print("tcp-qemu:   image), then this script, with nothing in between.")
        print("")
    print("tcp-qemu: guest side")
    check(ok_boot, "the guest booted, reached a prompt and took a DHCP lease")
    check(b"echo:" + PAYLOAD in r1, "single segment: the guest printed the echo the host sent back")
    check(b"BIG:" in r2, "multi-segment: the guest printed the large reply")
    import re as _re
    m2 = _re.search(rb"tcp: (\d+) byte\(s\) back", r2)
    got2 = int(m2.group(1)) if m2 else -1
    # SAY THE NUMBER. "did not equal 2888" sends the reader back to the guest to find out what it
    # was; printing it turns a failed assertion into a measurement.
    check(got2 == len(BIG_REPLY),
          "multi-segment: all %d bytes arrived, reassembled in order (guest reported %d)"
          % (len(BIG_REPLY), got2))
    print("tcp-qemu: host side")
    check(echo.connections >= 2, "the host accepted BOTH connections (%d)" % echo.connections)
    check(PAYLOAD in echo.got, "the host received exactly the payload the guest was told to send")
    check(b"big" in echo.got, "the host received the second request too")

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
        check(len(syn) >= 2, "a SYN opened each connection (%d)" % len(syn))
        check(len(synack) >= 2, "the peer's SYN-ACK for each (%d)" % len(synack))
        check(len(data) >= 5, "data segments in both directions (%d)" % len(data))
        # The large reply cannot fit in one segment, so several MUST carry a full MSS. If this fails
        # while the byte count passed, the reply came as one jumbo frame and the reassembly path
        # was never exercised - a green test that proved nothing, which is the thing to catch.
        big = [x for x in data if x[3] > 1000]
        check(len(big) >= 2, "the large reply really did span several segments (%d over 1000 bytes)"
              % len(big))
        check(len(fins) >= 3, "FINs from both sides across both connections (%d)" % len(fins))
        check(len(rsts) == 0, "no RST anywhere (%d)" % len(rsts))

        # OUR SYN MUST ADVERTISE A MAXIMUM SEGMENT SIZE.
        #
        # Checked on the wire rather than taken from the guest's word for it, because the failure
        # this guards is entirely invisible from inside: a peer that receives no MSS option must
        # assume 536 (RFC 1122 4.2.2.6), so a silent stack still WORKS - it just gets talked to in
        # 536-byte pieces forever, on every connection, with nothing anywhere reporting it. That is
        # the shape of bug this whole second instrument exists for.
        ours = [mss_option(x[4]) for x in syn]
        # `ours` NON-EMPTY, explicitly. `all()` over an empty list is True, so without this the three
        # checks below would report OK on a capture containing no SYN at all - an instrument printing
        # a pass it did not earn, which is the one failure mode a second instrument must not have.
        check(len(ours) >= 2 and all(m is not None for m in ours),
              "our SYN advertises a maximum segment size (%s)" % ours)
        check(len(ours) >= 2 and all(m == 1460 for m in ours),
              "the advertised MSS is the ethernet 1460, not a smaller guess (%s)" % ours)
        # And the option field must still PARSE as a whole: a data offset that disagrees with the
        # bytes after it is the classic way to get an option wrong, and it would be read by the peer
        # as a corrupt header rather than as our MSS.
        check(len(syn) >= 2 and all(len(x[4]) == 4 for x in syn),
              "the SYN option field is exactly the 4-byte MSS option (%s)"
              % [len(x[4]) for x in syn])
        # The peer's, if it sent one: not asserted as a value, because it is the peer's business, but
        # reported so a run that negotiated something unexpected says so instead of looking normal.
        theirs = [mss_option(x[4]) for x in synack]
        print("tcp-qemu:   (the peer offered %s)" % theirs)
        if syn and synack:
            check(synack[0][2] == (syn[0][1] + 1) & 0xFFFFFFFF,
                  "the peer acknowledged our ISS+1 (handshake sequencing is correct)")

    print("\ntcp-qemu: %s" % ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
