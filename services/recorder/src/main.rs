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
pub const REC_OP_START: u8 = 1; // [1][capacity:u64][plen][path][flen][filter]
pub const REC_OP_STOP: u8 = 2; // [2]
pub const REC_OP_STATUS: u8 = 3; // [3]

pub const REC_OK: u8 = 0;
pub const REC_ERR: u8 = 1;

/// `fs` opcodes. Wire format is [tag, op, path_len, path.., data..].
const FS_OP_WRITE_NEW: u8 = 24;
const FS_OP_WRITE_AT: u8 = 25;
const FS_OP_RENAME: u8 = 15;
const FS_OP_DELETE: u8 = 16;
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

/// Default capture size when the caller does not say. The shell normally computes one from a duration;
/// this is the floor if it sends nothing.
const DEFAULT_CAPACITY: u64 = 8 * 1024 * 1024;

/// How long to wait for a control message before draining anyway. The loop must serve requests AND
/// drain on a timer; `recv_timeout` does both without a second task.
const DRAIN_MS: u64 = 2000;

/// How many files a capture is spread over. The budget is divided among them, so the guaranteed
/// retention is (PIECES-1)/PIECES of what was asked for: with 2 you always hold at least half, and the
/// oscillation is what `covers` reports. Raising it tightens the floor and costs one rename per
/// rotation; it is deliberately ONE constant, so changing it is a one-line decision.
const PIECES: usize = 2;

const PATH_MAX: usize = 64;
const FILTER_MAX: usize = 12;

/// The `events` log query: [7][since:u64] -> [next:u64][oldest:u64][held:u64][wrapped:u8][text].
const EV_OP_LOGS: u8 = 7;
const EV_HDR: usize = 25;

/// The record separator `events` writes between a line's owner and its text.
const US: u8 = 0x1f;
/// Longest owner name written to disk (matches the SDK `PEER_LEN`).
const PEER_OUT: usize = 12;
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

/// Fill a freshly created capture file with zeros, so every block carries a valid CRC.
///
/// WITHOUT THIS THE FILE CANNOT BE READ AT ALL. `OP_WRITE_NEW` allocates the extent but writes no
/// data blocks, so everything past the last chunk we wrote has a stored CRC of zero - and `fs`
/// correctly refuses it:
///
///   fs: data block CRC mismatch at lba 4229 (stored 0x00000000, actual 0x0fbb6d54) - refusing
///   read: storage error
///
/// A capture that cannot be read back is not a capture. The cost is one pass of zero chunks when a
/// file is created (at start, and at each rotation), which is why the default size is modest: it is
/// paid at a moment the operator is already waiting, and never again while recording.
/// Chunks of pre-fill per tick. BOUNDED so the loop stays responsive: a capture on slow storage takes
/// longer to become ready, and nothing else waits on it while it does.
const FILL_CHUNKS_PER_TICK: usize = 24;

/// Advance the pre-fill of the current piece by one bounded slice. True while it still has work.
///
/// INCREMENTAL, AND THAT IS THE POINT. The first version filled the whole extent inside the START
/// request, so the SHELL blocked on storage I/O - 800 ms on a SATA SSD, and over TWELVE SECONDS on the
/// Pi 4's USB stick, where the request timed out and the capture never began. The size was never the
/// real defect: blocking a caller on an unbounded amount of device I/O is, and it would have bitten
/// again on any slower medium.
///
/// Doing it here costs a capture some time before it is READY, which `status` reports as `preparing`
/// rather than hiding. Nothing waits on this service, so slow storage delays only the capture.
fn fill_step(ctx: &ServiceContext, cap: &mut Capture) -> bool {
    if cap.filled >= cap.capacity {
        return false;
    }
    let zeros = [0u8; IO_CHUNK];
    let mut p = [0u8; PATH_MAX + 2];
    let pn = cap.cur_path(&mut p);
    for _ in 0..FILL_CHUNKS_PER_TICK {
        if cap.filled >= cap.capacity {
            break;
        }
        let n = IO_CHUNK.min((cap.capacity - cap.filled) as usize);
        let mut tail = [0u8; 8 + IO_CHUNK];
        tail[..8].copy_from_slice(&cap.filled.to_le_bytes());
        tail[8..8 + n].copy_from_slice(&zeros[..n]);
        if !fs_call(ctx, FS_OP_WRITE_AT, &p[..pn], &tail[..8 + n]) {
            ctx.log("recorder: pre-fill write failed - stopping the capture");
            cap.on = false;
            return false;
        }
        cap.filled += n as u64;
    }
    cap.filled < cap.capacity
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
    rotations: u64,
    /// Bytes written since the capture began, ACROSS rotations. `written` resets at each rotation, so
    /// it cannot answer "how fast is this filling" - which is the only honest way to say how long a
    /// capture actually covers.
    total_written: u64,
    /// Bytes of the current piece zero-filled so far. While this is below `capacity` the capture is
    /// PREPARING, not recording.
    filled: u64,
    /// Epoch seconds when the capture started. Coverage is a MEASURED rate, never the estimate the
    /// caller asked for: a duration is a target, and the machine decides whether it was met.
    started_at: u64,
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
            rotations: 0,
            total_written: 0,
            filled: 0,
            started_at: 0,
        }
    }

    /// The file currently being written - ALWAYS the base path.
    ///
    /// Rotation renames rather than alternating, so `/log.txt` is the newest piece at every moment and
    /// `read /log.txt` needs no knowledge of how many rotations have happened. The previous version
    /// swapped between two names, which meant the current file depended on a rotation count only
    /// `status` could tell you - the file you wanted was a coin toss.
    fn cur_path(&self, out: &mut [u8; PATH_MAX + 2]) -> usize {
        out[..self.plen].copy_from_slice(&self.path[..self.plen]);
        self.plen
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
        let mut p = [0u8; PATH_MAX + 2];
        let pn = self.cur_path(&mut p);
        let ok = fs_call(ctx, FS_OP_WRITE_AT, &p[..pn], &tail[..n]);
        if ok {
            self.written += self.staged as u64;
            self.total_written += self.staged as u64;
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

/// Close the current file and begin the other one. Bounded at TWO files: total disk use is fixed at
/// twice the chosen size, forever, no matter how long a capture runs.
///
/// Rotation rather than stopping, because stopping keeps the WRONG HALF. A fixed file that stops when
/// full preserves the beginning of a session and discards everything after - and the reason to run a
/// capture for an hour is almost always to catch something at the END of it. Two files always hold
/// between one and two files' worth of the most recent history, and each is readable in order, which a
/// single wrapping file would not be.
fn rotate(ctx: &ServiceContext, cap: &mut Capture) {
    let mut foot = [0u8; 64];
    let mut n = 0usize;
    for &c in b"recorder: piece full - continuing in a fresh one" { foot[n] = c; n += 1; }
    // Straight into the stage: `push_line` would refuse, the capture being full.
    for &c in foot[..n].iter().chain(NEWLINE.iter()) {
        if cap.staged < IO_CHUNK {
            cap.stage[cap.staged] = c;
            cap.staged += 1;
        }
    }
    let _ = cap.flush(ctx, true);

    // SHIFT THE PIECES DOWN, oldest first: .N-1 is deleted, .N-2 becomes .N-1, and the base becomes
    // .1. The base name is then free for a fresh file, which is what keeps `/log.txt` the newest at
    // every moment. Renames are metadata only, so this costs no data movement however big a piece is.
    let mut from = [0u8; PATH_MAX + 4];
    let mut to = [0u8; PATH_MAX + 4];
    for i in (1..PIECES).rev() {
        let fl = suffixed(&cap.path[..cap.plen], i - 1, &mut from);
        let tl = suffixed(&cap.path[..cap.plen], i, &mut to);
        // Delete the destination first: a rename onto an existing name is not guaranteed to replace it,
        // and a rotation that silently failed would leave the oldest piece never ageing out.
        let _ = fs_call(ctx, FS_OP_DELETE, &to[..tl], &[]);
        // OP_RENAME takes the NEW NAME as a bare basename, not a path.
        let base = basename(&to[..tl]);
        let _ = fs_call(ctx, FS_OP_RENAME, &from[..fl], base);
    }

    cap.rotations += 1;
    cap.written = 0;
    cap.staged = 0;
    cap.full = false;
    let mut p = [0u8; PATH_MAX + 2];
    let pn = cap.cur_path(&mut p);
    cap.filled = 0;
    if !fs_call(ctx, FS_OP_WRITE_NEW, &p[..pn], &cap.capacity.to_le_bytes()) {
        ctx.log("recorder: could not open the next capture file - stopping");
        cap.on = false;
        return;
    }
    ctx.log_fmt(format_args!("recorder: rotated ({} so far) - /...{} is the newest", cap.rotations, cap.rotations.min(1)));
}

/// `path` with `.N` appended, or `path` itself when `n == 0`. The newest piece has no suffix.
fn suffixed(path: &[u8], n: usize, out: &mut [u8; PATH_MAX + 4]) -> usize {
    out[..path.len()].copy_from_slice(path);
    if n == 0 {
        return path.len();
    }
    let mut k = path.len();
    out[k] = b'.'; k += 1;
    out[k] = b'0' + (n as u8).min(9); k += 1;
    k
}

/// The last path segment. `OP_RENAME` names the destination within the same directory.
fn basename(p: &[u8]) -> &[u8] {
    match p.iter().rposition(|&c| c == b'/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// Close the capture: footer, final flush, and a line saying which it was.
///
/// The footer is written in the ON-DISK form (`owner: text`), not the wire form (`owner US text`).
/// It is one of only two lines this service writes directly - every other line arrives from `events`
/// and is converted by the drain (see the note there). Emitting the raw separator here put a control
/// byte in the first and last lines of every capture while the body read cleanly.
fn finish(ctx: &ServiceContext, cap: &mut Capture, why: &[u8]) {
    let mut foot = [0u8; 96];
    let mut n = 0usize;
    for &c in b"recorder" {
        foot[n] = c;
        n += 1;
    }
    foot[n] = b':';
    n += 1;
    foot[n] = b' ';
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
            if keep {
                // WRITTEN AS `owner: text`, NOT `owner US text`.
                //
                // The wire form separates the fields with 0x1F so the shell can build records from
                // them. On DISK that is the wrong shape: the file is consumed with `read`, and a
                // control character in the middle of every line reads as corruption. It cannot usefully
                // be piped into records either - the shell pipe truncates at 16 KiB (CAP_MAX) and every
                // sink clips at 4 KiB, so a multi-megabyte capture never survives a pipeline whatever
                // its format. Optimise for the tool that will actually read it.
                //
                // The owner is dropped from the text where the text already opens with it, exactly as
                // the live view does, so `fs: serving file API` does not become `fs: fs: serving...`.
                let cut = line.iter().position(|&c| c == US).unwrap_or(0);
                let (owner, mut text) = line.split_at(cut);
                if !text.is_empty() {
                    text = &text[1..]; // past the separator
                }
                if !owner.is_empty() && text.len() > owner.len() && &text[..owner.len()] == owner {
                    let scan = text.len().min(owner.len() + 24);
                    let mut i = 0usize;
                    while i + 1 < scan {
                        if text[i] == b':' && text[i + 1] == b' ' {
                            text = &text[i + 2..];
                            break;
                        }
                        i += 1;
                    }
                }
                let mut out = [0u8; PEER_OUT + 2 + 256];
                let mut n = 0usize;
                for &c in owner.iter().take(PEER_OUT) { out[n] = c; n += 1; }
                out[n] = b':'; n += 1;
                out[n] = b' '; n += 1;
                for &c in text.iter().take(256) { out[n] = c; n += 1; }
                if !cap.push_line(ctx, &out[..n]) {
                    break;
                }
            }
        }
        ls = end + 1;
        if end == body.len() {
            break;
        }
    }
    cap.cursor = next;
    if cap.full {
        rotate(ctx, cap);
    }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.trace_as("recorder");
    let mut cap = Capture::new();
    let wait = ctx.duration_cycles(DRAIN_MS);
    ctx.log("recorder: ready (idle - `events persist start` begins a capture)");

    loop {
        // WHILE PREPARING, DO NOT SLEEP. `recv_timeout` parks for two seconds between messages, which
        // is right when idle or recording and hopeless while filling: it capped the pre-fill at one
        // slice per tick - about 85 KB every two seconds - so a megabyte took half a minute and a
        // capture that was stopped before it finished left an unfilled tail that `read` refuses.
        //
        // Non-blocking here instead, so the fill runs as fast as the device allows while control
        // messages are still served every iteration.
        let preparing = cap.on && cap.filled < cap.capacity;
        let incoming = if preparing { ctx.try_recv() } else { ctx.recv_timeout(wait) };
        if let Some(msg) = incoming {
            let p = msg.payload_bytes();
            match p.first().copied() {
                Some(REC_OP_START) if p.len() >= 11 => {
                    let want = u64::from_le_bytes([p[1], p[2], p[3], p[4], p[5], p[6], p[7], p[8]]);
                    let total = if want == 0 { DEFAULT_CAPACITY } else { want };
                    let plen = (p[9] as usize).min(PATH_MAX);
                    if p.len() < 10 + plen + 1 {
                        reply(&ctx, &[REC_ERR]);
                        continue;
                    }
                    cap = Capture::new();
                    cap.plen = plen;
                    cap.path[..plen].copy_from_slice(&p[10..10 + plen]);
                    let fo = 10 + plen;
                    let flen = (p[fo] as usize).min(FILTER_MAX);
                    if flen > 0 && p.len() >= fo + 1 + flen {
                        cap.flen = flen;
                        cap.filter[..flen].copy_from_slice(&p[fo + 1..fo + 1 + flen]);
                    }
                    // The caller's number is the TOTAL disk budget; it is split across the pieces, so
                    // "64MiB" means 64 MiB on disk rather than 64 per file. Asking for a budget and
                    // quietly using a multiple of it is the kind of small lie that is found late.
                    cap.capacity = (total / PIECES as u64).max(64 * 1024);
                    cap.started_at = ctx.epoch_secs_monotonic() as u64;
                    let mut path = [0u8; PATH_MAX];
                    path[..plen].copy_from_slice(&cap.path[..plen]);
                    // ALLOCATE ONLY, then answer. The extent is one cheap `fs` call; the pre-fill is
                    // the expensive part and now happens in the loop below, so the caller is never
                    // blocked on an unbounded amount of device I/O.
                    if !fs_call(&ctx, FS_OP_WRITE_NEW, &path[..plen], &cap.capacity.to_le_bytes()) {
                        ctx.log("recorder: could not create the capture file - is there a filesystem?");
                        reply(&ctx, &[REC_ERR]);
                        continue;
                    }
                    cap.on = true;
                    cap.filled = 0;
                    ctx.log_fmt(format_args!(
                        "recorder: capturing to {} pieces of {} KiB ({} KiB total)",
                        PIECES, cap.capacity / 1024, total / 1024));
                    reply(&ctx, &[REC_OK]);
                }
                Some(REC_OP_STOP) => {
                    if cap.on {
                        finish(&ctx, &mut cap, b"stopped");
                    }
                    reply(&ctx, &[REC_OK]);
                }
                Some(REC_OP_STATUS) => {
                    let mut out = [0u8; 80 + PATH_MAX];
                    out[0] = REC_OK;
                    out[1] = cap.on as u8;
                    out[2] = cap.full as u8;
                    out[3..11].copy_from_slice(&cap.lines.to_le_bytes());
                    out[11..19].copy_from_slice(&cap.written.to_le_bytes());
                    out[19..27].copy_from_slice(&cap.lost.to_le_bytes());
                    out[27..35].copy_from_slice(&cap.capacity.to_le_bytes());
                    out[35..43].copy_from_slice(&cap.rotations.to_le_bytes());
                    // MEASURED, not requested. Elapsed seconds and lifetime bytes let the reader work
                    // out the real fill rate and therefore what the capture actually covers - which is
                    // the only honest answer, because a duration asked for is a prediction about how
                    // chatty the machine will be and the machine decides that.
                    let now = ctx.epoch_secs_monotonic() as u64;
                    let elapsed = now.saturating_sub(cap.started_at);
                    out[43..51].copy_from_slice(&elapsed.to_le_bytes());
                    out[51..59].copy_from_slice(&cap.total_written.to_le_bytes());
                    out[59..67].copy_from_slice(&(PIECES as u64).to_le_bytes());
                    // How far the pre-fill has got. A capture that is still PREPARING is not idle and
                    // is not recording, and saying so is the difference between "it is working on it"
                    // and "it silently did nothing".
                    out[67..75].copy_from_slice(&cap.filled.to_le_bytes());
                    out[75] = cap.plen as u8;
                    out[76..76 + cap.plen].copy_from_slice(&cap.path[..cap.plen]);
                    reply(&ctx, &out[..76 + cap.plen]);
                }
                _ => reply(&ctx, &[REC_ERR]),
            }
        }
        if cap.on {
            if cap.filled < cap.capacity {
                // PREPARING: still making the extent readable. No draining yet - a line written past
                // the fill point would sit in a region `read` still refuses.
                if !fill_step(&ctx, &mut cap) && cap.on && cap.filled >= cap.capacity {
                    // A HEADER, so a file with no footer is known to have died rather than finished.
                    // In the ON-DISK form, like the footer and like every drained line - see `finish`.
                    let mut hdr = [0u8; 32];
                    let mut hn = 0usize;
                    for &c in b"recorder" {
                        hdr[hn] = c;
                        hn += 1;
                    }
                    hdr[hn] = b':';
                    hn += 1;
                    hdr[hn] = b' ';
                    hn += 1;
                    for &c in b"capture started" {
                        hdr[hn] = c;
                        hn += 1;
                    }
                    let _ = cap.push_line(&ctx, &hdr[..hn]);
                    ctx.log_fmt(format_args!("recorder: ready - capturing to {} KiB", cap.capacity / 1024));
                }
            } else {
                drain(&ctx, &mut cap);
            }
        }
    }
}
