//! `fs` — userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **Phase 1: a flat name → blob store.** Mounts by reading the superblock (LBA 0)
//! from `block-driver`, then stores/retrieves named files using an on-disk entry
//! table + a bump allocator (next-free-block high-water in the superblock).
//! Contiguous extents, no reclamation yet (delete leaks blocks until reformat —
//! a stated Phase-1 limit, §26.2/§26.6). All disk I/O goes through `block-driver`
//! over IPC; this service touches no hardware.
//!
//! On mount it runs a self-test: if `greeting` already exists it reads+verifies it
//! (persistence across reboots, step 5); otherwise it writes it and reads it back
//! (the write/read round-trip, step 4). It then serves the file API over IPC.

#![no_std]
#![no_main]

use godspeed_sdk::{CapHandle, Message, ServiceContext};

// On-disk format — MUST match `osdev mkfs` (docs/persistence.md §6). 512-byte blocks.
const SB_MAGIC: &[u8; 8] = b"GSPDFS01";
const BLOCK: usize = 512;
const MAX_FILES: usize = 16; // 16 entries × 32 bytes = 512 = one entry-table block
const NAME_MAX: usize = 16;
const MAX_FILE_BYTES: usize = 8 * BLOCK; // 4 KiB — fits one IPC message

// Block IPC protocol (fs <-> block-driver). MUST match `services/block-driver`.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
const BLK_OK: u8 = 0;

// fs file API (client <-> fs).
const OP_WRITE_FILE: u8 = 10;
const OP_READ_FILE: u8 = 11;
const OP_STAT_FILE: u8 = 12;
const FS_OK: u8 = 0;
const FS_ERR: u8 = 1;
const FS_NOTFOUND: u8 = 2;

#[derive(Clone, Copy)]
struct Entry {
    name: [u8; NAME_MAX],
    name_len: u8,
    used: bool,
    size: u32,
    first_block: u32,
    block_count: u32,
}

impl Entry {
    const EMPTY: Entry = Entry {
        name: [0; NAME_MAX],
        name_len: 0,
        used: false,
        size: 0,
        first_block: 0,
        block_count: 0,
    };
    fn matches(&self, name: &[u8]) -> bool {
        self.used && &self.name[..self.name_len as usize] == name
    }
}

struct Fs {
    total_blocks: u32,
    entry_table_start: u32,
    next_free_block: u32,
    file_count: u32,
    entries: [Entry; MAX_FILES],
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("fs: starting");

    let mut fs = match Fs::mount(&ctx) {
        Ok(f) => {
            ctx.log_fmt(format_args!(
                "fs: mounted ({} blocks, next_free={}, {} files)",
                f.total_blocks, f.next_free_block, f.file_count
            ));
            f
        }
        Err(e) => {
            ctx.log_fmt(format_args!("fs: mount FAILED: {}", e));
            loop { ctx.yield_cpu(); }
        }
    };

    self_test(&ctx, &mut fs);

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

/// Write `greeting` and read it back (or verify it across a reboot).
fn self_test(ctx: &ServiceContext, fs: &mut Fs) {
    const NAME: &[u8] = b"greeting";
    const DATA: &[u8] = b"hello, persistence!";
    let mut buf = [0u8; MAX_FILE_BYTES];

    if let Some(n) = fs.read_file(ctx, NAME, &mut buf) {
        if &buf[..n] == DATA {
            ctx.log("fs: persisted file 'greeting' verified across boot");
            return;
        }
    }
    match fs.write_file(ctx, NAME, DATA) {
        Ok(()) => match fs.read_file(ctx, NAME, &mut buf) {
            Some(n) if &buf[..n] == DATA => ctx.log("fs: file round-trip OK (greeting)"),
            Some(_) => ctx.log("fs: file round-trip MISMATCH"),
            None => ctx.log("fs: read-back FAILED"),
        },
        Err(e) => ctx.log_fmt(format_args!("fs: write FAILED: {}", e)),
    }
}

/// Dispatch one file-API request and reply through the client's `reply` cap.
fn serve(ctx: &ServiceContext, fs: &mut Fs, p: &[u8], reply: CapHandle) {
    if p.len() < 2 {
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[FS_ERR]));
        return;
    }
    let op = p[0];
    let nlen = (p[1] as usize).min(NAME_MAX);
    if p.len() < 2 + nlen {
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[FS_ERR]));
        return;
    }
    let name = &p[2..2 + nlen];
    match op {
        OP_WRITE_FILE => {
            let data = &p[2 + nlen..];
            let status = match fs.write_file(ctx, name, data) {
                Ok(()) => FS_OK,
                Err(_) => FS_ERR,
            };
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        }
        OP_READ_FILE => {
            let mut buf = [0u8; MAX_FILE_BYTES];
            match fs.read_file(ctx, name, &mut buf) {
                Some(n) => {
                    let mut out = [0u8; 5 + MAX_FILE_BYTES];
                    out[0] = FS_OK;
                    out[1..5].copy_from_slice(&(n as u32).to_le_bytes());
                    out[5..5 + n].copy_from_slice(&buf[..n]);
                    let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out[..5 + n]));
                }
                None => { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[FS_NOTFOUND])); }
            }
        }
        OP_STAT_FILE => {
            let mut out = [0u8; 6];
            match fs.find(name) {
                Some(i) => {
                    out[0] = FS_OK;
                    out[1] = 1; // exists
                    out[2..6].copy_from_slice(&fs.entries[i].size.to_le_bytes());
                }
                None => { out[0] = FS_OK; out[1] = 0; }
            }
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
        }
        _ => { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[FS_ERR])); }
    }
}

impl Fs {
    fn mount(ctx: &ServiceContext) -> Result<Fs, &'static str> {
        let sb = block_read(ctx, 0).ok_or("block 0 read failed (block-driver unreachable?)")?;
        if &sb[0..8] != SB_MAGIC {
            return Err("bad superblock magic — disk not formatted (run osdev mkfs)");
        }
        let entry_table_start = u32_at(&sb, 20);
        let mut fs = Fs {
            total_blocks: u32_at(&sb, 16),
            entry_table_start,
            next_free_block: u32_at(&sb, 32),
            file_count: u32_at(&sb, 36),
            entries: [Entry::EMPTY; MAX_FILES],
        };
        // The entry table is one block: MAX_FILES entries × 32 bytes.
        let et = block_read(ctx, entry_table_start).ok_or("entry-table read failed")?;
        for i in 0..MAX_FILES {
            let o = i * 32;
            let mut e = Entry::EMPTY;
            e.name.copy_from_slice(&et[o..o + NAME_MAX]);
            e.name_len = et[o + 16];
            e.used = et[o + 17] & 1 != 0;
            e.size = u32_at(&et, o + 20);
            e.first_block = u32_at(&et, o + 24);
            e.block_count = u32_at(&et, o + 28);
            fs.entries[i] = e;
        }
        Ok(fs)
    }

    fn find(&self, name: &[u8]) -> Option<usize> {
        self.entries.iter().position(|e| e.matches(name))
    }

    fn write_file(&mut self, ctx: &ServiceContext, name: &[u8], data: &[u8]) -> Result<(), &'static str> {
        if name.is_empty() || name.len() > NAME_MAX {
            return Err("bad name length");
        }
        if data.len() > MAX_FILE_BYTES {
            return Err("file too large");
        }
        let blocks = ((data.len() + BLOCK - 1) / BLOCK) as u32;
        if self.next_free_block + blocks > self.total_blocks {
            return Err("no space");
        }
        // Allocate a fresh contiguous extent (bump allocator; old blocks leak on
        // overwrite — Phase-1 limit). Find an existing entry by name or a free slot.
        let idx = self
            .find(name)
            .or_else(|| self.entries.iter().position(|e| !e.used))
            .ok_or("entry table full")?;
        let first_block = self.next_free_block;

        // Write the data blocks (last block zero-padded).
        for i in 0..blocks as usize {
            let mut blk = [0u8; BLOCK];
            let start = i * BLOCK;
            let end = (start + BLOCK).min(data.len());
            blk[..end - start].copy_from_slice(&data[start..end]);
            if !block_write(ctx, first_block + i as u32, &blk) {
                return Err("block write failed");
            }
        }
        self.next_free_block += blocks;

        let was_used = self.entries[idx].used;
        let mut e = Entry::EMPTY;
        let nl = name.len();
        e.name[..nl].copy_from_slice(name);
        e.name_len = nl as u8;
        e.used = true;
        e.size = data.len() as u32;
        e.first_block = first_block;
        e.block_count = blocks;
        self.entries[idx] = e;
        if !was_used {
            self.file_count += 1;
        }
        self.persist_meta(ctx)
    }

    fn read_file(&self, ctx: &ServiceContext, name: &[u8], out: &mut [u8]) -> Option<usize> {
        let i = self.find(name)?;
        let e = self.entries[i];
        let n = e.size as usize;
        if n > out.len() {
            return None;
        }
        for b in 0..e.block_count as usize {
            let blk = block_read(ctx, e.first_block + b as u32)?;
            let start = b * BLOCK;
            let end = (start + BLOCK).min(n);
            if start >= n {
                break;
            }
            out[start..end].copy_from_slice(&blk[..end - start]);
        }
        Some(n)
    }

    /// Persist the entry table + superblock back to disk (write-through, no journal).
    fn persist_meta(&self, ctx: &ServiceContext) -> Result<(), &'static str> {
        let mut et = [0u8; BLOCK];
        for (i, e) in self.entries.iter().enumerate() {
            let o = i * 32;
            et[o..o + NAME_MAX].copy_from_slice(&e.name);
            et[o + 16] = e.name_len;
            et[o + 17] = e.used as u8;
            et[o + 20..o + 24].copy_from_slice(&e.size.to_le_bytes());
            et[o + 24..o + 28].copy_from_slice(&e.first_block.to_le_bytes());
            et[o + 28..o + 32].copy_from_slice(&e.block_count.to_le_bytes());
        }
        if !block_write(ctx, self.entry_table_start, &et) {
            return Err("entry-table write failed");
        }
        // Re-read the superblock, patch the mutable fields, write it back.
        let mut sb = block_read(ctx, 0).ok_or("superblock read failed")?;
        sb[32..36].copy_from_slice(&self.next_free_block.to_le_bytes());
        sb[36..40].copy_from_slice(&self.file_count.to_le_bytes());
        if !block_write(ctx, 0, &sb) {
            return Err("superblock write failed");
        }
        Ok(())
    }
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Read one 512-byte block at `lba` from `block-driver` over IPC.
fn block_read(ctx: &ServiceContext, lba: u32) -> Option<[u8; BLOCK]> {
    let mut req = [0u8; 5];
    req[0] = OP_READ_BLOCK;
    req[1..5].copy_from_slice(&lba.to_le_bytes());
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

/// Write one 512-byte block at `lba` to `block-driver` over IPC.
fn block_write(ctx: &ServiceContext, lba: u32, data: &[u8; BLOCK]) -> bool {
    let mut req = [0u8; 5 + BLOCK];
    req[0] = OP_WRITE_BLOCK;
    req[1..5].copy_from_slice(&lba.to_le_bytes());
    req[5..].copy_from_slice(data);
    match ctx.request_with_reply("block-driver", &Message::from_bytes(&req)) {
        Some(reply) => reply.payload_bytes().first() == Some(&BLK_OK),
        None => false,
    }
}
