// SPDX-License-Identifier: GPL-2.0-only
//! The control channel: asking the running firmware a question and reading its answer.
//!
//! Everything up to here has been one-way. The firmware is loaded, running, and has published its shared
//! structure; the bus carries frames. This is the first exchange, and the first thing that can prove the
//! radio is a radio rather than a correctly-loaded lump: **its own MAC address**, which only the firmware
//! knows and no host-side arithmetic can fake.
//!
//! ## Three headers, stacked
//!
//! A control frame is a hardware header, a software header, a BCDC command header, and a payload. All three
//! layouts are quoted, because getting any field wrong produces a frame the firmware silently ignores.
//!
//! From OpenBSD's `bwfm`, which yields these where Linux's `sdio.c` truncates:
//!
//! ```c
//! struct bwfm_sdio_hwhdr {
//! 	uint16_t frmlen;
//! 	uint16_t cksum;
//! };
//!
//! struct bwfm_sdio_swhdr {
//! 	uint8_t seqnr;
//! 	uint8_t chanflag;
//! 	uint8_t nextlen;
//! 	uint8_t dataoff;
//! 	uint8_t flowctl;
//! 	uint8_t maxseqnr;
//! 	uint16_t res0;
//! };
//!
//! #define BWFM_SDIO_SWHDR_CHANNEL_CONTROL		0x00
//! ```
//!
//! and from Linux's `bcdc.c`, which does not truncate:
//!
//! ```c
//! struct brcmf_proto_bcdc_dcmd {
//! 	__le32 cmd;	/* dongle command value */
//! 	__le32 len;	/* lower 16: output buflen;
//! 			 * upper 16: input buflen (excludes header) */
//! 	__le32 flags;	/* flag defns given below */
//! 	__le32 status;	/* status code returned from the device */
//! };
//!
//! #define BCDC_DCMD_ERROR		0x01		/* 1=cmd failed */
//! #define BCDC_DCMD_SET		0x02		/* 0=get, 1=set cmd */
//! #define BCDC_DCMD_IF_MASK	0xF000		/* I/F index */
//! #define BCDC_DCMD_IF_SHIFT	12
//! #define BCDC_DCMD_ID_MASK	0xFFFF0000	/* id an cmd pairing */
//! #define BCDC_DCMD_ID_SHIFT	16		/* ID Mask shift bits */
//! ```
//!
//! ## Where a frame goes, which is NOT where it looks like it should
//!
//! This is the one fact here that would have been got wrong by guessing, so it is quoted in full:
//!
//! ```c
//! addr = sc->sc_cc->co_base;
//! bwfm_sdio_backplane(sc, addr);
//!
//! addr &= BWFM_SDIO_SB_OFT_ADDR_MASK;
//! addr |= BWFM_SDIO_SB_ACCESS_2_4B_FLAG;
//!
//! if (write)
//! 	err = bwfm_sdio_buf_write(sc, sc->sc_sf[2], addr, data, size);
//! ```
//!
//! A frame is written to **function 2 with the backplane window set to the CHIPCOMMON core base** -
//! `0x18000000` - not to address 0 and not to the firmware's RAM. The offset therefore works out to
//! `0x8000` (offset 0 plus the wide-access flag), which happens to look identical to every other access
//! this driver makes, for an entirely different reason.
//!
//! ## Padding, quoted because the rule is not the obvious one
//!
//! ```c
//! len = sizeof(*hwhdr) + sizeof(*swhdr) + m->m_len;
//! if (len > 512 && (len % 512) != 0)
//! 	roundto = 512;
//! else
//! 	roundto = 4;
//! ```
//!
//! Not "always pad to a block". A short frame pads to four bytes, and only a frame that is both longer than
//! a block and not a whole number of blocks rounds up to 512.
//!
//! ## What is CHECKED, because a silent wrong answer is the failure mode here
//!
//! - **The hardware header validates itself**: `frmlen ^ cksum` must be `0xFFFF`. The reference uses this
//!   exact test, and it is what distinguishes a real frame from a FIFO read that found nothing. A reply is
//!   therefore *waited for* by re-reading until the checksum holds, rather than assumed to be ready.
//! - **The reply's id must match the request's.** `id = (flags & BCDC_DCMD_ID_MASK) >> BCDC_DCMD_ID_SHIFT`,
//!   and a mismatch is a different exchange's answer - the same desync that cost the `fs` protocol a
//!   correlation tag. Checked, not assumed.
//! - **`BCDC_DCMD_ERROR` means the firmware refused**, and then `status` carries its reason. Reported, never
//!   swallowed (§26.7).
//! - **The MAC itself is sanity-checked.** All-zero or all-`0xFF` is not an address, and a driver that
//!   printed one would be reporting a successful exchange that returned nothing.
//!
//! Bounded and stack-only (§26.6.1): one 512-byte frame buffer, no heap, no allocation.

use godspeed_sdk::ServiceContext;

use crate::backplane::{Window, ACCESS_WIDE, CHIPCOMMON_BASE, OFFSET_MASK};
use crate::host::{blk_byte_mode, Host};
use crate::sdio;
use crate::sdio::DATA_FUNC;


/// `sizeof(struct bwfm_sdio_hwhdr)`.
const HWHDR: usize = 4;
/// `sizeof(struct bwfm_sdio_swhdr)`.
const SWHDR: usize = 8;
/// `sizeof(struct brcmf_proto_bcdc_dcmd)`.
const DCMD: usize = 16;
/// Where the payload starts, which is also what `swhdr.dataoff` is set to.
const PAYLOAD_AT: usize = HWHDR + SWHDR + DCMD;

/// `BWFM_SDIO_SWHDR_CHANNEL_CONTROL`.
const CHANNEL_CONTROL: u8 = 0x00;

/// `BRCMF_C_GET_VAR`.
const GET_VAR: u32 = 262;

/// `BCDC_DCMD_ERROR`.
const DCMD_ERROR: u32 = 0x01;
/// `BCDC_DCMD_ID_SHIFT`.
const DCMD_ID_SHIFT: u32 = 16;
/// `BCDC_DCMD_ID_MASK`.
const DCMD_ID_MASK: u32 = 0xFFFF_0000;
/// `BCDC_DCMD_SET` - the flag that makes a command a write rather than a read.
const DCMD_SET: u32 = 0x02;
/// `BRCMF_C_SET_VAR`.
const SET_VAR: u32 = 263;

/// One control frame, bounded. A `cur_etheraddr` exchange is 48 bytes; 512 is a whole block and covers
/// every control message this driver sends.
///
/// Public because the scan path reads frames into a buffer of the same size. Two constants for one wire
/// limit is the duplicated fact the enforcement layer rejects, and rightly.
pub const FRAME: usize = 512;

/// The frame FIFO's address: function 2, with the window set to chipcommon, offset 0, wide access.
fn frame_offset() -> u32 {
    (CHIPCOMMON_BASE & OFFSET_MASK) | ACCESS_WIDE
}

/// `roundto` from the reference: four bytes, unless the frame is both over a block and not a whole
/// number of blocks.
fn round_to(len: usize) -> usize {
    if len > 512 && len % 512 != 0 {
        512
    } else {
        4
    }
}

/// Send one BCDC control request and read its reply. Returns the reply payload length.
///
/// `name` is the iovar, NUL-terminated on the wire. `out` receives the payload the firmware returns,
/// which for a query begins with the value asked for.
fn query_iovar(
    h: &Host,
    w: &mut Window,
    seq: &mut u8,
    reqid: u16,
    name: &str,
    out: &mut [u8],
    ctx: &ServiceContext,
) -> Option<usize> {
    let mut frame = [0u8; FRAME];

    // The payload of a GET_VAR is the variable's name, NUL-terminated, followed by room for the answer.
    let name_len = name.len() + 1;
    let want = core::cmp::max(name_len, out.len());
    if PAYLOAD_AT + want > FRAME {
        ctx.log("wifi-driver: the control frame would not fit its bounded buffer");
        return None;
    }
    frame[PAYLOAD_AT..PAYLOAD_AT + name.len()].copy_from_slice(name.as_bytes());
    // The NUL is already there: the buffer is zeroed.

    let len = PAYLOAD_AT + want;
    let padded = {
        let r = round_to(len);
        (len + r - 1) / r * r
    };

    // ---- Hardware header: the length and its complement, which is how the receiver validates it. ----
    frame[0..2].copy_from_slice(&(len as u16).to_le_bytes());
    frame[2..4].copy_from_slice(&(!(len as u16)).to_le_bytes());

    // ---- Software header. `dataoff` points past all three headers, as the reference sets it. ----
    frame[4] = *seq;
    frame[5] = CHANNEL_CONTROL;
    frame[6] = 0; // nextlen: a hint, and zero means "no hint"
    frame[7] = PAYLOAD_AT as u8;
    // flowctl, maxseqnr and res0 stay zero - they are the CHIP's fields on receive, not the host's on send.

    // ---- BCDC command header. ----
    let flags = (reqid as u32) << DCMD_ID_SHIFT; // no SET bit: this is a get. Interface index 0.
    frame[HWHDR + SWHDR..HWHDR + SWHDR + 4].copy_from_slice(&GET_VAR.to_le_bytes());
    frame[HWHDR + SWHDR + 4..HWHDR + SWHDR + 8].copy_from_slice(&(want as u32).to_le_bytes());
    frame[HWHDR + SWHDR + 8..HWHDR + SWHDR + 12].copy_from_slice(&flags.to_le_bytes());
    // status stays zero on the way out; the firmware fills it on the way back.

    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        ctx.log("wifi-driver: could not point the window at chipcommon, so no frame can be sent");
        return None;
    }

    // BYTE MODE, and a whole number of words, which the reference asserts on
    // (`KASSERT((size & 0x3) == 0)`).
    let words = padded / 4;
    let mut wbuf = [0u32; FRAME / 4];
    for i in 0..words {
        wbuf[i] = u32::from_le_bytes([
            frame[i * 4],
            frame[i * 4 + 1],
            frame[i * 4 + 2],
            frame[i * 4 + 3],
        ]);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: asking the firmware for `{}` - {} byte frame padded to {}, seq {}, request id {}",
        name, len, padded, *seq, reqid
    ));
    if !sdio::write_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut wbuf[..words],
        blk_byte_mode(padded as u32),
        None,
        ctx,
    ) {
        return None;
    }
    *seq = seq.wrapping_add(1);

    // ---- The reply, read with the ONE frame reader. ----------------------------------------------
    // This used to hand-roll the header read and the two-step body read, and the copy went stale: it
    // rejected `frmlen 12, dataoff 12` as "impossible" when that is a legitimate HEADER-ONLY frame the chip
    // uses for flow control - which `read_frame` has always handled. Observed on hardware, and the reply
    // was probably one poll later. One parser now, so it cannot happen in one of two places.
    //
    // The loop shape was also wrong independently of that: it broke out on the FIRST checksum-valid header
    // and judged it afterwards, so any frame that was not the reply ended the exchange. A frame that is not
    // the reply is SKIPPED and the wait continues.
    const REPLY_TRIES: u32 = 200;
    let mut rbuf = [0u8; FRAME];
    let mut frames = 0u32;
    let mut headers_only = 0u32;
    let mut other_channel = 0u32;
    let mut wrong_id = 0u32;

    for _ in 0..REPLY_TRIES {
        match read_frame(h, w, &mut rbuf, ctx) {
            Some((chanflag, len)) => {
                frames += 1;
                // DESCRIBE THE FIRST FEW, before any judgement about whether they match. The point is to
                // find out what the firmware is sending, and a frame skipped by a rule that is itself
                // wrong would otherwise never be seen.
                const DUMP_FRAMES: u32 = 4;
                if frames <= DUMP_FRAMES {
                    describe_frame(frames, chanflag, &rbuf, len, ctx);
                }
                if chanflag & 0x0F != CHANNEL_CONTROL {
                    // An event or a data frame. Not this exchange's business.
                    other_channel += 1;
                    continue;
                }
                if len < DCMD {
                    // Header-only, or too short to carry a BCDC header. Flow control, not an answer.
                    headers_only += 1;
                    continue;
                }
                let rflags = u32::from_le_bytes([rbuf[8], rbuf[9], rbuf[10], rbuf[11]]);
                let status = u32::from_le_bytes([rbuf[12], rbuf[13], rbuf[14], rbuf[15]]);
                let rid = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
                if rid != reqid {
                    // ANOTHER EXCHANGE'S ANSWER. Skipped rather than parsed, and skipped rather than
                    // treated as fatal: matching ids is what stops a protocol going one reply out of step,
                    // and a stale reply arriving late is exactly what that guards against.
                    wrong_id += 1;
                    continue;
                }
                if rflags & DCMD_ERROR != 0 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver: the firmware REFUSED the request - BCDC error flag set, status \
                         {:#010x} ({})",
                        status, status as i32
                    ));
                    return None;
                }
                // The payload follows the 16-byte BCDC header. `read_frame` has already applied `dataoff`.
                let avail = len - DCMD;
                let n = core::cmp::min(avail, out.len());
                out[..n].copy_from_slice(&rbuf[DCMD..DCMD + n]);
                if frames > 1 {
                    ctx.log_fmt(format_args!(
                        "wifi-driver:   the reply arrived after {} other frame(s) - {} header-only, {} on \
                         another channel, {} from another exchange",
                        frames - 1, headers_only, other_channel, wrong_id
                    ));
                }
                return Some(n);
            }
            None => ctx.sleep_ms(1),
        }
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: no reply to `{}` across {} reads. {} frame(s) DID arrive: {} header-only (flow \
         control), {} on another channel, {} from another exchange - so \"nothing answered\" and \"nothing \
         MATCHED\" are told apart here rather than left to guess",
        name, REPLY_TRIES, frames, headers_only, other_channel, wrong_id
    ));
    None
}

/// Describe a frame that arrived, so a mismatch says WHAT it was rather than only that it happened.
///
/// The frame census - how many frames arrived, how many matched - is enough to prove the wire works and not
/// enough to say why a reply was not recognised. Three hypotheses fit "a control frame arrived whose id was
/// wrong": the id is at a different offset, the BCDC header starts at a different offset, or the firmware
/// uses a different id convention. This tells them apart in one boot.
///
/// Bounded deliberately: the caller prints only the first few frames. A 200-iteration loop that described
/// every frame would bury the answer in its own output, and a flood jams the console queue.
fn describe_frame(which: u32, chanflag: u8, buf: &[u8], len: usize, ctx: &ServiceContext) {
    let channel = chanflag & 0x0F;
    let kind = match channel {
        CHANNEL_CONTROL => "CONTROL",
        1 => "EVENT",
        2 => "DATA",
        3 => "GLOM",
        _ => "(unknown channel)",
    };
    ctx.log_fmt(format_args!(
        "wifi-driver:   frame {}: channel {:#04x} ({}), {} byte(s) after the SDPCM headers",
        which, chanflag, kind, len
    ));
    if len >= DCMD {
        let cmd = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let dlen = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let flags = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let status = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]);
        ctx.log_fmt(format_args!(
            "wifi-driver:     read as BCDC: cmd {} len {} flags {:#010x} status {:#010x} -> id {}, \
             set {}, error {}",
            cmd,
            dlen,
            flags,
            status,
            (flags & DCMD_ID_MASK) >> DCMD_ID_SHIFT,
            flags & DCMD_SET != 0,
            flags & DCMD_ERROR != 0
        ));
    }
    // THE BYTES THEMSELVES, because every decode above assumes an offset and the bytes assume nothing. If
    // the header starts elsewhere, `cmd 262` will be visible at some other position here.
    let show = core::cmp::min(len, 32);
    let mut i = 0;
    while i < show {
        let end = core::cmp::min(i + 8, show);
        ctx.log_fmt(format_args!(
            "wifi-driver:     [{:02}] {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
            i,
            buf[i],
            if i + 1 < end { buf[i + 1] } else { 0 },
            if i + 2 < end { buf[i + 2] } else { 0 },
            if i + 3 < end { buf[i + 3] } else { 0 },
            if i + 4 < end { buf[i + 4] } else { 0 },
            if i + 5 < end { buf[i + 5] } else { 0 },
            if i + 6 < end { buf[i + 6] } else { 0 },
            if i + 7 < end { buf[i + 7] } else { 0 }
        ));
        i = end;
    }
}

/// Read one frame off function 2, whatever channel it is on.
///
/// Returns `(chanflag, len)`, where `buf[..len]` is the frame **after** the SDIO hardware and software
/// headers - so for an event that is the pseudo-ethernet frame, starting at its destination address. The
/// caller decides what the channel means; this function only delivers bytes.
///
/// `None` means no frame was there. That is a normal, frequent answer rather than an error: the hardware
/// header validates itself (`frmlen ^ cksum == 0xFFFF`), so a read that does not validate is how an empty
/// FIFO looks, and the reference treats it the same way. It is therefore SILENT - logging every empty poll
/// would bury the frames that do arrive.
pub fn read_frame(
    h: &Host,
    w: &mut Window,
    buf: &mut [u8; FRAME],
    ctx: &ServiceContext,
) -> Option<(u8, usize)> {
    let mut hdr = [0u32; (HWHDR + SWHDR) / 4];
    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        return None;
    }
    if !sdio::read_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut hdr,
        blk_byte_mode((HWHDR + SWHDR) as u32),
        None,
        ctx,
    ) {
        return None;
    }
    let b0 = hdr[0].to_le_bytes();
    let b1 = hdr[1].to_le_bytes();
    let frmlen = u16::from_le_bytes([b0[0], b0[1]]);
    let cksum = u16::from_le_bytes([b0[2], b0[3]]);
    if frmlen == 0 || (frmlen ^ cksum) != 0xFFFF {
        return None;
    }
    let frmlen = frmlen as usize;
    let chanflag = b1[1];
    let dataoff = b1[3] as usize;
    if frmlen > FRAME || dataoff < HWHDR + SWHDR || dataoff > frmlen {
        ctx.log_fmt(format_args!(
            "wifi-driver: a frame validated its checksum but describes an impossible shape - frmlen {}, \
             dataoff {}, channel {:#04x} (headers are {}, buffer is {})",
            frmlen, dataoff, chanflag, HWHDR + SWHDR, FRAME
        ));
        return None;
    }

    let rest = frmlen - (HWHDR + SWHDR);
    if rest == 0 {
        // A header-only frame is legitimate - the chip uses them for flow control - and carries no payload.
        return Some((chanflag, 0));
    }
    let words = (rest + 3) / 4;
    let mut rbuf = [0u32; FRAME / 4];
    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        return None;
    }
    if !sdio::read_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut rbuf[..words],
        blk_byte_mode((words * 4) as u32),
        None,
        ctx,
    ) {
        return None;
    }
    let mut body = [0u8; FRAME];
    for i in 0..words {
        body[i * 4..i * 4 + 4].copy_from_slice(&rbuf[i].to_le_bytes());
    }
    // `dataoff` is measured from the start of the whole frame; the header read already took the first
    // twelve bytes, so the payload starts that far into what just arrived.
    let off = dataoff - (HWHDR + SWHDR);
    let len = rest - off;
    buf[..len].copy_from_slice(&body[off..off + len]);
    Some((chanflag, len))
}

/// Write an iovar - `BRCMF_C_SET_VAR` with the name, a NUL, and the value.
///
/// The wire form of a set is the same frame as a get with `BCDC_DCMD_SET` in the flags; the payload is the
/// variable's name, NUL-terminated, immediately followed by its value. This is what starts a scan.
///
/// The reply is read and its request id checked, because a set that the firmware refuses must not look like
/// one it accepted (§26.7) - the whole point of asking is to find out.
pub fn set_iovar(
    h: &Host,
    w: &mut Window,
    name: &str,
    value: &[u8],
    ctx: &ServiceContext,
) -> bool {
    let mut frame = [0u8; FRAME];
    let payload = name.len() + 1 + value.len();
    if PAYLOAD_AT + payload > FRAME {
        ctx.log_fmt(format_args!(
            "wifi-driver: setting `{}` would need {} payload bytes, over this driver's {} byte frame",
            name, payload, FRAME
        ));
        return false;
    }
    frame[PAYLOAD_AT..PAYLOAD_AT + name.len()].copy_from_slice(name.as_bytes());
    // The NUL is already in place - the buffer is zeroed.
    let at = PAYLOAD_AT + name.len() + 1;
    frame[at..at + value.len()].copy_from_slice(value);

    let len = PAYLOAD_AT + payload;
    let r = round_to(len);
    let padded = (len + r - 1) / r * r;
    let reqid: u16 = 2;

    frame[0..2].copy_from_slice(&(len as u16).to_le_bytes());
    frame[2..4].copy_from_slice(&(!(len as u16)).to_le_bytes());
    frame[4] = 0;
    frame[5] = CHANNEL_CONTROL;
    frame[7] = PAYLOAD_AT as u8;
    let flags = ((reqid as u32) << DCMD_ID_SHIFT) | DCMD_SET;
    frame[HWHDR + SWHDR..HWHDR + SWHDR + 4].copy_from_slice(&SET_VAR.to_le_bytes());
    frame[HWHDR + SWHDR + 4..HWHDR + SWHDR + 8].copy_from_slice(&(payload as u32).to_le_bytes());
    frame[HWHDR + SWHDR + 8..HWHDR + SWHDR + 12].copy_from_slice(&flags.to_le_bytes());

    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        return false;
    }
    let words = padded / 4;
    let mut wbuf = [0u32; FRAME / 4];
    for i in 0..words {
        wbuf[i] = u32::from_le_bytes([
            frame[i * 4],
            frame[i * 4 + 1],
            frame[i * 4 + 2],
            frame[i * 4 + 3],
        ]);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: setting `{}` - {} byte value, {} byte frame padded to {}",
        name, value.len(), len, padded
    ));
    if !sdio::write_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut wbuf[..words],
        blk_byte_mode(padded as u32),
        None,
        ctx,
    ) {
        return false;
    }

    // THE REPLY DECIDES. A set whose refusal is discarded is a silent failure, and this one starts a scan -
    // so "accepted" has to mean the firmware said so.
    const SET_TRIES: u32 = 200;
    let mut rbuf = [0u8; FRAME];
    for _ in 0..SET_TRIES {
        if let Some((chanflag, len)) = read_frame(h, w, &mut rbuf, ctx) {
            if chanflag & 0x0F != CHANNEL_CONTROL || len < 16 {
                continue;
            }
            let rflags = u32::from_le_bytes([rbuf[8], rbuf[9], rbuf[10], rbuf[11]]);
            let status = u32::from_le_bytes([rbuf[12], rbuf[13], rbuf[14], rbuf[15]]);
            let rid = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
            if rid != reqid {
                continue;
            }
            if rflags & DCMD_ERROR != 0 {
                ctx.log_fmt(format_args!(
                    "wifi-driver: the firmware REFUSED `{}` - status {:#010x} ({})",
                    name, status, status as i32
                ));
                return false;
            }
            return true;
        }
        ctx.sleep_ms(1);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: `{}` was sent and the firmware never acknowledged it across {} reads. It is NOT \
         reported as set, because a set nobody confirmed is indistinguishable from one that was refused",
        name, SET_TRIES
    ));
    false
}

/// Ask the firmware for its own MAC address - the first thing only a running radio can answer.
///
/// Returns true when a plausible address came back. All-zero and all-`0xFF` are rejected: both are what a
/// successful exchange that returned nothing looks like, and reporting one as the radio's address would be
/// exactly the silent wrong answer this driver keeps being built to avoid.
pub fn report_mac(h: &Host, w: &mut Window, ctx: &ServiceContext) -> bool {
    ctx.log("wifi-driver: stage 13 - the first question put to the firmware");

    let mut seq = 0u8;
    let mut mac = [0u8; 6];
    let n = match query_iovar(h, w, &mut seq, 1, "cur_etheraddr", &mut mac, ctx) {
        Some(n) => n,
        None => return false,
    };
    if n < 6 {
        ctx.log_fmt(format_args!(
            "wifi-driver: the firmware answered but returned only {} byte(s) where a MAC address is 6",
            n
        ));
        return false;
    }
    let all_zero = mac.iter().all(|&b| b == 0);
    let all_ff = mac.iter().all(|&b| b == 0xFF);
    if all_zero || all_ff {
        ctx.log_fmt(format_args!(
            "wifi-driver: the exchange SUCCEEDED and returned {} - which is not an address. A reply that \
             parses but carries nothing is reported as a failure, not as a MAC",
            if all_zero { "all zeros" } else { "all ones" }
        ));
        return false;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: THE RADIO ANSWERED - its MAC address is {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}. \
         Only the firmware knows this, so the control channel works end to end",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    ));
    true
}
