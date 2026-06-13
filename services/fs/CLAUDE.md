# services/fs/

Filesystem service. TCB member in v1 (§6.1). **Non-restartable in v1.**

## Why it's in the TCB (v1)

fs owns persistent state for the system (§15). It cannot persist its own metadata to itself; the block driver holds a direct hardware capability for metadata storage. v2 goal: transactional metadata recovery so fs becomes restartable (§6.3).

## Dependencies

- `block-driver`: all I/O goes through block-driver IPC.
- `registry`: registers its endpoint so supervisor and other services can find it.

## On-disk format: GSFS, hierarchical (magic `GSFS0002`)

Phase 2 evolved the flat name→blob store into a real **hierarchical** filesystem
(`docs/persistence.md` §6.2/§6.3). 512-byte blocks (= one AHCI sector = one block-IPC
request), all capacity fields **u64** (~8 ZiB ceiling):

- **Superblock** (LBA 0): magic, version, `block_size`, `total_blocks:u64`,
  `inode_table_start/blocks:u64`, `data_start:u64`, `next_free_block:u64`,
  `root_inode:u32`, `flags` (DEFAULT bit, for `drives`).
- **Inode table**: `INODE_COUNT` (256) slots × 64 bytes. Each inode: `type`
  (free|file|dir), `size:u64`, `first_block:u64`, `block_count:u64`.
- **Directory**: a dir inode whose single data block holds 16 entries × 32 bytes
  (`name_len:u8`, `name[27]`, `inode:u32`). Path walking starts at `root_inode`.

Bounded & loud (§26.6): fixed inode count, fixed name length, one block per directory,
contiguous file extents via a bump allocator (overwrite leaks the old extent — a Phase-1
carry-over). No POSIX permission bits (authority is by capability, §3.3), no hard links.
Bad superblock magic is a loud mount refusal, never an auto-reformat (§3.12).

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
