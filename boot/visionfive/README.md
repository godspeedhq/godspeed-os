# The VisionFive 2 Lite boot card

Everything needed to rebuild this board's SD card from nothing. Saved 2026-09-11, immediately before
the card was reflashed for a Raspberry Pi - which is the only reason this file exists rather than the
knowledge living on one microSD.

## The card is NOT the bootloader, and that is the important fact

SPL, OpenSBI and U-Boot all live in the board's **16 MB SPI flash**, not on the card. The board's own
log says so: `Trying to boot from SPI`, `Loading Environment from SPIFlash: SF: Detected gd25lq128`.

Two consequences worth stating plainly:

- **Wiping or reflashing the card cannot brick the board.** It boots to U-Boot regardless; it simply
  finds nothing to load.
- **But the card still needs the full StarFive partition layout**, because U-Boot's compiled-in
  environment loads from `mmc 0:3` and only from there. This was previously written up as "one plain
  FAT32 partition is enough", which is the error the rest of this file documents: the bootloader not
  being on the card does not make the card's LAYOUT free.

## Layout - and the correction that cost a boot

**The FAT32 partition must be PARTITION 3.** U-Boot's environment, stored in the board's SPI flash,
boots `mmc 0:3` and nothing else. Put the files on partition 1 and the board stops at its own splash
screen with:

```
Try booting from MMC0 ...
** Invalid partition 3 **
Couldn't find partition mmc 0:3
Retrieving file: /extlinux/extlinux.conf
** Invalid partition 3 **
Error reading config file
```

## The measured layout

Restore it by writing an official StarFive VisionFive image to the card (Rufus, Raspberry Pi Imager,
`dd` - it is a plain image write). Measured on a freshly flashed card, 2026-09-11:

```
disk: GPT, 7.61 GB
  p1     2 MB   offset   2097152   2e54b353-1271-4842-806f-e436d6af6985   SPL
  p2     4 MB   offset   4194304   5b193300-fc78-40cd-8002-e86c45580b47   U-Boot
  p3   100 MB   offset   8388608   c12a7328-f81f-11d2-ba4b-00a0c93ec93b   EFI System  <- mmc 0:3
  p4  3891 MB   offset 113246208   0fc63daf-8483-4772-8e79-3d69d8477de4   Linux rootfs
```

Only p3 matters to us. GodspeedOS is deployed by copying the kernel and DTB onto that ESP and ADDING a
label to the `/extlinux/extlinux.conf` already there - not replacing it, so the image's own kernel stays
in the menu as a known-good fallback (the same role a stock `kernel8.img` plays on the Pi 4).

The ESP is not mounted by Windows and has no drive letter. Assigning one needs an ELEVATED shell:

```powershell
Add-PartitionAccessPath -DiskNumber <N> -PartitionNumber 3 -AssignDriveLetter
```

An ordinary shell gets `Access to a CIM resource was not available to the client`, and reading the raw
partition table the other way is denied too (`Access to the path '\\.\PhysicalDrive<N>' is denied`).

## The correction that cost a boot

**This file used to record "MBR (not GPT) ... Partition 1 of 1 - FAT32, offset 1048576".** Every field
of that is wrong against the measurement above: wrong table format, wrong count, wrong index, wrong
offset. Booting it produced the failure quoted at the top of this file.

Two explanations were offered for it before anything was measured - that Windows hides partitions it
has no filesystem driver for, and that Windows renumbers partitions sequentially rather than reporting
table slots. **Both are disproven by the measurement.** Windows lists all four partitions here,
including two raw ones and an ext4 rootfs it cannot mount, and it reports the ESP as `PartitionNumber
3`, its true slot. So the tool was not hiding or renumbering anything, and the recorded note simply did
not describe a card this board can boot.

What the note should have been checked against was already in the repository:
`scripts/riscv_build.py` prints "copy it to the card's FAT partition (partition 3, the ESP)" every time
it builds. A measurement that contradicts a claim the build system is making out loud is the moment to
stop and reconcile, not to write the measurement down and move on.

Partitions 1 and 2 hold SPL and U-Boot, but the board does not boot from them - SPL, OpenSBI and U-Boot
all run from the 16 MB SPI flash, and U-Boot reports `bad CRC, using default environment`, so `mmc 0:3`
is compiled in rather than configured. That is why wiping this card cannot brick the board, and why
p1/p2 only need to exist.

## Contents

```
  extlinux/extlinux.conf                          594 bytes  (in this directory)
  dtbs/jh7110s-starfive-visionfive-2-lite.dtb   58716 bytes  (in this directory)
  godspeed-riscv64-visionfive.img              ~2.7 MB       (BUILD ARTEFACT - not saved)
```

The image is deliberately **not** kept here: `python scripts/riscv_build.py --release --visionfive`
regenerates it at `build/godspeed-riscv64-visionfive.img`. A stale copy of a build output is worse
than none, because somebody will eventually flash it.

## To rebuild the card

1. Write an official StarFive VisionFive image to the card with any imaging tool. Do NOT partition it
   by hand: the layout above, with the ESP at partition 3, is what U-Boot's compiled-in environment
   expects, and a hand-rolled single-partition card silently fails to boot.
2. Mount partition 3 from an ELEVATED shell (see above); Windows will not give it a letter otherwise.
3. Copy `dtbs/jh7110s-starfive-visionfive-2-lite.dtb` to `/dtbs/` on that partition.
4. `python scripts/riscv_build.py --release --visionfive`, then copy
   `build/godspeed-riscv64-visionfive.img` to the partition root.
5. ADD the label from `extlinux.conf` in this directory to the `/extlinux/extlinux.conf` already on the
   card, and point `default` at it. Appending rather than overwriting keeps the image's own kernel in
   the menu as a fallback.

## Why the device tree is kept here rather than fetched

It is the **authority** for facts about this board that upstream Linux does not carry. The base
`jh7110-starfive-visionfive-2.dtsi` in torvalds/linux has none of the `motorcomm,*` properties, and
those are what fixed networking on this port - `tx-clk-1000-inverted`, `rx-internal-delay-ps`,
`tx-internal-delay-ps`, `snps,force_thresh_dma_mode`, `snps,fixed-burst`, the `stmmac-axi-config`
node, and the `assigned-clock-parents` that settled the transmit clock argument.

Read it with the FDT walker in the scratchpad, or any `dtc -I dtb -O dts`. Do not substitute the
upstream file for it: the two disagree, and this one is the one the hardware obeys.
