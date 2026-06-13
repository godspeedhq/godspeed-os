# services/block-driver/

Userspace **AHCI (SATA)** disk driver (persistence, v2; §6.3, `docs/ahci.md`,
`docs/persistence.md`). Currently a TCB member (§6.1); the v2 goal is to drop it
(§6.3, Phase 3).

## Device: AHCI, MMIO + DMA

The driver talks to a SATA disk through an AHCI HBA: the kernel maps the HBA's ABAR
(MMIO) and grants a physically-contiguous DMA arena at spawn — the same path the USB
drivers use. It brings up port 0, IDENTIFYs the disk, runs a boot read self-test, then
serves block read/write to `fs` over IPC. It uses the SDK's safe `Mmio`/`Dma` wrappers,
so the driver itself is `unsafe`-free (§18.1).

Command shape: a command list (32 headers) + received-FIS area + a command table per
slot (H2D Register FIS type 0x27 + PRDT). ATA commands: IDENTIFY `0xEC`, READ DMA EXT
`0x25`, WRITE DMA EXT `0x35`, FLUSH EXT `0xEA` (writes flush to the medium so they
survive reboot). See `docs/ahci.md` for the register cheat-sheet.

## Why AHCI (not ATA PIO, not virtio-blk)

The T630's SSD is **AHCI-only** — no legacy/IDE mode — so AHCI is the production path.
ATA PIO was the bring-up backend (simplest correct device, no DMA); it was retired once
AHCI proved out on real hardware. virtio-blk is a QEMU-only paravirtual device that runs
on no real hardware, so it was never a candidate.

## DMA confinement (H1 / §6.4)

AHCI is a DMA-capable driver, so on a machine with an IOMMU it should be confined to its
granted arena. On the T630 the firmware hands the SATA controller over with a stale DMA
pointer (`0xffffffc0`, the same quirk `ehci` hits), so confining it faults a benign init
read — block-driver runs in **IOMMU passthrough** there. Full confinement needs an AHCI
BIOS/OS handoff (BOHC) — a future step (`docs/ahci.md` step E).

## Block IPC protocol (fs ↔ block-driver)

```
Request : [op:u8, lba:u64 LE, (WriteBlock only: 512 data bytes)]
Reply   : [status:u8, (ReadBlock only: 512 data bytes)]
OP_READ_BLOCK = 1, OP_WRITE_BLOCK = 2; STATUS_OK = 0, STATUS_ERR = 1
```

The LBA is **u64** (persistence §6.3) so GSFS's u64 capacity fields reach the device at
full width. One request moves one 512-byte block (= one sector). `fs` owns file layout;
`block-driver` only moves sectors — policy above, mechanism below.

## Failure semantics (§6.2)

While still a TCB member (Phase 1–2), block-driver death = kernel panic = system reboot.
Phase 3 (§6.3) makes it restartable and drops it from the TCB. Operationally it is
already restartable (death reclaims its DMA/IOMMU resources and the supervisor respawns
it); Phase 3 is the *trust* claim plus transactional recovery in `fs`.
