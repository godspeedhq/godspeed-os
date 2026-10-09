# services/block-driver/

Userspace disk driver (persistence, v2; §6.3, `docs/ahci.md`, `docs/persistence.md`). **Two backends,
one block-IPC protocol**, selected by the board fact `storage_is_usb` (set by `build.rs`): **x86 =
AHCI/SATA** (MMIO + DMA, `ahci.rs`); **every USB board (Pi 2, Pi 4, VisionFive 2) = a USB mass-storage
stick** (`usbdisk.rs`, reached over IPC through `xhciblk.rs` - the `dwc2` service on the Pi 2, `xhci` on
the others; no USB stack is in the kernel any more).
They serve `fs` identically; `fs` never knows which. The rest of this doc describes the AHCI backend.
**Restartable, NOT a TCB member** (Phase D amendment, §6.1, 2026-06-17): it holds no persistent state,
so its death is a supervisor restart (re-init the controller, re-register), not a reboot - `fs`
reacquires it by name and retries (§14.3).

> **The SD/EMMC card is NEVER used as storage on the Pi 2.** An SD/EMMC backend exists (`sdhci.rs`) but
> is **not compiled in** (`main.rs` has no `mod sdhci`). On a single-slot Pi 2 the SD card is the boot
> medium - firmware + kernel + FAT boot partition - and GSFS's superblock at LBA 0 lands on the card's
> partition table, so using it as storage **corrupts the card to RAW** (it destroyed two boot cards).
> With no USB stick, `block-driver` comes up with **no disk** (`fs` storage-unavailable), exactly as x86
> reports with no SATA disk - it never falls back to the card. `sdhci.rs` is kept only for a future board
> with a *separate* storage medium. See `backend_run` and `docs/pi2-deploy.md`.

## Device: AHCI, MMIO + DMA

The driver talks to a SATA disk through an AHCI HBA: the kernel maps the HBA's ABAR
(MMIO) and grants a physically-contiguous DMA arena at spawn - the same path the USB
drivers use. It brings up port 0, IDENTIFYs the disk, runs a boot read self-test, then
serves block read/write to `fs` over IPC. It uses the SDK's safe `Mmio`/`Dma` wrappers,
so the driver itself is `unsafe`-free (§18.1).

Command shape: a command list (32 headers) + received-FIS area + a command table per
slot (H2D Register FIS type 0x27 + PRDT). ATA commands: IDENTIFY `0xEC`, READ DMA EXT
`0x25`, WRITE DMA EXT `0x35`, FLUSH CACHE EXT `0xEA`. A write is NOT flushed on its own: FLUSH is
issued only on `OP_FLUSH`, which `fs` sends at its two journal barriers and at the end of a format
(§6.1, amendment 2026-07-25 as corrected 2026-08-22; `write_block` in `ahci.rs`). See `docs/ahci.md`
for the register cheat-sheet.

**I/O retry (Phase H).** Every read/write/zero goes through `issue_io`: a **bounded retry**
(`MAX_IO_ATTEMPTS = 3`) with **port recovery** between attempts (`recover_port` clears
PxSERR/PxIS and restarts the command engine if it halted). A transient command error is
recovered transparently + logged; a persistent one is reported loudly (§3.12) and returns an
error. The `io-error-test` build feature injects forced failures to exercise this path (QEMU
never fails a real disk read); off in production.

## Why AHCI (not ATA PIO, not virtio-blk)

The T630's SSD is **AHCI-only** - no legacy/IDE mode - so AHCI is the production path.
ATA PIO was the bring-up backend (simplest correct device, no DMA); it was retired once
AHCI proved out on real hardware. virtio-blk is a QEMU-only paravirtual device that runs
on no real hardware, so it was never a candidate.

## DMA confinement (H1 / §6.4)

AHCI is a DMA-capable driver, so on a machine with an IOMMU it should be confined to its
granted arena. On the T630 the firmware hands the SATA controller over with a stale DMA
pointer (`0xffffffc0`, the same quirk `ehci` hits), so confining it faults a benign init
read - block-driver runs in **IOMMU passthrough** there. Full confinement needs an AHCI
BIOS/OS handoff (BOHC) - a future step (`docs/ahci.md` step E).

## Block IPC protocol (fs ↔ block-driver)

```
Request : [tag:u8, op:u8, (op-specific fields below)]
Reply   : [tag:u8, status:u8, (op-specific fields below)]
OP_READ_BLOCK   = 1  [lba:u64 LE]                    -> [status, (OK only: 512 data bytes)]
OP_WRITE_BLOCK  = 2  [lba:u64 LE, 512 data bytes]    -> [status]
OP_CAPACITY     = 3  []                              -> [STATUS_OK, sectors:u64 LE]
OP_WRITE_ZEROS  = 4  [lba:u64 LE, count:u64 LE]      -> [status]
OP_FLUSH        = 5  []                              -> [status]
STATUS_OK = 0, STATUS_ERR = 1
```

The **tag** is a correlation byte `fs` puts at byte 0 of every request and block-driver echoes at byte
0 of the reply (`Reply` in `main.rs`), so a reply that arrives out of order is detected rather than
believed. The op constants live in `main.rs`, and `services/fs` must match them.

The LBA is **u64** (persistence §6.3) so GSFS's u64 capacity fields reach the device at
full width. One request moves one 512-byte block (= one sector). `fs` owns file layout;
`block-driver` only moves sectors - policy above, mechanism below.

## Failure semantics (§6.2)

**Restartable (Phase D, §6.1 amendment 2026-06-17).** block-driver death is no longer a
panic+reboot: the kernel notifies the supervisor, which respawns it; death reclaims its
DMA/IOMMU resources and the fresh instance re-inits the controller and re-registers. `fs`
reacquires it by name and retries its block I/O (§14.3). Only its *boot-time* spawn
must succeed to bootstrap persistence (§11.3).
