# services/fs/

Filesystem service. **Restartable, NOT a TCB member** (Phase D amendment, §6.1, 2026-06-17).

## Restartable (it once was trusted root)

fs owns persistent state (§15) and for v1 was trusted because it could not recover from a crash
mid-write. The crash-consistency journal (Phase C, `docs/persistence.md` §6.8) closes that: every
metadata mutation commits atomically and fs **recovers to a consistent state on mount**. So fs is
now restartable — the kernel notifies the supervisor of its death, which respawns it; fs re-mounts
(recovering via the journal), re-registers with the registry, and clients reacquire it and retry
(§14.3). Only its *boot-time* spawn must succeed (§11.3). Pinned by §22 Test 13.

## Dependencies

- `block-driver`: all I/O goes through block-driver IPC.
- `registry`: registers its endpoint so supervisor and other services can find it.

## On-disk format: GSFS, hierarchical (magic `GSFS0005`)

The format is **GSFS0005** (`docs/persistence.md` §6.4 + §6.6 + §6.10). Three on-disk
structures and no more — a **superblock**, a **free bitmap** (1 bit/block, the only global
structure), and a **directory tree** of self-describing 64-byte `file_record`s (no inode
table, no inode number, no global file cap; the tree *is* the index, the bitmap is the
allocation map, reclamation is intrinsic). 512-byte blocks (= one AHCI sector = one block-IPC
request), all capacity fields **u64** (~8 ZiB ceiling).

**Every block self-verifies with a CRC32** (GSFS0005):

- **Superblock** (LBA 0): magic, version (5), `block_size`, `total_blocks:u64`,
  `bitmap_start/blocks:u64`, `data_start:u64`, `root_first_block/block_count:u64`,
  `free_blocks:u64`, `flags` (DEFAULT bit), `label`, `journal_start/blocks:u64` @108
  (reserved crash-consistency region, filled by Phase C), and a **`sb_crc32` @124** over
  the first 124 bytes — **verified on mount, loud refusal on mismatch** (§3.12).
- **Free bitmap** (LBA `bitmap_start..journal_start`): set bit = used block. (Not checksummed —
  reconstructible from the tree.)
- **Journal** (LBA `journal_start..data_start`): reserved, 64 blocks (32 KiB).
- **Directory block** (512 B): **7** `file_record`s × 64 B (`type u8`, `name_len u8`,
  `name[38]`, `size u64`, `first_block u64`, `block_count u64`) + a 64-byte trailer whose
  first 4 bytes are the **block's CRC32** over its 448-byte record region — verified on
  every directory read (`dir_read`/`td_read`), stamped on every write (`dir_write`/`td_write`).
- **File-data block** (512 B): **508 bytes payload + CRC32 @508**. A file of N bytes spans
  `ceil(N/508)` data blocks; the CRC covers the payload, verified on every read (`data_read`),
  stamped on every write (`data_write`). The per-message streaming chunk is `7×508 = 3556`.

The CRC32 (IEEE 802.3) lives in `src/crc32.rs`; `osdev` carries a byte-identical copy so a
host-baked image checksums exactly as `fs` would.

Bounded & loud (§26.6): the only ceiling is the disk (the bitmap); directories grow;
contiguous file extents; no POSIX permission bits (authority is by capability, §3.3), no
hard links. Bad magic **or** bad CRC is a loud mount refusal, never an auto-reformat (§3.12).

## Exposed interface (via IPC — name field carries a path `/a/b/c`)

| Request     | Op | Args              | Response |
|-------------|----|-------------------|----------|
| `WriteFile` | 10 | path, data bytes  | `Ok` / `Err` |
| `ReadFile`  | 11 | path              | size + file bytes / `NotFound` |
| `StatFile`  | 12 | path              | exists, size:u64, is_dir |
| `Mkdir`     | 13 | path              | `Ok` / `Err` |
| `ListDir`   | 14 | path              | `{name, is_dir}` entries |
| `WriteNew`  | 24 | path, total:u64   | `Ok` / `Err` — create/truncate sized for a large file |
| `WriteAt`   | 25 | path, offset:u64, chunk | `Ok` / `Err` — write a chunk (block-aligned offset) |
| `ReadAt`    | 26 | path, offset:u64, len:u32 | `Ok`, n:u32, bytes / `NotFound` |

**Large files (streaming).** `WriteFile`/`ReadFile` carry a whole *small* file in one ≤4 KiB
IPC message. Files larger than one message use the offset-addressed ops: `WriteNew` allocates
the full extent and sizes the file, then a sequence of `WriteAt` chunks fills it; read it back
with `StatFile` (for the size) + a sequence of `ReadAt` chunks. Stateless — each request is
self-contained (no open-file table; §8). The on-disk file is a contiguous u64 extent, so size
is bounded only by free space (fragmentation/grow-relocation is the known contiguous-extent
limitation; a block-list is a deferred refinement, §26.2). The shell streams `cat`/`copy` and
the pipe `write` sink through these ops.

## State and persistence (§15)

fs holds only superblock geometry + a maintained free count in memory (no inode table —
the tree and bitmap live on disk, read on demand). Directory blocks are read/written on
demand.

**Crash-consistency (Phase C, `docs/persistence.md` §6.8).** Every metadata mutation runs
as one **atomic transaction** through the reserved journal region: structural writes
(directory/bitmap/superblock) are staged in memory (read-your-writes), then `commit_txn`
writes them to the journal, writes a checksummed commit record (the atomic point),
checkpoints them home, and invalidates the journal. File **data** is written direct (into an
extent nothing references until the transaction commits). On **mount**, `recover` replays a
committed-but-unfinished transaction (valid commit magic + CRC) and discards a torn one — so a
single power loss leaves the filesystem either entirely unchanged or fully applied, never
half-updated. A transaction stages ≤ `TXN_CAP` (56) blocks (loud failure past that);
`delete_tree` commits the unlink atomically then reclaims the subtree in bounded per-extent
transactions. Proven by `osdev test fs-journal`.

This is the transactional metadata recovery §6.3/§15 calls for. With it, fs no longer
*needs* to be non-restartable on crash-safety grounds — dropping fs + block-driver from the
TCB is the remaining Phase D step (a `CLAUDE.md` §6 amendment).
