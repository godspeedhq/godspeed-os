//! `fs` — filesystem service. TCB member in v1 (§6.1, §15).
//!
//! Owns persistent state for the system. Depends on `block-driver` for I/O.
//! Cannot persist to itself; metadata is stored via block-driver directly (§15).
//!
//! Non-restartable in v1. v2 goal: transactional metadata recovery (§6.3).
//!
//! v1 scope: read/write files by path; no directories beyond a flat namespace.
//! Serves `supervisor` (reads service binaries) and any other service that
//! holds an `ipc_send = ["fs"]` capability.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let block = ctx.capability("ipc_send.block-driver")
        .expect("fs: missing block-driver cap");

    mount(&ctx, &block);
    ctx.log("fs: ready");

    loop {
        let msg = ctx.recv();
        handle_request(&ctx, msg, &block);
    }
}

fn mount(ctx: &ServiceContext, block: &godspeed_sdk::CapHandle) {
    todo!("read superblock from block 0; validate magic; build in-memory inode table")
}

fn handle_request(ctx: &ServiceContext, msg: Message, block: &godspeed_sdk::CapHandle) {
    todo!("decode FsRequest opcode; dispatch to read_file / write_file / stat; reply via IPC")
}
