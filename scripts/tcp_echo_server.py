#!/usr/bin/env python3
"""A host-side TCP peer for HARDWARE tests, where QEMU's SLIRP is not in the picture.

`scripts/tcp_qemu_test.py` carries its own echo server because it also drives QEMU. On real hardware
the board is on the LAN and the peer has to be reachable from it, so this runs standalone and prints
the address to type on the board.

    python scripts/tcp_echo_server.py [port]

Answers any payload with `echo:<payload>`, except one containing `big`, which gets a reply too large
for one segment - so a board can exercise reassembly and window updates, the same two things the QEMU
test found bugs in. Every connection is logged with what arrived and what was sent, because the point
of a hardware test is to see both ends and compare.
"""

import socket
import sys
import threading

BIG_REPLY = b"BIG:" + (b"0123456789abcdef" * 180)


def lan_addrs():
    """Every address this machine might be reachable at from another box on the LAN.

    A board cannot reach 127.0.0.1, and printing it would send someone to type an address that
    cannot work. So loopback is excluded and the rest are offered rather than guessed between.
    """
    out = []
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.connect(("8.8.8.8", 53))          # no traffic; just picks the default route's source
        out.append(s.getsockname()[0])
        s.close()
    except OSError:
        pass
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            ip = info[4][0]
            if not ip.startswith("127.") and ip not in out:
                out.append(ip)
    except OSError:
        pass
    return out


def serve(conn, addr, n):
    try:
        conn.settimeout(15.0)
        data = conn.recv(4096)
        print("  [%d] from %s:%d - %d byte(s): %r" % (n, addr[0], addr[1], len(data), data[:64]))
        if b"big" in data:
            reply = BIG_REPLY
        else:
            reply = b"echo:" + data
        conn.sendall(reply)
        print("  [%d] sent %d byte(s)%s" % (n, len(reply),
                                            " (multi-segment)" if len(reply) > 1460 else ""))
        conn.shutdown(socket.SHUT_WR)       # our FIN, so the board must complete the exchange
        try:
            conn.recv(64)
        except OSError:
            pass
    except OSError as e:
        print("  [%d] %s" % (n, e))
    finally:
        conn.close()
        print("  [%d] closed" % n)


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 7777
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
    srv.listen(8)
    addrs = lan_addrs()
    print("tcp-echo: listening on 0.0.0.0:%d" % port)
    if addrs:
        print("tcp-echo: on the board, type one of:")
        for a in addrs:
            print("             tcp %s %d hello" % (a, port))
            print("             tcp %s %d big" % (a, port))
    else:
        print("tcp-echo: could not work out this machine's LAN address - find it and use that")
    print("tcp-echo: if nothing arrives, the firewall is the first thing to check")
    n = 0
    try:
        while True:
            conn, addr = srv.accept()
            n += 1
            threading.Thread(target=serve, args=(conn, addr, n), daemon=True).start()
    except KeyboardInterrupt:
        print("\ntcp-echo: stopped after %d connection(s)" % n)
    finally:
        srv.close()


if __name__ == "__main__":
    main()
