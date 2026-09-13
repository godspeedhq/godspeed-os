# 26. The VisionFive 2 Lite's U-Boot cannot load large files from the SD card

**Severity:** blocking for hardware verification of the riscv64 port. Not a GodspeedOS defect - the
same failure happens to StarFive's own Debian files on a freshly flashed stock card.
**Status: OPEN, board-side.** Recorded so `db3b800b` stays honestly unconfirmed rather than assumed.

## What happens

2026-09-13, deploying `db32b674` to the VisionFive 2 Lite. U-Boot reads the card, shows the menu,
selects GodspeedOS, and then:

```
419 bytes read in 1 ms (409.2 KiB/s)          <- uEnv.txt        OK
Retrieving file: /extlinux/extlinux.conf
1605 bytes read in 3 ms (522.5 KiB/s)         <- extlinux.conf   OK
...
Retrieving file: /godspeed-riscv64-visionfive.img
Failed to load '/godspeed-riscv64-visionfive.img'          <- 2.7 MB
Skipping godspeed for failure retrieving kernel
Retrieving file: /initrd.img-6.12.5-starfive
Failed to load '/initrd.img-6.12.5-starfive'               <- 13.9 MB, STOCK
Skipping l0 for failure retrieving initrd
Skipping l0r for failure retrieving initrd
```

**Small reads succeed; every multi-megabyte read fails.** All three boot entries fail, two of them
StarFive's own. A full reflash of the card to the stock image changed nothing.

## What is RULED OUT, with evidence

- **Not the GodspeedOS image.** riscv64 at `db32b674` boots clean under QEMU: banner, hart, memory,
  capabilities, IPC, `supervisor: ready`, `shell: ready`, all 11 services wired. The flat image links
  `.text` at VMA `0x40200000`, exactly where U-Boot jumps.
- **Not the copy.** The deployed kernel hashes `190565C0450F9ABD...` on the card and the local build
  hashes the same. Reproducible, and the right bytes arrived.
- **Not card corruption.** A full reflash reproduced the failure exactly. The first theory in the
  session was corruption and it was WRONG; the reflash is what disproved it.
- **Not the card's ability to return large reads.** `scripts/deploy_visionfive.ps1` now cold-reads the
  card with `FILE_FLAG_NO_BUFFERING` (cache bypassed, every read hits the device):

  ```
  kernel       2,743,208 bytes   194 ms  13814 KiB/s  190565C0450F9ABD   (matches what was written)
  initrd      13,981,593 bytes   710 ms  19240 KiB/s  (stock, never written by the script)
  disk 1 partition 3   Realtek PCIE Card Reader   7.6 GB   health Healthy / OK
  ```

  The 13.9 MB file U-Boot cannot load reads perfectly, in full, at 19 MB/s.
- **Not our extlinux.conf.** U-Boot parses it, shows the `godspeed` label and selects it. The failure
  is in retrieving the file it names, and the stock entries fail the same way.

## What that leaves

The board's SD read path under U-Boot, on long transfers only. Consistent with either a marginal
supply (a sustained multi-MB read draws more than the idle that reached the menu) or a timing-mode
negotiation this particular 8 GB card cannot sustain in that slot - U-Boot frequently does not fall
back where Linux would. Note also that nothing on the board read anywhere near the 14-19 MB/s a PC
gets from the same card.

## The next concrete step

In yield order, none of which needs a rebuild or a redeploy:

1. **A different microSD** - a name-brand 16-32 GB A1/A2 card. Highest-yield single test, and it
   addresses the mode-negotiation hypothesis directly.
2. **A different PSU and cable.**
3. **Reseat the card.**

`scripts/deploy_visionfive.ps1` runs the cold-read diagnostics on every invocation, so it doubles as
the card checker for any replacement.

## What stays blocked

`db3b800b` (riscv64 spawns the USB host before the disk that lives on it) is verified in QEMU and
still wants a board boot, as `backlog/20` item 6 records. So does everything else on
`portability-hardening` that riscv64 has never executed on hardware: the single `USB_IMAGES` table,
`has_hw_enumerator`, `has_ehci` being false on this port (a latent bug this branch fixed), the
`storage_is_usb` board table, the TSC floor that this port's 4 MHz counter needs, and the seam
members added in `b78c8d79` and `3c75638e`.
