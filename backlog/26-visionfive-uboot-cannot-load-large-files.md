# 26. U-Boot fails to load the GodspeedOS kernel from the card, and we do not yet know why

**Severity:** blocks hardware verification of the riscv64 port. Not a GodspeedOS kernel defect - the
failure is in U-Boot, before our first instruction runs.
**Status: OPEN, cause NOT established.**

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

## What is left, as hypotheses and labelled as such

Nothing below is established. No third theory gets written down as fact in this file.

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

## The next concrete step: ONE command, and the file is already in place

Hypothesis 3 is disproved (above), so the card now holds our kernel AND boots Debian. Nothing more
needs writing. At the `StarFive #` prompt (hit a key during `Hit any key to stop autoboot: 2 1 0`, which is the
two-second window BEFORE the one-second menu):

```
fatload mmc 0:3 ${kernel_addr_r} godspeed-riscv64-visionfive.img
```

That prints the real error instead of the pxe wrapper's generic "Failed to load", and is the single
most informative command available.

## What stays blocked

`db3b800b` (riscv64 spawns the USB host before the disk that lives on it) is verified in QEMU and
still wants a board boot, as `backlog/20` item 6 records. So does everything else on
`portability-hardening` that riscv64 has never executed on hardware: the single `USB_IMAGES` table,
`has_hw_enumerator`, `has_ehci` being false on this port, the `storage_is_usb` board table, the TSC
floor this port's 4 MHz counter needs, and the seam members added in `b78c8d79` and `3c75638e`.
