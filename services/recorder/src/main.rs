// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
#![no_main]
//! `recorder` - drains the `events` log to a file, so a capture survives the screen scrolling away.
//!
//! **Why this is not part of `events`.** Writing to disk BLOCKS: a file write is a request/reply to
//! `fs` and the caller waits for the answer. `events` is a single-threaded recv loop, and while it
//! waits it is not draining its endpoint - so every service's log copies, trace events and metrics
//! pile into its 16-deep queue and are dropped. On a healthy disk that is milliseconds; on a SICK one
//! it is the full deadline, repeatedly, which is exactly when the interesting events are happening.
//! `events` would go blind at the moment it is most needed.
//!
//! This service has the same blocking problem and it does not matter, because nothing depends on it.
//! When the disk is slow `recorder` stalls, `events` keeps its window, and the operator can still read
//! what happened. The dependency points the right way: `recorder` needs `events` and `fs`, and neither
//! of them needs `recorder`.
//!
//! **Spawned on demand**, never at boot, and deliberately NOT restarted on death. It is absent from
//! the kernel's managed-service lists, which is what keeps this feature a zero-kernel-change one. A
//! respawned recorder would not know its target path, so it would be alive and writing nothing while
//! `status` said "running" - worse than dead. Instead the capture opens with a header line and closes
//! with a footer, so a file with no footer says plainly that it died.
//!
//! **Bounded by construction.** `fs` allocates a file's whole extent up front (`OP_WRITE_NEW`), so a
//! capture has a fixed size chosen when it starts. It cannot grow until the disk is full and take the
//! filesystem down with it - the failure that would turn a troubleshooting tool into an outage. Full
//! means stop, and say so.

use godspeed_sdk::{Message, ServiceContext};

/// Control opcodes on this service's endpoint.
pub const REC_OP_START: u8 = 1; // [1][mib][plen][path][flen][filter]
pub const REC_OP_STOP: u8 = 2; // [2]
pub const REC_OP_STATUS: u8 = 3; // [3]

pub const REC_OK: u8 = 0;
pub const REC_ERR: u8 = 1;

/// `fs` opcodes. Wire format is [tag, op, path_len, path.., data..].
const FS_OP_WRITE_NEW: u8 = 24;
const FS_OP_WRITE_AT: u8 = 25;
const FS_OK: u8 = 0;
/// Correlation tag for this service's own `fs` requests. Distinct from 0, which is both what an
/// unthinking caller sends and the value of `FS_OK` - a collision that has hidden a bug before.
const FS_TAG: u8 = 0xE1;

/// One streaming chunk, matching the `fs` `MAX_FILE_BYTES` (7 data-block payloads of 508 bytes).
///
/// Offsets handed to `WRITE_AT` must stay block-aligned or `fs` has to read-modify-write, so this
/// service only flushes at multiples of this. The one short flush - the tail on STOP - is still at an
/// aligned offset, and nothing follows it.
const IO_CHUNK: usize = 7 * 508;

/// Default capture size when the caller does not say: big enough for a long session at a few hundred
/// bytes a line, small enough that a forgotten capture cannot eat a disk.
const DEFAULT_MIB: u8 = 1;

/// How long to wait for a control message before draining anyway. The loop must serve requests AND
/// drain on a timer; `recv_timeout` does both without a second task.
const DRAIN_MS: u64 = 2000;

const PATH_MAX: usize = 64;
const FILTER_MAX: usize = 12;

/// The `events` log query: [7][since:u64] -> [next:u64][oldest:u64][held:u64][wrapped:u8][text].
const EV_OP_LOGS: u8 = 7;
const EV_HDR: usize = 25;

/// The record separator `events` writes between a line's owner and its text.
const US: u8 = 0x1f;
const NEWLINE: [u8; 1] = [10];

fn reply(ctx: &ServiceContext, out: &[u8]) {
    if let Some(cap) = ctx.take_pending_cap() {
        let _ = ctx.try_send_by_handle(cap, &Message::from_bytes(out));
        // Reclaim it: a reply cap is a one-shot return address handed to us inside the request, and
        // sending on it does not consume it. Leaving it behind burns a cap-table slot per reply.
        ctx.remove_cap(cap);
    }
}

/// One bounded `fs` request. True when `fs` answered OK.
fn fs_call(ctx: &ServiceContext, op: u8, path: &[u8], tail: &[u8]) -> bool {
    let mut req = [0u8; 3 + PATH_MAX + 8 + IO_CHUNK];
    req[0] = FS_TAG;
    req[1] = op;
    req[2] = path.len() as u8;
    req[3..3 + path.len()].copy_from_slice(path);
    let off = 3 + path.len();
    if off + tail.len() > req.len() {
        return false;
    }
    req[off..off + tail.len()].copy_from_slice(tail);
    let n = off + tail.len();
    match ctx.request_with_reply_deadline("fs", &Message::from_bytes(&req[..n]), 5) {
        // THE REPLY IS [tag, status], NOT [status]. `fs` echoes the correlation tag back as byte 0,
        // which is what lets a caller recognise its own reply among the requests it is serving. Reading
        // byte 0 as the status made every successful write look like a failure - the file was created
        // and the service reported "could not create the capture file", which is the worst kind of
        // wrong because both halves are convincing.
        Some(r) => {
            let b = r.payload_bytes();
            b.first() == Some(&FS_TAG) && b.get(1) == Some(&FS_OK)
        }
        None => false,
    }
}

struct Capture {
    on: bool,
    path: [u8; PATH_MAX],
    plen: usize,
    filter: [u8; FILTER_MAX],
    flen: usize,
    cursor: u64,
    /// Bytes committed. A multiple of `IO_CHUNK` for as long as recording continues.
    written: u64,
    capacity: u64,
    lines: u64,
    /// Lines the window overwrote before this service read them. Counted and reported: a capture with
    /// a hole in it must say so rather than read as complete (invariant 12).
    lost: u64,
    stage: [u8; IO_CHUNK],
    staged: usize,
    full: bool,
}

impl Capture {
    const fn new() -> Self {
        Self {
            on: false,
            path: [0; PATH_MAX],
            plen: 0,
            filter: [0; FILTER_MAX],
            flen: 0,
            cursor: 0,
            written: 0,
            capacity: 0,
            lines: 0,
            lost: 0,
            stage: [0; IO_CHUNK],
            staged: 0,
            full: false,
        }
    }

    /// Commit the staged bytes at the current offset. `final_tail` allows the one short write, which
    /// happens on STOP where no aligned offset is needed afterwards.
    fn flush(&mut self, ctx: &ServiceContext, final_tail: bool) -> bool {
        if self.staged == 0 {
            return true;
        }
        if !final_tail && self.staged < IO_CHUNK {
            return true; // not a whole chunk yet; keep staging so offsets stay aligned
        }
        let mut tail = [0u8; 8 + IO_CHUNK];
        tail[..8].copy_from_slice(&self.written.to_le_bytes());
        tail[8..8 + self.staged].copy_from_slice(&self.stage[..self.staged]);
        let n = 8 + self.staged;
        let mut p = [0u8; PATH_MAX];
        p[..self.plen].copy_from_slice(&self.path[..self.plen]);
        let ok = fs_call(ctx, FS_OP_WRITE_AT, &p[..self.plen], &tail[..n]);
        if ok {
            self.written += self.staged as u64;
            self.staged = 0;
        }
        ok
    }

    /// Stage one line, flushing whole chunks as they fill. False once the capture is full.
    fn push_line(&mut self, ctx: &ServiceContext, line: &[u8]) -> bool {
        if self.full {
            return false;
        }
        for &c in line.iter().chain(NEWLINE.iter()) {
            if self.written + self.staged as u64 >= self.capacity {
                self.full = true;
                return false;
            }
            self.stage[self.staged] = c;
            self.staged += 1;
            if self.staged == IO_CHUNK && !self.flush(ctx, false) {
                return false;
            }
        }
        self.lines += 1;
        true
    }
}

/// Close the capture: footer, final flush, and a line saying which it was.
fn finish(ctx: &ServiceContext, cap: &mut Capture, why: &[u8]) {
    let mut foot = [0u8; 96];
    let mut n = 0usize;
    for &c in b"recorder" {
        foot[n] = c;
        n += 1;
    }
    foot[n] = US;
    n += 1;
    for &c in b"capture ended (" {
        foot[n] = c;
        n += 1;
    }
    for &c in why {
        foot[n] = c;
        n += 1;
    }
    foot[n] = b')';
    n += 1;
    let _ = cap.push_line(ctx, &foot[..n]);
    let _ = cap.flush(ctx, true);
    cap.on = false;
    ctx.log_fmt(format_args!(
        "recorder: capture ended - {} line(s), {} byte(s), {} lost to the window",
        cap.lines, cap.written, cap.lost
    ));
}

/// Ask `events` for whatever is new, and stage it.
fn drain(ctx: &ServiceContext, cap: &mut Capture) {
    let mut req = [0u8; 9];
    req[0] = EV_OP_LOGS;
    req[1..9].copy_from_slice(&cap.cursor.to_le_bytes());
    let r = match ctx.request_with_reply_deadline("events", &Message::from_bytes(&req), 3) {
        Some(r) => r,
        None => return, // events is busy or gone; the next tick tries again
    };
    let b = r.payload_bytes();
    if b.len() < EV_HDR {
        return;
    }
    let next = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
    let oldest = u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]);
    // THE WINDOW OUTRAN US. `events` holds 8 KiB; if this service was stalled on a slow disk while the
    // machine was chatty, lines fell out before they were read. Counted here and reported by `status`
    // and in the footer, because a capture with an unexplained gap is worse than one that admits it.
    if cap.cursor > 0 && oldest > cap.cursor + 1 {
        cap.lost += oldest - cap.cursor - 1;
    }
    let body = &b[EV_HDR..];
    let mut ls = 0usize;
    for i in 0..=body.len() {
        let end = if i == body.len() {
            body.len()
        } else if body[i] == 10 {
            i
        } else {
            continue;
        };
        if end > ls {
            let line = &body[ls..end];
            // The record is `owner US text`. Filtering matches the OWNER FIELD, so it works even where
            // a service log prefix differs from its registered name (`dwc2` writes `dwc2-svc:`).
            let keep = if cap.flen == 0 {
                true
            } else {
                let cut = line.iter().position(|&c| c == US).unwrap_or(line.len());
                line[..cut] == cap.filter[..cap.flen]
            };
            if keep && !cap.push_line(ctx, line) {
                break;
            }
        }
        ls = end + 1;
        if end == body.len() {
            break;
        }
    }
    cap.cursor = next;
    if cap.full {
        finish(ctx, cap, b"FULL");
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.trace_as("recorder");
    let mut cap = Capture::new();
    let wait = ctx.duration_cycles(DRAIN_MS);
    ctx.log("recorder: ready (idle - `events persist start` begins a capture)");

    loop {
        if let Some(msg) = ctx.recv_timeout(wait) {
            let p = msg.payload_bytes();
            match p.first().copied() {
                Some(REC_OP_START) if p.len() >= 4 => {
                    let mib = if p[1] == 0 { DEFAULT_MIB } else { p[1] };
                    let plen = (p[2] as usize).min(PATH_MAX);
                    if p.len() < 3 + plen + 1 {
                        reply(&ctx, &[REC_ERR]);
                        continue;
                    }
                    cap = Capture::new();
                    cap.plen = plen;
                    cap.path[..plen].copy_from_slice(&p[3..3 + plen]);
                    let fo = 3 + plen;
                    let flen = (p[fo] as usize).min(FILTER_MAX);
                    if flen > 0 && p.len() >= fo + 1 + flen {
                        cap.flen = flen;
                        cap.filter[..flen].copy_from_slice(&p[fo + 1..fo + 1 + flen]);
                    }
                    cap.capacity = (mib as u64) << 20;
                    let mut path = [0u8; PATH_MAX];
                    path[..plen].copy_from_slice(&cap.path[..plen]);
                    if !fs_call(&ctx, FS_OP_WRITE_NEW, &path[..plen], &cap.capacity.to_le_bytes()) {
                        ctx.log("recorder: could not create the capture file - is there a filesystem?");
                        reply(&ctx, &[REC_ERR]);
                        continue;
                    }
                    cap.on = true;
                    // A HEADER, so a file with no footer is known to have died rather than finished.
                    let mut hdr = [0u8; 32];
                    let mut hn = 0usize;
                    for &c in b"recorder" {
                        hdr[hn] = c;
                        hn += 1;
                    }
                    hdr[hn] = US;
                    hn += 1;
                    for &c in b"capture started" {
                        hdr[hn] = c;
                        hn += 1;
                    }
                    let _ = cap.push_line(&ctx, &hdr[..hn]);
                    ctx.log_fmt(format_args!("recorder: capturing to a {} MiB file", mib));
                    reply(&ctx, &[REC_OK]);
                }
                Some(REC_OP_STOP) => {
                    if cap.on {
                        finish(&ctx, &mut cap, b"stopped");
                    }
                    reply(&ctx, &[REC_OK]);
                }
                Some(REC_OP_STATUS) => {
                    let mut out = [0u8; 40 + PATH_MAX];
                    out[0] = REC_OK;
                    out[1] = cap.on as u8;
                    out[2] = cap.full as u8;
                    out[3..11].copy_from_slice(&cap.lines.to_le_bytes());
                    out[11..19].copy_from_slice(&cap.written.to_le_bytes());
                    out[19..27].copy_from_slice(&cap.lost.to_le_bytes());
                    out[27..35].copy_from_slice(&cap.capacity.to_le_bytes());
                    out[35] = cap.plen as u8;
                    out[36..36 + cap.plen].copy_from_slice(&cap.path[..cap.plen]);
                    reply(&ctx, &out[..36 + cap.plen]);
                }
                _ => reply(&ctx, &[REC_ERR]),
            }
        }
        if cap.on {
            drain(&ctx, &mut cap);
        }
    }
}
