# The VisionFive 2 Lite boot card

How to prepare an SD card that boots GodspeedOS on the StarFive VisionFive 2 Lite, and why it has to be
done this way. The procedure comes first; the reasoning and the history are below it.

## Prepare the card

**1. Write an official StarFive VisionFive image to the card.** Any imaging tool - Rufus, Raspberry Pi
Imager, `dd`. This is a plain image write.

Do **not** partition the card by hand. The layout that image produces is what this board's U-Boot
expects, and a hand-rolled one fails silently at boot (see *Failure modes* below).

**2. Give partition 3 a drive letter, from an ELEVATED shell.**

```powershell
Add-PartitionAccessPath -DiskNumber <N> -PartitionNumber 3 -AccessPath 'P:\'
```

Find `<N>` with `Get-Disk`. Windows hides EFI System Partitions and gives them no letter, so nothing
appears in Explorer until you do this - that is expected, not a fault.

**3. Build the kernel.**

```
python scripts/riscv_build.py --release --visionfive
```

Add `--features riscv-single-hart` to bring up one hart and park the rest.

**4. Deploy, from the same elevated shell.**

```powershell
pwsh -File scripts\deploy_visionfive.ps1
```

Pass `-Esp X:` if you used a letter other than `P:`. The run is transcribed to
`build\deploy_visionfive.log`, so the result is read from a file rather than copied out of a console.

The script verifies rather than assumes at every step. It refuses to write unless the target carries
both `extlinux/extlinux.conf` and the board device tree the config references, so pointing it at the
wrong drive writes nothing. It compares the kernel by SHA256 after copying rather than trusting a quiet
`Copy-Item`. It parses the installed config back to confirm GodspeedOS is the default **and** that the
Debian `l0`/`l0r` labels survived, so the fallback cannot be lost silently. The stock config is backed
up once to `extlinux.conf.orig`, and that backup is never overwritten.

**5. Boot.** Card into the board, power on. The menu appears for one second and then boots GodspeedOS.
Any keypress during that second opens the menu, where `l0` is still Debian.

To go back to a stock card: `Copy-Item P:\extlinux\extlinux.conf.orig P:\extlinux\extlinux.conf -Force`.

## Why it works this way

**The bootloader is not on the card.** SPL, OpenSBI and U-Boot all live in the board's **16 MB SPI
flash**; its log says so (`Trying to boot from SPI`, `Loading Environment from SPIFlash: SF: Detected
gd25lq128`). So wiping or reflashing the card cannot brick the board - it still reaches U-Boot, it just
finds nothing to load.

**That does not make the card's layout free, and this is the trap.** U-Boot loads from `mmc 0:3` and
nowhere else, and the setting is not configurable from the card: it reports `bad CRC, using default
environment`, so the partition index is compiled into the U-Boot in SPI flash. The FAT partition must
therefore be **partition 3**.

The official image's layout, measured 2026-09-11:

```
disk: GPT, 7.61 GB
  p1     2 MB   offset   2097152   2e54b353-1271-4842-806f-e436d6af6985   SPL
  p2     4 MB   offset   4194304   5b193300-fc78-40cd-8002-e86c45580b47   U-Boot
  p3   100 MB   offset   8388608   c12a7328-f81f-11d2-ba4b-00a0c93ec93b   EFI System  <- mmc 0:3
  p4  3891 MB   offset 113246208   0fc63daf-8483-4772-8e79-3d69d8477de4   Linux rootfs
```

Only p3 matters. p1 and p2 are not used for booting (that is SPI flash's job) but the image writes
them, and leaving them alone is what keeps the ESP at index 3.

Our kernel goes on p3, and our label is **appended** to the `extlinux.conf` already there rather than
replacing it - so the image's own Debian kernel stays in the menu as a known-good fallback, the same
role a stock `kernel8.img` plays on the Pi 4. The device tree is already on that partition from the
image, at `/dtbs/6.12.5-starfive/starfive/jh7110s-starfive-visionfive-2-lite.dtb`, which is the file the
config points at.

## Failure modes

| What you see | What it means |
|---|---|
| Board stops at its own splash screen; serial shows `** Invalid partition 3 **`, `Couldn't find partition mmc 0:3`, `Error reading config file` | The FAT partition is not at index 3. Re-flash the official image; do not hand-partition. |
| No drive letter appears after flashing | Correct behaviour. Windows hides EFI System Partitions. Step 2 above. |
| `Access to a CIM resource was not available to the client` | The shell is not elevated. Mounting an ESP needs administrator. |
| `Access to the path '\\.\PhysicalDrive<N>' is denied` | Same cause, for raw partition-table access. |
| `Access to the path 'P:\...' is denied` on a partition that IS mounted | Windows ACLs an ESP to administrators. Read and write it from the elevated shell. |
| Menu appears and seems to wait for input | It is a one-second countdown, not a question. `timeout` is in deciseconds. |
| Menu label still says something you changed | The deploy did not run. Check `build\deploy_visionfive.log` and the kernel's byte count. |

## What is in this directory

```
  extlinux-on-stock-esp.conf                     1566 bytes   installed by deploy_visionfive.ps1
  extlinux.conf                                   949 bytes   for a card carrying ONLY our kernel
  dtbs/jh7110s-starfive-visionfive-2-lite.dtb   58716 bytes   reference copy; see below
```

The two configs describe **different cards** and are not interchangeable. `extlinux-on-stock-esp.conf`
is the one the procedure above uses: it reproduces the image's two Debian labels byte for byte, points
`default` at GodspeedOS, and references the device tree where the official image puts it.
`extlinux.conf` is for a card carrying nothing but our kernel, with the device tree at `/dtbs/`.

The kernel image is deliberately **not** kept here; `scripts/riscv_build.py` regenerates it. A stale
copy of a build output is worse than none, because somebody will eventually flash it.

## Why the device tree is kept here rather than fetched

It is the **authority** for facts about this board that upstream Linux does not carry. The base
`jh7110-starfive-visionfive-2.dtsi` in torvalds/linux has none of the `motorcomm,*` properties, and
those are what fixed networking on this port - `tx-clk-1000-inverted`, `rx-internal-delay-ps`,
`tx-internal-delay-ps`, `snps,force_thresh_dma_mode`, `snps,fixed-burst`, the `stmmac-axi-config`
node, and the `assigned-clock-parents` that settled the transmit clock argument.

Read it with any `dtc -I dtb -O dts`. Do not substitute the upstream file for it: the two disagree, and
this one is the one the hardware obeys.

## History, and what it cost

An earlier card used a different arrangement: a single FAT32 partition whose 16-byte MBR entry was moved
from slot 1 to slot 3. `backlog/14-riscv64-port.md` records it, and `build/mbr_backup.bin` is the
pre-move backup (slot 1, type 0x0C, startLBA 2048, 15952344 sectors; slots 2, 3, 4 empty). It works, but
**how the move was performed was never written down**, and it needs raw-sector access from an elevated
shell. The procedure above needs no sector surgery, so it is the one to use.

This file previously recorded the card as "MBR (not GPT) ... Partition 1 of 1 - FAT32, offset 1048576" -
a note that omitted the only field U-Boot reads, the slot. Restoring from it produced a card that could
not boot, and the recovery went wrong three more times before it went right:

- Two explanations of Windows partition numbering were published as conclusions before either was
  measured. **Both were withdrawn.** The second was "checked" against a GPT card with all four slots
  occupied, where index and slot match trivially - a test whose cases cannot come apart, reported as a
  disproof. Whether a Windows `PartitionNumber` is a slot is still **unresolved for MBR**; it cannot be
  known from what survives, since the only backup predates the move. The rule that needs none of this:
  **read the slot, never the ordinal.**
- A `diskpart` recipe was added that built a three-primary MBR card, written to satisfy "the ESP must be
  partition 3" without knowing the layout. It would have produced another card that does not boot. It
  was removed.
- An `extlinux.conf` was mangled by pasting a multi-line PowerShell here-string into an interactive
  console, which executed line by line and wrote garbage over a working file. Hand over a **file** and a
  one-line copy, never a multi-line paste.

All of it was avoidable. `scripts/riscv_build.py` prints "copy it to the card's FAT partition
(partition 3, the ESP)" on every single build, `backlog/14` describes the deployment, and a serial
capture of the card booting before anything was touched had been sitting in `build/` the whole time.
That capture is the whole answer, in six lines:

```
Retrieving file: /extlinux/extlinux.conf
916 bytes read in 4 ms
U-Boot menu
1:      Debian GNU/Linux trixie/sid 6.12.5-starfive
Enter choice: 1:        Debian GNU/Linux trixie/sid 6.12.5-starfive
Retrieving file: /dtbs/6.12.5-starfive/starfive/jh7110s-starfive-visionfive-2-lite.dtb
```

It is quoted here because the file it came from is **not tracked** - `build/` is gitignored, so both it
and `build/mbr_backup.bin` are local-only and will not survive a clean or a clone. Evidence a document
relies on belongs in the document.
**A measurement that contradicts something the project says out loud is the moment to stop and
reconcile, not to write the measurement down and move on** - and the project's own records are the first
place to look, before reasoning about how something must have been set up.
