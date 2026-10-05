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

use crate::aic_wire::{
    add_if, build_frame, chan_config, channel_of, parse_result, scanu_start, le32, mm_start, rf_calib, txpwr_lvl_v3, Groups, BLOCK, CHAN_TX_POWER_DBM, COEX,
    FRAME_MAX, ME_CONFIG,
};

/// `BLOCK` as the SDIO layer takes it.
const BLOCK_U32: u32 = BLOCK as u32;

/// Function 1 is the whole bus on the D80: the vendor driver assigns no message function for this part.
const F1: u8 = 1;

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
const TYPE_CFG_CMD_RSP: u8 = 0x11;
/// Receive: type bit 4 set is a configuration packet (a message); clear is a data packet.
const TYPE_CFG: u8 = 0x10;
/// A data packet's hardware receive header, skipped to find the next packet.
const RX_HW_HDR: usize = 60;

/// The debug task's message ids (`LMAC_FIRST_MSG(TASK_DBG)` = 1 << 10), and the two task ids.
const DBG_MEM_READ_REQ: u16 = 0x0400;
const DBG_MEM_READ_CFM: u16 = 0x0401;
const TASK_DBG: u16 = 1;

/// The register whose value carries the chip revision, read first by `aicbsp_driver_fw_init`.
pub const CHIP_ID_ADDR: u32 = 0x4050_0000;

/// The largest reply read at once: eight blocks. A confirm is a few dozen bytes; the room is for the
/// running firmware's print packets arriving alongside one. Anything larger is reported and left unread.
const RX_MAX_BLOCKS: usize = 8;

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
    if !sd::set_block_size(h, F1, BLOCK_U32 as u16, ctx) || !sd::enable_function(h, F1, ctx) {
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
/// message header `{id, dest, src, param_len}`, the parameters; then, if that is not a whole number of
/// 512-byte blocks, a 4-byte zero tail and zeros up to the next whole block. A frame of one block goes in
/// byte mode and a longer one in block mode (`sd::write_fifo`, the Linux core's choice). `quiet` drops the
/// per-message line, for the upload's hundreds of blocks.
pub(crate) fn send_msg(h: &dyn SdioHost, id: u16, dest: u16, param: &[u8], quiet: bool, ctx: &ServiceContext) -> bool {
    let mut frame = [0u8; FRAME_MAX];
    let Some(total) = build_frame(id, dest, param, &mut frame) else {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC message {:#06x} with {} parameter bytes does not fit the {}-byte frame - not sent",
            id, param.len(), FRAME_MAX));
        return false;
    };
    if !quiet {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC sending message {:#06x} to task {} ({} parameter byte(s); header {:02x} {:02x} {:02x} {:02x})",
            id, dest, param.len(), frame[0], frame[1], frame[2], frame[3]));
    }
    push_frame(h, &frame, total, id, ctx)
}

/// Put one built frame on the bus - a message or a data frame, the same way (`aicwf_sdio_send`): only when
/// the chip has room for it, then through the write FIFO. `what` names it in a refusal: a message id, or
/// `0xffff` for data.
pub(crate) fn push_frame(h: &dyn SdioHost, frame: &[u8; FRAME_MAX], total: usize, what: u16, ctx: &ServiceContext) -> bool {

    // Sent only when the chip has MORE room than the frame: `len < buffer_cnt * BUFFER_SIZE`, strictly, so
    // a three-block frame needs two free buffers.
    let n = free_buffers(h, ctx);
    if n == 0 || total as u32 >= n as u32 * FW_BUFFER {
        ctx.log_fmt(format_args!(
            "wifi-driver: AIC flow control reports {} free firmware buffer(s) after 20 ms, {} needed for {} bytes - frame {:#06x} was not sent",
            n, total / FW_BUFFER as usize + 1, total, what));
        return false;
    }
    // The frame becomes words in place of a second buffer: the FIFO takes them little-endian.
    let mut words = [0u32; FRAME_MAX / 4];
    for (i, w) in words[..total / 4].iter_mut().enumerate() {
        *w = u32::from_le_bytes([frame[4 * i], frame[4 * i + 1], frame[4 * i + 2], frame[4 * i + 3]]);
    }
    sd::write_fifo(h, F1, REG_WR_FIFO, &mut words[..total / 4], BLOCK_U32, ctx)
}

/// Poll for what the chip has to say, as `aicwf_sdio_hal_irqhandler` would on an interrupt: read the status
/// register; acknowledge a soft interrupt; and when it names a length, read that much from the read FIFO.
/// Returns the number of bytes read into `buf`, 0 when nothing came within `budget`.
pub(crate) fn receive(h: &dyn SdioHost, buf: &mut [u32], budget: Budget, quiet: bool, ctx: &ServiceContext) -> usize {
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
                (st & 0x7f) as usize * BLOCK
            };
            if !quiet {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC status {:#04x} after {} look(s) - {} byte(s) to read", st, looks, bytes));
            }
            if bytes == 0 || bytes > buf.len() * 4 || bytes % 4 != 0 {
                ctx.log_fmt(format_args!("wifi-driver: AIC has {} bytes waiting, more than the {}-byte read - left unread", bytes, buf.len() * 4));
                return 0;
            }
            let words = &mut buf[..bytes / 4];
            return if sd::read_fifo(h, F1, REG_RD_FIFO, words, BLOCK_U32, ctx) { bytes } else { 0 };
        }
        if d.expired() {
            // Silence is the caller's to report when it is quiet: a scan polls through quiet stretches.
            if quiet {
                return 0;
            }
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC said nothing in {} ms ({} looks at F1 0x04; F1 0x01 = {:#04x}, F1 0x03 = {:#04x})",
                budget.as_us() / 1000, looks, rd(h, REG_PENDING).unwrap_or(0), rd(h, REG_FLOW_CTRL).unwrap_or(0)));
            return 0;
        }
        d.pause();
    }
}


/// How many parameter bytes of a confirm are kept: the firmware version's `{len, str[63]}` is the largest
/// this driver reads.
pub const CFM_MAX: usize = 64;

/// A configuration packet carrying the firmware's own print text (`rwnx_rx_handle_print`, "FWLOG").
const TYPE_CFG_PRINT: u8 = 0x13;

/// Walk the packets in `bytes` (`aicwf_process_rxframes`): each starts with a 16-bit length and a type byte;
/// a configuration packet's message sits right after its 4-byte header as `{id, dest, src, param_len,
/// pattern, param...}`, a print packet carries text, and a data packet is skipped with its hardware header.
/// Returns the confirm `want` - its parameter length and up to `CFM_MAX` parameter bytes - if one is there.
/// Every other message is an indication, as `cmd_mgr_msgind` treats one, and is logged.
fn find_cfm(bytes: &[u8], want: u16, quiet: bool, ctx: &ServiceContext) -> Option<(usize, [u8; CFM_MAX])> {
    let mut at = 0usize;
    let mut packets = 0u32;
    let mut found = None;
    while at + 4 <= bytes.len() && packets < 32 {
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
        let end = (m + plen).min(bytes.len());
        if ty & 0x7f == TYPE_CFG_PRINT {
            let text = &bytes[m..end];
            let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
            let text = core::str::from_utf8(text).unwrap_or("(not text)");
            ctx.log_fmt(format_args!("wifi-driver: AIC firmware says: {}", text.trim_end()));
        } else if ty & 0x7f == TYPE_CFG_CMD_RSP && m + 12 <= bytes.len() {
            let id = u16::from_le_bytes([bytes[m], bytes[m + 1]]);
            let param_len = u16::from_le_bytes([bytes[m + 6], bytes[m + 7]]) as usize;
            if id == want && found.is_none() {
                if !quiet {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: AIC receive - message {:#06x} with {} parameter byte(s) (packet type {:#04x}, length {})",
                        id, param_len, ty, plen));
                }
                let mut p = [0u8; CFM_MAX];
                let n = param_len.min(CFM_MAX).min(bytes.len().saturating_sub(m + 12));
                p[..n].copy_from_slice(&bytes[m + 12..m + 12 + n]);
                found = Some((param_len, p));
            } else {
                ctx.log_fmt(format_args!(
                    "wifi-driver: AIC receive - message {:#06x} with {} parameter byte(s) that nothing asked for (an indication)",
                    id, param_len));
            }
        } else {
            ctx.log_fmt(format_args!("wifi-driver: AIC receive - a configuration packet of type {:#04x}, length {}", ty, plen));
        }
        at += ((plen + 3) & !3) + 4;
    }
    found
}

/// One request and its confirm: send `id` to task `dest` with `param`, then read what the chip has to say
/// until the confirm `cfm` is among it or `CFM_WAIT` runs out - a reply that holds only a firmware print or
/// an indication is read past, not taken for a missing confirm. Returns the confirm's parameter length and
/// first `CFM_MAX` bytes. `quiet` keeps a successful exchange off the log; a failure always says which step.
fn request_to(h: &dyn SdioHost, id: u16, dest: u16, param: &[u8], cfm: u16, quiet: bool, ctx: &ServiceContext) -> Option<(usize, [u8; CFM_MAX])> {
    /// The vendor driver waits six seconds for a confirm; a ROM or firmware that has answered every
    /// request in milliseconds is given two.
    const CFM_WAIT: Budget = Budget::ms(2_000);
    if !send_msg(h, id, dest, param, quiet, ctx) {
        return None;
    }
    let mut d = wait::Deadline::start(ctx, CFM_WAIT);
    let mut reads = 0u32;
    loop {
        let mut buf = [0u32; RX_MAX_BLOCKS * BLOCK / 4];
        let n = receive(h, &mut buf, Budget::ms(1_000), quiet, ctx);
        if n > 0 {
            reads += 1;
            let mut bytes = [0u8; RX_MAX_BLOCKS * BLOCK];
            for (i, w) in buf[..n / 4].iter().enumerate() {
                bytes[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
            }
            if !quiet {
                ctx.log_fmt(format_args!("wifi-driver: AIC receive - first 16 bytes {:02x?}", &bytes[..16.min(n)]));
            }
            if let Some(r) = find_cfm(&bytes[..n], cfm, quiet, ctx) {
                return Some(r);
            }
        }
        if d.expired() {
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC request {:#06x} - no {:#06x} confirm in {} ms ({} read(s))",
                id, cfm, CFM_WAIT.as_us() / 1000, reads));
            return None;
        }
    }
}

/// A request to the debug task, which every ROM exchange is.
fn request(h: &dyn SdioHost, id: u16, param: &[u8], cfm: u16, quiet: bool, ctx: &ServiceContext) -> Option<(usize, [u8; CFM_MAX])> {
    request_to(h, id, TASK_DBG, param, cfm, quiet, ctx)
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

// ------------------------------------------------------------------------- the running firmware (V3)

/// The firmware's own tasks: the MAC management task most requests go to.
pub(crate) const TASK_MM: u16 = 0;

// `enum mm_msg_tag` (`lmac_msg.h`), `LMAC_FIRST_MSG(TASK_MM)` = 0, each confirm one past its request.
const MM_SET_RF_CALIB_REQ: u16 = 0x0069;
const MM_GET_MAC_ADDR_REQ: u16 = 0x0073;
const MM_SET_TXPWR_IDX_LVL_REQ: u16 = 0x0077;
const MM_SET_STACK_START_REQ: u16 = 0x007B;
const MM_GET_FW_VERSION_REQ: u16 = 0x0080;

/// What the running firmware said about itself.
pub struct FwFacts {
    pub version: [u8; 63],
    pub version_len: usize,
    pub mac: [u8; 6],
    pub five_ghz: bool,
}

/// The running firmware's bring-up, as `aicwf_sdio_probe` and `rwnx_cfg80211_init` begin it for the D80,
/// up to the MAC address: the wake check the runtime driver adds, the sub-id read, stack start, firmware
/// version, the two RF messages the defaults send (`aicwf_set_rf_config_8800d80`), then the MAC. The vendor
/// driver goes on past any confirm that fails; this one stops and says which, because a step that failed is
/// the next thing to look at.
pub fn bring_up(h: &dyn SdioHost, ctx: &ServiceContext) -> Option<FwFacts> {
    // The runtime driver's own wake check: write the wake value, 5 ms, read the awake bit. It only logs.
    let _ = wr(h, F1, REG_TO_DEVICE, WAKE);
    delay::hold(ctx, Budget::ms(5));
    let p = rd(h, REG_PENDING).unwrap_or(0);
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC after the start - F1 0x01 = {:#04x} ({})",
        p, if p & AWAKE_BIT != 0 { "awake" } else { "NOT showing awake; the vendor driver logs this and goes on" }));

    let sub = mem_read(h, 0x0000_0020, false, ctx)?;
    ctx.log_fmt(format_args!("wifi-driver: AIC chip sub-id {:#04x} (the word at 0x20 is {:#010x})", sub & 0xff, sub));

    let (_, c) = request_to(h, MM_SET_STACK_START_REQ, TASK_MM, &[1, 0, 0x20, 0], MM_SET_STACK_START_REQ + 1, false, ctx)?;
    let five_ghz = c[0] != 0;
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC stack started - 5 GHz {}, vendor info {:#04x}", if five_ghz { "supported" } else { "not supported" }, c[1]));

    let (len, c) = request_to(h, MM_GET_FW_VERSION_REQ, TASK_MM, &[0], MM_GET_FW_VERSION_REQ + 1, false, ctx)?;
    let mut version = [0u8; 63];
    let vlen = (c[0] as usize).min(63).min(len.saturating_sub(1));
    version[..vlen].copy_from_slice(&c[1..1 + vlen]);

    let (_, c) = request_to(h, MM_SET_TXPWR_IDX_LVL_REQ, TASK_MM, &txpwr_lvl_v3(), MM_SET_TXPWR_IDX_LVL_REQ + 1, false, ctx)?;
    ctx.log_fmt(format_args!("wifi-driver: AIC transmit power table taken (confirm starts {:02x?})", &c[..4]));
    let (_, c) = request_to(h, MM_SET_RF_CALIB_REQ, TASK_MM, &rf_calib(), MM_SET_RF_CALIB_REQ + 1, false, ctx)?;
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC RF calibrated - rx gain tables at {:#010x} / {:#010x}, tx at {:#010x} / {:#010x}",
        le32(&c, 0), le32(&c, 4), le32(&c, 8), le32(&c, 12)));

    let (_, c) = request_to(h, MM_GET_MAC_ADDR_REQ, TASK_MM, &1u32.to_le_bytes(), MM_GET_MAC_ADDR_REQ + 1, false, ctx)?;
    let mut mac = [0u8; 6];
    mac.copy_from_slice(&c[..6]);
    Some(FwFacts { version, version_len: vlen, mac, five_ghz })
}

/// The firmware's station-management task, which the `ME_*` messages go to.
pub(crate) const TASK_ME: u16 = 5;

pub(crate) const MM_RESET_REQ: u16 = 0x0000;
const MM_START_REQ: u16 = 0x0002;
const MM_VERSION_REQ: u16 = 0x0004;
const MM_ADD_IF_REQ: u16 = 0x0006;
const MM_SET_COEX_REQ: u16 = 0x0065;
const ME_CONFIG_REQ: u16 = 0x1400;
const ME_CHAN_CONFIG_REQ: u16 = 0x1402;

/// The interface the firmware made for this driver: its index, which every later message names.
pub struct Interface {
    pub index: u8,
}

/// The rest of the bring-up, in the vendor runtime driver's order (`rwnx_cfg80211_init`, then `rwnx_open`
/// for the first interface): reset, version, the capability and channel configuration, start, Bluetooth
/// coexistence, and one station interface at the firmware's own MAC address. Each confirm is required;
/// the vendor driver checks only the last one's status, this one stops at whichever fails and says so.
pub fn bring_up_station(h: &dyn SdioHost, mac: [u8; 6], five_ghz: bool, ctx: &ServiceContext) -> Option<Interface> {
    request_to(h, MM_RESET_REQ, TASK_MM, &[], MM_RESET_REQ + 1, false, ctx)?;
    ctx.log("wifi-driver: AIC firmware reset");

    let (len, c) = request_to(h, MM_VERSION_REQ, TASK_MM, &[], MM_VERSION_REQ + 1, false, ctx)?;
    let lmac = le32(&c, 0);
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC LMAC {}.{}.{}.{}, MAC hardware {:#010x} {:#010x}, PHY {:#010x} {:#010x}, features {:#010x}, {} stations, {} interfaces ({} bytes)",
        lmac >> 24, (lmac >> 16) & 0xff, (lmac >> 8) & 0xff, lmac & 0xff,
        le32(&c, 4), le32(&c, 8), le32(&c, 12), le32(&c, 16), le32(&c, 20),
        u16::from_le_bytes([c[24], c[25]]), c[26], len));

    request_to(h, ME_CONFIG_REQ, TASK_ME, &ME_CONFIG, ME_CONFIG_REQ + 1, false, ctx)?;
    ctx.log("wifi-driver: AIC capabilities configured (HT, VHT, HE; one stream, 80 MHz)");
    let chans = chan_config(five_ghz);
    request_to(h, ME_CHAN_CONFIG_REQ, TASK_ME, &chans, ME_CHAN_CONFIG_REQ + 1, false, ctx)?;
    ctx.log_fmt(format_args!(
        "wifi-driver: AIC channel list taken - {} at 2.4 GHz, {} at 5 GHz, {} dBm", chans[252], chans[253], CHAN_TX_POWER_DBM));

    request_to(h, MM_START_REQ, TASK_MM, &mm_start(), MM_START_REQ + 1, false, ctx)?;
    ctx.log("wifi-driver: AIC MAC started");
    request_to(h, MM_SET_COEX_REQ, TASK_MM, &COEX, MM_SET_COEX_REQ + 1, false, ctx)?;

    let (_, c) = request_to(h, MM_ADD_IF_REQ, TASK_MM, &add_if(mac), MM_ADD_IF_REQ + 1, false, ctx)?;
    if c[0] != 0 {
        ctx.log_fmt(format_args!("wifi-driver: AIC refused the station interface - status {:#04x}", c[0]));
        return None;
    }
    Some(Interface { index: c[1] })
}

// ------------------------------------------------------------------------------------------ scan (V4)

pub(crate) const TASK_SCANU: u16 = 4;
pub(crate) const SCANU_START_REQ: u16 = 0x1000;
/// The END of a scan: it arrives unsolicited after the last result, as `{vif_idx, status, result_cnt}`.
pub(crate) const SCANU_START_CFM: u16 = 0x1001;
pub(crate) const SCANU_RESULT_IND: u16 = 0x1004;
/// The scan request's own confirm, which the vendor driver waits for and discards (`_ADDTIONAL`, sic).
pub(crate) const SCANU_START_CFM_ADDITIONAL: u16 = 0x1009;
/// A message that arrives once per channel swept: 39 in a sweep of 14 + 25 channels, on every scan the
/// board has run (2026-10-05). Its name is not confirmed from the vendor source, so it is numbered, not
/// named; nothing waits for it, and it is read and left without a line each, which was 39 lines a scan.
pub(crate) const PER_CHANNEL_IND: u16 = 0x004f;
/// The pair that replaces it when a JOINED radio scans: 39 of each in one sweep (2026-10-05 10:23), one pair
/// per channel - consistent with the radio leaving its own channel and coming back, which is what the
/// names here say. Not confirmed from the vendor source, so numbered, and read without a line each.
pub(crate) const JOINED_CHANNEL_OUT: u16 = 0x0044;
pub(crate) const JOINED_CHANNEL_BACK: u16 = 0x0045;

/// The largest read the scan takes at once, in blocks: results queue up while the radio sweeps, and each
/// carries a whole beacon.
const SCAN_RX_BLOCKS: usize = 16;
/// The bytes one read takes at most: the scan's 16 blocks, which is also the most a burst of data frames
/// queues before it is read.
pub(crate) const RX_BYTES: usize = SCAN_RX_BLOCKS * BLOCK;

/// `receive`, into bytes: what the chip had waiting within `budget`, little-endian as the FIFO gives it.
pub(crate) fn receive_bytes(h: &dyn SdioHost, bytes: &mut [u8; RX_BYTES], budget: Budget, quiet: bool, ctx: &ServiceContext) -> usize {
    let mut buf = [0u32; RX_BYTES / 4];
    let n = receive(h, &mut buf, budget, quiet, ctx);
    for (i, w) in buf[..n / 4].iter().enumerate() {
        bytes[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    n
}

/// One packet out of a read: a message (id and its FULL parameters), or a data packet (the whole packet,
/// its 60-byte receive header first).
pub(crate) enum Packet<'a> {
    Msg(u16, &'a [u8]),
    Data(&'a [u8]),
}

/// Walk every packet in `bytes` as `aicwf_process_rxframes` does, handing each message (id and FULL
/// parameters) and each data packet to `on`; firmware prints are logged, and the transmit confirms (type
/// 0x12, the vendor driver's own bookkeeping, nothing owed to the chip) are passed over. Returns how many
/// packets were handed on.
pub(crate) fn walk_packets(bytes: &[u8], on: &mut dyn FnMut(Packet), ctx: &ServiceContext) -> u32 {
    let mut at = 0usize;
    let mut handed = 0u32;
    let mut packets = 0u32;
    while at + 4 <= bytes.len() && packets < 64 {
        packets += 1;
        let plen = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
        let ty = bytes[at + 2];
        if plen == 0 {
            break;
        }
        if ty & TYPE_CFG != TYPE_CFG {
            // A data packet: the length counts the frame AFTER the 60-byte receive header.
            let end = (at + RX_HW_HDR + plen).min(bytes.len());
            on(Packet::Data(&bytes[at..end]));
            handed += 1;
            at += (plen + RX_HW_HDR + 3) & !3;
            continue;
        }
        let m = at + 4;
        let end = (m + plen).min(bytes.len());
        if ty & 0x7f == TYPE_CFG_PRINT {
            let text = &bytes[m..end];
            let text = &text[..text.iter().position(|&b| b == 0).unwrap_or(text.len())];
            ctx.log_fmt(format_args!(
                "wifi-driver: AIC firmware says: {}", core::str::from_utf8(text).unwrap_or("(not text)").trim_end()));
        } else if ty & 0x7f == TYPE_CFG_CMD_RSP && m + 12 <= end {
            let id = u16::from_le_bytes([bytes[m], bytes[m + 1]]);
            let param_len = u16::from_le_bytes([bytes[m + 6], bytes[m + 7]]) as usize;
            let p_end = (m + 12 + param_len).min(end);
            on(Packet::Msg(id, &bytes[m + 12..p_end]));
            handed += 1;
        }
        at += ((plen + 3) & !3) + 4;
    }
    handed
}

/// `walk_packets`, messages only.
fn walk_messages(bytes: &[u8], on: &mut dyn FnMut(u16, &[u8]), ctx: &ServiceContext) -> u32 {
    walk_packets(bytes, &mut |p| if let Packet::Msg(id, params) = p { on(id, params) }, ctx)
}

/// What one sweep heard, and how it ended.
pub struct SweepEnd {
    /// The scan's own end message arrived (`SCANU_START_CFM`), with its status and result count.
    pub ended: Option<(u8, u8)>,
    /// Result indications received, and those that could not be read as a beacon.
    pub results: u32,
    pub unreadable: u32,
    pub ms: u64,
}

/// One wildcard scan of every channel, start to end, outside the serve loop: the request, its confirm,
/// every result into `scan` (deduplicated by BSSID, the strongest kept, security classified from the
/// beacon's own elements by `sdk/wifi`), until the end message or `budget`. Polled, like everything here.
pub fn scan_once(h: &dyn SdioHost, vif: u8, five_ghz: bool, scan: &mut godspeed_wifi::bss::Scan, budget: Budget, ctx: &ServiceContext) -> SweepEnd {
    let mut end = SweepEnd { ended: None, results: 0, unreadable: 0, ms: 0 };
    if !send_msg(h, SCANU_START_REQ, TASK_SCANU, &scanu_start(vif, five_ghz), false, ctx) {
        return end;
    }
    let mut d = wait::Deadline::start(ctx, budget);
    let mut confirmed = false;
    while end.ended.is_none() {
        let mut buf = [0u32; SCAN_RX_BLOCKS * BLOCK / 4];
        let n = receive(h, &mut buf, Budget::ms(500), true, ctx);
        if n > 0 {
            let mut bytes = [0u8; SCAN_RX_BLOCKS * BLOCK];
            for (i, w) in buf[..n / 4].iter().enumerate() {
                bytes[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
            }
            walk_messages(&bytes[..n], &mut |id, p| match id {
                SCANU_START_CFM_ADDITIONAL => confirmed = true,
                SCANU_RESULT_IND => {
                    end.results += 1;
                    match parse_result(p) {
                        Some(r) => {
                            let mut net = godspeed_wifi::bss::Network::blank();
                            net.bssid = r.bssid();
                            if let Some(s) = r.ssid() {
                                net.ssid[..s.len()].copy_from_slice(s);
                                net.ssid_len = s.len() as u8;
                            }
                            net.chanspec = channel_of(r.freq) as u16;
                            net.rssi = r.rssi as i16;
                            net.security = godspeed_wifi::bss::classify(r.ies(), r.capability());
                            scan.keep(net);
                        }
                        None => end.unreadable += 1,
                    }
                }
                SCANU_START_CFM => end.ended = Some((p.get(1).copied().unwrap_or(0xff), p.get(2).copied().unwrap_or(0))),
                PER_CHANNEL_IND => {}
                other => ctx.log_fmt(format_args!(
                    "wifi-driver: AIC during the scan - message {:#06x} ({} parameter bytes), not a scan message", other, p.len())),
            }, ctx);
        }
        if d.expired() {
            break;
        }
    }
    end.ms = d.elapsed_us() / 1000;
    if !confirmed {
        ctx.log("wifi-driver: AIC scan - the request's own confirm (0x1009) never came");
    }
    end
}
