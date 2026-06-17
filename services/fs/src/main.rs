// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `fs` — userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **GSFS0004 — checksummed scalable format (docs/persistence.md §6.4 + §6.6).** Three on-disk
//! structures and no more: a **superblock**, a **free bitmap** (1 bit/block, read on
//! demand — the only global structure, a free *map* not a file index), and the
//! **directory tree** of **self-describing `file_record` entries** (`{type, name, size,
//! first_block, block_count}` — no inode table, no inode number, no global file cap). The
//! directory tree *is* the index (walk a path from root); the bitmap is the allocation
//! map; reclamation is intrinsic (`delete`/overwrite free bits). Directories **grow**
//! (reallocate a bigger extent when full) so there is no per-directory entry cap either.
//! The only ceiling is the disk. (`fs_index`, the deferred global enumeration cache, is
//! §6.5 — not built; built when a `find`/search need pulls it in.)
//!
//! `fs` is the single owner of the filesystem (§8): it serves one IPC request at a time,
//! so every mutation is serialized — no concurrency to reconcile. All disk I/O goes through
//! `block-driver` over IPC; this service touches no hardware. Raw-tolerant: a bad magic is
//! a loud refusal, never an auto-format (§3.12).

#![no_std]
#![no_main]

use godspeed_sdk::{CapHandle, Message, ServiceContext};

mod crc32;
use crc32::crc32;

// ── On-disk format — MUST match `osdev format_superblock` (persistence.md §6.6). ──
// GSFS0004 adds integrity checksums (no layout churn otherwise): a superblock CRC32 and a
// per-directory-block CRC32, both verified on read — corruption is a loud refusal (§3.12),
// never silent. The format also reserves a fixed journal region (filled by the Phase-C
// crash-consistency work) so the on-disk geometry is baked once.
const SB_MAGIC: &[u8; 8] = b"GSFS0004";
const BLOCK: usize = 512;
const BITS_PER_BMBLOCK: u64 = (BLOCK as u64) * 8; // 4096 bits per bitmap block

// file_record entry: 64 bytes. GSFS0004 fits 7 per 512-byte directory block and reserves
// the last 64 bytes as a trailer holding the block's CRC32 (over the 448-byte record
// region). The record layout itself is unchanged from GSFS0003 — names stay 38 bytes.
const REC_SIZE: usize = 64;
const RECS_PER_BLOCK: usize = 7; // 7×64 = 448 bytes of records + a 64-byte CRC trailer
const DIR_REC_REGION: usize = RECS_PER_BLOCK * REC_SIZE; // 448 — CRC covers [0..448)
const DIR_CRC_OFF: usize = DIR_REC_REGION; // 448 — u32 CRC32 of the record region
const NAME_MAX: usize = 38; // entry: type u8 @0, name_len u8 @1, name[38] @2, size @40, first @48, count @56

// Crash-consistency journal region (GSFS0004 geometry). Fixed size, bounded (§26.6): a
// transaction larger than this is refused loudly, never partially applied.
const JOURNAL_BLOCKS: u64 = 64; // 64 × 512 B = 32 KiB
// One commit/header block + up to TXN_CAP data blocks must fit the journal region.
const TXN_CAP: usize = 56; // max structural blocks one transaction may stage
const JOURNAL_MAGIC: u32 = 0x474A_3034; // "GJ04" — marks a committed transaction
const COMMIT_CRC_OFF: usize = 508; // commit record: CRC32 of [0..8+n*8] lives at @508

// Recursive-delete depth cap (§26.6). Paths are capped well below this by the wire
// `path_len` (u8) and the shell's PATH_MAX (120), so this is a backstop, not the binding
// limit — a too-deep tree is refused loudly rather than risking the service stack.
const MAX_TREE_DEPTH: u32 = 64;
const LABEL_MAX: usize = 31; // superblock: label_len u8 @76, label[31] @77

const ITYPE_FREE: u8 = 0;
const ITYPE_FILE: u8 = 1;
const ITYPE_DIR: u8 = 2;

// Per-message data chunk: the most file bytes that travel in one IPC message, bounded so a
// request/reply (a few header bytes + data) never exceeds MAX_PAYLOAD (4096). This is the
// **streaming chunk size**, NOT a file-size cap — large files are read/written across many
// of these chunks via the offset-addressed ops (WRITE_NEW/WRITE_AT/READ_AT). The one-shot
// WRITE_FILE/READ_FILE ops carry a whole small file (≤ this) in a single message.
const MAX_FILE_BYTES: usize = 7 * BLOCK; // 3584 — streaming chunk size

// Block IPC protocol (fs <-> block-driver). MUST match `services/block-driver`.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
const OP_CAPACITY: u8 = 3;
const OP_WRITE_ZEROS: u8 = 4; // [op, lba:u64, count:u64] — zero a run of blocks (fast format)
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
// Large-file streaming (offset-addressed; stateless — each request is self-contained, §8).
// A big file = WRITE_NEW (allocate the whole extent, size it) then a sequence of WRITE_AT
// chunks; read it back with STAT (for the size) + a sequence of READ_AT chunks.
const OP_WRITE_NEW: u8 = 24; // [op, plen, path, total:u64] — create/truncate `path` sized `total`
const OP_WRITE_AT: u8 = 25;  // [op, plen, path, offset:u64, chunk…] — write chunk at byte offset
const OP_READ_AT: u8 = 26;   // [op, plen, path, offset:u64, len:u32] → [FS_OK, n:u32, bytes]
const FS_OK: u8 = 0;
const FS_ERR: u8 = 1;
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3;

/// In-memory superblock view. No inode table — the tree lives on disk and is read on
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
    flags: u32,
    label: [u8; LABEL_MAX],
    label_len: u8,
    // Crash-consistency journal (Phase C). While `txn_active`, structural writes (directory,
    // bitmap, superblock) are STAGED here — with read-your-writes — instead of going to disk,
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

    let capacity = block_capacity(&ctx).unwrap_or(0);
    ctx.log_fmt(format_args!("fs: disk capacity = {} sectors ({} MiB)", capacity, capacity / 2048));

    // Raw-tolerant: a bad superblock is the normal state of a never-flashed drive (§3.12).
    let mut fs: Option<Fs> = match Fs::mount(&ctx) {
        Ok(f) => {
            ctx.log_fmt(format_args!(
                "fs: mounted GSFS0004 ({} blocks, bitmap {}..{}, root@{}, {} free)",
                f.total_blocks, f.bitmap_start, f.data_start, f.root_first_block, f.free_blocks
            ));
            Some(f)
        }
        Err(e) => {
            ctx.log_fmt(format_args!("fs: no filesystem ({}) — awaiting drives flash", e));
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

    // Register our name so clients can (re)acquire a cap to us via the registry — the path
    // that lets the shell recover after an `fs` restart (Phase D, §14.3).
    let _ = ctx.register("fs");

    ctx.log("fs: serving file API");
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() {
            Some(c) => c,
            None => continue,
        };
        serve(&ctx, &mut fs, capacity, msg.payload_bytes(), reply);
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
/// chunks via `write_new`/`write_at`/`read_at` — far past the one-message cap. The content
/// is a deterministic pattern generated and verified chunk-by-chunk, so no big buffer is
/// needed anywhere. Creates the file if absent (boot 1), then always verifies (boot 2 proves
/// it survived the reboot).
#[cfg(feature = "selftest")]
fn large_file_check(ctx: &ServiceContext, fs: &mut Fs) {
    const BIG: &[u8] = b"/big.bin";
    const N: u64 = 200 * 1024; // 204800 bytes — ~57 chunks
    let pat = |k: u64| -> u8 { (k.wrapping_mul(131).wrapping_add(7) & 0xFF) as u8 };

    let present = matches!(fs.walk(ctx, BIG), Some(e) if e.itype == ITYPE_FILE && e.size == N);
    if !present {
        if fs.write_new(ctx, BIG, N).is_err() { ctx.log("fs: large write_new FAILED"); return; }
        let mut chunk = [0u8; MAX_FILE_BYTES];
        let mut off = 0u64;
        while off < N {
            let len = (MAX_FILE_BYTES as u64).min(N - off) as usize;
            for i in 0..len { chunk[i] = pat(off + i as u64); }
            if fs.write_at(ctx, BIG, off, &chunk[..len]).is_err() {
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
/// replays the committed transaction from the journal — so the file is present with the right
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
    // Boot 1: file absent — write it, arming the crash so commit_txn halts post-commit.
    ctx.log("fs: jcrash boot1 — writing /jcrash.txt, will halt after commit record");
    fs.begin_txn();
    fs.crash_after_commit = true;
    let _ = fs.write_path(ctx, F, D);
    let _ = fs.commit_txn(ctx); // halts inside (armed) — control does not return
    ctx.log("fs: jcrash boot1 did NOT crash (unexpected)");
}

/// Dispatch one request and reply through the client's `reply` cap.
fn serve(ctx: &ServiceContext, vol: &mut Option<Fs>, capacity: u64, p: &[u8], reply: CapHandle) {
    let send = |bytes: &[u8]| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(bytes)); };
    if p.is_empty() {
        send(&[FS_ERR]);
        return;
    }

    // drives API — INFO/FLASH work on a raw disk; LABEL/RESET as below.
    match p[0] {
        OP_DRIVES_INFO => {
            // [FS_OK, mounted, capacity:u64, used:u64, flags:u8, label_len:u8, label…]
            let mut out = [0u8; 28 + LABEL_MAX];
            out[0] = FS_OK;
            out[2..10].copy_from_slice(&capacity.to_le_bytes());
            if let Some(f) = vol {
                out[1] = 1;
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
            if capacity == 0 { send(&[FS_ERR]); }
            else if block_write(ctx, 0, &[0u8; BLOCK]) { *vol = None; send(&[FS_OK]); }
            else { send(&[FS_ERR]); }
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
            send(&[match fs.write_at(ctx, path, offset, chunk) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
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
        // delete_tree manages its own transactions (unlink + batched frees) — not wrapped.
        OP_DELETE_TREE => send(&[match fs.delete_tree(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_MOVE => txn!(fs.move_path(ctx, path, tail)),
        _ => send(&[FS_ERR]),
    }
}

impl Fs {
    // ── mount / format / drive metadata ──────────────────────────────────────
    fn mount(ctx: &ServiceContext) -> Result<Fs, &'static str> {
        let sb = block_read(ctx, 0).ok_or("block 0 read failed (block-driver unreachable?)")?;
        if &sb[0..8] != SB_MAGIC {
            return Err("bad superblock magic — disk not formatted (run drives flash)");
        }
        // Integrity: the superblock carries a CRC32 over its first 124 bytes (§3.12). A
        // mismatch is a loud refusal — a corrupt superblock is never trusted or auto-fixed.
        if u32_at(&sb, 124) != crc32(&sb[..124]) {
            return Err("superblock checksum mismatch — refusing to mount corrupt filesystem");
        }
        // Crash recovery: replay a committed-but-unfinished transaction before serving (§9).
        // Idempotent — a clean shutdown leaves no commit record, so this is a no-op then.
        Fs::recover(ctx, u64_at(&sb, 108));
        let mut label = [0u8; LABEL_MAX];
        let ll = (sb[76] as usize).min(LABEL_MAX);
        label[..ll].copy_from_slice(&sb[77..77 + ll]);
        Ok(Fs {
            total_blocks: u64_at(&sb, 16),
            bitmap_start: u64_at(&sb, 24),
            data_start: u64_at(&sb, 40),
            journal_start: u64_at(&sb, 108),
            journal_blocks: u64_at(&sb, 116),
            root_first_block: u64_at(&sb, 48),
            root_block_count: u64_at(&sb, 56),
            free_blocks: u64_at(&sb, 64),
            flags: u32_at(&sb, 72),
            label,
            label_len: ll as u8,
            txn_active: false,
            txn_n: 0,
            txn_overflow: false,
            txn_lba: [0; TXN_CAP],
            txn_blk: [[0u8; BLOCK]; TXN_CAP],
            crash_after_commit: false,
        })
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
            ctx.log_fmt(format_args!("fs: directory block CRC mismatch at lba {} — refusing", lba));
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
        // 2. Write the commit record — the atomic point. magic + n + home LBAs + CRC32.
        let mut commit = [0u8; BLOCK];
        commit[0..4].copy_from_slice(&JOURNAL_MAGIC.to_le_bytes());
        commit[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        for i in 0..n {
            commit[8 + i * 8..16 + i * 8].copy_from_slice(&self.txn_lba[i].to_le_bytes());
        }
        let crc = crc32(&commit[..8 + n * 8]);
        commit[COMMIT_CRC_OFF..COMMIT_CRC_OFF + 4].copy_from_slice(&crc.to_le_bytes());
        if !block_write(ctx, self.journal_start, &commit) { return Err("journal commit write failed"); }
        // Test-only: simulate a power loss right here — commit record durable, home not yet
        // updated. The next mount must replay this transaction. (Never set in production.)
        if self.crash_after_commit {
            ctx.log("fs: [journal-crash-test] commit record durable — halting before checkpoint (simulated crash)");
            loop { ctx.yield_cpu(); }
        }
        // 3. Checkpoint: write each staged block to its home LBA.
        for i in 0..n {
            if !block_write(ctx, self.txn_lba[i], &self.txn_blk[i]) {
                // Commit is durable: the next mount will replay this transaction. Report, but
                // the data is safe — no corruption, only a deferred checkpoint.
                return Err("checkpoint write failed (will replay on next mount)");
            }
        }
        // 4. Invalidate the journal (idempotent — recovery tolerates a stale commit too).
        let _ = block_write(ctx, self.journal_start, &[0u8; BLOCK]);
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
        for i in 0..n {
            let lba = u64_at(&commit, 8 + i * 8);
            if let Some(blk) = block_read(ctx, journal_start + 1 + i as u64) {
                let _ = block_write(ctx, lba, &blk);
            }
        }
        let _ = block_write(ctx, journal_start, &[0u8; BLOCK]); // invalidate
        ctx.log_fmt(format_args!("fs: journal recovered {} block(s) from an interrupted write", n));
    }

    /// Format the disk as an empty GSFS0004 sized to `capacity`, then mount. Same layout
    /// `osdev format_superblock` writes. `drives flash`; only ever user-initiated (§3.12).
    fn format(ctx: &ServiceContext, capacity: u64, label: &[u8]) -> Result<Fs, &'static str> {
        let total_blocks = capacity;
        let bitmap_start: u64 = 1;
        let bitmap_blocks = (total_blocks + BITS_PER_BMBLOCK - 1) / BITS_PER_BMBLOCK;
        // Reserve the journal region between the bitmap and the data region (GSFS0004).
        let journal_start = bitmap_start + bitmap_blocks;
        let journal_blocks = JOURNAL_BLOCKS;
        let data_start = journal_start + journal_blocks;
        let root_first_block = data_start;
        let root_block_count: u64 = 1;
        let used_through = data_start + root_block_count;
        if total_blocks < used_through + 1 {
            return Err("disk too small for a filesystem");
        }
        let free_blocks = total_blocks - used_through;

        let mut sb = [0u8; BLOCK];
        sb[0..8].copy_from_slice(SB_MAGIC);
        sb[8..12].copy_from_slice(&4u32.to_le_bytes());
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
        let sb_crc = crc32(&sb[..124]);
        sb[124..128].copy_from_slice(&sb_crc.to_le_bytes());
        if !block_write(ctx, 0, &sb) { return Err("superblock write failed"); }

        // Zero the bitmap region in one batched op (driver writes multi-sector zero runs —
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
        // Re-stamp the integrity CRC over the updated superblock (§3.12).
        let sb_crc = crc32(&sb[..124]);
        sb[124..128].copy_from_slice(&sb_crc.to_le_bytes());
        if !self.tb_write(ctx, 0, &sb) { return Err("superblock write failed"); }
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
                        self.free_blocks = self.free_blocks.saturating_sub(n);
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
        self.free_blocks += count;
        self.persist_super(ctx)
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
        // No free slot — grow, then place in the fresh block.
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
            if e.itype != ITYPE_FILE { return Err("path is a directory"); }
        }
        let blocks = ((data.len() + BLOCK - 1) / BLOCK).max(1) as u64;
        // Alloc the new extent first (old still allocated), so a failure leaves the file
        // intact; free the old extent only after the record points at the new one.
        let first = self.alloc_run(ctx, blocks)?;
        for i in 0..blocks as usize {
            let mut blk = [0u8; BLOCK];
            let s = i * BLOCK;
            let e = (s + BLOCK).min(data.len());
            if s < data.len() { blk[..e - s].copy_from_slice(&data[s..e]); }
            if !block_write(ctx, first + i as u64, &blk) { return Err("block write failed"); }
        }
        match existing {
            Some(e) => {
                let (old_first, old_count) = (e.first_block, e.block_count);
                let ne = Entry { itype: ITYPE_FILE, size: data.len() as u64, first_block: first, block_count: blocks, loc: e.loc };
                self.persist_entry(ctx, &ne)?;
                self.free_run(ctx, old_first, old_count)?;
            }
            None => self.dir_add(ctx, &mut parent, name, ITYPE_FILE, data.len() as u64, first, blocks)?,
        }
        Ok(())
    }

    fn read_path(&self, ctx: &ServiceContext, path: &[u8], out: &mut [u8]) -> Option<usize> {
        let e = self.walk(ctx, path)?;
        if e.itype != ITYPE_FILE { return None; }
        let size = e.size as usize;
        if size > out.len() { return None; }
        for b in 0..e.block_count {
            let start = (b as usize) * BLOCK;
            if start >= size { break; }
            let blk = block_read(ctx, e.first_block + b)?;
            let end = (start + BLOCK).min(size);
            out[start..end].copy_from_slice(&blk[..end - start]);
        }
        Some(size)
    }

    // ── large-file streaming (offset-addressed; the data path that lifts the one-message
    //    file-size cap). A big file is created once with `write_new` (which allocates the
    //    whole extent up front), filled by sequential `write_at` chunks, and read back with
    //    `read_at` chunks. No open-file/session state — each call is self-contained (§8).

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
            if e.itype != ITYPE_FILE { return Err("path is a directory"); }
        }
        let blocks = ((total + BLOCK as u64 - 1) / BLOCK as u64).max(1);
        let first = self.alloc_run(ctx, blocks)?;
        match existing {
            Some(e) => {
                let (old_first, old_count) = (e.first_block, e.block_count);
                let ne = Entry { itype: ITYPE_FILE, size: total, first_block: first, block_count: blocks, loc: e.loc };
                self.persist_entry(ctx, &ne)?;
                self.free_run(ctx, old_first, old_count)
            }
            None => self.dir_add(ctx, &mut parent, name, ITYPE_FILE, total, first, blocks),
        }
    }

    /// Write `chunk` into an existing file at byte `offset`. The offset must be block-aligned
    /// (clients stream in block-aligned chunks), so whole blocks are written with no
    /// read-modify-write; the final block of a partial chunk is zero-padded. Bounded to the
    /// file's allocated extent — a write past it is a loud error, never an overrun.
    fn write_at(&self, ctx: &ServiceContext, path: &[u8], offset: u64, chunk: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.itype != ITYPE_FILE { return Err("not a file"); }
        if offset % BLOCK as u64 != 0 { return Err("unaligned offset"); }
        if offset + chunk.len() as u64 > e.block_count * BLOCK as u64 { return Err("write past extent"); }
        let start = e.first_block + offset / BLOCK as u64;
        let nblk = (chunk.len() + BLOCK - 1) / BLOCK;
        for i in 0..nblk {
            let mut blk = [0u8; BLOCK];
            let s = i * BLOCK;
            let end = (s + BLOCK).min(chunk.len());
            blk[..end - s].copy_from_slice(&chunk[s..end]);
            if !block_write(ctx, start + i as u64, &blk) { return Err("block write failed"); }
        }
        Ok(())
    }

    /// Read up to `len` bytes from `path` starting at byte `offset` into `out` (clamped to the
    /// file's size and `out.len()`). Returns the number of bytes read (0 at/after EOF). The
    /// offset need not be block-aligned — it reads across block boundaries as needed.
    fn read_at(&self, ctx: &ServiceContext, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Option<usize> {
        let e = self.walk(ctx, path)?;
        if e.itype != ITYPE_FILE { return None; }
        let size = e.size;
        if offset >= size { return Some(0); }
        let n = len.min((size - offset) as usize).min(out.len());
        let mut done = 0;
        while done < n {
            let pos = offset as usize + done;
            let blk = block_read(ctx, e.first_block + (pos / BLOCK) as u64)?;
            let within = pos % BLOCK;
            let take = (BLOCK - within).min(n - done);
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
                    return Ok(());
                }
            }
        }
        Err("entry not found")
    }

    fn delete(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.loc.is_none() { return Err("cannot delete root"); }
        if e.itype == ITYPE_DIR && !self.dir_is_empty(ctx, &e).ok_or("dir read failed")? {
            return Err("directory not empty");
        }
        let (parent, name) = self.walk_parent(ctx, path).ok_or("not found")?;
        self.dir_remove(ctx, &parent, name)?;
        self.free_run(ctx, e.first_block, e.block_count)
    }

    /// `delete … recursive`: remove a file or a WHOLE subtree. Unlinks the entry from its
    /// parent, then frees the entry and every descendant via `free_subtree`. A file is just
    /// the depth-0 case (no children), so this is a strict superset of `delete`.
    /// Manages its own transactions (so it is NOT wrapped by the serve-level transaction):
    /// the unlink is one atomic transaction, then the now-unreachable subtree is reclaimed in
    /// bounded per-extent transactions — too many blocks to stage as a single atomic set.
    fn delete_tree(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.loc.is_none() { return Err("cannot delete root"); }
        let (parent, name) = self.walk_parent(ctx, path).ok_or("not found")?;
        // Atomic unlink: once this transaction commits, the subtree is unreachable. A crash
        // before it leaves the tree untouched; after it, the entry is gone.
        self.begin_txn();
        let r = self.dir_remove(ctx, &parent, name);
        self.end_txn(ctx, r)?;
        // Reclaim the unreachable subtree in bounded transactions. A crash here only leaks
        // blocks (nothing references them) — never corruption.
        self.free_subtree(ctx, e.itype, e.first_block, e.block_count, 0)
    }

    /// Free an entry's blocks and, if it is a directory, all of its descendants first
    /// (post-order). Bounded two ways (§26.6): a hard **depth** cap, and small stack frames —
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
        }
        // Each extent freed in its own bounded transaction (the subtree is already unlinked).
        self.free_run_txn(ctx, first, count)
    }

    /// Move (relink) an entry: same data, new directory/name. Same-directory move is a
    /// rename. No data copied — only the directory entries change.
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
        self.dir_remove(ctx, &sparent, sname)
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────
fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && !name.iter().any(|&b| b == b'/')
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
        None
    }
}

/// Zero a run of `count` blocks from `lba` in one batched op (the driver writes
/// multi-sector zero commands — no per-block IPC). Used to clear the bitmap at format.
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
        Some(reply) => reply.payload_bytes().first() == Some(&BLK_OK),
        None => false,
    }
}
