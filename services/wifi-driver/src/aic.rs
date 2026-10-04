// SPDX-License-Identifier: GPL-2.0-only
//! The AICSemi AIC8800D80's SDIO message bus, the first exchange of phase V2 (`docs/wifi-aic8800.md` 5):
//! function 1 set up and woken, one debug memory read sent to the chip's ROM, and its confirm read back.
//! The word it returns carries the chip revision, which decides the firmware set the upload will need.
//!
//! Every register, value and byte offset below is the vendor driver's, read from `radxa-pkg/aic8800` at
//! `09c65d61` (the commit `nonfree/aic8800d80/PROVENANCE` pins), `aic8800_bsp/`, as an executable
//! datasheet (26.14): `aicwf_sdiov3_func_init` and `aicwf_sdio_bus_start` for the setup,
//! `aicwf_sdio_wakeup` for the wake, `rwnx_set_cmd_tx` and `aicwf_sdio_tx_msg` for the frame,
//! `aicwf_sdio_flow_ctrl` for the buffer check, `aicwf_sdio_hal_irqhandler` and `aicwf_process_rxframes`
//! for the receive, `aicbsp_driver_fw_init` for the revision. What is ours is the model: the vendor driver
//! reads from its SDIO interrupt handler; this one POLLS the same status register, bounded, because this
//! host takes no interrupts (`dwmmc.rs`). That substitution is the one thing here the reference does not
//! show working, and the first card is what settles it.

use godspeed::driver::delay;
use godspeed::driver::wait::{self, Budget};
use godspeed_sdk::ServiceContext;
use godspeed_wifi::sdio::{self as sd, SdioHost};

/// Function 1 is the whole bus on the D80: the vendor driver assigns no message function for this part.
const F1: u8 = 1;
/// The block size the vendor driver gives function 1, and the unit its transfers are padded to.
const BLOCK: u32 = 512;

// Function 1's registers, the D80's "V3" map (`aicsdio.h`).
const REG_INTR_ENABLE: u32 = 0x00;
const REG_PENDING: u32 = 0x01;
const REG_TO_DEVICE: u32 = 0x02;
const REG_FLOW_CTRL: u32 = 0x03;
const REG_MISC_INT_STATUS: u32 = 0x04;
const REG_BYTEMODE_LEN: u32 = 0x05;
const REG_BYTEMODE_ENABLE: u32 = 0x07;
const REG_RD_FIFO: u32 = 0x0F;
const REG_WR_FIFO: u32 = 0x10;

/// Written to `REG_TO_DEVICE` to wake the chip; `REG_PENDING` bit 4 says it is awake.
const WAKE: u8 = 0x11;
const AWAKE_BIT: u8 = 0x10;
/// `REG_MISC_INT_STATUS` bit 7: a device-to-host soft interrupt, acknowledged by clearing bit 0 of
/// `REG_PENDING`.
const OTHER_INT: u8 = 0x80;
/// `REG_MISC_INT_STATUS` reading exactly this means "byte mode: the length is in `REG_BYTEMODE_LEN`, in
/// words". Any other value is a block count in its low seven bits.
const STATUS_BYTE_MODE: u8 = 120;

/// One firmware buffer, `BUFFER_SIZE`: a frame is sent only when the flow-control register reports more
/// free buffers than it needs.
const FW_BUFFER: u32 = 1536;

/// The bus header's type byte for a host-to-chip command, and the receive side's for a command response.
const TYPE_CMD: u8 = 0x11;
const TYPE_CFG_CMD_RSP: u8 = 0x11;
/// Receive: type bit 4 set is a configuration packet (a message); clear is a data packet.
const TYPE_CFG: u8 = 0x10;
/// A data packet's hardware receive header, skipped to find the next packet.
const RX_HW_HDR: usize = 60;

/// The debug task's message ids (`LMAC_FIRST_MSG(TASK_DBG)` = 1 << 10), and the two task ids.
const DBG_MEM_READ_REQ: u16 = 0x0400;
const DBG_MEM_READ_CFM: u16 = 0x0401;
const TASK_DBG: u16 = 1;
const DRV_TASK_ID: u16 = 100;

/// The register whose value carries the chip revision, read first by `aicbsp_driver_fw_init`.
pub const CHIP_ID_ADDR: u32 = 0x4050_0000;

/// The largest reply this first exchange reads: four blocks. A memory-read confirm is a 24-byte packet,
/// so anything near this is a surprise worth reporting rather than a buffer to grow.
const RX_MAX_BLOCKS: usize = 4;

/// CRC-8 over the bus header's first three bytes: polynomial `0x07`, initial 0, as `crc8_ponl_107` writes
/// it. The D80 checks it; a wrong one is refused by the chip without a word.
fn crc8(bytes: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in bytes {
        let mut i: u8 = 0x80;
        while i > 0 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
            if b & i != 0 {
                crc ^= 0x07;
            }
            i >>= 1;
        }
    }
    crc
}

fn rd(h: &dyn SdioHost, reg: u32) -> Option<u8> {
    sd::read_reg(h, F1, reg)
}
fn wr(h: &dyn SdioHost, func: u8, reg: u32, v: u8) -> bool {
    sd::write_reg(h, func, reg, v).is_some()
}

/// Function 1 as the vendor driver leaves it before its first message: block size 512, enabled and ready,
/// CCCR `0xF2` = `0x7F`, block mode only, and the chip's interrupt sources enabled at both ends - which
/// this host never takes, but the chip may gate the status register's reporting on them, so they are
/// set exactly as the reference sets them.
fn setup(h: &dyn SdioHost, ctx: &ServiceContext) -> bool {
    if !sd::set_block_size(h, F1, BLOCK as u16, ctx) || !sd::enable_function(h, F1, ctx) {
        return false;
    }
    let steps: [(u8, u32, u8, &str); 4] = [
        (0, 0xF2, 0x7F, "CCCR 0xF2 = 0x7f"),
        (F1, REG_BYTEMODE_ENABLE, 0x01, "byte mode off (F1 0x07 = 1)"),
        (0, 0x04, 0x07, "CCCR interrupt enable = 0x07"),
        (F1, REG_INTR_ENABLE, 0x07, "F1 interrupt enable = 0x07"),
    ];
    for (func, reg, v, what) in steps {
        if !wr(h, func, reg, v) {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC setup - the write '{}' was refused (INT={:#010x})", what, h.last_int()));
            return false;
        }
    }
    ctx.log("wifi-driver: AIC function 1 set up as the vendor driver leaves it (block 512, 0xF2, block mode, interrupt enables)");
    true
}

/// `aicwf_sdio_wakeup`: up to 20 times, write `WAKE` and look up to 10 times, 200 us apart, for the awake
/// bit. The vendor driver goes on even when it never sees it; this one says which it was and goes on too,
/// since the memory read that follows is the real test.
fn wake(h: &dyn SdioHost, ctx: &ServiceContext) -> bool {
    for attempt in 1..=20u32 {
        if !wr(h, F1, REG_TO_DEVICE, WAKE) {
            ctx.log_fmt(format_args!("wifi-driver: AIC wake - the write to F1 0x02 was refused (INT={:#010x})", h.last_int()));
            return false;
        }
        for _ in 0..10 {
            if let Some(p) = rd(h, REG_PENDING) {
                if p & AWAKE_BIT != 0 {
                    ctx.log_fmt(format_args!("wifi-driver: AIC awake (F1 0x01 = {:#04x}) on wake attempt {}", p, attempt));
                    return true;
                }
            }
            delay::hold(ctx, Budget::us(200));
        }
        delay::hold(ctx, Budget::us(100));
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC never showed the awake bit in 20 wake attempts (F1 0x01 = {:#04x}); going on as the vendor driver does",
        rd(h, REG_PENDING).unwrap_or(0)));
    true
}

/// `aicwf_sdio_flow_ctrl`: the chip's free firmware buffers, from `REG_FLOW_CTRL`. Asked until nonzero,
/// over ~20 ms, the vendor driver's own schedule rounded to a duration.
fn free_buffers(h: &dyn SdioHost, ctx: &ServiceContext) -> u8 {
    let mut d = wait::Deadline::paced(ctx, Budget::ms(20), Budget::ms(1));
    loop {
        if let Some(n) = rd(h, REG_FLOW_CTRL) {
            if n != 0 {
                return n;
            }
        }
        if d.expired() {
            return 0;
        }
        d.pause();
    }
}

/// Build and send one host-to-chip message (`rwnx_set_cmd_tx` + `aicwf_sdio_tx_msg`): the 4-byte bus header
/// `[len lo, len hi (4 bits), 0x11, crc8]` where `len` counts what follows it, a zero word, the 8-byte
/// message header `{id, dest, src, param_len}`, the parameters, then zeros to a whole 512-byte block.
fn send_msg(h: &dyn SdioHost, id: u16, dest: u16, param: &[u8], ctx: &ServiceContext) -> bool {
    let msg_len = 8 + param.len();
    let len = msg_len + 4; // the dummy word, then the message
    let mut frame = [0u8; BLOCK as usize];
    if 4 + len + 4 > frame.len() {
        ctx.log("wifi-driver: AIC message too long for one block - not sent");
        return false;
    }
    frame[0] = (len & 0xff) as u8;
    frame[1] = ((len >> 8) & 0x0f) as u8;
    frame[2] = TYPE_CMD;
    frame[3] = crc8(&frame[0..3]);
    let m = 8; // after the header and the zero word
    frame[m..m + 2].copy_from_slice(&id.to_le_bytes());
    frame[m + 2..m + 4].copy_from_slice(&dest.to_le_bytes());
    frame[m + 4..m + 6].copy_from_slice(&DRV_TASK_ID.to_le_bytes());
    frame[m + 6..m + 8].copy_from_slice(&(param.len() as u16).to_le_bytes());
    frame[m + 8..m + 8 + param.len()].copy_from_slice(param);

    let n = free_buffers(h, ctx);
    if n == 0 || BLOCK >= n as u32 * FW_BUFFER {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC flow control reports {} free firmware buffer(s) after 20 ms - the message was not sent", n));
        return false;
    }
    let mut words = [0u32; BLOCK as usize / 4];
    for (i, w) in words.iter_mut().enumerate() {
        *w = u32::from_le_bytes([frame[4 * i], frame[4 * i + 1], frame[4 * i + 2], frame[4 * i + 3]]);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC sending message {:#06x} to task {} ({} parameter byte(s); header {:02x} {:02x} {:02x} {:02x}; {} free buffer(s))",
        id, dest, param.len(), frame[0], frame[1], frame[2], frame[3], n));
    sd::write_fifo(h, F1, REG_WR_FIFO, &mut words, BLOCK, ctx)
}

/// Poll for what the chip has to say, as `aicwf_sdio_hal_irqhandler` would on an interrupt: read the status
/// register; acknowledge a soft interrupt; and when it names a length, read that much from the read FIFO.
/// Returns the number of bytes read into `buf`, 0 when nothing came within `budget`.
fn receive(h: &dyn SdioHost, buf: &mut [u32; RX_MAX_BLOCKS * BLOCK as usize / 4], budget: Budget, ctx: &ServiceContext) -> usize {
    let mut d = wait::Deadline::paced(ctx, budget, Budget::ms(1));
    let mut looks = 0u32;
    loop {
        looks += 1;
        let st = rd(h, REG_MISC_INT_STATUS).unwrap_or(0);
        if st & OTHER_INT != 0 {
            if let Some(p) = rd(h, REG_PENDING) {
                let _ = wr(h, F1, REG_PENDING, p & !0x01);
            }
            ctx.log_fmt(format_args!("wifi-driver: AIC soft interrupt acknowledged (status {:#04x})", st));
        }
        let st = st & !OTHER_INT;
        if st != 0 {
            let bytes = if st == STATUS_BYTE_MODE {
                rd(h, REG_BYTEMODE_LEN).map_or(0, |w| w as usize * 4)
            } else if (st | 0x08) > 120 {
                // The vendor driver's "function 2" branch, which on this part has no function to read from.
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC status {:#04x} falls in the vendor driver's function-2 branch, which the D80 has no function for - not read", st));
                return 0;
            } else {
                (st & 0x7f) as usize * BLOCK as usize
            };
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC status {:#04x} after {} look(s) - {} byte(s) to read", st, looks, bytes));
            if bytes == 0 || bytes > buf.len() * 4 || bytes % 4 != 0 {
                ctx.log("wifi-driver: AIC reply length is not one this first exchange reads - left unread");
                return 0;
            }
            let words = &mut buf[..bytes / 4];
            return if sd::read_fifo(h, F1, REG_RD_FIFO, words, BLOCK, ctx) { bytes } else { 0 };
        }
        if d.expired() {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC said nothing in {} ms ({} looks at F1 0x04; F1 0x01 = {:#04x}, F1 0x03 = {:#04x})",
                budget.as_us() / 1000, looks, rd(h, REG_PENDING).unwrap_or(0), rd(h, REG_FLOW_CTRL).unwrap_or(0)));
            return 0;
        }
        d.pause();
    }
}

/// Find the confirm `want` among the packets in `bytes` (`aicwf_process_rxframes`): each packet starts with
/// a 16-bit length and a type byte; a configuration packet's message sits right after its 4-byte header as
/// `{id, dest, src, param_len, pattern, param...}`, and a data packet is skipped with its hardware header.
/// Returns the message's parameters.
fn find_cfm(bytes: &[u8], want: u16, ctx: &ServiceContext) -> Option<[u8; 8]> {
    let mut at = 0usize;
    let mut packets = 0u32;
    while at + 4 <= bytes.len() && packets < 16 {
        packets += 1;
        let plen = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
        let ty = bytes[at + 2];
        if plen == 0 {
            break;
        }
        if ty & TYPE_CFG != TYPE_CFG {
            ctx.log_fmt(format_args!("wifi-driver: AIC receive - a data packet of {} byte(s), skipped", plen));
            at += (plen + RX_HW_HDR + 3) & !3;
            continue;
        }
        let m = at + 4;
        if ty & 0x7f == TYPE_CFG_CMD_RSP && m + 12 <= bytes.len() {
            let id = u16::from_le_bytes([bytes[m], bytes[m + 1]]);
            let param_len = u16::from_le_bytes([bytes[m + 6], bytes[m + 7]]) as usize;
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC receive - message {:#06x} with {} parameter byte(s) (packet type {:#04x}, length {})",
                id, param_len, ty, plen));
            if id == want && param_len >= 8 && m + 12 + 8 <= bytes.len() {
                let mut p = [0u8; 8];
                p.copy_from_slice(&bytes[m + 12..m + 20]);
                return Some(p);
            }
        } else {
            ctx.log_fmt(format_args!("wifi-driver: AIC receive - a configuration packet of type {:#04x}, length {}", ty, plen));
        }
        at += ((plen + 3) & !3) + 4;
    }
    None
}

/// Read one 32-bit word of the chip's memory through its ROM: `DBG_MEM_READ_REQ` out, `DBG_MEM_READ_CFM`
/// back with `{memaddr, memdata}`. `None` when any step failed, which the lines before it name.
pub fn mem_read(h: &dyn SdioHost, addr: u32, ctx: &ServiceContext) -> Option<u32> {
    if !send_msg(h, DBG_MEM_READ_REQ, TASK_DBG, &addr.to_le_bytes(), ctx) {
        return None;
    }
    let mut buf = [0u32; RX_MAX_BLOCKS * BLOCK as usize / 4];
    let n = receive(h, &mut buf, Budget::ms(1_000), ctx);
    if n == 0 {
        return None;
    }
    let mut bytes = [0u8; RX_MAX_BLOCKS * BLOCK as usize];
    for (i, w) in buf[..n / 4].iter().enumerate() {
        bytes[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC receive - first 16 bytes {:02x?}", &bytes[..16.min(n)]));
    let p = find_cfm(&bytes[..n], DBG_MEM_READ_CFM, ctx)?;
    let got_addr = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let data = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    if got_addr != addr {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC memory read confirm names {:#010x}, not the {:#010x} asked for", got_addr, addr));
        return None;
    }
    Some(data)
}

/// Phase V2's first exchange: set up, wake, read the chip id. Returns the word at `CHIP_ID_ADDR`.
pub fn first_exchange(h: &dyn SdioHost, ctx: &ServiceContext) -> Option<u32> {
    if !setup(h, ctx) || !wake(h, ctx) {
        return None;
    }
    mem_read(h, CHIP_ID_ADDR, ctx)
}
