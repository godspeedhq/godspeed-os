# services/fs/

Filesystem service. TCB member in v1 (§6.1). **Non-restartable in v1.**

## Why it's in the TCB (v1)

fs owns persistent state for the system (§15). It cannot persist its own metadata to itself; the block driver holds a direct hardware capability for metadata storage. v2 goal: transactional metadata recovery so fs becomes restartable (§6.3).

## Dependencies

- `block-driver`: all I/O goes through block-driver IPC.
- `registry`: registers its endpoint so supervisor and other services can find it.

## On-disk format: GSFS, hierarchical (magic `GSFS0004`)

The format is **GSFS0004** (`docs/persistence.md` §6.4 + §6.6). Three on-disk structures
and no more — a **superblock**, a **free bitmap** (1 bit/block, the only global structure),
and a **directory tree** of self-describing 64-byte `file_record`s (no inode table, no
inode number, no global file cap; the tree *is* the index, the bitmap is the allocation
map, reclamation is intrinsic). 512-byte blocks (= one AHCI sector = one block-IPC
request), all capacity fields **u64** (~8 ZiB ceiling).

**GSFS0004 adds integrity checksums** (no layout churn otherwise):

- **Superblock** (LBA 0): magic, version (4), `block_size`, `total_blocks:u64`,
  `bitmap_start/blocks:u64`, `data_start:u64`, `root_first_block/block_count:u64`,
  `free_blocks:u64`, `flags` (DEFAULT bit), `label`, `journal_start/blocks:u64` @108
  (reserved crash-consistency region, filled by Phase C), and a **`sb_crc32` @124** over
  the first 124 bytes — **verified on mount, loud refusal on mismatch** (§3.12).
- **Free bitmap** (LBA `bitmap_start..journal_start`): set bit = used block.
- **Journal** (LBA `journal_start..data_start`): reserved, 64 blocks (32 KiB).
- **Directory block** (512 B): **7** `file_record`s × 64 B (`type u8`, `name_len u8`,
  `name[38]`, `size u64`, `first_block u64`, `block_count u64`) + a 64-byte trailer whose
  first 4 bytes are the **block's CRC32** over its 448-byte record region — verified on
  every directory read (`dir_read`), stamped on every write (`dir_write`).

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

## State and persistence (§15)

fs holds the inode table in memory (built from the on-disk table at mount); directory
blocks are read/written on demand. All writes are write-through to block-driver — no
write-back cache.

The filesystem cannot recover from a crash that leaves a partial write in progress. v1
accepts this: both block-driver and fs are non-restartable. The only recovery mechanism
is a full system reboot. Transactional metadata recovery is Phase 3 (the TCB-drop work,
§6.3).
