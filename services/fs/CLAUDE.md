# services/fs/

Filesystem service. **Restartable, NOT a TCB member** (Phase D amendment, §6.1, 2026-06-17).

## Restartable (it once was trusted root)

fs owns persistent state (§15) and for v1 was trusted because it could not recover from a crash
mid-write. The crash-consistency journal (Phase C, `docs/persistence.md` §6.8) closes that: every
metadata mutation commits atomically and fs **recovers to a consistent state on mount**. So fs is
now restartable - the kernel notifies the supervisor of its death, which respawns it; fs re-mounts
(recovering via the journal), re-registers its name in the kernel directory, and clients reacquire it
and retry (§14.3). Only its *boot-time* spawn must succeed (§11.3). Pinned by §22 Test 13.

## Dependencies

- `block-driver`: all I/O goes through block-driver IPC.
- kernel name directory: fs registers its endpoint name so the supervisor and other services can resolve it.

## On-disk format: GSFS, hierarchical (magic `GSFS0008`, frozen)

The format is **GSFS0008** (`docs/persistence.md` §6.4 + §6.6 + §6.10 + §6.12 + §6.15). The magic is
**frozen at `GSFS0008`** - versioning now lives in three superblock **feature masks** (compat @124,
ro_compat @128, incompat @132, under the CRC @136), so the format evolves by feature bits, not new
magic. Mount policy: an unknown `incompat` bit → refuse loudly; an unknown `ro_compat` bit → mount
read-only (writes refused); unknown `compat` bits → ignored. Feature bits are set **only when used**
(extent-lists is an incompat bit set on first fragmentation), so a disk earns forward-compatibility
until it actually uses a feature. See §6.15; proven by `osdev test fs-compat`. Three on-disk
structures and no more - a **superblock**, a **free bitmap** (1 bit/block, the only global
structure), and a **directory tree** of self-describing 64-byte `file_record`s (no inode
table, no inode number, no global file cap; the tree *is* the index, the bitmap is the
allocation map, reclamation is intrinsic). 512-byte blocks (= one AHCI sector = one block-IPC
request), all capacity fields **u64** (~8 ZiB ceiling).

**Backup superblock (GSFS0008):** a second superblock copy lives at the **last block**
(`total_blocks-1`, reserved in the bitmap). `mount` validates the primary (LBA 0); if its
magic/CRC fails it falls back to the backup (located via the device capacity, so it works
even when the primary is unreadable) and heals the primary. Both copies are written together
by `format`, kept in sync by `persist_super` (staged in the same transaction), and both wiped
by `drives reset`.

**Every block self-verifies with a CRC32** (GSFS0008):

- **Superblock** (LBA 0): magic, version (8), `block_size`, `total_blocks:u64`,
  `bitmap_start/blocks:u64`, `data_start:u64`, `root_first_block/block_count:u64`,
  `free_blocks:u64`, `flags` (DEFAULT bit), `label`, `journal_start/blocks:u64` @108
  (reserved crash-consistency region, filled by Phase C), **feature masks** `feature_compat` @124 /
  `feature_ro_compat` @128 / `feature_incompat` @132 (GSFS0008, §6.15), and a **`sb_crc32` @136**
  over the first 136 bytes (now covering the masks) - **verified on mount, loud refusal on
  mismatch** (§3.12).
- **Free bitmap** (LBA `bitmap_start..journal_start`): set bit = used block. (Not checksummed -
  reconstructible from the tree.)
- **Journal** (LBA `journal_start..data_start`): reserved, 64 blocks (32 KiB).
- **Directory block** (512 B): **7** `file_record`s × 64 B (`type u8` - 0 free, 1 contiguous
  file, 2 dir, 3 fragmented file; `name_len u8`, `name[38]`, `size u64`, `first_block u64`,
  `block_count u64`) + a 64-byte trailer whose first 4 bytes are the **block's CRC32** over its
  448-byte record region - verified on every directory read (`dir_read`/`td_read`), stamped on
  every write (`dir_write`/`td_write`).
- **File-data block** (512 B): **508 bytes payload + CRC32 @508**. A file of N bytes spans
  `ceil(N/508)` data blocks; the CRC covers the payload, verified on every read (`data_read`),
  stamped on every write (`data_write`). The per-message streaming chunk is `7×508 = 3556`.
- **Extent block** (512 B, GSFS0008 - fragmented files only): `n_extents:u32 @0`, then up to
  `EXT_MAX = 31` `(start:u64, len:u64)` data runs from `@8`, + a **CRC32 @508** over `[0..508)`.
  A `type`-3 file's `first_block` points here (`block_count` = 1); the runs list its data.

**Files: contiguous, or fragmented (GSFS0008 extent lists; `docs/persistence.md` §6.12).** A file
is normally one **contiguous** extent (`type` 1 - the fast path: data is `first_block ..
first_block+block_count`). When no contiguous run is free, the file is stored **fragmented**
(`type` 3): `first_block` → a CRC'd **extent block** listing up to 31 scattered `(start, len)`
runs. The contiguous path is unchanged; the fragmented path engages only when contiguous
allocation fails, and a file needing more than 31 runs is refused loudly (§26.6). The extent
block is metadata, staged in the same crash-consistency transaction as the record and bitmap.

The CRC32 (IEEE 802.3) lives in `src/crc32.rs`; `osdev` carries a byte-identical copy so a
host-baked image checksums exactly as `fs` would.

Bounded & loud (§26.6): the only ceiling is the disk (the bitmap); directories grow;
contiguous file extents; no POSIX permission bits (authority is by capability, §3.3), no
hard links. Bad magic **or** bad CRC is a loud mount refusal, never an auto-reformat (§3.12).

## Exposed interface (via IPC - name field carries a path `/a/b/c`)

| Request     | Op | Args              | Response |
|-------------|----|-------------------|----------|
| `WriteFile` | 10 | path, data bytes  | `Ok` / `Err` |
| `ReadFile`  | 11 | path              | size + file bytes / `NotFound` |
| `StatFile`  | 12 | path              | exists, size:u64, is_dir |
| `Mkdir`     | 13 | path              | `Ok` / `Err` |
| `ListDir`   | 14 | path              | `{name, is_dir}` entries |
| `WriteNew`  | 24 | path, total:u64   | `Ok` / `Err` - create/truncate sized for a large file |
| `WriteAt`   | 25 | path, offset:u64, chunk | `Ok` / `Err` - write a chunk (block-aligned offset) |
| `ReadAt`    | 26 | path, offset:u64, len:u32 | `Ok`, n:u32, bytes / `NotFound` |
| `Check`     | 27 | -                 | fsck: `Ok`, files/dirs/bad:u32, used/free:u64 - rebuild bitmap+free from the tree, verify CRCs |
| `WriteAtJ`  | 28 | path, offset:u64, chunk | `Ok` / `Err` - like `WriteAt` but the chunk's data is **journaled** (Phase J): applied atomically (crash → replayed or discarded, never torn). Bounded to one chunk; default `WriteAt` stays direct |
| `Scrub`     | 29 | -                 | scrub (Phase K): `Ok`, files/dirs/bad:u32, scanned:u64 - READ-ONLY CRC integrity sweep over the tree; reports bad blocks, **changes nothing** (unlike `Check`, which repairs) |
| `Open`      | 30 | path, rights:u8   | file-as-capability (§7.10, P2): mint a delegated resource for the file, reply `[Ok]` + the **file cap** embedded. The holder then INVOKES the cap (kernel-badged) - `serve_filecap` resolves the badge's resource_id → file via the open-file table and enforces op ≤ the badged right (FOP_READ/WRITE/STAT/CLOSE). Proven by `osdev test file-cap` (§22 Test 14). |

**Large files (streaming).** `WriteFile`/`ReadFile` carry a whole *small* file in one ≤4 KiB
IPC message. Files larger than one message use the offset-addressed ops: `WriteNew` allocates
the full extent and sizes the file, then a sequence of `WriteAt` chunks fills it; read it back
with `StatFile` (for the size) + a sequence of `ReadAt` chunks. Stateless - each request is
self-contained (no open-file table; §8). Size is bounded only by free space: a file is a
contiguous u64 extent when one is free, else fragmented across an extent list (GSFS0008, §6.12
above), so a fragmented disk no longer refuses a write it has room for. The shell streams
`read`/`copy` and the pipe `write` sink through these ops.

## State and persistence (§15)

fs holds only superblock geometry + a maintained free count in memory (no inode table -
the tree and bitmap live on disk, read on demand). Directory blocks are read/written on
demand.

**Crash-consistency (Phase C, `docs/persistence.md` §6.8).** Every metadata mutation runs
as one **atomic transaction** through the reserved journal region: structural writes
(directory/bitmap/superblock) are staged in memory (read-your-writes), then `commit_txn`
writes them to the journal, writes a checksummed commit record (the atomic point),
checkpoints them home, and invalidates the journal. File **data** is written direct (into an
extent nothing references until the transaction commits). On **mount**, `recover` replays a
committed-but-unfinished transaction (valid commit magic + CRC) and discards a torn one - so a
single power loss leaves the filesystem either entirely unchanged or fully applied, never
half-updated. A transaction stages ≤ `TXN_CAP` (56) blocks (loud failure past that);
`delete_tree` commits the unlink atomically then reclaims the subtree in bounded per-extent
transactions. Proven by `osdev test fs-journal`.

**Opt-in data journaling (Phase J, §6.13).** `WriteFile`/overwrite are already crash-atomic (data
flushed before the metadata commit; copy-on-write to a fresh extent). The streaming `write_at` path
wrote data direct, so a crash mid-chunk left torn data - caught by the data CRC on read, but not
recovered. `WriteAtJ` (op 28) stages the chunk's data blocks in the transaction (`data_stage`) so
the chunk commits **atomically** through the journal - replayed or discarded on crash, never torn.
Opt-in per write (default `WriteAt` stays direct); bounded to one chunk by the 64-block journal (no
whole-file atomicity - stated honestly, not faked). Proven by `osdev test fs-djournal`.

**Scrub (Phase K, §6.14).** Every block self-verifies *on read*; `Scrub` (op 29, `drives scrub`)
makes that *proactive* - a READ-ONLY walk of the tree verifying every block's CRC, reporting
`(files, dirs, bad, scanned)` and writing nothing (distinct from `Check`, which repairs). `scrub`/
`scrub_subtree` are `&self`. Without redundancy it DETECTS bit-rot but cannot repair it; the cadence
is operator-driven (no background-task primitive - "periodic" is policy, not a hidden timer, §26.4).
Proven by `osdev test fs-scrub`.

This is the transactional metadata recovery §6.3/§15 calls for. With it, fs no longer
*needs* to be non-restartable on crash-safety grounds - dropping fs + block-driver from the
TCB is the remaining Phase D step (a `CLAUDE.md` §6 amendment).
