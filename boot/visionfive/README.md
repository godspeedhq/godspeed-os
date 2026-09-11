# The VisionFive 2 Lite boot card

Everything needed to rebuild this board's SD card from nothing. Saved 2026-09-11, immediately before
the card was reflashed for a Raspberry Pi - which is the only reason this file exists rather than the
knowledge living on one microSD.

## The card is NOT the bootloader, and that is the important fact

SPL, OpenSBI and U-Boot all live in the board's **16 MB SPI flash**, not on the card. The board's own
log says so: `Trying to boot from SPI`, `Loading Environment from SPIFlash: SF: Detected gd25lq128`.

Two consequences worth stating plainly:

- **The card carries a kernel and nothing else.** One plain FAT32 partition is enough. There is no
  SPL partition, no U-Boot partition, and none of the GPT type GUIDs a from-scratch VisionFive 2
  install needs.
- **Wiping or reflashing the card cannot brick the board.** It boots to U-Boot regardless; it simply
  finds nothing to load. Restoring is copying three files back.

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

**This file originally recorded "MBR, 1 of 1 partition", and that was wrong.** It came from Windows
`Get-Partition`, which does not list partitions it has no filesystem driver for - so it showed the ESP
and silently hid the two before it. A measurement that omits the part that matters is worse than no
measurement, because it gets trusted. `scripts/riscv_build.py` had been saying "partition 3, the ESP"
in its own deploy message the whole time.

Partitions 1 and 2 do not need CONTENTS - SPL, OpenSBI and U-Boot all live in SPI flash (see below).
They only need to exist, so that the FAT32 lands at index 3.

### Rebuilding the layout (Windows `diskpart`, as Administrator)

> Identify the card the safe way first: `list disk`, physically REMOVE the card, `list disk` again -
> the disk that disappeared is yours. Reinsert before selecting. Selecting the wrong disk here erases
> it.

```
diskpart
  list disk
  select disk N          <- the card, confirmed by the remove-and-compare above
  clean
  create partition primary size=2
  create partition primary size=4
  create partition primary
  select partition 3
  format fs=fat32 quick label=GODSPEED
  assign
  exit
```

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

1. Partition as **MBR** with a single **FAT32** partition starting at 1 MiB. No boot flag needed.
2. Copy `extlinux.conf` to `/extlinux/extlinux.conf` on the card.
3. Copy `dtbs/jh7110s-starfive-visionfive-2-lite.dtb` to `/dtbs/` on the card.
4. `python scripts/riscv_build.py --release --visionfive`, then copy
   `build/godspeed-riscv64-visionfive.img` to the card root.

## Why the device tree is kept here rather than fetched

It is the **authority** for facts about this board that upstream Linux does not carry. The base
`jh7110-starfive-visionfive-2.dtsi` in torvalds/linux has none of the `motorcomm,*` properties, and
those are what fixed networking on this port - `tx-clk-1000-inverted`, `rx-internal-delay-ps`,
`tx-internal-delay-ps`, `snps,force_thresh_dma_mode`, `snps,fixed-burst`, the `stmmac-axi-config`
node, and the `assigned-clock-parents` that settled the transmit clock argument.

Read it with the FDT walker in the scratchpad, or any `dtc -I dtb -O dts`. Do not substitute the
upstream file for it: the two disagree, and this one is the one the hardware obeys.
