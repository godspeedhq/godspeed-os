//! `fs` — userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **Phase 2: a real hierarchical filesystem (GSFS, magic "GSFS0002").** Mounts by
//! reading the superblock (LBA 0) from `block-driver`, then resolves files and
//! directories by **path** (`/a/b/c`) through an on-disk **inode table** + per-directory
//! **directory blocks** (`name → inode`). All capacity-bearing fields are **u64**
//! (volume size, file size, block pointers, the block-IPC LBA) — see §6.3, the ~8 ZiB
//! ceiling. Directories and files are inodes; a directory's contents are entries naming
//! child inodes. Path walking starts at the root inode.
//!
//! Bounded & loud, in the Godspeed spirit (§26.6): a fixed inode count, a fixed name
//! length, one block per directory (16 entries), contiguous file extents via a bump
//! allocator (no reclamation yet — overwrite leaks the old extent, a Phase-1 carry-over,
//! §26.2). No POSIX permission bits (authority is by capability, §3.3) and no hard links.
//! Bad superblock magic is a loud mount refusal, never an auto-reformat (§3.12).
//!
//! All disk I/O goes through `block-driver` over IPC; this service touches no hardware.
//! On mount it runs a self-test that exercises the hierarchy (mkdir + a nested file) and
//! is reboot-aware (verifies persisted files on a second boot), then serves the file API.

#![no_std]
#![no_main]

use godspeed_sdk::{CapHandle, Message, ServiceContext};

// ── On-disk format — MUST match `osdev mkfs` (docs/persistence.md §6.2/§6.3). ──
// 512-byte blocks (= one ATA/AHCI sector = one block-IPC request), so block number = LBA.
const SB_MAGIC: &[u8; 8] = b"GSFS0002";
const BLOCK: usize = 512;

// Inode table: INODE_COUNT slots × INODE_SIZE bytes.
const INODE_SIZE: usize = 64;
const INODES_PER_BLOCK: usize = BLOCK / INODE_SIZE; // 8
const INODE_COUNT: usize = 256;

const ITYPE_FREE: u8 = 0;
const ITYPE_FILE: u8 = 1;
const ITYPE_DIR: u8 = 2;

// Directory block: one 512-byte block = DIRENTS_PER_BLOCK entries × DIRENT_SIZE bytes.
// One block per directory in this phase (bounded; overflow is a loud error).
const DIRENT_SIZE: usize = 32;
const DIRENTS_PER_BLOCK: usize = BLOCK / DIRENT_SIZE; // 16
const NAME_MAX: usize = 27; // dirent: name_len u8 + name[27] + inode u32 = 32

// One file read/write travels in a single IPC message; bound the body so the file API
// reply (5-byte header + data) never exceeds MAX_PAYLOAD (4096).
const MAX_FILE_BYTES: usize = 7 * BLOCK; // 3584

// Block IPC protocol (fs <-> block-driver). MUST match `services/block-driver`.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
const OP_CAPACITY: u8 = 3;
const BLK_OK: u8 = 0;

// fs file API (client <-> fs). Paths are passed where the name was (§8 of persistence.md).
const OP_WRITE_FILE: u8 = 10;
const OP_READ_FILE: u8 = 11;
const OP_STAT_FILE: u8 = 12;
const OP_MKDIR: u8 = 13;
const OP_LIST_DIR: u8 = 14;
const FS_OK: u8 = 0;
const FS_ERR: u8 = 1;
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3; // no filesystem on the disk (raw — needs `drives flash`)

#[derive(Clone, Copy)]
struct Inode {
    itype: u8,
    size: u64,
    first_block: u64,
    block_count: u64,
}

impl Inode {
    const FREE: Inode = Inode { itype: ITYPE_FREE, size: 0, first_block: 0, block_count: 0 };
    fn is_dir(&self) -> bool { self.itype == ITYPE_DIR }
    fn is_file(&self) -> bool { self.itype == ITYPE_FILE }
}

struct Fs {
    total_blocks: u64,
    inode_table_start: u64,
    inode_table_blocks: u64,
    data_start: u64,
    next_free_block: u64,
    root_inode: u32,
    inodes: [Inode; INODE_COUNT],
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("fs: starting");

    // Disk size, from block-driver's IDENTIFY — what a future `drives flash` sizes a
    // fresh filesystem to (step 3b). 0 if block-driver is unreachable.
    let capacity = block_capacity(&ctx).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "fs: disk capacity = {} sectors ({} MiB)", capacity, capacity / 2048
    ));

    // Raw-tolerant: a bad superblock is NOT fatal (a raw disk is the normal state of a
    // never-flashed drive). fs stays up; `drives flash` will format it (step 3b). It is
    // never auto-formatted — silent reformat is forbidden (§3.12).
    let mut fs: Option<Fs> = match Fs::mount(&ctx) {
        Ok(f) => {
            ctx.log_fmt(format_args!(
                "fs: mounted GSFS ({} blocks, {} inodes, data@{}, next_free={})",
                f.total_blocks, INODE_COUNT, f.data_start, f.next_free_block
            ));
            Some(f)
        }
        Err(e) => {
            ctx.log_fmt(format_args!("fs: no filesystem ({}) — awaiting drives flash", e));
            None
        }
    };

    if let Some(ref mut f) = fs {
        self_test(&ctx, f);
    }

    // Serve the file API to other services over IPC (the reply-cap pattern, §8).
    ctx.log("fs: serving file API");
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() {
            Some(c) => c,
            None => continue,
        };
        serve(&ctx, &mut fs, msg.payload_bytes(), reply);
        ctx.remove_cap(reply);
    }
}

/// Exercise the hierarchy and reboot survival. On a fresh disk: `mkdir /etc`, write a
/// nested file `/etc/motd`, write a top-level `/greeting`. On a later boot: verify both
/// already exist (persistence). Log strings are stable — the `osdev` tests gate on them.
fn self_test(ctx: &ServiceContext, fs: &mut Fs) {
    const GREET: &[u8] = b"/greeting";
    const GREET_DATA: &[u8] = b"hello, persistence!";
    const DIR: &[u8] = b"/etc";
    const NESTED: &[u8] = b"/etc/motd";
    const NESTED_DATA: &[u8] = b"godspeed hierarchical fs";
    let mut buf = [0u8; MAX_FILE_BYTES];

    // Reboot path: if /greeting is already there and correct, we have persisted state.
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

    // Fresh disk: build a small tree.
    match fs.mkdir(ctx, DIR) {
        Ok(()) => ctx.log("fs: mkdir /etc OK"),
        Err(e) => ctx.log_fmt(format_args!("fs: mkdir /etc FAILED: {}", e)),
    }
    match fs.write_path(ctx, NESTED, NESTED_DATA) {
        Ok(()) => match fs.read_path(ctx, NESTED, &mut buf) {
            Some(n) if &buf[..n] == NESTED_DATA => ctx.log("fs: nested file round-trip OK (/etc/motd)"),
            Some(_) => ctx.log("fs: nested round-trip MISMATCH"),
            None => ctx.log("fs: nested read-back FAILED"),
        },
        Err(e) => ctx.log_fmt(format_args!("fs: nested write FAILED: {}", e)),
    }
    match fs.write_path(ctx, GREET, GREET_DATA) {
        Ok(()) => match fs.read_path(ctx, GREET, &mut buf) {
            Some(n) if &buf[..n] == GREET_DATA => ctx.log("fs: file round-trip OK (greeting)"),
            Some(_) => ctx.log("fs: file round-trip MISMATCH"),
            None => ctx.log("fs: read-back FAILED"),
        },
        Err(e) => ctx.log_fmt(format_args!("fs: write FAILED: {}", e)),
    }
}

/// Dispatch one file-API request and reply through the client's `reply` cap.
/// Layout: `[op:u8, path_len:u8, path[path_len], (WriteFile: data)]`.
///
/// File ops require a mounted filesystem; on a raw disk (`fs` is `None`) they return
/// `FS_NOFS` so the caller can tell "no filesystem here" from "not found". (The drives
/// API — flash/info/label — is added in step 3b and works in both states.)
fn serve(ctx: &ServiceContext, vol: &mut Option<Fs>, p: &[u8], reply: CapHandle) {
    let send = |bytes: &[u8]| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(bytes)); };
    if p.len() < 2 {
        send(&[FS_ERR]);
        return;
    }
    let fs = match vol {
        Some(f) => f,
        None => {
            send(&[FS_NOFS]);
            return;
        }
    };
    let op = p[0];
    let plen = p[1] as usize;
    if p.len() < 2 + plen {
        send(&[FS_ERR]);
        return;
    }
    let path = &p[2..2 + plen];
    match op {
        OP_WRITE_FILE => {
            let data = &p[2 + plen..];
            send(&[match fs.write_path(ctx, path, data) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }
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
            // Reply: [FS_OK, exists:u8, size:u64 LE, is_dir:u8]
            let mut out = [0u8; 11];
            out[0] = FS_OK;
            match fs.walk(ctx, path) {
                Some(i) => {
                    let n = fs.inodes[i as usize];
                    out[1] = 1;
                    out[2..10].copy_from_slice(&n.size.to_le_bytes());
                    out[10] = n.is_dir() as u8;
                }
                None => out[1] = 0,
            }
            send(&out);
        }
        OP_MKDIR => {
            send(&[match fs.mkdir(ctx, path) { Ok(()) => FS_OK, Err(_) => FS_ERR }]);
        }
        OP_LIST_DIR => {
            // Reply: [FS_OK, count:u8, {name_len:u8, name[name_len], is_dir:u8}…]
            match fs.list_dir(ctx, path) {
                Some(out) => send(&out),
                None => send(&[FS_NOTFOUND]),
            }
        }
        _ => send(&[FS_ERR]),
    }
}

impl Fs {
    fn mount(ctx: &ServiceContext) -> Result<Fs, &'static str> {
        let sb = block_read(ctx, 0).ok_or("block 0 read failed (block-driver unreachable?)")?;
        if &sb[0..8] != SB_MAGIC {
            return Err("bad superblock magic — disk not formatted (run osdev mkfs)");
        }
        let inode_table_start = u64_at(&sb, 24);
        let inode_table_blocks = u64_at(&sb, 32);
        let mut fs = Fs {
            total_blocks: u64_at(&sb, 16),
            inode_table_start,
            inode_table_blocks,
            data_start: u64_at(&sb, 40),
            next_free_block: u64_at(&sb, 48),
            root_inode: u32_at(&sb, 56),
            inodes: [Inode::FREE; INODE_COUNT],
        };
        // Read the inode table (INODE_COUNT inodes, INODES_PER_BLOCK per block).
        for b in 0..fs.inode_table_blocks {
            let blk = block_read(ctx, inode_table_start + b).ok_or("inode-table read failed")?;
            for s in 0..INODES_PER_BLOCK {
                let idx = (b as usize) * INODES_PER_BLOCK + s;
                if idx >= INODE_COUNT { break; }
                fs.inodes[idx] = decode_inode(&blk, s * INODE_SIZE);
            }
        }
        Ok(fs)
    }

    // ── inode allocation + persistence ───────────────────────────────────────
    fn alloc_inode(&mut self) -> Option<u32> {
        self.inodes.iter().position(|n| n.itype == ITYPE_FREE).map(|i| i as u32)
    }

    /// Persist one inode by read-modify-writing its inode-table block.
    fn persist_inode(&self, ctx: &ServiceContext, idx: u32) -> Result<(), &'static str> {
        let block = self.inode_table_start + (idx as u64) / (INODES_PER_BLOCK as u64);
        let slot = (idx as usize) % INODES_PER_BLOCK;
        let mut blk = block_read(ctx, block).ok_or("inode block read failed")?;
        encode_inode(&mut blk, slot * INODE_SIZE, &self.inodes[idx as usize]);
        if !block_write(ctx, block, &blk) {
            return Err("inode block write failed");
        }
        Ok(())
    }

    /// Bump-allocate `n` contiguous data blocks; persist the superblock high-water.
    fn alloc_blocks(&mut self, ctx: &ServiceContext, n: u64) -> Result<u64, &'static str> {
        if self.next_free_block + n > self.total_blocks {
            return Err("no space");
        }
        let first = self.next_free_block;
        self.next_free_block += n;
        self.persist_superblock(ctx)?;
        Ok(first)
    }

    fn persist_superblock(&self, ctx: &ServiceContext) -> Result<(), &'static str> {
        let mut sb = block_read(ctx, 0).ok_or("superblock read failed")?;
        sb[48..56].copy_from_slice(&self.next_free_block.to_le_bytes());
        if !block_write(ctx, 0, &sb) {
            return Err("superblock write failed");
        }
        Ok(())
    }

    // ── directory primitives (one block per directory in this phase) ──────────
    /// Find `name` in directory inode `dir`; returns the child inode number.
    fn dir_lookup(&self, ctx: &ServiceContext, dir: u32, name: &[u8]) -> Option<u32> {
        let d = self.inodes[dir as usize];
        if !d.is_dir() { return None; }
        let blk = block_read(ctx, d.first_block)?;
        for e in 0..DIRENTS_PER_BLOCK {
            let o = e * DIRENT_SIZE;
            let nl = blk[o] as usize;
            if nl == 0 || nl > NAME_MAX { continue; }
            if &blk[o + 1..o + 1 + nl] == name {
                return Some(u32_at(&blk, o + 28));
            }
        }
        None
    }

    /// Add `name → child` to directory inode `dir` (first free dirent slot).
    fn dir_add(&self, ctx: &ServiceContext, dir: u32, name: &[u8], child: u32) -> Result<(), &'static str> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err("bad name length");
        }
        let d = self.inodes[dir as usize];
        let mut blk = block_read(ctx, d.first_block).ok_or("dir read failed")?;
        for e in 0..DIRENTS_PER_BLOCK {
            let o = e * DIRENT_SIZE;
            if blk[o] == 0 {
                blk[o] = name.len() as u8;
                blk[o + 1..o + 1 + name.len()].copy_from_slice(name);
                blk[o + 28..o + 32].copy_from_slice(&child.to_le_bytes());
                if !block_write(ctx, d.first_block, &blk) {
                    return Err("dir write failed");
                }
                return Ok(());
            }
        }
        Err("directory full")
    }

    // ── path walking ─────────────────────────────────────────────────────────
    /// Resolve an absolute path to an inode number (file or dir). `/` → root.
    fn walk(&self, ctx: &ServiceContext, path: &[u8]) -> Option<u32> {
        let mut cur = self.root_inode;
        for comp in components(path) {
            cur = self.dir_lookup(ctx, cur, comp)?;
        }
        Some(cur)
    }

    /// Split a path into (parent dir inode, last component). Walks every component but
    /// the last (a one-component lookahead); errors if a parent directory is missing.
    /// Path must be absolute and have ≥1 component.
    fn walk_parent<'a>(&self, ctx: &ServiceContext, path: &'a [u8]) -> Result<(u32, &'a [u8]), &'static str> {
        let mut cur = self.root_inode;
        let mut last: Option<&[u8]> = None;
        for comp in components(path) {
            if let Some(name) = last {
                cur = self.dir_lookup(ctx, cur, name).ok_or("path not found")?;
            }
            last = Some(comp);
        }
        let name = last.ok_or("empty path")?;
        Ok((cur, name))
    }

    // ── operations ────────────────────────────────────────────────────────────
    fn mkdir(&mut self, ctx: &ServiceContext, path: &[u8]) -> Result<(), &'static str> {
        let (parent, name) = self.walk_parent(ctx, path)?;
        if !self.inodes[parent as usize].is_dir() {
            return Err("parent is not a directory");
        }
        if self.dir_lookup(ctx, parent, name).is_some() {
            return Err("already exists");
        }
        let idx = self.alloc_inode().ok_or("inode table full")?;
        let first = self.alloc_blocks(ctx, 1)?;
        // Zero the new (empty) directory block.
        if !block_write(ctx, first, &[0u8; BLOCK]) {
            return Err("dir block init failed");
        }
        self.inodes[idx as usize] = Inode { itype: ITYPE_DIR, size: 0, first_block: first, block_count: 1 };
        self.persist_inode(ctx, idx)?;
        self.dir_add(ctx, parent, name, idx)
    }

    fn write_path(&mut self, ctx: &ServiceContext, path: &[u8], data: &[u8]) -> Result<(), &'static str> {
        if data.len() > MAX_FILE_BYTES {
            return Err("file too large");
        }
        let (parent, name) = self.walk_parent(ctx, path)?;
        if !self.inodes[parent as usize].is_dir() {
            return Err("parent is not a directory");
        }
        // Existing file → reuse its inode (old extent leaks, Phase-1 carry); else create.
        let existing = self.dir_lookup(ctx, parent, name);
        if let Some(i) = existing {
            if !self.inodes[i as usize].is_file() {
                return Err("path is a directory");
            }
        }
        let blocks = ((data.len() + BLOCK - 1) / BLOCK).max(1) as u64;
        let first = self.alloc_blocks(ctx, blocks)?;
        for i in 0..blocks as usize {
            let mut blk = [0u8; BLOCK];
            let start = i * BLOCK;
            let end = (start + BLOCK).min(data.len());
            if start < data.len() {
                blk[..end - start].copy_from_slice(&data[start..end]);
            }
            if !block_write(ctx, first + i as u64, &blk) {
                return Err("block write failed");
            }
        }
        let idx = match existing {
            Some(i) => i,
            None => {
                let i = self.alloc_inode().ok_or("inode table full")?;
                self.inodes[i as usize].itype = ITYPE_FILE; // reserve before dir_add persists
                i
            }
        };
        self.inodes[idx as usize] = Inode {
            itype: ITYPE_FILE,
            size: data.len() as u64,
            first_block: first,
            block_count: blocks,
        };
        self.persist_inode(ctx, idx)?;
        if existing.is_none() {
            self.dir_add(ctx, parent, name, idx)?;
        }
        Ok(())
    }

    fn read_path(&self, ctx: &ServiceContext, path: &[u8], out: &mut [u8]) -> Option<usize> {
        let idx = self.walk(ctx, path)?;
        let n = self.inodes[idx as usize];
        if !n.is_file() { return None; }
        let size = n.size as usize;
        if size > out.len() { return None; }
        for b in 0..n.block_count {
            let start = (b as usize) * BLOCK;
            if start >= size { break; }
            let blk = block_read(ctx, n.first_block + b)?;
            let end = (start + BLOCK).min(size);
            out[start..end].copy_from_slice(&blk[..end - start]);
        }
        Some(size)
    }

    /// List a directory's entries into a wire buffer:
    /// `[FS_OK, count:u8, {name_len:u8, name[name_len], is_dir:u8}…]`.
    fn list_dir(&self, ctx: &ServiceContext, path: &[u8]) -> Option<[u8; BLOCK]> {
        let idx = self.walk(ctx, path)?;
        let d = self.inodes[idx as usize];
        if !d.is_dir() { return None; }
        let blk = block_read(ctx, d.first_block)?;
        let mut out = [0u8; BLOCK];
        out[0] = FS_OK;
        let mut count = 0u8;
        let mut w = 2usize;
        for e in 0..DIRENTS_PER_BLOCK {
            let o = e * DIRENT_SIZE;
            let nl = blk[o] as usize;
            if nl == 0 || nl > NAME_MAX { continue; }
            if w + 1 + nl + 1 > BLOCK { break; }
            let child = u32_at(&blk, o + 28);
            out[w] = nl as u8;
            out[w + 1..w + 1 + nl].copy_from_slice(&blk[o + 1..o + 1 + nl]);
            out[w + 1 + nl] = self.inodes[child as usize].is_dir() as u8;
            w += 1 + nl + 1;
            count += 1;
        }
        out[1] = count;
        Some(out)
    }
}

/// Iterate the non-empty `/`-separated components of an absolute path.
fn components(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty())
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

fn decode_inode(blk: &[u8], o: usize) -> Inode {
    Inode {
        itype: blk[o],
        size: u64_at(blk, o + 8),
        first_block: u64_at(blk, o + 16),
        block_count: u64_at(blk, o + 24),
    }
}
fn encode_inode(blk: &mut [u8], o: usize, n: &Inode) {
    blk[o] = n.itype;
    blk[o + 8..o + 16].copy_from_slice(&n.size.to_le_bytes());
    blk[o + 16..o + 24].copy_from_slice(&n.first_block.to_le_bytes());
    blk[o + 24..o + 32].copy_from_slice(&n.block_count.to_le_bytes());
}

/// Ask `block-driver` for the disk's sector count (OP_CAPACITY → [BLK_OK, sectors:u64]).
fn block_capacity(ctx: &ServiceContext) -> Option<u64> {
    let reply = ctx.request_with_reply("block-driver", &Message::from_bytes(&[OP_CAPACITY]))?;
    let p = reply.payload_bytes();
    if p.first() == Some(&BLK_OK) && p.len() >= 9 {
        Some(u64_at(p, 1))
    } else {
        None
    }
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
