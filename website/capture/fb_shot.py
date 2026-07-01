"""Boot os.img headless in QEMU with an emulated GPU, let it boot, capture the
framebuffer via HMP screendump (PPM), and convert to PNG with stdlib zlib. Used
to make the gallery's boot frame. ROOT is derived from this file's location, so
it works on any checkout; QEMU/OVMF paths are the Windows dev defaults (override
below if yours differ)."""
import socket, subprocess, time, os, struct, zlib, re

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
OVMF = os.environ.get("OVMF", r"C:\Program Files\qemu\share\edk2-x86_64-code.fd")
QEMU = os.environ.get("QEMU", r"C:\Program Files\qemu\qemu-system-x86_64.exe")
IMG  = os.path.join(ROOT, "build", "os.img")
PPM  = os.path.join(ROOT, "build", "fb_shot.ppm")
PNG  = os.path.join(ROOT, "build", "fb_shot.png")
PPM_FWD = PPM.replace("\\", "/")
PORT = 55560

for f in (PPM, PNG):
    if os.path.exists(f):
        os.remove(f)

args = [QEMU,
    "-drive", f"if=pflash,format=raw,readonly=on,file={OVMF}",
    "-drive", f"format=raw,file={IMG}",
    "-vga", "std", "-display", "none",
    "-device", "qemu-xhci", "-device", "usb-kbd",
    "-monitor", f"tcp:127.0.0.1:{PORT},server,nowait",
    "-smp", "4", "-m", "512", "-no-reboot",
]
p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    time.sleep(25)
    s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
    s.settimeout(3)
    time.sleep(0.5)
    try: s.recv(4096)
    except Exception: pass
    s.send(f"screendump {PPM_FWD}\n".encode())
    end = time.time() + 4
    buf = b""
    while time.time() < end:
        try: buf += s.recv(4096)
        except Exception: break
    txt = re.sub(r"\x1b\[[0-9;]*[A-Za-z]", "", buf.decode(errors="replace")).replace("\x08", "")
    print("monitor:", txt.strip()[-300:])
    s.send(b"quit\n")
    time.sleep(0.5)
finally:
    p.terminate()
    try: p.wait(timeout=5)
    except Exception: p.kill()

time.sleep(0.5)
if not os.path.exists(PPM):
    print("NO PPM produced"); raise SystemExit(1)

data = open(PPM, "rb").read()
assert data[:2] == b"P6", "not a P6 PPM"
idx, vals = 2, []
while len(vals) < 3:
    while idx < len(data) and data[idx:idx+1].isspace(): idx += 1
    if data[idx:idx+1] == b"#":
        while idx < len(data) and data[idx:idx+1] != b"\n": idx += 1
        continue
    st = idx
    while idx < len(data) and not data[idx:idx+1].isspace(): idx += 1
    vals.append(int(data[st:idx]))
w, h, _maxv = vals
idx += 1
pix = data[idx:idx + w*h*3]

def chunk(typ, d):
    c = typ + d
    return struct.pack(">I", len(d)) + c + struct.pack(">I", zlib.crc32(c) & 0xffffffff)

stride = w*3
raw = bytearray()
for y in range(h):
    raw.append(0)
    raw.extend(pix[y*stride:(y+1)*stride])
png = (b"\x89PNG\r\n\x1a\n"
       + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
       + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
       + chunk(b"IEND", b""))
open(PNG, "wb").write(png)
print(f"PPM {w}x{h} -> PNG {os.path.getsize(PNG)} bytes -> {PNG}")
