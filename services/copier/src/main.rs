// SPDX-License-Identifier: GPL-2.0-only
// 18.2: `unsafe` is FORBIDDEN outside the four kernel layers and the SDK's audited ABI.
// `unsafe_check.py` greps for it; this makes the COMPILER refuse it, which catches what a
// grep cannot - unsafe produced by a macro, or spelled across lines. `deny` rather than
// `forbid` for exactly one reason: the exported `service_main` symbol needs
// `#[allow(unsafe_code)]`, because a `#[no_mangle]` declaration is itself covered by this
// lint (a colliding symbol is a soundness hole). `forbid` cannot be relaxed even there.
#![deny(unsafe_code)]
#![no_std]
#![no_main]
//! `copier` - copies one file without owning the prompt.
//!
//! **Why a service and not a loop in the shell.** This shell has no threads, and its main loop
//! blocks reading keys, so detached work makes progress in exactly one of two ways: it is a task,
//! or it is a resumable state machine advanced between keystrokes. `docs/job-control-design.md` §4
//! argues for the task, and three of its reasons are mechanical rather than aesthetic:
//!
//! 1. **Stopping it is `kill`**, the mechanism the supervisor already has, rather than bookkeeping
//!    that has to be correct in every arm of a state machine.
//! 2. **The shell's stack does not grow.** `cmd_edit` already holds the deepest frame at 61 KiB of
//!    a 256 KiB user stack; N state machines live in that budget, N services do not.
//! 3. **A job that faults is a service that faults** - the supervisor reports it, the kernel
//!    reclaims its frames, `status` lists it. In the shell, a faulting job is a faulting SHELL,
//!    which is the one service whose death takes the session with it.
//!
//! **WHAT THIS SERVICE CANNOT DO IS THE POINT.** It holds `fs` and its log. It has no
//! `console_push`, so it cannot write over a prompt somebody is typing at - not by convention but
//! because it holds no cap that reaches the console (§3.1). It cannot spawn, reboot, or reach the
//! network. That is strictly less authority than the shell's own, which is what a job running
//! inside the shell's loop would have had.
//!
//! **The honest limit of that claim, recorded rather than implied (§26.7).** The design note this
//! implements describes minting a READ cap for the source and a WRITE cap for the destination and
//! handing over exactly those, so the job could reach nothing else *for its whole life*. That is
//! not what this does, because the shell's `spawn` surface takes a name and nothing else -
//! `utilities/10_spawn.md` §5 lists per-invocation cap delegation as future work. So the bound
//! here is the CONTRACT's (`fs`, entire) rather than the two paths'. It is a real reduction from
//! the alternative and it is not the one the design claimed; when `spawn` learns to delegate, this
//! service should take its paths as caps and stop resolving them by name.
//!
//! **Spawned on demand**, never at boot, and deliberately not restarted on death - the `recorder`
//! shape (`services/recorder/src/main.rs`). A respawned copier would not know what it was copying,
//! so it would be alive and copying nothing while `jobs` reported `running`.
//!
//! **One copy at a time.** A second `START` while one is running is refused, loudly, rather than
//! queued: a queue is unbounded growth wearing a small word (§26.6), and the shell's job table
//! refuses at its own edge for the same reason.

use godspeed as gs;
use godspeed_sdk::{Message, ServiceContext};

/// Control opcodes on this service's endpoint.
pub const CP_OP_START: u8 = 1; // [1][kind][slen][src][dlen][dst]
pub const CP_OP_STATUS: u8 = 2; // [2]
pub const CP_OP_CANCEL: u8 = 3; // [3]
/// Read the job's transcript. [4][offset:u32] -> [CP_OK, 4, dropped:u32, total:u32, bytes..]
pub const CP_OP_OUTPUT: u8 = 4;

pub const CP_OK: u8 = 0;
pub const CP_ERR: u8 = 1;

/// WHAT KIND OF WORK a job is. Two, and the bar for a third is not "is it long" but "is its value
/// its EFFECT rather than its OUTPUT" - a detached job holds no console capability, so a command
/// whose whole product is printed text has nowhere to put it and must not be detached.
pub const KIND_COPY: u8 = 0;
/// `delete <path> recursive`. Cheap to add because `fs` already does the walk in one operation
/// (`OP_DELETE_TREE`), so this service issues ONE request and waits - no walking, no per-entry
/// bookkeeping, no partial-tree question that this service would have to answer.
pub const KIND_DELETE_TREE: u8 = 1;
/// `drives check`. The first job whose product is a REPORT rather than an effect, which is what
/// the transcript exists for. One `fs` request; this service renders the verdict into its ring.
pub const KIND_CHECK: u8 = 2;
/// `drives scrub` - the read-only integrity sweep. Same shape as the check exactly: one `fs`
/// request, a verdict, and a transcript to put it in. It is here because that shape made it nearly
/// free, not because somebody wanted a longer list.
pub const KIND_SCRUB: u8 = 3;
/// `churn <seconds>` - sustained write/rename/delete traffic so a power cut lands somewhere.
///
/// The one job kind with a REAL percentage that is not a byte count: its bound is a duration, so
/// elapsed-over-total is a true measure rather than an invented one.
///
/// The content pattern is `sdk::churn`, NOT a copy of it. The shell's `churn verify` reads the same
/// module, so a writer here and a checker there cannot drift - and if they did, verify would report
/// "NONE torn" while no longer able to recognise a tear, which is a safety check that passes
/// because it broke.
pub const KIND_CHURN: u8 = 4;

/// Job states, four words and no abbreviations (`docs/job-control-design.md` §6).
pub const ST_IDLE: u8 = 0;
pub const ST_RUNNING: u8 = 1;
pub const ST_DONE: u8 = 2;
pub const ST_FAILED: u8 = 3;
pub const ST_CANCELLED: u8 = 4;

/// Why a job failed. `failed` carries the reason when asked for (§6), so the shell never has to
/// print "failed" and leave the operator guessing which half went wrong.
pub const WHY_NONE: u8 = 0;
pub const WHY_NO_SOURCE: u8 = 1; // missing, or a directory
pub const WHY_NO_DEST: u8 = 2; // could not create the destination
pub const WHY_READ: u8 = 3;
pub const WHY_WRITE: u8 = 4;
/// `fs` kept accepting the request and not answering it within the deadline, for every retry. A
/// SLOW dependency is not a failed one, and reporting it as a write error sent a reader looking at
/// the disk when the truth was that a `drives check` held `fs` for six seconds. Measured, not
/// theorised: it is what the first `jobs` suite run produced.
pub const WHY_UNANSWERED: u8 = 5;

const FS_OK: u8 = 0;


/// One streaming chunk, matching the shell's `IO_CHUNK` (7 data-block payloads of 508 bytes).
/// The same size the shell's own `copy` streams in, so this service moves a file in exactly the
/// pieces the filesystem is shaped for.
const IO_CHUNK: usize = 7 * 508;

/// Matches the shell's `PATH_MAX`. A path longer than this cannot be typed at the prompt, so the
/// two ends agree by construction rather than by a comment asking them to.
const PATH_MAX: usize = 120;

/// How long the idle loop parks between control messages. Only reached when NOT copying: a running
/// job polls instead, so the copy runs at whatever rate the device allows.
const IDLE_MS: u64 = 500;

/// Seconds to wait for one `fs` round trip. A chunk is small and `fs` answers promptly or has a
/// real problem; this is the same budget `recorder` gives its writes.

/// How many times one chunk is re-sent when `fs` does not answer in time. Bounded (§26.6), so a
/// filesystem that has genuinely stopped ends the job rather than wedging it - six tries at five
/// seconds is half a minute of patience and then a loud, accurate failure.
const CHUNK_TRIES: u32 = 6;


/// THE TRANSCRIPT: a fixed ring holding what a job had to say, replayed by `foreground`.
///
/// This is the answer to "a detached job has nowhere to write its output" that does NOT involve
/// giving the job a console. It holds no `console_push` capability and still does not; the bytes sit
/// here until somebody asks for them, so a job can never write over a prompt.
///
/// FIXED, and it says when it has dropped. 4 KiB of `.bss` - no heap, no growth (§26.6.1). When a
/// job produces more than that the OLDEST bytes are aged out, exactly as `scrollback` does, and the
/// count of dropped bytes is reported with the replay. A bounded buffer that silently truncates is
/// worse than no buffer at all: it hands somebody a partial `drives check` that reads as a complete
/// one, which is the silent failure invariant 12 forbids.
const TRANSCRIPT: usize = 4096;

struct Transcript {
    buf: [u8; TRANSCRIPT],
    len: usize,
    /// Bytes that fell out of the front. Nonzero means the replay is incomplete and must say so.
    dropped: u32,
}

impl Transcript {
    const fn new() -> Self {
        Transcript { buf: [0u8; TRANSCRIPT], len: 0, dropped: 0 }
    }
    fn clear(&mut self) {
        self.len = 0;
        self.dropped = 0;
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.len == TRANSCRIPT {
                // Age out the oldest line rather than the oldest BYTE, so a replay never begins
                // mid-word. Falling back to a byte shift when there is no newline keeps this
                // bounded in the pathological case (one enormous line).
                let cut = self.buf.iter().position(|&c| c == b'\n').map(|i| i + 1).unwrap_or(1);
                self.buf.copy_within(cut.., 0);
                self.len -= cut;
                self.dropped = self.dropped.saturating_add(cut as u32);
            }
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
}

struct Job {
    state: u8,
    why: u8,
    kind: u8,
    out: Transcript,
    /// Churn only: the iteration counter, and what it has managed so far. Reported at the end
    /// because a load generator that says only "it ran" hides the case where it exercised one
    /// transaction shape while claiming three - which is a bug this churn has actually had.
    step: u64,
    writes: u64,
    renames: u64,
    deletes: u64,
    src: [u8; PATH_MAX],
    slen: usize,
    dst: [u8; PATH_MAX],
    dlen: usize,
    total: u64,
    copied: u64,
    started_at: u64,
    ended_at: u64,
}

impl Job {
    const fn new() -> Self {
        Job {
            state: ST_IDLE,
            why: WHY_NONE,
            kind: KIND_COPY,
            out: Transcript::new(),
            step: 0,
            writes: 0,
            renames: 0,
            deletes: 0,
            src: [0u8; PATH_MAX],
            slen: 0,
            dst: [0u8; PATH_MAX],
            dlen: 0,
            total: 0,
            copied: 0,
            started_at: 0,
            ended_at: 0,
        }
    }
}

fn reply(ctx: &ServiceContext, out: &[u8]) {
    if let Some(cap) = ctx.take_pending_cap() {
        let _ = ctx.try_send_by_handle(cap, &Message::from_bytes(out));
        // Reclaim it: a reply cap is a one-shot return address handed to us inside the request, and
        // sending on it does not consume it. Leaving it behind burns a cap-table slot per reply.
        ctx.remove_cap(cap);
    }
}

/// Every reply echoes the op it answers at byte 1, so a caller can tell this reply from a stale one
/// left over from an abandoned request. `recorder` learned this the expensive way.
fn reply_op(ctx: &ServiceContext, op: u8, status: u8) {
    reply(ctx, &[status, op]);
}

/// One `fs` round trip. Returns the reply payload WITH the correlation tag already checked and
/// stripped, or `None`.
///
/// THE REPLY IS [tag, status, ..], NOT [status, ..]. `fs` echoes the tag back as byte 0, which is
/// what lets a caller recognise its own reply among the requests it is serving. Reading byte 0 as
/// the status makes every success look like a failure - the file is written and the service reports
/// that it could not be, which is the worst kind of wrong because both halves are convincing. The
/// shell does not hit this only because its `fs_take_tagged` strips the tag before returning.
/// What one `fs` round trip did. The distinction between `Slow` and `Failed` is the whole point:
/// they are different facts about the system and they have different right answers.
enum Fs {
    /// A reply arrived, correlation tag matched, status was OK. Carries the body length.
    Ok(usize),
    /// The deadline passed with no reply. `fs` is alive and busy; the request may still be in its
    /// queue. NEVER treat this as an error for a non-idempotent operation.
    Slow,
    /// The send failed even after reacquiring by name, or `fs` answered something other than OK.
    Failed,
}

/// Map a library outcome onto this service's three-way result.
///
/// `OutcomeUnknown` is `Slow` and NOTHING ELSE is. That distinction - "the deadline passed and `fs`
/// may still act on this" versus "it failed" - is the one this service must never blur, because
/// `fs_idempotent` re-sends on the first and must not on the second.
///
/// Every other error is `Failed` AND IS NAMED IN THE LOG. The previous code collapsed NotFound,
/// NoFilesystem, PermissionDenied and a malformed reply into one word; `fs` had said which, and the
/// answer was thrown away (26.7).
fn fsr(ctx: &ServiceContext, what: &str, r: Result<usize, gs::Error>) -> Fs {
    match r {
        Ok(n) => Fs::Ok(n),
        Err(gs::Error::OutcomeUnknown) => Fs::Slow,
        Err(e) => {
            ctx.log_fmt(format_args!("copier: {} failed - {}", what, e.as_str()));
            Fs::Failed
        }
    }
}

fn fs_delete(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8]) -> Fs {
    fsr(ctx, "delete", fs.delete(path).map(|_| 0))
}

fn fs_mkdir(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8]) -> Fs {
    fsr(ctx, "mkdir", fs.create_dir(path).map(|_| 0))
}

fn fs_write_file(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8], data: &[u8]) -> Fs {
    fsr(ctx, "write", fs.write(path, data).map(|_| 0))
}

fn fs_rename(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8], newname: &[u8]) -> Fs {
    fsr(ctx, "rename", fs.rename(path, newname).map(|_| 0))
}

fn fs_write_new(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8], size: u64) -> Fs {
    fsr(ctx, "allocate", fs.create_sized(path, size).map(|_| 0))
}

/// One positional read. Idempotent - see `fs_idempotent`.
fn fs_read_at(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8], off: u64, out: &mut [u8]) -> Fs {
    fsr(ctx, "read", fs.read_at(path, off, out))
}

/// One positional write into an already-allocated extent. Idempotent - see `fs_idempotent`.
fn fs_write_at(ctx: &ServiceContext, fs: &mut gs::fs::Fs, path: &[u8], off: u64, data: &[u8]) -> Fs {
    fsr(ctx, "write-at", fs.write_at(path, off, data).map(|_| 0))
}

/// An `fs` call that may be RE-SENT when the deadline passes, up to `CHUNK_TRIES`.
///
/// ONLY TWO OPERATIONS MAY USE THIS, and the rule is not "reads are safe": it is that re-sending
/// must not be able to produce a different outcome than sending once.
///
///   - `READ_AT` changes nothing.
///   - `WRITE_AT` writes known bytes at a FIXED offset into an extent that is already allocated.
///     Doing it twice writes the same bytes to the same place. It is idempotent in the strict
///     sense, which is what makes the re-send safe.
///
/// This is deliberately NOT the `op_is_mutating` rule the shell applies to `rename`, `move` and
/// `delete` (carnage §3.5), and the difference is worth being precise about, because "never re-send
/// a mutating request" is otherwise the house rule. Those operations are relative to a state that
/// the first attempt may already have changed - a second `rename` finds nothing where the first
/// left nothing - so a re-send can turn a success into a reported failure. A positional overwrite
/// has no such dependence. `WRITE_NEW` and `DELETE` are NOT idempotent in this sense and do not
/// come through here.
fn fs_idempotent(ctx: &ServiceContext, mut attempt: impl FnMut() -> Fs) -> Fs {
    let mut slow = 0u32;
    loop {
        match attempt() {
            Fs::Slow => {
                slow += 1;
                if slow >= CHUNK_TRIES {
                    return Fs::Slow;
                }
                // Let the machine get on with whatever is holding `fs` - a scrub, another client's
                // transaction - instead of spending the next deadline the same way.
                ctx.yield_cpu();
            }
            other => return other,
        }
    }
}

/// Size of `path`, and whether it is a directory. `None` if it does not exist.
fn fs_stat(fs: &mut gs::fs::Fs, path: &[u8]) -> Option<(u64, bool)> {
    match fs.stat(path) {
        Ok(st) => Some((st.size, st.is_dir)),
        Err(_) => None,
    }
}

/// THE DESTINATION IS DELETED WHEN A COPY DOES NOT FINISH, and this is not tidiness.
///
/// `fs` allocates a file's whole extent up front (`OP_WRITE_NEW`), so an interrupted copy leaves a
/// file of the RIGHT SIZE whose tail is undefined content - the carnage document's own words for
/// what `write-new` guarantees. That is the one outcome worse than no file at all: `dir` shows the
/// expected bytes, `read` returns something, and nothing anywhere says the tail is garbage. A
/// cancelled or failed copy therefore removes what it made, and the shell reports that it did.
fn discard_partial(ctx: &ServiceContext, fs: &mut gs::fs::Fs, job: &Job) {
    if job.dlen == 0 {
        return;
    }
    let mut sink = [0u8; 8];
    // NOT through `fs_idempotent`: a delete is exactly the operation whose re-send can report a
    // failure for work that succeeded (carnage §3.5).
    let gone = matches!(fs_delete(ctx, fs, &job.dst[..job.dlen]), Fs::Ok(_));
    if gone {
        ctx.log("copier: removed the partial destination (an unfinished copy is a full-size file with an undefined tail)");
    } else {
        // A failed cleanup is still a failure and must stay as visible as the one that caused it
        // (§26.7). Saying nothing here would leave exactly the misleading file this function exists
        // to prevent, with nobody told.
        ctx.log("copier: COULD NOT remove the partial destination - it is full-size with an undefined tail, delete it before trusting it");
    }
}

/// Copy one chunk. Returns when the job's state has been advanced.
fn copy_chunk(ctx: &ServiceContext, fs: &mut gs::fs::Fs, job: &mut Job) {
    if job.copied >= job.total {
        job.state = ST_DONE;
        job.ended_at = ctx.epoch_secs_monotonic() as u64;
        ctx.log_fmt(format_args!("copier: done, {} bytes", job.total));
        return;
    }
    let mut chunk = [0u8; IO_CHUNK];
    // The typed read returns the COUNT. The length-prefix decode that used to live here - a `want`
    // from the first four bytes, a `have` clamped three ways, a copy out of a staging buffer - was
    // the wire format leaking into this service, and it is the library's now.
    let off = job.copied;
    let (src, slen) = (job.src, job.slen);
    let r = fs_idempotent(ctx, || fs_read_at(ctx, fs, &src[..slen], off, &mut chunk));
    let got = match r {
        Fs::Ok(n) => n,
        Fs::Slow => {
            job.state = ST_FAILED;
            job.why = WHY_UNANSWERED;
            job.ended_at = ctx.epoch_secs_monotonic() as u64;
            ctx.log("copier: `fs` did not answer a read within the retry budget");
            discard_partial(ctx, fs, job);
            return;
        }
        _ => {
            job.state = ST_FAILED;
            job.why = WHY_READ;
            job.ended_at = ctx.epoch_secs_monotonic() as u64;
            discard_partial(ctx, fs, job);
            return;
        }
    };
    if got == 0 {
        // Short of the declared size but the source has no more to give. The destination's extent
        // was allocated for `total`, so its tail is undefined - the same trap as a cancel, and it
        // gets the same answer rather than a quiet "done".
        job.state = ST_FAILED;
        job.why = WHY_READ;
        job.ended_at = ctx.epoch_secs_monotonic() as u64;
        ctx.log("copier: the source ended early - it changed under the copy");
        discard_partial(ctx, fs, job);
        return;
    }

    let (dst, dlen) = (job.dst, job.dlen);
    let w = fs_idempotent(ctx, || fs_write_at(ctx, fs, &dst[..dlen], off, &chunk[..got]));
    match w {
        Fs::Ok(_) => {}
        Fs::Slow => {
            job.state = ST_FAILED;
            job.why = WHY_UNANSWERED;
            job.ended_at = ctx.epoch_secs_monotonic() as u64;
            ctx.log("copier: `fs` did not answer a write within the retry budget");
            discard_partial(ctx, fs, job);
            return;
        }
        Fs::Failed => {
            job.state = ST_FAILED;
            job.why = WHY_WRITE;
            job.ended_at = ctx.epoch_secs_monotonic() as u64;
            discard_partial(ctx, fs, job);
            return;
        }
    }
    job.copied += got as u64;
    if job.copied >= job.total {
        job.state = ST_DONE;
        job.ended_at = ctx.epoch_secs_monotonic() as u64;
        ctx.log_fmt(format_args!("copier: done, {} bytes", job.total));
    }
}

/// One churn iteration: write a slot, and every fifth one rename it and delete the result.
///
/// ONE ITERATION PER LOOP PASS, for the reason a copy does one chunk: this service is
/// single-threaded, and a job that did its whole run inside one call would answer no status and no
/// cancel for its entire duration.
///
/// The mix is the shell's, deliberately: a whole-file write takes the journal path, while rename and
/// delete move directory entries and free extents, which is where the interesting interrupted states
/// live. Writing only files would exercise one transaction shape while claiming three.
fn churn_step(ctx: &ServiceContext, fs: &mut gs::fs::Fs, job: &mut Job) {
    const DIR: &[u8] = b"/churn";
    const SLOTS: u64 = 8;
    const SIZES: [usize; 4] = [64, 500, 1200, 3000];

    let elapsed = (ctx.epoch_secs_monotonic() as u64).saturating_sub(job.started_at);
    if elapsed >= job.total {
        job.state = ST_DONE;
        // THE CLOCK STOPS AT THE FULL DURATION. Without this the last figure recorded was the
        // second before the deadline, so a finished churn sat at 91% - which reads as a run that
        // stopped short rather than one that completed.
        job.copied = job.total;
        job.ended_at = ctx.epoch_secs_monotonic() as u64;
        let mut line = [0u8; 160];
        let n = render_churn(&mut line, job);
        job.out.write(&line[..n]);
        return;
    }
    job.copied = elapsed;

    let i = job.step;
    job.step += 1;
    let slot = (i % SLOTS) as usize;
    let n = SIZES[(i as usize / SLOTS as usize) % SIZES.len()];

    let mut buf = [0u8; 3000];
    let gen = godspeed_sdk::churn::generation(i);
    godspeed_sdk::churn::fill(&mut buf[..n], gen);

    let mut path = [0u8; 32];
    let mut pl = 0usize;
    for &b in DIR { path[pl] = b; pl += 1; }
    path[pl] = b'/'; pl += 1;
    path[pl] = b'f'; pl += 1;
    path[pl] = b'0' + slot as u8; pl += 1;
    path[pl..pl + 4].copy_from_slice(b".bin");
    pl += 4;

    let mut sink = [0u8; 8];
    if matches!(fs_write_file(ctx, fs, &path[..pl], &buf[..n]), Fs::Ok(_)) {
        job.writes += 1;
    }

    if i % 5 == 4 {
        let mut np = [0u8; 32];
        np[..pl].copy_from_slice(&path[..pl]);
        np[pl - 4..pl].copy_from_slice(b".ren");
        // OP_RENAME takes the NEW NAME, not a path: the slice starts after the final `/`, or every
        // rename is refused for containing a slash and the run silently becomes writes-only.
        let name_at = DIR.len() + 1;
        if matches!(fs_rename(ctx, fs, &path[..pl], &np[name_at..pl]), Fs::Ok(_)) {
            job.renames += 1;
            if matches!(fs_delete(ctx, fs, &np[..pl]), Fs::Ok(_)) {
                job.deletes += 1;
            }
        }
    }
}

/// What the run DID, not that it ran.
fn render_churn(out: &mut [u8; 160], job: &Job) -> usize {
    use core::fmt::Write as _;
    struct Sink<'a> { buf: &'a mut [u8; 160], n: usize }
    impl core::fmt::Write for Sink<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &c in s.as_bytes() {
                if self.n < self.buf.len() { self.buf[self.n] = c; self.n += 1; }
            }
            Ok(())
        }
    }
    let mut sink = Sink { buf: out, n: 0 };
    let _ = write!(sink, "churn: {} writes, {} renames, {} deletes in {}s\n",
                   job.writes, job.renames, job.deletes, job.total);
    sink.n
}

/// Render a check/scrub verdict into the transcript. Numbers in, one line out.
///
/// `write!` into a fixed array rather than digit-by-digit arithmetic: `format_args!` does not
/// allocate, and §26.6.1 says so explicitly - hand-rolling number formatting to avoid a heap that
/// was never involved is itself the mistake that section warns about.
fn render_verdict(out: &mut [u8; 160], files: u32, dirs: u32, bad: u32,
                  free: u64, stored_before: u64) -> usize {
    use core::fmt::Write as _;
    struct Sink<'a> { buf: &'a mut [u8; 160], n: usize }
    impl core::fmt::Write for Sink<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for &c in s.as_bytes() {
                if self.n < self.buf.len() { self.buf[self.n] = c; self.n += 1; }
            }
            Ok(())
        }
    }
    let mut sink = Sink { buf: out, n: 0 };
    // BAD FIRST WHEN THERE IS ANY. The one number that changes what the operator does next should
    // not be the third clause of a sentence that opens with a file count.
    if bad > 0 {
        let _ = write!(sink, "{} BAD block(s) - {} file(s), {} director(ies) scanned\n", bad, files, dirs);
    } else {
        let _ = write!(sink, "ok - 0 bad, {} file(s), {} director(ies) scanned\n", files, dirs);
    }
    // THE REPAIR QUESTION, which is why a check is usually run after a crash. The superblock's count
    // before the rebuild against the tree's count after it: equal means the accounting was already
    // right, and any difference is named with its direction, because the two directions have
    // opposite consequences. Higher-than-truth means a block the tree owns was considered free.
    if stored_before == free {
        let _ = write!(sink, "the free count already agreed with the tree - nothing was repaired\n");
    } else if stored_before > free {
        let _ = write!(sink, "REPAIRED the FREE COUNT - the superblock claimed {} free, the tree says {} (counted too much free space, off by {})\n",
                       stored_before, free, stored_before - free);
    } else {
        let _ = write!(sink, "REPAIRED the FREE COUNT - the superblock claimed {} free, the tree says {} (counted too little free space, off by {})\n",
                       stored_before, free, free - stored_before);
    }
    sink.n
}

/// [CP_OK, CP_OP_STATUS, state, why, kind, copied:u64, total:u64, elapsed:u64, slen, src.., dlen, dst..]
fn reply_status(ctx: &ServiceContext, job: &Job) {
    let mut out = [0u8; 4 + 24 + 2 + 2 * PATH_MAX];
    out[0] = CP_OK;
    out[1] = CP_OP_STATUS;
    out[2] = job.state;
    out[3] = job.why;
    out[4] = job.kind;
    out[5..13].copy_from_slice(&job.copied.to_le_bytes());
    out[13..21].copy_from_slice(&job.total.to_le_bytes());
    // MEASURED, not predicted. A finished job's clock stops at `ended_at`, so `jobs` does not show
    // a `done` row whose elapsed time keeps climbing - which reads as "still working" at a glance
    // and is the kind of small lie that costs an operator a real minute.
    let now = ctx.epoch_secs_monotonic() as u64;
    let end = if job.state == ST_RUNNING || job.state == ST_IDLE { now } else { job.ended_at };
    out[21..29].copy_from_slice(&end.saturating_sub(job.started_at).to_le_bytes());
    let mut n = 29;
    out[n] = job.slen as u8;
    n += 1;
    out[n..n + job.slen].copy_from_slice(&job.src[..job.slen]);
    n += job.slen;
    out[n] = job.dlen as u8;
    n += 1;
    out[n..n + job.dlen].copy_from_slice(&job.dst[..job.dlen]);
    n += job.dlen;
    reply(ctx, &out[..n]);
}

#[allow(unsafe_code)] // the exported entry symbol - see the crate attribute
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    ctx.trace_as("copier");
    // ONE filesystem handle for the life of the service, because the correlation tag belongs to the
    // CHANNEL and must differ between consecutive requests. The previous code used a CONSTANT tag,
    // so a late reply to a request that had already timed out passed its own check and was read as
    // the answer to the next one - and this service has explicit `Slow` handling, so that path is
    // reachable rather than theoretical.
    let mut gfs = gs::fs::Fs::new(&ctx);
    let mut job = Job::new();
    let wait = ctx.duration_cycles(IDLE_MS);
    ctx.log("copier: ready (idle - `background copy <src> <dst>` begins a copy)");

    loop {
        // WHILE COPYING, DO NOT SLEEP. `recv_timeout` parks between messages, which is right when
        // idle and hopeless while copying - it would cap the transfer at one chunk per park, so a
        // megabyte would take minutes of wall time doing nothing. Non-blocking here instead, so the
        // copy runs at whatever rate the device allows while `jobs` and `foreground` are still
        // answered every iteration. Exactly the shape `recorder`'s pre-fill needed, for the same
        // reason and after the same mistake.
        let running = job.state == ST_RUNNING;
        let incoming = if running { ctx.try_recv() } else { ctx.recv_timeout(wait) };

        if let Some(msg) = incoming {
            let p = msg.payload_bytes();
            let op_code = p.first().copied().unwrap_or(0xFF);
            match p.first().copied() {
                Some(CP_OP_START) if p.len() >= 4 => {
                    if job.state == ST_RUNNING {
                        // One at a time, refused rather than queued (§26.6).
                        reply_op(&ctx, op_code, CP_ERR);
                        continue;
                    }
                    let kind = p[1];
                    let slen = (p[2] as usize).min(PATH_MAX);
                    if p.len() < 3 + slen + 1 {
                        reply_op(&ctx, op_code, CP_ERR);
                        continue;
                    }
                    let dlen = (p[3 + slen] as usize).min(PATH_MAX);
                    if p.len() < 4 + slen + dlen {
                        reply_op(&ctx, op_code, CP_ERR);
                        continue;
                    }
                    // A TRAILING u64 PARAMETER, read only by the kinds that take one. Appending it
                    // rather than threading it through every kind's fields keeps the kinds that
                    // have no parameter unchanged.
                    let pend = 4 + slen + dlen;
                    let param = if p.len() >= pend + 8 {
                        u64::from_le_bytes([p[pend], p[pend + 1], p[pend + 2], p[pend + 3],
                                            p[pend + 4], p[pend + 5], p[pend + 6], p[pend + 7]])
                    } else { 0 };
                    let mut fresh = Job::new();
                    fresh.kind = kind;
                    fresh.slen = slen;
                    fresh.src[..slen].copy_from_slice(&p[3..3 + slen]);
                    fresh.dlen = dlen;
                    fresh.dst[..dlen].copy_from_slice(&p[4 + slen..4 + slen + dlen]);

                    // `drives check` IS ONE REQUEST TOO, and it is the first job whose whole
                    // product is a report. `fs` walks the volume and answers with a verdict; this
                    // service renders it into the transcript, and `foreground` replays it. The
                    // rendering is deliberately the SUMMARY only - the per-block detail `fs` logs
                    // goes to the log floor as it always has, and duplicating it here would be a
                    // second copy of a truth that already has an owner (§26.4).
                    if kind == KIND_CHURN {
                        // Bounded by the caller's duration, and by this service's own ceiling: an
                        // unbounded churn is a service that never stops writing to somebody's disk.
                        fresh.total = if param == 0 || param > 3600 { 30 } else { param };
                        fresh.state = ST_RUNNING;
                        fresh.started_at = ctx.epoch_secs_monotonic() as u64;
                        job = fresh;
                        job.out.clear();
                        job.out.write(b"churn: writing continuously - CUT THE POWER AT ANY POINT\n");
                        let mut sink = [0u8; 8];
                        let _ = fs_mkdir(&ctx, &mut gfs, b"/churn");
                        reply_op(&ctx, op_code, CP_OK);
                        continue;
                    }

                    if kind == KIND_CHECK || kind == KIND_SCRUB {
                        fresh.state = ST_RUNNING;
                        fresh.started_at = ctx.epoch_secs_monotonic() as u64;
                        job = fresh;
                        job.out.clear();
                        job.out.write(if kind == KIND_SCRUB {
                            b"drives scrub - verifying every block's CRC\n" as &[u8]
                        } else {
                            b"drives check - walking the volume\n" as &[u8]
                        });
                        reply_op(&ctx, op_code, CP_OK);
                        // A SCRUB READS THE WHOLE VOLUME, so it gets the sweep budget rather than
                        // the chunk one. Five seconds was not nearly enough and said so in the
                        // worst possible way - by blaming the filesystem. `gs::fs::SWEEP_SECS` is
                        // the library's name for the same number this service arrived at.
                        //
                        // The verdict is TYPED now. What used to live here - a shared prefix decoded
                        // by hand, an inline closure reading u64s at computed offsets, a length
                        // check deciding whether the accounting was present - was the wire format in
                        // this service. What is left is the only part that was ever copier's: which
                        // verdict to render.
                        let verdict = if kind == KIND_SCRUB {
                            gfs.scrub().map(|v| (v.files, v.dirs, v.bad, 0u64, 0u64))
                        } else {
                            gfs.check().map(|v| (v.files, v.dirs, v.bad, v.free, v.free_before))
                        };
                        job.ended_at = ctx.epoch_secs_monotonic() as u64;
                        match fsr(&ctx, if kind == KIND_SCRUB { "scrub" } else { "check" },
                                  verdict.map(|_| 0)) {
                            Fs::Ok(_) => {
                                // Unwrap is not reachable: `fsr` returned Ok only because this did.
                                let (files, dirs, bad, free, before) =
                                    verdict.unwrap_or((0, 0, 0, 0, 0));
                                let mut line = [0u8; 160];
                                let len = render_verdict(&mut line, files, dirs, bad, free, before);
                                job.out.write(&line[..len]);
                                job.state = ST_DONE;
                            }
                            Fs::Slow => {
                                job.out.write(b"check: fs did not answer within the budget\n");
                                job.state = ST_FAILED;
                                job.why = WHY_UNANSWERED;
                            }
                            Fs::Failed => {
                                job.out.write(b"check: FAILED - the filesystem reported a problem\n");
                                job.state = ST_FAILED;
                                job.why = WHY_READ;
                            }
                        }
                        continue;
                    }

                    // A RECURSIVE DELETE IS ONE REQUEST, so it is started and finished right here
                    // rather than advanced in the loop below. The reply still goes back first:
                    // `fs` does the walk, this service just waits for it, and the caller must not
                    // wait for both.
                    if kind == KIND_DELETE_TREE {
                        fresh.state = ST_RUNNING;
                        fresh.started_at = ctx.epoch_secs_monotonic() as u64;
                        job = fresh;
                        job.out.clear();
                        job.out.write(b"deleting a subtree\n");
                        ctx.log("copier: deleting a subtree");
                        reply_op(&ctx, op_code, CP_OK);
                        let mut sink = [0u8; 8];
                        // Not through `fs_idempotent`: a re-sent tree delete finds nothing the
                        // second time and reports a failure for work that succeeded - carnage §3.5
                        // exactly.
                        // `delete_all` carries the sweep budget itself, so the tree deadline this
                        // service used to pass in is the library's now - and it is the same number.
                        let outcome = fsr(&ctx, "delete-tree",
                                          gfs.delete_all(&job.src[..job.slen]).map(|_| 0));
                        let _ = &mut sink;
                        job.ended_at = ctx.epoch_secs_monotonic() as u64;
                        match outcome {
                            Fs::Ok(_) => {
                                job.state = ST_DONE;
                                ctx.log("copier: subtree deleted");
                            }
                            Fs::Slow => {
                                job.state = ST_FAILED;
                                job.why = WHY_UNANSWERED;
                                ctx.log("copier: `fs` did not finish the subtree delete within the budget");
                            }
                            Fs::Failed => {
                                job.state = ST_FAILED;
                                job.why = WHY_NO_SOURCE;
                                ctx.log("copier: the subtree delete failed");
                            }
                        }
                        continue;
                    }

                    // STAT AND ALLOCATE, THEN ANSWER - never copy before replying. The caller is
                    // waiting on this reply to print a job id and give the prompt back, and the
                    // whole point of the feature is that it does not wait for the transfer. Both
                    // calls here are one round trip each.
                    let (size, is_dir) = match fs_stat(&mut gfs, &fresh.src[..slen]) {
                        Some(v) => v,
                        None => {
                            fresh.state = ST_FAILED;
                            fresh.why = WHY_NO_SOURCE;
                            job = fresh;
                            reply_op(&ctx, op_code, CP_ERR);
                            continue;
                        }
                    };
                    if is_dir {
                        // A subtree copy is a WALK, and a walk that is interrupted leaves a prefix
                        // of a tree - a different permitted-outcome question than one file, and one
                        // nothing here answers. Refused rather than half-supported (§26.2).
                        fresh.state = ST_FAILED;
                        fresh.why = WHY_NO_SOURCE;
                        job = fresh;
                        ctx.log("copier: refused - the source is a directory, and a detached subtree copy is not built");
                        reply_op(&ctx, op_code, CP_ERR);
                        continue;
                    }
                    fresh.total = size;
                    let mut sink = [0u8; 8];
                    // NOT idempotent: a second `WRITE_NEW` for the same path fails because the
                    // first one succeeded, so it does not go through `fs_idempotent`.
                    if !matches!(fs_write_new(&ctx, &mut gfs, &fresh.dst[..dlen], size), Fs::Ok(_)) {
                        fresh.state = ST_FAILED;
                        fresh.why = WHY_NO_DEST;
                        job = fresh;
                        reply_op(&ctx, op_code, CP_ERR);
                        continue;
                    }
                    fresh.state = ST_RUNNING;
                    fresh.started_at = ctx.epoch_secs_monotonic() as u64;
                    job = fresh;
                    job.out.clear();
                    job.out.write(b"copying\n");
                    ctx.log_fmt(format_args!("copier: copying {} bytes", size));
                    reply_op(&ctx, op_code, CP_OK);
                }
                Some(CP_OP_CANCEL) => {
                    // A DELETE JOB CANNOT BE REACHED HERE AT ALL, and that is worth stating rather
                    // than leaving to be inferred: the tree delete is one blocking `fs` request, so
                    // while it runs this service is not reading its endpoint and a cancel simply
                    // waits behind it. By the time it is read the job has ended. There is no
                    // half-measure available - `fs` owns the walk - and pretending otherwise would
                    // be a cancel that says it worked and did nothing.
                    if job.state == ST_RUNNING {
                        job.state = ST_CANCELLED;
                        job.ended_at = ctx.epoch_secs_monotonic() as u64;
                        if job.kind == KIND_COPY {
                            discard_partial(&ctx, &mut gfs, &job);
                        }
                        ctx.log("copier: cancelled");
                    }
                    reply_op(&ctx, op_code, CP_OK);
                }
                Some(CP_OP_STATUS) => reply_status(&ctx, &job),
                Some(CP_OP_OUTPUT) => {
                    // PAGED, because one message is 4 KiB and the ring is 4 KiB: the caller asks
                    // for a byte offset and gets what fits. A transcript that could not be read
                    // back in full would be the same silent truncation the drop counter exists to
                    // prevent, one layer along.
                    let off = if p.len() >= 5 {
                        u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize
                    } else { 0 };
                    let mut out = [0u8; 16 + 2048];
                    out[0] = CP_OK;
                    out[1] = CP_OP_OUTPUT;
                    out[2..6].copy_from_slice(&job.out.dropped.to_le_bytes());
                    out[6..10].copy_from_slice(&(job.out.len as u32).to_le_bytes());
                    let start = off.min(job.out.len);
                    let n = (job.out.len - start).min(2048);
                    out[10..10 + n].copy_from_slice(&job.out.buf[start..start + n]);
                    reply(&ctx, &out[..10 + n]);
                }
                _ => reply_op(&ctx, 0xFF, CP_ERR),
            }
        }

        if job.state == ST_RUNNING {
            match job.kind {
                KIND_COPY => copy_chunk(&ctx, &mut gfs, &mut job),
                KIND_CHURN => churn_step(&ctx, &mut gfs, &mut job),
                _ => {}
            }
        }
    }
}
