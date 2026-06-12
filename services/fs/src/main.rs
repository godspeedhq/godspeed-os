//! `fs` — userspace filesystem service (persistence, v2; §15, docs/persistence.md).
//!
//! **Phase 1 (this file): mount.** Read the superblock (LBA 0) from `block-driver`
//! over IPC, validate its magic (loud failure on mismatch, §3.12), and report the
//! geometry. The named-blob read/write API (`ReadFile`/`WriteFile`/`StatFile`) and
//! the entry table come in later phases.
//!
//! All disk I/O goes through `block-driver` (this service touches no hardware): a
//! synchronous request/reply over IPC (`ctx.request_with_reply`), which embeds a
//! per-request reply cap so block-driver can answer (§8, the registry pattern).

#![no_std]
#![no_main]

use godspeed_sdk::{Message, ServiceContext};

// On-disk superblock (LBA 0). MUST match `osdev mkfs` (docs/persistence.md §6).
//   0  magic[8] = "GSPDFS01"
//   8  version       u32 LE
//   12 block_size    u32 LE
//   16 total_blocks  u32 LE
const SB_MAGIC: &[u8; 8] = b"GSPDFS01";

// Block IPC protocol (fs <-> block-driver). MUST match `services/block-driver`.
const OP_READ_BLOCK: u8 = 1;
const STATUS_OK: u8 = 0;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.log("fs: starting");

    match mount(&ctx) {
        Ok((version, block_size, total_blocks)) => ctx.log_fmt(format_args!(
            "fs: mounted (GSPDFS v{}, block_size={}, total_blocks={})",
            version, block_size, total_blocks
        )),
        Err(e) => ctx.log_fmt(format_args!("fs: mount FAILED: {}", e)),
    }

    // Phase 1 stops at mount; the fs request API is a later phase.
    loop {
        ctx.yield_cpu();
    }
}

/// Read + validate the superblock. Returns (version, block_size, total_blocks).
fn mount(ctx: &ServiceContext) -> Result<(u32, u32, u32), &'static str> {
    let sb = read_block(ctx, 0).ok_or("block 0 read failed (block-driver unreachable?)")?;
    if &sb[0..8] != SB_MAGIC {
        return Err("bad superblock magic — disk not formatted (run osdev mkfs)");
    }
    let version = u32::from_le_bytes([sb[8], sb[9], sb[10], sb[11]]);
    let block_size = u32::from_le_bytes([sb[12], sb[13], sb[14], sb[15]]);
    let total_blocks = u32::from_le_bytes([sb[16], sb[17], sb[18], sb[19]]);
    Ok((version, block_size, total_blocks))
}

/// Read one 512-byte block at `lba` from `block-driver` over IPC.
fn read_block(ctx: &ServiceContext, lba: u32) -> Option<[u8; 512]> {
    let mut req = [0u8; 5];
    req[0] = OP_READ_BLOCK;
    req[1..5].copy_from_slice(&lba.to_le_bytes());
    let reply = ctx.request_with_reply("block-driver", &Message::from_bytes(&req))?;
    let p = reply.payload_bytes();
    if p.first() == Some(&STATUS_OK) && p.len() >= 1 + 512 {
        let mut out = [0u8; 512];
        out.copy_from_slice(&p[1..1 + 512]);
        Some(out)
    } else {
        None
    }
}
