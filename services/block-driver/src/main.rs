//! `block-driver` — trusted block device driver. TCB member in v1 (§6.1).
//!
//! Owns the `hw_mmio` and `hw_interrupt` capabilities for the storage device.
//! Exposes a read/write block interface to `fs` via IPC.
//!
//! Non-restartable in v1: `fs` depends on it and a restart would lose disk
//! state. v2 goal: make block-driver restartable with transactional recovery (§6.3).
//!
//! v1 target: QEMU virtio-blk (MMIO variant, single queue).

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, Message};

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    let mmio = ctx.capability("hw_mmio").expect("block-driver: missing hw_mmio cap");
    let irq  = ctx.capability("hw_interrupt").expect("block-driver: missing hw_interrupt cap");

    init_virtio_blk(&mmio);
    ctx.log("block-driver: ready");

    loop {
        // Wait for either a block request from fs or an interrupt from the device.
        todo!("recv from either fs endpoint or interrupt endpoint; dispatch accordingly")
    }
}

fn init_virtio_blk(mmio: &godspeed_sdk::CapHandle) {
    todo!("negotiate virtio features, set up virtqueue, enable interrupts")
}

fn handle_read_request(lba: u64, buf_cap: godspeed_sdk::CapHandle) {
    todo!("submit virtio read descriptor, wait for completion, return data via IPC reply")
}

fn handle_write_request(lba: u64, data: &[u8]) {
    todo!("submit virtio write descriptor, wait for completion, reply with Ok")
}
