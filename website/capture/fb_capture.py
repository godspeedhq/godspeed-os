"""Boot os.img headless in QEMU, drive the shell over COM1 serial (where the shell
polls RX), and screendump the framebuffer at chosen states. This is how the
observe/chaos/shell/drives gallery images are made.

Usage:
    python fb_capture.py [state ...]      # default: observe chaos
    states: observe | chaos | shell | drives

`drives` needs a formatted GSFS disk on the AHCI controller; set GALLERY_DISK to a
disk image path (a throwaway copy of a persist image) and it is attached as an
ich9-ahci data disk. ROOT is derived from this file's location; QEMU/OVMF paths are
the Windows dev defaults (override via the QEMU/OVMF env vars)."""
import socket, subprocess, time, os, sys, struct, zlib, re

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OVMF = os.environ.get("OVMF", r"C:\Program Files\qemu\share\edk2-x86_64-code.fd")
QEMU = os.environ.get("QEMU", r"C:\Program Files\qemu\qemu-system-x86_64.exe")
DISK = os.environ.get("GALLERY_DISK", "")
IMG  = os.path.join(ROOT, "build", "os.img")
OUT  = os.path.join(ROOT, "build")
MON_PORT, SER_PORT = 55560, 55561
WANT = sys.argv[1:] or ["observe", "chaos"]


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


# Boot disk on legacy IDE + (optionally) a data disk ALONE on ich9-ahci so the
# AHCI block-driver targets it on port 0, exactly like osdev's storage tests.
boot_drive = f"format=raw,file={IMG}"
disk_args = []
if DISK:
    boot_drive = f"format=raw,file={IMG},if=ide"
    disk_args = ["-device", "ich9-ahci,id=ahci",
                 "-drive", f"id=data,format=raw,file={DISK},if=none",
                 "-device", "ide-hd,drive=data,bus=ahci.0"]

args = [QEMU,
    "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF}",
    "-drive", boot_drive, *disk_args,
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

    def cap_observe():
        print("cmd: observe")
        send_cmd("observe"); print("  serial:", repr(drain(4).strip()[-160:]))
        screendump(mon, "observe")
        ser.send(b"q"); time.sleep(1.0); ser.send(b"\r"); drain(2)

    def cap_chaos():
        print("cmd: chaos max-carnage all-services")
        send_cmd("chaos max-carnage all-services"); drain(2)
        ser.send(b"y\r"); print("  serial:", repr(drain(14).strip()[-220:]))
        screendump(mon, "chaos")
        ser.send(b"q"); time.sleep(2.0); drain(2)

    def cap_shell():
        print("cmd: shell session")
        send_cmd("clear"); drain(1)
        for c in ["date", "cores", "uptime", "caps"]:
            send_cmd(c); print(f"  {c}:", repr(drain(2).strip()[-120:]))
        screendump(mon, "shell")

    def cap_drives():
        print("cmd: drives")
        send_cmd("clear"); drain(1)
        send_cmd("drives"); print("  serial:", repr(drain(3).strip()[-260:]))
        screendump(mon, "drives")

    def cap_edit():
        print("cmd: edit /notes.txt")
        send_cmd("clear"); drain(1)
        send_cmd("edit /notes.txt"); print("  open:", repr(drain(2).strip()[-140:]))
        lines = [
            "GodspeedOS notes",
            "",
            "edit: a full-screen editor.",
            "Files of any size open windowed.",
            "The original stays on disk.",
        ]
        # Type slower than the guest's per-key full-screen redraw (slow under TCG),
        # else the UART FIFO overflows and characters scramble.
        for i, ln in enumerate(lines):
            for ch in ln:
                ser.send(ch.encode()); time.sleep(0.12)
            if i != len(lines) - 1:
                ser.send(b"\r"); time.sleep(0.35)   # Enter splits the line (new line)
        drain(4)
        screendump(mon, "edit")
        ser.send(b"\x11"); time.sleep(0.4); ser.send(b"d")  # Ctrl-Q -> discard, to exit clean

    steps = {"observe": cap_observe, "chaos": cap_chaos, "shell": cap_shell,
             "drives": cap_drives, "edit": cap_edit}
    for name in WANT:
        fn = steps.get(name)
        if fn is None:
            print(f"WARN: unknown state '{name}'"); continue
        if name == "drives" and not DISK:
            print("SKIP drives: set GALLERY_DISK to a formatted GSFS image"); continue
        fn()

    mon.send(b"quit\n"); time.sleep(0.5)
finally:
    p.terminate()
    try: p.wait(timeout=5)
    except Exception: p.kill()
print("done")
