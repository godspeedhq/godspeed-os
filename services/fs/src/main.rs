// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `fs` - userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **GSFS0008 - checksummed scalable format with extent lists (docs/persistence.md §6.4 +
//! §6.6 + §6.12).** Three on-disk
//! structures and no more: a **superblock**, a **free bitmap** (1 bit/block, read on
//! demand - the only global structure, a free *map* not a file index), and the
//! **directory tree** of **self-describing `file_record` entries** (`{type, name, size,
//! first_block, block_count}` - no inode table, no inode number, no global file cap). The
//! directory tree *is* the index (walk a path from root); the bitmap is the allocation
//! map; reclamation is intrinsic (`delete`/overwrite free bits). Directories **grow**
//! (reallocate a bigger extent when full) so there is no per-directory entry cap either.
//! The only ceiling is the disk. (`fs_index`, the deferred global enumeration cache, is
//! §6.5 - not built; built when a `find`/search need pulls it in.)
//!
//! `fs` is the single owner of the filesystem (§8): it serves one IPC request at a time,
//! so every mutation is serialized - no concurrency to reconcile. All disk I/O goes through
//! `block-driver` over IPC; this service touches no hardware. Raw-tolerant: a bad magic is
//! a loud refusal, never an auto-format (§3.12).

#![no_std]
#![no_main]

use godspeed_sdk::{CapHandle, Message, ServiceContext};

mod crc32;
use crc32::crc32;

// ── On-disk format - MUST match `osdev format_superblock` (persistence.md §6.6/§6.10/§6.15). ──
// GSFS0008: every block self-verifies with a CRC32 - the superblock (CRC @136), each
// directory block (CRC trailer @448), and each **file-data block** (508-byte payload +
// CRC32 @508). Corruption is a loud refusal (§3.12), never silent. The format reserves a
// fixed journal region (Phase-C crash-consistency) so the on-disk geometry is baked once.
//
// **The magic is FROZEN at "GSFS0008" (§6.15, Phase L).** Versioning moved off the magic and onto
// three FEATURE-MASK words so the format can evolve WITHOUT a reformat-only bump: a newer feature
// is a bit in one of the masks, set on disk only when actually used, and the mount policy decides
// what an older build does with a bit it doesn't recognise (see `mount`). The magic answers only
// "is this GSFS?"; the masks answer "what does this disk use?".
const SB_MAGIC: &[u8; 8] = b"GSFS0008";
const SB_VERSION: u32 = 8;
const BLOCK: usize = 512;
const BITS_PER_BMBLOCK: u64 = (BLOCK as u64) * 8; // 4096 bits per bitmap block

// Superblock feature masks (GSFS0008, §6.15). Three u32 words after the journal geometry, under
// the widened superblock CRC (now @136 over [0..136)). Each bit is a feature the disk USES:
//   compat    - an older build that doesn't know the bit may still mount READ-WRITE (safe to ignore)
//   ro_compat - ... may mount READ-ONLY (reading is safe; writing could corrupt the feature)
//   incompat  - ... must REFUSE to mount (the on-disk structure is fundamentally different)
const FEAT_COMPAT_OFF: usize = 124;    // u32
const FEAT_RO_COMPAT_OFF: usize = 128; // u32
const FEAT_INCOMPAT_OFF: usize = 132;  // u32
const SB_CRC_OFF: usize = 136;         // u32 CRC32 over [0..136) - moved from @124, covers the masks

// Defined feature bits + what THIS build understands. A disk bit outside the KNOWN_* set drives the
// mount policy above. (As features are added post-0008, define a bit here - never a new magic.)
const FEAT_COMPAT_BACKUP_SB: u32 = 0x1; // a backup superblock sits at the last LBA (Phase F)
const FEAT_INCOMPAT_EXTENTS: u32 = 0x1; // some file is fragmented (extent list, Phase I) - needed to read it
const KNOWN_COMPAT: u32 = FEAT_COMPAT_BACKUP_SB;
const KNOWN_RO_COMPAT: u32 = 0;
const KNOWN_INCOMPAT: u32 = FEAT_INCOMPAT_EXTENTS;

// Extent lists (GSFS0008). A file is normally a single contiguous extent (`ITYPE_FILE`:
// first_block..first_block+block_count is the data - the fast path). When no contiguous run is
// free, the file becomes FRAGMENTED (`ITYPE_FILE_FRAG`): its `first_block` points to a single
// CRC'd **extent block** and `block_count` = 1. The extent block lists the data runs, so a big
// file can be stored across scattered free space. Bounded (§26.6): one extent block, so up to
// EXT_MAX runs - a file fragmented into more pieces than that is refused loudly.
const EXT_N_OFF: usize = 0;        // u32: number of {start,len} extents
const EXT_ENTRIES_OFF: usize = 8;  // {start:u64, len:u64} pairs
const EXT_ENTRY_SIZE: usize = 16;
const EXT_CRC_OFF: usize = 508;    // u32 CRC32 over [0..508)
const EXT_MAX: usize = (EXT_CRC_OFF - EXT_ENTRIES_OFF) / EXT_ENTRY_SIZE; // 31 runs per extent block

// File-data block: 508 bytes of payload + a 4-byte CRC32 trailer @508 (GSFS0008). A file of N
// bytes spans ceil(N/508) data blocks; each carries the CRC of its own payload, verified on
// every read. (Directory blocks use a different split - 448 records + CRC; superblock/bitmap/
// journal blocks are raw, with their own integrity schemes.)
const DATA_PAYLOAD: usize = 508;
const DATA_CRC_OFF: usize = DATA_PAYLOAD; // 508 - u32 CRC32 of the 508-byte payload

// file_record entry: 64 bytes. GSFS0008 fits 7 per 512-byte directory block and reserves
// the last 64 bytes as a trailer holding the block's CRC32 (over the 448-byte record
// region). The record layout itself is unchanged from GSFS0003 - names stay 38 bytes.
const REC_SIZE: usize = 64;
const RECS_PER_BLOCK: usize = 7; // 7×64 = 448 bytes of records + a 64-byte CRC trailer
const DIR_REC_REGION: usize = RECS_PER_BLOCK * REC_SIZE; // 448 - CRC covers [0..448)
const DIR_CRC_OFF: usize = DIR_REC_REGION; // 448 - u32 CRC32 of the record region
const NAME_MAX: usize = 38; // entry: type u8 @0, name_len u8 @1, name[38] @2, size @40, first @48, count @56

// Crash-consistency journal region (GSFS0008 geometry). Fixed size, bounded (§26.6): a
// transaction larger than this is refused loudly, never partially applied.
const JOURNAL_BLOCKS: u64 = 64; // 64 × 512 B = 32 KiB
// One commit/header block + up to TXN_CAP data blocks must fit the journal region.
const TXN_CAP: usize = 56; // max structural blocks one transaction may stage
const JOURNAL_MAGIC: u32 = 0x474A_3034; // "GJ04" - marks a committed transaction
const COMMIT_CRC_OFF: usize = 508; // commit record: CRC32 of [0..8+n*8] lives at @508

// Recursive-delete depth cap (§26.6). Paths are capped well below this by the wire
// `path_len` (u8) and the shell's PATH_MAX (120), so this is a backstop, not the binding
// limit - a too-deep tree is refused loudly rather than risking the service stack.
const MAX_TREE_DEPTH: u32 = 64;
const LABEL_MAX: usize = 31; // superblock: label_len u8 @76, label[31] @77

const ITYPE_FREE: u8 = 0;
const ITYPE_FILE: u8 = 1;      // inline single contiguous extent (first_block, block_count)
const ITYPE_DIR: u8 = 2;
const ITYPE_FILE_FRAG: u8 = 3; // fragmented file: first_block → extent block, block_count = 1

// Per-message data chunk: the most file payload bytes that travel in one IPC message - exactly
// 7 data-block payloads, so a streaming WRITE_AT chunk is always whole blocks (no read-modify-
// write) and its byte offset stays block-aligned. NOT a file-size cap (large files cross many
// of these via WRITE_NEW/WRITE_AT/READ_AT). 7×508 + a few header bytes ≤ MAX_PAYLOAD (4096).
// The shell's IO_CHUNK must equal this.
const MAX_FILE_BYTES: usize = 7 * DATA_PAYLOAD; // 3556 - streaming chunk size (7 data blocks)

// Block IPC protocol (fs <-> block-driver). MUST match `services/block-driver`.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
const OP_CAPACITY: u8 = 3;
const OP_WRITE_ZEROS: u8 = 4; // [op, lba:u64, count:u64] - zero a run of blocks (fast format)
const BLK_OK: u8 = 0;

// fs file API (client <-> fs). `[op, path_len, path, (WriteFile: data | Rename/Move: tail)]`.
const OP_WRITE_FILE: u8 = 10;
const OP_READ_FILE: u8 = 11;
const OP_STAT_FILE: u8 = 12;
const OP_MKDIR: u8 = 13;
const OP_LIST_DIR: u8 = 14;
const OP_RENAME: u8 = 15;
const OP_DELETE: u8 = 16;
const OP_MOVE: u8 = 17;
const OP_MKDIR_P: u8 = 18; // mkdir creating any missing parent directories
const OP_DELETE_TREE: u8 = 19; // delete a file or a WHOLE subtree (recursive)
// drives API.
const OP_DRIVES_INFO: u8 = 20;
const OP_FLASH: u8 = 21;
const OP_LABEL: u8 = 22;
const OP_RESET: u8 = 23;
// Large-file streaming (offset-addressed; stateless - each request is self-contained, §8).
// A big file = WRITE_NEW (allocate the whole extent, size it) then a sequence of WRITE_AT
// chunks; read it back with STAT (for the size) + a sequence of READ_AT chunks.
const OP_WRITE_NEW: u8 = 24; // [op, plen, path, total:u64] - create/truncate `path` sized `total`
const OP_WRITE_AT: u8 = 25;  // [op, plen, path, offset:u64, chunk…] - write chunk at byte offset
const OP_READ_AT: u8 = 26;   // [op, plen, path, offset:u64, len:u32] → [FS_OK, n:u32, bytes]
const OP_CHECK: u8 = 27;     // fsck: rebuild bitmap+free from the tree, report CRC failures →
                             // [FS_OK, files:u32, dirs:u32, bad:u32, used:u64, free:u64]
const OP_WRITE_AT_J: u8 = 28; // [op, plen, path, offset:u64, chunk…] - like WRITE_AT but the
                             // chunk's data blocks are JOURNALED (Phase J): the chunk is applied
                             // atomically (crash → fully replayed or fully discarded, never torn).
                             // Bounded to one chunk (≤7 blocks) by the journal; default WRITE_AT
                             // stays direct. Opt-in per write - the caller chooses the guarantee.
const OP_SCRUB: u8 = 29;      // scrub (Phase K): READ-ONLY integrity sweep - walk the tree, verify
                             // every block's CRC, report → [FS_OK, files:u32, dirs:u32, bad:u32,
                             // scanned:u64]. Writes nothing (unlike CHECK, which repairs the bitmap).
const OP_OPEN: u8 = 30;       // file-as-capability (§7.10, P2): [op, plen, path, rights:u8] → mint a
                             // delegated resource for the file, reply [FS_OK] + the embedded FILE CAP.
                             // The client then operates the file by INVOKING that cap (no fs name in
                             // hand), the kernel badges the request with the resource id + right.
const FS_OK: u8 = 0;
const FS_ERR: u8 = 1;
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3;
const FS_DENIED: u8 = 4;     // op requires a right the file cap lacks (non-escalation, §7.3)

// File-cap operations - the FIRST payload byte of a badged `ResourceInvoke` (§7.10). The kernel
// has already validated the cap holds the invoked right; fs enforces that the op needs ≤ that right.
const FOP_READ: u8 = 1;  // [FOP_READ, offset:u64, len:u32]      → [FS_OK, n:u32, bytes]   (needs READ)
const FOP_WRITE: u8 = 2; // [FOP_WRITE, offset:u64, chunk…]      → [FS_OK]                 (needs WRITE)
const FOP_STAT: u8 = 3;  // [FOP_STAT]                           → [FS_OK, size:u64]       (needs READ)
const FOP_CLOSE: u8 = 4; // [FOP_CLOSE]  → [FS_OK]; revoke the resource + free the open-file slot (any holder)

// Capability right bits - MUST match the kernel `Rights` bitfield (§7.4) and the SDK `RIGHT_*`.
const RIGHT_READ: u8 = 1 << 0;
const RIGHT_WRITE: u8 = 1 << 1;
const RIGHT_GRANT: u8 = 1 << 4;

// Open-file table (file-as-capability): maps a delegated `ResourceId` → the file path it names, so
// a badged invoke (which carries only the resource id) resolves to a file. Bounded (§26.6): at most
// MAX_OPEN files open at once; the path is re-walked per op (handles the file being moved/deleted -
// the walk simply fails then). Lives on `Fs` (owned state, not a static - §3.9), reset on mount (an
// fs restart kills all outstanding file caps, §14.3, so the table starts empty).
//
// NOTE - do not naively raise these: `Fs` is a stack local returned by value from `mount`/`format`
// (it already holds the 28 KiB `txn_blk`), so bumping the table to 128 (let alone 256) overflows
// fs's stack and kills it on boot. A bigger table needs `Fs` (or this table) OFF the stack first.
const MAX_OPEN: usize = 64;
const OPEN_PATH_MAX: usize = 96;
#[derive(Clone, Copy)]
struct OpenFile {
    rid: u64, // 0 = free slot
    plen: u8,
    path: [u8; OPEN_PATH_MAX],
}

/// In-memory superblock view. No inode table - the tree lives on disk and is read on
/// demand; the bitmap likewise (this struct holds only geometry + the maintained free
/// count + the root's extent + drive label/flags).
struct Fs {
    total_blocks: u64,
    bitmap_start: u64,
    data_start: u64,
    journal_start: u64,
    journal_blocks: u64,
    root_first_block: u64,
    root_block_count: u64,
    free_blocks: u64,
    /// Whether `free_blocks` has been derived from the bitmap yet (Commandment III — derived lazily,
    /// never trusted from a persisted copy). False from mount until the first `ensure_free_count`;
    /// the incremental alloc/free updates apply only once it is true.
    free_known: bool,
    flags: u32,
    label: [u8; LABEL_MAX],
    label_len: u8,
    // Feature masks (GSFS0008, §6.15). Preserved across `persist_super`; `feat_incompat` gains
    // FEAT_INCOMPAT_EXTENTS the first time a file fragments. `read_only` is set at mount when the
    // disk carries an unknown `ro_compat` bit (mount degraded rather than refuse), and gates every
    // mutating op in `serve` (loud refusal, never a silent no-op).
    feat_compat: u32,
    feat_ro_compat: u32,
    feat_incompat: u32,
    read_only: bool,
    // Crash-consistency journal (Phase C). While `txn_active`, structural writes (directory,
    // bitmap, superblock) are STAGED here - with read-your-writes - instead of going to disk,
    // then committed atomically through the on-disk journal region (`commit_txn`). Data-block
    // writes bypass this and go direct. A staged set larger than `TXN_CAP` is refused loudly.
    txn_active: bool,
    txn_n: usize,
    txn_overflow: bool,
    txn_lba: [u64; TXN_CAP],
    txn_blk: [[u8; BLOCK]; TXN_CAP],
    // Test-only crash injection (set only by the `journal-crash-test` build): halt inside
    // `commit_txn` right after the commit record is durable but before the checkpoint, to
    // simulate a power loss at the worst moment. Always false in production.
    crash_after_commit: bool,
    // Open-file table (file-as-capability, §7.10): delegated ResourceId → file path. `rid == 0`
    // is a free slot. Reset on mount (an fs restart invalidates all outstanding file caps).
    open_files: [OpenFile; MAX_OPEN],
}

/// A decoded `file_record` plus where it lives, so it can be written back. `loc == None`
/// means the root directory (its extent lives in the superblock, it has no parent entry).
#[derive(Clone, Copy)]
struct Entry {
    itype: u8,
    size: u64,
    first_block: u64,
    block_count: u64,
    loc: Option<Loc>,
}
#[derive(Clone, Copy)]
struct Loc {
    block: u64,
    slot: usize,
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("fs: starting");

    // `block-driver` may still be re-initialising when we start - a chaos storm can restart it just
    // before (or alongside) us, or our cached cap may be stale after its restart. Retry the capacity
    // query - reacquiring its cap - until it reports a REAL disk, so we never mount a phantom
    // 0-capacity disk and then "serve" a broken filesystem (every read would fail until a manual
    // respawn - the bug `chaos max-carnage` exposed). Bounded by wall-clock time: on a genuinely
    // diskless machine block-driver reports 0 forever, so we fall through and serve raw (§3.12).
    const FS_BLOCK_WAIT_SECS: i64 = 8;
    const FS_BLOCK_RETRY_YIELDS: u32 = 4000;
    let capacity = {
        let start = ctx.datetime().epoch_secs();
        let mut cap = block_capacity(&ctx).unwrap_or(0);
        while cap == 0 && ctx.datetime().epoch_secs() - start < FS_BLOCK_WAIT_SECS {
            let _ = ctx.reacquire_via_registry("block-driver");
            for _ in 0..FS_BLOCK_RETRY_YIELDS { ctx.yield_cpu(); }
            cap = block_capacity(&ctx).unwrap_or(0);
        }
        cap
    };
    ctx.log_fmt(format_args!("fs: disk capacity = {} sectors ({} MiB)", capacity, capacity / 2048));

    // Raw-tolerant: a bad superblock is the normal state of a never-flashed drive (§3.12).
    let mut fs: Option<Fs> = match Fs::mount(&ctx) {
        Ok(f) => {
            ctx.log_fmt(format_args!(
                "fs: mounted GSFS0008 ({} blocks, bitmap {}..{}, root@{}, free count derived on demand)",
                f.total_blocks, f.bitmap_start, f.data_start, f.root_first_block
            ));
            Some(f)
        }
        Err(e) => {
            ctx.log_fmt(format_args!("fs: no filesystem ({}) - awaiting drives flash", e));
            None
        }
    };

    // TEST builds only (`--features selftest`): exercises the tree + reboot survival by
    // WRITING to the disk. Never enabled in production (it would pollute a user's disk).
    #[cfg(feature = "selftest")]
    if let Some(ref mut f) = fs {
        self_test(&ctx, f);
    }

    #[cfg(feature = "journal-crash-test")]
    if let Some(ref mut f) = fs {
        journal_crash_test(&ctx, f);
    }

    #[cfg(feature = "frag-test")]
    if let Some(ref mut f) = fs {
        frag_test(&ctx, f);
    }

    #[cfg(feature = "data-journal-test")]
    if let Some(ref mut f) = fs {
        data_journal_test(&ctx, f);
    }

    // Path C (Phase 4): no self-registration - the kernel name-directory records "fs" at spawn
    // (refreshed on restart), so the shell reacquires us by name via the directory (§14.3).

    ctx.log("fs: serving file API");
    loop {
        let msg = ctx.recv();
        // A delegated-resource badge (§7.10) is set ONLY by the kernel after it validated a real
        // file cap - so its presence means "this is a trusted file-cap invocation", impossible to
        // forge over the ordinary fs send-cap. No badge → a name-addressed request.
        let badge = ctx.last_recv_badge();
        let reply = match ctx.take_pending_cap() {
            Some(c) => c,
            None => continue,
        };
        match badge {
            Some((rid, right)) => serve_filecap(&ctx, &mut fs, rid, right, msg.payload_bytes(), reply),
            None => serve(&ctx, &mut fs, capacity, msg.payload_bytes(), reply),
        }
        ctx.remove_cap(reply);
    }
}

#[cfg(feature = "selftest")]
fn self_test(ctx: &ServiceContext, fs: &mut Fs) {
    const GREET: &[u8] = b"/greeting";
    const GREET_DATA: &[u8] = b"hello, persistence!";
    const DIR: &[u8] = b"/etc";
    const NESTED: &[u8] = b"/etc/motd";
    const NESTED_DATA: &[u8] = b"godspeed scalable fs";
    let mut buf = [0u8; MAX_FILE_BYTES];

    // Data-integrity probe: if a host-baked `/probe.bin` exists, read it - exercising the
    // per-data-block CRC (GSFS0008). A corrupt block is refused loudly by `data_read`, so the
    // read fails rather than returning garbage. (Used by `osdev test fs-corrupt` case 3.)
    if fs.walk(ctx, b"/probe.bin").is_some() {
        match fs.read_path(ctx, b"/probe.bin", &mut buf) {
            Some(n) => ctx.log_fmt(format_args!("fs: probe.bin read OK ({} bytes)", n)),
            None    => ctx.log("fs: probe.bin read FAILED (data integrity)"),
        }
    }

    if let Some(n) = fs.read_path(ctx, GREET, &mut buf) {
        if &buf[..n] == GREET_DATA {
            ctx.log("fs: persisted file 'greeting' verified across boot");
            if let Some(m) = fs.read_path(ctx, NESTED, &mut buf) {
                if &buf[..m] == NESTED_DATA {
                    ctx.log("fs: nested '/etc/motd' verified across boot");
                }
            }
            large_file_check(ctx, fs); // boot 2: verify the big file survived the reboot
            return;
        }
    }
    match fs.mkdir(ctx, DIR) {
        Ok(()) => ctx.log("fs: mkdir /etc OK"),
        Err(e) => ctx.log_fmt(format_args!("fs: mkdir /etc FAILED: {}", e)),
    }
    match fs.write_path(ctx, NESTED, NESTED_DATA) {
        Ok(()) => match fs.read_path(ctx, NESTED, &mut buf) {
            Some(n) if &buf[..n] == NESTED_DATA => ctx.log("fs: nested file round-trip OK (/etc/motd)"),
            _ => ctx.log("fs: nested round-trip MISMATCH"),
        },
        Err(e) => ctx.log_fmt(format_args!("fs: nested write FAILED: {}", e)),
    }
    match fs.write_path(ctx, GREET, GREET_DATA) {
        Ok(()) => match fs.read_path(ctx, GREET, &mut buf) {
            Some(n) if &buf[..n] == GREET_DATA => ctx.log("fs: file round-trip OK (greeting)"),
            _ => ctx.log("fs: file round-trip MISMATCH"),
        },
        Err(e) => ctx.log_fmt(format_args!("fs: write FAILED: {}", e)),
    }
    large_file_check(ctx, fs); // boot 1: write the big file (and verify the round-trip)
}

/// Large-file round-trip proof (selftest only): a 200 KiB file written/read in streaming
/// chunks via `write_new`/`write_at`/`read_at` - far past the one-message cap. The content
/// is a deterministic pattern generated and verified chunk-by-chunk, so no big buffer is
/// needed anywhere. Creates the file if absent (boot 1), then always verifies (boot 2 proves
/// it survived the reboot).
#[cfg(feature = "selftest")]
fn large_file_check(ctx: &ServiceContext, fs: &mut Fs) {
    const BIG: &[u8] = b"/big.bin";
    const N: u64 = 200 * 1024; // 204800 bytes - ~57 chunks
    let pat = |k: u64| -> u8 { (k.wrapping_mul(131).wrapping_add(7) & 0xFF) as u8 };

    let present = matches!(fs.walk(ctx, BIG), Some(e) if e.itype == ITYPE_FILE && e.size == N);
    if !present {
        if fs.write_new(ctx, BIG, N).is_err() { ctx.log("fs: large write_new FAILED"); return; }
        let mut chunk = [0u8; MAX_FILE_BYTES];
        let mut off = 0u64;
        while off < N {
            let len = (MAX_FILE_BYTES as u64).min(N - off) as usize;
            for i in 0..len { chunk[i] = pat(off + i as u64); }
            if fs.write_at(ctx, BIG, off, &chunk[..len], false).is_err() {
                ctx.log("fs: large write_at FAILED"); return;
            }
            off += len as u64;
        }
    }
    let mut buf = [0u8; MAX_FILE_BYTES];
    let mut off = 0u64;
    while off < N {
        let want = (MAX_FILE_BYTES as u64).min(N - off) as usize;
        match fs.read_at(ctx, BIG, off, want, &mut buf) {
            Some(n) if n == want => {
                for i in 0..n {
                    if buf[i] != pat(off + i as u64) { ctx.log("fs: large-file MISMATCH"); return; }
                }
            }
            _ => { ctx.log("fs: large-file READ FAILED"); return; }
        }
        off += want as u64;
    }
    ctx.log_fmt(format_args!("fs: large-file {} B round-trip OK", N));
}

/// Crash-consistency proof (`journal-crash-test` build only). Two-boot, same binary, same
/// disk: on boot 1 the file is absent, so it writes `/jcrash.txt` through a transaction that
/// **halts right after the commit record is durable but before the checkpoint** (simulated
/// power loss). On boot 2 the file is absent on its home blocks, but `mount`'s recovery
/// replays the committed transaction from the journal - so the file is present with the right
/// bytes, proving the write was atomic and survived the crash.
#[cfg(feature = "journal-crash-test")]
fn journal_crash_test(ctx: &ServiceContext, fs: &mut Fs) {
    const F: &[u8] = b"/jcrash.txt";
    const D: &[u8] = b"journal crash consistency proof";
    let mut buf = [0u8; MAX_FILE_BYTES];
    if let Some(n) = fs.read_path(ctx, F, &mut buf) {
        if &buf[..n] == D {
            ctx.log("fs: jcrash RECOVERED+VERIFIED across simulated crash");
        } else {
            ctx.log("fs: jcrash recovered but DATA MISMATCH");
        }
        return;
    }
    // Boot 1: file absent - write it, arming the crash so commit_txn halts post-commit.
    ctx.log("fs: jcrash boot1 - writing /jcrash.txt, will halt after commit record");
    fs.begin_txn();
    fs.crash_after_commit = true;
    let _ = fs.write_path(ctx, F, D);
    let _ = fs.commit_txn(ctx); // halts inside (armed) - control does not return
    ctx.log("fs: jcrash boot1 did NOT crash (unexpected)");
}

/// Extent-list / fragmentation proof (`frag-test` build only). Two-boot, same binary, same
/// disk. Boot 1: fill the disk with small files, then delete every other one so the only free
/// space left is scattered small gaps - no contiguous run survives. Write `/frag.bin`, a file
/// far bigger than any gap: contiguous allocation must FAIL and the fragmented
/// (`ITYPE_FILE_FRAG`) path engages, storing the data across the gaps via a CRC'd extent block.
/// Verify the read-back exactly. Boot 2: the same `/frag.bin` is re-read across the reboot,
/// proving the extent list persists. The disk is small (set by `osdev test fs-frag`).
#[cfg(feature = "frag-test")]
fn frag_test(ctx: &ServiceContext, fs: &mut Fs) {
    const FRAG: &[u8] = b"/frag.bin";
    const NB: u64 = 20 * DATA_PAYLOAD as u64; // 20 data blocks - far larger than any gap
    let pat = |k: u64| -> u8 { (k.wrapping_mul(151).wrapping_add(13) & 0xFF) as u8 };

    // Boot 2: the proof file already exists - re-verify it survived the reboot.
    if let Some(e) = fs.walk(ctx, FRAG) {
        let kind = if e.itype == ITYPE_FILE_FRAG { "FRAGMENTED" } else { "contiguous" };
        ctx.log_fmt(format_args!("fs: [frag] /frag.bin present after reboot ({}, {} B)", kind, e.size));
        if e.itype == ITYPE_FILE_FRAG && frag_verify(ctx, fs, FRAG, NB, pat) {
            ctx.log("fs: [frag] reboot re-read OK");
        } else {
            ctx.log("fs: [frag] reboot re-read FAILED");
        }
        return;
    }

    // Boot 1, step 1: fill the disk with 2-block files until no space remains.
    const FILL_BYTES: usize = 2 * DATA_PAYLOAD;
    let filler = [0xABu8; FILL_BYTES];
    let mut nm = [0u8; 12];
    let mut count = 0u32;
    loop {
        let path = frag_name(&mut nm, count);
        match fs.write_path(ctx, path, &filler) {
            Ok(()) => count += 1,
            Err(_) => break,
        }
        if count >= 4000 { break; } // safety bound (§26.6) - never reached on the test disk
    }
    ctx.log_fmt(format_args!("fs: [frag] filled {} files", count));

    // Step 2: delete every other file → free space becomes scattered ~2-block gaps.
    let mut i = 0u32;
    let mut deleted = 0u32;
    while i < count {
        let path = frag_name(&mut nm, i);
        if fs.delete(ctx, path).is_ok() { deleted += 1; }
        i += 2;
    }
    ctx.log_fmt(format_args!("fs: [frag] deleted {} files ({} free blocks scattered)", deleted, fs.free_blocks));

    // Step 3: write a 20-block file - no contiguous run that big exists, so it MUST fragment.
    if fs.write_new(ctx, FRAG, NB).is_err() { ctx.log("fs: [frag] write_new FAILED"); return; }
    let mut chunk = [0u8; MAX_FILE_BYTES];
    let mut off = 0u64;
    while off < NB {
        let len = (MAX_FILE_BYTES as u64).min(NB - off) as usize;
        for j in 0..len { chunk[j] = pat(off + j as u64); }
        if fs.write_at(ctx, FRAG, off, &chunk[..len], false).is_err() { ctx.log("fs: [frag] write_at FAILED"); return; }
        off += len as u64;
    }
    match fs.walk(ctx, FRAG) {
        Some(e) if e.itype == ITYPE_FILE_FRAG =>
            ctx.log_fmt(format_args!("fs: [frag] /frag.bin is FRAGMENTED ({} B across an extent list)", e.size)),
        Some(e) =>
            ctx.log_fmt(format_args!("fs: [frag] NOT fragmented (itype {}) - a contiguous run existed", e.itype)),
        None => { ctx.log("fs: [frag] /frag.bin vanished"); return; }
    }
    if frag_verify(ctx, fs, FRAG, NB, pat) {
        ctx.log("fs: [frag] write+read round-trip OK");
    } else {
        ctx.log("fs: [frag] write+read round-trip FAILED");
    }
}

/// Read `n` bytes of `path` in streaming chunks and check the deterministic pattern.
#[cfg(feature = "frag-test")]
fn frag_verify(ctx: &ServiceContext, fs: &Fs, path: &[u8], n: u64, pat: impl Fn(u64) -> u8) -> bool {
    let mut buf = [0u8; MAX_FILE_BYTES];
    let mut off = 0u64;
    while off < n {
        let want = (MAX_FILE_BYTES as u64).min(n - off) as usize;
        match fs.read_at(ctx, path, off, want, &mut buf) {
            Some(m) if m == want => {
                for j in 0..m { if buf[j] != pat(off + j as u64) { return false; } }
            }
            _ => return false,
        }
        off += want as u64;
    }
    true
}

/// Build the path `/f<n>` into `buf` for the fill/delete loop (frag-test only).
#[cfg(feature = "frag-test")]
fn frag_name(buf: &mut [u8; 12], n: u32) -> &[u8] {
    buf[0] = b'/';
    buf[1] = b'f';
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    let mut v = n;
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    let digits = &tmp[i..];
    buf[2..2 + digits.len()].copy_from_slice(digits);
    &buf[..2 + digits.len()]
}

/// Data-journaling proof (`data-journal-test` build only). Two-boot, same binary, same disk.
/// Boot 1: create `/jdata.bin` (its metadata commits, but its data blocks are still **zeros** on
/// disk), then do ONE **journaled** `write_at` (`journal = true`) through a transaction armed to
/// halt right after the commit record is durable but before the checkpoint - so the data blocks
/// live in the journal and were NEVER written to their home LBAs. Boot 2: `mount`'s recovery
/// replays the committed transaction, writing the data home; the file reads back exactly. This is
/// airtight: the home blocks were zeros (a zero block fails the data CRC), so a correct read can
/// only mean the journal supplied the data - proving the chunk was crash-atomic, not torn.
#[cfg(feature = "data-journal-test")]
fn data_journal_test(ctx: &ServiceContext, fs: &mut Fs) {
    const F: &[u8] = b"/jdata.bin";
    const N: u64 = 4 * DATA_PAYLOAD as u64; // 4 data blocks - comfortably within the journal
    let pat = |k: u64| -> u8 { (k.wrapping_mul(193).wrapping_add(29) & 0xFF) as u8 };

    // Boot 2: the file exists - its data must have been recovered from the journal.
    if fs.walk(ctx, F).is_some() {
        let mut buf = [0u8; MAX_FILE_BYTES];
        match fs.read_at(ctx, F, 0, N as usize, &mut buf) {
            Some(n) if n == N as usize && (0..n).all(|j| buf[j] == pat(j as u64)) =>
                ctx.log("fs: jdata RECOVERED+VERIFIED across simulated crash"),
            Some(_) => ctx.log("fs: jdata recovered but DATA MISMATCH"),
            None => ctx.log("fs: jdata read FAILED (data not recovered - home blocks still zero)"),
        }
        return;
    }

    // Boot 1: create the file (metadata commits direct; data blocks remain zero on disk), then a
    // journaled write_at that halts post-commit. The data is staged → journal, not yet home.
    ctx.log("fs: jdata boot1 - journaled write_at, will halt after commit record");
    if fs.write_new(ctx, F, N).is_err() { ctx.log("fs: jdata write_new FAILED"); return; }
    let mut chunk = [0u8; N as usize];
    for j in 0..chunk.len() { chunk[j] = pat(j as u64); }
    fs.begin_txn();
    fs.crash_after_commit = true;
    let _ = fs.write_at(ctx, F, 0, &chunk, true); // stages the data blocks in the txn
    let _ = fs.commit_txn(ctx); // halts inside (armed) - control does not return
    ctx.log("fs: jdata boot1 did NOT crash (unexpected)");
}

/// Dispatch one request and reply through the client's `reply` cap.
fn serve(ctx: &ServiceContext, vol: &mut Option<Fs>, capacity: u64, p: &[u8], reply: CapHandle) {
    let send = |bytes: &[u8]| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(bytes)); };
    if p.is_empty() {
        send(&[FS_ERR]);
        return;
    }

    // drives API - INFO/FLASH work on a raw disk; LABEL/RESET as below.
    match p[0] {
        OP_DRIVES_INFO => {
            // [FS_OK, mounted, capacity:u64, used:u64, flags:u8, label_len:u8, label…]
            let mut out = [0u8; 28 + LABEL_MAX];
            out[0] = FS_OK;
            out[2..10].copy_from_slice(&capacity.to_le_bytes());
            if let Some(f) = vol {
                out[1] = 1;
                // Derive the free count on demand (Commandment III, lazy). At interactive `drives` time
                // block-driver is up, so this succeeds; if it can't (transiently down) the count is
                // simply not-yet-known and is reported once a later query can read the bitmap.
                f.ensure_free_count(ctx);
                let used = f.total_blocks.saturating_sub(f.free_blocks);
                out[10..18].copy_from_slice(&f.total_blocks.to_le_bytes());
                out[18..26].copy_from_slice(&used.to_le_bytes());
                out[26] = f.flags as u8;
                let ll = (f.label_len as usize).min(LABEL_MAX);
                out[27] = ll as u8;
                out[28..28 + ll].copy_from_slice(&f.label[..ll]);
                send(&out[..28 + ll]);
            } else {
                send(&out[..28]);
            }
            return;
        }
        OP_FLASH => {
            if capacity == 0 { send(&[FS_ERR]); return; }
            let ll = if p.len() >= 2 { (p[1] as usize).min(LABEL_MAX) } else { 0 };
            let label = if p.len() >= 2 + ll { &p[2..2 + ll] } else { &[][..] };
            match Fs::format(ctx, capacity, label) {
                Ok(f) => { *vol = Some(f); send(&[FS_OK]); }
                Err(_) => send(&[FS_ERR]),
            }
            return;
        }
        OP_LABEL => {
            let ll = if p.len() >= 2 { (p[1] as usize).min(LABEL_MAX) } else { 0 };
            let label = if p.len() >= 2 + ll { &p[2..2 + ll] } else { &[][..] };
            match vol {
                Some(f) if f.read_only => {
                    ctx.log("fs: label refused - filesystem mounted READ-ONLY (unsupported ro_compat feature)");
                    send(&[FS_ERR]);
                }
                Some(f) => {
                    f.begin_txn();
                    let r = f.relabel(ctx, label);
                    send(&[match f.end_txn(ctx, r) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
                }
                None => send(&[FS_NOFS]),
            }
            return;
        }
        OP_RESET => {
            // Wipe BOTH superblock copies (primary @0 and backup @capacity-1), else mount would
            // "recover" the just-reset filesystem from the surviving backup.
            if capacity == 0 { send(&[FS_ERR]); }
            else {
                let z = [0u8; BLOCK];
                let ok = block_write(ctx, 0, &z) && block_write(ctx, capacity - 1, &z);
                if ok { *vol = None; send(&[FS_OK]); } else { send(&[FS_ERR]); }
            }
            return;
        }
        OP_CHECK => {
            // fsck: self-managed (not one transaction - rewrites the whole bitmap; idempotent).
            match vol {
                Some(f) if f.read_only => {
                    ctx.log("fs: check refused - filesystem mounted READ-ONLY (fsck writes the bitmap)");
                    send(&[FS_ERR]);
                }
                Some(f) => match f.check(ctx) {
                    Ok((files, dirs, bad, used)) => {
                        let mut out = [0u8; 29];
                        out[0] = FS_OK;
                        out[1..5].copy_from_slice(&files.to_le_bytes());
                        out[5..9].copy_from_slice(&dirs.to_le_bytes());
                        out[9..13].copy_from_slice(&bad.to_le_bytes());
                        out[13..21].copy_from_slice(&used.to_le_bytes());
                        out[21..29].copy_from_slice(&f.free_blocks.to_le_bytes());
                        send(&out);
                    }
                    Err(_) => send(&[FS_ERR]),
                },
                None => send(&[FS_NOFS]),
            }
            return;
        }
        OP_SCRUB => {
            // scrub: READ-ONLY integrity sweep - verify every referenced block's CRC, report,
            // change nothing on disk (distinct from `check`, which repairs the bitmap). Op-driven
            // (the operator/policy sets the cadence; no hidden background task, §26.4).
            match vol {
                Some(f) => match f.scrub(ctx) {
                    Ok((files, dirs, bad, scanned)) => {
                        let mut out = [0u8; 21];
                        out[0] = FS_OK;
                        out[1..5].copy_from_slice(&files.to_le_bytes());
                        out[5..9].copy_from_slice(&dirs.to_le_bytes());
                        out[9..13].copy_from_slice(&bad.to_le_bytes());
                        out[13..21].copy_from_slice(&scanned.to_le_bytes());
                        send(&out);
                    }
                    Err(_) => send(&[FS_ERR]),
                },
                None => send(&[FS_NOFS]),
            }
            return;
        }
        _ => {}
    }

    // File ops require a mounted filesystem.
    if p.len() < 2 { send(&[FS_ERR]); return; }
    let fs = match vol {
        Some(f) => f,
        None => { send(&[FS_NOFS]); return; }
    };
    let op = p[0];
    // Read-only mount (unknown ro_compat feature, §6.15): refuse every mutating op LOUDLY rather
    // than silently dropping the write. Reads (STAT/READ/READ_AT/LIST) pass through.
    if fs.read_only && op_is_mutating(op) {
        ctx.log("fs: write refused - filesystem mounted READ-ONLY (unsupported ro_compat feature)");
        send(&[FS_ERR]);
        return;
    }
    let plen = p[1] as usize;
    if p.len() < 2 + plen { send(&[FS_ERR]); return; }
    let path = &p[2..2 + plen];
    let tail = &p[2 + plen..]; // WriteFile data, or Rename newname, or Move dst-path
    // Run a metadata-mutating op as ONE atomic transaction: stage its structural writes, then
    // commit them through the journal (`end_txn`). A crash before the commit record leaves the
    // fs unchanged; after it, the op is replayed on the next mount. (delete_tree manages its
    // own transactions; write_at writes only data, so neither is wrapped here.)
    macro_rules! txn {
        ($e:expr) => {{
            fs.begin_txn();
            let r = $e;
            send(&[match fs.end_txn(ctx, r) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }};
    }
    match op {
        OP_WRITE_FILE => txn!(fs.write_path(ctx, path, tail)),
        OP_READ_FILE => {
            let mut buf = [0u8; MAX_FILE_BYTES];
            match fs.read_path(ctx, path, &mut buf) {
                Some(n) => {
                    let mut out = [0u8; 5 + MAX_FILE_BYTES];
                    out[0] = FS_OK;
                    out[1..5].copy_from_slice(&(n as u32).to_le_bytes());
                    out[5..5 + n].copy_from_slice(&buf[..n]);
                    send(&out[..5 + n]);
                }
                None => send(&[FS_NOTFOUND]),
            }
        }
        OP_WRITE_NEW => {
            if tail.len() < 8 { send(&[FS_ERR]); return; }
            let total = u64_at(tail, 0);
            txn!(fs.write_new(ctx, path, total));
        }
        OP_WRITE_AT => {
            if tail.len() < 8 { send(&[FS_ERR]); return; }
            let offset = u64_at(tail, 0);
            let chunk = &tail[8..];
            // Direct (not journaled): no transaction - the fast streaming path (§6.8 data model).
            send(&[match fs.write_at(ctx, path, offset, chunk, false) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }
        OP_WRITE_AT_J => {
            if tail.len() < 8 { send(&[FS_ERR]); return; }
            let offset = u64_at(tail, 0);
            let chunk = &tail[8..];
            // Journaled (Phase J): stage the chunk's data blocks in a transaction so it commits
            // atomically (crash → replayed or discarded, never torn). Bounded to one chunk.
            fs.begin_txn();
            let r = fs.write_at(ctx, path, offset, chunk, true);
            send(&[match fs.end_txn(ctx, r) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }
        OP_READ_AT => {
            if tail.len() < 12 { send(&[FS_ERR]); return; }
            let offset = u64_at(tail, 0);
            let len = (u32_at(tail, 8) as usize).min(MAX_FILE_BYTES);
            let mut buf = [0u8; MAX_FILE_BYTES];
            match fs.read_at(ctx, path, offset, len, &mut buf) {
                Some(n) => {
                    let mut out = [0u8; 5 + MAX_FILE_BYTES];
                    out[0] = FS_OK;
                    out[1..5].copy_from_slice(&(n as u32).to_le_bytes());
                    out[5..5 + n].copy_from_slice(&buf[..n]);
                    send(&out[..5 + n]);
                }
                None => send(&[FS_NOTFOUND]),
            }
        }
        OP_STAT_FILE => {
            let mut out = [0u8; 11];
            out[0] = FS_OK;
            match fs.walk(ctx, path) {
                Some(e) => {
                    out[1] = 1;
                    out[2..10].copy_from_slice(&e.size.to_le_bytes());
                    out[10] = (e.itype == ITYPE_DIR) as u8;
                }
                None => out[1] = 0,
            }
            send(&out);
        }
        OP_MKDIR => txn!(fs.mkdir(ctx, path)),
        OP_MKDIR_P => txn!(fs.mkdir_parents(ctx, path)),
        OP_LIST_DIR => match fs.list_dir(ctx, path) {
            Some(out) => send(&out),
            None => send(&[FS_NOTFOUND]),
        },
        OP_RENAME => txn!(fs.rename(ctx, path, tail)),
        OP_DELETE => txn!(fs.delete(ctx, path)),
        // delete_tree manages its own transactions (unlink + batched frees) - not wrapped.
        OP_DELETE_TREE => send(&[match fs.delete_tree(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_MOVE => txn!(fs.move_path(ctx, path, tail)),
        OP_OPEN => {
            // [op, plen, path, rights:u8] → mint a delegated resource for the file and reply
            // [FS_OK] with the FILE CAP embedded; the client then operates the file by invoking
            // that cap (§7.10). `open_file` sends its own reply (it must embed the cap), so we
            // only send FS_ERR if it failed before replying.
            let want = if tail.is_empty() { 0 } else { tail[0] & (RIGHT_READ | RIGHT_WRITE) };
            if fs.open_file(ctx, path, want, reply).is_err() { send(&[FS_ERR]); }
        }
        _ => send(&[FS_ERR]),
    }
}

/// Serve a **file-cap invocation** (§7.10) - a message the kernel badged with the delegated
/// `resource_id` + the `right` it validated. The badge is unforgeable (set only by the kernel
/// after the cap check), so reaching here means the caller holds a real, live cap. We resolve the
/// resource id → the open file's path, enforce that the operation needs **≤ the validated right**
/// (the load-bearing non-escalation check, §7.3 - a READ cap can never write), and act.
fn serve_filecap(ctx: &ServiceContext, vol: &mut Option<Fs>, rid: u64, right: u8, p: &[u8], reply: CapHandle) {
    let send = |bytes: &[u8]| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(bytes)); };
    let fs = match vol { Some(f) => f, None => { send(&[FS_NOFS]); return; } };
    if p.is_empty() { send(&[FS_ERR]); return; }
    let fop = p[0];

    // FOP_CLOSE needs no path (the holder retires its own handle).
    if fop == FOP_CLOSE {
        let _ = ctx.resource_revoke(rid); // gen bump → this cap (and any copies) go stale
        fs.open_free(rid);
        send(&[FS_OK]);
        return;
    }

    // Resolve the resource id → path (copied out, so `fs` can be borrowed mutably below).
    let (path_buf, plen) = match fs.open_path(rid) {
        Some(x) => x,
        None    => { send(&[FS_NOTFOUND]); return; } // unknown/closed resource
    };
    let path = &path_buf[..plen];

    match fop {
        FOP_READ => {
            if right & RIGHT_READ == 0 { send(&[FS_DENIED]); return; } // op needs READ
            if p.len() < 13 { send(&[FS_ERR]); return; }
            let offset = u64_at(p, 1);
            let len = (u32_at(p, 9) as usize).min(MAX_FILE_BYTES);
            let mut buf = [0u8; MAX_FILE_BYTES];
            match fs.read_at(ctx, path, offset, len, &mut buf) {
                Some(n) => {
                    let mut out = [0u8; 5 + MAX_FILE_BYTES];
                    out[0] = FS_OK;
                    out[1..5].copy_from_slice(&(n as u32).to_le_bytes());
                    out[5..5 + n].copy_from_slice(&buf[..n]);
                    send(&out[..5 + n]);
                }
                None => send(&[FS_NOTFOUND]),
            }
        }
        FOP_WRITE => {
            if right & RIGHT_WRITE == 0 { send(&[FS_DENIED]); return; } // ← non-escalation: a READ cap can't write
            if fs.read_only { send(&[FS_ERR]); return; }
            if p.len() < 9 { send(&[FS_ERR]); return; }
            let offset = u64_at(p, 1);
            let chunk = &p[9..];
            send(&[match fs.write_at(ctx, path, offset, chunk, false) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }
        FOP_STAT => {
            if right & RIGHT_READ == 0 { send(&[FS_DENIED]); return; }
            match fs.walk(ctx, path) {
                Some(e) => {
                    let mut out = [0u8; 9];
                    out[0] = FS_OK;
                    out[1..9].copy_from_slice(&e.size.to_le_bytes());
                    send(&out);
                }
                None => send(&[FS_NOTFOUND]),
            }
        }
        _ => send(&[FS_ERR]),
    }
}

impl Fs {
    // ── mount / format / drive metadata ──────────────────────────────────────
    /// Validate a superblock copy: correct (frozen) magic AND CRC32 over the first `SB_CRC_OFF`
    /// bytes - which now includes the GSFS0008 feature masks (§3.12, §6.15).
    fn sb_valid(b: &[u8; BLOCK]) -> bool {
        &b[0..8] == SB_MAGIC && u32_at(b, SB_CRC_OFF) == crc32(&b[..SB_CRC_OFF])
    }

    /// Read the superblock, falling back to the **backup copy at the last LBA** if the primary
    /// (LBA 0) is unreadable or fails its CRC (GSFS0008). The backup is located via the device
    /// capacity, so it works even when the primary is unreadable (no chicken-and-egg). On a
    /// successful fallback the primary is healed (rewritten from the backup).
    fn read_superblock(ctx: &ServiceContext) -> Result<[u8; BLOCK], &'static str> {
        let primary = block_read(ctx, 0);
        if let Some(ref b) = primary {
            if Self::sb_valid(b) { return Ok(*b); }
        }
        // Primary missing/corrupt - try the backup at capacity-1.
        let cap = block_capacity(ctx).unwrap_or(0);
        if cap >= 2 {
            if let Some(bk) = block_read(ctx, cap - 1) {
                if Self::sb_valid(&bk) {
                    ctx.log("fs: primary superblock bad - recovered from backup superblock");
                    let _ = block_write(ctx, 0, &bk); // heal the primary copy
                    return Ok(bk);
                }
            }
        }
        match primary {
            Some(ref b) if &b[0..8] == SB_MAGIC =>
                Err("superblock checksum mismatch (both copies) - refusing to mount corrupt filesystem"),
            _ => Err("bad superblock magic - disk not formatted (run drives flash)"),
        }
    }

    fn mount(ctx: &ServiceContext) -> Result<Fs, &'static str> {
        let sb = Self::read_superblock(ctx)?;
        // Feature policy (GSFS0008, §6.15). The masks are valid because `sb_valid` (above) covers
        // them under the superblock CRC. An `incompat` bit this build doesn't know means the
        // on-disk structure is fundamentally different - REFUSE loudly (never a risky misread). An
        // unknown `ro_compat` bit means we can read but not safely write - mount READ-ONLY. Unknown
        // `compat` bits are safe to ignore. This is what lets the format evolve without a reformat.
        let feat_compat = u32_at(&sb, FEAT_COMPAT_OFF);
        let feat_ro_compat = u32_at(&sb, FEAT_RO_COMPAT_OFF);
        let feat_incompat = u32_at(&sb, FEAT_INCOMPAT_OFF);
        let unknown_incompat = feat_incompat & !KNOWN_INCOMPAT;
        if unknown_incompat != 0 {
            ctx.log_fmt(format_args!(
                "fs: refusing to mount - disk uses incompatible features (incompat mask {:#010x}, unknown {:#010x})",
                feat_incompat, unknown_incompat));
            return Err("disk uses incompatible features this build does not understand");
        }
        let read_only = (feat_ro_compat & !KNOWN_RO_COMPAT) != 0;
        if read_only {
            ctx.log_fmt(format_args!(
                "fs: mounting READ-ONLY - disk uses ro_compat features this build doesn't support (ro_compat mask {:#010x})",
                feat_ro_compat));
        }
        // Crash recovery: replay a committed-but-unfinished transaction before serving (§9).
        // Idempotent - a clean shutdown leaves no commit record, so this is a no-op then. (A
        // read-only mount still recovers: replaying an already-committed write is not a new write,
        // and leaving the fs torn would be worse - see §6.15.)
        Fs::recover(ctx, u64_at(&sb, 108));
        let mut label = [0u8; LABEL_MAX];
        let ll = (sb[76] as usize).min(LABEL_MAX);
        label[..ll].copy_from_slice(&sb[77..77 + ll]);
        let fs = Fs {
            total_blocks: u64_at(&sb, 16),
            bitmap_start: u64_at(&sb, 24),
            data_start: u64_at(&sb, 40),
            journal_start: u64_at(&sb, 108),
            journal_blocks: u64_at(&sb, 116),
            root_first_block: u64_at(&sb, 48),
            root_block_count: u64_at(&sb, 56),
            free_blocks: 0,
            // Derived LAZILY from the bitmap on first use (ensure_free_count) — Commandment III: the
            // bitmap is the one truth; sb[64] is not trusted. Deferring the scan keeps mount fast and
            // robust to a transient block-driver outage (a free-count read never fails the mount).
            free_known: false,
            flags: u32_at(&sb, 72),
            label,
            label_len: ll as u8,
            feat_compat,
            feat_ro_compat,
            feat_incompat,
            read_only,
            txn_active: false,
            txn_n: 0,
            txn_overflow: false,
            txn_lba: [0; TXN_CAP],
            txn_blk: [[0u8; BLOCK]; TXN_CAP],
            crash_after_commit: false,
            open_files: [OpenFile { rid: 0, plen: 0, path: [0u8; OPEN_PATH_MAX] }; MAX_OPEN],
        };
        Ok(fs)
    }

    // ── crash-consistency journal (Phase C) ──────────────────────────────────
    // Every metadata mutation runs as one atomic transaction: structural writes are staged
    // (`tb_write`/`td_write`) instead of going to disk, then `commit_txn` writes them to the
    // journal with a checksummed commit record (the atomic point), checkpoints them home, and
    // invalidates the journal. A crash before the commit record lands leaves home untouched
    // (discarded); a crash after it is replayed idempotently on the next mount (`recover`).
    // Reads honor staged writes (read-your-writes) so an op sees its own changes.

    /// Read a block, returning the staged version if this transaction has written `lba`.
    fn tb_read(&self, ctx: &ServiceContext, lba: u64) -> Option<[u8; BLOCK]> {
        if self.txn_active {
            let mut i = self.txn_n;
            while i > 0 {
                i -= 1;
                if self.txn_lba[i] == lba { return Some(self.txn_blk[i]); }
            }
        }
        block_read(ctx, lba)
    }

    /// Write a block: stage it in the active transaction (de-duplicating by `lba`), else write
    /// through to disk. Staging overflow is recorded and surfaces as a loud commit failure.
    fn tb_write(&mut self, ctx: &ServiceContext, lba: u64, data: &[u8; BLOCK]) -> bool {
        if self.txn_active {
            for i in 0..self.txn_n {
                if self.txn_lba[i] == lba { self.txn_blk[i] = *data; return true; }
            }
            if self.txn_n >= TXN_CAP { self.txn_overflow = true; return false; }
            self.txn_lba[self.txn_n] = lba;
            self.txn_blk[self.txn_n] = *data;
            self.txn_n += 1;
            true
        } else {
            block_write(ctx, lba, data)
        }
    }

    /// Directory-block read with CRC verify, honoring staged writes.
    fn td_read(&self, ctx: &ServiceContext, lba: u64) -> Option<[u8; BLOCK]> {
        let blk = self.tb_read(ctx, lba)?;
        if u32_at(&blk, DIR_CRC_OFF) != crc32(&blk[..DIR_REC_REGION]) {
            ctx.log_fmt(format_args!("fs: directory block CRC mismatch at lba {} - refusing", lba));
            return None;
        }
        Some(blk)
    }

    /// Directory-block write: stamp the CRC trailer, then stage/through via `tb_write`.
    fn td_write(&mut self, ctx: &ServiceContext, lba: u64, blk: &mut [u8; BLOCK]) -> bool {
        let c = crc32(&blk[..DIR_REC_REGION]);
        blk[DIR_CRC_OFF..DIR_CRC_OFF + 4].copy_from_slice(&c.to_le_bytes());
        self.tb_write(ctx, lba, blk)
    }

    fn begin_txn(&mut self) {
        self.txn_active = true;
        self.txn_n = 0;
        self.txn_overflow = false;
    }

    fn abort_txn(&mut self) {
        self.txn_active = false;
        self.txn_n = 0;
        self.txn_overflow = false;
    }

    /// Commit the staged transaction atomically: write the staged blocks into the journal, then
    /// a checksummed commit record (the atomic point), then checkpoint them to their home LBAs,
    /// then invalidate the journal. On overflow or any failure the transaction is dropped; if it
    /// failed before the commit record landed, home is untouched (the fs is unchanged).
    fn commit_txn(&mut self, ctx: &ServiceContext) -> Result<(), &'static str> {
        if self.txn_overflow { self.abort_txn(); return Err("transaction too large to commit atomically"); }
        let n = self.txn_n;
        if n == 0 { self.txn_active = false; return Ok(()); }
        // Snapshot so we can write through `block_write` while `txn_active` is cleared.
        self.txn_active = false;
        // 1. Stage the data blocks in the journal (journal_start+1 ..).
        for i in 0..n {
            if !block_write(ctx, self.journal_start + 1 + i as u64, &self.txn_blk[i]) {
                return Err("journal data write failed");
            }
        }
        // 2. Write the commit record - the atomic point. magic + n + home LBAs + CRC32.
        let mut commit = [0u8; BLOCK];
        commit[0..4].copy_from_slice(&JOURNAL_MAGIC.to_le_bytes());
        commit[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            commit[8 + i * 8..16 + i * 8].copy_from_slice(&self.txn_lba[i].to_le_bytes());
        }
        let crc = crc32(&commit[..8 + n * 8]);
        commit[COMMIT_CRC_OFF..COMMIT_CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
        if !block_write(ctx, self.journal_start, &commit) { return Err("journal commit write failed"); }
        // Test-only: simulate a power loss right here - commit record durable, home not yet
        // updated. The next mount must replay this transaction. (Never set in production.)
        if self.crash_after_commit {
            ctx.log("fs: [journal-crash-test] commit record durable - halting before checkpoint (simulated crash)");
            loop { ctx.yield_cpu(); }
        }
        // 3. Checkpoint: write each staged block to its home LBA.
        for i in 0..n {
            if !block_write(ctx, self.txn_lba[i], &self.txn_blk[i]) {
                // Commit is durable: the next mount will replay this transaction. Report, but
                // the data is safe - no corruption, only a deferred checkpoint.
                return Err("checkpoint write failed (will replay on next mount)");
            }
        }
        // 4. Invalidate the journal. The checkpoint above already landed every block home, so a
        // failure here is safe (the next mount re-replays this committed transaction idempotently) -
        // but log it loudly rather than silently re-replaying on every future mount (§26.7).
        if !block_write(ctx, self.journal_start, &[0u8; BLOCK]) {
            ctx.log("fs: journal invalidation failed post-commit (txn re-replays next mount; data safe)");
        }
        Ok(())
    }

    /// Commit on `Ok`, drop on `Err`. The single wrapper around a transactional operation.
    fn end_txn(&mut self, ctx: &ServiceContext, res: Result<(), &'static str>) -> Result<(), &'static str> {
        match res {
            Ok(()) => self.commit_txn(ctx),
            Err(e) => { self.abort_txn(); Err(e) }
        }
    }

    /// Free an extent as its own small transaction (used by recursive delete, where the subtree
    /// is already unlinked so each batch is independently consistent).
    fn free_run_txn(&mut self, ctx: &ServiceContext, first: u64, count: u64) -> Result<(), &'static str> {
        self.begin_txn();
        let r = self.free_run(ctx, first, count);
        self.end_txn(ctx, r)
    }

    /// Replay a committed-but-unfinished transaction at mount (idempotent). Called with the
    /// journal geometry from the just-validated superblock, before serving any request.
    fn recover(ctx: &ServiceContext, journal_start: u64) {
        let commit = match block_read(ctx, journal_start) { Some(b) => b, None => return };
        if u32_at(&commit, 0) != JOURNAL_MAGIC { return; }
        let n = u32_at(&commit, 4) as usize;
        if n == 0 || n > TXN_CAP || 8 + n * 8 > COMMIT_CRC_OFF { return; }
        if crc32(&commit[..8 + n * 8]) != u32_at(&commit, COMMIT_CRC_OFF) { return; }
        let mut replayed_ok = true;
        for i in 0..n {
            let lba = u64_at(&commit, 8 + i * 8);
            match block_read(ctx, journal_start + 1 + i as u64) {
                Some(blk) => if !block_write(ctx, lba, &blk) { replayed_ok = false; },
                None => replayed_ok = false,
            }
        }
        if replayed_ok {
            // Invalidate ONLY once every block landed home. On a replay I/O failure, leave the journal
            // intact so the next mount re-replays (idempotent) - never clear a half-applied commit
            // (§26.7: a silent clear here would lose a committed transaction).
            if !block_write(ctx, journal_start, &[0u8; BLOCK]) {
                ctx.log("fs: journal invalidation failed after recovery (re-replays next mount)");
            }
            ctx.log_fmt(format_args!("fs: journal recovered {} block(s) from an interrupted write", n));
        } else {
            ctx.log("fs: journal replay hit a block I/O error - left intact, re-replays on next mount");
        }
    }

    /// Format the disk as an empty GSFS0008 sized to `capacity`, then mount. Same layout
    /// `osdev format_superblock` writes. `drives flash`; only ever user-initiated (§3.12).
    fn format(ctx: &ServiceContext, capacity: u64, label: &[u8]) -> Result<Fs, &'static str> {
        let total_blocks = capacity;
        let bitmap_start: u64 = 1;
        let bitmap_blocks = (total_blocks + BITS_PER_BMBLOCK - 1) / BITS_PER_BMBLOCK;
        // Reserve the journal region between the bitmap and the data region (GSFS0008).
        let journal_start = bitmap_start + bitmap_blocks;
        let journal_blocks = JOURNAL_BLOCKS;
        let data_start = journal_start + journal_blocks;
        let root_first_block = data_start;
        let root_block_count: u64 = 1;
        let used_through = data_start + root_block_count;
        // The backup superblock occupies the last block (`total_blocks-1`); it is reserved out
        // of the data region, so the disk must be big enough for the system blocks + a backup.
        if total_blocks < used_through + 2 {
            return Err("disk too small for a filesystem");
        }
        let backup_lba = total_blocks - 1;
        let free_blocks = total_blocks - used_through - 1; // -1 for the reserved backup block

        let mut sb = [0u8; BLOCK];
        sb[0..8].copy_from_slice(SB_MAGIC);
        sb[8..12].copy_from_slice(&SB_VERSION.to_le_bytes());
        sb[12..16].copy_from_slice(&(BLOCK as u32).to_le_bytes());
        sb[16..24].copy_from_slice(&total_blocks.to_le_bytes());
        sb[24..32].copy_from_slice(&bitmap_start.to_le_bytes());
        sb[32..40].copy_from_slice(&bitmap_blocks.to_le_bytes());
        sb[40..48].copy_from_slice(&data_start.to_le_bytes());
        sb[48..56].copy_from_slice(&root_first_block.to_le_bytes());
        sb[56..64].copy_from_slice(&root_block_count.to_le_bytes());
        sb[64..72].copy_from_slice(&free_blocks.to_le_bytes());
        sb[72..76].copy_from_slice(&0u32.to_le_bytes());
        let ll = label.len().min(LABEL_MAX);
        sb[76] = ll as u8;
        sb[77..77 + ll].copy_from_slice(&label[..ll]);
        sb[108..116].copy_from_slice(&journal_start.to_le_bytes());
        sb[116..124].copy_from_slice(&journal_blocks.to_le_bytes());
        // Feature masks (GSFS0008): a fresh disk always has the backup superblock (compat), no
        // ro_compat feature, and no fragmented file yet (incompat gains EXTENTS lazily, on first
        // fragmentation - see `alloc_file`).
        sb[FEAT_COMPAT_OFF..FEAT_COMPAT_OFF + 4].copy_from_slice(&FEAT_COMPAT_BACKUP_SB.to_le_bytes());
        sb[FEAT_RO_COMPAT_OFF..FEAT_RO_COMPAT_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
        sb[FEAT_INCOMPAT_OFF..FEAT_INCOMPAT_OFF + 4].copy_from_slice(&0u32.to_le_bytes());
        let sb_crc = crc32(&sb[..SB_CRC_OFF]);
        sb[SB_CRC_OFF..SB_CRC_OFF + 4].copy_from_slice(&sb_crc.to_le_bytes());
        // Write the superblock to BOTH the primary (LBA 0) and the backup (last LBA) copies.
        if !block_write(ctx, 0, &sb) { return Err("superblock write failed"); }
        if !block_write(ctx, backup_lba, &sb) { return Err("backup superblock write failed"); }

        // Zero the bitmap region in one batched op (driver writes multi-sector zero runs -
        // keeps `drives flash` fast even on a 122 GB disk), then mark [0..used_through) used.
        if !block_write_zeros(ctx, bitmap_start, bitmap_blocks) { return Err("bitmap init failed"); }
        let mut b = 0u64;
        while b < used_through {
            let bm_blk = bitmap_start + b / BITS_PER_BMBLOCK;
            let mut blk = block_read(ctx, bm_blk).ok_or("bitmap read failed")?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let stop = used_through.min(base + BITS_PER_BMBLOCK);
            while b < stop {
                let w = (b - base) as usize;
                blk[w / 8] |= 1 << (w % 8);
                b += 1;
            }
            if !block_write(ctx, bm_blk, &blk) { return Err("bitmap write failed"); }
        }
        // Reserve the backup superblock's block (`backup_lba`) so the allocator never hands it
        // out for file data and overwrites the backup.
        {
            let bm_blk = bitmap_start + backup_lba / BITS_PER_BMBLOCK;
            let mut blk = block_read(ctx, bm_blk).ok_or("bitmap read failed")?;
            let w = (backup_lba % BITS_PER_BMBLOCK) as usize;
            blk[w / 8] |= 1 << (w % 8);
            if !block_write(ctx, bm_blk, &blk) { return Err("bitmap write failed"); }
        }
        // Empty root directory block (stamped with its CRC trailer via dir_write).
        let mut root = [0u8; BLOCK];
        if !dir_write(ctx, root_first_block, &mut root) { return Err("root dir init failed"); }
        // Clear the journal commit block so a re-flash of a previously-used disk can't replay a
        // stale transaction (a fresh host image is already zeroed here).
        if !block_write(ctx, journal_start, &[0u8; BLOCK]) { return Err("journal init failed"); }

        Fs::mount(ctx)
    }

    fn relabel(&mut self, ctx: &ServiceContext, label: &[u8]) -> Result<(), &'static str> {
        let ll = label.len().min(LABEL_MAX);
        self.label = [0u8; LABEL_MAX];
        self.label[..ll].copy_from_slice(&label[..ll]);
        self.label_len = ll as u8;
        self.persist_super(ctx)
    }

    /// Re-write the mutable superblock fields (free count, root extent, flags, label) from
    /// current in-memory state. Geometry (total/bitmap/data) is fixed at format.
    fn persist_super(&mut self, ctx: &ServiceContext) -> Result<(), &'static str> {
        let mut sb = self.tb_read(ctx, 0).ok_or("superblock read failed")?;
        sb[48..56].copy_from_slice(&self.root_first_block.to_le_bytes());
        sb[56..64].copy_from_slice(&self.root_block_count.to_le_bytes());
        sb[64..72].copy_from_slice(&self.free_blocks.to_le_bytes());
        sb[72..76].copy_from_slice(&self.flags.to_le_bytes());
        let ll = (self.label_len as usize).min(LABEL_MAX);
        sb[76] = ll as u8;
        for b in &mut sb[77..77 + LABEL_MAX] { *b = 0; }
        sb[77..77 + ll].copy_from_slice(&self.label[..ll]);
        // Journal geometry (@108..124) is fixed at format and rides through untouched.
        // Feature masks (GSFS0008): preserved across writes; `feat_incompat` may have gained
        // FEAT_INCOMPAT_EXTENTS since mount (first fragmentation), so write the live values.
        sb[FEAT_COMPAT_OFF..FEAT_COMPAT_OFF + 4].copy_from_slice(&self.feat_compat.to_le_bytes());
        sb[FEAT_RO_COMPAT_OFF..FEAT_RO_COMPAT_OFF + 4].copy_from_slice(&self.feat_ro_compat.to_le_bytes());
        sb[FEAT_INCOMPAT_OFF..FEAT_INCOMPAT_OFF + 4].copy_from_slice(&self.feat_incompat.to_le_bytes());
        // Re-stamp the integrity CRC over the updated superblock - now covering the masks (§3.12).
        let sb_crc = crc32(&sb[..SB_CRC_OFF]);
        sb[SB_CRC_OFF..SB_CRC_OFF + 4].copy_from_slice(&sb_crc.to_le_bytes());
        // Write BOTH copies - primary (LBA 0) and backup (last LBA) - staged in the same
        // transaction, so they commit atomically and the backup never lags the primary.
        if !self.tb_write(ctx, 0, &sb) { return Err("superblock write failed"); }
        if !self.tb_write(ctx, self.total_blocks - 1, &sb) { return Err("backup superblock write failed"); }
        Ok(())
    }

    // ── free bitmap (read on demand; the only global structure) ───────────────
    /// Allocate a contiguous run of `n` free blocks; mark them used; update the free count.
    fn alloc_run(&mut self, ctx: &ServiceContext, n: u64) -> Result<u64, &'static str> {
        if n == 0 { return Err("zero alloc"); }
        let mut run_start: Option<u64> = None;
        let mut run_len: u64 = 0;
        let mut b = self.data_start;
        while b < self.total_blocks {
            let bm_blk = self.bitmap_start + b / BITS_PER_BMBLOCK;
            let blk = self.tb_read(ctx, bm_blk).ok_or("bitmap read failed")?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let mut within = b - base;
            while within < BITS_PER_BMBLOCK {
                let idx = base + within;
                if idx >= self.total_blocks { break; }
                let used = (blk[(within / 8) as usize] >> (within % 8)) & 1 != 0;
                if used {
                    run_start = None;
                    run_len = 0;
                } else {
                    if run_start.is_none() { run_start = Some(idx); run_len = 0; }
                    run_len += 1;
                    if run_len == n {
                        let start = run_start.unwrap();
                        self.bm_set_range(ctx, start, n, true)?;
                        if self.free_known { self.free_blocks = self.free_blocks.saturating_sub(n); }
                        self.persist_super(ctx)?;
                        return Ok(start);
                    }
                }
                within += 1;
            }
            b = base + BITS_PER_BMBLOCK;
        }
        Err("no space")
    }

    fn free_run(&mut self, ctx: &ServiceContext, first: u64, count: u64) -> Result<(), &'static str> {
        if count == 0 { return Ok(()); }
        self.bm_set_range(ctx, first, count, false)?;
        if self.free_known { self.free_blocks += count; }
        self.persist_super(ctx)
    }

    /// Derive the free-block count by scanning the bitmap — Commandment III: the bitmap is the ONE
    /// truth for which blocks are free, so the count is *derived* from it, never trusted from a
    /// persisted copy that could drift. Counts clear (free) bits over `[data_start, total_blocks)`,
    /// exactly the region `alloc_run` allocates from (so it agrees with `format`'s initial count and
    /// every alloc/free since). Called at mount, after journal recovery, so a drift can never persist
    /// across a restart; the in-memory count is then maintained incrementally by alloc_run/free_run.
    fn count_free_blocks(&self, ctx: &ServiceContext) -> Result<u64, &'static str> {
        let mut free = 0u64;
        let mut b = self.data_start;
        while b < self.total_blocks {
            let bm_blk = self.bitmap_start + b / BITS_PER_BMBLOCK;
            let blk = self.tb_read(ctx, bm_blk).ok_or("bitmap read failed (free-count)")?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let mut within = b - base;
            while within < BITS_PER_BMBLOCK {
                let idx = base + within;
                if idx >= self.total_blocks { break; }
                let used = (blk[(within / 8) as usize] >> (within % 8)) & 1 != 0;
                if !used { free += 1; }
                within += 1;
            }
            b = base + BITS_PER_BMBLOCK;
        }
        Ok(free)
    }

    /// Ensure `free_blocks` is known, deriving it from the bitmap on first use (Commandment III — the
    /// count is derived, never trusted from a persisted copy). Returns false if the derive can't run
    /// (block-driver transiently down); callers then show a deferred count rather than failing. Cheap
    /// once known — the incremental alloc/free updates keep it current after this.
    fn ensure_free_count(&mut self, ctx: &ServiceContext) -> bool {
        if self.free_known { return true; }
        match self.count_free_blocks(ctx) {
            Ok(c) => { self.free_blocks = c; self.free_known = true; true }
            Err(_) => false,
        }
    }

    /// Set/clear the bits for blocks `[first, first+count)`, one bitmap block at a time.
    fn bm_set_range(&mut self, ctx: &ServiceContext, first: u64, count: u64, used: bool) -> Result<(), &'static str> {
        let end = first + count;
        let mut b = first;
        while b < end {
            let bm_blk = self.bitmap_start + b / BITS_PER_BMBLOCK;
            let mut blk = self.tb_read(ctx, bm_blk).ok_or("bitmap read failed")?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let stop = end.min(base + BITS_PER_BMBLOCK);
            while b < stop {
                let w = (b - base) as usize;
                if used { blk[w / 8] |= 1 << (w % 8); } else { blk[w / 8] &= !(1 << (w % 8)); }
                b += 1;
            }
            if !self.tb_write(ctx, bm_blk, &blk) { return Err("bitmap write failed"); }
        }
        Ok(())
    }

    // ── extent lists (GSFS0008) ───────────────────────────────────────────────
    // A file is normally one contiguous extent (`ITYPE_FILE`, the fast path: data is
    // `first_block..first_block+block_count`). When no contiguous run is free, the file is
    // stored FRAGMENTED (`ITYPE_FILE_FRAG`): `first_block` → a single CRC'd **extent block**
    // (`block_count` = 1) that lists the scattered data runs. Bounded (§26.6): one extent
    // block ⇒ ≤ EXT_MAX runs; a file that would need more is refused loudly.

    /// Read and decode a fragmented file's extent block (CRC-verified). Returns the runs
    /// `(start, len)` and their count. A CRC failure is a loud refusal (§3.12), never garbage.
    fn ext_of(&self, ctx: &ServiceContext, e: &Entry) -> Option<([(u64, u64); EXT_MAX], usize)> {
        let blk = self.tb_read(ctx, e.first_block)?;
        if u32_at(&blk, EXT_CRC_OFF) != crc32(&blk[..EXT_CRC_OFF]) {
            ctx.log_fmt(format_args!("fs: extent block CRC mismatch at lba {} - refusing", e.first_block));
            return None;
        }
        let n = (u32_at(&blk, EXT_N_OFF) as usize).min(EXT_MAX);
        let mut out = [(0u64, 0u64); EXT_MAX];
        for i in 0..n {
            let o = EXT_ENTRIES_OFF + i * EXT_ENTRY_SIZE;
            out[i] = (u64_at(&blk, o), u64_at(&blk, o + 8));
        }
        Some((out, n))
    }

    /// Build a fragmented file's extent block (`n` runs), stamp its CRC, and stage the write.
    fn ext_write(&mut self, ctx: &ServiceContext, lba: u64, exts: &[(u64, u64); EXT_MAX], n: usize) -> bool {
        let mut blk = [0u8; BLOCK];
        blk[EXT_N_OFF..EXT_N_OFF + 4].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            let o = EXT_ENTRIES_OFF + i * EXT_ENTRY_SIZE;
            blk[o..o + 8].copy_from_slice(&exts[i].0.to_le_bytes());
            blk[o + 8..o + 16].copy_from_slice(&exts[i].1.to_le_bytes());
        }
        let c = crc32(&blk[..EXT_CRC_OFF]);
        blk[EXT_CRC_OFF..EXT_CRC_OFF + 4].copy_from_slice(&c.to_le_bytes());
        self.tb_write(ctx, lba, &blk)
    }

    /// Find the first free run of blocks at or after `from`: returns `(start, len)` of the next
    /// maximal contiguous free span, or `None` if the disk has no free block left. Scans the
    /// bitmap one block at a time (each read honors staged writes).
    fn find_free_run(&self, ctx: &ServiceContext, from: u64) -> Option<(u64, u64)> {
        let mut b = from.max(self.data_start);
        let mut start: Option<u64> = None;
        let mut len = 0u64;
        while b < self.total_blocks {
            let bm_blk = self.bitmap_start + b / BITS_PER_BMBLOCK;
            let blk = self.tb_read(ctx, bm_blk)?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let mut within = b - base;
            while within < BITS_PER_BMBLOCK {
                let idx = base + within;
                if idx >= self.total_blocks { break; }
                let used = (blk[(within / 8) as usize] >> (within % 8)) & 1 != 0;
                if used {
                    if start.is_some() { return Some((start.unwrap(), len)); }
                } else {
                    if start.is_none() { start = Some(idx); len = 0; }
                    len += 1;
                }
                within += 1;
            }
            b = base + BITS_PER_BMBLOCK;
        }
        start.map(|s| (s, len))
    }

    /// Allocate `blocks` data blocks as up to `EXT_MAX` scattered runs (the fragmented path,
    /// used only when no single contiguous run is free). Marks the bits used and decrements the
    /// free count. Returns the number of runs filled into `out`. Loud failure if the disk lacks
    /// the space or would need more than `EXT_MAX` runs (frees what it grabbed first).
    fn alloc_extents(&mut self, ctx: &ServiceContext, blocks: u64, out: &mut [(u64, u64); EXT_MAX])
        -> Result<usize, &'static str> {
        let mut need = blocks;
        let mut n = 0usize;
        let mut from = self.data_start;
        while need > 0 {
            if n >= EXT_MAX {
                for i in 0..n { let (s, l) = out[i]; let _ = self.bm_set_range(ctx, s, l, false); }
                return Err("file too fragmented");
            }
            let (start, len) = match self.find_free_run(ctx, from) {
                Some(r) => r,
                None => {
                    for i in 0..n { let (s, l) = out[i]; let _ = self.bm_set_range(ctx, s, l, false); }
                    return Err("no space");
                }
            };
            let take = len.min(need);
            self.bm_set_range(ctx, start, take, true)?;
            out[n] = (start, take);
            n += 1;
            need -= take;
            from = start + len;
        }
        if self.free_known { self.free_blocks = self.free_blocks.saturating_sub(blocks); }
        self.persist_super(ctx)?;
        Ok(n)
    }

    /// Allocate space for a file of `blocks` data blocks, preferring one contiguous extent
    /// (`ITYPE_FILE`, the fast path). If no contiguous run is free, fall back to a fragmented
    /// file (`ITYPE_FILE_FRAG`): scattered data runs + a CRC'd extent block listing them.
    /// Returns the record fields `(itype, first_block, block_count)`.
    fn alloc_file(&mut self, ctx: &ServiceContext, blocks: u64) -> Result<(u8, u64, u64), &'static str> {
        match self.alloc_run(ctx, blocks) {
            Ok(first) => Ok((ITYPE_FILE, first, blocks)),
            Err("no space") => {
                let mut exts = [(0u64, 0u64); EXT_MAX];
                let ne = self.alloc_extents(ctx, blocks, &mut exts)?;
                // One more block for the extent block itself.
                let ext_lba = match self.alloc_run(ctx, 1) {
                    Ok(l) => l,
                    Err(e) => {
                        for i in 0..ne { let (s, l) = exts[i]; let _ = self.free_run(ctx, s, l); }
                        return Err(e);
                    }
                };
                if !self.ext_write(ctx, ext_lba, &exts, ne) {
                    let _ = self.free_run(ctx, ext_lba, 1);
                    for i in 0..ne { let (s, l) = exts[i]; let _ = self.free_run(ctx, s, l); }
                    return Err("extent block write failed");
                }
                // First fragmentation on this disk: record the EXTENTS incompat feature so a build
                // that doesn't understand extent lists refuses rather than misreads (§6.15). Staged
                // in the same transaction as the rest of the write; a no-op once already set.
                if self.feat_incompat & FEAT_INCOMPAT_EXTENTS == 0 {
                    self.feat_incompat |= FEAT_INCOMPAT_EXTENTS;
                    self.persist_super(ctx)?;
                }
                Ok((ITYPE_FILE_FRAG, ext_lba, 1))
            }
            Err(e) => Err(e),
        }
    }

    /// Free a file's data blocks: a contiguous extent frees in one run; a fragmented file frees
    /// each listed run, then the extent block itself. (Directories are freed via `free_run`.)
    fn free_file(&mut self, ctx: &ServiceContext, e: &Entry) -> Result<(), &'static str> {
        match e.itype {
            ITYPE_FILE_FRAG => {
                let (exts, ne) = self.ext_of(ctx, e).ok_or("extent block read failed")?;
                for i in 0..ne { let (s, l) = exts[i]; self.free_run(ctx, s, l)?; }
                self.free_run(ctx, e.first_block, e.block_count) // the extent block (count = 1)
            }
            _ => self.free_run(ctx, e.first_block, e.block_count),
        }
    }

    // ── directory tree (self-describing entries) ──────────────────────────────
    fn root_entry(&self) -> Entry {
        Entry { itype: ITYPE_DIR, size: 0, first_block: self.root_first_block, block_count: self.root_block_count, loc: None }
    }

    /// Find `name` among a directory's entries; returns the child (with its on-disk loc).
    fn dir_find(&self, ctx: &ServiceContext, dir: &Entry, name: &[u8]) -> Option<Entry> {
        for bi in 0..dir.block_count {
            let block = dir.first_block + bi;
            let blk = self.td_read(ctx, block)?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                if blk[o] == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if &blk[o + 2..o + 2 + nl] == name {
                    return Some(Entry {
                        itype: blk[o],
                        size: u64_at(&blk, o + 40),
                        first_block: u64_at(&blk, o + 48),
                        block_count: u64_at(&blk, o + 56),
                        loc: Some(Loc { block, slot }),
                    });
                }
            }
        }
        None
    }

    fn walk(&self, ctx: &ServiceContext, path: &[u8]) -> Option<Entry> {
        let mut cur = self.root_entry();
        for comp in components(path) {
            if cur.itype != ITYPE_DIR { return None; }
            cur = self.dir_find(ctx, &cur, comp)?;
        }
        Some(cur)
    }

    /// (parent dir entry, last component). Walks all but the last component.
    fn walk_parent<'a>(&self, ctx: &ServiceContext, path: &'a [u8]) -> Option<(Entry, &'a [u8])> {
        let mut cur = self.root_entry();
        let mut last: Option<&[u8]> = None;
        for comp in components(path) {
            if let Some(name) = last {
                if cur.itype != ITYPE_DIR { return None; }
                cur = self.dir_find(ctx, &cur, name)?;
            }
            last = Some(comp);
        }
        last.map(|name| (cur, name))
    }

    /// Add a record to `dir`, growing the directory if it has no free slot.
    fn dir_add(&mut self, ctx: &ServiceContext, dir: &mut Entry, name: &[u8],
               itype: u8, size: u64, first: u64, count: u64) -> Result<(), &'static str> {
        for bi in 0..dir.block_count {
            let block = dir.first_block + bi;
            let mut blk = self.td_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                if blk[slot * REC_SIZE] == ITYPE_FREE {
                    encode_rec(&mut blk, slot, itype, name, size, first, count);
                    if !self.td_write(ctx, block, &mut blk) { return Err("dir write failed"); }
                    return Ok(());
                }
            }
        }
        // No free slot - grow, then place in the fresh block.
        self.grow_dir(ctx, dir)?;
        let block = dir.first_block + dir.block_count - 1;
        let mut blk = self.td_read(ctx, block).ok_or("dir read failed")?;
        encode_rec(&mut blk, 0, itype, name, size, first, count);
        if !self.td_write(ctx, block, &mut blk) { return Err("dir write failed"); }
        Ok(())
    }

    /// Grow a directory by one block: reallocate a bigger contiguous extent, copy, free the
    /// old, and persist the directory's own record (extent changed).
    fn grow_dir(&mut self, ctx: &ServiceContext, dir: &mut Entry) -> Result<(), &'static str> {
        let new_count = dir.block_count + 1;
        let new_first = self.alloc_run(ctx, new_count)?;
        for bi in 0..dir.block_count {
            let mut blk = self.td_read(ctx, dir.first_block + bi).ok_or("dir read failed")?;
            if !self.td_write(ctx, new_first + bi, &mut blk) { return Err("dir copy failed"); }
        }
        let mut fresh = [0u8; BLOCK];
        if !self.td_write(ctx, new_first + dir.block_count, &mut fresh) { return Err("dir grow init failed"); }
        let (old_first, old_count) = (dir.first_block, dir.block_count);
        dir.first_block = new_first;
        dir.block_count = new_count;
        self.persist_entry(ctx, dir)?;
        self.free_run(ctx, old_first, old_count)
    }

    /// Write back an entry's mutable fields (size/first_block/block_count) at its loc;
    /// for the root, update the superblock instead.
    fn persist_entry(&mut self, ctx: &ServiceContext, e: &Entry) -> Result<(), &'static str> {
        match e.loc {
            None => {
                self.root_first_block = e.first_block;
                self.root_block_count = e.block_count;
                self.persist_super(ctx)
            }
            Some(loc) => {
                let mut blk = self.td_read(ctx, loc.block).ok_or("record read failed")?;
                let o = loc.slot * REC_SIZE;
                blk[o + 40..o + 48].copy_from_slice(&e.size.to_le_bytes());
                blk[o + 48..o + 56].copy_from_slice(&e.first_block.to_le_bytes());
                blk[o + 56..o + 64].copy_from_slice(&e.block_count.to_le_bytes());
                if !self.td_write(ctx, loc.block, &mut blk) { return Err("record write failed"); }
                Ok(())
            }
        }
    }

    fn dir_remove(&mut self, ctx: &ServiceContext, dir: &Entry, name: &[u8]) -> Result<(), &'static str> {
        for bi in 0..dir.block_count {
            let block = dir.first_block + bi;
            let mut blk = self.td_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                if blk[o] == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if &blk[o + 2..o + 2 + nl] == name {
                    blk[o] = ITYPE_FREE;
                    if !self.td_write(ctx, block, &mut blk) { return Err("dir write failed"); }
                    return Ok(());
                }
            }
        }
        Err("entry not found")
    }

    fn dir_is_empty(&self, ctx: &ServiceContext, dir: &Entry) -> Option<bool> {
        for bi in 0..dir.block_count {
            let blk = self.td_read(ctx, dir.first_block + bi)?;
            for slot in 0..RECS_PER_BLOCK {
                if blk[slot * REC_SIZE] != ITYPE_FREE { return Some(false); }
            }
        }
        Some(true)
    }

    // ── operations ────────────────────────────────────────────────────────────
    fn mkdir(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let (mut parent, name) = self.walk_parent(ctx, path).ok_or("path not found")?;
        if parent.itype != ITYPE_DIR { return Err("parent is not a directory"); }
        if !valid_name(name) { return Err("bad name"); }
        if self.dir_find(ctx, &parent, name).is_some() { return Err("already exists"); }
        let first = self.alloc_run(ctx, 1)?;
        let mut blk = [0u8; BLOCK];
        if !self.td_write(ctx, first, &mut blk) { return Err("dir block init failed"); }
        self.dir_add(ctx, &mut parent, name, ITYPE_DIR, 0, first, 1)
    }

    /// `mkdir … parents`: create every missing directory along `path`. Walks component by
    /// component from root; descends into existing dirs, creates missing ones. Idempotent
    /// (a fully-existing path is OK); errors only if a component is in the way as a file.
    fn mkdir_parents(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let mut cur = self.root_entry();
        for comp in components(path) {
            if cur.itype != ITYPE_DIR { return Err("a path component is not a directory"); }
            match self.dir_find(ctx, &cur, comp) {
                Some(child) => cur = child,
                None => {
                    if !valid_name(comp) { return Err("bad name"); }
                    let first = self.alloc_run(ctx, 1)?;
                    let mut blk = [0u8; BLOCK];
                    if !self.td_write(ctx, first, &mut blk) { return Err("dir block init failed"); }
                    self.dir_add(ctx, &mut cur, comp, ITYPE_DIR, 0, first, 1)?;
                    cur = self.dir_find(ctx, &cur, comp).ok_or("created dir not found")?;
                }
            }
        }
        Ok(())
    }

    fn write_path(&mut self, ctx: &ServiceContext, path: &[u8], data: &[u8]) -> Result<(), &'static str> {
        if data.len() > MAX_FILE_BYTES { return Err("file too large"); }
        let (mut parent, name) = self.walk_parent(ctx, path).ok_or("path not found")?;
        if parent.itype != ITYPE_DIR { return Err("parent is not a directory"); }
        if !valid_name(name) { return Err("bad name"); }
        let existing = self.dir_find(ctx, &parent, name);
        if let Some(ref e) = existing {
            if !is_file(e.itype) { return Err("path is a directory"); }
        }
        let blocks = ((data.len() + DATA_PAYLOAD - 1) / DATA_PAYLOAD).max(1) as u64;
        // Alloc the new file first (old still allocated), so a failure leaves the file intact;
        // free the old extent only after the record points at the new one. `alloc_file` returns
        // a contiguous extent (fast path) or a fragmented file when no contiguous run is free.
        let (itype, first, count) = self.alloc_file(ctx, blocks)?;
        // Write the data to its allocated blocks: contiguous → arithmetic; fragmented → walk the
        // extent runs the extent block just recorded (read-your-writes via the staged txn).
        if itype == ITYPE_FILE {
            for i in 0..blocks as usize {
                let s = i * DATA_PAYLOAD;
                let e = (s + DATA_PAYLOAD).min(data.len());
                let payload = if s < data.len() { &data[s..e] } else { &[][..] };
                if !data_write(ctx, first + i as u64, payload) { return Err("block write failed"); }
            }
        } else {
            let frag_e = Entry { itype, size: 0, first_block: first, block_count: count, loc: None };
            let (exts, ne) = self.ext_of(ctx, &frag_e).ok_or("extent block read failed")?;
            let mut produced = 0usize;
            'fill: for ei in 0..ne {
                let (s, l) = exts[ei];
                for j in 0..l {
                    let so = produced * DATA_PAYLOAD;
                    if so >= data.len() && produced > 0 { break 'fill; }
                    let eo = (so + DATA_PAYLOAD).min(data.len());
                    let payload = if so < data.len() { &data[so..eo] } else { &[][..] };
                    if !data_write(ctx, s + j, payload) { return Err("block write failed"); }
                    produced += 1;
                }
            }
        }
        match existing {
            Some(e) => {
                let ne = Entry { itype, size: data.len() as u64, first_block: first, block_count: count, loc: e.loc };
                self.persist_entry(ctx, &ne)?;
                self.free_file(ctx, &e)?;
            }
            None => self.dir_add(ctx, &mut parent, name, itype, data.len() as u64, first, count)?,
        }
        Ok(())
    }

    fn read_path(&self, ctx: &ServiceContext, path: &[u8], out: &mut [u8]) -> Option<usize> {
        let e = self.walk(ctx, path)?;
        if !is_file(e.itype) { return None; }
        let size = e.size as usize;
        if size > out.len() { return None; }
        // Resolve the extent list once if fragmented; a contiguous file maps by arithmetic.
        let frag = if e.itype == ITYPE_FILE_FRAG { Some(self.ext_of(ctx, &e)?) } else { None };
        let nblocks = (size + DATA_PAYLOAD - 1) / DATA_PAYLOAD;
        for b in 0..nblocks {
            let start = b * DATA_PAYLOAD;
            let lba = match &frag {
                None => e.first_block + b as u64,
                Some((exts, ne)) => nth_data_block(exts, *ne, b as u64)?,
            };
            let blk = data_read(ctx, lba)?;
            let end = (start + DATA_PAYLOAD).min(size);
            out[start..end].copy_from_slice(&blk[..end - start]);
        }
        Some(size)
    }

    // ── large-file streaming (offset-addressed; the data path that lifts the one-message
    //    file-size cap). A big file is created once with `write_new` (which allocates the
    //    whole extent up front), filled by sequential `write_at` chunks, and read back with
    //    `read_at` chunks. No open-file/session state - each call is self-contained (§8).

    /// Create or truncate `path` to a file sized for `total` bytes: allocate a contiguous
    /// extent big enough, record it (size = `total`), and leave the data for `write_at` to
    /// fill. Like `write_path`, the new extent is allocated before the old is freed, so a
    /// failure leaves the previous file intact.
    fn write_new(&mut self, ctx: &ServiceContext, path: &[u8], total: u64) -> Result<(), &'static str> {
        let (mut parent, name) = self.walk_parent(ctx, path).ok_or("path not found")?;
        if parent.itype != ITYPE_DIR { return Err("parent is not a directory"); }
        if !valid_name(name) { return Err("bad name"); }
        let existing = self.dir_find(ctx, &parent, name);
        if let Some(ref e) = existing {
            if !is_file(e.itype) { return Err("path is a directory"); }
        }
        let blocks = ((total + DATA_PAYLOAD as u64 - 1) / DATA_PAYLOAD as u64).max(1);
        let (itype, first, count) = self.alloc_file(ctx, blocks)?;
        match existing {
            Some(e) => {
                let ne = Entry { itype, size: total, first_block: first, block_count: count, loc: e.loc };
                self.persist_entry(ctx, &ne)?;
                self.free_file(ctx, &e)
            }
            None => self.dir_add(ctx, &mut parent, name, itype, total, first, count),
        }
    }

    /// Write `chunk` into an existing file at byte `offset`. The offset must be **payload-block
    /// aligned** (a multiple of DATA_PAYLOAD - clients stream in such chunks), so whole data
    /// blocks are written with no read-modify-write; the final block of a partial chunk is
    /// zero-padded. Bounded to the file's allocated extent - a write past it is a loud error.
    ///
    /// `journal` selects the durability contract (Phase J, §6.13): `false` writes the data
    /// blocks **direct** (the fast streaming path - a torn chunk is caught by the data CRC on
    /// read but not recovered); `true` **stages** them in the active transaction (`data_stage`)
    /// so the whole chunk commits atomically - a crash replays or discards it, never tears it.
    /// The journaled caller (`OP_WRITE_AT_J`) wraps this in `begin_txn`/`end_txn`.
    fn write_at(&mut self, ctx: &ServiceContext, path: &[u8], offset: u64, chunk: &[u8], journal: bool) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if !is_file(e.itype) { return Err("not a file"); }
        if offset % DATA_PAYLOAD as u64 != 0 { return Err("unaligned offset"); }
        // The file's data-block count: a contiguous file's `block_count`, else the extents'
        // total (a fragmented file's `block_count` counts only the extent block).
        let total_blocks = match e.itype {
            ITYPE_FILE => e.block_count,
            _ => (e.size + DATA_PAYLOAD as u64 - 1) / DATA_PAYLOAD as u64,
        };
        if offset + chunk.len() as u64 > total_blocks * DATA_PAYLOAD as u64 { return Err("write past extent"); }
        let frag = if e.itype == ITYPE_FILE_FRAG { Some(self.ext_of(ctx, &e).ok_or("extent block read failed")?) } else { None };
        let base_idx = offset / DATA_PAYLOAD as u64;
        let nblk = (chunk.len() + DATA_PAYLOAD - 1) / DATA_PAYLOAD;
        for i in 0..nblk {
            let s = i * DATA_PAYLOAD;
            let end = (s + DATA_PAYLOAD).min(chunk.len());
            let idx = base_idx + i as u64;
            let lba = match &frag {
                None => e.first_block + idx,
                Some((exts, ne)) => nth_data_block(exts, *ne, idx).ok_or("extent out of range")?,
            };
            let ok = if journal { self.data_stage(ctx, lba, &chunk[s..end]) } else { data_write(ctx, lba, &chunk[s..end]) };
            if !ok { return Err("block write failed"); }
        }
        Ok(())
    }

    /// Stamp a file-data block's CRC32 and **stage** it in the active transaction (Phase J).
    /// The data thus rides the journal with the metadata and is checkpointed atomically; mirror
    /// of `data_write` but transaction-aware (read-your-writes via `tb_write`).
    fn data_stage(&mut self, ctx: &ServiceContext, lba: u64, payload: &[u8]) -> bool {
        let mut blk = [0u8; BLOCK];
        let n = payload.len().min(DATA_PAYLOAD);
        blk[..n].copy_from_slice(&payload[..n]);
        let c = crc32(&blk[..DATA_PAYLOAD]);
        blk[DATA_CRC_OFF..DATA_CRC_OFF + 4].copy_from_slice(&c.to_le_bytes());
        self.tb_write(ctx, lba, &blk)
    }

    /// Read up to `len` bytes from `path` starting at byte `offset` into `out` (clamped to the
    /// file's size and `out.len()`). Returns the number of bytes read (0 at/after EOF). The
    /// offset need not be block-aligned - it reads across block boundaries as needed.
    fn read_at(&self, ctx: &ServiceContext, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Option<usize> {
        let e = self.walk(ctx, path)?;
        if !is_file(e.itype) { return None; }
        let size = e.size;
        if offset >= size { return Some(0); }
        let n = len.min((size - offset) as usize).min(out.len());
        let frag = if e.itype == ITYPE_FILE_FRAG { Some(self.ext_of(ctx, &e)?) } else { None };
        let mut done = 0;
        while done < n {
            let pos = offset as usize + done;
            let idx = (pos / DATA_PAYLOAD) as u64;
            let lba = match &frag {
                None => e.first_block + idx,
                Some((exts, ne)) => nth_data_block(exts, *ne, idx)?,
            };
            let blk = data_read(ctx, lba)?;
            let within = pos % DATA_PAYLOAD;
            let take = (DATA_PAYLOAD - within).min(n - done);
            out[done..done + take].copy_from_slice(&blk[within..within + take]);
            done += take;
        }
        Some(n)
    }

    /// Reply: `[FS_OK, count:u8, {name_len:u8, name, is_dir:u8, size:u64}…]`, one block.
    fn list_dir(&self, ctx: &ServiceContext, path: &[u8]) -> Option<[u8; BLOCK]> {
        let d = self.walk(ctx, path)?;
        if d.itype != ITYPE_DIR { return None; }
        let mut out = [0u8; BLOCK];
        out[0] = FS_OK;
        let mut count = 0u8;
        let mut w = 2usize;
        for bi in 0..d.block_count {
            let blk = self.td_read(ctx, d.first_block + bi)?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                let t = blk[o];
                if t == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if w + 1 + nl + 1 + 8 > BLOCK { break; }
                out[w] = nl as u8;
                out[w + 1..w + 1 + nl].copy_from_slice(&blk[o + 2..o + 2 + nl]);
                out[w + 1 + nl] = (t == ITYPE_DIR) as u8;
                out[w + 2 + nl..w + 2 + nl + 8].copy_from_slice(&blk[o + 40..o + 48]); // size:u64
                w += 1 + nl + 1 + 8;
                count += 1;
            }
        }
        out[1] = count;
        Some(out)
    }

    fn rename(&mut self, ctx: &ServiceContext, path: &[u8], newname: &[u8]) -> Result<(), &'static str> {
        if !valid_name(newname) { return Err("bad new name"); }
        let (parent, oldname) = self.walk_parent(ctx, path).ok_or("path not found")?;
        if parent.itype != ITYPE_DIR { return Err("parent not a directory"); }
        if self.dir_find(ctx, &parent, newname).is_some() { return Err("name already exists"); }
        for bi in 0..parent.block_count {
            let block = parent.first_block + bi;
            let mut blk = self.td_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                if blk[o] == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if &blk[o + 2..o + 2 + nl] == oldname {
                    blk[o + 1] = newname.len() as u8;
                    for b in &mut blk[o + 2..o + 2 + NAME_MAX] { *b = 0; }
                    blk[o + 2..o + 2 + newname.len()].copy_from_slice(newname);
                    if !self.td_write(ctx, block, &mut blk) { return Err("dir write failed"); }
                    // The old path no longer names this file - revoke any open caps to it, so a
                    // held cap can never silently rebind to a different file later created at the
                    // old path (confused-deputy avoidance; §7.10). Same discipline as delete.
                    self.revoke_open_by_path(ctx, path);
                    return Ok(());
                }
            }
        }
        Err("entry not found")
    }

    // ── file-as-capability: open-file table (§7.10) ───────────────────────────
    /// Open an existing file `path`: mint a delegated resource owned by fs, record
    /// `resource_id → path`, and reply `[FS_OK]` with the **file cap embedded** for the client.
    /// The client operates the file by invoking that cap (the kernel badges the request with the
    /// resource id + right; `serve_filecap` resolves it back here). Minted with `GRANT` so fs can
    /// transfer a copy; fs drops its own copy afterward (it serves via the badge, not the cap).
    fn open_file(&mut self, ctx: &ServiceContext, path: &[u8], want: u8, reply: CapHandle)
        -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if !is_file(e.itype) { return Err("not a file"); }
        if path.len() > OPEN_PATH_MAX { return Err("path too long"); }
        let slot = self.open_files.iter().position(|o| o.rid == 0).ok_or("too many open files")?;
        let (rid, cap) = ctx.resource_mint(want | RIGHT_GRANT).ok_or("mint failed")?;
        let mut of = OpenFile { rid, plen: path.len() as u8, path: [0u8; OPEN_PATH_MAX] };
        of.path[..path.len()].copy_from_slice(path);
        self.open_files[slot] = of;
        // Hand a derived copy to the client; drop fs's original either way.
        let granted = match ctx.derive_cap(cap) {
            Some(c) => ctx.send_with_cap_by_handle(reply, c, &Message::from_bytes(&[FS_OK])).is_ok(),
            None    => false,
        };
        ctx.remove_cap(cap);
        if !granted {
            self.open_files[slot].rid = 0;
            let _ = ctx.resource_revoke(rid); // nothing was handed out - undo the mint
            return Err("grant failed");
        }
        Ok(())
    }

    /// Resolve a delegated resource id → its file path (copied out so `self` can be reborrowed).
    fn open_path(&self, rid: u64) -> Option<([u8; OPEN_PATH_MAX], usize)> {
        self.open_files.iter()
            .find(|o| o.rid != 0 && o.rid == rid)
            .map(|o| (o.path, o.plen as usize))
    }

    /// Free the open-file slot for `rid` (after a close/revoke). Idempotent.
    fn open_free(&mut self, rid: u64) {
        if rid == 0 { return; }
        for o in self.open_files.iter_mut() {
            if o.rid == rid { o.rid = 0; }
        }
    }

    /// Revoke every open file cap naming `path` - called on delete, so deleting a file a client
    /// holds open makes its cap fail with `CapRevoked` on next use (the revocable property, §7.5).
    fn revoke_open_by_path(&mut self, ctx: &ServiceContext, path: &[u8]) {
        for i in 0..MAX_OPEN {
            let o = self.open_files[i];
            if o.rid != 0 && &o.path[..o.plen as usize] == path {
                let _ = ctx.resource_revoke(o.rid);
                self.open_files[i].rid = 0;
            }
        }
    }

    fn delete(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.loc.is_none() { return Err("cannot delete root"); }
        if e.itype == ITYPE_DIR && !self.dir_is_empty(ctx, &e).ok_or("dir read failed")? {
            return Err("directory not empty");
        }
        let (parent, name) = self.walk_parent(ctx, path).ok_or("not found")?;
        self.dir_remove(ctx, &parent, name)?;
        self.revoke_open_by_path(ctx, path); // invalidate any open file caps to the deleted file
        self.free_file(ctx, &e)
    }

    /// `delete … recursive`: remove a file or a WHOLE subtree. Unlinks the entry from its
    /// parent, then frees the entry and every descendant via `free_subtree`. A file is just
    /// the depth-0 case (no children), so this is a strict superset of `delete`.
    /// Manages its own transactions (so it is NOT wrapped by the serve-level transaction):
    /// the unlink is one atomic transaction, then the now-unreachable subtree is reclaimed in
    /// bounded per-extent transactions - too many blocks to stage as a single atomic set.
    fn delete_tree(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.loc.is_none() { return Err("cannot delete root"); }
        let (parent, name) = self.walk_parent(ctx, path).ok_or("not found")?;
        // Atomic unlink: once this transaction commits, the subtree is unreachable. A crash
        // before it leaves the tree untouched; after it, the entry is gone.
        self.begin_txn();
        let r = self.dir_remove(ctx, &parent, name);
        self.end_txn(ctx, r)?;
        self.revoke_open_by_path(ctx, path); // invalidate any open file caps to the deleted entry
        // Reclaim the unreachable subtree in bounded transactions. A crash here only leaks
        // blocks (nothing references them) - never corruption.
        self.free_subtree(ctx, e.itype, e.first_block, e.block_count, 0)
    }

    /// Free an entry's blocks and, if it is a directory, all of its descendants first
    /// (post-order). Bounded two ways (§26.6): a hard **depth** cap, and small stack frames -
    /// each level extracts its block's child extents into fixed locals (≤8 per block) and
    /// drops the 512-byte block buffer *before* recursing, so a frame never carries it down.
    fn free_subtree(&mut self, ctx: &ServiceContext, itype: u8, first: u64, count: u64, depth: u32)
        -> Result<(), &'static str> {
        if depth > MAX_TREE_DEPTH { return Err("tree too deep"); }
        if itype == ITYPE_DIR {
            for bi in 0..count {
                // Scope the block read so `blk` is gone before we recurse into the children.
                let (kids, nk) = {
                    let blk = self.td_read(ctx, first + bi).ok_or("dir read failed")?;
                    let mut kids = [(0u8, 0u64, 0u64); RECS_PER_BLOCK];
                    let mut nk = 0usize;
                    for slot in 0..RECS_PER_BLOCK {
                        let o = slot * REC_SIZE;
                        if blk[o] == ITYPE_FREE { continue; }
                        kids[nk] = (blk[o], u64_at(&blk, o + 48), u64_at(&blk, o + 56));
                        nk += 1;
                    }
                    (kids, nk)
                };
                for k in 0..nk {
                    let (kt, kf, kc) = kids[k];
                    self.free_subtree(ctx, kt, kf, kc, depth + 1)?;
                }
            }
        } else if itype == ITYPE_FILE_FRAG {
            // Free the scattered data runs (each its own bounded txn) before the extent block.
            let frag_e = Entry { itype, size: 0, first_block: first, block_count: count, loc: None };
            if let Some((exts, ne)) = self.ext_of(ctx, &frag_e) {
                for i in 0..ne { let (s, l) = exts[i]; self.free_run_txn(ctx, s, l)?; }
            }
        }
        // Each extent freed in its own bounded transaction (the subtree is already unlinked).
        self.free_run_txn(ctx, first, count)
    }

    // ── fsck / `drives check` (Phase G) ───────────────────────────────────────
    // Recover after detection. The directory tree is the source of truth: rebuild the free
    // bitmap and free count from it (fixing any drift), and verify every block's CRC, reporting
    // (not deleting) files/dirs that fail. Writes are DIRECT (not journaled) - the operation is
    // far larger than one transaction and is idempotent (re-running converges), so a crash
    // mid-check is harmless: the tree is still truth and a re-run finishes the job.

    /// Walk the filesystem from root, rebuild the bitmap + free count, verify CRCs. Returns
    /// `(files, dirs, bad, used)`.
    fn check(&mut self, ctx: &ServiceContext) -> Result<(u32, u32, u32, u64), &'static str> {
        // Start from an all-free bitmap (fast batched zero), then mark what is actually used.
        // The bitmap region is [bitmap_start, journal_start).
        let bitmap_blocks = self.journal_start - self.bitmap_start;
        if !block_write_zeros(ctx, self.bitmap_start, bitmap_blocks) { return Err("bitmap zero failed"); }
        // System blocks [0, data_start): superblock + bitmap + journal. Plus the backup block.
        self.bm_set_range(ctx, 0, self.data_start, true)?;
        self.bm_set_range(ctx, self.total_blocks - 1, 1, true)?;
        let mut st = (0u32, 0u32, 0u32, self.data_start + 1); // (files, dirs, bad, used)
        let root = self.root_entry();
        self.check_subtree(ctx, root.itype, root.first_block, root.block_count, 0, &mut st)?;
        // Recompute the free count from what the tree actually uses, and persist BOTH superblock
        // copies (heals a drifted free count + refreshes the backup).
        self.free_blocks = self.total_blocks - st.3;
        self.free_known = true; // fsck recomputed it from the tree
        self.persist_super(ctx)?;
        Ok((st.0, st.1, st.2, st.3))
    }

    /// Mark a node's extent used, recurse into directories, verify file/dir CRCs. `st` is
    /// `(files, dirs, bad, used)`. Bounded like `free_subtree` (depth cap + small frames).
    fn check_subtree(&mut self, ctx: &ServiceContext, itype: u8, first: u64, count: u64,
                     depth: u32, st: &mut (u32, u32, u32, u64)) -> Result<(), &'static str> {
        if depth > MAX_TREE_DEPTH { return Err("tree too deep"); }
        self.bm_set_range(ctx, first, count, true)?; // the extent is referenced → mark it used
        st.3 += count;
        if itype == ITYPE_DIR {
            st.1 += 1;
            for bi in 0..count {
                let (kids, nk) = {
                    match self.td_read(ctx, first + bi) {
                        Some(blk) => {
                            let mut kids = [(0u8, 0u64, 0u64); RECS_PER_BLOCK];
                            let mut nk = 0usize;
                            for slot in 0..RECS_PER_BLOCK {
                                let o = slot * REC_SIZE;
                                if blk[o] == ITYPE_FREE { continue; }
                                kids[nk] = (blk[o], u64_at(&blk, o + 48), u64_at(&blk, o + 56));
                                nk += 1;
                            }
                            (kids, nk)
                        }
                        None => { st.2 += 1; continue; } // corrupt dir block - already logged loudly
                    }
                };
                for k in 0..nk {
                    let (kt, kf, kc) = kids[k];
                    self.check_subtree(ctx, kt, kf, kc, depth + 1, st)?;
                }
            }
        } else {
            st.0 += 1;
            // Verify each data block's CRC (data_read logs a mismatch loudly). Blocks stay
            // marked used regardless - the entry references them; freeing would risk reuse.
            if itype == ITYPE_FILE_FRAG {
                // `first` (already marked above) is the extent block; its runs hold the data.
                let frag_e = Entry { itype, size: 0, first_block: first, block_count: count, loc: None };
                match self.ext_of(ctx, &frag_e) {
                    Some((exts, ne)) => {
                        let mut ok = true;
                        for i in 0..ne {
                            let (s, l) = exts[i];
                            self.bm_set_range(ctx, s, l, true)?; // data runs are referenced → used
                            st.3 += l;
                            for j in 0..l { if data_read(ctx, s + j).is_none() { ok = false; } }
                        }
                        if !ok { st.2 += 1; }
                    }
                    None => { st.2 += 1; } // corrupt extent block - logged loudly
                }
            } else {
                let mut ok = true;
                for bi in 0..count {
                    if data_read(ctx, first + bi).is_none() { ok = false; }
                }
                if !ok { st.2 += 1; }
            }
        }
        Ok(())
    }

    // ── scrub / `drives scrub` (Phase K) ──────────────────────────────────────
    // READ-ONLY integrity sweep: walk the tree and verify every referenced block's CRC,
    // reporting (files, dirs, bad, scanned). Writes NOTHING (unlike `check`, which repairs the
    // bitmap) - so it is safe to run on a healthy filesystem at whatever cadence the operator
    // sets. Without redundancy (no RAID, by design), scrub DETECTS latent bit-rot but cannot
    // repair it: a bad block is reported (and any read of it stays a loud refusal, §3.12), the
    // data is already lost. The cadence is operator-driven - GodspeedOS has no background-task
    // primitive, so "periodic" is policy (run `drives scrub` on a schedule), not a hidden timer
    // (§26.4: no silent complexity). Read-only ⇒ `&self`, no transaction.

    /// Walk the filesystem from root verifying every block's CRC; change nothing. Returns
    /// `(files, dirs, bad, scanned)`.
    fn scrub(&self, ctx: &ServiceContext) -> Result<(u32, u32, u32, u64), &'static str> {
        let mut st = (0u32, 0u32, 0u32, 0u64); // (files, dirs, bad, scanned)
        let root = self.root_entry();
        self.scrub_subtree(ctx, root.itype, root.first_block, root.block_count, 0, &mut st)?;
        Ok((st.0, st.1, st.2, st.3))
    }

    /// Verify a node's blocks read-only, recurse into directories. `st` is `(files, dirs, bad,
    /// scanned)`. Mirrors `check_subtree` minus every write. Bounded (depth cap + small frames).
    fn scrub_subtree(&self, ctx: &ServiceContext, itype: u8, first: u64, count: u64,
                     depth: u32, st: &mut (u32, u32, u32, u64)) -> Result<(), &'static str> {
        if depth > MAX_TREE_DEPTH { return Err("tree too deep"); }
        if itype == ITYPE_DIR {
            st.1 += 1;
            for bi in 0..count {
                st.3 += 1; // the directory block itself (read + CRC-verified by td_read)
                let (kids, nk) = {
                    match self.td_read(ctx, first + bi) {
                        Some(blk) => {
                            let mut kids = [(0u8, 0u64, 0u64); RECS_PER_BLOCK];
                            let mut nk = 0usize;
                            for slot in 0..RECS_PER_BLOCK {
                                let o = slot * REC_SIZE;
                                if blk[o] == ITYPE_FREE { continue; }
                                kids[nk] = (blk[o], u64_at(&blk, o + 48), u64_at(&blk, o + 56));
                                nk += 1;
                            }
                            (kids, nk)
                        }
                        None => { st.2 += 1; continue; } // corrupt dir block (CRC mismatch, logged loud)
                    }
                };
                for k in 0..nk {
                    let (kt, kf, kc) = kids[k];
                    self.scrub_subtree(ctx, kt, kf, kc, depth + 1, st)?;
                }
            }
        } else {
            st.0 += 1;
            if itype == ITYPE_FILE_FRAG {
                st.3 += count; // the extent block (count == 1), read + CRC-verified by ext_of
                let frag_e = Entry { itype, size: 0, first_block: first, block_count: count, loc: None };
                match self.ext_of(ctx, &frag_e) {
                    Some((exts, ne)) => {
                        let mut ok = true;
                        for i in 0..ne {
                            let (s, l) = exts[i];
                            st.3 += l;
                            for j in 0..l { if data_read(ctx, s + j).is_none() { ok = false; } }
                        }
                        if !ok { st.2 += 1; }
                    }
                    None => { st.2 += 1; } // corrupt extent block - logged loudly
                }
            } else {
                st.3 += count;
                let mut ok = true;
                for bi in 0..count {
                    if data_read(ctx, first + bi).is_none() { ok = false; }
                }
                if !ok { st.2 += 1; }
            }
        }
        Ok(())
    }

    /// Move (relink) an entry: same data, new directory/name. Same-directory move is a
    /// rename. No data copied - only the directory entries change.
    fn move_path(&mut self, ctx: &ServiceContext, src: &[u8], dst: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, src).ok_or("source not found")?;
        if e.loc.is_none() { return Err("cannot move root"); }
        let (mut dparent, dname) = self.walk_parent(ctx, dst).ok_or("dest path not found")?;
        if dparent.itype != ITYPE_DIR { return Err("dest not a directory"); }
        if !valid_name(dname) { return Err("bad dest name"); }
        if self.dir_find(ctx, &dparent, dname).is_some() { return Err("dest exists"); }
        let (sparent, sname) = self.walk_parent(ctx, src).ok_or("source not found")?;
        // Same directory → it's a rename (avoids the grow-stale-parent hazard).
        if sparent.first_block == dparent.first_block {
            return self.rename(ctx, src, dname);
        }
        self.dir_add(ctx, &mut dparent, dname, e.itype, e.size, e.first_block, e.block_count)?;
        self.dir_remove(ctx, &sparent, sname)?;
        // `src` no longer names this file - revoke any open caps to it (confused-deputy
        // avoidance, §7.10). The same-directory case above goes through `rename`, which does this.
        self.revoke_open_by_path(ctx, src);
        Ok(())
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────
fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && !name.iter().any(|&b| b == b'/')
}

/// A regular file, contiguous (`ITYPE_FILE`) or fragmented (`ITYPE_FILE_FRAG`). Both store
/// data; they differ only in how the data blocks are located (extent-list GSFS0008).
fn is_file(itype: u8) -> bool {
    itype == ITYPE_FILE || itype == ITYPE_FILE_FRAG
}

/// Whether a file-API op writes the filesystem - gated on a READ-ONLY mount (§6.15). The
/// early-match ops (LABEL/CHECK mutate; FLASH/RESET reformat-or-wipe and are allowed; INFO/SCRUB
/// read) are guarded inline; this covers the path-addressed ops dispatched below.
fn op_is_mutating(op: u8) -> bool {
    matches!(op,
        OP_WRITE_FILE | OP_WRITE_NEW | OP_WRITE_AT | OP_WRITE_AT_J |
        OP_MKDIR | OP_MKDIR_P | OP_RENAME | OP_DELETE | OP_DELETE_TREE | OP_MOVE)
}

/// Map the `n`-th data block of a fragmented file to its LBA by walking the extent runs.
/// `exts[..ne]` are `(start, len)` runs in file order; returns `None` past the last block.
fn nth_data_block(exts: &[(u64, u64); EXT_MAX], ne: usize, n: u64) -> Option<u64> {
    let mut acc = 0u64;
    for i in 0..ne {
        let (start, len) = exts[i];
        if n < acc + len { return Some(start + (n - acc)); }
        acc += len;
    }
    None
}

fn encode_rec(blk: &mut [u8], slot: usize, itype: u8, name: &[u8], size: u64, first: u64, count: u64) {
    let o = slot * REC_SIZE;
    for b in &mut blk[o..o + REC_SIZE] { *b = 0; }
    blk[o] = itype;
    let nl = name.len().min(NAME_MAX);
    blk[o + 1] = nl as u8;
    blk[o + 2..o + 2 + nl].copy_from_slice(&name[..nl]);
    blk[o + 40..o + 48].copy_from_slice(&size.to_le_bytes());
    blk[o + 48..o + 56].copy_from_slice(&first.to_le_bytes());
    blk[o + 56..o + 64].copy_from_slice(&count.to_le_bytes());
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

/// One block-driver RPC with restart recovery: if the reply is missing (block-driver may have
/// restarted, leaving our cached cap EndpointDead), reacquire a fresh cap via the registry and
/// retry once (Phase D, §14.3). All block I/O goes through here.
fn block_rpc(ctx: &ServiceContext, req: &[u8]) -> Option<Message> {
    let msg = Message::from_bytes(req);
    if let Some(r) = ctx.request_with_reply("block-driver", &msg) {
        return Some(r);
    }
    if ctx.reacquire_via_registry("block-driver") {
        return ctx.request_with_reply("block-driver", &msg);
    }
    None
}

/// Ask `block-driver` for the disk's sector count (OP_CAPACITY → [BLK_OK, sectors:u64]).
fn block_capacity(ctx: &ServiceContext) -> Option<u64> {
    let reply = block_rpc(ctx, &[OP_CAPACITY])?;
    let p = reply.payload_bytes();
    if p.first() == Some(&BLK_OK) && p.len() >= 9 { Some(u64_at(p, 1)) } else { None }
}

/// Read one 512-byte block at `lba` from `block-driver` over IPC (u64 LBA, §6.3).
fn block_read(ctx: &ServiceContext, lba: u64) -> Option<[u8; BLOCK]> {
    let mut req = [0u8; 9];
    req[0] = OP_READ_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    let reply = block_rpc(ctx, &req)?;
    let p = reply.payload_bytes();
    if p.first() == Some(&BLK_OK) && p.len() >= 1 + BLOCK {
        let mut out = [0u8; BLOCK];
        out.copy_from_slice(&p[1..1 + BLOCK]);
        Some(out)
    } else {
        ctx.log_fmt(format_args!("fs: block read failed at lba {} (device I/O error)", lba));
        None
    }
}

/// Zero a run of `count` blocks from `lba` in one batched op (the driver writes
/// multi-sector zero commands - no per-block IPC). Used to clear the bitmap at format.
fn block_write_zeros(ctx: &ServiceContext, lba: u64, count: u64) -> bool {
    let mut req = [0u8; 17];
    req[0] = OP_WRITE_ZEROS;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    req[9..17].copy_from_slice(&count.to_le_bytes());
    match block_rpc(ctx, &req) {
        Some(reply) => reply.payload_bytes().first() == Some(&BLK_OK),
        None => false,
    }
}

/// Stamp a **directory block**'s CRC trailer over its record region and write it (raw, no
/// transaction). Used by `format` for the root block; the operational path uses the
/// transaction-aware `Fs::td_write`/`Fs::td_read`.
fn dir_write(ctx: &ServiceContext, lba: u64, blk: &mut [u8; BLOCK]) -> bool {
    let c = crc32(&blk[..DIR_REC_REGION]);
    blk[DIR_CRC_OFF..DIR_CRC_OFF + 4].copy_from_slice(&c.to_le_bytes());
    block_write(ctx, lba, blk)
}

/// Write one 512-byte block at `lba` to `block-driver` over IPC (u64 LBA, §6.3).
fn block_write(ctx: &ServiceContext, lba: u64, data: &[u8; BLOCK]) -> bool {
    let mut req = [0u8; 9 + BLOCK];
    req[0] = OP_WRITE_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    req[9..].copy_from_slice(data);
    match block_rpc(ctx, &req) {
        Some(reply) if reply.payload_bytes().first() == Some(&BLK_OK) => true,
        _ => {
            ctx.log_fmt(format_args!("fs: block write failed at lba {} (device I/O error)", lba));
            false
        }
    }
}

/// Write a **file-data block** at `lba`: copy ≤508 payload bytes (zero-padding the rest),
/// stamp the CRC32 of the 508-byte payload at @508, and write the 512-byte block. Direct (not
/// journaled) - data lands in an allocated-but-uncommitted extent, so a pre-commit crash just
/// leaves harmless garbage in free space (Phase C). `payload.len()` must be ≤ DATA_PAYLOAD.
fn data_write(ctx: &ServiceContext, lba: u64, payload: &[u8]) -> bool {
    let mut blk = [0u8; BLOCK];
    let n = payload.len().min(DATA_PAYLOAD);
    blk[..n].copy_from_slice(&payload[..n]);
    let c = crc32(&blk[..DATA_PAYLOAD]);
    blk[DATA_CRC_OFF..DATA_CRC_OFF + 4].copy_from_slice(&c.to_le_bytes());
    block_write(ctx, lba, &blk)
}

/// Read a **file-data block** at `lba` and verify its CRC trailer. Returns the full 512-byte
/// block (payload is `[..DATA_PAYLOAD]`) on success; a CRC mismatch is a loud refusal (§3.12) -
/// `fs` never hands back bytes from a corrupt data block.
fn data_read(ctx: &ServiceContext, lba: u64) -> Option<[u8; BLOCK]> {
    let blk = block_read(ctx, lba)?;
    let stored = u32_at(&blk, DATA_CRC_OFF);
    let actual = crc32(&blk[..DATA_PAYLOAD]);
    if stored != actual {
        ctx.log_fmt(format_args!(
            "fs: data block CRC mismatch at lba {} (stored {:#010x}, actual {:#010x}) - refusing",
            lba, stored, actual
        ));
        return None;
    }
    Some(blk)
}
