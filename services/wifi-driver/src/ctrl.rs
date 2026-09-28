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

/// One control frame, bounded. A `cur_etheraddr` exchange is 48 bytes; 512 is a whole block and covers
/// every control message this driver sends.
const FRAME: usize = 512;

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

    // ---- The reply. -------------------------------------------------------------------------------
    // WAIT FOR IT BY VALIDATING IT, not by assuming it is there. The hardware header validates itself
    // (`frmlen ^ cksum == 0xFFFF`), which is exactly how the reference tells a frame from nothing, so a
    // read that does not validate means the firmware has not answered yet.
    const REPLY_TRIES: u32 = 200;
    let mut hdr = [0u32; (HWHDR + SWHDR) / 4];
    let mut frmlen = 0usize;
    let mut dataoff = 0usize;
    let mut got = false;
    for _ in 0..REPLY_TRIES {
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
        let fl = u16::from_le_bytes([b0[0], b0[1]]);
        let ck = u16::from_le_bytes([b0[2], b0[3]]);
        if fl != 0 && (fl ^ ck) == 0xFFFF {
            frmlen = fl as usize;
            dataoff = b1[3] as usize;
            got = true;
            break;
        }
        ctx.sleep_ms(1);
    }
    if !got {
        ctx.log_fmt(format_args!(
            "wifi-driver: no valid reply frame after {} reads over ~{} ms - the hardware header never \
             satisfied `frmlen ^ cksum == 0xFFFF`, which is how a real frame is told from an empty FIFO. \
             The request was accepted by the bus; the firmware did not answer it",
            REPLY_TRIES, REPLY_TRIES
        ));
        return None;
    }
    if frmlen < PAYLOAD_AT || frmlen > FRAME || dataoff < HWHDR + SWHDR || dataoff > frmlen {
        ctx.log_fmt(format_args!(
            "wifi-driver: the reply header validated but describes an impossible frame - frmlen {}, \
             dataoff {} (headers are {}+{}, buffer is {})",
            frmlen, dataoff, HWHDR, SWHDR, FRAME
        ));
        return None;
    }

    // The rest of the frame. The header read already consumed the first twelve bytes, which is the
    // reference's own two-step shape.
    let rest = frmlen - (HWHDR + SWHDR);
    let rest_words = (rest + 3) / 4;
    let mut rbuf = [0u32; FRAME / 4];
    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        return None;
    }
    if !sdio::read_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut rbuf[..rest_words],
        blk_byte_mode((rest_words * 4) as u32),
        None,
        ctx,
    ) {
        return None;
    }
    let mut body = [0u8; FRAME];
    for i in 0..rest_words {
        body[i * 4..i * 4 + 4].copy_from_slice(&rbuf[i].to_le_bytes());
    }

    // The BCDC header sits at the start of what follows the software header.
    let rflags = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
    let status = u32::from_le_bytes([body[12], body[13], body[14], body[15]]);
    let rid = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
    if rid != reqid {
        ctx.log_fmt(format_args!(
            "wifi-driver: the reply's request id is {} but {} was asked - this is another exchange's \
             answer, so it is DISCARDED rather than parsed. Matching ids is what stops a protocol \
             silently going a reply out of step",
            rid, reqid
        ));
        return None;
    }
    if rflags & DCMD_ERROR != 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: the firmware REFUSED the request - BCDC error flag set, status {:#010x} ({})",
            status, status as i32
        ));
        return None;
    }

    // `dataoff` is relative to the whole frame; the payload is that far in, minus what the header read
    // already took.
    let off = dataoff - (HWHDR + SWHDR);
    let avail = rest.saturating_sub(off);
    let n = core::cmp::min(avail, out.len());
    out[..n].copy_from_slice(&body[off..off + n]);
    Some(n)
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
