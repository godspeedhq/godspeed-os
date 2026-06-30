# Milestone - Persistence (AHCI Block Driver + GSFS) ✅

**Status:** ✅ Complete - hardware-proven on the HP T630's real Samsung SSD (AHCI port 0,
`SAMSUNG MZNLN128HAHQ-000H1`). Files survive a real power-cycle reboot.

---

## Scope

v1 deliberately had no persistence beyond a trusted block driver (CLAUDE.md §23.4). This milestone
gives GodspeedOS a real, crash-consistent filesystem on its own on-disk format (**GSFS0008**), served
by a userspace `fs` service over a userspace AHCI `block-driver` - and, crucially, makes both services
**restartable** so they could leave the TCB (§6.1, Phase-D amendment). The kernel learns nothing about
files: it tracks an opaque `ResourceId` and its owning endpoint and nothing more (§4.4 / §15); `fs`
alone maps `ResourceId → file`.

The design rule throughout was the constitution's: **loud failure over silent corruption** (§3.12).
Every integrity failure mode is detected (CRC), reported, and refused rather than served as garbage.

---

## Achievements

- ✅ **AHCI (SATA) block driver - MMIO + DMA.** Replaced ATA PIO (the T630's SSD is AHCI-only):
  command list / received-FIS / PRDT, IDENTIFY, read/write, and **block-I/O retry** that recovers a
  transient command failure (`osdev test fs-ioretry`). The driver is `unsafe`-free behind the SDK's
  audited `Mmio`/`Dma` wrappers (§18.1). `docs/ahci.md`.
- ✅ **GSFS0008 on-disk format.** Superblock + **backup superblock**, a free **bitmap**, and a
  self-describing **`file_record` tree** with intrinsic reclamation. The free count + bitmap are a
  *derived view* of the tree (Commandment III - reconcilable, not a second truth), rebuilt by fsck.
  `docs/persistence.md` §6.4.
- ✅ **Extent lists / fragmentation.** A file that can't fit contiguously takes the fragmented
  `ITYPE_FILE_FRAG` extent-list path; the extent list survives a reboot (`osdev test fs-frag`).
- ✅ **Large streaming files.** 200 KiB+ files written/read in streaming `IO_CHUNK` chunks
  (`WriteNew`/`WriteAt`/`ReadAt`) and re-verified across a reboot (`osdev test fs-large`).
- ✅ **Crash-consistency via a redo-journal.** Every metadata mutation commits through the journal;
  **mount REPLAYS** committed transactions and **REJECTS** a commit record with a bad CRC (no replay,
  mounts clean). Opt-in **data journaling** makes a chunk write crash-atomic, not torn
  (`osdev test fs-journal`, `fs-djournal`). `docs/persistence.md` §6.8.
- ✅ **Integrity tooling.** `drives check` (fsck) rebuilds the free count + bitmap from the tree;
  read-only `drives scrub` reports bad blocks without repairing; **feature-flag policy** lets the
  format evolve (unknown `incompat` → mount refused, `ro_compat` → read-only, `compat` → normal);
  a corrupt primary superblock **recovers from the backup** (`osdev test fs-check`, `fs-scrub`,
  `fs-compat`, `fs-corrupt`).
- ✅ **`block-driver` + `fs` are restartable and left the TCB (Phase D).** Once `fs` could re-mount to
  a consistent state via the journal, its death became a supervisor restart, not a panic+reboot:
  `fs` re-mounts (recovering), re-registers, and clients reacquire it by name and retry. Pinned by
  **§22 Test 13** (`osdev test fs-restart`). The non-restartable set shrank toward `{kernel}`.

---

## Files / tests

| Area | Where |
|------|-------|
| AHCI backend | `services/block-driver/`, `docs/ahci.md` |
| Filesystem + journal | `services/fs/`, `docs/persistence.md` |
| File-as-capability (delegated resource caps) | see `milestones/storage/file-as-capability.md` |
| TCB-drop amendment | CLAUDE.md §6.1 (Phase D, 2026-06-17), §15 |

**Test suite** (all green; counts per `osdev/CLAUDE.md`): `osdev test files` (records/pipes/`result`/
`run`/`assert` over a raw AHCI disk), `fs-corrupt` (14), `fs-check` (5), `fs-ioretry` (5), `fs-large`,
`fs-frag` (11), `fs-journal` (11), `fs-djournal` (7), `fs-compat` (12), `fs-scrub` (6),
`fs-restart` (7, §22 Test 13).

---

## Hardware verification

Proven on the **HP T630** against the internal Samsung SSD: format (GSFS0008), write, `reboot`,
re-read - the file persists across a real power cycle. The shell `selfcheck` suite (which exercises
the full persistence stack - format, journal, `drives check`, read-only `drives scrub`) has run green
on the T630. Persistence is hardware-validated, not just QEMU. Serial in
`build/putty_serial_output.log`.
