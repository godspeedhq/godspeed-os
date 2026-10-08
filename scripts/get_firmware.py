#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Fetch the vendor radio firmware a board needs, and VERIFY it.

**Corrected 2026-10-08:** the CYW43455's files are in this repository now, under `nonfree/brcm43455/`
with their LICENCE and PROVENANCE - `570d43ee` reversed `docs/wifi.md` section 8's decision the day it was
made, and section 8 now says why the files ARE here. What follows is the reasoning as it stood before
that; this script's remaining use is firmware whose licence forbids redistribution (`docs/licensing.md`).

`docs/wifi.md` section 8 decides that no vendor firmware blob is committed here, following what Linux
does: the kernel tree carries none, they live in a separate `linux-firmware` that distributions package,
and the driver reads the file off the filesystem at runtime.

That decision leaves one question, which is the one this script answers: **where does somebody booting
GodspeedOS actually GET the file?**

  * NOT from their existing Raspberry Pi OS install, which was this author's first answer and is wrong.
    The Pi BOOT firmware (`start4.elf`, `bootcode.bin`) is on the FAT partition and readable, but the
    radio blob lives in the Linux rootfs under `/lib/firmware/brcm/` - ext4, which this OS cannot read.
  * NOT from a GodspeedOS mirror. Redistributing means carrying the Broadcom copyright-notice obligation
    with every copy forever, and a mirror repository for one blob on one board is infrastructure nothing
    has pulled into existence (26.2).
  * From the canonical distribution, fetched by the machine's owner, ONCE.

WHAT THIS REPOSITORY KEEPS IS THE CHECKSUM, AND THAT IS THE POINT. A SHA-256 is a fact, not a blob: it
is 64 characters, it cannot be "nearly right", and it turns a substituted, truncated or corrupted
download into a loud failure at setup instead of a radio that associates and then behaves strangely for
reasons nobody can see. Recording facts and refusing to record binaries is the same instinct applied to
two different things.

WHEN THE URL DIES - and it will eventually - this script fails LOUDLY and names the exact files, so the
operator can get them from a Pi OS install or their distribution's package. A loud failure with
instructions beats a silently stale mirror, which is the same argument 26.7 makes about everything else.

Usage:
    py scripts/get_firmware.py pi4            fetch + verify into firmware/ (gitignored)
    py scripts/get_firmware.py pi4 --list     resolve and report only; downloads nothing
"""
import hashlib
import io
import json
import os
import sys
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_DIR = os.path.join(ROOT, "firmware")

# The canonical distribution: the Raspberry Pi distro's own non-free firmware package. Upstream
# `linux-firmware` carries the Cypress binaries, but the NVRAM `.txt` is BOARD-specific calibration and
# comes from the board vendor - which is why this points at the Pi distro rather than at upstream.
BASE_DIR = "debian/config/brcm80211/brcm"
API = ("https://api.github.com/repos/RPi-Distro/firmware-nonfree/contents/"
       "{path}?ref=bookworm")

# One entry per board. The three files are what `brcmfmac` loads for this part: the firmware image, the
# regulatory (CLM) blob, and the board's NVRAM calibration text.
#
# `sha256` is None until a first verified fetch fills it in - deliberately, because inventing a checksum
# would be worse than having none: it would assert a fact nobody measured. Run with `--list`, read the
# digests it prints, and paste them here. From then on the check is real.
BOARDS = {
    "pi4": {
        "what": "Raspberry Pi 4 Model B - Cypress/Broadcom CYW43455",
        "files": [
            ("brcmfmac43455-sdio.raspberrypi,4-model-b.bin",      None),
            ("brcmfmac43455-sdio.raspberrypi,4-model-b.clm_blob", None),
            ("brcmfmac43455-sdio.txt",                            None),
        ],
    },
}

LICENCE_NOTE = """
  LICENCE. These files are Broadcom/Cypress firmware, redistributable under
  LICENCE.broadcom_bcm43xx with the copyright notice attached, and explicitly NOT modifiable,
  reverse-engineerable, decompilable or disassemblable. GodspeedOS neither ships nor studies
  them: this fetches what your board needs onto your machine, and the driver loads it.
"""


def fetch_json(url):
    req = urllib.request.Request(url, headers={
        "User-Agent": "godspeed-get-firmware",
        "Accept": "application/vnd.github+json",
    })
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read().decode("utf-8"))


def fetch_bytes(url):
    req = urllib.request.Request(url, headers={"User-Agent": "godspeed-get-firmware"})
    with urllib.request.urlopen(req, timeout=120) as r:
        return r.read()


def looks_like_path(data):
    """Is this blob actually a symlink target rather than firmware?

    Anything this small that decodes as a clean relative path is a symlink, whatever the API calls its
    type - which it got wrong for the clm_blob on the first run. The size guard downstream stays as the
    backstop, because it is what turned a silent corruption into a refusal.
    """
    if len(data) >= 256 or b"\x00" in data:
        return None
    try:
        s = data.decode("utf-8").strip()
    except UnicodeDecodeError:
        return None
    if not s or "\n" in s or " " in s:
        return None
    return s if ("/" in s or s.endswith((".bin", ".clm_blob", ".txt"))) else None


def resolve(name, depth=0):
    """Return (real_path, size, download_url) following symlinks, up to two levels.

    THE SYMLINK IS NOT A DETAIL - it is the thing that would otherwise corrupt the download silently.
    The board-specific names in that repository are symlinks (31, 36 and 22 bytes), so fetching the raw
    URL returns the TARGET PATH AS TEXT, not the firmware. A 31-byte "firmware image" written to a disk
    is a radio that never starts, and nothing would say why.

    AND THE TARGET IS A PATH, not a name: the first version took `basename` and looked in the same
    directory, but the real blobs are `../cypress/cyfmac43455-sdio.bin` - a sibling. The Cypress naming
    is the giveaway. These are Cypress parts that Broadcom sold on, so the binaries live under
    `cypress/` and the `brcm*` names are compatibility symlinks.
    """
    path = name if "/" in name else BASE_DIR + "/" + name
    meta = fetch_json(API.format(path=path))
    size = meta.get("size", 0)
    url = meta.get("download_url")

    target = meta.get("target") if meta.get("type") == "symlink" else None
    if target is None and size < 256 and url:
        # The API did not call it a symlink; ask the bytes.
        target = looks_like_path(fetch_bytes(url))
    if target and depth < 2:
        resolved = os.path.normpath(os.path.join(os.path.dirname(path), target)).replace("\\", "/")
        print("    %s is a symlink -> %s" % (os.path.basename(path), target))
        return resolve(resolved, depth + 1)

    return path, size, url


def main():
    args = [a for a in sys.argv[1:]]
    list_only = "--list" in args
    args = [a for a in args if not a.startswith("--")]
    if len(args) != 1 or args[0] not in BOARDS:
        print("usage: py scripts/get_firmware.py <board> [--list]")
        print("boards: %s" % ", ".join(sorted(BOARDS)))
        return 2

    board = args[0]
    spec = BOARDS[board]
    print("firmware: %s (%s)" % (board, spec["what"]))
    print(LICENCE_NOTE.rstrip())
    if not list_only:
        if not os.path.isdir(OUT_DIR):
            os.makedirs(OUT_DIR)
        print("  into %s  (gitignored - docs/wifi.md 8)" % OUT_DIR)

    problems = 0
    for name, want in spec["files"]:
        print("  %s" % name)
        try:
            real, size, url = resolve(name)
        except Exception as e:  # noqa: BLE001 - the reason matters more than the type
            print("    RESOLVE FAILED: %s" % e)
            print("    The canonical URL may have moved. Get this file from a Raspberry Pi OS install")
            print("    (/lib/firmware/brcm/) or your distribution's firmware-brcm80211 package.")
            problems += 1
            continue
        if not url:
            print("    no download URL in the API response - cannot fetch")
            problems += 1
            continue
        try:
            data = fetch_bytes(url)
        except Exception as e:  # noqa: BLE001
            print("    DOWNLOAD FAILED: %s" % e)
            problems += 1
            continue

        got = hashlib.sha256(data).hexdigest()
        print("    %s bytes, sha256 %s" % (len(data), got))

        # A SIZE THAT LOOKS LIKE A SYMLINK IS A FAILURE, not a small file. Said explicitly because this
        # is the one corruption that would otherwise reach a disk and look like working firmware.
        if len(data) < 256:
            print("    REFUSED: %s bytes is a symlink target or an error page, not firmware" % len(data))
            problems += 1
            continue

        if want is None:
            print("    no recorded checksum yet - paste the digest above into BOARDS to make this real")
        elif got != want:
            print("    CHECKSUM MISMATCH - expected %s" % want)
            print("    REFUSED. A substituted or corrupted blob is a radio that misbehaves for reasons")
            print("    nobody can see, so this fails here instead.")
            problems += 1
            continue
        else:
            print("    checksum OK")

        if not list_only:
            dest = os.path.join(OUT_DIR, os.path.basename(name))
            with io.open(dest, "wb") as fh:
                fh.write(data)
            print("    wrote %s" % dest)

    if problems:
        print("firmware: %d problem(s) - nothing above them can be attempted" % problems)
        return 1
    print("firmware: %d file(s) OK%s" % (len(spec["files"]), "" if list_only else ", ready to bake onto a data disk"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
