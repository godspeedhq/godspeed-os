//! `block-driver` — userspace **AHCI (SATA)** disk driver (persistence, v2; §6.3,
//! docs/ahci.md, docs/persistence.md).
//!
//! An MMIO + DMA driver: the kernel maps the AHCI HBA's ABAR and grants a
//! physically-contiguous DMA arena at spawn (the same path as the USB drivers).
//! It IDENTIFYs the disk, runs a boot self-test, then serves block read/write to
//! `fs` over IPC.
//!
//! (ATA PIO + the `hw_pio` capability were the bring-up backend; retired once AHCI
//! proved out — the T630's SSD is AHCI-only, so AHCI is the production path.)

#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

mod ahci;

// Block IPC protocol (fs <-> block-driver). MUST match `services/fs`.
//   Request : [op:u8, lba:u64 LE, (WriteBlock only: 512 data bytes)]
//   Reply   : [status:u8, (ReadBlock only: 512 data bytes)]
// The LBA is u64 (persistence §6.3): GSFS capacity fields are u64, so the block
// address reaches the device at full width.
const OP_READ_BLOCK: u8 = 1;
const OP_WRITE_BLOCK: u8 = 2;
// Capacity request: [OP_CAPACITY] → reply [STATUS_OK, sectors:u64 LE]. Lets `fs`
// size a freshly-flashed filesystem to the real disk (drives §7, persistence §6.3).
const OP_CAPACITY: u8 = 3;
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    match ctx.mmio() {
        Some(hba) => ahci::run(&ctx, &hba),
        None => {
            ctx.log("block-driver: no AHCI controller found (no SATA disk?)");
            loop {
                ctx.yield_cpu();
            }
        }
    }
}
