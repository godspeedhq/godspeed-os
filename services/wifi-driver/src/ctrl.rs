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
use crate::host::{blk_block_mode, blk_byte_mode, Host};
use crate::sdio;
use crate::sdio::{DATA_BLOCK, DATA_FUNC};


/// `sizeof(struct bwfm_sdio_hwhdr)`.
const HWHDR: usize = 4;
/// `sizeof(struct bwfm_sdio_swhdr)`.
const SWHDR: usize = 8;
/// `sizeof(struct brcmf_proto_bcdc_dcmd)`.
const DCMD: usize = 16;
/// Where the iovar payload starts within a frame: past all three headers.
///
/// **This is NOT `swhdr.dataoff`.** It was used as both, and that was the bug - see `DATA_OFF`.
const PAYLOAD_AT: usize = HWHDR + SWHDR + DCMD;

/// What goes in `swhdr.dataoff`: where the PROTOCOL DATA begins, which is the BCDC header.
///
/// ```c
/// swhdr->dataoff = sizeof(*hwhdr) + sizeof(*swhdr);
/// ```
///
/// 4 + 8 = 12. It points **at** the BCDC header, immediately after the SDIO hardware and software headers -
/// not past it at the payload.
///
/// **This driver wrote `PAYLOAD_AT` (28) here for three boots.** The firmware therefore skipped the real
/// BCDC header and read the iovar name as one: it parsed `cmd` from `"cur_"`, kept `"dd"` from
/// `cur_ether`**`add`**`r` in the id field, set the error bit and returned -24. Every byte of that reply was
/// this driver's own payload coming back with a complaint attached.
///
/// The quote above was already in this module's documentation, and the comment beside the write claimed it
/// "points past all three headers, as the reference sets it". It points past two. A correct citation sitting
/// beside code that contradicts it reads as verification, which is why this constant exists: one name, one
/// value, and the two call sites cannot disagree.
const DATA_OFF: usize = HWHDR + SWHDR;

/// `BWFM_SDIO_SWHDR_CHANNEL_CONTROL`.
const CHANNEL_CONTROL: u8 = 0x00;

/// `BRCMF_C_GET_VAR`.
const GET_VAR: u32 = 262;
/// `BRCMF_C_UP` - raise the interface. A scan on a down interface is refused with `BCME_NOTUP`.
const CMD_UP: u32 = 2;
/// `BWFM_C_SET_INFRA` - infrastructure mode, which a station needs.
const CMD_SET_INFRA: u32 = 20;
/// `BWFM_C_SET_AP` - access-point mode, which a station does not want.
const CMD_SET_AP: u32 = 118;

/// Name a firmware error code, because a number teaches nothing and a name teaches the fix.
///
/// From `brcmf_fil_errstr`, where the negative code indexes the table:
///
/// ```c
/// static const char * const brcmf_fil_errstr[] = {
/// 	"BCME_OK",                          /* 0 */
/// 	"BCME_ERROR",                       /* 1 */
/// 	"BCME_BADARG",                      /* 2 */
/// 	"BCME_BADOPTION",                   /* 3 */
/// 	"BCME_NOTUP",                       /* 4 */
/// ```
///
/// Only the entries actually quoted from the reference are named; anything else prints as its number rather
/// than being guessed at. The two that have been seen on this hardware are both here: `-4` (a scan before
/// the interface was up) and `-24` (`BCME_BADLEN`, which is what the firmware said when a wrong `dataoff`
/// made it read an iovar name as a header).
fn err_name(status: i32) -> &'static str {
    // Indices 0-25, each quoted WITH its index comment from the reference table. Two fetches of that table
    // disagreed by one about where NOTREADY sits (25 or 26), so the three entries past 25 that were here are
    // gone: a wrong name is worse than a number, and the number still prints.
    match -status {
        0 => "BCME_OK",
        1 => "BCME_ERROR",
        2 => "BCME_BADARG",
        3 => "BCME_BADOPTION",
        4 => "BCME_NOTUP - the interface is down; BRCMF_C_UP must be issued first",
        5 => "BCME_NOTDOWN",
        6 => "BCME_NOTAP",
        7 => "BCME_NOTSTA",
        8 => "BCME_BADKEYIDX",
        9 => "BCME_RADIOOFF",
        10 => "BCME_NOTBANDLOCKED",
        11 => "BCME_NOCLK",
        12 => "BCME_BADRATESET",
        13 => "BCME_BADBAND",
        14 => "BCME_BUFTOOSHORT",
        15 => "BCME_BUFTOOLONG",
        16 => "BCME_BUSY",
        17 => "BCME_NOTASSOCIATED",
        18 => "BCME_BADSSIDLEN",
        19 => "BCME_OUTOFRANGECHAN",
        20 => "BCME_BADCHAN",
        21 => "BCME_BADADDR",
        22 => "BCME_NORESOURCE",
        23 => "BCME_UNSUPPORTED - this firmware does not take that command or iovar in that form",
        24 => "BCME_BADLEN",
        25 => "BCME_NOTREADY",
        _ => "(not a code this driver names)",
    }
}

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

/// The two counters an exchange carries, owned by whoever runs it.
///
/// **Both of these were constants, and that was a bug.** `set_cmd` hardcoded `reqid = 2` and SDPCM sequence
/// 0, so every command in the driver shared one identity - which makes id matching useless, and it silently
/// let `interface up` match a reply that was not its own and report success. Observed on hardware: the
/// command logged nothing, then the firmware said the interface was still down.
///
/// Owned and passed rather than global (§3.8, and §3.9 forbids the shortcut). `query_iovar` already took
/// `seq: &mut u8` for this reason; the request id now gets the same treatment.
pub struct Session {
    /// The SDPCM sequence number, one per frame sent.
    seq: u8,
    /// The BCDC request id, one per command, echoed by the firmware in its reply.
    reqid: u16,
    /// Everything counted, for `wifi debug`.
    pub stats: Stats,
    /// The last `TRACE_FRAMES` frames, for `wifi debug trace`.
    pub trace: Trace,
    /// The driver's own clock: the cycle counter at `new`, and cycles per millisecond (0 if the kernel does
    /// not know - then every timestamp reads 0 and says so rather than inventing one).
    t0: u64,
    cycles_per_ms: u64,
}

/// What the control channel and the frame reader have counted since the session began. Raw facts, no
/// verdicts (rule 7); `wifi debug stats` and `wifi debug transport` print them.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub ctrl_sent: u32,
    pub ctrl_accepted: u32,
    pub ctrl_refused: u32,
    pub ctrl_unanswered: u32,
    pub rx_ctrl: u32,
    pub rx_event: u32,
    pub rx_data: u32,
    pub rx_glom: u32,
    pub rx_header_only: u32,
    pub rx_other: u32,
    pub tx_bytes: u32,
    pub rx_bytes: u32,
    /// Frames on the event or data channel that arrived DURING a control wait and were read and skipped
    /// there - the scan's own results, if a control exchange runs mid-sweep. Counted so it is visible.
    pub rx_skipped_in_ctrl_wait: u32,
    /// Event counts by the codes this driver names (`code::index`), plus one bucket for the rest.
    pub events: [u32; EVENT_BUCKETS],
    pub last_event_code: u32,
    pub last_event_status: u32,
    pub last_refused_cmd: u32,
    pub last_refused_status: i32,
}

/// `code::index` maps an event code to one of these; the last is "other".
pub const EVENT_BUCKETS: usize = 10;

/// One frame as the trace remembers it: 16 bytes, so 64 of them are one KiB.
#[derive(Clone, Copy, Default)]
pub struct TraceEntry {
    /// Milliseconds since the session began (0 when the clock is unavailable).
    pub ms: u32,
    /// `trace_kind::*`.
    pub kind: u8,
    /// The SDPCM channel byte as received, or 0 for a send.
    pub chanflag: u8,
    /// The request id (control), or 0.
    pub id: u16,
    /// The command (control) or the event code (event), or 0.
    pub what: u32,
    /// The firmware's status (control reply or event), or 0.
    pub status: i32,
    /// The frame length on the wire.
    pub len: u16,
}

pub mod trace_kind {
    pub const TX_CTRL: u8 = 1;
    pub const RX_CTRL: u8 = 2;
    pub const RX_EVENT: u8 = 3;
    pub const RX_DATA: u8 = 4;
    pub const RX_GLOM: u8 = 5;
    pub const RX_OTHER: u8 = 6;
}

/// How many frames the trace keeps. Fixed; the oldest is overwritten (26.6.1). 64 is two full scans.
pub const TRACE_FRAMES: usize = 64;

/// A ring of the last frames. `entries()` yields oldest first.
pub struct Trace {
    ring: [TraceEntry; TRACE_FRAMES],
    next: usize,
    total: u32,
}

impl Trace {
    fn new() -> Self {
        Trace { ring: [TraceEntry::default(); TRACE_FRAMES], next: 0, total: 0 }
    }
    fn push(&mut self, e: TraceEntry) {
        self.ring[self.next] = e;
        self.next = (self.next + 1) % TRACE_FRAMES;
        self.total = self.total.wrapping_add(1);
    }
    /// How many are held (up to `TRACE_FRAMES`), and how many were ever pushed.
    pub fn held(&self) -> usize {
        core::cmp::min(self.total as usize, TRACE_FRAMES)
    }
    pub fn total(&self) -> u32 {
        self.total
    }
    /// The held entries, oldest first.
    pub fn entry(&self, i: usize) -> TraceEntry {
        let held = self.held();
        let start = if held < TRACE_FRAMES { 0 } else { self.next };
        self.ring[(start + i) % TRACE_FRAMES]
    }
}

impl Session {
    pub fn new(ctx: &ServiceContext) -> Self {
        // brcmf_proto_bcdc_query_dcmd pre-increments, so the first request is 1 rather than 0.
        Session {
            seq: 0,
            reqid: 0,
            stats: Stats::default(),
            trace: Trace::new(),
            t0: ctx.read_tsc(),
            cycles_per_ms: ctx.tsc_ticks_per_10ms() / 10,
        }
    }

    /// Milliseconds since the session began, by the driver's own clock. 0 when the kernel gave no rate.
    pub fn now_ms(&self, ctx: &ServiceContext) -> u32 {
        if self.cycles_per_ms == 0 {
            return 0;
        }
        (ctx.read_tsc().wrapping_sub(self.t0) / self.cycles_per_ms) as u32
    }

    /// A control request went out.
    fn note_tx(&mut self, ctx: &ServiceContext, cmd: u32, id: u16, len: usize) {
        self.stats.ctrl_sent += 1;
        self.stats.tx_bytes = self.stats.tx_bytes.wrapping_add(len as u32);
        self.trace.push(TraceEntry {
            ms: self.now_ms(ctx), kind: trace_kind::TX_CTRL, chanflag: 0, id, what: cmd, status: 0, len: len as u16,
        });
    }

    /// A frame was read. Classified by channel; an event's code and status are read out of it so the trace
    /// can name it. Called by every reader, so the trace is the whole traffic and not one path's view.
    pub fn note_frame(&mut self, ctx: &ServiceContext, f: &Frame, buf: &[u8], in_ctrl_wait: bool) {
        self.stats.rx_bytes = self.stats.rx_bytes.wrapping_add(f.frmlen as u32);
        let channel = f.chanflag & 0x0F;
        let body = &buf[f.off..f.off + f.len];
        let (kind, what, status) = if f.len < DCMD && channel == CHANNEL_CONTROL {
            self.stats.rx_header_only += 1;
            (trace_kind::RX_OTHER, 0, 0)
        } else if channel == CHANNEL_CONTROL {
            self.stats.rx_ctrl += 1;
            let p = 0;
            let rflags = u32::from_le_bytes([body[p + 8], body[p + 9], body[p + 10], body[p + 11]]);
            let st = u32::from_le_bytes([body[p + 12], body[p + 13], body[p + 14], body[p + 15]]) as i32;
            let cmd = u32::from_le_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]);
            let id = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
            self.trace.push(TraceEntry {
                ms: self.now_ms(ctx), kind: trace_kind::RX_CTRL, chanflag: f.chanflag, id, what: cmd, status: st,
                len: f.frmlen,
            });
            return;
        } else if channel == crate::scan::CHANNEL_EVENT {
            self.stats.rx_event += 1;
            if in_ctrl_wait {
                self.stats.rx_skipped_in_ctrl_wait += 1;
            }
            match crate::scan::event_head(body) {
                Some((code, st)) => {
                    let b = crate::scan::code::index(code);
                    self.stats.events[b] += 1;
                    self.stats.last_event_code = code;
                    self.stats.last_event_status = st;
                    (trace_kind::RX_EVENT, code, st as i32)
                }
                None => (trace_kind::RX_EVENT, 0, 0),
            }
        } else if channel == crate::scan::CHANNEL_DATA {
            self.stats.rx_data += 1;
            if in_ctrl_wait {
                self.stats.rx_skipped_in_ctrl_wait += 1;
            }
            (trace_kind::RX_DATA, 0, 0)
        } else if channel == crate::scan::CHANNEL_GLOM {
            self.stats.rx_glom += 1;
            (trace_kind::RX_GLOM, 0, 0)
        } else {
            self.stats.rx_other += 1;
            (trace_kind::RX_OTHER, 0, 0)
        };
        self.trace.push(TraceEntry {
            ms: self.now_ms(ctx), kind, chanflag: f.chanflag, id: 0, what, status, len: f.frmlen,
        });
    }

    /// The reply to our own request was matched, and either accepted or refused.
    fn note_ctrl_reply(&mut self, cmd: u32, refused: bool, status: i32) {
        if refused {
            self.stats.ctrl_refused += 1;
            self.stats.last_refused_cmd = cmd;
            self.stats.last_refused_status = status;
        } else {
            self.stats.ctrl_accepted += 1;
        }
    }

    /// The next request id. Wraps at 16 bits because that is the width of the field; a wrap can only
    /// collide with an exchange 65535 commands old, which no reply outlives.
    fn next_id(&mut self) -> u16 {
        self.reqid = self.reqid.wrapping_add(1);
        self.reqid
    }

    /// The next SDPCM sequence number.
    fn next_seq(&mut self) -> u8 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }
}

/// One frame, bounded - and the bound is the WIRE's, not this driver's convenience.
///
/// ```c
/// #define MAX_RX_DATASZ	2048
/// ```
///
/// **This was 512, and the radio does not care.** With a scan running the firmware sent frames of 584 to
/// 1952 bytes and 17 of them were discarded as an "impossible shape" - which was true of the buffer and not
/// of the frame. An outgoing control message still fits in a fraction of this; what needed the room is
/// everything the firmware sends back, and the escan results are among it.
///
/// Public because the scan path reads frames into a buffer of the same size. Two constants for one wire
/// limit is the duplicated fact the enforcement layer rejects, and rightly.
pub const FRAME: usize = 2048;

/// The frame FIFO's address: function 2, with the window set to chipcommon, offset 0, wide access.
fn frame_offset() -> u32 {
    (CHIPCOMMON_BASE & OFFSET_MASK) | ACCESS_WIDE
}

/// How to ask for a transfer of `padded` bytes: byte mode, or block mode with this many blocks.
///
/// **A byte-mode CMD53 carries at most 512 bytes**, because its count field is nine bits wide and a count of
/// zero means 512. Asking for more produced a malformed transfer - the argument said 512 while `BLKSIZECNT`
/// said 2048, and the controller errored after moving what it had been told. Every control frame was under
/// 512 until the buffer grew, so this surfaced the moment it mattered and not before.
///
/// `round_to` already pads anything over 512 to a multiple of 512 - that is the reference's own rule - so a
/// large frame is block-aligned before it gets here and only needs asking for correctly.
fn transfer_mode(bytes: usize) -> (u32, Option<u32>, usize) {
    if bytes > DATA_BLOCK as usize {
        // ROUNDED UP. Block mode cannot express a partial block, and a frame is not a block multiple: a
        // 1436-byte body is 2.8 blocks, and `bytes / DATA_BLOCK` truncates to 2 and silently loses 412
        // bytes. So this asks for three and the caller uses what it needs.
        let block = DATA_BLOCK as usize;
        let blocks = ((bytes + block - 1) / block) as u32;
        (
            blk_block_mode(blocks, DATA_BLOCK as u32),
            Some(blocks),
            blocks as usize * block,
        )
    } else {
        // ROUNDED UP TO A WORD, for the same reason the block branch rounds to a block: the FIFO moves 32
        // bits at a time, so a transfer is a whole number of words or the argument and BLKSIZECNT disagree.
        // Observed exactly that way - a 30-byte body asked for 28 in the argument and 30 in the block
        // register, and the controller errored before moving anything. This function returning the byte
        // count unrounded is what let its caller truncate `30 / 4` to 7 words.
        let words = (bytes + 3) & !3;
        (blk_byte_mode(words as u32), None, words)
    }
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

/// Ask the firmware for an iovar's value. Returns the reply payload length.
///
/// `name` is the iovar, NUL-terminated on the wire. `out` receives the payload the firmware returns,
/// which for a query begins with the value asked for.
pub fn query_iovar(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    name: &str,
    out: &mut [u8],
    ctx: &ServiceContext,
) -> Option<usize> {
    // The payload of a GET_VAR is the variable's name, NUL-terminated, followed by room for the answer.
    let mut named = [0u8; 64];
    if name.len() + 1 > named.len() {
        ctx.log("wifi-driver: an iovar name longer than this driver's 63-byte bound - not sent");
        return None;
    }
    named[..name.len()].copy_from_slice(name.as_bytes());
    let want = core::cmp::max(name.len() + 1, out.len());
    query_raw(h, w, s, GET_VAR, &named[..name.len() + 1], want, name, out, ctx)
}

/// Ask the firmware a GET COMMAND - `BRCMF_C_GET_BSSID`, `BRCMF_C_GET_RSSI` and their kind - where the
/// payload sent is the structure the answer is written back into (`brcmf_fil_cmd_data_get`). `inout` goes
/// out as sent and comes back overwritten. Returns the reply payload length.
pub fn query_cmd(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    cmd: u32,
    inout: &mut [u8],
    what: &str,
    ctx: &ServiceContext,
) -> Option<usize> {
    let mut sent = [0u8; 64];
    if inout.len() > sent.len() {
        ctx.log("wifi-driver: a GET command with more than 64 bytes of structure - not sent");
        return None;
    }
    sent[..inout.len()].copy_from_slice(inout);
    let n = inout.len();
    query_raw(h, w, s, cmd, &sent[..n], n, what, inout, ctx)
}

/// Send one BCDC GET request - any command, any payload - and read its reply. Returns the reply payload
/// length. `payload` is what goes out after the 16-byte dcmd header; `want` is the room the firmware is told
/// it has for the answer, which lands in `out`. The two GET forms above are thin wrappers over this.
fn query_raw(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    cmd: u32,
    payload: &[u8],
    want: usize,
    name: &str,
    out: &mut [u8],
    ctx: &ServiceContext,
) -> Option<usize> {
    let reqid = s.next_id();
    let mut frame = [0u8; FRAME];

    let want = core::cmp::max(want, payload.len());
    if PAYLOAD_AT + want > FRAME {
        ctx.log("wifi-driver: the control frame would not fit its bounded buffer");
        return None;
    }
    frame[PAYLOAD_AT..PAYLOAD_AT + payload.len()].copy_from_slice(payload);

    let len = PAYLOAD_AT + want;
    let padded = {
        let r = round_to(len);
        (len + r - 1) / r * r
    };

    // ---- Hardware header: the length and its complement, which is how the receiver validates it. ----
    frame[0..2].copy_from_slice(&(len as u16).to_le_bytes());
    frame[2..4].copy_from_slice(&(!(len as u16)).to_le_bytes());

    // ---- Software header. `dataoff` points AT the BCDC header - past the SDIO headers and no
    // further - which is what `sizeof(*hwhdr) + sizeof(*swhdr)` means. This comment used to say
    // "past all three headers" and cite the reference for it, while the reference says two. ----
    frame[4] = s.next_seq();
    frame[5] = CHANNEL_CONTROL;
    frame[6] = 0; // nextlen: a hint, and zero means "no hint"
    // `dataoff` points AT the BCDC header, not past it. See `DATA_OFF`.
    frame[7] = DATA_OFF as u8;
    // flowctl, maxseqnr and res0 stay zero - they are the CHIP's fields on receive, not the host's on send.

    // ---- BCDC command header. ----
    let flags = (reqid as u32) << DCMD_ID_SHIFT; // no SET bit: this is a get. Interface index 0.
    frame[HWHDR + SWHDR..HWHDR + SWHDR + 4].copy_from_slice(&cmd.to_le_bytes());
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
    // Not logged per request any more: `wifi debug trace` holds every send with its id, command and
    // length, and a refusal or a silence is still logged below. The log used to carry a line per frame,
    // which is what the trace ring exists to replace.
    let _ = name;
    // The third value is what the controller will actually move. For a write `round_to` has already
    // padded to a block multiple, so it equals `padded` - guaranteed by one function now rather than
    // assumed by two.
    let (blk, blocks, _moved) = transfer_mode(padded);
    if !sdio::write_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut wbuf[..words],
        blk,
        blocks,
        ctx,
    ) {
        return None;
    }
    s.note_tx(ctx, cmd, reqid, len);

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
            Some(f) => {
                frames += 1;
                s.note_frame(ctx, &f, &rbuf, true);
                // The first frames used to be described here in full (`describe_frame`), which found the
                // dataoff and header-only bugs of sections 31-33. The trace now records every frame's
                // channel, id, command and status; the full dump is kept for a bring-up that needs it.
                const DUMP_FRAMES: u32 = 0;
                if frames <= DUMP_FRAMES {
                    describe_frame(frames, &f, &rbuf, ctx);
                }
                let len = f.len;
                let p = f.off;
                if f.chanflag & 0x0F != CHANNEL_CONTROL {
                    // An event or a data frame. Not this exchange's business.
                    other_channel += 1;
                    continue;
                }
                if len < DCMD {
                    // Header-only, or too short to carry a BCDC header. Flow control, not an answer.
                    headers_only += 1;
                    continue;
                }
                let rflags = u32::from_le_bytes([rbuf[p + 8], rbuf[p + 9], rbuf[p + 10], rbuf[p + 11]]);
                let status = u32::from_le_bytes([rbuf[p + 12], rbuf[p + 13], rbuf[p + 14], rbuf[p + 15]]);
                let rid = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
                if rid != reqid {
                    // ANOTHER EXCHANGE'S ANSWER. Skipped rather than parsed, and skipped rather than
                    // treated as fatal: matching ids is what stops a protocol going one reply out of step,
                    // and a stale reply arriving late is exactly what that guards against.
                    wrong_id += 1;
                    continue;
                }
                if rflags & DCMD_ERROR != 0 {
                    s.note_ctrl_reply(cmd, true, status as i32);
                    ctx.log_fmt(format_args!(
                        "wifi-driver: the firmware REFUSED the request - {} (status {})",
                        err_name(status as i32),
                        status as i32
                    ));
                    return None;
                }
                s.note_ctrl_reply(cmd, false, status as i32);
                // The payload follows the 16-byte BCDC header. `read_frame` has already applied `dataoff`.
                let avail = len - DCMD;
                let n = core::cmp::min(avail, out.len());
                out[..n].copy_from_slice(&rbuf[p + DCMD..p + DCMD + n]);
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
    s.stats.ctrl_unanswered += 1;
    ctx.log_fmt(format_args!(
        "wifi-driver: no reply to `{}` across {} reads. {} frame(s) DID arrive: {} header-only (flow \
         control), {} on another channel, {} from another exchange - so \"nothing answered\" and \"nothing \
         MATCHED\" are told apart here rather than left to guess",
        name, REPLY_TRIES, frames, headers_only, other_channel, wrong_id
    ));
    None
}

/// What one received frame's headers actually said.
///
/// `read_frame` used to return `(chanflag, len)`, which is the two facts its callers needed and none of the
/// four that decide where the payload starts. A frame whose header was surprising therefore produced a
/// surprising slice with no way to see why - the state the section 32 boot ended in. The reader now reports
/// what it read and the caller decides what to do with it.
pub struct Frame {
    /// The software header's channel byte, unmasked.
    pub chanflag: u8,
    /// The sequence number the chip put on it.
    pub seq: u8,
    /// The chip's hint at the next frame's length, in 16-byte units (`swhdr->nextlen << 4`). 0 means none.
    pub nextlen: u8,
    /// `hwhdr->frmlen` - the WHOLE frame including its 12 bytes of headers.
    pub frmlen: u16,
    /// `swhdr->dataoff` - where the payload starts, measured from the start of the whole frame.
    pub dataoff: u8,
    /// How many bytes of body arrived: `frmlen - 12`. `buf[..body]` is all of it.
    pub body: usize,
    /// Where the payload starts within `buf`: `dataoff - 12`.
    pub off: usize,
    /// How many payload bytes there are: `body - off`.
    pub len: usize,
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
fn describe_frame(which: u32, f: &Frame, buf: &[u8], ctx: &ServiceContext) {
    let channel = f.chanflag & 0x0F;
    let kind = match channel {
        CHANNEL_CONTROL => "CONTROL",
        1 => "EVENT",
        2 => "DATA",
        3 => "GLOM",
        _ => "(unknown channel)",
    };
    // THE HEADER FIELDS FIRST, because they are what decide every offset below and were invisible.
    ctx.log_fmt(format_args!(
        "wifi-driver:   frame {}: channel {:#04x} ({}), frmlen {}, dataoff {}, seq {}, nextlen {} -> {} \
         byte(s) of body, payload at +{} for {} byte(s)",
        which, f.chanflag, kind, f.frmlen, f.dataoff, f.seq, f.nextlen, f.body, f.off, f.len
    ));
    if f.dataoff as usize != HWHDR + SWHDR {
        // Worth saying out loud: a reply whose payload does not start right after the headers is the case
        // this driver has never seen, and the one that would explain a slice nobody can account for.
        ctx.log_fmt(format_args!(
            "wifi-driver:     NOTE dataoff is {}, not the {} a request uses - so the payload does NOT \
             start immediately after the SDPCM headers",
            f.dataoff,
            HWHDR + SWHDR
        ));
    }
    let len = f.len;
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
    // the header starts elsewhere, `cmd 262` (`06 01 00 00`) will be visible at some other position here.
    //
    // THE WHOLE BODY, from frame byte 12 - not the payload slice. The bytes before `dataoff` used to be
    // discarded inside the reader before anyone could look at them, which is how a frame that made no sense
    // stayed that way for a boot. Offsets shown are from the start of the body, so `dataoff - 12` is where
    // the payload is claimed to begin.
    let show = core::cmp::min(f.body, 48);
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
) -> Option<Frame> {
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
        // SAY WHICH. A glommed frame (channel 3) over the buffer is a legitimate frame this driver chooses
        // not to read - the reference drops them too - and calling it "impossible" was a lie the log told on
        // every scan. Anything else over the buffer, or with a dataoff outside the frame, really is a shape
        // this driver cannot account for.
        if chanflag & 0x0F == 3 && frmlen > FRAME {
            ctx.log_fmt(format_args!(
                "wifi-driver: a {} byte glommed frame (channel 3) exceeds this driver's {} byte buffer and \
                 is not read - the reference drops glommed frames too, and the scan works without them",
                frmlen, FRAME
            ));
        } else {
            ctx.log_fmt(format_args!(
                "wifi-driver: a frame validated its checksum but describes an impossible shape - frmlen {}, \
                 dataoff {}, channel {:#04x} (headers are {}, buffer is {})",
                frmlen, dataoff, chanflag, HWHDR + SWHDR, FRAME
            ));
        }
        return None;
    }

    let rest = frmlen - (HWHDR + SWHDR);
    if rest == 0 {
        // A header-only frame is legitimate - the chip uses them for flow control - and carries no payload.
        return Some(Frame {
            chanflag,
            seq: b1[0],
            nextlen: b1[2],
            frmlen: frmlen as u16,
            dataoff: dataoff as u8,
            body: 0,
            off: 0,
            len: 0,
        });
    }
    // BLOCK MODE OVER 512 BYTES, rounded up. A frame body is not a block multiple, so this may read more
    // than the frame holds; only `rest` bytes are used. Byte mode here carried 1436 bytes with a nine-bit
    // count field and the controller errored without ever starting a data phase.
    let (blk, blocks, moved) = transfer_mode(rest);
    let words = moved / 4;
    if words > FRAME / 4 {
        ctx.log_fmt(format_args!(
            "wifi-driver: a {} byte frame body rounds to {} bytes, over this driver's {} byte buffer",
            rest, moved, FRAME
        ));
        return None;
    }
    let mut rbuf = [0u32; FRAME / 4];
    if !w.set_for(h, CHIPCOMMON_BASE, ctx) {
        return None;
    }
    if !sdio::read_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut rbuf[..words],
        blk,
        blocks,
        ctx,
    ) {
        return None;
    }
    let mut body = [0u8; FRAME];
    for i in 0..words {
        body[i * 4..i * 4 + 4].copy_from_slice(&rbuf[i].to_le_bytes());
    }
    // Everything past `rest` is over-read from rounding to a whole block and is not part of this frame.
    // THE WHOLE BODY GOES IN `buf`, and the Frame says where the payload is inside it. The reader used to
    // slice here and hand back only the payload, discarding the bytes before `dataoff` - which are exactly
    // the ones needed to explain a frame that does not parse.
    //
    // `flen = hwhdr->frmlen - (sizeof(*hwhdr) + sizeof(*swhdr))` and
    // `off = swhdr->dataoff - (sizeof(*hwhdr) + sizeof(*swhdr))`, both quoted from the reference.
    let off = dataoff - (HWHDR + SWHDR);
    buf[..rest].copy_from_slice(&body[..rest]);
    Some(Frame {
        chanflag,
        seq: b1[0],
        nextlen: b1[2],
        frmlen: frmlen as u16,
        dataoff: dataoff as u8,
        body: rest,
        off,
        len: rest - off,
    })
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
    s: &mut Session,
    name: &str,
    value: &[u8],
    ctx: &ServiceContext,
) -> bool {
    // An iovar SET is `BRCMF_C_SET_VAR` whose payload is the name, a NUL, then the value. It is one shape of
    // BCDC command among several, so it builds its payload and hands it to `set_cmd` - which is the only
    // place the frame layout lives. A third copy of that layout is how `set_iovar` inherited the `dataoff`
    // bug from `query_iovar` in the first place.
    let mut payload = [0u8; FRAME];
    let n = name.len() + 1 + value.len();
    if PAYLOAD_AT + n > FRAME {
        ctx.log_fmt(format_args!(
            "wifi-driver: setting `{}` would need {} payload bytes, over this driver's {} byte frame",
            name, n, FRAME
        ));
        return false;
    }
    payload[..name.len()].copy_from_slice(name.as_bytes());
    // The NUL is already in place - the buffer is zeroed.
    let at = name.len() + 1;
    payload[at..at + value.len()].copy_from_slice(value);
    ctx.log_fmt(format_args!(
        "wifi-driver: setting `{}` - {} byte value", name, value.len()
    ));
    set_cmd(h, w, s, SET_VAR, &payload[..n], name, ctx)
}

/// Send one BCDC command with a payload, and wait for the firmware to accept or refuse it.
///
/// `what` names the thing being done, for the log only. A plain command like `BRCMF_C_UP` has no payload;
/// an iovar set carries its name and value, built by `set_iovar`.
///
/// **The reply decides.** A command whose refusal is discarded is a silent failure (§26.7), and these
/// commands are the ones that put the radio into a state - so "accepted" has to mean the firmware said so.
pub fn set_cmd(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    cmd: u32,
    value: &[u8],
    what: &str,
    ctx: &ServiceContext,
) -> bool {
    let mut frame = [0u8; FRAME];
    let payload = value.len();
    if PAYLOAD_AT + payload > FRAME {
        ctx.log_fmt(format_args!(
            "wifi-driver: `{}` would need {} payload bytes, over this driver's {} byte frame",
            what, payload, FRAME
        ));
        return false;
    }
    frame[PAYLOAD_AT..PAYLOAD_AT + payload].copy_from_slice(value);

    let len = PAYLOAD_AT + payload;
    let r = round_to(len);
    let padded = (len + r - 1) / r * r;
    // ONE ID PER COMMAND. This was the constant 2 for every command in the driver, which made a reply
    // unattributable and let `interface up` report a success it never received.
    let reqid = s.next_id();

    frame[0..2].copy_from_slice(&(len as u16).to_le_bytes());
    frame[2..4].copy_from_slice(&(!(len as u16)).to_le_bytes());
    frame[4] = s.next_seq();
    frame[5] = CHANNEL_CONTROL;
    // `dataoff` points AT the BCDC header, not past it. See `DATA_OFF`.
    frame[7] = DATA_OFF as u8;
    let flags = ((reqid as u32) << DCMD_ID_SHIFT) | DCMD_SET;
    frame[HWHDR + SWHDR..HWHDR + SWHDR + 4].copy_from_slice(&cmd.to_le_bytes());
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
    // Not logged per command: the trace holds the send with its id, command and length. A refusal or a
    // silence is still logged below, loudly.
    // The third value is what the controller will actually move. For a write `round_to` has already
    // padded to a block multiple, so it equals `padded` - guaranteed by one function now rather than
    // assumed by two.
    let (blk, blocks, _moved) = transfer_mode(padded);
    if !sdio::write_extended(
        h,
        DATA_FUNC,
        frame_offset(),
        &mut wbuf[..words],
        blk,
        blocks,
        ctx,
    ) {
        return false;
    }
    s.note_tx(ctx, cmd, reqid, len);

    // THE REPLY DECIDES. A set whose refusal is discarded is a silent failure, and this one starts a scan -
    // so "accepted" has to mean the firmware said so.
    const SET_TRIES: u32 = 200;
    let mut rbuf = [0u8; FRAME];
    for _ in 0..SET_TRIES {
        if let Some(f) = read_frame(h, w, &mut rbuf, ctx) {
            s.note_frame(ctx, &f, &rbuf, true);
            if f.chanflag & 0x0F != CHANNEL_CONTROL || f.len < DCMD {
                continue;
            }
            let p = f.off;
            let rflags = u32::from_le_bytes([rbuf[p + 8], rbuf[p + 9], rbuf[p + 10], rbuf[p + 11]]);
            let status = u32::from_le_bytes([rbuf[p + 12], rbuf[p + 13], rbuf[p + 14], rbuf[p + 15]]);
            let rid = ((rflags & DCMD_ID_MASK) >> DCMD_ID_SHIFT) as u16;
            if rid != reqid {
                continue;
            }
            if rflags & DCMD_ERROR != 0 {
                s.note_ctrl_reply(cmd, true, status as i32);
                ctx.log_fmt(format_args!(
                    "wifi-driver: the firmware REFUSED `{}` - {} (status {})",
                    what,
                    err_name(status as i32),
                    status as i32
                ));
                return false;
            }
            s.note_ctrl_reply(cmd, false, status as i32);
            // Success is not logged per command any more: the matched id, command and status are in the
            // trace, a better record than a line per exchange. "Silent success" is not the failure this
            // used to guard against - that was a command matched to the WRONG reply, and the id match above
            // is what prevents it; the trace shows the match.
            return true;
        }
        ctx.sleep_ms(1);
    }
    s.stats.ctrl_unanswered += 1;
    ctx.log_fmt(format_args!(
        "wifi-driver: `{}` was sent and the firmware never acknowledged it across {} reads. It is NOT \
         reported as done, because something nobody confirmed is indistinguishable from something refused",
        what, SET_TRIES
    ));
    false
}

/// Raise the interface - `BRCMF_C_UP`, no payload.
///
/// Without this the firmware refuses a scan with `BCME_NOTUP` (-4), which is exactly what it did. brcmfmac
/// issues this during bring-up before anything else touches the radio.
/// `BWFM_C_DOWN` - take the interface down. `bwfm_stop`: `bwfm_fwvar_cmd_set_int(sc, BWFM_C_DOWN, 1)`.
const CMD_DOWN: u32 = 3;
/// `BWFM_C_DISASSOC` - leave the network. `bwfm_newstate`, on the way back to SCAN:
/// `bwfm_fwvar_cmd_set_data(sc, BWFM_C_DISASSOC, NULL, 0)` - no payload at all.
const CMD_DISASSOC: u32 = 52;

/// `BRCMF_C_GET_BSSID` - the access point's address, 6 bytes; all zeros when not associated
/// (`brcmf_fil_cmd_data_get(ifp, BRCMF_C_GET_BSSID, bssid, ETH_ALEN)`).
const CMD_GET_BSSID: u32 = 23;
/// `BRCMF_C_GET_RSSI` - `brcmf_scb_val_le { __le32 val; u8 ea[6]; }`, sent with `val` 0 and `ea` zero for
/// our own station; the firmware writes the RSSI, in dBm, into `val`.
const CMD_GET_RSSI: u32 = 127;

/// What the firmware says about the link RIGHT NOW - not what this driver remembers of its last join.
pub struct Link {
    /// The access point, or all zeros when the radio is not associated.
    pub bssid: [u8; 6],
    /// dBm. Meaningful only when associated.
    pub rssi: i32,
    /// The `chanspec` iovar: band in bits 15:14, channel in the low 8.
    pub chanspec: u16,
}

impl Link {
    pub fn associated(&self) -> bool {
        self.bssid.iter().any(|&b| b != 0)
    }
}

/// Three GETs that together are the truth of the link: BSSID, RSSI, chanspec. `None` only if the firmware
/// would not answer the first; the other two degrade to zero with a log line, since an address with no
/// signal reading is still an association.
pub fn link_now(h: &Host, w: &mut Window, s: &mut Session, ctx: &ServiceContext) -> Option<Link> {
    let mut bssid = [0u8; 6];
    query_cmd(h, w, s, CMD_GET_BSSID, &mut bssid, "bssid", ctx)?;
    let mut link = Link { bssid, rssi: 0, chanspec: 0 };
    if !link.associated() {
        return Some(link);
    }
    let mut scb = [0u8; 10];
    match query_cmd(h, w, s, CMD_GET_RSSI, &mut scb, "rssi", ctx) {
        Some(n) if n >= 4 => link.rssi = i32::from_le_bytes([scb[0], scb[1], scb[2], scb[3]]),
        _ => ctx.log("wifi-driver: the firmware gave no RSSI - reported as 0"),
    }
    let mut cs = [0u8; 4];
    match query_iovar(h, w, s, "chanspec", &mut cs, ctx) {
        Some(n) if n >= 2 => link.chanspec = u16::from_le_bytes([cs[0], cs[1]]),
        _ => ctx.log("wifi-driver: the firmware gave no chanspec - reported as 0"),
    }
    Some(link)
}

/// Leave whatever network the radio is on. The reference sends the bare command with nothing after it, so
/// so does this; the radio stays up and can scan or join again at once.
pub fn disassoc(h: &Host, w: &mut Window, s: &mut Session, ctx: &ServiceContext) -> bool {
    set_cmd(h, w, s, CMD_DISASSOC, &[], "disassociate", ctx)
}

/// Power the radio down. `interface_up` is its opposite and re-runs the whole UP chain, which is what
/// `bwfm_stop` does too (DOWN, then UP again with the mode commands between).
pub fn radio_down(h: &Host, w: &mut Window, s: &mut Session, ctx: &ServiceContext) -> bool {
    set_cmd_int(h, w, s, CMD_DOWN, 1, "down", ctx)
}

pub fn interface_up(h: &Host, w: &mut Window, s: &mut Session, ctx: &ServiceContext) -> bool {
    // UP takes the VALUE 0, which reads oddly and is what the reference passes.
    if !set_cmd_int(h, w, s, CMD_UP, 0, "interface up", ctx) {
        return false;
    }
    // Infrastructure mode on, access-point mode off: a station.
    if !set_cmd_int(h, w, s, CMD_SET_INFRA, 1, "infrastructure mode", ctx) {
        return false;
    }
    if !set_cmd_int(h, w, s, CMD_SET_AP, 0, "access-point mode off", ctx) {
        return false;
    }
    // `BWFM_C_SET_PM` (power management) is deliberately NOT sent: it is an optimisation of something that
    // does not work yet (§26.2), and its value depends on a policy this driver has not got.
    true
}

/// Send a firmware command whose payload is one little-endian 32-bit integer.
///
/// ```c
/// data = htole32(data);
/// return bwfm_fwvar_cmd_set_data(sc, cmd, &data, sizeof(data));
/// ```
///
/// **Four bytes, not none.** `BRCMF_C_UP` was sent here with a zero-byte payload, and the firmware accepted a
/// well-formed command carrying no value and did nothing with it - accepted, status 0, no effect, and a scan
/// still refused with `BCME_NOTUP`. An integer command without its integer is not the command.
pub fn set_cmd_int(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    cmd: u32,
    value: u32,
    what: &str,
    ctx: &ServiceContext,
) -> bool {
    set_cmd(h, w, s, cmd, &value.to_le_bytes(), what, ctx)
}

/// Download a blob to the running firmware through an iovar, in chunks.
///
/// This is how the CLM regulatory data reaches the chip - not written into RAM like the firmware image, but
/// handed to the firmware that is already running.
///
/// ```c
/// err = brcmf_c_download_blob(ifp, fw->data, fw->size, "clmload", "clmload_status");
///
/// dload_buf->flag = cpu_to_le16(flag);
/// dload_buf->dload_type = cpu_to_le16(DL_TYPE_CLM);
/// dload_buf->len = cpu_to_le32(len);
/// dload_buf->crc = cpu_to_le32(0);
/// ```
///
/// ```c
/// struct brcmf_dload_data_le {
/// 	__le16 flag;  __le16 dload_type;  __le32 len;  __le32 crc;  u8 data[];
/// };
/// #define DL_BEGIN	0x0002
/// #define DL_END		0x0004
/// #define DL_TYPE_CLM	2
/// #define MAX_CHUNK_LEN	1400
/// ```
///
/// **The CRC is explicitly zero, not computed.** Worth quoting, because computing one is the obvious wrong
/// guess and the firmware would reject every chunk.
///
/// **Chunked to this driver's frame rather than to `MAX_CHUNK_LEN`.** 1400 exceeds the 512-byte control
/// frame, and the reference's constant is its buffer's limit rather than the protocol's - a chunked download
/// is chunked either way. `DL_BEGIN` marks the first chunk and `DL_END` the last; a blob small enough for one
/// chunk carries both, which is what the reference does too.
pub fn download_blob(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    iovar: &str,
    dtype: u16,
    blob: &[u8],
    ctx: &ServiceContext,
) -> bool {
    /// `sizeof(struct brcmf_dload_data_le)` without its trailing data.
    const DLOAD_HDR: usize = 12;
    /// `BWFM_DLOAD_FLAG_BEGIN`, `(1 << 1)` - brcmfmac names the same value `DL_BEGIN` (0x0002).
    const DL_BEGIN: u16 = 1 << 1;
    /// `BWFM_DLOAD_FLAG_END`, `(1 << 2)` - brcmfmac names the same value `DL_END` (0x0004).
    const DL_END: u16 = 1 << 2;
    /// `BWFM_DLOAD_FLAG_HANDLER_VER_1`, `(1 << 12)`, with `BWFM_DLOAD_FLAG_HANDLER_VER_MASK` being
    /// `(0xf << 12)` - so the flag's high nibble is a PROTOCOL VERSION, not a spare bit.
    ///
    /// **Set on every chunk, and this driver set it on none.** With the version zero the firmware cannot
    /// interpret the rest of the header, which is why `clmload` came back with a generic `BCME_ERROR`
    /// rather than a specific complaint.
    ///
    /// This is `arch/CLAUDE.md`'s rule being broken by the person who wrote it: *never assemble a register
    /// value out of bit names; copy the one the reference writes.* brcmfmac's
    /// `dload_buf->flag = cpu_to_le16(flag)` takes `flag` as a PARAMETER, and rather than trace where that
    /// parameter was built I assembled a value from the two constants a `#define` list happened to name. A
    /// constants list says what exists, not what is required.
    const DL_HANDLER_VER_1: u16 = 1 << 12;

    /// `MAX_CHUNK_LEN` in brcmfmac, `BWFM_DLOAD_MAX_LEN` in OpenBSD - the same 1400 in both.
    ///
    /// **This is the PROTOCOL's limit, not a buffer size.** The previous version of this function argued
    /// that 1400 was "the reference's buffer limit rather than the protocol's" and chunked to whatever the
    /// frame allowed; with a 2048-byte frame that became 2000-byte chunks and the firmware refused them.
    ///
    /// Two codebases that share no source do not pick the same buffer size by coincidence - their agreement
    /// WAS the evidence, and it was reasoned past. The same mistake as assembling the download flag from bit
    /// names: preferring an inference to what the sources say.
    const MAX_CHUNK: usize = 1400;

    // What is left of a frame once the SDPCM and BCDC headers, the iovar name and the download header have
    // taken their share, and never more than the protocol allows. Rounded down to four so every chunk is a
    // whole number of words.
    let room = core::cmp::min(
        FRAME - PAYLOAD_AT - (iovar.len() + 1) - DLOAD_HDR,
        MAX_CHUNK,
    ) & !3;
    if room == 0 {
        ctx.log_fmt(format_args!(
            "wifi-driver: `{}` leaves no room for blob data in a {} byte frame", iovar, FRAME
        ));
        return false;
    }

    ctx.log_fmt(format_args!(
        "wifi-driver: downloading {} bytes to `{}` in chunks of up to {}",
        blob.len(), iovar, room
    ));

    let mut buf = [0u8; FRAME];
    let mut off = 0usize;
    let mut chunks = 0u32;
    while off < blob.len() {
        let n = core::cmp::min(room, blob.len() - off);
        // THE HANDLER VERSION FIRST, on every chunk, exactly as the reference does.
        let mut flag = DL_HANDLER_VER_1;
        if off == 0 {
            flag |= DL_BEGIN;
        }
        if off + n == blob.len() {
            flag |= DL_END;
        }
        buf[0..2].copy_from_slice(&flag.to_le_bytes());
        buf[2..4].copy_from_slice(&dtype.to_le_bytes());
        buf[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        // ZERO, quoted: `dload_buf->crc = cpu_to_le32(0);`
        buf[8..12].copy_from_slice(&0u32.to_le_bytes());
        buf[DLOAD_HDR..DLOAD_HDR + n].copy_from_slice(&blob[off..off + n]);

        if !set_iovar(h, w, s, iovar, &buf[..DLOAD_HDR + n], ctx) {
            ctx.log_fmt(format_args!(
                "wifi-driver: `{}` failed {} bytes in, on chunk {} of {} bytes (flag {:#06x})",
                iovar, off, chunks + 1, n, flag
            ));
            return false;
        }
        chunks += 1;
        off += n;
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: `{}` accepted all {} bytes in {} chunk(s)", iovar, blob.len(), chunks
    ));
    true
}

/// `DL_TYPE_CLM`.
pub const DL_TYPE_CLM: u16 = 2;

/// Ask the firmware to send the events this driver needs. Without this it sends NONE.
///
/// Both references do the same three steps - read the mask, set bits in it, write it back:
///
/// ```c
/// if (bwfm_fwvar_var_set_data(sc, "event_msgs", evmask, sizeof(evmask)))
/// ```
///
/// **Read before write, deliberately.** It is what the references do, and it means the mask's length comes
/// from the firmware rather than from a constant this driver would have to guess (`BWFM_EVENT_MASK_LEN` is
/// `roundup(BWFM_E_LAST, 8) / 8`, and `BWFM_E_LAST` is not a number I have). A bit is
/// `mask[code / 8] |= 1 << (code % 8)`.
///
/// This is why the event channel has produced nothing so far, and it would have kept a correctly-accepted
/// scan silent.
pub fn enable_events(
    h: &Host,
    w: &mut Window,
    s: &mut Session,
    codes: &[u32],
    ctx: &ServiceContext,
) -> bool {
    // 24 bytes covers 192 event codes, which is past every code this driver names. The firmware answers with
    // its own length and only that much is written back.
    let mut mask = [0u8; 24];
    let n = match query_iovar(h, w, s, "event_msgs", &mut mask, ctx) {
        Some(n) if n > 0 => n,
        _ => {
            ctx.log(
                "wifi-driver: could not read `event_msgs`, so the firmware's event mask is unknown and is \
                 NOT overwritten - guessing its length could disable events that already work",
            );
            return false;
        }
    };
    for &c in codes {
        let byte = (c / 8) as usize;
        if byte >= n {
            ctx.log_fmt(format_args!(
                "wifi-driver: event {} needs byte {} of the mask but the firmware's mask is {} byte(s) - not \
                 enabling it",
                c, byte, n
            ));
            continue;
        }
        mask[byte] |= 1 << (c % 8);
    }
    ctx.log_fmt(format_args!(
        "wifi-driver: enabling {} event(s) in the firmware's {}-byte event mask",
        codes.len(), n
    ));
    set_iovar(h, w, s, "event_msgs", &mask[..n], ctx)
}

/// Ask the firmware for its own MAC address - the first thing only a running radio can answer.
///
/// Returns true when a plausible address came back. All-zero and all-`0xFF` are rejected: both are what a
/// successful exchange that returned nothing looks like, and reporting one as the radio's address would be
/// exactly the silent wrong answer this driver keeps being built to avoid.
pub fn report_mac(h: &Host, w: &mut Window, ctx: &ServiceContext) -> bool {
    ctx.log("wifi-driver: stage 13 - the first question put to the firmware");

    let mut session = Session::new(ctx);
    let mut mac = [0u8; 6];
    let n = match query_iovar(h, w, &mut session, "cur_etheraddr", &mut mac, ctx) {
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

/// The firmware's own account of itself, asked the way Linux asks before it decides what the HOST must do.
///
/// Three GETs, at the point `brcmf_feat_attach` runs - after the preinit commands, before `UP`:
/// - `ver`: the version string of the code RUNNING on the radio. `strings` finds `7.45.265 (28bca26 CY)`
///   in the image; this is the same fact from the other side of the upload.
/// - `cap`: the capability words `brcmf_feat_firmware_capabilities` matches against its table (`mbss`,
///   `p2p`, `sae `, `idauth`, ...). Long - a few hundred bytes - so it is printed in pieces.
/// - `sup_wpa`: `brcmf_feat_iovar_int_get(ifp, BRCMF_FEAT_FWSUP, "sup_wpa")`. Linux's rule, quoted from
///   `feature.c`: the feature is present iff this GET does not come back `BCME_UNSUPPORTED`. When it is
///   present the firmware runs the WPA2 4-way handshake itself and the host only supplies the passphrase;
///   when it is absent the host must run the handshake (Linux: wpa_supplicant; OpenBSD: net80211, which
///   in fact sets `sup_wpa 0` on purpose and does it itself) and install the keys with `wsec_key`.
///
/// Why this exists: two SETs of `sup_wpa` - plain on 2026-09-28, `bsscfg:` form on 2026-09-29 - were
/// refused -23 on this board, while Pi OS on the same chip family has the feature ON (its users turn it
/// off with `feature_disable=0x2000`). So either this build lacks it or the SET differs from the GET Linux
/// rests on. This asks the exact question, and prints the answer rather than deciding anything from it.
pub fn report_firmware(h: &Host, w: &mut Window, s: &mut Session, ctx: &ServiceContext) {
    /// Print a firmware string in pieces `log_fmt`'s fixed buffer can hold.
    fn log_text(label: &str, buf: &[u8], ctx: &ServiceContext) {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let text = &buf[..end];
        if text.is_empty() {
            ctx.log_fmt(format_args!("wifi-driver:   `{}` answered with an empty string", label));
            return;
        }
        const PIECE: usize = 96;
        let mut at = 0;
        while at < text.len() {
            let to = core::cmp::min(at + PIECE, text.len());
            match core::str::from_utf8(&text[at..to]) {
                Ok(t) => ctx.log_fmt(format_args!("wifi-driver:   {} {}", label, t.trim_end())),
                Err(_) => ctx.log_fmt(format_args!(
                    "wifi-driver:   {} bytes {}..{} are not text",
                    label, at, to
                )),
            }
            at = to;
        }
    }

    ctx.log("wifi-driver: asking the firmware what it is, before deciding what the host must do");

    let mut ver = [0u8; 128];
    match query_iovar(h, w, s, "ver", &mut ver, ctx) {
        Some(_) => log_text("ver:", &ver, ctx),
        None => ctx.log("wifi-driver:   no version string - the exchange itself failed, logged above"),
    }

    let mut cap = [0u8; 512];
    match query_iovar(h, w, s, "cap", &mut cap, ctx) {
        Some(_) => log_text("cap:", &cap, ctx),
        None => ctx.log("wifi-driver:   no capability string - the exchange itself failed, logged above"),
    }

    let mut sup = [0u8; 4];
    match query_iovar(h, w, s, "sup_wpa", &mut sup, ctx) {
        Some(n) if n >= 4 => ctx.log_fmt(format_args!(
            "wifi-driver:   `sup_wpa` GET answered {} - by Linux's rule this firmware HAS an internal \
             supplicant, so the two refused SETs were about the SET, not the feature",
            u32::from_le_bytes(sup)
        )),
        Some(n) => ctx.log_fmt(format_args!(
            "wifi-driver:   `sup_wpa` GET answered with {} byte(s) where an int is 4 - accepted, but the \
             value is not readable",
            n
        )),
        None => ctx.log(
            "wifi-driver:   `sup_wpa` GET refused - by Linux's rule (`brcmf_feat_iovar_int_get`, feature.c) \
             this firmware has NO internal supplicant. The host must run the WPA2 4-way handshake and \
             install the keys with `wsec_key`, as OpenBSD's net80211 does on this same chip",
        ),
    }
}
