# 26. U-Boot fails to load the GodspeedOS kernel from the card, and we do not yet know why

**Severity:** blocks hardware verification of the riscv64 port. Not a GodspeedOS kernel defect - the
failure is in U-Boot, before our first instruction runs.
**Status: CLOSED 2026-09-13.** Cause was CRLF in `extlinux.conf`. Fixed, enforced by
`scripts/line_ending_check.py` in the build gates and in CI, and the port is now **fully validated on
the board**:

    selfcheck            ran 461, failed 0      (x3 - before chaos, after chaos, after hot-plug)
    chaos max-carnage    100 rounds, 583 kills, 493 flooded, kernel alive
    hot-plug             hub port 3 empty -> attached, 6 re-enumerations, recovered
    ping 8.8.8.8         2/2, 0% loss
    ls / file tree       good

    0 kernel panics, 0 liveness wedges, 0 exceptions in 17,102 lines

The 51 `PANIC in service` lines are all inside the chaos window and all carry the same designed
reason - "dying so the supervisor restarts us" - which is the restart mechanism working, not a fault.
The Pi 4 produced 50 of the same across its own storm.

> **This entry previously concluded "board-side: the board cannot load large files, try another card
> or PSU". THAT WAS WRONG and is corrected below.** It was written from a reading of the evidence that
> a later boot disproved. The wrong version cost a card reflash and a round of hardware theory; the
> reasoning error that produced it is recorded at the end, because it is the useful part.

## What happens

Deploying `db32b674` to the VisionFive 2 Lite. U-Boot reads the card, shows the menu, selects
GodspeedOS, and then:

```
419 bytes read in 1 ms (409.2 KiB/s)          <- uEnv.txt        OK
Retrieving file: /extlinux/extlinux.conf
1605 bytes read in 3 ms (522.5 KiB/s)         <- our config      OK
U-Boot menu
3:  GodspeedOS riscv64
Enter choice: 3:  GodspeedOS riscv64
Retrieving file: /godspeed-riscv64-visionfive.img
Failed to load '/godspeed-riscv64-visionfive.img'          <- 2.7 MB
Skipping godspeed for failure retrieving kernel
Retrieving file: /initrd.img-6.12.5-starfive
Failed to load '/initrd.img-6.12.5-starfive'               <- collateral, see below
```

## What is RULED OUT, each with evidence

- **Not the board, and not "large reads".** A freshly flashed stock card boots Debian on this same
  board, loading **13,981,593 bytes in 605 ms (22 MiB/s)** and then 10,440,372 more at the same rate.
  The board does large reads fine. This is the measurement that disproved the first version of this
  entry.
- **Not the card.** `scripts/deploy_visionfive.ps1` cold-reads it with `FILE_FLAG_NO_BUFFERING`
  (cache bypassed): our kernel returns all 2,743,208 bytes hashing `190565C0450F9ABD...`, matching
  what was written; the stock initrd returns all 13,981,593 bytes at 19 MiB/s. Health OK.
- **Not the GodspeedOS image.** riscv64 at `db32b674` boots under QEMU through to `shell: ready` with
  all 11 services wired. `.text` links at VMA `0x40200000`, where U-Boot jumps.
- **Not corruption, and not a stale card.** Reflashed twice; the failure reproduces exactly.
- **Not the procedure.** `boot/visionfive/README.md`'s five steps were followed as written.
- **Not our config being unreadable.** U-Boot parses it, lists the `godspeed` label, and selects it.
  The failure is retrieving the file the label names.

## THE REASONING ERROR, recorded because it is what cost the time

The Debian initrd failure in the log above was read as an INDEPENDENT CONTROL - "even StarFive's own
untouched file fails, therefore this is not us" - and two rounds of hardware theory were built on it.

**It is not independent.** It happens AFTER ours, in the same U-Boot session, and only ever after
ours. A failed multi-megabyte read plausibly leaves U-Boot's FAT or MMC state unusable, so everything
following it fails as collateral. On a stock card, with no GodspeedOS entry to fail first, the same
initrd loads perfectly.

Two entries failing does not make the second one a control. **A control has to run FIRST, or in a
session of its own.**

`boot/visionfive/README.md` already carried the general form of this lesson, from the previous
VisionFive incident: *"a measurement that contradicts something the project says out loud is the
moment to stop and reconcile, not to write the measurement down and move on."* The project said out
loud that this board boots GodspeedOS. It was not reconciled with; it was reasoned around.

## THE CAUSE

**`extlinux.conf` was CRLF.** `file(1)`: `ASCII text, with CRLF line terminators`, 61 CR bytes. The
stock 916-byte config that boots is plain `ASCII text`.

U-Boot's extlinux parser takes the trailing `\r` as part of the FILENAME, so it opens
`/godspeed-riscv64-visionfive.img\r`, which does not exist. The MENU still renders perfectly because
a `\r` in a display string only returns the cursor - which is exactly why this read as a load failure
rather than a config fault, and why the label looked right in every log.

It also explains the "collateral" Debian failure without any theory about U-Boot state: with a CRLF
config, EVERY label's filename carries the `\r`. Both entries were failing for one reason.

**Where the CRLF came from.** The repository stores the file as LF. `.gitattributes` marked it
`*.conf text`, which means "normalize on commit, convert to NATIVE on checkout" - and native on a
Windows checkout is CRLF. `Copy-Item` then put those bytes on the card. The same file already
carried `*.sh text eol=lf` under the heading "Scripts that MUST stay LF even on a Windows checkout";
the rule had simply never been extended to a file a BOOTLOADER reads.

**Fixed at three layers**, because any one of them alone would leave the trap for someone else:

1. `.gitattributes` gains `boot/** text eol=lf` - the whole directory, not a list of extensions,
   because the next board will bring a file type nobody thought to add.
2. The four checked-out boot configs are renormalized (visionfive x2, pi2, pi4 - all four were CRLF;
   the Pi firmware tolerates it, which is why only this board ever complained).
3. `deploy_visionfive.ps1` no longer `Copy-Item`s the config. It normalizes the bytes itself and
   then COUNTS CR BYTES ON THE CARD, refusing the deploy if any survive. Whether the board boots must
   not depend on a contributor's git settings or an editor that helpfully fixed a file.

Proven before committing: 2 CRs in, 0 out, and the script parses.

## The hypotheses this replaced, kept because the disproofs cost real time

1. **Our file is not loadable by U-Boot's FAT driver** even though Windows reads it perfectly. The
   stock files were laid down contiguously by an image writer onto an empty filesystem; ours is
   written by `Copy-Item` into a populated one. Size is not the discriminator - a 10.4 MB stock file
   loads and our 2.7 MB one does not.
2. **Our label is malformed in a way that makes the load fail rather than the parse.** It carries no
   `initrd` and uses `fdt <path>` where the stock entries use `fdtdir <dir>`.
3. ~~**The write itself damages the ESP** from U-Boot's point of view while leaving it perfect for
   Windows.~~ **DISPROVED 2026-09-13.** The kernel was copied onto a freshly flashed stock card with
   `Copy-Item`, leaving the stock `extlinux.conf` in place, and **Debian still boots**. So writing a
   2.7 MB file through Windows into the populated ESP does not damage it for U-Boot. That also means
   the file is sitting on a card that boots, which is what makes the next step a single command.

## The confirming boot, and what it also confirmed

`7fa2e2e2` booted on the board. That was riscv64's FIRST hardware run of anything on
`portability-hardening`, so it confirmed seven changes at once:

| what | evidence |
|---|---|
| `db3b800b` USB host before the disk | `task: 'xhci' spawned OK` precedes `task: 'block-driver' spawned OK`, and no `COULD NOT REACQUIRE` |
| `has_ehci` false on this port | **zero** occurrences of `ehci` in the whole log (it used to fall inside `not(any(arm, aarch64))`) |
| `has_hw_enumerator` | `task: 'hw-enumerator' spawned OK` |
| one `USB_IMAGES` table | `xhci` row present and spawned |
| TSC floor default-safe (`7f8f6924`) | ping 8.8.8.8 3/3, **21-59 ms** - a real RTT. The old 100 MHz floor rejected this port's 4 MHz counter and reported 0 |
| `storage_is_usb` board table | block-driver came up on the xhci path |
| seam members (`b78c8d79`, `3c75638e`) | 4 harts, 4 cores ready, dwmac PHY link up, shell at `gsh>` |

## What stopped this recurring

`scripts/line_ending_check.py` - reads the `eol=lf` patterns from `.gitattributes` (last-match-wins,
so a later `binary` rule overrides) and refuses any of them containing a carriage return. Wired into
`arm_build.py`, `riscv_build.py`, `build.yml` and `release.yml`.

On its first run it found two more: `scripts/qemu_iommu.sh` was CRLF with `#!/usr/bin/env bash^M`
(latent, would fail on Linux), and `boot/**  text eol=lf` had wrongly marked the 58 KB device tree
blob as text, which git would eventually have "normalized". Both fixed in the same commit.

## What stays blocked

`db3b800b` (riscv64 spawns the USB host before the disk that lives on it) is verified in QEMU and
still wants a board boot, as `backlog/20` item 6 records. So does everything else on
`portability-hardening` that riscv64 has never executed on hardware: the single `USB_IMAGES` table,
`has_hw_enumerator`, `has_ehci` being false on this port, the `storage_is_usb` board table, the TSC
floor this port's 4 MHz counter needs, and the seam members added in `b78c8d79` and `3c75638e`.
