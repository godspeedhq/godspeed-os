# Persistence: Block Driver + Filesystem (v2)

> **Status:** Design doc, non-normative. Like everything in `docs/`, this trails the
> spec (`CLAUDE.md`) and does not amend it. Where this doc and the constitution
> disagree, the constitution wins. Items that *would* change the constitution (a new
> capability mechanism, dropping a service from the TCB) are called out as requiring a
> recorded `CLAUDE.md` amendment with sign-off.

## 1. Scope and milestone

This is **v2 work**, not v1. The v1 milestone (§23) is complete — multi-core boot,
ping↔pong cross-core IPC, supervisor restart. Real persistence was explicitly **out of
scope for v1** (§23.4, "Filesystem persistence beyond the trusted block driver"), and
§6.3 names the remaining v2 work directly: *"the remaining v2 work is block-driver / fs."*

So this effort is the defining piece of v2: **make `block-driver` and `fs` real, and put
them on the path out of the TCB** (§6.3). It is held to the v2 bar (restartability,
honest crash semantics), not the v1 "a trusted stub is acceptable" bar.

**Terminology note.** To avoid colliding with the project's version numbers, the
filesystem's internal build stages are called **Phase 1, Phase 2, …**, never "v1/v2".
The component is called **the filesystem** (service name `fs`); it has no invented brand
name.

## 2. Decisions at a glance

| Decision | Choice | One-line reason |
|----------|--------|-----------------|
| Filesystem | **Our own, from first principles** | ext4/btrfs are POSIX + enormous; both fights the constitution (§2.4, §3.3, §26.11). No interop requirement to justify the cost. |
| Authority model | **File = capability** (north star) | Authority by capability, not by mode bits (§3.3). Extends the capability model instead of bolting a permission model beside it. |
| Block device | **ATA PIO (legacy IDE)** | Simplest correct device; **no DMA → least-privilege by construction**; works in QEMU and has a hardware path; stepping-stone to AHCI. |
| Namespace | **Hierarchical (directories + paths)** — adopted; Phase 1 shipped *flat* | Flat name→blob isn't realistic for real use; real directories from the start (§6.2). Still no links/permissions (authority is by capability, §3.3). |
| Bounds | **Fixed, bounded** | Like the rest of the system (queue depth 16, MAX_ENDPOINTS): fixed file count / name length, no unbounded trees (§26.6). |
| Crash model (Phase 1) | **Write-through, honest loss** | A crash mid-write may lose that write; refuse to mount on bad magic (§3.12). Transactional recovery is Phase 3 (§6.3). |

## 3. Why not ext4 / btrfs / a standard filesystem

A standard on-disk format buys exactly one thing — **interop** (read Linux's disks, let
Linux read ours) — and GodspeedOS has no interop requirement (it runs on a dedicated
machine/VM, Appendix A.5, storing its own services' state). Against zero benefit, the
costs are disqualifying:

1. **They encode identity-based authority.** Every ext4 inode carries `uid/gid/mode`.
   That is *"user X may read this file"* — precisely the identity-based authority §3.3
   forbids (*"All authority is explicit. Capabilities, not identity."*). In our world the
   authority to touch a file is *holding the capability to it*; those inode fields would
   be dead, contradictory machinery.
2. **They blow the whiteboard rule.** §26.11 makes "explainable by one engineer in 30
   minutes" a hard requirement. ext4 is tens of thousands of lines (extent trees, htree
   directories, journaling); btrfs is ~140k lines of CoW B-trees, checksums, subvolumes,
   snapshots, RAID. Neither is whiteboardable. Importing either violates the one thing §26
   exists to protect — *"The Model Is The Product"* (§26.1).
3. **"Porting" is really a rewrite** (the Appendix D.6 argument, applied to a filesystem):
   they assume a POSIX VFS, a page cache, kernel locking, `errno`. You reimplement from
   the on-disk-format spec — and that format is the part you marry forever.

So: a deliberately minimal filesystem, **pulled into existence by what §15 needs**
(§26.2), not by what "a real filesystem has."

### 3.1 Why not just FAT (we already need it for the boot ESP)?

FAT is the sharpest version of the question — the UEFI boot ESP *must* be FAT, so
why not use FAT (or exFAT) for data too and skip GSFS? Note first that the §3
*permission* argument does **not** apply here: FAT has no `uid/gid/mode`, so it
does not clash with capability authority the way ext4 does. The real reasons:

1. **"We already need FAT" is softer than it looks.** GodspeedOS implements **zero
   FAT** today — the ESP is built by host tooling (`osdev image`) and read by Limine;
   the kernel has never touched a FAT byte. And a **minimal, write-once boot ESP**
   (a few static files, or a stamped prebuilt blob) is *far* simpler than a **general
   read/write/grow/delete data filesystem**. Making a boot ESP does not hand us a free
   general FAT.
2. **A *correct* FAT is not "simple."** It carries 40 years of compatibility cruft —
   8.3 names, the VFAT long-filename + checksum scheme, FAT-chain allocation, FSInfo,
   two FAT copies. A clean inode+directory GSFS is **comparable-or-less** code than a
   correct VFAT, and it is *ours*: our fields (the `drives` `DEFAULT` flag + drive
   `label`), our evolution path (file-as-capability, §7), no legacy, no exFAT patents.
3. **No interop requirement** (§3) — FAT's one real win is universal readability on
   other OSes, which we do not need.

> **Reconsider-if:** the deciding factor is *not* "FAT is bad" — it is that we'd have
> to implement FAT inside the OS anyway and a correct one isn't simpler, plus we have
> no interop need. **If reading GodspeedOS data drives on Windows/Mac/Linux ever
> becomes a goal, FAT/exFAT deserve a serious second look.** Recorded so a future
> contributor sees the trade was made with eyes open, not by reflex.

### 3.2 Capacity is the wrong axis — the real difference is features

A natural question is "how much can GSFS hold vs ext4/NTFS/APFS/ZFS?" The honest
answer is that **capacity is just field width** and a poor way to compare:

| Filesystem | Max file | Max volume |
|------------|----------|-----------|
| FAT32 | 4 GiB | 2 TiB |
| exFAT | 16 EiB | 128 PiB |
| NTFS | 8 PiB | 8 PiB |
| ext4 | 16 TiB | 1 EiB |
| APFS | 8 EiB | 8 EiB |
| ZFS | 16 EiB | 256 ZiB (2¹²⁸-addressable) |
| **GSFS (today)** | 4 GiB (u32 size) | 2 TiB (u32 blocks × 512 B) |
| **GSFS (u64 fields)** | 16 EiB | ~8 ZiB |

GSFS is FAT32-class **today only because Phase-1 chose 32-bit fields**; widening
`size` + `total_blocks` to `u64` is a one-line change that vaults it past ext4. So
capacity does not distinguish these filesystems. What ext4/NTFS/APFS/ZFS actually buy
is **features** — journaling (crash-safety), checksums (bit-rot), snapshots,
copy-on-write, B-tree metadata that stays fast at millions of files. **GSFS
deliberately has none yet** — that, not size, is its real trade-off: §26.6/§20
minimalism now, with journaling arriving as the Phase-3 transactional-recovery work
(§6.3) only when a real need pulls it in.

## 4. Layering and responsibilities

```text
  clients (supervisor, services)         hold a cap to fs (Phase 1)
        │  ipc: ReadFile / WriteFile / Stat / List / Delete   or, later, a per-file cap
        ▼
  fs service        owns the on-disk FORMAT: superblock, entry table, free bitmap,
        │            name→blob logic, capability minting (Phase 2). No hardware access.
        │  ipc: ReadBlocks(lba,count) / WriteBlocks(lba,data)
        ▼
  block-driver      owns the DEVICE: ATA PIO port I/O. No knowledge of files.
        │  port I/O (hw_pio cap)            Translates LBA ↔ sectors. No DMA.
        ▼
  ATA disk (QEMU if=ide; real disk later)
```

The line between `fs` and `block-driver` is the same line every microkernel draws:
**policy (file layout) above, mechanism (move sectors) below.** `block-driver` never
learns what a file is; `fs` never touches a port.

## 5. Block driver — ATA PIO

### 5.1 Why ATA PIO

- **No DMA → least-privilege by construction.** A PIO driver moves sectors through I/O
  ports one 16-bit word at a time; it *cannot* DMA anywhere. So it never holds the
  DMA-anywhere reach that forced H1 (the IOMMU work) into existence — it is not
  kernel-equivalent *even without an IOMMU*. That is a strictly cleaner TCB story than a
  DMA block driver, and it is what eventually lets `block-driver` leave the TCB on its own
  merits (§6.3), independent of IOMMU presence.
- **Simplest correct device** (§2.4, §20): no feature negotiation, no descriptor rings,
  no command lists. Set LBA, issue command, poll status, transfer.
- **Hardware path + stepping-stone:** works in QEMU (`if=ide`, already the boot transport)
  and potentially on the T630 in legacy/IDE mode; ATA's command set (READ/WRITE SECTORS,
  LBA) carries directly into a future AHCI driver. (virtio-blk, by contrast, is a
  QEMU-only paravirtual device that runs on no real hardware.)

The cost — PIO is slow — is irrelevant for v2 persistence (§20: correctness over
performance). We optimize transfer width later, never at the cost of clarity.

### 5.2 The new mechanism it needs: `hw_pio`

ATA PIO uses **port-mapped I/O** (ports `0x1F0–0x1F7` / `0x3F6` for the primary channel),
not memory-mapped I/O. Today the SDK exposes MMIO (`Mmio::read32` etc.) but not port I/O.
So Phase 1 adds a small, audited mechanism, mirroring the existing MMIO story:

- A **`hw_pio` capability** in the contract: a port range the driver may touch, validated
  and granted at spawn exactly like `hw_mmio`.
- **SDK port-I/O wrappers** (`Pio::inb/outb/inw/outw/insw/outsw`) in an audited
  `sdk/rust/src/pio.rs`, each block carrying a `// SAFETY:` comment — the same isolation
  §18.1 already grants the MMIO/DMA accessor modules. The driver service itself stays
  `unsafe`-free behind the safe wrappers.

The kernel already performs port I/O for the serial console, so the kernel-side cost is
modest. **This is a real new authority surface; it must be added to §18.1's audited
hardware/ABI layer and the unsafe audit, and `hw_pio` documented as a capability kind.**

### 5.3 ATA PIO read/write (the whole protocol)

```text
  read sector (LBA28, polled):
    outb(0x1F6, 0xE0 | ((lba >> 24) & 0x0F))   # drive 0, LBA mode, top nibble
    outb(0x1F2, count)                          # sector count
    outb(0x1F3, lba & 0xFF)                     # LBA low
    outb(0x1F4, (lba >> 8) & 0xFF)              # LBA mid
    outb(0x1F5, (lba >> 16) & 0xFF)             # LBA high
    outb(0x1F7, 0x20)                           # READ SECTORS
    poll 0x1F7 until (status & BSY)==0 && (status & DRQ)!=0   # or error bit → IoError
    insw(0x1F0, buf, 256)                       # 256 words = 512 bytes
  write sector: same addressing, command 0x30 (WRITE SECTORS), outsw, then CACHE FLUSH (0xE7).
```

Polled completion in Phase 1 (no interrupt handler) keeps it dead simple; the
`hw_interrupt` cap in the contract can stay unused until we want interrupt-driven
completion (a later optimization, not a correctness need).

## 6. Filesystem — on-disk format

A flat **name → blob** store. Proposed geometry (concrete but tunable in Phase 1):

```text
  filesystem block = 4 KiB = 8 ATA sectors      (matches the system's 4 KiB granularity)

  ┌───────────────┬──────────────────┬───────────────┬────────────────────────┐
  │ Block 0       │ Blocks 1..E       │ Blocks E..F    │ Blocks F..end           │
  │ Superblock    │ Entry table       │ Free bitmap    │ Data region             │
  │ magic,version,│ MAX_FILES entries │ 1 bit / data   │ file contents,          │
  │ geometry,     │ fixed-size        │ block          │ block-aligned           │
  │ checksum      │                   │                │                         │
  └───────────────┴──────────────────┴───────────────┴────────────────────────┘
```

**Superblock** — `magic` (refuse to mount if wrong — loud failure, never auto-reformat,
§3.12), `version`, `block_size`, `total_blocks`, `entry_table_blocks`, `bitmap_blocks`,
`data_start`, and a `checksum` over the superblock.

**Entry** (fixed-size, fills the entry table) — `name[NAME_MAX]`, `size_bytes`,
`first_block`, `block_count`, `generation`, `flags` (in-use / free).

**Allocation (Phase 1): contiguous extents.** Each file occupies a contiguous run of data
blocks (`first_block .. first_block+block_count`). This is the whiteboard-simplest scheme
and a perfect fit for the actual workload — mostly write-once blobs (service binaries,
state snapshots). The known limitation is fragmentation and that growing a file may
require relocation; both are acceptable Phase 1 and revisited only if a real need pulls
(block lists / extents) into existence (§26.2).

**Proposed bounds** (in the spirit of queue-depth-16, MAX_ENDPOINTS — bounded everything,
§26.6): `BLOCK_SIZE = 4096`, `NAME_MAX = 64`, `MAX_FILES = 256`. Final numbers set in
Phase 1; the point is they are *fixed and stated*, not elastic.

### 6.1 Why bulk data is chunked and copied (a constitution consequence)

Two invariants shape the data path and are worth making explicit:

- **§8.5:** max IPC message is 4 KiB, kernel-copied.
- **§2.5:** zero-copy IPC is *permanently rejected* — so `fs` and `block-driver` may **not**
  share a buffer; bulk data crosses between them as copied messages.

Therefore a large read (e.g. a 200 KiB service binary) is inherently a sequence of
message-sized, copied transfers — `fs` requests blocks from `block-driver` in chunks that
fit one message (≤ ~4 KiB minus header; Phase 1 may start at a single 512 B sector per
message for simplicity, widening later to several sectors per message). This is slower
than a shared-buffer design *by construction*, and that is the accepted cost of the
no-shared-memory invariant — a clean illustration of §20 (correctness and clarity over
performance) and §26.7 (the copy is the honest, bounded behavior).

### 6.2 Hierarchical evolution — real directories (adopted)

The flat single-entry-table format above shipped in **Phase 1** (mount, named files,
reboot survival — all working). It is being **evolved to a real hierarchical
filesystem**: flat name→blob isn't realistic for actual use, so GSFS gets **real
directories from the start** of the next phase. (See `docs/drives.md` for the
user-facing path/addressing model, `[N:]label/path/to/file`.)

The hierarchical format, still GodSpeed-minimal (bounded, no POSIX permissions —
authority is by capability, §3.3 — and no hard links). **All capacity-bearing fields
are `u64` from the start** (block counts, file sizes, block pointers, the block-IPC
LBA) — see §6.3:

```text
  Superblock (LBA 0): magic "GSFS…", version, block_size, total_blocks:u64,
      inode_table_start/blocks, data_start, next_free_block:u64,
      root_inode, flags (DEFAULT bit), label[N]      ← drives.md: default + identity
  Inode table: fixed array of inodes. Each inode:
      type (free | file | dir), size_bytes:u64, first_block:u64, block_count:u64
  Directory: a file (inode.type = dir) whose contents are entries:
      { name[NAME_MAX], inode_number }               ← name → inode within this dir
  Data region: file contents + directory blocks.
```

### 6.3 Capacity: `u64` fields from day one (decided)

Phase 1 used `u32` for block counts + file size — FAT32-class limits (2 TiB volume,
4 GiB file, §3.2). The hierarchical format **widens every capacity-bearing field to
`u64`**: `total_blocks`, `next_free_block`, inode `size_bytes`/`first_block`/
`block_count`, **and the block-IPC `lba`** (`ReadBlock`/`WriteBlock` carry an 8-byte
LBA, and `block-driver`/AHCI already use a `u64` LBA). These must move *together* —
widening only some unlocks nothing.

The resulting ceiling is effectively unlimited: **~8 ZiB volume** (2⁶⁴ × 512 B) and a
**16 EiB max file** — past ext4, into ZFS territory. The cost is a handful of bytes per
superblock/inode; the bound is *stated and fixed*, not elastic (§26.6). This is the
"capacity is field width" point (§3.2) acted on: pick the wide field once, at the
format's birth, and never revisit the ceiling.

> **Why now, not retrofitted onto the flat format:** the flat Phase-1 format is being
> replaced by this hierarchical one, so `u64` is baked into the *new* format at design
> time rather than churned into throwaway code. `u64` is the convention from here on.

- **Path walking:** resolve `/a/b/c` by reading the **root inode** (a directory),
  finding `a` → its inode, reading that directory, finding `b`, … Each component is
  one directory lookup. Bounded path depth + entries-per-directory (§26.6).
- **Operations:** `mkdir` (allocate a dir inode + add an entry to the parent),
  create/`write` (allocate a file inode + entry), `ls` (read a directory's entries),
  `cat`/read (walk to the file inode, read its extent), `cd` (resolve a directory,
  update the session's current-directory inode).
- **Still bounded & loud:** fixed inode count, fixed name length, contiguous extents
  (no reclamation yet, Phase-1 carry-over); bad magic still refuses to mount (§3.12).
- **The `label` + `flags(DEFAULT)` superblock fields** are what `drives` uses for
  drive identity (invariant 11) and the persistent default drive (`docs/drives.md`).

This supersedes the flat §6 format as the target; the flat version remains the
historical Phase-1 record.
## 7. File = capability (the north star)

The spine that makes this filesystem *ours* rather than a generic store: a file is named
and reached **by capability**, consistent with §3.3 and §7.

### 7.1 The mechanism problem

The kernel has **no concept of a file** (§4.4 anti-scope — no filesystem logic in the
kernel), yet **only the kernel can mint unforgeable capabilities** (§7.3). "A file is a
capability" must bridge those two facts. Three options were weighed:

| Option | What it is | Verdict |
|--------|-----------|---------|
| Bearer token | `fs` returns a 128-bit unguessable handle, presented per call | A *service-level token*, not a kernel cap — forgeable by guessing, sits beside the capability model. Rejected as the north star (weak claim). |
| Endpoint-per-open-file | `fs` creates a kernel endpoint per open file; client holds a real SEND cap | Genuinely unforgeable, but every endpoint costs ~64 KiB (our own efficiency measurement) — heavy with many open files. Rejected for cost. |
| **Kernel-delegated resource caps** | Extend the kernel's `ResourceId+Rights+Generation` model so a service can ask the kernel to mint a cap for a *service-defined* resource it owns | **Chosen.** Real kernel caps (unforgeable, revocable, generationed) with **no file logic in the kernel** — it tracks an opaque resource owned by `fs`. Generalizes the capability model; useful beyond files. |

### 7.2 How kernel-delegated resource caps work

The kernel already keys capabilities on `ResourceId` with a `Generation` (§7.2, §7.5). The
extension: a service (`fs`) **owns** a band of resource IDs and asks the kernel to mint
caps for them with chosen `Rights`. The kernel:

- mints/validates/revokes exactly as for any resource — generation bump invalidates every
  outstanding file cap at once (§7.5), giving `fs` clean revocation (delete a file → bump
  → all its caps go stale, surfacing as the usual `CapRevoked`/`EndpointDead`-class error);
- never learns what the resource *means*. `fs` maps `ResourceId → file`. A read/write cap
  to a file is then a first-class capability the holder can validate, be denied, or have
  revoked — identical machinery to endpoint caps.

This keeps every capability property (§7.3: unforgeable, non-escalating, scoped,
revocable, generationed) true for files, while honoring §4.4 (kernel stays file-agnostic).

> **This is a capability-model extension and will require a recorded `CLAUDE.md`
> amendment (§7 / §4.4) with sign-off** before Phase 2 lands. It is called out here as a
> known constitutional change, not slipped in.

### 7.3 Phasing

- **Phase 1 — authority = the cap to `fs`.** Holding `ipc_send=["fs"]` is the authority;
  files are addressed by name in the request. This ships *working, reboot-surviving
  persistence* without the new kernel mechanism.
- **Phase 2 — per-file capabilities** via §7.2. `fs` returns a file cap on create/open;
  read/write present the cap. File-as-capability becomes *true*, not approximate.

## 8. IPC protocols (proposed)

**Client ↔ fs** (Phase 1, name-addressed):

| Request | Args | Reply |
|---------|------|-------|
| `WriteFile` | name, data (chunked) | `Ok` / `NoSpace` / `IoError` |
| `ReadFile` | name | data (chunked) / `NotFound` / `IoError` |
| `StatFile` | name | `{exists, size}` |
| `ListFiles` | — | names (chunked) |
| `DeleteFile` | name | `Ok` / `NotFound` |

**fs ↔ block-driver:**

| Request | Args | Reply |
|---------|------|-------|
| `ReadBlocks` | lba, count (message-bounded) | sector data / `IoError` |
| `WriteBlocks` | lba, data (message-bounded) | `Ok` / `IoError` |

All replies are exactly one of `{Ok-with-data, defined error}` — never silent
(§3.12, mirrors the IPC `send` discipline of §8.6).

## 9. Crash, restart, and the TCB trajectory (§6.3)

- **Phase 1 (write-through, in the TCB).** Writes go straight to disk; no journal. A crash
  mid-write can corrupt the file being written (and the entry table if it strikes during a
  metadata update). On mount, a bad superblock magic/checksum is a **loud refusal**, never
  a silent reformat. `block-driver` and `fs` remain TCB members (§6.1); their death is a
  panic+reboot (§6.2) — the v1 posture, carried because nothing transactional exists yet.
- **Phase 3 (transactional, out of the TCB).** The §6.3 goal: give `fs` atomic-commit
  semantics so a restart can recover to a consistent state, then **drop `fs` and
  `block-driver` from the TCB** (a recorded `CLAUDE.md` §6 amendment with sign-off). A
  **log-structured** layout is the natural route — appends with an atomic commit record
  make crash-consistency fall out for free, and pair well with the no-overwrite discipline.
  Because the ATA PIO driver has no DMA reach (§5.1), `block-driver` can leave the TCB on
  its own merits without depending on IOMMU presence — a cleaner exit than the DMA drivers
  had.

## 10. Phased build plan

1. **Block driver read path. ✅ done** (`osdev test blockdev`). Added the `hw_pio`
   grant (kernel-mediated `PortRead`/`PortWrite` syscalls validated per access,
   grant store in `capability/hw_pio.rs`), SDK `pio.rs` (`Pio`), and `block-driver`
   ATA-PIO reads sector 0 of a QEMU secondary-channel `if=ide` disk and logs it —
   verified by reading back a host-written magic. Port I/O is kernel-mediated because
   ring-3 drivers cannot run `in`/`out` (granting IOPL would be ambient authority).
2. **Block driver write + round-trip. ✅ done** (`osdev test blockdev`, case P1.2). The
   driver writes a known pattern to a scratch LBA (WRITE SECTORS + FLUSH CACHE), reads it
   back, and asserts equal — proving the device read/write path end to end. All in the
   driver via the existing `Pio` wrapper; no new kernel surface.
3. **Filesystem mount + format. ✅ done** (`osdev test blockdev`, case P1.3). Host-side
   `osdev mkfs <image>` writes the superblock (magic `GSFS0001`, version, block_size,
   total_blocks) into LBA 0; `block-driver` gained an IPC server loop (`ReadBlock` /
   `WriteBlock`, the per-request reply-cap pattern); `fs` mounts by reading LBA 0 over
   that IPC (SDK `request_with_reply`), validating the magic loudly, and logging the
   geometry. The entry table + free bitmap are written by `mkfs` in a later step (no
   files exist yet). Forward `fs → block-driver` cap wires via `send_peers` (block-driver
   spawns first, kernel auto-registers its endpoint name); the reply rides a per-request
   cap fs embeds.
4. **Filesystem read/write (name→blob). ✅ done** (`osdev test blockdev`, case P1.4).
   On-disk entry table (16 entries × 32 B, one block) + a bump allocator (next-free-block
   in the superblock; contiguous extents, no reclamation yet). `fs` stores/retrieves named
   files: `write_file` allocates an extent, writes the data blocks + entry table +
   superblock through block-driver `WriteBlock`; `read_file` walks the entry's extent via
   `ReadBlock`. Verified by a mount-time round-trip (`greeting`). `fs` also serves the
   client API (`WriteFile`/`ReadFile`/`StatFile`, ops 10–12) over IPC via the reply-cap
   pattern.
5. **Reboot survival (the headline). ✅ done** (`osdev test blockdev-reboot`, case P1.5).
   Format once; boot 1 — `fs` creates `greeting`; **reboot on the same disk image without
   reformatting** — boot 2 mounts (superblock persisted: `next_free=3, 1 files`) and reads
   the file back byte-for-byte (`fs: persisted file 'greeting' verified across boot`). The
   disk is a host file, so this is the real durability guarantee. **Phase 1 complete.**
6. **Phase 2: file-as-capability.** Kernel-delegated resource caps (after the §7 amendment
   is signed off); `fs` returns/validates per-file caps.

## 11. Test plan — QEMU is sufficient (and authoritative here)

Unlike the H1 IO_PAGE_FAULT (which QEMU's lenient `amd-iommu` could not show), persistence
is a case where **QEMU gives a real, trustworthy answer** and the whole feature can be
built and verified headless, away from the T630:

- **The disk is a host file** (`-drive file=disk.img,if=ide`). It survives across QEMU
  runs by definition, so reboot-survival is the real thing: boot → write → quit → boot →
  read. No flashing.
- **ATA PIO is faithfully emulated** — real BSY/DRQ handshake, real status/error bits, real
  sector semantics. What passes in QEMU behaves the same on an ATA controller.
- **Format + filesystem logic are hardware-independent** — bytes in blocks.

The single thing QEMU cannot answer is whether the **T630's storage controller speaks ATA
PIO in legacy mode** — a separate, later hardware bring-up question, not a filesystem
question. Test layout follows the §22 pattern: an identity-style **reboot-survival** test
(write, reboot, read, assert) is the executable form of "persistence persists," plus
property tests (round-trip any bytes), fuzz (malformed superblock → loud refuse, never
panic), and chaos (crash mid-write → mount refuses or recovers, never silently corrupts).

## 12. Open questions (to resolve as phases land)

- Final bounds (`MAX_FILES`, `NAME_MAX`, transfer width per message).
- Exact `hw_pio` capability syntax in the contract schema (range form, like `hw_mmio`).
- The precise §7 amendment wording for kernel-delegated resource caps (Phase 2).
- Whether Phase 3 is log-structured or journaled-update (decide when transactional
  recovery is actually built; both reach the §6.3 goal).
- Bare-metal storage controller for the T630 (ATA legacy mode vs. an eventual AHCI driver)
  — deferred; does not block any QEMU phase.
