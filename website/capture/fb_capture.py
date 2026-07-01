"""Boot os.img headless in QEMU, drive the shell over COM1 serial (where the shell
polls RX), and screendump the framebuffer at chosen states (observe, chaos, ...).
Same PPM->PNG conversion as fb_shot.py. This is how the observe/chaos gallery
images are made. ROOT is derived from this file's location; QEMU/OVMF paths are the
Windows dev defaults (override via the QEMU/OVMF env vars)."""
import socket, subprocess, time, os, struct, zlib, re

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OVMF = os.environ.get("OVMF", r"C:\Program Files\qemu\share\edk2-x86_64-code.fd")
QEMU = os.environ.get("QEMU", r"C:\Program Files\qemu\qemu-system-x86_64.exe")
IMG  = os.path.join(ROOT, "build", "os.img")
OUT  = os.path.join(ROOT, "build")
MON_PORT, SER_PORT = 55560, 55561


def ppm_to_png(ppm_local, png_local):
    data = open(ppm_local, "rb").read()
    assert data[:2] == b"P6", "not a P6 PPM"
    idx, vals = 2, []
    while len(vals) < 3:
        while idx < len(data) and data[idx:idx + 1].isspace(): idx += 1
        if data[idx:idx + 1] == b"#":
            while idx < len(data) and data[idx:idx + 1] != b"\n": idx += 1
            continue
        st = idx
        while idx < len(data) and not data[idx:idx + 1].isspace(): idx += 1
        vals.append(int(data[st:idx]))
    w, h, _ = vals; idx += 1
    pix = data[idx:idx + w * h * 3]

    def chunk(typ, d):
        c = typ + d
        return struct.pack(">I", len(d)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)

    stride = w * 3
    raw = bytearray()
    for y in range(h):
        raw.append(0); raw.extend(pix[y * stride:(y + 1) * stride])
    png = (b"\x89PNG\r\n\x1a\n"
           + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
           + chunk(b"IEND", b""))
    open(png_local, "wb").write(png)
    return w, h


args = [QEMU,
    "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF}",
    "-drive", f"format=raw,file={IMG}",
    "-vga", "std", "-display", "none",
    "-device", "qemu-xhci", "-device", "usb-kbd",
    "-monitor", f"tcp:127.0.0.1:{MON_PORT},server,nowait",
    "-serial", f"tcp:127.0.0.1:{SER_PORT},server,nowait",
    "-smp", "4", "-m", "512", "-no-reboot",
]
p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def mon_connect():
    s = socket.create_connection(("127.0.0.1", MON_PORT), timeout=10)
    s.settimeout(3); time.sleep(0.3)
    try: s.recv(4096)
    except Exception: pass
    return s


def screendump(mon, name):
    ppm = os.path.join(OUT, f"{name}.ppm")
    png = os.path.join(OUT, f"{name}.png")
    for f in (ppm, png):
        if os.path.exists(f):
            try: os.remove(f)
            except Exception: pass
    mon.send(f"screendump {ppm.replace(chr(92), '/')}\n".encode())
    time.sleep(2.5)
    if not os.path.exists(ppm):
        print(f"  NO PPM for {name}"); return
    w, h = ppm_to_png(ppm, png)
    print(f"  captured {name}: {w}x{h} -> {png}")


try:
    time.sleep(2)
    ser = socket.create_connection(("127.0.0.1", SER_PORT), timeout=10)
    ser.settimeout(1)

    def drain(secs):
        end = time.time() + secs; buf = b""
        while time.time() < end:
            try: buf += ser.recv(4096)
            except Exception: pass
        return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", buf.decode(errors="replace"))

    def send_cmd(s):
        for ch in s:
            ser.send(ch.encode()); time.sleep(0.04)
        ser.send(b"\r")

    # wait for shell ready
    buf = b""; end = time.time() + 55; ready = False
    while time.time() < end:
        try: buf += ser.recv(4096)
        except Exception: pass
        if b"shell: ready" in buf:
            ready = True; break
    print("shell ready" if ready else "WARN: shell-ready not seen; proceeding")
    time.sleep(1)
    mon = mon_connect()

    # --- observe ---
    print("cmd: observe")
    send_cmd("observe")
    print("  serial:", repr(drain(4).strip()[-200:]))
    screendump(mon, "observe")
    ser.send(b"q"); time.sleep(1.0); ser.send(b"\r")    # exit observe -> prompt
    print("  after-q:", repr(drain(2).strip()[-160:]))

    # --- chaos max-carnage (guarded by a [y/N] confirm) ---
    print("cmd: chaos max-carnage all-services")
    send_cmd("chaos max-carnage all-services")
    print("  prompt:", repr(drain(2).strip()[-140:]))
    ser.send(b"y\r")                                     # confirm the [y/N] gate
    print("  serial:", repr(drain(14).strip()[-300:]))  # let several rounds run
    screendump(mon, "chaos")
    ser.send(b"q"); time.sleep(2.0)                      # abort the storm (serial q)
    print("  after-q:", repr(drain(2).strip()[-160:]))

    mon.send(b"quit\n"); time.sleep(0.5)
finally:
    p.terminate()
    try: p.wait(timeout=5)
    except Exception: p.kill()
print("done")
