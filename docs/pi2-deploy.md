# Deploying GodspeedOS to a Raspberry Pi 2

GodspeedOS on the Pi 2 uses **two pieces of media**, and they are not interchangeable:

| Medium | Holds | Prepared by |
|--------|-------|-------------|
| **microSD card** | the Pi firmware + our `kernel7.img` (the OS boots from here) | Raspberry Pi Imager + the steps in Part 1 |
| **USB stick** | the GSFS filesystem (all persistent data) | `diskpart clean` + `drives flash`, Part 2 |

The SD card is the boot medium and is never formatted by the OS (`fs` refuses it - it is where the
firmware lives). The USB stick is the storage, formatted by `drives flash` from inside the running OS.
This split is deliberate: the Pi has one SD slot and boots from it, so formatting that card would
destroy the boot medium. Storage goes on the USB stick.

---

## Part 1 - The boot microSD card

The card needs the Raspberry Pi firmware (`bootcode.bin`, `start.elf`, `fixup.dat`), a `config.txt`
pointing at our kernel, and `build/kernel7.img`. The firmware files are **not in this repo** (they are
Broadcom's, and large); the Raspberry Pi Imager writes them for you.

1. **Write any 32-bit Raspberry Pi OS to the card with the Raspberry Pi Imager.** You are only after the
   firmware and a FAT boot partition; the Linux userspace it also writes is ignored once `config.txt`
   points the firmware at our kernel.
2. **Reinsert the card** so Windows mounts the small FAT **boot partition** (labelled `bootfs` or
   `boot`).
3. **Copy `build/config.txt` onto it, replacing the Imager's** (say yes to overwrite). Ours is the
   canonical `boot/pi2/config.txt`, staged next to the kernel by `arm_build.py`. It is three lines:
   - `kernel=kernel7.img` - load our flat image by name (the firmware loads it at `0x8000`, where
     `kernel/kernel-arm.ld` expects it).
   - `arm_64bit=0` - force a 32-bit boot; our port is ARMv7, and this guards against a 64-bit OS image's
     default trying to load a `kernel8`.
   - `enable_uart=1` - keep the PL011 clock stable so the 115200 8N1 serial console is not garbled.
4. **Copy `build/kernel7.img` onto it.**
5. **Create `kernel7.img.bak` on the card** - copy `kernel7.img` to `kernel7.img.bak` right there. The
   flash flow (`scripts/`, the `bootfs` copy) uses this file as its "is this the right card?" guard and
   refuses to write a card that lacks it. (On the original stock card this `.bak` was the Raspbian
   kernel - a way back to Linux; ours is just a copy of our own image.)
6. **Eject safely, boot the Pi, watch serial (115200 8N1).** You should see `GodspeedOS arm32: _start
   reached...`, then the boot, then `dwc2: USB IRQ DELIVERY CONFIRMED` (present since the interrupt-route
   work). If serial is blank, the firmware cannot read the card (bad FAT / wrong files); if it prints
   `_start` then stops, the kernel faulted.

**If the label is `boot` not `bootfs`,** rename the volume to `bootfs` in Windows so the flash flow
finds it, or the copy step just becomes a manual drag of the two files.

**Reflashing after the first setup** is only step 4 (copy the new `build/kernel7.img`); the firmware,
`config.txt`, and `.bak` stay. Verify the copy by **SHA256 match** (source vs card) - that is the
reliable check, not a boot attempt.

---

## Part 2 - The storage USB stick

`fs` refuses to format a stick that still carries a partition table unless you pass `force` (the
`foreign_disk` guard - it assumes a partitioned disk might be someone's data or a boot medium). The
clean way to prepare a stick is therefore to **blank it first** on the PC, after which `drives flash`
takes it with no `force` needed.

### Blanking the stick (Windows `diskpart`)

> **This destroys the selected disk. Select the wrong one and you wipe it - your system drive is in the
> same list.** Identify the stick by size, and confirm it the safe way below before selecting anything.

```
diskpart
list disk                     <- note every disk and its SIZE
```

**Confirm which disk is the stick before selecting it.** The safe method: run `list disk`, physically
**remove** the stick, run `list disk` again - the disk that *disappeared* is your stick. Reinsert it and
select that number. (This is exactly the check that catches "Disk 0 is 953 GB" being the system drive,
not the stick.)

```
select disk N                 <- N = the stick you confirmed by size + disappear/reappear
clean                         <- removes the partition table (fast; also zeros the first/last MB,
                                 which is where a stale GSFS superblock + its backup live, so this is
                                 enough for fs to see a blank disk)
exit
```

- `clean` is sufficient: it zeros the MBR/GPT area and the first/last megabyte, which covers both the
  partition signature `fs` checks at block 0 **and** the GSFS backup superblock at the last block.
- `clean all` (what you may see used) zeros **every** sector - thorough but slow on a large stick, and
  unnecessary for `fs`. Use it only if you want the whole medium provably blank.

### Formatting it in GodspeedOS

Boot the OS with the blanked stick attached, then at the `gsh>` prompt:

```
drives flash 0 data           <- blank disk: no 'force' needed. formats as GSFS, labelled "data"
```

If the stick still has a partition table (you skipped the blanking), `fs` refuses it and tells you to
add `force`:

```
drives flash 0 data force     <- overrides the foreign-disk guard; ERASES whatever is there
```

After formatting, the stick is mounted and ready - `write`, `read`, `ls`, `selfcheck`, etc. all work,
no reboot.

### Durability caveat (this hardware)

A USB mass-storage stick acknowledges a write when it has the data in its own buffer, not when it is on
flash. Our stick **refuses SCSI SYNCHRONIZE CACHE**, so durability rides on **FUA** (force-unit-access
on every write - `USE_FUA` in `services/dwc2`, formerly `arch/arm/dwc2.rs` before the USB stack left the
kernel), which this stick honours. With FUA on, every
acknowledged write is on the medium before the ack, so a power cut does not lose the tail of a write
sequence. See `CLAUDE.md` §6.1 (the backend-conditional recovery amendment) for the full treatment.

### If the stick misbehaves - it may be the stick

USB sticks fail, and a failing one is not always obvious: a dodgy stick can stay electrically connected
(so the port still reports it present) while its own controller wedges under sustained I/O, dropping off
the bus roughly once per heavy run. The recovery layers catch it, but it costs a few operations each
time. If a stick drops off repeatedly under load, **swap it before assuming a software bug** - one bad
stick cost a week of chasing a ghost that a $3 swap settled (`milestones/ALMANAC.md`, 2026-07-27). Eight
consecutive clean `selfcheck` runs on a good stick is the bar.

---

## Troubleshooting the boot

| Symptom | Meaning |
|---------|---------|
| Red LED on, green LED doing nothing, **no serial output** | The firmware cannot read the card - bad FAT partition, or missing/incompatible firmware files. Re-image the card (Part 1). |
| Serial prints `_start reached` then stops | Our kernel faulted early - a real kernel bug, not a media problem. |
| `gsh>` prompt but `storage-unavailable` / no `drives` | No USB stick, or it is not enumerating. Check it is attached; try Part 2 on a known-good stick. |
| Green + red LEDs, nothing on screen or serial, card won't mount in the PC either | The SD card itself has failed or corrupted. Re-image it (Part 1); if it will not take an image, use a different card. |
