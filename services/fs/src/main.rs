// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! `fs` — userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **Phase 3: GSFS0003 — the scalable format (docs/persistence.md §6.4).** Three on-disk
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

// ── On-disk format — MUST match `osdev format_superblock` (persistence.md §6.4). ──
const SB_MAGIC: &[u8; 8] = b"GSFS0003";
const BLOCK: usize = 512;
const BITS_PER_BMBLOCK: u64 = (BLOCK as u64) * 8; // 4096 bits per bitmap block

// file_record entry: 64 bytes, 8 per block.
const REC_SIZE: usize = 64;
const RECS_PER_BLOCK: usize = BLOCK / REC_SIZE; // 8
const NAME_MAX: usize = 38; // entry: type u8 @0, name_len u8 @1, name[38] @2, size @40, first @48, count @56

// Recursive-delete depth cap (§26.6). Paths are capped well below this by the wire
// `path_len` (u8) and the shell's PATH_MAX (120), so this is a backstop, not the binding
// limit — a too-deep tree is refused loudly rather than risking the service stack.
const MAX_TREE_DEPTH: u32 = 64;
const LABEL_MAX: usize = 31; // superblock: label_len u8 @76, label[31] @77

const ITYPE_FREE: u8 = 0;
const ITYPE_FILE: u8 = 1;
const ITYPE_DIR: u8 = 2;

// One file read/write travels in a single IPC message; bound the body so the READ reply
// (5-byte header + data) never exceeds MAX_PAYLOAD (4096).
const MAX_FILE_BYTES: usize = 7 * BLOCK; // 3584

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
    root_first_block: u64,
    root_block_count: u64,
    free_blocks: u64,
    flags: u32,
    label: [u8; LABEL_MAX],
    label_len: u8,
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
                "fs: mounted GSFS0003 ({} blocks, bitmap {}..{}, root@{}, {} free)",
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
                Some(f) => send(&[match f.relabel(ctx, label) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
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
    match op {
        OP_WRITE_FILE => send(&[match fs.write_path(ctx, path, tail) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
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
        OP_MKDIR => send(&[match fs.mkdir(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_MKDIR_P => send(&[match fs.mkdir_parents(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_LIST_DIR => match fs.list_dir(ctx, path) {
            Some(out) => send(&out),
            None => send(&[FS_NOTFOUND]),
        },
        OP_RENAME => send(&[match fs.rename(ctx, path, tail) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_DELETE => send(&[match fs.delete(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_DELETE_TREE => send(&[match fs.delete_tree(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
        OP_MOVE => send(&[match fs.move_path(ctx, path, tail) { Ok(()) => FS_OK, Err(_) => FS_ERR }]),
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
        let mut label = [0u8; LABEL_MAX];
        let ll = (sb[76] as usize).min(LABEL_MAX);
        label[..ll].copy_from_slice(&sb[77..77 + ll]);
        Ok(Fs {
            total_blocks: u64_at(&sb, 16),
            bitmap_start: u64_at(&sb, 24),
            data_start: u64_at(&sb, 40),
            root_first_block: u64_at(&sb, 48),
            root_block_count: u64_at(&sb, 56),
            free_blocks: u64_at(&sb, 64),
            flags: u32_at(&sb, 72),
            label,
            label_len: ll as u8,
        })
    }

    /// Format the disk as an empty GSFS0003 sized to `capacity`, then mount. Same layout
    /// `osdev format_superblock` writes. `drives flash`; only ever user-initiated (§3.12).
    fn format(ctx: &ServiceContext, capacity: u64, label: &[u8]) -> Result<Fs, &'static str> {
        let total_blocks = capacity;
        let bitmap_start: u64 = 1;
        let bitmap_blocks = (total_blocks + BITS_PER_BMBLOCK - 1) / BITS_PER_BMBLOCK;
        let data_start = bitmap_start + bitmap_blocks;
        let root_first_block = data_start;
        let root_block_count: u64 = 1;
        let used_through = data_start + root_block_count;
        if total_blocks < used_through + 1 {
            return Err("disk too small for a filesystem");
        }
        let free_blocks = total_blocks - used_through;

        let mut sb = [0u8; BLOCK];
        sb[0..8].copy_from_slice(SB_MAGIC);
        sb[8..12].copy_from_slice(&3u32.to_le_bytes());
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
        if !block_write(ctx, 0, &sb) { return Err("superblock write failed"); }

        // Zero the bitmap region in one batched op (driver writes multi-sector zero runs —
        // keeps `drives flash` fast even on a 122 GB disk), then mark [0..used_through) used.
        let zero = [0u8; BLOCK];
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
        // Empty root directory block.
        if !block_write(ctx, root_first_block, &zero) { return Err("root dir init failed"); }

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
    fn persist_super(&self, ctx: &ServiceContext) -> Result<(), &'static str> {
        let mut sb = block_read(ctx, 0).ok_or("superblock read failed")?;
        sb[48..56].copy_from_slice(&self.root_first_block.to_le_bytes());
        sb[56..64].copy_from_slice(&self.root_block_count.to_le_bytes());
        sb[64..72].copy_from_slice(&self.free_blocks.to_le_bytes());
        sb[72..76].copy_from_slice(&self.flags.to_le_bytes());
        let ll = (self.label_len as usize).min(LABEL_MAX);
        sb[76] = ll as u8;
        for b in &mut sb[77..77 + LABEL_MAX] { *b = 0; }
        sb[77..77 + ll].copy_from_slice(&self.label[..ll]);
        if !block_write(ctx, 0, &sb) { return Err("superblock write failed"); }
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
            let blk = block_read(ctx, bm_blk).ok_or("bitmap read failed")?;
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
    fn bm_set_range(&self, ctx: &ServiceContext, first: u64, count: u64, used: bool) -> Result<(), &'static str> {
        let end = first + count;
        let mut b = first;
        while b < end {
            let bm_blk = self.bitmap_start + b / BITS_PER_BMBLOCK;
            let mut blk = block_read(ctx, bm_blk).ok_or("bitmap read failed")?;
            let base = (b / BITS_PER_BMBLOCK) * BITS_PER_BMBLOCK;
            let stop = end.min(base + BITS_PER_BMBLOCK);
            while b < stop {
                let w = (b - base) as usize;
                if used { blk[w / 8] |= 1 << (w % 8); } else { blk[w / 8] &= !(1 << (w % 8)); }
                b += 1;
            }
            if !block_write(ctx, bm_blk, &blk) { return Err("bitmap write failed"); }
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
            let blk = block_read(ctx, block)?;
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
            let mut blk = block_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                if blk[slot * REC_SIZE] == ITYPE_FREE {
                    encode_rec(&mut blk, slot, itype, name, size, first, count);
                    if !block_write(ctx, block, &blk) { return Err("dir write failed"); }
                    return Ok(());
                }
            }
        }
        // No free slot — grow, then place in the fresh block.
        self.grow_dir(ctx, dir)?;
        let block = dir.first_block + dir.block_count - 1;
        let mut blk = block_read(ctx, block).ok_or("dir read failed")?;
        encode_rec(&mut blk, 0, itype, name, size, first, count);
        if !block_write(ctx, block, &blk) { return Err("dir write failed"); }
        Ok(())
    }

    /// Grow a directory by one block: reallocate a bigger contiguous extent, copy, free the
    /// old, and persist the directory's own record (extent changed).
    fn grow_dir(&mut self, ctx: &ServiceContext, dir: &mut Entry) -> Result<(), &'static str> {
        let new_count = dir.block_count + 1;
        let new_first = self.alloc_run(ctx, new_count)?;
        for bi in 0..dir.block_count {
            let blk = block_read(ctx, dir.first_block + bi).ok_or("dir read failed")?;
            if !block_write(ctx, new_first + bi, &blk) { return Err("dir copy failed"); }
        }
        if !block_write(ctx, new_first + dir.block_count, &[0u8; BLOCK]) { return Err("dir grow init failed"); }
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
                let mut blk = block_read(ctx, loc.block).ok_or("record read failed")?;
                let o = loc.slot * REC_SIZE;
                blk[o + 40..o + 48].copy_from_slice(&e.size.to_le_bytes());
                blk[o + 48..o + 56].copy_from_slice(&e.first_block.to_le_bytes());
                blk[o + 56..o + 64].copy_from_slice(&e.block_count.to_le_bytes());
                if !block_write(ctx, loc.block, &blk) { return Err("record write failed"); }
                Ok(())
            }
        }
    }

    fn dir_remove(&self, ctx: &ServiceContext, dir: &Entry, name: &[u8]) -> Result<(), &'static str> {
        for bi in 0..dir.block_count {
            let block = dir.first_block + bi;
            let mut blk = block_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                if blk[o] == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if &blk[o + 2..o + 2 + nl] == name {
                    blk[o] = ITYPE_FREE;
                    if !block_write(ctx, block, &blk) { return Err("dir write failed"); }
                    return Ok(());
                }
            }
        }
        Err("entry not found")
    }

    fn dir_is_empty(&self, ctx: &ServiceContext, dir: &Entry) -> Option<bool> {
        for bi in 0..dir.block_count {
            let blk = block_read(ctx, dir.first_block + bi)?;
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
        if !block_write(ctx, first, &[0u8; BLOCK]) { return Err("dir block init failed"); }
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
                    if !block_write(ctx, first, &[0u8; BLOCK]) { return Err("dir block init failed"); }
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

    /// Reply: `[FS_OK, count:u8, {name_len:u8, name, is_dir:u8, size:u64}…]`, one block.
    fn list_dir(&self, ctx: &ServiceContext, path: &[u8]) -> Option<[u8; BLOCK]> {
        let d = self.walk(ctx, path)?;
        if d.itype != ITYPE_DIR { return None; }
        let mut out = [0u8; BLOCK];
        out[0] = FS_OK;
        let mut count = 0u8;
        let mut w = 2usize;
        for bi in 0..d.block_count {
            let blk = block_read(ctx, d.first_block + bi)?;
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

    fn rename(&self, ctx: &ServiceContext, path: &[u8], newname: &[u8]) -> Result<(), &'static str> {
        if !valid_name(newname) { return Err("bad new name"); }
        let (parent, oldname) = self.walk_parent(ctx, path).ok_or("path not found")?;
        if parent.itype != ITYPE_DIR { return Err("parent not a directory"); }
        if self.dir_find(ctx, &parent, newname).is_some() { return Err("name already exists"); }
        for bi in 0..parent.block_count {
            let block = parent.first_block + bi;
            let mut blk = block_read(ctx, block).ok_or("dir read failed")?;
            for slot in 0..RECS_PER_BLOCK {
                let o = slot * REC_SIZE;
                if blk[o] == ITYPE_FREE { continue; }
                let nl = blk[o + 1] as usize;
                if nl == 0 || nl > NAME_MAX { continue; }
                if &blk[o + 2..o + 2 + nl] == oldname {
                    blk[o + 1] = newname.len() as u8;
                    for b in &mut blk[o + 2..o + 2 + NAME_MAX] { *b = 0; }
                    blk[o + 2..o + 2 + newname.len()].copy_from_slice(newname);
                    if !block_write(ctx, block, &blk) { return Err("dir write failed"); }
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
    fn delete_tree(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let e = self.walk(ctx, path).ok_or("not found")?;
        if e.loc.is_none() { return Err("cannot delete root"); }
        // Unlink first so a mid-walk failure can't leave the parent pointing at half-freed
        // blocks; the subtree is then unreachable and we reclaim it.
        let (parent, name) = self.walk_parent(ctx, path).ok_or("not found")?;
        self.dir_remove(ctx, &parent, name)?;
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
                    let blk = block_read(ctx, first + bi).ok_or("dir read failed")?;
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
        self.free_run(ctx, first, count)
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

/// Ask `block-driver` for the disk's sector count (OP_CAPACITY → [BLK_OK, sectors:u64]).
fn block_capacity(ctx: &ServiceContext) -> Option<u64> {
    let reply = ctx.request_with_reply("block-driver", &Message::from_bytes(&[OP_CAPACITY]))?;
    let p = reply.payload_bytes();
    if p.first() == Some(&BLK_OK) && p.len() >= 9 { Some(u64_at(p, 1)) } else { None }
}

/// Read one 512-byte block at `lba` from `block-driver` over IPC (u64 LBA, §6.3).
fn block_read(ctx: &ServiceContext, lba: u64) -> Option<[u8; BLOCK]> {
    let mut req = [0u8; 9];
    req[0] = OP_READ_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    let reply = ctx.request_with_reply("block-driver", &Message::from_bytes(&req))?;
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
    match ctx.request_with_reply("block-driver", &Message::from_bytes(&req)) {
        Some(reply) => reply.payload_bytes().first() == Some(&BLK_OK),
        None => false,
    }
}

/// Write one 512-byte block at `lba` to `block-driver` over IPC (u64 LBA, §6.3).
fn block_write(ctx: &ServiceContext, lba: u64, data: &[u8; BLOCK]) -> bool {
    let mut req = [0u8; 9 + BLOCK];
    req[0] = OP_WRITE_BLOCK;
    req[1..9].copy_from_slice(&lba.to_le_bytes());
    req[9..].copy_from_slice(data);
    match ctx.request_with_reply("block-driver", &Message::from_bytes(&req)) {
        Some(reply) => reply.payload_bytes().first() == Some(&BLK_OK),
        None => false,
    }
}
