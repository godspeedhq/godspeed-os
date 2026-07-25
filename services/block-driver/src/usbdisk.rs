// SPDX-License-Identifier: GPL-2.0-only
//! `block-driver` ARM backend: a USB mass-storage stick, served through the in-kernel DWC2 stack.
//!
//! **Why this backend exists.** The Pi has exactly one SD slot and the machine boots from it, so
//! formatting that card destroys the boot medium (`fs` refuses to, see `foreign_disk`). A USB stick is
//! the storage that can actually be given to GSFS on this board: boot from SD, store on USB.
//!
//! **Why it goes through the kernel.** ARM does not route device IRQs to userspace yet, so the whole USB
//! stack - controller, enumeration, Bulk-Only transport - lives in the kernel (`arch/arm/dwc2.rs`,
//! `arch/arm/CLAUDE.md`). This driver therefore does not touch hardware at all: it holds the `USB_DISK`
//! capability and moves blocks with three gated syscalls, exactly as the ARM `nic-driver` bridges USB
//! ethernet frames to `net-stack`. The block protocol above them - the same one `fs` speaks to the AHCI
//! and EMMC backends - is this driver's own, so `fs` cannot tell which disk it is talking to.
//!
//! **Core affinity.** The kernel serves these syscalls only from core 0: the single DWC2 host channel and
//! its DMA buffer are shared with the keyboard poll, which runs in core 0's timer ISR, and they are kept
//! mutually exclusive by the fact that an ARM syscall runs with interrupts masked (the soundness
//! invariant on `dwc2::DMA`). A request from another core would be refused, so the supervisor spawns this
//! driver on core 0 when it serves a USB disk.

use godspeed_sdk::{Message, ServiceContext};

/// Serve one block-IPC request. Same wire protocol as the AHCI and EMMC backends - `fs` is unaware of
/// which one it is talking to.
fn serve(sectors: u64, ctx: &ServiceContext, p: &[u8], reply: godspeed_sdk::CapHandle) {
    use super::{OP_CAPACITY, OP_FLUSH, OP_READ_BLOCK, OP_WRITE_BLOCK, OP_WRITE_ZEROS, STATUS_ERR, STATUS_OK};
    let err = |ctx: &ServiceContext| { let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[STATUS_ERR])); };
    if p.is_empty() { return err(ctx); }
    if p[0] == OP_FLUSH {
        // The one backend that genuinely needs this: a stick acknowledges a WRITE(10) into its own
        // buffer, so without SYNCHRONIZE CACHE a reset loses the tail of everything just written.
        let status = if ctx.usb_disk_flush() { STATUS_OK } else { STATUS_ERR };
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        return;
    }
    if p[0] == OP_CAPACITY {
        let mut out = [0u8; 9];
        out[0] = STATUS_OK;
        out[1..9].copy_from_slice(&sectors.to_le_bytes());
        let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
        return;
    }
    if p.len() < 9 { return err(ctx); }
    let lba = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
    match p[0] {
        OP_READ_BLOCK => {
            let mut buf = [0u8; 512];
            if ctx.usb_disk_read(lba, &mut buf) {
                let mut out = [0u8; 513];
                out[0] = STATUS_OK;
                out[1..].copy_from_slice(&buf);
                let _ = ctx.send_by_handle(reply, &Message::from_bytes(&out));
            } else { err(ctx); }
        }
        OP_WRITE_BLOCK => {
            if p.len() < 521 { return err(ctx); }
            let mut buf = [0u8; 512];
            buf.copy_from_slice(&p[9..521]);
            let status = if ctx.usb_disk_write(lba, &buf) { STATUS_OK } else { STATUS_ERR };
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[status]));
        }
        OP_WRITE_ZEROS => {
            if p.len() < 17 { return err(ctx); }
            let count = u64::from_le_bytes([p[9], p[10], p[11], p[12], p[13], p[14], p[15], p[16]]);
            let zero = [0u8; 512];
            let mut ok = true;
            for i in 0..count {
                if !ctx.usb_disk_write(lba + i, &zero) { ok = false; break; }
            }
            let _ = ctx.send_by_handle(reply, &Message::from_bytes(&[if ok { STATUS_OK } else { STATUS_ERR }]));
        }
        _ => err(ctx),
    }
}

/// Serve block I/O from the USB mass-storage device. The caller has already confirmed one is attached.
pub fn run(ctx: &ServiceContext, sectors: u64) -> ! {
    ctx.log_fmt(format_args!("block-driver: USB mass storage serving block I/O ({} sectors = {} MiB)",
                             sectors, sectors / 2048));
    loop {
        let msg = ctx.recv();
        let reply = match ctx.take_pending_cap() { Some(c) => c, None => continue };
        serve(sectors, ctx, msg.payload_bytes(), reply);
        ctx.remove_cap(reply);
    }
}
