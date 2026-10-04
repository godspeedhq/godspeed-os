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

/// The largest frame the vendor driver sends, `CMD_BUF_MAX`: a block write's 1032 parameter bytes and its
/// headers, rounded to three 512-byte blocks.
const FRAME_MAX: usize = 1536;

/// Build and send one host-to-chip message (`rwnx_set_cmd_tx` + `aicwf_sdio_tx_msg`): the 4-byte bus header
/// `[len lo, len hi (4 bits), 0x11, crc8]` where `len` counts what follows it, a zero word, the 8-byte
/// message header `{id, dest, src, param_len}`, the parameters; then, if that is not a whole number of
/// 512-byte blocks, a 4-byte zero tail and zeros up to the next whole block. A frame of one block goes in
/// byte mode and a longer one in block mode (`sd::write_fifo`, the Linux core's choice). `quiet` drops the
/// per-message line, for the upload's hundreds of blocks.
fn send_msg(h: &dyn SdioHost, id: u16, dest: u16, param: &[u8], quiet: bool, ctx: &ServiceContext) -> bool {
    let len = 4 + 8 + param.len(); // the dummy word, the message header, the parameters
    let mut total = (4 + len + 3) & !3;
    if total % BLOCK as usize != 0 {
        total = (total + 4).div_ceil(BLOCK as usize) * BLOCK as usize;
    }
    if total > FRAME_MAX || len > 0xfff {
        ctx.log_fmt(format_args!("wifi-driver: AIC message {:#06x} is {} bytes, past the {}-byte frame - not sent", id, total, FRAME_MAX));
        return false;
    }
    let mut frame = [0u8; FRAME_MAX];
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

    // Sent only when the chip has MORE room than the frame: `len < buffer_cnt * BUFFER_SIZE`, strictly, so
    // a three-block frame needs two free buffers.
    let n = free_buffers(h, ctx);
    if n == 0 || total as u32 >= n as u32 * FW_BUFFER {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC flow control reports {} free firmware buffer(s) after 20 ms, {} needed for {} bytes - message {:#06x} was not sent",
            n, total / FW_BUFFER as usize + 1, total, id));
        return false;
    }
    if !quiet {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC sending message {:#06x} to task {} ({} parameter byte(s); header {:02x} {:02x} {:02x} {:02x}; {} free buffer(s))",
            id, dest, param.len(), frame[0], frame[1], frame[2], frame[3], n));
    }
    // The frame becomes words in place of a second buffer: the FIFO takes them little-endian.
    let mut words = [0u32; FRAME_MAX / 4];
    for (i, w) in words[..total / 4].iter_mut().enumerate() {
        *w = u32::from_le_bytes([frame[4 * i], frame[4 * i + 1], frame[4 * i + 2], frame[4 * i + 3]]);
    }
    sd::write_fifo(h, F1, REG_WR_FIFO, &mut words[..total / 4], BLOCK, ctx)
}

/// Poll for what the chip has to say, as `aicwf_sdio_hal_irqhandler` would on an interrupt: read the status
/// register; acknowledge a soft interrupt; and when it names a length, read that much from the read FIFO.
/// Returns the number of bytes read into `buf`, 0 when nothing came within `budget`.
fn receive(h: &dyn SdioHost, buf: &mut [u32; RX_MAX_BLOCKS * BLOCK as usize / 4], budget: Budget, quiet: bool, ctx: &ServiceContext) -> usize {
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
            if !quiet {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC status {:#04x} after {} look(s) - {} byte(s) to read", st, looks, bytes));
            }
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
/// Returns the confirm's parameter length and its first eight parameter bytes (zero past its length).
fn find_cfm(bytes: &[u8], want: u16, quiet: bool, ctx: &ServiceContext) -> Option<(usize, [u8; 8])> {
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
            if !quiet || id != want {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC receive - message {:#06x} with {} parameter byte(s) (packet type {:#04x}, length {})",
                    id, param_len, ty, plen));
            }
            if id == want {
                let mut p = [0u8; 8];
                let n = param_len.min(8).min(bytes.len().saturating_sub(m + 12));
                p[..n].copy_from_slice(&bytes[m + 12..m + 12 + n]);
                return Some((param_len, p));
            }
        } else {
            ctx.log_fmt(format_args!("wifi-driver: AIC receive - a configuration packet of type {:#04x}, length {}", ty, plen));
        }
        at += ((plen + 3) & !3) + 4;
    }
    if !quiet {
        ctx.log_fmt(format_args!("wifi-driver: AIC receive - no {:#06x} confirm among {} packet(s)", want, packets));
    }
    None
}

/// One request to the chip's ROM and its confirm: send `id` with `param`, poll for the reply, find `cfm`.
/// Returns the confirm's parameter length and first eight bytes. `quiet` keeps a successful exchange off
/// the log; a failure always says which step it was.
fn request(h: &dyn SdioHost, id: u16, param: &[u8], cfm: u16, quiet: bool, ctx: &ServiceContext) -> Option<(usize, [u8; 8])> {
    if !send_msg(h, id, TASK_DBG, param, quiet, ctx) {
        return None;
    }
    let mut buf = [0u32; RX_MAX_BLOCKS * BLOCK as usize / 4];
    let n = receive(h, &mut buf, Budget::ms(1_000), quiet, ctx);
    if n == 0 {
        if quiet {
            ctx.log_fmt(format_args!("wifi-driver: AIC request {:#06x} got no reply", id));
        }
        return None;
    }
    let mut bytes = [0u8; RX_MAX_BLOCKS * BLOCK as usize];
    for (i, w) in buf[..n / 4].iter().enumerate() {
        bytes[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    if !quiet {
        ctx.log_fmt(format_args!("wifi-driver: AIC receive - first 16 bytes {:02x?}", &bytes[..16.min(n)]));
    }
    let r = find_cfm(&bytes[..n], cfm, quiet, ctx);
    if r.is_none() && quiet {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC request {:#06x} - the reply held no {:#06x} confirm (first 16 bytes {:02x?})",
            id, cfm, &bytes[..16.min(n)]));
    }
    r
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Read one 32-bit word of the chip's memory through its ROM: `DBG_MEM_READ_REQ` out, `DBG_MEM_READ_CFM`
/// back with `{memaddr, memdata}`. `None` when any step failed, which the lines before it name.
pub fn mem_read(h: &dyn SdioHost, addr: u32, quiet: bool, ctx: &ServiceContext) -> Option<u32> {
    let (len, p) = request(h, DBG_MEM_READ_REQ, &addr.to_le_bytes(), DBG_MEM_READ_CFM, quiet, ctx)?;
    let got_addr = le32(&p, 0);
    if len < 8 || got_addr != addr {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC memory read confirm names {:#010x} with {} parameter byte(s), not {:#010x} with 8",
            got_addr, len, addr));
        return None;
    }
    Some(le32(&p, 4))
}

/// Phase V2's first exchange: set up, wake, read the chip id. Returns the word at `CHIP_ID_ADDR`.
pub fn first_exchange(h: &dyn SdioHost, ctx: &ServiceContext) -> Option<u32> {
    if !setup(h, ctx) || !wake(h, ctx) {
        return None;
    }
    mem_read(h, CHIP_ID_ADDR, false, ctx)
}

// ------------------------------------------------------------------------------ the upload (V2)

const DBG_MEM_BLOCK_WRITE_REQ: u16 = 0x040B;
const DBG_MEM_BLOCK_WRITE_CFM: u16 = 0x040C;
/// A block write's data field: always sent whole, the real size in `memsize` (`dbg_mem_block_write_req`).
const BLOCK_WRITE_DATA: usize = 1024;

/// Where the patch table says the patches go: its first group, `AICBT_PINF_T` (type 0), read as
/// `aicbt_patch_info_unpack` reads it - the ADID's address in its first pair's value, the ROM patch's in
/// the second's, the extension patch count in the fifth's, then `(id, address)` per extension patch.
pub struct PatchInfo {
    pub adid: u32,
    pub patch: u32,
    pub ext0: u32,
}

/// The table's groups as `(name, type, pairs)`, walked as `aicbt_patch_table_alloc` walks them: a 16-byte
/// file tag, then `name[16], type u32, len u32, len * (addr u32, value u32)` to the end of the file.
pub struct Groups<'t> {
    t: &'t [u8],
    at: usize,
}

impl<'t> Groups<'t> {
    pub fn new(t: &'t [u8]) -> Option<Self> {
        if t.len() < 16 || &t[..12] != b"AICBT_PT_TAG" {
            return None;
        }
        Some(Groups { t, at: 16 })
    }
}

impl<'t> Iterator for Groups<'t> {
    /// `(name, type, the pairs' bytes)`.
    type Item = (&'t [u8], u32, &'t [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        if self.at + 24 > self.t.len() {
            return None;
        }
        let name = &self.t[self.at..self.at + 16];
        let ty = le32(self.t, self.at + 16);
        let len = le32(self.t, self.at + 20) as usize;
        let start = self.at + 24;
        let end = start.checked_add(len.checked_mul(8)?)?;
        if end > self.t.len() {
            return None;
        }
        self.at = end;
        let name_len = name.iter().position(|&b| b == 0).unwrap_or(16);
        Some((&name[..name_len], ty, &self.t[start..end]))
    }
}

/// Read the load addresses out of the table's first group, refusing a table that does not have the shape
/// this file's has: a type-0 group first, at least six pairs, exactly one extension patch, id 0.
pub fn patch_info(table: &[u8], ctx: &ServiceContext) -> Option<PatchInfo> {
    let Some(mut g) = Groups::new(table) else {
        ctx.log("wifi-driver: AIC patch table does not start with its AICBT_PT_TAG - not used");
        return None;
    };
    let Some((name, ty, d)) = g.next() else {
        ctx.log("wifi-driver: AIC patch table has no first group");
        return None;
    };
    let w = |i: usize| le32(d, 4 * i);
    if ty != 0 || d.len() < 6 * 8 || w(9) != 1 || w(10) != 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC patch table's first group is {:?} type {} with {} pair(s) - not the information group this driver reads",
            core::str::from_utf8(name).unwrap_or("?"), ty, d.len() / 8));
        return None;
    }
    let info = PatchInfo { adid: w(1), patch: w(3), ext0: w(11) };
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC patch table - ADID at {:#010x}, patch at {:#010x}, extension patch 0 at {:#010x}",
        info.adid, info.patch, info.ext0));
    Some(info)
}

/// Write `bytes` into the chip's memory at `addr` in 1 KiB block writes, each confirmed
/// (`rwnx_plat_bin_fw_upload_android`): every chunk 1024 bytes but the last, which carries its real size.
pub fn upload(h: &dyn SdioHost, what: &str, addr: u32, bytes: &[u8], ctx: &ServiceContext) -> bool {
    let mut param = [0u8; 8 + BLOCK_WRITE_DATA];
    let mut off = 0usize;
    let mut blocks = 0u32;
    let d = wait::Deadline::start(ctx, Budget::ms(600_000));
    while off < bytes.len() {
        let n = (bytes.len() - off).min(BLOCK_WRITE_DATA);
        param[..4].copy_from_slice(&(addr + off as u32).to_le_bytes());
        param[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        param[8..8 + n].copy_from_slice(&bytes[off..off + n]);
        param[8 + n..].fill(0);
        match request(h, DBG_MEM_BLOCK_WRITE_REQ, &param, DBG_MEM_BLOCK_WRITE_CFM, true, ctx) {
            Some((_, p)) if le32(&p, 0) == 0 => {}
            Some((len, p)) => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC upload of {} - block {} at {:#010x} confirmed with status {:#010x} ({} parameter bytes), not 0; stopped",
                    what, blocks, addr + off as u32, le32(&p, 0), len));
                return false;
            }
            None => {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC upload of {} stopped at block {} ({:#010x}), {} of {} bytes sent",
                    what, blocks, addr + off as u32, off, bytes.len()));
                return false;
            }
        }
        off += n;
        blocks += 1;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC uploaded {} - {} bytes to {:#010x} in {} block write(s), every one confirmed, {} ms",
        what, bytes.len(), addr, blocks, d.elapsed_us() / 1000));
    true
}

/// Read the first word back from where a file went and compare it with the file's. A confirm says the ROM
/// took the write; this says the memory holds it.
pub fn check_first_word(h: &dyn SdioHost, what: &str, addr: u32, bytes: &[u8], ctx: &ServiceContext) -> bool {
    let want = le32(bytes, 0);
    match mem_read(h, addr, true, ctx) {
        Some(got) if got == want => {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC {} reads back {:#010x} at {:#010x} - the file's first word", what, got, addr));
            true
        }
        Some(got) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC {} reads back {:#010x} at {:#010x}, the file says {:#010x}", what, got, addr, want));
            false
        }
        None => false,
    }
}

const DBG_MEM_WRITE_REQ: u16 = 0x0402;
const DBG_MEM_WRITE_CFM: u16 = 0x0403;
const DBG_START_APP_REQ: u16 = 0x040D;
const DBG_START_APP_CFM: u16 = 0x040E;

/// Write one 32-bit word of the chip's memory through its ROM: `DBG_MEM_WRITE_REQ` `{memaddr, memdata}`,
/// confirmed by `DBG_MEM_WRITE_CFM` naming the same address.
pub fn mem_write(h: &dyn SdioHost, addr: u32, val: u32, ctx: &ServiceContext) -> bool {
    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&addr.to_le_bytes());
    p[4..].copy_from_slice(&val.to_le_bytes());
    match request(h, DBG_MEM_WRITE_REQ, &p, DBG_MEM_WRITE_CFM, true, ctx) {
        Some((_, c)) if le32(&c, 0) == addr => true,
        Some((len, c)) => {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC memory write of {:#010x} to {:#010x} confirmed for {:#010x} ({} parameter bytes)",
                val, addr, le32(&c, 0), len));
            false
        }
        None => {
            ctx.log_fmt(format_args!("wifi-driver: AIC memory write of {:#010x} to {:#010x} failed", val, addr));
            false
        }
    }
}

/// Group types in the patch table (`aicbt_patch_table_alloc`): the information group, the trap and patch
/// tables, the Bluetooth mode block, the power-on writes, a second patch table, and version text.
const GROUP_BTMODE: u32 = 3;
const GROUP_POWER_ON: u32 = 4;
const GROUP_VERSION: u32 = 6;

/// The values the vendor driver puts in the Bluetooth mode group before writing it, slot by slot, for the
/// D80 with this build's defaults (`aicbt_patch_table_load`): no hardware info (so "none" and `-1`), no
/// second chip flag, Bluetooth-only co-antenna mode 5, the UART port 2, 1.5 Mbaud, flow control on, low
/// power off, and the vendor's fixed final word. The radio's Bluetooth is not driven here; the group is
/// written because the ROM patches read it.
const BTMODE_VALUES: [u32; 9] = [1, 0xFFFF_FFFF, 0, 5, 2, 1_500_000, 1, 0, 0x6f2f];

/// Every group of the patch table but the version text, as `(address, value)` memory writes, in file
/// order, with the Bluetooth mode values replaced and a 500 us pause after the power-on group - the
/// INFORMATION group included, as `aicbt_patch_table_load` writes it (its last two pairs land at
/// addresses 1 and 0; the reference does it and the chip accepts it, so this does too).
pub fn table_writes(h: &dyn SdioHost, table: &[u8], ctx: &ServiceContext) -> bool {
    let Some(groups) = Groups::new(table) else { return false };
    let mut total = 0u32;
    let d = wait::Deadline::start(ctx, Budget::ms(600_000));
    for (name, ty, pairs) in groups {
        if ty == GROUP_VERSION {
            continue;
        }
        let n = pairs.len() / 8;
        if ty == GROUP_BTMODE && n != BTMODE_VALUES.len() {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC patch table's Bluetooth mode group has {} pairs, not the {} this driver fills - stopped",
                n, BTMODE_VALUES.len()));
            return false;
        }
        for i in 0..n {
            let addr = le32(pairs, 8 * i);
            let val = if ty == GROUP_BTMODE { BTMODE_VALUES[i] } else { le32(pairs, 8 * i + 4) };
            if !mem_write(h, addr, val, ctx) {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC patch table write {} of group {:?} failed - stopped",
                    i, core::str::from_utf8(name).unwrap_or("?")));
                return false;
            }
            total += 1;
        }
        if ty == GROUP_POWER_ON {
            delay::hold(ctx, Budget::us(500));
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC patch table written - {} memory writes, every one confirmed, {} ms", total, d.elapsed_us() / 1000));
    true
}

/// Where `fmacfw` goes and starts (`RAM_FMAC_FW_ADDR`).
pub const FMAC_ADDR: u32 = 0x0012_0000;

/// `aicwifi_patch_config_8800d80`: read three pointers and the version out of the uploaded `fmacfw`, then
/// write its patch header and three `(offset, value)` pairs where the image says. The pointers are READ
/// from the chip, not taken from the file, as the reference does; the file's values are logged beside
/// them so a difference shows.
pub fn patch_config(h: &dyn SdioHost, fmac: &[u8], ctx: &ServiceContext) -> bool {
    let rd = |addr: u32| mem_read(h, addr, true, ctx);
    let file = |addr: u32| le32(fmac, (addr - FMAC_ADDR) as usize);
    let (Some(config_base), Some(patch_str), Some(version)) =
        (rd(FMAC_ADDR + 0x198), rd(FMAC_ADDR + 0x1A0), rd(FMAC_ADDR + 0x1C))
    else {
        ctx.log("wifi-driver: AIC patch configuration - a pointer read from fmacfw failed");
        return false;
    };
    let start = if version > 0x0609_0100 {
        match rd(FMAC_ADDR + 0x1A4) {
            Some(v) => v,
            None => return false,
        }
    } else {
        0x0016_F800
    };
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC fmacfw version {:#010x}, config base {:#010x}, patch header {:#010x}, pairs at {:#010x} (the file says {:#010x} {:#010x} {:#010x} {:#010x})",
        version, config_base, patch_str, start,
        file(FMAC_ADDR + 0x1C), file(FMAC_ADDR + 0x198), file(FMAC_ADDR + 0x1A0), file(FMAC_ADDR + 0x1A4)));
    const PAIRS: [(u32, u32); 3] = [(0x00b4, 0xf301_0000), (0x0170, 0x0100_000a), (0x0188, 0x0000_0003)];
    let mut writes: [(u32, u32); 4 + 2 * 3 + 4] = [(0, 0); 14];
    writes[0] = (patch_str, 0x4843_5450); // "PTCH"
    writes[1] = (patch_str + 8, 0x5054_4348);
    writes[2] = (patch_str + 4, start);
    writes[3] = (patch_str + 0xC, PAIRS.len() as u32);
    for (n, (off, val)) in PAIRS.iter().enumerate() {
        writes[4 + 2 * n] = (start + 8 * n as u32, off + config_base);
        writes[5 + 2 * n] = (start + 8 * n as u32 + 4, *val);
    }
    for k in 0..4u32 {
        writes[10 + k as usize] = (patch_str + 0x30 + 4 * k, 0);
    }
    for (addr, val) in writes {
        if !mem_write(h, addr, val, ctx) {
            return false;
        }
    }
    ctx.log("wifi-driver: AIC fmacfw patch configuration written (PTCH header, 3 pairs, block sizes cleared)");
    true
}

/// `aicwifi_start_from_bootrom`: `DBG_START_APP_REQ {bootaddr, boottype 1 (auto)}`, and its confirm's
/// boot status. Then `F1 0x02 = 4`, which the vendor driver writes once the firmware is started.
pub fn start_app(h: &dyn SdioHost, ctx: &ServiceContext) -> Option<u32> {
    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&FMAC_ADDR.to_le_bytes());
    p[4..].copy_from_slice(&1u32.to_le_bytes());
    let (len, c) = request(h, DBG_START_APP_REQ, &p, DBG_START_APP_CFM, false, ctx)?;
    let status = le32(&c, 0);
    let handed = wr(h, F1, REG_TO_DEVICE, 4);
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC start confirmed - boot status {:#010x} ({} parameter bytes); F1 0x02 = 4 {}",
        status, len, if handed { "written" } else { "REFUSED" }));
    Some(status)
}
