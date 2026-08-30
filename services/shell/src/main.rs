// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
#![no_main]
//! The shell - the user's interface to GodspeedOS, and a capability broker (Appendix B.3).
//!
//! This is the largest file in the repository, so read this before scrolling. It is one file because
//! the shell is one task with one input loop; splitting it by command would scatter that loop across
//! modules without making any command easier to find.
//!
//! **What it is.** Not a Unix shell: there is no fork, no exec, no inherited descriptors, no ambient
//! stdin. It reads the console, parses a line, and either answers from its own state or asks a SERVICE
//! over IPC - `fs` for files, `block-driver` for disks, the supervisor for spawning. Every authority it
//! passes to a child is one it holds and grants explicitly.
//!
//! **How it is laid out.** A prompt/input loop, a command table dispatching to `cmd_*` functions (one
//! per built-in), the gsh scripting language (`docs/`-documented: vars, if, for, fn, pipes), and the
//! pipe machinery that composes built-ins with `|`.
//!
//! **What to know before editing.** The user stack is 256 KiB (`USER_STACK_PAGES` = 64 x 4 KiB, in
//! `kernel/src/task/mod.rs`) and `pipe_run`'s frame already sits near it - measured at 177,297 bytes,
//! 68%, on ENTRY, before any stage buffer is added. This line said 64 KiB for a long time and was
//! simply wrong; it then misled the first instrument written to check it into reporting 270%.
//! A large local in a command function can overflow the rest - mark record-builders `#[inline(never)]`
//! (see the shell-stack note in `docs/`). There is no heap (§26.6.1): fixed arrays, bounded arenas, and
//! streaming in `IO_CHUNK` pieces. And the shell is restartable like everything else - a crash gives a
//! fresh prompt, losing the in-flight command but not the session (§6.2).

use godspeed_sdk::{ServiceContext, CapInfo, CapHandle, Message, IpcError, ReqOutcome, ClockSource, Datetime};
use godspeed_sdk::record::{Table, Value, RecordSink, parse_predicate, AggOp, AggErr, REC_MAX_ROWS, REC_ARENA};

/// Per-iteration sleep for the muted loop (a foreground app owns the console), in TSC cycles
/// (~30 ms at 2 GHz; QEMU's 1-tick fallback makes it ~one quantum). Matches the `observe` q-poll
/// cadence - long enough that the core halts between checks, short enough that regaining the
/// keyboard reprints the prompt with no perceptible delay.
const MUTED_POLL_MS: u64 = 30;

/// Longest command line the editor accepts.
///
/// A FIXED ceiling is deliberate - the shell has no heap, so the line, the history and the history
/// merge are all fixed arrays (§26.6). But it must be generous enough that a real command never meets
/// it: at 128 an ordinary long path plus arguments, or a four-stage pipe, ran out of room mid-typing,
/// which reads as a broken keyboard. 256 is past anything the utilities actually need while staying
/// cheap - the constant multiplies by HIST_MAX (16) in `History` AND in the session-merge buffer, so
/// this doubling costs about 4 KiB of the shell's stack, not 4 KiB per line. Reaching it is now
/// audible (BEL in `insert`) rather than silent, so the ceiling is honest either way.
const MAX_LINE: usize = 256;
/// Tokens the command tokenizer keeps. Anything past this is SILENTLY DROPPED, so the ceiling has to
/// sit above every real command shape - at 4 it did not: `drives flash 0 data force` is five tokens, so
/// the `force` an operator explicitly typed never reached the parser and the disk was refused as though
/// it had been omitted. (The same limit had already forced a hand-rolled re-parse elsewhere in this
/// file for a six-token command - a standing sign the ceiling was too low.) Eight covers every command
/// the utilities define, for a few dozen bytes of stack per argument array.
const MAX_ARGS: usize = 8;

// fs API (shell <-> fs). MUST match `services/fs`.
//   File ops:   [op, path_len:u8, path[path_len], (WriteFile: data)]
const OP_WRITE_FILE: u8 = 10;
const OP_READ_FILE: u8 = 11;
const OP_STAT_FILE: u8 = 12;
const OP_MKDIR: u8 = 13;
const OP_LIST_DIR: u8 = 14;
const OP_RENAME: u8 = 15;
const OP_DELETE: u8 = 16;
const OP_MOVE: u8 = 17;
const OP_MKDIR_P: u8 = 18;
const OP_DELETE_TREE: u8 = 19; // delete a file or a whole subtree (recursive)
// drives ops:
const OP_DRIVES_INFO: u8 = 20;
const OP_FLASH: u8 = 21;
const OP_LABEL: u8 = 22;
const OP_RESET: u8 = 23;
const OP_CHECK: u8 = 27; // fsck: rebuild bitmap+free from the tree, report CRC failures
const OP_SCRUB: u8 = 29; // scrub: read-only CRC integrity sweep (reports, changes nothing)
// large-file streaming ops (offset-addressed): create a sized file, then write/read chunks.
const OP_WRITE_NEW: u8 = 24; // [op, plen, path, total:u64]
const OP_WRITE_AT: u8 = 25;  // [op, plen, path, offset:u64, chunk]
const OP_READ_AT: u8 = 26;   // [op, plen, path, offset:u64, len:u32] -> [FS_OK, n:u32, bytes]
// One streaming chunk: the most file bytes carried per message (matches fs MAX_FILE_BYTES =
// 7 data-block payloads). Must be a multiple of the 508-byte data payload so WRITE_AT offsets
// stay block-aligned (no read-modify-write).
const IO_CHUNK: usize = 7 * 508; // 3556
const FS_OK: u8 = 0;
const FS_ERR: u8 = 1;       // fs could not complete the operation (typically a device I/O error)
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3;
const FS_UNAVAIL: u8 = 4;   // present-but-unreadable storage: do NOT flash (data may be intact)
const FS_FOREIGN: u8 = 6; // fs refused a destructive op: the disk holds a foreign partition table or
                          // boot sector (on a single-disk board, the very disk it boots from)
const FS_DENIED: u8 = 5; // file-cap op needs a right the cap lacks (non-escalation, §7.3); DISTINCT
                         // from FS_UNAVAIL(4) so a client can tell "denied" from "storage down" (audit L2)
// File-as-capability (§7.10, P2): Open mints a file cap; the holder invokes it (FOP_*).
const OP_OPEN: u8 = 30;  // [op, plen, path, rights:u8] → [FS_OK] + embedded FILE CAP
const FOP_READ: u8 = 1;  // [FOP_READ, offset:u64, len:u32]  (needs READ)
const FOP_WRITE: u8 = 2; // [FOP_WRITE, offset:u64, chunk…]  (needs WRITE)
const FOP_CLOSE: u8 = 4; // [FOP_CLOSE] → revoke the resource
const RIGHT_READ: u8 = 1 << 0;
const RIGHT_WRITE: u8 = 1 << 1;
const LABEL_MAX: usize = 31;
const PATH_MAX: usize = 120; // fits in MAX_LINE; path_len is u8

// ── pipe output capture ────────────────────────────────────────────────────────
// When a built-in is the *producer* side of a pipe (`read /f | upper`), its text is captured
// into one message-sized buffer instead of going to the console (§26.6: bounded; loud if the
// output overflows). The captured bytes are then sent to the sink (a service endpoint or the
// `write` built-in). Only produced *text* flows through `Out`; errors always go to the console.
// End-of-stream marker a producer service sends to a built-in sink (the shell draining a
// `service | write` pipe). A non-empty sentinel - the IPC path doesn't deliver an empty body.
const PIPE_EOT: u8 = 0x04; // ASCII EOT
// One pipe stage's buffer.
//
// 16 KiB, and this number is now MEASURED rather than argued. It was 64 KiB; it was cut to 16 on a
// theory, reverted when the user rightly pointed out the theory had no evidence behind it, and is
// back because the kernel's EL0 stack dump finally caught the fault in the act:
//
//     > roster | select name seat | to json | assert contains Luke
//     *** aarch64 EXCEPTION  ELR_EL1 = 0x0  x30 = 0x0
//         SP_EL0 = 0x7ffd2a90   x29 = 0x7fffb150
//         stack at SP_EL0: 16 words, ALL ZERO
//
// Read off those registers: the stack is 185,200 bytes deep (71% of 256 KiB), the caller's frame
// sits at 20,144, so the frame being entered is ~165 KiB BY ITSELF, and the zeros at SP are its own
// stack-probe region. That is `pipe_run` (143,360 at 64 KiB) plus a stage, in the one pipeline that
// combines the two largest frames in this file - pipe_run and cmd_roster (90,112). Their sum is
// 233,472 of a 262,144 stack before a single caller is counted.
//
// At 16 KiB the same two are 45,056 and 40,960. That is the whole fix: the frames were never
// survivable together, and the margin was thin enough that it tipped on what happened to be on the
// stack already - which is why it looked intermittent and boot-dependent for days.
//
// Cost, unchanged from when this was 64 KiB: a producer whose output exceeds the buffer is TRUNCATED
// and says so. Only the console can accept more than PIPE_MSG_MAX anyway; every other sink clips at
// 4 KiB regardless of this number. Lifting it for real is the streaming work (docs/pipes.md).
const CAP_MAX: usize = 16 * 1024;
// The pipe buffer must stay wider than a single IPC message, or a stage crossing a service boundary
// truncates a message it was handed whole - a bound that would look like a service bug rather than a
// buffer that was shrunk past its floor. Pinned so the next person to tune CAP_MAX for stack cannot
// take it below the one value it genuinely cannot go below.
const _: () = assert!(CAP_MAX >= 4 * PIPE_MSG_MAX,
    "CAP_MAX must leave room for several IPC messages; see the note above it");

// A single IPC message body (= sdk MAX_PAYLOAD). A stage that must cross a service boundary is
// bounded by this until pipe streaming chunks across messages.
const PIPE_MSG_MAX: usize = 4096;
struct Cap {
    buf: [u8; CAP_MAX],
    len: usize,
    overflow: bool,
}
impl Cap {
    fn new() -> Self { Cap { buf: [0u8; CAP_MAX], len: 0, overflow: false } }
    fn push(&mut self, b: &[u8]) {
        let room = CAP_MAX - self.len;
        let take = b.len().min(room);
        if take < b.len() { self.overflow = true; }
        self.buf[self.len..self.len + take].copy_from_slice(&b[..take]);
        self.len += take;
    }
    fn bytes(&self) -> &[u8] { &self.buf[..self.len] }
}
impl core::fmt::Write for Cap {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { self.push(s.as_bytes()); Ok(()) }
}

/// A producer built-in's output target: straight to the console, or into a capture buffer
/// when the built-in feeds a pipe. Methods take `ctx` (the console case needs it; the capture
/// case ignores it).
enum Out<'a> {
    Console,
    Capture(&'a mut Cap),
    /// A utility writing its OWN output to a file (`selfcheck save <path>`, `run … save <path>`).
    /// Accumulates into a bounded report buffer that is written to the file in one streamed pass
    /// when the run finishes - direct, NOT through the pipe, so an orchestrator (which runs its own
    /// sub-pipelines) can save its output without the nested-capture stack overflow that piping it
    /// causes. No heap; the bound is loud (§26.6).
    File(&'a mut ReportBuf),
    /// A captured function body's output (`let x = $(myfn …)`). The CaptureCall frame points a
    /// statement's `out` here; on the function's return the buffer becomes the variable's value.
    FnCap(&'a mut FnCapBuf),
}
impl Out<'_> {
    /// Write a string, no trailing newline.
    fn put(&mut self, ctx: &ServiceContext, s: &str) {
        match self {
            Out::Console => console_write_chunked(ctx, s.as_bytes()),
            Out::Capture(c) => c.push(s.as_bytes()),
            Out::File(r) => r.push(s.as_bytes()),
            Out::FnCap(c) => c.push(s.as_bytes()),
        }
    }
    /// Write raw bytes, no trailing newline (file content may not be clean UTF-8).
    fn put_bytes(&mut self, ctx: &ServiceContext, b: &[u8]) {
        match self {
            Out::Console => console_write_chunked(ctx, b),
            Out::Capture(c) => c.push(b),
            Out::File(r) => r.push(b),
            Out::FnCap(c) => c.push(b),
        }
    }
    /// Write a string followed by a newline.
    fn line(&mut self, ctx: &ServiceContext, s: &str) {
        self.put(ctx, s);
        self.put(ctx, "\n");
    }
    /// Write formatted args followed by a newline.
    fn line_fmt(&mut self, ctx: &ServiceContext, args: core::fmt::Arguments) {
        match self {
            Out::Console => ctx.console_writeln_fmt(args),
            Out::Capture(c) => { let _ = core::fmt::write(c, args); c.push(b"\n"); }
            Out::File(r) => { let _ = core::fmt::write(r, args); r.push(b"\n"); }
            Out::FnCap(c) => { let _ = core::fmt::write(c, args); c.push(b"\n"); }
        }
    }
}

/// A bounded accumulator for a utility's saved report (`selfcheck save <path>`). Fixed stack array,
/// no heap; a report exceeding `REPORT_MAX` sets `overflow` (loud, never a silent truncation -
/// §26.6/§3.12). The size is a deliberate balance: big enough for the self-check transcript
/// (~12 KiB), but small enough that it + a sub-pipeline's transient buffers (a `| assert` is ~128
/// KiB) fit the 256 KiB user stack. That ceiling is the BINDING constraint - 32 KiB overflowed the
/// stack on a `run … save` whose suite has `| assert` lines, 16 KiB fits (QEMU/HW-proven; frames are
/// identical on both). It is the whole reason this is a direct file write, not a (nesting) pipe
/// capture. A truly large report would want a streaming sink (append per chunk); not needed yet.
const REPORT_MAX: usize = 12 * 1024;
struct ReportBuf {
    buf: [u8; REPORT_MAX],
    len: usize,
    overflow: bool,
}
impl ReportBuf {
    fn new() -> Self { ReportBuf { buf: [0u8; REPORT_MAX], len: 0, overflow: false } }
    fn push(&mut self, b: &[u8]) {
        let space = REPORT_MAX - self.len;
        let n = b.len().min(space);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
        if n < b.len() { self.overflow = true; }
    }
    fn bytes(&self) -> &[u8] { &self.buf[..self.len] }
}
impl core::fmt::Write for ReportBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { self.push(s.as_bytes()); Ok(()) }
}

/// Bounded accumulator for `$(fn)` output capture (the `CaptureCall` frame routes a function body's
/// output here). 512 B: this buffer lives in the `run_lines` frame for the WHOLE run (not just during
/// a capture), so it must be small enough to coexist with the heaviest path - `run … save` with a
/// `| assert` line already peaks ~148 KiB co-resident against a 256 KiB user stack (a 4 KiB buffer
/// here overflowed it). 512 B holds the typical captured value (a name, a number, a short line); a
/// bigger one overflows LOUDLY (§26.6), never silently. No heap: scratch space, filled then dropped.
const FNCAP_MAX: usize = 512;
struct FnCapBuf {
    buf: [u8; FNCAP_MAX],
    len: usize,
    overflow: bool,
}
impl FnCapBuf {
    fn new() -> Self { FnCapBuf { buf: [0u8; FNCAP_MAX], len: 0, overflow: false } }
    fn reset(&mut self) { self.len = 0; self.overflow = false; }
    fn push(&mut self, b: &[u8]) {
        let space = FNCAP_MAX - self.len;
        let n = b.len().min(space);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
        if n < b.len() { self.overflow = true; }
    }
    fn bytes(&self) -> &[u8] { &self.buf[..self.len] }
}
impl core::fmt::Write for FnCapBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result { self.push(s.as_bytes()); Ok(()) }
}

// Entry point called by the kernel after spawning this service.
// ctx.console_writeln() appends a newline. The kernel echoes each console keystroke to the
// display (arch::console_push_byte), so we don't echo here - just accumulate
// bytes until \r or \n. (On a serial terminal, turn local echo OFF to avoid
// doubled characters.)
/// The shell's own context: the SDK's `ServiceContext` plus the state the shell owns.
///
/// C6-1. `ServiceContext` is a zero-sized marker that reads the task's context page, so wrapping it
/// costs nothing and `Deref` keeps every existing `ctx.log(...)` working unchanged. Deref coercion also
/// means a `&ShellCtx` still passes to anything expecting a `&ServiceContext`, so adding owned state
/// here did not touch a single call site - only the signatures of the functions that need it.
///
/// `Cell` rather than a lock or an atomic: this is one task on one core, so the cost of thread-safety
/// would buy nothing, and an atomic here is what made the old version look acceptable while still being
/// unowned. Interior mutability inside an owned struct is not global mutable state.
/// The service's user stack, in bytes: `USER_STACK_PAGES = 64` x 4 KiB in `kernel/src/task/mod.rs`.
///
/// **This said 64 KiB in its first version**, taken from the prose at the top of this file rather
/// than from the kernel that allocates the stack - and the first measurement it produced read
/// "177297 of 65536 bytes (270% of the user stack)", a number that is not merely wrong but
/// impossible. A diagnostic that reports 270% of a limit is worse than no diagnostic: it invites the
/// reader to disbelieve the instrument instead of the code.
///
/// It is a duplicated fact either way (Commandment III) - `fs` hardcodes `256 * 1024` at its own
/// stack report for the same reason - and the honest fix is one SDK-side accessor both read. Recorded
/// here rather than quietly spread a third time.
const USER_STACK_BYTES: usize = 64 * 4096;

pub struct ShellCtx {
    inner: ServiceContext,
    /// The fs request correlation tag (see `next_fs_tag`).
    fs_tag: core::cell::Cell<u8>,
    /// Deepest `pipe_run` frame seen so far, in bytes. OWNED here rather than kept in a module-level
    /// `static`, which is the anonymous singleton Invariant 9 forbids - the same mistake that had to be
    /// undone in `xhci` an hour ago, and one this file already avoids for `fs_tag`.
    pipe_stack_hwm: core::cell::Cell<usize>,
}

impl core::ops::Deref for ShellCtx {
    type Target = ServiceContext;
    fn deref(&self) -> &ServiceContext { &self.inner }
}

#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Name this service in the trace ring. It cannot ask what it is called (identity is not ambient),
    // so a traced service says - see `sdk::trace` for why that costs nothing in trust.
    ctx.trace_as("shell");
    // `fs` requests carry a correlation tag at byte 0 (`req[0] = tag`), so the opcode is at byte 1.
    ctx.trace_op_at("fs", 1);
    // C6-1: wrap the SDK context in the shell's own, which owns the fs correlation tag. Everything
    // below still calls `ctx.log(...)` unchanged - `ShellCtx` derefs to `ServiceContext` - and deref
    // coercion lets it pass to anything expecting the SDK type. The tag now has an owner with the same
    // lifetime as the shell, instead of being a `static` that outlives nothing and belongs to no one.
    let ctx = ShellCtx {
        inner: ctx,
        fs_tag: core::cell::Cell::new(0),
        pipe_stack_hwm: core::cell::Cell::new(0),
    };
    let ctx = &ctx;
    // The boot sequence (kernel + every service's logs, the xHCI enumeration) is
    // shown on the TV during startup - the user wants to see it come up. We log our
    // "ready" line into that stream, then wait for the input driver to report in
    // (the deterministic end-of-boot signal) before automatically clearing the TV
    // and presenting a clean prompt - no keypress, no timer.
    for _ in 0..256 {
        ctx.yield_cpu();
    }
    // One atomic console write (text + newline together) so a concurrent driver boot-log
    // can't slip between the message and its newline on the serial console.
    ctx.console_write("shell: ready (type 'help')\n");

    // The clock FLOOR is deliberately NOT read here any more. Doing fs I/O during startup was a mistake:
    // fs is at its slowest right then (mounting, replaying its journal), so the read timed out, and an
    // abandoned fs request poisons the reply channel for the whole session - a reply carries no request
    // id, so the NEXT command consumes it and every command after that is one answer behind. That is what
    // made `drives` report a healthy 15 GB disk as absent, and later made `drives flash` announce a
    // failure for a format that was still running. The floor is read where it is USED instead: `date sync`
    // seeds it before validating a fetched time (see cmd_date), which is a user-initiated moment when fs
    // has long settled and a slow answer is visible rather than silent.

    // Do not wait: note it and carry on. A keystroke arriving later wakes the blocking read.
    if !input_driver_announced(&ctx) {
        ctx.log("shell: input driver not announced yet - prompting anyway (a later keystroke still wakes us)");
    }

    // Boot is done: dismiss the boot screen on the TV (clear + stop mirroring logs
    // to it) and present a clean prompt. Serial keeps the full stream. This is also
    // the first `gsh> ` the serial-driven shell-test waits on.
    ctx.console_boot_complete();

    // The shell owns echo from here on. The kernel's auto-echo (console_push_byte)
    // can only echo single bytes blindly, so it prints the `[` and `A` of an arrow
    // key's `ESC [ A` sequence before the shell consumes them - smearing "[A" onto
    // the line. We turn kernel echo OFF and echo printable bytes ourselves below, so
    // escape sequences are swallowed silently and line editing stays under our control.
    ctx.console_echo(false);
    // A one-time grounding hint above the first prompt after boot - so a fresh user knows
    // where to start. Only here, not on every prompt (that would be noise). Sent as ONE
    // console write so a concurrent driver boot-log can't land between the hint and the
    // prompt (it stays one atomic unit on the serial console too).
    ctx.console_write("(F1=help or type 'help')\ngsh> ");

    let mut line = Line::new();
    // Current location on the (single) drive: the directory bare/relative paths target,
    // moved by `cd` (utilities/17_cd.md). Session state; resets to "/" each boot.
    let mut cwd = Cwd::root();
    // Command history for up/down-arrow recall. `nav == hist.len()` means the live line.
    let mut hist = History::new();
    // History is loaded LAZILY, not here. Touching fs on the startup path would stall the prompt whenever
    // fs is slow/wedged (the shell must come up instantly - history is an enhancement, never a requirement,
    // §26.7), and the only signal the user wants prior history is the up-arrow. So the FIRST up-arrow loads
    // /.gsh_history (bounded, best-effort) and merges it behind the session; startup touches fs zero times.
    // See `History::load` + the up-arrow arm in `handle_csi`. nav starts at 0 (empty ring = live line).
    let mut nav = hist.len();
    // The previous command's result (the Ok/Err model), reported by `result`. Threaded as
    // local session state - no global (services hold no global mutable state, §3.9).
    let mut last_result: Result<(), ShellError> = Ok(());
    // When a foreground app (e.g. the `chaos` service, syscall 40) owns the console, the shell goes
    // "muted": it stays quiet (no prompt, no read) so it can neither smear that app's screen nor
    // swallow its `q`, and prints a fresh prompt only when it regains the keyboard. `muted` tracks it.
    let mut muted = false;
    // Ceiling on the regain drain. The console ring is finite, so this only has to be larger than it;
    // the bound exists so a writer cannot hold the loop, not because the ring is expected to be full.
    const CONSOLE_DRAIN_MAX: usize = 512;
    // Whether this boot's automatic network clock has been written to the on-disk floor yet. One
    // attempt per boot: a failed write is a degraded state to report, not a thing to retry forever.
    let mut clock_floor_recorded = false;
    // Set once the clock is judged never to be coming - see its use below. Separate from
    // `clock_floor_recorded` because the floor genuinely was NOT recorded; this only says stop waiting.
    let mut clock_gaveup = false;
    // Consecutive probes where `time` did not answer at all. See the backoff below.
    let mut clock_silent: u32 = 0;

    loop {
        // Muted: a foreground app owns the console. Sleep + skip - don't draw, don't blocking-read. The
        // Phase-1 kernel gate only covers the non-blocking poll, so THIS loop gate is what keeps the
        // main (blocking) read path from stealing the foreground app's `q`. We SLEEP rather than
        // busy-yield: the trigger is `chaos` (max-carnage / kill-storm), which holds the console for the
        // WHOLE run, so a yield-loop pegged this core for minutes at a stretch. Worse, `yield_cpu`
        // increments CORE_TOTAL_TICKS, so the spin inflated the very denominator every CPU% is divided
        // by - the observer distorting what it observes. Park + wake-on-release is still the endgame;
        // this is the cheap 99% of it. Regain latency stays one sleep, imperceptible at the prompt.
        // The boot clock-floor check runs ABOVE the mute gate and does not wait for a keypress.
        //
        // It used to sit below both, in front of a BLOCKING `console_read` - so it only ran when
        // somebody typed, and it was skipped entirely while a foreground app held the console. The case
        // it exists for is a machine that boots with a cable in, learns the time by SNTP, and then
        // loses power: an UNATTENDED machine, which is exactly the one that never types. The feature was
        // dead on arrival for its own scenario.
        //
        // Cheap: one syscall per pass until it fires, then a plain bool test forever.
        // GIVE UP WAITING once the clock is clearly not coming, and go back to blocking.
        //
        // The polling branch below exists to keep this loop turning until SNTP lands. On a board with
        // no RTC where SNTP never answers - no cable, or a network that will not resolve it - the
        // latch above NEVER fires, so the shell polls every MUTED_POLL_MS (30 ms) FOREVER, with a
        // sleep no keystroke can interrupt. Every character then waits 0-30 ms after the driver has
        // already delivered it, in bursts: exactly the typing stutter reported on hardware, and the
        // reason two fixes inside the USB driver made no visible difference - they are upstream of
        // this quantiser.
        //
        // 30 s is far longer than a successful sync takes (measured: ~5 s from boot on this board when
        // the network answers at all), so this cannot rob a machine that was going to sync. After it,
        // polling can accomplish nothing - the condition it is waiting for cannot become true without
        // a network - so continuing to pay for it is pure cost.
        //
        // Commandment VIII: wait on the truth, but BOUND the wait and act when it does not arrive.
        if !clock_gaveup && ctx.epoch_secs_monotonic() > 30 {
            clock_gaveup = true;
            ctx.log("shell: no network clock after 30s - blocking on input again (the floor stays unrecorded)");
        }
        // `clock_gaveup` GATES THIS TOO. It was set three lines above and then not used here, so the
        // shell kept asking `time` for the clock source on EVERY loop iteration, forever, on any
        // machine that never syncs - which is the normal case with no cable. That is an RPC on the
        // INPUT PATH, ahead of `console_read`.
        //
        // Under a chaos storm it is worse than wasteful: the log shows `time: request had no reply cap
        // - dropping`, so the request cannot be answered at all, and the shell waits out its deadline
        // once per iteration. That is the multi-second pause an operator sees mid-storm, and it sits
        // in front of the keyboard read, so the prompt looks dead while it happens.
        //
        // Once we have given up on the clock there is nothing left to learn, so stop asking.
        // BACK OFF WHEN THE ANSWER IS SILENCE. Asking a service that is not answering costs the full
        // `time_rpc` budget - 2 s, a reacquire, another 2 s - and this sits in front of the keyboard
        // read, so the prompt is dead for that long. Doing it every iteration for thirty seconds is
        // why a machine with no network felt broken at the prompt.
        //
        // An answer that simply is not NTP yet is CHEAP and worth repeating - the clock may still
        // arrive. Silence is expensive and means the service is unreachable, so after three of those
        // we stop asking and say so once. The floor then stays unrecorded, which is a degraded state
        // that is already reported rather than hidden (§26.7) - and an unrecorded floor is a far
        // smaller harm than an interface that stops serving its user while it waits on a dependency.
        // ONE probe per iteration, and its result decides both things: whether to record the floor,
        // and whether asking again is worth what it costs.
        let clock_probe = if !clock_gaveup && !clock_floor_recorded {
            let r = time_source_probe(ctx);
            if r.is_none() {
                clock_silent += 1;
                if clock_silent >= 3 {
                    clock_gaveup = true;
                    ctx.log("shell: `time` is not answering - stopped asking for the clock \
                             (the floor stays unrecorded; the prompt stays responsive)");
                }
            } else {
                clock_silent = 0;
            }
            r
        } else {
            None
        };
        if !clock_gaveup && !clock_floor_recorded && clock_probe == Some(ClockSource::Ntp) {
            clock_floor_recorded = true;
            if let Some(f) = time_floor(ctx) {
                if (0..=u32::MAX as i64).contains(&f) {
                    // NOT quiet. `quiet` is right for the reboot path, where the machine is resetting
                    // and a failed floor is not worth an operator's attention. Here it suppressed both
                    // failure reports while latching the flag before the attempt, so a write that lost
                    // its race with a still-mounting `fs` was neither retried nor mentioned - a
                    // degraded state made invisible (§26.7).
                    clock_floor_persist(&ctx, f as u32, false);
                }
            }
        }

        if !ctx.is_console_foreground() {
            ctx.sleep_ms(MUTED_POLL_MS);
            muted = true;
            continue;
        }
        // Regained the keyboard: a fresh prompt - and exactly ONE.
        //
        // There are two ways the shell learns a foreground app has finished, and they used to be able
        // to fire together. If the shell reached the top of this loop while the app held the console it
        // is MUTED, and this branch draws the prompt. If the app claimed the console while the shell was
        // already blocked in `console_read`, it never got here, so the kernel pushes a newline on
        // release to wake it - which the shell then reads as an empty command and answers with a prompt
        // of its own. When the shell was muted AND a wake newline was queued, both happened: two
        // prompts, which is how this presented.
        //
        // Draining first makes the two cases converge. Muted: the wake byte is discarded here and this
        // branch draws the one prompt. Blocked: nothing is muted, the newline wakes the read and the
        // empty-command path draws the one prompt. Bounded by the ring, not by the writer (§26.6).
        if muted {
            for _ in 0..CONSOLE_DRAIN_MAX {
                if ctx.try_console_read().is_none() {
                    break;
                }
            }
            ctx.console_write(PROMPT);
            muted = false;
        }

        // Read the next byte - but do NOT block indefinitely while the boot floor is still unrecorded.
        //
        // An idle shell blocks here forever, so the loop never turns and the check above never runs. On
        // an unattended machine - the only kind the boot floor is FOR - that meant the record was made
        // when somebody eventually pressed a key, or never. Polling until the sync lands and blocking
        // ever after costs a wakeup every MUTED_POLL_MS for the few seconds SNTP takes, and nothing at
        // all for the rest of the boot.
        // ALWAYS BLOCK ON INPUT. The prompt waits for a keystroke and nothing else.
        //
        // This used to poll every 30 ms until a network clock arrived, so that an unattended machine
        // would record its clock floor without anybody typing. The reasoning assumed "the few seconds
        // SNTP takes" - but with no cable SNTP never lands, the give-up latch is 30 SECONDS, and the
        // sleep it polls with is one no keystroke can interrupt. So on every boot of a machine with no
        // network, the keyboard was dead-to-stuttering for the first thirty seconds. Reported from
        // hardware as "keyboard doesn't work, I had to type in serial", and the shell's own comment
        // predicted the symptom exactly ("bursts... the typing stutter reported on hardware") without
        // anyone noticing it described the normal case rather than an edge one.
        //
        // The rule it breaks is the one this system is built on: wait on the TRUTH you need. What the
        // prompt needs is a keystroke, and `console_read` blocks until exactly that - the kernel parks
        // the task and the RX interrupt wakes it, with a lost-wakeup guard already in place. A clock
        // has nothing to do with typing, and putting it in front of the read makes the user's primary
        // interface hostage to a dependency that may never answer. That is the same fault as the
        // network path spinning against the driver it was waiting for, one layer up and far more
        // visible.
        //
        // The unattended clock-floor duty is NOT re-implemented here by other means, because the shell
        // is the wrong owner for it: `time` owns the clock, so `time` should own its floor - reading
        // it at start-up and persisting it when the clock is set. Removing it from the input path is
        // the fix; putting it where it belongs is separate work, recorded rather than faked.
        let b = ctx.console_read();

        match b {
            // Ctrl+Alt+Del (the SEC-2 follow-up). The USB driver cannot reboot - SEC-2 took REBOOT
            // away from it - so it only SIGNALS the chord on the console stream. The decision is
            // made HERE because the shell is the principal that legitimately holds REBOOT. This
            // restores the chord's UX without handing the driver back a direct reset from any
            // context, which is what SEC-2 actually removed. The signal byte is outside ASCII, so
            // no typed key produces it, and the chord is a deliberate three-key combination.
            godspeed_sdk::hid::CTRL_ALT_DEL_SIGNAL => {
                ctx.console_write("\r\n");
                cmd_reboot(&ctx);
            }
            b'\r' | b'\n' => {
                // We own echo now, so move to a fresh line ourselves (the kernel used
                // to echo the Enter as "\r\n").
                ctx.console_write("\r\n");
                if line.len > 0 {
                    // A line that READS a secret (`input secret ...`) never enters the recall ring or
                    // /.gsh_history (§8 secret taint): a password recovered on up-arrow would defeat the
                    // whole point of invisible entry. It still EXECUTES below; it just isn't remembered.
                    if !line_reads_secret(line.bytes()) {
                        hist.push(line.bytes());
                        hist.save(&ctx); // write-through to the fs (best-effort; never stalls the prompt)
                    }
                    last_result = execute(&ctx, line.bytes(), &mut cwd, last_result, 0, &mut Out::Console);
                    line.len = 0;
                    line.cur = 0;
                }
                nav = hist.len();
                ctx.console_write(PROMPT);
            }
            0x1B => {
                // Escape: either a bare ESC (the Escape key → clear the line) or the start
                // of a terminal escape sequence (arrows + the extended-keyboard navigation
                // cluster, which send ESC [ … / ESC O …). `read_escape_byte` distinguishes
                // them without blocking forever on a bare ESC; a confirmed sequence's
                // remaining bytes are already queued (the keyboard pushes them atomically),
                // so the rest reads blockingly.
                match read_escape_byte(&ctx) {
                    None => { line.clear(&ctx); nav = hist.len(); } // bare ESC → clear line
                    Some(b'[') => handle_csi(&ctx, &mut line, &mut hist, &mut nav),
                    Some(b'O') => {
                        // SS3 (F1-F4 = ESC O P/Q/R/S). F1 opens help; F2-F4 have no action.
                        if ctx.console_read() == b'P' {
                            run_help_key(&ctx, &mut cwd, &mut last_result, &mut line);
                        }
                    }
                    Some(_)    => {}                              // other ESC x: ignore
                }
            }
            0x7f | 0x08 => line.backspace(&ctx),
            0x09 => {
                // Tab - complete the command name (first token) or a FILE PATH (a later token,
                // resolved against `cwd`). One match → fill it in; several → a numbered menu (digit
                // selects, Tab cycles). Event-driven (redraws only on this keypress).
                complete_tab(&ctx, &mut line, &cwd);
            }
            0x03 => {
                // Ctrl-C - clear line
                ctx.console_writeln("^C");
                line.len = 0;
                line.cur = 0;
                nav = hist.len();
                ctx.console_write(PROMPT);
            }
            b if b >= 0x20 && b < 0x7f => line.insert(&ctx, b),
            _ => {}
        }
    }
}

/// F1 → run `help`, preserving the line being edited. Help (the pager) takes over the
/// screen and clears it on exit, so afterwards we reprint the prompt + the in-progress
/// line and park the cursor at its end. Runs at depth 0 (interactive) so help pages.
fn run_help_key(
    ctx: &ShellCtx,
    cwd: &mut Cwd,
    last_result: &mut Result<(), ShellError>,
    line: &mut Line,
) {
    ctx.console_write("\r\n");
    *last_result = execute(ctx, b"help", cwd, *last_result, 0, &mut Out::Console);
    ctx.console_write(PROMPT);
    line.cur = line.len; // cursor at end after the reprint
    if line.len > 0 {
        ctx.console_write(core::str::from_utf8(line.bytes()).unwrap_or(""));
    }
}

/// Read the first byte after an ESC, distinguishing a bare ESC (the Escape key, which
/// sends nothing more) from the start of a terminal escape sequence. The keyboard driver
/// pushes a navigation key's whole `ESC [ … ~` atomically, so its follow-up byte is
/// already queued and `try_console_read` returns it at once; a serial terminal may split
/// the bytes, so we wait a bounded few monotonic ticks (`ESC_WAIT_TICKS`) before giving
/// up. `None` ⇒ bare ESC. Returning quickly matters so a held key's repeats stay snappy.
/// How long to wait for a follow-up byte, counted in SCHEDULER QUANTA rather than cycles.
///
/// This was `200_000_000` "cycles", meaning ~100 ms at ~2 GHz - true only on x86. `read_tsc` on the
/// Pi 2 is the generic timer at ~1 MHz, not a 2 GHz CPU clock, so the same literal waited **~200
/// SECONDS**. Pressing Escape on the Pi hung the line editor for over three minutes. It hid because
/// only a BARE Escape reaches the wait: a real sequence's bytes are already queued, so arrows and
/// Home/Delete returned instantly and nothing looked wrong.
///
/// Counting quanta needs no notion of the counter's rate, which is the whole point - `sleep(1)` is
/// exactly one quantum on every architecture (`cycles_to_ticks` floors to 1 tick), so ~10 quanta is
/// ~100 ms on x86, on ARM, and on any future port, with no constant to recalibrate. It also stays
/// correct if ARM's quantum figure is later un-stubbed, since `1` still floors to one tick.
///
/// Sleeping rather than spinning also parks the task instead of pegging the core, and costs at most
/// one quantum of latency on a split sequence - a serial terminal's follow-up byte arrives within a
/// character time (~87 us at 115200), so it is caught on the first check either way.
const ESC_WAIT_QUANTA: u32 = 10;
fn read_escape_byte(ctx: &ServiceContext) -> Option<u8> {
    if let Some(b) = ctx.try_console_read() { return Some(b); }
    for _ in 0..ESC_WAIT_QUANTA {
        ctx.sleep(1); // exactly one scheduler quantum, on every arch
        if let Some(b) = ctx.try_console_read() { return Some(b); }
    }
    None
}

/// Handle a CSI sequence (everything after `ESC [`). Reads the optional numeric
/// parameter and the final byte, then dispatches the key. Covers the arrows (history +
/// cursor), Home/End, and the `~`-terminated navigation keys (Insert/Delete/Home/End/
/// PageUp/PageDown) and function keys an extended keyboard sends. Unknown sequences are
/// consumed and ignored - never smeared onto the line. Bounded: a final byte must arrive
/// within `CSI_MAX` bytes or we stop (defensive against a malformed serial stream).
fn handle_csi(ctx: &ShellCtx, line: &mut Line, hist: &mut History, nav: &mut usize) {
    const CSI_MAX: usize = 8;
    let mut param: u16 = 0;
    let mut have_param = false;
    let mut final_byte = 0u8;
    for _ in 0..CSI_MAX {
        let c = ctx.console_read();
        if c.is_ascii_digit() {
            have_param = true;
            param = param.saturating_mul(10).saturating_add((c - b'0') as u16);
        } else if c == b';' {
            // Multi-parameter (e.g. modified keys): we only act on the first; keep reading.
            continue;
        } else {
            final_byte = c; // 0x40..=0x7E terminates a CSI
            break;
        }
    }
    match final_byte {
        b'A' => { // Up - older command
            if *nav > 0 {
                // There is an older IN-MEMORY entry: just step to it. Memory only - fs is NOT touched
                // while the user is still walking their own session commands.
                *nav -= 1;
                line.set(ctx, hist.get(*nav));
            } else if !hist.loaded && hist.len() < HIST_MAX {
                // At the OLDEST in-memory entry, with room for more: only NOW - the user has run out of
                // session history - do the bounded disk load (at most once, the `loaded` gate). It merges
                // /.gsh_history BEHIND the session, so `added` older lines land at the front; step into the
                // newest of them and continue the up-nav. A wedged/absent/empty file adds nothing -> stay put.
                // Consequence: startup never touches fs; a session that already fills HIST_MAX never loads
                // (this arm's guard is false); the disk is read at most once, only on running out of history.
                let before = hist.len();
                hist.load(ctx);
                let added = hist.len() - before;
                if added > 0 { *nav = added - 1; line.set(ctx, hist.get(*nav)); }
            }
            // else: at the top and either already loaded or the ring is full - nothing older to show.
        }
        b'B' => { // Down - newer command (past the end → blank live line)
            if *nav < hist.len() {
                *nav += 1;
                let l: &[u8] = if *nav == hist.len() { &[] } else { hist.get(*nav) };
                line.set(ctx, l);
            }
        }
        b'C' => line.right(ctx), // Right - move cursor within the line
        b'D' => line.left(ctx),  // Left
        b'H' => line.home(ctx),  // Home (ESC[H)
        b'F' => line.end(ctx),   // End  (ESC[F)
        b'~' => match param {    // navigation cluster: ESC[<n>~
            1 | 7 => line.home(ctx),   // Home
            4 | 8 => line.end(ctx),    // End
            3     => line.delete(ctx), // Delete (forward delete)
            // 2 = Insert, 5 = PageUp, 6 = PageDown, 11.. = F-keys: no shell action, ignored.
            _ => { let _ = have_param; }
        },
        _ => {} // unknown final byte - already consumed, do nothing
    }
}

/// Tab completion. Splits the line into pipe SEGMENTS (`a | b | c`) and completes the current token
/// within its segment: the segment's FIRST word completes as a **command name** (`UTILS`, so it works
/// after a `|` too); a later token completes as a **subcommand keyword** (`observe now`, `to json`,
/// `sort reverse`, the trailing `mkdir … parents`) and otherwise as a **file path**. One match fills
/// it; several show the numbered menu (1-9 selects, Tab cycles). Operates from end-of-line so the menu
/// reprint lines up with the cursor (§26.6: bounded).
fn complete_tab(ctx: &ShellCtx, line: &mut Line, cwd: &Cwd) {
    if line.len == 0 { return; }
    line.end(ctx);
    // Current token starts after the last space (or line start); its pipe segment starts after the
    // last '|' before it. Computed as plain indices so no borrow of `line` outlives the dispatch.
    let bytes = line.bytes();
    let tok_start = bytes.iter().rposition(|&b| b == b' ').map(|s| s + 1).unwrap_or(0);
    let seg_start = bytes[..tok_start].iter().rposition(|&b| b == b'|').map(|i| i + 1).unwrap_or(0);
    // The token is the segment's COMMAND if only spaces sit between the segment start and it.
    let is_command = bytes[seg_start..tok_start].iter().all(|&b| b == b' ');
    // Does this segment's command take FILE PATHS as arguments? A service-name / number / keyword
    // command (chaos, kill, ping, ...) never does, so Tab past its keyword must NOT list the
    // filesystem (which wrongly surfaced /.gsh_history, e.g. `chaos max-carnage all-services <tab>`
    // landing on the rounds arg). Computed here while `bytes` is borrowed, before any mutating call.
    let seg_cmd = bytes[seg_start..].split(|&b| b == b' ').find(|w| !w.is_empty());
    let is_no_path = seg_cmd.map(|c| NO_PATH_CMDS.iter().any(|k| k.as_bytes() == c)).unwrap_or(false);

    if is_command {
        // Command names = built-in utilities + system-library scripts (both are typed by name).
        let mut names: [&str; 96] = [""; 96];
        let mut n = 0usize;
        for &u in UTILS { if n < names.len() { names[n] = u; n += 1; } }
        for &(lib, _) in LIBRARY { if n < names.len() { names[n] = lib; n += 1; } }
        complete_from_list(ctx, line, tok_start, &names[..n]);     // command name (after a `|` too)
    } else if !complete_keyword(ctx, line, seg_start, tok_start) && !is_no_path {
        complete_path(ctx, line, cwd, tok_start);                 // not a keyword and takes paths → file path
    }
}

/// Commands whose arguments are service names, numbers, or fixed keywords - NEVER file paths. Tab at
/// an argument position for these must not list the filesystem (which surfaced /.gsh_history). Their
/// keyword/target arguments are completed in `complete_keyword`; anything past that has no completion,
/// rather than falling through to path completion. (Path-taking commands - ls/read/write/mkdir/... -
/// are absent, so they still path-complete.)
///
/// CONVENTION (`utilities/0_conventions.md` rule 9): a new non-path utility must be added here in the
/// same commit; a path-taking utility is left out. Opting out of path completion is explicit + per-command.
const NO_PATH_CMDS: &[&str] = &[
    "chaos", "kill", "spawn", "restart", "ping", "net", "drives", "observe", "date", "uptime",
    "wait", "watch", "whatis", "busiest", "random", "gpio", "trace",
];

/// Commands whose FIRST argument (the token right after the command, within its pipe segment) is a
/// fixed keyword - completed only at that position. Pipe-stage verbs (`to`/`from`/`sort`/`match`) are
/// here too, so `… | to j⇥` → `json` and `… | sort r⇥` → `reverse`. Keep in sync with each command's
/// argument parsing (verified against utilities/*.md + the `cmd_*` parsers). The universal `version`
/// and `help` subcommands are appended automatically in `complete_keyword` (every utility answers
/// `<util> version` / `<util> help`), so they are NOT listed here.
const SUBCMD_FIRST: &[(&str, &[&str])] = &[
    ("observe", &["now"]),
    ("trace",   &["blocked", "chain", "deps", "endpoint", "endpoints", "ipc", "failures", "status"]),
    ("busiest", &["mem", "restarts", "queue"]),
    ("date",    &["epoch", "sync"]),
    ("net",     &["dns", "stats", "arp", "scan", "renew", "lease"]),
    ("drives",  &["flash", "label", "reset", "check", "scrub"]),
    ("chaos",   &["kill-storm", "flood-storm", "mem-pressure", "spawn-storm", "max-carnage", "link-flap"]),
    ("write",   &["append", "prepend"]),
    ("sort",    &["reverse"]),
    ("match",   &["except"]),
    ("to",      &["json", "yaml"]),
    ("from",    &["json"]),
];

/// Info / no-argument utilities: their only first-argument subcommands are the universal `version`
/// and `help`. Tab at their first-arg position completes those (and NEVER falls through to a
/// filesystem listing - they take no path). This is the info-command analogue of `NO_PATH_CMDS`:
/// every one of these is a keyword command, so a new one belongs here (conventions rule 9).
const INFO_CMDS: &[&str] = &[
    "about", "version", "mem", "cores", "sock", "uptime", "status", "roster", "clear", "reboot",
    "result", "selfcheck", "caps", "help",
    // Library scripts whose only first-arg subcommands are the universal version/help ("size" is
    // absent on purpose - its argument is a path, so it keeps path completion; "watch" has its own
    // command-name completion case; "busiest" completes its column keywords via SUBCMD_FIRST).
    "health", "online",
];

/// Commands with a TRAILING modifier keyword that follows the variable argument(s) - completed at any
/// position after the first arg, when it prefix-matches and is not already present (`mkdir /x p⇥` →
/// `parents`, `copy /a /b r⇥` → `recursive`). Never offered as the first argument (that token is the
/// path being named/operated on, not the modifier).
const SUBCMD_TRAILING: &[(&str, &[&str])] = &[
    ("mkdir",  &["parents"]),
    ("copy",   &["recursive"]),
    ("delete", &["recursive"]),
];

/// Complete the current token (`tok_start..end`) as a subcommand keyword of its segment's command.
/// `seg_start..tok_start` holds the command + any already-typed args, which decide the command and
/// whether this is the first argument. Returns `true` if it completed/offered a menu, `false` to fall
/// through to path completion.
fn complete_keyword(ctx: &ServiceContext, line: &mut Line, seg_start: usize, tok_start: usize) -> bool {
    let head = &line.bytes()[seg_start..tok_start];           // command + prior args (+ spaces)
    let mut words = head.split(|&b| b == b' ').filter(|w| !w.is_empty());
    let cmd = match words.next() { Some(c) => c, None => return false };
    let prior = words.clone().count();                        // args typed before the current token

    // `chaos max-carnage <target>`: complete the 2nd arg as the target (all-services + the service names).
    // The target may be a comma-separated list, so complete the segment after the LAST comma - so
    // `nic-driver,net-<tab>` finishes `net-stack` while the earlier listed targets are preserved verbatim.
    if "chaos".as_bytes() == cmd && prior == 1 && words.clone().next() == Some("max-carnage".as_bytes()) {
        const TARGETS: &[&str] =
            &["all-services", "supervisor", "block-driver", "fs", "logger", "xhci", "ehci", "shell", "nic-driver", "net-stack"];
        let seg_start = {
            let tok = &line.bytes()[tok_start..];
            tok.iter().rposition(|&b| b == b',').map(|i| tok_start + i + 1).unwrap_or(tok_start)
        };
        return complete_from_list(ctx, line, seg_start, TARGETS);
    }

    // `chaos max-carnage <target> <rounds> <tab>`: complete the optional confirm-skip word. Per
    // utilities/0_conventions.md 5, a subcommand that does not complete is one the operator has to
    // already know about - which is what makes typing words as cheap as flags.
    if "chaos".as_bytes() == cmd && prior == 3 && words.clone().next() == Some("max-carnage".as_bytes()) {
        return complete_from_list(ctx, line, tok_start, &["yes"]);
    }

    // `kill <svc>[,svc,...]`: complete a service name plus the `all-services` keyword. kill takes a
    // comma-separated list, so complete the segment after the LAST comma (like chaos max-carnage) - so
    // `ehci,xh<tab>` finishes `ehci,xhci` while the earlier listed targets are preserved verbatim.
    if "kill".as_bytes() == cmd && prior == 0 {
        const KILL_TARGETS: &[&str] =
            &["all-services", "supervisor", "block-driver", "fs", "logger", "xhci", "ehci", "shell", "nic-driver", "net-stack", "version", "help"];
        let seg_start = {
            let tok = &line.bytes()[tok_start..];
            tok.iter().rposition(|&b| b == b',').map(|i| tok_start + i + 1).unwrap_or(tok_start)
        };
        return complete_from_list(ctx, line, seg_start, KILL_TARGETS);
    }

    // `spawn <svc>[,svc,...]`: complete the demo/app services, comma-list aware (segment after last comma).
    if "spawn".as_bytes() == cmd && prior == 0 {
        const SPAWN_TARGETS: &[&str] = &["ping", "pong", "version", "help"];
        let seg_start = {
            let tok = &line.bytes()[tok_start..];
            tok.iter().rposition(|&b| b == b',').map(|i| tok_start + i + 1).unwrap_or(tok_start)
        };
        return complete_from_list(ctx, line, seg_start, SPAWN_TARGETS);
    }

    // `watch <command ...>` / `whatis <name>`: the first argument IS a command name, so complete
    // it from the same set the command position uses (built-ins + library scripts).
    if ("watch".as_bytes() == cmd || "whatis".as_bytes() == cmd) && prior == 0 {
        let mut names: [&str; 96] = [""; 96];
        let mut n = 0usize;
        for &u in UTILS { if n < names.len() { names[n] = u; n += 1; } }
        for &(lib, _) in LIBRARY { if n < names.len() { names[n] = lib; n += 1; } }
        return complete_from_list(ctx, line, tok_start, &names[..n]);
    }

    // `restart <name> [core]`: complete the restartable services (single target, not a comma-list).
    if "restart".as_bytes() == cmd && prior == 0 {
        const RESTART_TARGETS: &[&str] = &["supervisor", "block-driver", "fs", "logger", "xhci",
            "ehci", "shell", "nic-driver", "net-stack", "ping", "pong", "version", "help"];
        return complete_from_list(ctx, line, tok_start, RESTART_TARGETS);
    }

    // `ping [count N] [bytes N] <ip>`: the option keywords may appear in either order before the IP, so
    // complete them at ANY position where the token prefix-matches one not already used (not just first).
    if "ping".as_bytes() == cmd {
        const PING_OPTS: &[&str] = &["count", "bytes", "version", "help"];
        let mut avail = [""; 2];
        let mut a = 0usize;
        for &k in PING_OPTS {
            let used = head.split(|&b| b == b' ').any(|w| w == k.as_bytes());
            if !used && a < avail.len() { avail[a] = k; a += 1; }
        }
        return complete_from_list(ctx, line, tok_start, &avail[..a]);
    }

    // Info / no-argument utilities: the only first-arg subcommands are the universal version + help
    // (every utility answers `<util> version` / `<util> help`). Offer them and stop - these take no
    // path, so `about <tab>` must never list the filesystem. Return true even on no match so it does
    // not fall through to path completion.
    if prior == 0 && INFO_CMDS.iter().any(|c| c.as_bytes() == cmd) {
        complete_from_list(ctx, line, tok_start, &["version", "help"]);
        return true;
    }

    if let Some((_, cands)) = SUBCMD_FIRST.iter().find(|(c, _)| c.as_bytes() == cmd) {
        // First-argument keyword only: a later arg is a path/value (e.g. `write append /f`), not a key.
        if prior != 0 { return false; }
        // Append the universal `version` + `help` subcommands (deduped) so every utility completes
        // them, not just its specific keywords. A dual command (write/sort/match) whose token matches
        // no keyword returns false here and falls through to path completion.
        let mut all: [&str; 16] = [""; 16];
        let mut n = 0usize;
        for &c in *cands { if n < all.len() { all[n] = c; n += 1; } }
        for &c in ["version", "help"].iter() {
            if !cands.contains(&c) && n < all.len() { all[n] = c; n += 1; }
        }
        return complete_from_list(ctx, line, tok_start, &all[..n]);
    }
    if let Some((_, cands)) = SUBCMD_TRAILING.iter().find(|(c, _)| c.as_bytes() == cmd) {
        if prior == 0 { return false; }                       // first arg is the path, not the modifier
        // Offer only modifiers not already present in the segment.
        let mut avail = [""; 8];
        let mut a = 0usize;
        for &k in *cands {
            let used = head.split(|&b| b == b' ').any(|w| w == k.as_bytes());
            if !used && a < avail.len() { avail[a] = k; a += 1; }
        }
        return complete_from_list(ctx, line, tok_start, &avail[..a]);
    }
    false
}

/// Match the current token (`tok_start..end`) against `cands`: 0 matches → `false` (no change); 1 →
/// fill it + a trailing space; several → the numbered menu (digit selects, Tab cycles). The single
/// completion engine shared by command-name and keyword completion. Returns `true` when it acted.
fn complete_from_list(ctx: &ServiceContext, line: &mut Line, tok_start: usize, cands: &[&str]) -> bool {
    let token = &line.bytes()[tok_start..];
    let mut matches = [""; 64];
    let mut n = 0usize;
    for &k in cands {
        if k.as_bytes().starts_with(token) {
            if n < matches.len() { matches[n] = k; n += 1; }
        }
    }
    if n == 0 { return false; }
    if n == 1 { fill_keyword(ctx, line, tok_start, matches[0], true); return true; }
    keyword_menu(ctx, line, tok_start, &matches[..n]);
    true
}

/// Replace the line from `tok_start` to end with `name`; `commit` appends a trailing space (a chosen
/// completion), else nothing (a Tab-cycle preview).
fn fill_keyword(ctx: &ServiceContext, line: &mut Line, tok_start: usize, name: &str, commit: bool) {
    let mut tmp = [0u8; MAX_LINE];
    let mut t = tok_start.min(MAX_LINE);
    tmp[..t].copy_from_slice(&line.buf[..t]);
    let c = name.as_bytes();
    let take = c.len().min(MAX_LINE.saturating_sub(t + 1));
    tmp[t..t + take].copy_from_slice(&c[..take]); t += take;
    if commit && t < MAX_LINE { tmp[t] = b' '; t += 1; }
    line.set(ctx, &tmp[..t]);
}

/// Numbered menu for keyword candidates: a digit (1-9) commits, Tab cycles, any other key keeps the
/// line. Mirrors `path_menu`.
fn keyword_menu(ctx: &ServiceContext, line: &mut Line, tok_start: usize, cands: &[&str]) {
    let n = cands.len();
    let shown = n.min(9);
    ctx.console_write("\r\n");
    for k in 0..shown {
        let mut row = [0u8; 48];
        let mut p = 0usize;
        row[p] = b'1' + k as u8; p += 1; row[p] = b')'; p += 1; row[p] = b' '; p += 1;
        let name = cands[k].as_bytes();
        let take = name.len().min(row.len() - p - 3);
        row[p..p + take].copy_from_slice(&name[..take]); p += take;
        row[p] = b' '; p += 1; row[p] = b' '; p += 1;
        ctx.console_write(core::str::from_utf8(&row[..p]).unwrap_or(""));
    }
    if n > shown { ctx.console_write("(type more to narrow) "); }
    ctx.console_write("\r\n");
    ctx.console_write(PROMPT);
    ctx.console_write(str_of(line.bytes()));
    let mut idx = usize::MAX;
    loop {
        let key = ctx.console_read();
        if (b'1'..=b'9').contains(&key) {
            let d = (key - b'1') as usize;
            if d < shown { fill_keyword(ctx, line, tok_start, cands[d], true); }
            return;
        }
        if key == 0x09 {
            idx = if idx == usize::MAX { 0 } else { (idx + 1) % n };
            fill_keyword(ctx, line, tok_start, cands[idx], false);
            continue;
        }
        return;
    }
}

/// One matched directory entry, as offsets into the (owned) LIST_DIR reply buffer.
#[derive(Clone, Copy)]
struct PathHit { off: usize, len: usize, is_dir: bool }

/// Complete the path token from `tok_start` to end-of-line against the directory it names. The
/// token splits into a dir part (up to the last `/`) and the leaf being typed; we `LIST_DIR` the
/// resolved dir and match entries whose name starts with the leaf. One match → fill it (+ `/` for a
/// dir, ` ` for a file); several → fill the common prefix, print a numbered menu, then **digit**
/// selects or **Tab** cycles to the next candidate (any other key keeps the line). No new authority
/// - the shell already holds the `fs` LIST_DIR cap (the same `ls` uses).
fn complete_path(ctx: &ShellCtx, line: &mut Line, cwd: &Cwd, tok_start: usize) {
    let bytes = line.bytes();
    let token = &bytes[tok_start..];
    // dir part (everything up to and including the last '/') and the leaf being typed.
    let (dir_in_tok, leaf): (&[u8], &[u8]) = match token.iter().rposition(|&b| b == b'/') {
        Some(i) => (&token[..=i], &token[i + 1..]),
        None => (&[][..], token),
    };
    // Resolve the directory to an absolute path (relative parts resolve against cwd).
    let mut dirbuf = [0u8; PATH_MAX];
    let dirpath: &[u8] = if dir_in_tok.is_empty() {
        cwd.as_str().as_bytes()
    } else {
        match resolve_path(cwd.as_str(), core::str::from_utf8(dir_in_tok).unwrap_or("/"), &mut dirbuf) {
            Some(n) => &dirbuf[..n],
            None => return,
        }
    };
    // LIST_DIR (the reply is one ≤512-byte block); copy it so it can outlive the fs reply across
    // the menu/cycle loop below.
    let mut rbuf = [0u8; 512];
    let rn;
    {
        let reply = match fs_request(ctx, OP_LIST_DIR, dirpath, &[]) { Some(r) => r, None => return };
        let pb = reply.payload_bytes();
        if !(pb.first() == Some(&FS_OK) && pb.len() >= 2) { return; } // not a dir / error → no menu
        rn = pb.len().min(512);
        rbuf[..rn].copy_from_slice(&pb[..rn]);
    }
    // Collect entries whose name starts with `leaf`.
    let count = rbuf[1] as usize;
    let mut hits = [PathHit { off: 0, len: 0, is_dir: false }; 32];
    let mut n = 0usize;
    let mut i = 2usize;
    for _ in 0..count {
        if i >= rn { break; }
        let nl = rbuf[i] as usize; i += 1;
        if i + nl + 9 > rn { break; }                 // entry = name_len, name, is_dir, size:u64
        let is_dir = rbuf[i + nl] != 0;
        if rbuf[i..i + nl].starts_with(leaf) && n < hits.len() {
            hits[n] = PathHit { off: i, len: nl, is_dir }; n += 1;
        }
        i += nl + 9;
    }
    if n == 0 { return; }
    let base_len = tok_start + dir_in_tok.len();      // the line is fixed up to here

    if n == 1 {
        let h = hits[0];
        fill_path(ctx, line, base_len, &rbuf[h.off..h.off + h.len], Some(h.is_dir));
        return;
    }
    // Several: fill the longest common prefix first (often resolves enough on its own).
    let lcp = path_lcp(&rbuf, &hits[..n]);
    if lcp > leaf.len() {
        let h = hits[0];
        fill_path(ctx, line, base_len, &rbuf[h.off..h.off + lcp], None); // no sep - still ambiguous
    }
    path_menu(ctx, line, base_len, &rbuf, &hits[..n]);
}

/// Length of the longest common prefix shared by all matched names.
fn path_lcp(rbuf: &[u8; 512], hits: &[PathHit]) -> usize {
    let mut len = hits[0].len;
    for h in &hits[1..] {
        let mut k = 0;
        while k < len && k < h.len && rbuf[hits[0].off + k] == rbuf[h.off + k] { k += 1; }
        len = k;
    }
    len
}

/// Replace the line from `base_len` to end with `name`. `sep` Some(is_dir) appends `/` (dir) or ` `
/// (file) - a committed completion; None appends nothing - a still-ambiguous common-prefix fill.
fn fill_path(ctx: &ServiceContext, line: &mut Line, base_len: usize, name: &[u8], sep: Option<bool>) {
    let mut tmp = [0u8; MAX_LINE];
    let mut t = base_len.min(MAX_LINE);
    tmp[..t].copy_from_slice(&line.buf[..t]);
    let take = name.len().min(MAX_LINE.saturating_sub(t + 1));
    tmp[t..t + take].copy_from_slice(&name[..take]); t += take;
    if let Some(is_dir) = sep {
        if t < MAX_LINE { tmp[t] = if is_dir { b'/' } else { b' ' }; t += 1; }
    }
    line.set(ctx, &tmp[..t]);
}

/// Print the numbered candidate menu, then run the selection loop: a **digit** (1-9) commits that
/// entry; **Tab** cycles to the next candidate (filling it, no separator); any other key keeps the
/// current line and returns (that key is not consumed as input - minor: re-press to use it).
fn path_menu(ctx: &ShellCtx, line: &mut Line, base_len: usize, rbuf: &[u8; 512], hits: &[PathHit]) {
    let n = hits.len();
    let shown = n.min(9);
    ctx.console_write("\r\n");
    for k in 0..shown {
        let h = hits[k];
        let mut row = [0u8; 48];
        let mut p = 0usize;
        row[p] = b'1' + k as u8; p += 1; row[p] = b')'; p += 1; row[p] = b' '; p += 1;
        let take = h.len.min(row.len() - p - 3);
        row[p..p + take].copy_from_slice(&rbuf[h.off..h.off + take]); p += take;
        if h.is_dir && p < row.len() { row[p] = b'/'; p += 1; } // dir cue
        row[p] = b' '; p += 1; row[p] = b' '; p += 1;
        ctx.console_write(core::str::from_utf8(&row[..p]).unwrap_or(""));
    }
    if n > shown { ctx.console_write("(type more to narrow) "); }
    ctx.console_write("\r\n");
    ctx.console_write(PROMPT);
    ctx.console_write(str_of(line.bytes()));

    let mut idx = usize::MAX; // MAX = no candidate filled yet (showing the common-prefix)
    loop {
        let key = ctx.console_read();
        if (b'1'..=b'9').contains(&key) {
            let d = (key - b'1') as usize;
            if d < shown { let h = hits[d]; fill_path(ctx, line, base_len, &rbuf[h.off..h.off + h.len], Some(h.is_dir)); }
            return;
        }
        if key == 0x09 { // Tab → cycle to the next candidate
            idx = if idx == usize::MAX { 0 } else { (idx + 1) % n };
            let h = hits[idx];
            fill_path(ctx, line, base_len, &rbuf[h.off..h.off + h.len], None);
            continue;
        }
        return; // any other key: keep the current line (common-prefix or last cycled candidate)
    }
}

/// A bounded ring of recent command lines for up/down-arrow recall (§26.6: fixed size,
/// oldest dropped when full). Lives in the shell session; cleared each boot.
const HIST_MAX: usize = 16;
/// The prompt. Its WIDTH sets the column the typed line starts at, so the two are defined
/// together and checked at compile time - a longer prompt with a stale redraw column would
/// erase from inside the prompt.
const PROMPT: &str = "gsh> ";
/// Cursor to the first column of the typed text (CHA, 1-based), then erase to end of line.
const REDRAW: &str = "\x1b[6G\x1b[K";
const _: () = assert!(PROMPT.len() + 1 == 6);

struct History {
    lines: [[u8; MAX_LINE]; HIST_MAX],
    lens: [usize; HIST_MAX],
    n: usize,
    /// One-shot lazy-load gate: the disk history is NOT read at startup (that would touch fs on the
    /// prompt's critical path); it is loaded on the FIRST up-arrow and merged behind the session, and
    /// this flag ensures that happens exactly once - success OR bounded-miss - so every later up-arrow
    /// is instant and never re-touches fs.
    loaded: bool,
}
impl History {
    fn new() -> Self {
        History { lines: [[0u8; MAX_LINE]; HIST_MAX], lens: [0; HIST_MAX], n: 0, loaded: false }
    }
    fn len(&self) -> usize { self.n }
    fn get(&self, i: usize) -> &[u8] { &self.lines[i][..self.lens[i]] }
    fn push(&mut self, line: &[u8]) {
        if self.n > 0 && self.get(self.n - 1) == line { return; } // skip consecutive dupes
        let l = line.len().min(MAX_LINE);
        if self.n == HIST_MAX {
            for i in 1..HIST_MAX {
                self.lines[i - 1] = self.lines[i];
                self.lens[i - 1] = self.lens[i];
            }
            self.n = HIST_MAX - 1;
        }
        self.lines[self.n][..l].copy_from_slice(&line[..l]);
        self.lens[self.n] = l;
        self.n += 1;
    }
    /// Best-effort persistence to `/.gsh_history` (§15: history is shell-OWNED state, externalized to the
    /// fs so a `kill shell` respawn reconstructs it). History is an ENHANCEMENT, never a requirement, so
    /// neither path may hang the prompt on a slow/down fs (§26.7): the LOAD is lazy (only when the user
    /// runs out of session history) and bounded, the SAVE is deadline-bounded (a true fire-and-forget save
    /// is blocked by fs's single-endpoint block-driver multiplexing - see `save`). A rug-pulled fs is then a
    /// non-event - no recall this session, shell fully usable - and both resume when fs heals.
    ///
    /// LAZY + one-shot + merge-behind. Not called at startup, and not merely on the first up-arrow - only
    /// when the user navigates PAST their last session command (up-arrow at the oldest in-memory entry) AND
    /// there is room (len < HIST_MAX), so a session that already fills the ring never touches fs at all. Runs
    /// at most once (the `loaded` gate, set even on a bounded miss so later up-arrows never re-touch fs).
    /// Bounded: a wedged/absent fs just leaves the session's own commands. The file's lines are OLDER than
    /// anything typed this session, so they merge BEHIND the session commands (newest, kept); `push` drops
    /// the oldest - a file line - if the merge exceeds HIST_MAX. `#[inline(never)]` keeps its ~4 KiB frame
    /// off the interactive key path (the shell's user stack is tight).
    #[inline(never)]
    fn load(&mut self, ctx: &ShellCtx) {
        if self.loaded { return; }
        self.loaded = true;
        let path = b"/.gsh_history";
        let sz = match fs_stat_bounded(ctx, path, HIST_LOAD_SECS) { Some((s, false)) if s > 0 => s as usize, _ => return };
        let sz = sz.min(HIST_MAX * (MAX_LINE + 1));
        let mut buf = [0u8; HIST_MAX * (MAX_LINE + 1)];
        if !read_file_exact_bounded(ctx, path, 0, &mut buf[..sz], HIST_LOAD_SECS) { return; }
        // Snapshot the session lines (newest, must survive), rebuild the ring as [file lines..., session
        // lines], so up-arrow walks the session first and the file underneath, oldest dropped first.
        let mut sess = [[0u8; MAX_LINE]; HIST_MAX];
        let mut sess_len = [0usize; HIST_MAX];
        let sn = self.n;
        for i in 0..sn { let l = self.get(i); sess[i][..l.len()].copy_from_slice(l); sess_len[i] = l.len(); }
        self.n = 0;
        for l in buf[..sz].split(|&b| b == b'\n') {
            if !l.is_empty() { self.push(l); }
        }
        for i in 0..sn { self.push(&sess[i][..sess_len[i]]); }
    }
    /// Deadline-bounded write-through of the <=16-line ring to `/.gsh_history`. Bounded (HIST_SAVE_SECS)
    /// so a mid-restart / slow fs cannot hang the prompt forever, but NOT fire-and-forget: a true
    /// no-reply async write is blocked by an fs-architecture constraint - fs multiplexes client requests
    /// and its block-driver replies on ONE endpoint and relies on clients being synchronous (one in-flight),
    /// so a second client message queued during an fs transaction gets consumed by fs's `block_rpc` as a
    /// stray block reply, breaking the transaction and the queued command. Making the save fire-and-forget
    /// needs fs to separate its block-driver reply channel first (see the fork report / follow-up). Until
    /// then this stays synchronous: instant on a healthy fs, a bounded (<=HIST_SAVE_SECS) shrug on a wedged
    /// one - best-effort either way, never an error surfaced to the user (§26.7). `#[inline(never)]` keeps
    /// its buffer off the Enter path (tight user stack).
    #[inline(never)]
    fn save(&self, ctx: &ShellCtx) {
        let path: &[u8] = b"/.gsh_history";
        let mut buf = [0u8; HIST_MAX * (MAX_LINE + 1)];
        let mut pos = 0usize;
        for i in 0..self.n {
            let l = self.get(i);
            buf[pos..pos + l.len()].copy_from_slice(l); pos += l.len();
            buf[pos] = b'\n'; pos += 1;
        }
        let _ = fs_request_bounded(ctx, OP_WRITE_FILE, path, &buf[..pos], HIST_SAVE_SECS);
    }
}

/// The editable input line with a cursor, so the navigation cluster of a standard
/// extended keyboard (Left/Right/Home/End/Delete) edits *mid-line*, not just at the
/// end. `cur` is the insertion point in `0..=len`. Every edit echoes itself using only
/// `\x08` (non-destructive cursor-left on both the framebuffer console and a serial
/// terminal), character reprints (cursor-right), and `ESC[K` (erase to end of line) -
/// the lowest common denominator both honour, so editing looks identical over HDMI and
/// over the serial console. Bounded (§26.6): `MAX_LINE`, loud-safe (over-long input is
/// simply not accepted).
struct Line {
    buf: [u8; MAX_LINE],
    len: usize,
    cur: usize,
}
impl Line {
    fn new() -> Self { Line { buf: [0u8; MAX_LINE], len: 0, cur: 0 } }
    fn bytes(&self) -> &[u8] { &self.buf[..self.len] }

    /// Reprint from the cursor to end-of-line, erase any stale tail (`ESC[K`), then
    /// step the cursor back to `cur`. Used after an insert/delete shifts the tail.
    /// Echo one line edit as a SINGLE console message: `lead`, the tail behind the cursor, an
    /// optional erase, and the backspaces that put the cursor back.
    ///
    /// Every `console_write` is an IPC message to the `console` service, whose endpoint queue is 16
    /// deep (§8.5). This used to cost 2 + n messages per keystroke - the character, an unconditional
    /// `ESC[K`, and one backspace per tail character - so typing fast filled that queue in a handful
    /// of keys. A full queue BLOCKS the sender (`BlockedOnSend`), and a blocked shell is not calling
    /// `ConsoleRead`, so keystrokes piled into the kernel's input ring until it overflowed and dropped
    /// them. That is the "keyboard trips when you bash it" reported from the Wyse. One message per
    /// edit removes the pressure at its source rather than absorbing it downstream.
    ///
    /// `erase` is passed only when the line SHRANK (backspace, delete), which is the only case that
    /// leaves a stale cell to the right of the reprinted tail. An insert grows the line, so the erase
    /// is pure cost there - and it was being paid on every single character typed.
    fn echo_edit(&self, ctx: &ServiceContext, lead: &[u8], erase: bool) {
        let t = self.len - self.cur;
        // `console_write` caps a message at 256 bytes, so a tail too long to batch falls back to the
        // unbatched path below - correct, just chattier, and only reachable when editing near the
        // start of a line more than half the maximum length.
        if lead.len() + 2 * t + 3 <= 256 {
            let mut b = [0u8; 256];
            let mut n = 0;
            b[n..n + lead.len()].copy_from_slice(lead);
            n += lead.len();
            b[n..n + t].copy_from_slice(&self.buf[self.cur..self.len]);
            n += t;
            if erase {
                b[n..n + 3].copy_from_slice(b"\x1b[K");
                n += 3;
            }
            for _ in 0..t {
                b[n] = 0x08;
                n += 1;
            }
            if n > 0 { ctx.console_write(core::str::from_utf8(&b[..n]).unwrap_or("")); }
            return;
        }
        if !lead.is_empty() { ctx.console_write(core::str::from_utf8(lead).unwrap_or("")); }
        if t > 0 { ctx.console_write(core::str::from_utf8(&self.buf[self.cur..self.len]).unwrap_or("")); }
        if erase { ctx.console_write("\x1b[K"); }
        for _ in 0..t { ctx.console_write("\x08"); }
    }

    /// Insert a printable byte at the cursor.
    fn insert(&mut self, ctx: &ServiceContext, b: u8) {
        if self.len >= MAX_LINE {
            // The line is full. Say so instead of swallowing the keystroke: a limit that drops input
            // with no sign of it reads as a broken keyboard ("there seems to be a typing boundary"),
            // and a ceiling reached LOUDLY is the point of having a fixed one at all (§26.6/§26.7).
            // BEL is what a terminal expects here - serial rings it, the framebuffer console ignores
            // it - so the line being edited is never disturbed.
            ctx.console_write("\x07");
            return;
        }
        let mut i = self.len;
        while i > self.cur { self.buf[i] = self.buf[i - 1]; i -= 1; }
        self.buf[self.cur] = b;
        self.len += 1;
        self.cur += 1;
        // Echo the inserted byte and the shifted tail as ONE message. No erase: an insert GROWS the
        // line, so the reprinted tail is longer than what it replaces and nothing stale can remain.
        self.echo_edit(ctx, &[b], false);
    }

    /// Delete the character before the cursor (Backspace).
    fn backspace(&mut self, ctx: &ServiceContext) {
        if self.cur == 0 { return; }
        for i in self.cur..self.len { self.buf[i - 1] = self.buf[i]; }
        self.len -= 1;
        self.cur -= 1;
        // Step left onto the deleted cell, reprint the tail, erase the cell it no longer covers -
        // one message. The erase IS needed here: the line shrank, so the tail leaves one stale cell.
        self.echo_edit(ctx, b"\x08", true);
    }

    /// Delete the character at the cursor (the Delete key - forward delete).
    fn delete(&mut self, ctx: &ServiceContext) {
        if self.cur >= self.len { return; }
        for i in (self.cur + 1)..self.len { self.buf[i - 1] = self.buf[i]; }
        self.len -= 1;
        self.echo_edit(ctx, b"", true);
    }

    fn left(&mut self, ctx: &ServiceContext) {
        if self.cur > 0 { self.cur -= 1; ctx.console_write("\x08"); }
    }
    fn right(&mut self, ctx: &ServiceContext) {
        if self.cur < self.len {
            let s = [self.buf[self.cur]];
            ctx.console_write(core::str::from_utf8(&s).unwrap_or("")); // reprint = move right
            self.cur += 1;
        }
    }
    fn home(&mut self, ctx: &ServiceContext) {
        while self.cur > 0 { self.cur -= 1; ctx.console_write("\x08"); }
    }
    fn end(&mut self, ctx: &ServiceContext) {
        if self.cur < self.len {
            ctx.console_write(core::str::from_utf8(&self.buf[self.cur..self.len]).unwrap_or(""));
            self.cur = self.len;
        }
    }

    /// Erase the visible input and replace it with `new`, cursor at the end. Used by
    /// history recall, tab completion, and the bare-ESC clear. Erases from wherever the
    /// cursor is: step to the input start, `ESC[K` to wipe to end of line, then print.
    fn set(&mut self, ctx: &ServiceContext, new: &[u8]) {
        // ABSOLUTE, not a walk back. This used to emit `cur` backspaces to return to the start of
        // the typed text, which assumes the cursor is exactly where we last left it - and the
        // console is a SHARED device the shell does not hold a lock on. Anything else that writes
        // (a service log during chaos) moves the cursor out from under that count, so the walk back
        // stops short of the prompt and the erase begins mid-text. What is left is a stale prefix:
        // recalling history showed "gsh> chaos mselfcheck all-services 50 yes", and backspacing
        // then emptied the BUFFER while those stale columns stayed on screen - so the line looked
        // like "chaos m" but Enter submitted nothing. The buffer was right the whole time; only our
        // belief about the cursor had drifted.
        //
        // CHA cannot drift: it names the column outright, so the erase always starts at the true
        // start of the typed text and the prompt itself is preserved (which is why this is not a
        // carriage return + reprint - a second prompt is indistinguishable from a real one).
        ctx.console_write(REDRAW);
        self.cur = 0;
        let n = new.len().min(MAX_LINE);
        self.buf[..n].copy_from_slice(&new[..n]);
        self.len = n;
        self.cur = n;
        if n > 0 { ctx.console_write(core::str::from_utf8(&self.buf[..n]).unwrap_or("")); }
    }

    /// Clear to an empty line (cursor at 0), erasing what was shown.
    fn clear(&mut self, ctx: &ServiceContext) { self.set(ctx, &[]); }
}

/// Wait until the input subsystem reports in - the deterministic end-of-boot
/// signal. The xHCI driver sets `input_ready` once it finishes, in every terminal
/// path (keyboard up, no keyboard, or no controller), and it is the last
/// subsystem to come up. So when it reports, the boot sequence - including the
/// asynchronous xHCI enumeration on another core - is genuinely done, and we can
/// clear the boot screen without ever cutting it off mid-stream. The loop is just
/// polling that flag; `MAX_SPINS` is a pure safety net for the impossible case
/// where the driver never reports (it would mean xHCI hard-crashed at boot).
/// Report whether the input driver has announced itself - and do NOT wait for it.
///
/// This used to spin up to fifty million times before the shell printed its first prompt, waiting for
/// the USB driver to report in. Three things wrong with that, and the third is the one that matters:
///
///   - it is a wait on a COUNT, which is not a duration: the same loop is a different length of time
///     on every machine and every boot;
///   - it is a spin, so it burns the core that `dwc2` needs to finish bringing the keyboard UP - the
///     shell starving the very driver it was waiting for, the same self-defeating shape as the network
///     path waiting on frames while saturating the service that fetches them;
///   - and it is pointless. `console_read` blocks in the kernel on the input ring and is woken when a
///     byte arrives, whether that byte comes from serial now or from a USB keyboard that finished
///     enumerating a second later. Waiting for the driver to "be ready" buys nothing that blocking on
///     the input itself does not already give.
///
/// The prompt is the user's interface and must exist from the moment the shell runs. Everything else -
/// USB enumeration, the disk, the clock - comes up behind it, and the prompt simply works when it
/// does.
fn input_driver_announced(ctx: &ServiceContext) -> bool {
    ctx.input_ready()
}

/// Split `s` into args with **minimal quoting**: a token wrapped in a matching pair of `'…'`
/// or `"…"` is one argument with the surrounding pair stripped - **no escapes, no nesting, no
/// expansion** (single and double behave identically). This is what lets `match "two words"`
/// pass a multi-word pattern; unquoted tokens split on whitespace exactly as before. Returns
/// the arg count; each arg is a slice of `s` (no allocation).
fn tokenize<'a>(s: &'a str, args: &mut [&'a str; MAX_ARGS]) -> usize {
    let b = s.as_bytes();
    let mut argc = 0usize;
    let mut i = 0usize;
    while i < b.len() && argc < MAX_ARGS {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i >= b.len() { break; }
        if b[i] == b'\'' || b[i] == b'"' {
            let q = b[i];
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != q { j += 1; }
            args[argc] = &s[start..j];
            i = if j < b.len() { j + 1 } else { j }; // step past the closing quote
        } else {
            let start = i;
            while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
            args[argc] = &s[start..i];
        }
        argc += 1;
    }
    argc
}

/// Strip one matching surrounding `'…'`/`"…"` pair from a rest-of-line argument (e.g. `echo`,
/// `write` content), so `echo "I am text"` prints `I am text`. Same minimal rule as `tokenize`.
fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// A command's typed failure (the `Err` of a command `Result`). Modelled on Rust's `Result`: the
/// common path is just "is it `Ok`?" - callers never need to know these names. The variants exist
/// for when you *do* want to pin a specific failure (negative tests, a future `assert`). Unit
/// variants (no payload): the human-readable detail stays in the command's own printed message;
/// this enum is the category. `Unknown` is the catch-all for a failure not yet given its own
/// variant, so *every* failure is at least `Err(Unknown)`. Grown one variant at a time as
/// commands are converted to the `Result` model (docs follow-up).
#[derive(Clone, Copy)]
enum ShellError {
    /// A file/path the command needed does not exist.
    FileNotFound,
    /// The action was refused by authority/policy (a protected core service, a session-critical
    /// service). Mirrors the kernel's "no ambient authority" refusals (§3.1).
    Denied,
    /// An `assert` did not hold (the test failed).
    AssertFailed,
    /// A failure not yet categorised into its own variant.
    Unknown,
}
impl ShellError {
    /// The variant's Rust-cased name, for `result` to print as `Err(<name>)`.
    fn name(self) -> &'static str {
        match self {
            ShellError::FileNotFound => "FileNotFound",
            ShellError::Denied => "Denied",
            ShellError::AssertFailed => "AssertFailed",
            ShellError::Unknown => "Unknown",
        }
    }
}

/// Run one command line. Returns the command's `Result` (the Ok/Err model): `Ok(())` on success,
/// `Err(ShellError)` on failure. `prev` is the previous line's result, so the `result` command
/// can report it. `depth` is the script-nesting level (0 = interactive); `run` is refused at
/// depth > 0 so a script can't run another script (keeps the user stack bounded). Commands are
/// being converted to return `Result` incrementally - those not yet converted run via the legacy
/// dispatch and are treated as `Ok`.
///
/// `#[inline(never)]`: `cmd_run` calls `execute` per script line, so `execute` must NOT be
/// inlined into `cmd_run` - that would fold `execute`'s whole frame (including the `pipe_run`
/// path's 64 KiB `Stream`) into `cmd_run`'s, blowing the bounded user stack on the nested
/// `run → cmd_run → execute` path (the same inlining-inflates-frame trap as the record builders).
#[inline(never)]
fn execute(ctx: &ShellCtx, line: &[u8], cwd: &mut Cwd, prev: Result<(), ShellError>, depth: u8, out: &mut Out) -> Result<(), ShellError> {
    let Ok(s) = core::str::from_utf8(line) else {
        ctx.console_writeln("shell: invalid input");
        return Err(ShellError::Unknown);
    };
    let s = s.trim();
    if s.is_empty() { return prev; } // a blank line is not a command - last result unchanged

    // Capability-mediated pipe: `producer | sink`. The shell brokers the channel
    // (Appendix D.3): spawn the consumer, then spawn the producer with a SEND cap
    // to the consumer's endpoint delegated to it - the producer has no ambient
    // authority of its own.
    if s.contains('|') {
        // One unified pipeline: threads bytes or records, with from/to bridging the two worlds.
        // Returns the pipeline's Result - an `… | assert` sink sets it (else Ok / a stage error).
        return pipe_run(ctx, cwd, s, out);
    }

    let mut args = [""; MAX_ARGS];
    let argc = tokenize(s, &mut args);
    if argc == 0 { return prev; }

    // Per-utility `help` / `version` (0_conventions.md): every utility self-documents.
    // `<util> help` and `<util> version` are intercepted here for every utility; subcommand
    // help (`<util> <sub> help`, e.g. `drives flash help`) is intercepted just below.
    if argc == 2 && is_util(args[0]) {
        if args[1] == "version" { util_version(ctx, args[0]); return Ok(()); }
        if args[1] == "help" { util_help(ctx, args[0]); return Ok(()); }
    }
    if argc == 3 && args[2] == "help" && is_util(args[0]) {
        if sub_help(ctx, args[0], args[1]) { return Ok(()); }
    }

    // Commands on the Ok/Err Result model (converted incrementally). These `return` their result.
    match args[0] {
        "read" => return if argc < 2 {
            ctx.console_writeln("usage: read <path>");
            Err(ShellError::Unknown)
        } else {
            cmd_read(ctx, cwd, args[1], out)
        },
        // `result` reports the PREVIOUS command's result (this one always succeeds at reporting).
        "result" => { cmd_result(ctx, prev); return Ok(()); }
        // `assert ok/fails <cmd>` - the result form (the content form `… | assert contains X` is
        // a pipe sink, handled in pipe_run). `s` is the trimmed whole line.
        "assert" => return cmd_assert(ctx, cwd, s["assert".len()..].trim(), depth),
        "run" => {
            if depth > 0 {
                ctx.console_writeln("run: a script cannot run another script (no nesting)");
                return Err(ShellError::Unknown);
            }
            if argc < 2 {
                ctx.console_writeln("usage: run <path> [args...]  |  run <path> save <path>");
                return Err(ShellError::Unknown);
            }
            // Optional `save <path>` streams the run REPORT to a file (the utility writes its own
            // file - direct, not a pipe; see cmd_selfcheck / docs/pipes.md). Otherwise the tokens
            // after the path are the script's params ($arg1.., $args, $argcount); $self is the path.
            let save = if argc >= 4 && args[2] == "save" { Some(args[3]) } else { None };
            let params = if save.is_some() { Params::empty(args[1]) } else { parse_params(ctx, s, args[1], 2) };
            return cmd_run(ctx, cwd, args[1], depth, save, &params);
        }
        // `selfcheck [save <path>]` - run the embedded suite; `save` streams its report to a file.
        "selfcheck" => return cmd_selfcheck(ctx, cwd, depth, s["selfcheck".len()..].trim()),
        _ => {}
    }

    // Dispatch - every command returns its `Result` (Ok/Err); an unknown command is `Err`.
    // The info commands always succeed (they return `Ok`), but they are on the model uniformly.
    return match args[0] {
        "help"    => cmd_help(ctx, depth),
        "clear"   => cmd_clear(ctx),
        "echo"    => cmd_echo(ctx, strip_quotes(s["echo".len()..].trim()), out),
        "input"   => { run_input(ctx, s["input".len()..].trim(), out); Ok(()) }
        "about"   => cmd_about(ctx, out),
        "version" => cmd_version_os(ctx, out),
        "mem"     => cmd_mem(ctx, out),
        "cores"   => cmd_cores(ctx, if argc >= 2 { args[1] } else { "" }, out),
        "trace"   => cmd_trace(ctx, s["trace".len()..].trim()),
        "date"    => cmd_date(ctx, if argc >= 2 { args[1] } else { "" }, out),
        "net"     => cmd_net(ctx, s["net".len()..].trim(), out),
        "ping"    => cmd_ping(ctx, s["ping".len()..].trim(), out),
        "sock"    => cmd_sock(ctx, out),
        "uptime"  => cmd_uptime(ctx),
        "random"  => cmd_random(ctx, if argc >= 2 { args[1] } else { "" }),
        "gpio"    => cmd_gpio(ctx, if argc >= 2 { args[1] } else { "" }, if argc >= 3 { args[2] } else { "" }),
        "wait"    => cmd_wait(ctx, if argc >= 2 { args[1] } else { "" }),
        "whatis"  => cmd_whatis(ctx, if argc >= 2 { args[1] } else { "" }, out),
        "status"  => cmd_status(ctx),
        "observe" => if argc >= 2 && args[1] == "now" { cmd_observe_now(ctx) } else { cmd_observe_live(ctx) },
        // The example record SERVICE, callable bare (renders its table) as well as piped.
        "roster"  => cmd_roster(ctx),
        // No argument → show the shell's OWN capabilities (authority is explicit; the shell can
        // inspect itself like any other service). `caps <bogus>` → Err(FileNotFound).
        "caps"    => if argc < 2 { cmd_caps(ctx, "shell") } else { cmd_caps(ctx, args[1]) },
        // service-control - on the Result model: `assert fails spawn supervisor` holds (a
        // protected core service is `Err(Denied)`); a missing arg is a usage `Err`.
        "spawn"   => {
            if argc < 2 { ctx.console_writeln("usage: spawn <svc> | <svc>,<svc>,...   (e.g. spawn ping,pong)"); Err(ShellError::Unknown) }
            else { cmd_spawn(ctx, args[1]) }
        }
        // Phase-0 naming-migration diagnostics (docs/naming-design.md).
        "spawncap" => {
            if argc < 2 { ctx.console_writeln("usage: spawncap <name>"); Err(ShellError::Unknown) }
            else { cmd_spawncap(ctx, args[1]) }
        }
        "spawnwired" => cmd_spawnwired(ctx),
        "kill"    => {
            if argc < 2 { ctx.console_writeln("usage: kill <svc> | <svc>,<svc>,... | all-services   ('help kill' for detail)"); Err(ShellError::Unknown) }
            else { cmd_kill(ctx, args[1]) }
        }
        "restart" => {
            if argc < 2 { ctx.console_writeln("usage: restart <name> [core]"); Err(ShellError::Unknown) }
            else {
                let core = if argc >= 3 { parse_u32(args[2]) } else { None };
                cmd_restart(ctx, args[1], core)
            }
        }
        "reboot"  => cmd_reboot(ctx), // `-> !` coerces to the match arm's Result type
        "chaos"   => cmd_chaos(ctx, cwd, s["chaos".len()..].trim()),
        "drives"  => cmd_drives(ctx, &args, argc),
        // ── file/storage commands - converted to the Result model ──
        // ("read" and "result" are on the Result model above, not here.)
        // file-as-capability (§7.10, P2): end-to-end demo + self-check on an existing file -
        // open → write/read VIA THE CAP → non-escalation (RO cap can't write) → forged-handle →
        // revoke-on-close. Prints per-step results; the harness asserts on them (Test 14).
        "fcap"    => cmd_fcap(ctx, if argc >= 2 { args[1] } else { "" }),
        "ls"      => cmd_ls(ctx, cwd, if argc >= 2 { args[1] } else { "" }, out),
        "edit"    => cmd_edit(ctx, cwd, s["edit".len()..].trim()),
        "write"   => cmd_write(ctx, cwd, s["write".len()..].trim()),
        "fmt"     => cmd_fmt(ctx, cwd, s["fmt".len()..].trim()),
        "mkdir"   => {
            if argc < 2 { ctx.console_writeln("usage: mkdir <path> [parents]"); Err(ShellError::Unknown) }
            else { cmd_mkdir(ctx, cwd, args[1], argc >= 3 && args[2] == "parents") }
        }
        "cd"      => cmd_cd(ctx, cwd, if argc >= 2 { args[1] } else { "/" }),
        "copy"    => {
            if argc < 3 { ctx.console_writeln("usage: copy <src> <dst> [recursive]"); Err(ShellError::Unknown) }
            else if argc >= 4 && args[3] == "recursive" { cmd_copy_tree(ctx, cwd, args[1], args[2]) }
            else { cmd_copy(ctx, cwd, args[1], args[2]) }
        }
        "rename"  => {
            if argc < 3 { ctx.console_writeln("usage: rename <path> <newname>"); Err(ShellError::Unknown) }
            else { cmd_rename(ctx, cwd, args[1], args[2]) }
        }
        "delete"  => {
            if argc < 2 { ctx.console_writeln("usage: delete <path> [recursive]"); Err(ShellError::Unknown) }
            else { cmd_delete(ctx, cwd, args[1], argc >= 3 && args[2] == "recursive") }
        }
        "move"    => {
            if argc < 3 { ctx.console_writeln("usage: move <src> <dst>"); Err(ShellError::Unknown) }
            else { cmd_move(ctx, cwd, args[1], args[2]) }
        }
        "find"    => {
            if argc < 2 { ctx.console_writeln("usage: find <name> [path]"); Err(ShellError::Unknown) }
            else { cmd_find(ctx, cwd, args[1], if argc >= 3 { args[2] } else { "/" }, out) }
        }
        "tree"    => cmd_tree(ctx, cwd, if argc >= 2 { args[1] } else { "" }, out),
        // filter built-ins (direct form) - on the Result model (Err(FileNotFound) on a bad path).
        "match"   => cmd_match(ctx, cwd, &args, argc),
        "count"   => cmd_count(ctx, cwd, &args, argc),
        "sort"    => cmd_sort(ctx, cwd, &args, argc),
        "first"   => cmd_take(ctx, cwd, &args, argc, false),
        "last"    => cmd_take(ctx, cwd, &args, argc, true),
        other => {
            // PATH-like system library: a name that is not a built-in but matches a baked-in library
            // script runs that script (a fresh, self-contained run; any args become $arg1..). Like
            // run/selfcheck it is one interpreter layer, so it is refused inside another script
            // (depth > 0) to keep the bounded user stack safe.
            if let Some(src) = library_script(other) {
                if depth > 0 {
                    ctx.console_writeln_fmt(format_args!(
                        "{}: a library command runs a script - not available inside another script", other));
                    return Err(ShellError::Unknown);
                }
                return run_lines(ctx, cwd, src.as_bytes(), depth + 1, out, &parse_params(ctx, s, other, 1), true);
            }
            // Build "unknown: <cmd>" in a stack buffer to avoid two ctx.log calls
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"unknown: ");
            write_bytes(&mut buf, &mut pos, other.as_bytes());
            ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or("unknown cmd"));
            Err(ShellError::Unknown) // an unknown command is a failure (so `assert fails …` holds)
        }
    };
}

/// `result` - print the previous command's result in Rust's `Result` shape: `Ok` on success,
/// `Err(<Variant>)` on failure (the specific reason was already printed by that command). The
/// common use is just eyeballing `Ok` vs not; a future `assert`/`run` reads the same value.
fn cmd_result(ctx: &ServiceContext, prev: Result<(), ShellError>) {
    match prev {
        Ok(()) => ctx.console_writeln("Ok"),
        Err(e) => ctx.console_writeln_fmt(format_args!("Err({})", e.name())),
    }
}

/// Largest script `run` will read (one `fs` file; the whole thing is buffered on the stack).
/// Largest resident `.gsh` CODE `run` will hold. `cmd_run` streams the file in and MINIFIES it on
/// load (comments / blank lines / indentation stripped, `compact_step`), so this bounds the *code*,
/// not the raw file - a heavily-commented source can be much larger on disk and still fit. 2 IO_CHUNKs
/// (~7 KiB) is the most the bounded user stack allows while this buffer coexists with the heaviest run
/// path (a `run … save` whose script has a `| assert` pipe: buffer + 16 KiB report + a 64 KiB pipe
/// stream + a 64 KiB assert cap; `4 x` was MEASURED to overflow it). Code past this truncates LOUDLY -
/// a huge script is a program (the `.gsh` -> `.gs` line, §26.6.1 / docs/scripting.md §9).
const SCRIPT_MAX: usize = 2 * IO_CHUNK; // 7112

/// Trim leading/trailing ASCII whitespace from a byte slice (lines/commands in a script).
fn trim_bytes(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && b[s].is_ascii_whitespace() { s += 1; }
    while e > s && b[e - 1].is_ascii_whitespace() { e -= 1; }
    &b[s..e]
}

/// The code span of a single line `buf[ls..le)`: strip a `#` comment (quote-aware: a `#` inside
/// `'…'`/`"…"`, or one not preceded by whitespace like `a#b`, is literal) and trim leading/trailing
/// whitespace. Returns `(code_start, code_end)`. INTERNAL whitespace is preserved - rest-of-line
/// commands (`echo`, `write`) stay byte-faithful.
fn compact_line(buf: &[u8], ls: usize, le: usize) -> (usize, usize) {
    let mut quote: u8 = 0;
    let mut ce = le;
    let mut i = ls;
    while i < le {
        let c = buf[i];
        if quote != 0 { if c == quote { quote = 0; } i += 1; continue; }
        match c {
            b'\'' | b'"' => quote = c,
            b'#' if i == ls || buf[i - 1].is_ascii_whitespace() => { ce = i; break; }
            _ => {}
        }
        i += 1;
    }
    let mut cs = ls;
    while cs < ce && buf[cs].is_ascii_whitespace() { cs += 1; }
    let mut e = ce;
    while e > cs && buf[e - 1].is_ascii_whitespace() { e -= 1; }
    (cs, e)
}

/// In-place streaming minifier step: compact the region `buf[start..dataend)` (a held partial line
/// plus a freshly-read raw chunk) by finalizing every COMPLETE line (comment/blank/indent stripped,
/// internal whitespace collapsed to single spaces outside quotes) into `buf[start..]`, and - unless
/// `eof` - leaving the trailing partial line moved up right after
/// the finalized code as the new hold. Compaction only ever shrinks, so the write cursor stays behind
/// the read cursor: purely in place, no scratch buffer (§26.6.1 - change the representation, not the
/// memory). Returns `(finalized_end, hold_len)`.
fn compact_step(buf: &mut [u8], start: usize, dataend: usize, eof: bool) -> (usize, usize) {
    let mut w = start;
    let mut ls = start;
    while ls < dataend {
        let mut le = ls;
        while le < dataend && buf[le] != b'\n' { le += 1; }
        let has_nl = le < dataend;
        if !has_nl && !eof {
            // trailing partial line - carry it forward as the new hold (moved up behind `w`).
            let plen = dataend - ls;
            if w != ls { for k in 0..plen { buf[w + k] = buf[ls + k]; } }
            return (w, plen);
        }
        let (cs, e) = compact_line(buf, ls, le);
        if e > cs {
            // Copy the trimmed content into buf[w..], COLLAPSING runs of whitespace OUTSIDE quotes
            // to a single space (gsh separates tokens by whitespace, so N spaces tokenize as one;
            // inside '..' / ".." whitespace is LITERAL and copied verbatim). Compaction only ever
            // shrinks, so w stays behind cs - purely in place, no scratch (§26.6.1). Leading/trailing
            // whitespace is already gone (compact_line trimmed it), so no stray edge space is emitted.
            let mut quote: u8 = 0;
            let mut prev_ws = false;
            let mut k = cs;
            while k < e {
                let c = buf[k];
                if quote != 0 {
                    buf[w] = c; w += 1;
                    if c == quote { quote = 0; }
                    prev_ws = false;
                } else if c == b'\'' || c == b'"' {
                    quote = c; buf[w] = c; w += 1; prev_ws = false;
                } else if c.is_ascii_whitespace() {
                    if !prev_ws { buf[w] = b' '; w += 1; prev_ws = true; }
                } else {
                    buf[w] = c; w += 1; prev_ws = false;
                }
                k += 1;
            }
            buf[w] = b'\n';
            w += 1;
        }
        if !has_nl { break; } // eof, last line had no newline
        ls = le + 1;
    }
    (w, 0)
}

/// `run <path>` - execute a script file: each command is run exactly as if typed at the prompt.
/// Lines split on `\n`; a non-comment line further splits on `;` (so a `.gsh` can be real
/// multi-line, or `cmd ; cmd ; cmd` - the latter is how scripts are authored before a host-side
/// editor exists). `#`-comment lines and blanks are skipped; each command is echoed (`> cmd`) so
/// the serial transcript self-documents; a summary reports how many ran and how many returned
/// `Err`. `run` itself is `Ok` iff every command was `Ok`.
///
/// Scripts cannot nest: `run` at `depth > 0` is refused (in `execute`). `#[inline(never)]` keeps
/// the script buffer off the hot pipe frame, and the `fs` reply is dropped before any command
/// runs - both bound the user stack (see the pipe stack-overflow lesson).
#[inline(never)]
fn cmd_run(ctx: &ShellCtx, cwd: &mut Cwd, arg: &str, depth: u8, save: Option<&str>, params: &Params) -> Result<(), ShellError> {
    let mut pbuf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut pbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    // Stream + MINIFY the script into the buffer (comments / blank lines / indentation stripped and
    // internal whitespace collapsed as it loads, so a heavily-commented or padded source loads whole
    // even when its raw size exceeds SCRIPT_MAX),
    // then resolve `import` / `from … import` at LOAD time (append the libs' functions in place).
    let mut script = [0u8; SCRIPT_MAX];
    let (mut code, truncated) = stream_minify(ctx, path, &mut script);
    if code == 0 {
        ctx.console_writeln_fmt(format_args!("run: not found or empty: {}", str_of(path)));
        return Err(ShellError::FileNotFound);
    }
    if truncated {
        ctx.console_writeln_fmt(format_args!("run: script CODE exceeds {} bytes - truncated (a huge script is a program)", SCRIPT_MAX));
    }
    resolve_imports(ctx, &mut script, &mut code);
    run_with_optional_save(ctx, cwd, &script[..code], depth, save, params)
}

const IMPORT_MAX: usize = 16; // max names in one `from … import a b c …`

/// Stream a file into `dst`, MINIFYING on the fly (`compact_step`): comments / blank lines /
/// indentation stripped as it loads. Returns `(code_len, truncated)`. Used for both the main script
/// and each imported lib; `dst` is a sub-slice of the resident buffer, so no second big buffer.
fn stream_minify(ctx: &ShellCtx, path: &[u8], dst: &mut [u8]) -> (usize, bool) {
    let cap = dst.len();
    if cap < IO_CHUNK { return (0, true); } // no room even for one chunk
    let mut code = 0usize;
    let mut hold = 0usize;
    let mut raw_off = 0u64;
    let mut truncated = false;
    loop {
        let region = code + hold;
        if region + IO_CHUNK > cap { truncated = true; break; }
        let n = fs_read_at(ctx, path, raw_off, &mut dst[region..region + IO_CHUNK]).unwrap_or(0);
        raw_off += n as u64;
        let eof = n < IO_CHUNK;
        let (nc, nh) = compact_step(dst, code, code + hold + n, eof);
        code = nc;
        hold = nh;
        if eof { break; }
    }
    if hold > 0 { let (nc, _) = compact_step(dst, code, code + hold, true); code = nc; }
    (code, truncated)
}

/// Reconstruct one function's definition as `fn <alias><params>{<body>}` into `scratch`, reading the
/// original text from the loaded lib in `script` (offsets via `ft`). `alias` renames only the entry
/// binding (the `import … as …` rename); params + body are copied verbatim (nested braces preserved).
/// Returns the byte length, or 0 if it would not fit `scratch` (a function too large to import).
fn build_fn_def(scratch: &mut [u8], alias: &[u8], script: &[u8], base: usize, ft: &FnTable, fi: usize) -> usize {
    let mut w = 0usize;
    let hdr = b"fn ";
    let ps = base + ft.params_off[fi] as usize;
    let pe = base + ft.params_end[fi] as usize;
    let bs = base + ft.body_start[fi] as usize;
    let be = base + ft.body_end[fi] as usize;
    let total = hdr.len() + alias.len() + (pe - ps) + 1 + (be - bs) + 1;
    if total > scratch.len() { return 0; }
    scratch[w..w + hdr.len()].copy_from_slice(hdr); w += hdr.len();
    scratch[w..w + alias.len()].copy_from_slice(alias); w += alias.len();
    scratch[w..w + (pe - ps)].copy_from_slice(&script[ps..pe]); w += pe - ps;
    scratch[w] = b'{'; w += 1;
    scratch[w..w + (be - bs)].copy_from_slice(&script[bs..be]); w += be - bs;
    scratch[w] = b'}'; w += 1;
    w
}

/// Resolve ONE import statement (`stmt`, copied out of `script`): `import <path>` (all functions) or
/// `from <path> import <name> [as <alias>] …` (selective). Loads the lib into the buffer tail, extracts
/// the requested functions (renamed on `as`) after it, then moves them down to `*code` - so only the
/// requested (renamed) functions remain, indexed by the run's pre-scan. Loud + no-op on any error.
#[inline(never)]
fn resolve_one_import(ctx: &ShellCtx, stmt: &[u8], is_from: bool, script: &mut [u8], code: &mut usize) {
    let s = str_of(stmt);
    let mut toks = [""; 40];
    let mut nt = 0usize;
    for t in s.split_ascii_whitespace() { if nt < toks.len() { toks[nt] = t; nt += 1; } }
    if nt < 2 { ctx.console_writeln("import: missing path"); return; }
    let mut path = [0u8; PATH_MAX];
    let pb = toks[1].as_bytes();
    let plen = pb.len().min(PATH_MAX);
    path[..plen].copy_from_slice(&pb[..plen]);
    // Selective specs: name [as alias] … (empty for the whole-lib `import <path>` form).
    let mut names = [[0u8; VAR_NAME_MAX]; IMPORT_MAX];
    let mut aliases = [[0u8; VAR_NAME_MAX]; IMPORT_MAX];
    let mut nlen = [0u8; IMPORT_MAX];
    let mut alen = [0u8; IMPORT_MAX];
    let mut nreq = 0usize;
    if is_from {
        if nt < 4 || toks[2] != "import" { ctx.console_writeln("import: expected 'from <path> import <name> …'"); return; }
        let mut i = 3;
        while i < nt && nreq < IMPORT_MAX {
            let name = toks[i]; i += 1;
            let mut alias = name;
            if i < nt && toks[i] == "as" {
                if i + 1 >= nt { ctx.console_writeln("import: 'as' needs an alias"); return; }
                alias = toks[i + 1]; i += 2;
            }
            let nb = name.as_bytes(); let nl = nb.len().min(VAR_NAME_MAX);
            names[nreq][..nl].copy_from_slice(&nb[..nl]); nlen[nreq] = nl as u8;
            let ab = alias.as_bytes(); let al = ab.len().min(VAR_NAME_MAX);
            aliases[nreq][..al].copy_from_slice(&ab[..al]); alen[nreq] = al as u8;
            nreq += 1;
        }
        if nreq == 0 { ctx.console_writeln("import: 'from <path> import' needs at least one name"); return; }
    }
    // Load the lib (minified) into the tail, pre-scan it, extract the wanted functions after it.
    let libstart = *code;
    let (liblen, _) = stream_minify(ctx, &path[..plen], &mut script[libstart..]);
    if liblen == 0 { ctx.console_writeln_fmt(format_args!("import: cannot load '{}'", str_of(&path[..plen]))); return; }
    let lib_ft = prescan_fns(ctx, &script[libstart..libstart + liblen]);
    let extstart = libstart + liblen;
    let mut w = extstart;
    let mut scratch = [0u8; 512];
    for fi in 0..lib_ft.count {
        let no = libstart + lib_ft.name_off[fi] as usize;
        let nl = lib_ft.name_len[fi] as usize;
        // Is this function wanted, and under what (aliased) name? Copy the alias out of `script` first.
        let mut abuf = [0u8; VAR_NAME_MAX];
        let mut al = 0usize;
        let want = if !is_from {
            abuf[..nl].copy_from_slice(&script[no..no + nl]); al = nl; true
        } else {
            let mut hit = false;
            for j in 0..nreq {
                if names[j][..nlen[j] as usize] == script[no..no + nl] {
                    al = alen[j] as usize;
                    abuf[..al].copy_from_slice(&aliases[j][..al]);
                    hit = true; break;
                }
            }
            hit
        };
        if !want { continue; }
        let dl = build_fn_def(&mut scratch, &abuf[..al], script, libstart, &lib_ft, fi);
        if dl == 0 { ctx.console_writeln("import: a function is too large to import"); continue; }
        if w + dl + 1 > script.len() { ctx.console_writeln("import: buffer full"); break; }
        script[w..w + dl].copy_from_slice(&scratch[..dl]);
        w += dl;
        script[w] = b'\n'; w += 1;
    }
    // Move the extracted functions [extstart..w] down over the loaded lib scratch to [libstart..].
    let extlen = w - extstart;
    for k in 0..extlen { script[libstart + k] = script[extstart + k]; }
    *code = libstart + extlen;
}

/// Load-time import resolution (§7 libraries): scan the main script for `import` / `from … import`
/// statements and, for each, append the requested (optionally `as`-renamed) library functions to the
/// buffer so the run's pre-scan indexes them. Explicit paths, flat namespace, loud on error. Runs
/// BEFORE any pipe/report buffers exist, so the small parse scratch is well inside the stack.
fn resolve_imports(ctx: &ShellCtx, script: &mut [u8], code: &mut usize) {
    let scan_end = *code; // only the MAIN script is scanned (a lib importing a lib is not resolved)
    let mut pos = 0usize;
    while pos < scan_end {
        pos = skip_seps(script, pos);
        if pos >= scan_end { break; }
        let is_import = matches_kw(script, pos, b"import");
        let is_from = matches_kw(script, pos, b"from");
        if !(is_import || is_from) {
            let (_, next) = read_statement(script, pos);
            pos = if next < scan_end && script[next] == b'{' {
                find_matching_brace(script, next).map(|e| e + 1).unwrap_or(scan_end)
            } else if next > pos { next } else { pos + 1 };
            continue;
        }
        // Copy the import statement OUT of `script`, then mutate `script` to load its lib.
        let (stmt, next) = read_statement(script, pos);
        let mut sb = [0u8; 256];
        let sl = stmt.len().min(sb.len());
        sb[..sl].copy_from_slice(&stmt[..sl]);
        pos = if next > pos { next } else { pos + 1 };
        resolve_one_import(ctx, &sb[..sl], is_from, script, code);
    }
}

// ───────────────────────── gsh interpreter (Slice 1: vars + expansion + params + fail) ─────────
// docs/scripting.md. Bounded, no-heap (§26.6): every structure below is a fixed array, loud on
// overflow. The interpreter lives ENTIRELY at the `run_lines` layer and does `$`-expansion BEFORE
// calling `execute`, so `execute`/`pipe_run` stay byte-identical to the flat-runner path - the only
// new persistent per-run frame is `Vars` (~5 KiB), well inside the run-path stack headroom.

const VAR_MAX: usize = 32;
const VAR_NAME_MAX: usize = 24;
const VAR_ARENA: usize = 4096;
const PARAM_MAX: usize = 9;
const EXP_MAX: usize = 1024;
/// Max gsh function call depth (recursion bound). Each level is a scope frame in `Vars` + a `Call`
/// block frame in the executor - explicit stacks, no native recursion (§9). Loud on overflow.
const CALL_DEPTH_MAX: usize = 16;
/// A MUTABLE variable's value lives in a fixed per-var slot, overwritten IN PLACE on reassign - so a
/// loop counter (`i = $i + 1`) never grows the value arena (§26.6.1). A mutable value past this is
/// loud. Immutable values still use the (larger) bump arena, since they are written once.
const MUT_SLOT: usize = 48;
/// Hard iteration backstop for the unbounded `loop` (§5): a runaway is a loud stop, never a silent
/// hang (invariant 12). `for` is self-bounded by its iterator; this guards `loop`.
const LOOP_CAP: u32 = 100_000;
/// Max `defer`red commands live at once (§5): each records only a (offset, len, scope-depth) into the
/// resident script - fixed, cheap. Loud past this.
const DEFER_MAX: usize = 16;

/// A gsh run's variable table: a fixed name array + a value bump-arena + one overflow flag (modeled
/// on the record `Table`). Immutable by default; `let mut` opts into reassignment. Loud on a full
/// table/arena, a redeclare, or an undeclared/immutable reassign - never silent (§26.7).
///
/// SCOPING (§7): a function call opens a scope with `enter_scope` (records the current count/alen as
/// the local base); its `let`s land above the base. A lookup inside a function sees its own locals
/// [base..count) then the IMMUTABLE globals [0..scope_count[0]) - never mutable globals or a caller's
/// locals (invariant 9, one layer up). `exit_scope` truncates back to the base, reclaiming the locals.
struct Vars {
    names: [[u8; VAR_NAME_MAX]; VAR_MAX],
    name_len: [u8; VAR_MAX],
    val_off: [u16; VAR_MAX],
    val_len: [u16; VAR_MAX],
    mutable: [bool; VAR_MAX],
    count: usize,
    arena: [u8; VAR_ARENA],
    alen: usize,
    // Mutable values live in fixed slots (overwritten in place on reassign - no arena growth in loops).
    mut_slots: [[u8; MUT_SLOT]; VAR_MAX],
    mut_len: [u8; VAR_MAX],
    // Secret taint (from `input secret`): the value may not be echoed to the console (§8). Rides along
    // on assignment. A guard rail against the accidental `echo`, not a vault (write/assign are allowed).
    secret: [bool; VAR_MAX],
    // Scope stack: scope_count[i]/scope_alen[i] = the table/arena base of the i-th open function.
    scope_count: [usize; CALL_DEPTH_MAX],
    scope_alen: [usize; CALL_DEPTH_MAX],
    sp: usize, // 0 = global scope only
}

/// Why a variable operation failed (each maps to a loud console line).
#[derive(Clone, Copy)]
enum VarErr { TableFull, ArenaFull, NameTooLong, Redeclare, Undeclared, Immutable, ValueTooLong, Reserved }

impl Vars {
    fn new() -> Self {
        Vars {
            names: [[0u8; VAR_NAME_MAX]; VAR_MAX], name_len: [0; VAR_MAX],
            val_off: [0; VAR_MAX], val_len: [0; VAR_MAX], mutable: [false; VAR_MAX],
            count: 0, arena: [0u8; VAR_ARENA], alen: 0,
            mut_slots: [[0u8; MUT_SLOT]; VAR_MAX], mut_len: [0; VAR_MAX], secret: [false; VAR_MAX],
            scope_count: [0; CALL_DEPTH_MAX], scope_alen: [0; CALL_DEPTH_MAX], sp: 0,
        }
    }
    fn name_eq(&self, i: usize, name: &[u8]) -> bool {
        &self.names[i][..self.name_len[i] as usize] == name
    }
    /// Is variable `name` secret-tainted (from `input secret`)?
    fn is_secret_name(&self, name: &[u8]) -> bool {
        self.lookup(name).map(|i| self.secret[i]).unwrap_or(false)
    }
    /// Mark variable `name` secret-tainted (after a `$(input secret …)` capture, or taint propagation).
    fn mark_secret_name(&mut self, name: &[u8]) {
        if let Some(i) = self.lookup(name) { self.secret[i] = true; }
    }
    /// The current scope's local base (0 at global scope).
    fn base(&self) -> usize { if self.sp > 0 { self.scope_count[self.sp - 1] } else { 0 } }
    /// Open a function scope: `let`s from here live only until `exit_scope`. Loud on depth overflow.
    fn enter_scope(&mut self) -> Result<(), VarErr> {
        if self.sp >= CALL_DEPTH_MAX { return Err(VarErr::TableFull); }
        self.scope_count[self.sp] = self.count;
        self.scope_alen[self.sp] = self.alen;
        self.sp += 1;
        Ok(())
    }
    /// Close the current function scope, reclaiming its locals (table + arena) back to the base.
    fn exit_scope(&mut self) {
        if self.sp == 0 { return; }
        self.sp -= 1;
        self.count = self.scope_count[self.sp];
        self.alen = self.scope_alen[self.sp];
    }
    /// Scope-aware lookup (§7): the current scope's locals (newest first), then only the IMMUTABLE
    /// globals - never a mutable global or a caller's locals. At global scope this is just the table.
    fn lookup(&self, name: &[u8]) -> Option<usize> {
        let base = self.base();
        for i in (base..self.count).rev() { if self.name_eq(i, name) { return Some(i); } }
        if self.sp > 0 {
            let gcount = self.scope_count[0];
            for i in (0..gcount).rev() { if !self.mutable[i] && self.name_eq(i, name) { return Some(i); } }
        }
        None
    }
    fn value(&self, i: usize) -> &[u8] {
        if self.mutable[i] {
            &self.mut_slots[i][..self.mut_len[i] as usize]
        } else {
            let off = self.val_off[i] as usize;
            &self.arena[off..off + self.val_len[i] as usize]
        }
    }
    /// Copy `val` into the arena; `None` if it would not fit (arena full or len > u16).
    fn intern(&mut self, val: &[u8]) -> Option<(u16, u16)> {
        if val.len() > u16::MAX as usize || self.alen + val.len() > VAR_ARENA { return None; }
        let off = self.alen as u16;
        self.arena[self.alen..self.alen + val.len()].copy_from_slice(val);
        self.alen += val.len();
        Some((off, val.len() as u16))
    }
    fn define(&mut self, name: &[u8], val: &[u8], mutable: bool) -> Result<(), VarErr> {
        if name.len() > VAR_NAME_MAX { return Err(VarErr::NameTooLong); }
        // Reserved parameter words resolve before variables (`push_ref`), so a binding that shadows
        // one is unreadable - refuse it loudly at THE binding funnel (covers `let`, for-loop vars, and
        // fn params alike; §26.4, audit U2). `let` also pre-checks via `valid_var_name`.
        if is_reserved_param_name(name) { return Err(VarErr::Reserved); }
        // Redeclare is scope-LOCAL: a function's local may shadow a global of the same name.
        let base = self.base();
        for i in base..self.count { if self.name_eq(i, name) { return Err(VarErr::Redeclare); } }
        if self.count >= VAR_MAX { return Err(VarErr::TableFull); }
        let i = self.count;
        if mutable {
            // A mutable value lives in a fixed slot (overwritten in place on reassign - no arena growth).
            if val.len() > MUT_SLOT { return Err(VarErr::ValueTooLong); }
            self.mut_slots[i][..val.len()].copy_from_slice(val);
            self.mut_len[i] = val.len() as u8;
        } else {
            let (off, len) = self.intern(val).ok_or(VarErr::ArenaFull)?;
            self.val_off[i] = off; self.val_len[i] = len;
        }
        self.names[i][..name.len()].copy_from_slice(name);
        self.name_len[i] = name.len() as u8;
        self.mutable[i] = mutable;
        self.secret[i] = false; // a fresh define clears any stale taint on a reused slot
        self.count += 1;
        Ok(())
    }
    fn reassign(&mut self, name: &[u8], val: &[u8]) -> Result<(), VarErr> {
        let i = self.lookup(name).ok_or(VarErr::Undeclared)?;
        self.set_slot(i, val)
    }
    /// Overwrite a mutable variable's slot IN PLACE (no arena growth). Loud if immutable or too long.
    fn set_slot(&mut self, i: usize, val: &[u8]) -> Result<(), VarErr> {
        if !self.mutable[i] { return Err(VarErr::Immutable); }
        if val.len() > MUT_SLOT { return Err(VarErr::ValueTooLong); }
        self.mut_slots[i][..val.len()].copy_from_slice(val);
        self.mut_len[i] = val.len() as u8;
        Ok(())
    }
    /// Ensure a mutable loop variable `name` holds `val`: reassign if it exists (must be mutable),
    /// else define it fresh. Returns its index.
    fn set_loop_var(&mut self, name: &[u8], val: &[u8]) -> Result<usize, VarErr> {
        if let Some(i) = self.lookup(name) { self.set_slot(i, val)?; Ok(i) }
        else { self.define(name, val, true)?; Ok(self.count - 1) }
    }
    /// Reset the table + arena to a saved base (drops a loop body's per-iteration locals, so a `let`
    /// inside the body is fresh each iteration, while variables below the base stay visible).
    fn reset_to(&mut self, count: usize, alen: usize) {
        if count <= self.count { self.count = count; self.alen = alen; }
    }
}

/// Print the loud message for a variable error (`name` is the offending binding).
fn var_err_msg(ctx: &ServiceContext, name: &str, e: VarErr) {
    match e {
        VarErr::TableFull => ctx.console_writeln_fmt(format_args!("gsh: too many variables (max {}) at '{}'", VAR_MAX, name)),
        VarErr::ArenaFull => ctx.console_writeln_fmt(format_args!("gsh: variable storage full at '{}'", name)),
        VarErr::NameTooLong => ctx.console_writeln_fmt(format_args!("gsh: variable name too long (max {}): '{}'", VAR_NAME_MAX, name)),
        VarErr::Redeclare => ctx.console_writeln_fmt(format_args!("gsh: '{}' already declared (mutate with 'let mut' + '{} = ...')", name, name)),
        VarErr::Undeclared => ctx.console_writeln_fmt(format_args!("gsh: cannot reassign undeclared '{}'", name)),
        VarErr::Immutable => ctx.console_writeln_fmt(format_args!("gsh: cannot reassign immutable '{}' (declare it 'let mut')", name)),
        VarErr::ValueTooLong => ctx.console_writeln_fmt(format_args!("gsh: value for mutable '{}' too long (max {} bytes)", name, MUT_SLOT)),
        VarErr::Reserved => ctx.console_writeln_fmt(format_args!("gsh: '{}' is a reserved parameter word ($arg1..$arg9/$args/$argcount/$self) - cannot be a variable, loop var, or fn param", name)),
    }
}

/// A gsh run's parameters: `$self` (invoked name), `$arg1..$arg9`, `$args` (all), `$argcount`. Zero-copy - the
/// slices borrow the `run` line.
struct Params<'a> {
    argv: [&'a str; PARAM_MAX],
    argc: usize,
    name: &'a str,
}
impl<'a> Params<'a> {
    fn empty(name: &'a str) -> Self { Params { argv: [""; PARAM_MAX], argc: 0, name } }
}

/// Scan one quote-aware word from `b` starting at `i`; returns `(value_start, value_end, next_i)`
/// with any surrounding quote pair stripped from the value span.
fn scan_word(b: &[u8], mut i: usize) -> (usize, usize, usize) {
    if i < b.len() && (b[i] == b'\'' || b[i] == b'"') {
        let q = b[i]; let s = i + 1; let mut j = s;
        while j < b.len() && b[j] != q { j += 1; }
        (s, j, if j < b.len() { j + 1 } else { j })
    } else {
        let s = i;
        while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
        (s, i, i)
    }
}

/// Parse script params from a raw `run` line: skip `skip` leading words (the `run` verb + the path),
/// then collect up to `PARAM_MAX` quote-aware tokens. `name` becomes `$self`.
fn parse_params<'a>(ctx: &ServiceContext, line: &'a str, name: &'a str, skip: usize) -> Params<'a> {
    let b = line.as_bytes();
    let mut i = 0usize;
    let mut p = Params::empty(name);
    for _ in 0..skip {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i >= b.len() { return p; }
        let (_, _, next) = scan_word(b, i); i = next;
    }
    while p.argc < PARAM_MAX {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i >= b.len() { break; }
        let (s, e, next) = scan_word(b, i);
        p.argv[p.argc] = &line[s..e]; p.argc += 1; i = next;
    }
    // Loud on the ceiling (§26.6, audit U5): arguments past PARAM_MAX are unreachable via
    // $args/$argcount - say so rather than silently swallowing them.
    while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
    if i < b.len() {
        ctx.console_writeln_fmt(format_args!(
            "gsh: only the first {} arguments are available ($arg1..$arg{}); the rest were dropped", PARAM_MAX, PARAM_MAX));
    }
    p
}

/// A bounded expansion output buffer (one command line's worth). Loud overflow (§26.6).
struct ExpBuf { buf: [u8; EXP_MAX], len: usize, overflow: bool }
impl ExpBuf {
    fn new() -> Self { ExpBuf { buf: [0u8; EXP_MAX], len: 0, overflow: false } }
    fn push(&mut self, c: u8) { if self.len < EXP_MAX { self.buf[self.len] = c; self.len += 1; } else { self.overflow = true; } }
    fn push_bytes(&mut self, b: &[u8]) { for &c in b { self.push(c); } }
    fn push_u32(&mut self, mut n: u32) {
        if n == 0 { self.push(b'0'); return; }
        let mut tmp = [0u8; 10]; let mut k = 0;
        while n > 0 { tmp[k] = b'0' + (n % 10) as u8; n /= 10; k += 1; }
        while k > 0 { k -= 1; self.push(tmp[k]); }
    }
    fn push_i64(&mut self, v: i64) {
        if v < 0 { self.push(b'-'); }
        let mut n = (v as i128).unsigned_abs(); // i128 abs is safe even for i64::MIN
        if n == 0 { self.push(b'0'); return; }
        let mut tmp = [0u8; 24]; let mut k = 0;
        while n > 0 { tmp[k] = b'0' + (n % 10) as u8; n /= 10; k += 1; }
        while k > 0 { k -= 1; self.push(tmp[k]); }
    }
    fn as_bytes(&self) -> &[u8] { &self.buf[..self.len] }
}

/// Resolve one `$...` reference at `b[i]` (`b[i] == b'$'`) and push its value into `out`. Returns
/// the index just past the reference, or `Err` (loud) on an undefined var/param or unsupported `$(`.
fn push_ref(ctx: &ServiceContext, b: &[u8], i: usize, vars: &Vars, params: &Params, out: &mut ExpBuf) -> Result<usize, ()> {
    let j = i + 1; // past '$'
    if j >= b.len() { ctx.console_writeln("gsh: lone '$'"); return Err(()); }
    if b[j] == b'(' { ctx.console_writeln("gsh: $( ) capture works as a whole value (let x = $(cmd)), not embedded"); return Err(()); }
    // The POSIX cipher forms are RETIRED (they said nothing about their purpose); the error
    // teaches the words that replaced them rather than reporting a puzzling "undefined variable".
    if matches!(b[j], b'@' | b'#') || b[j].is_ascii_digit() {
        ctx.console_writeln("gsh: $1/$@/$#/$0 are retired - use $arg1..$arg9, $args, $argcount, $self");
        return Err(());
    }
    let start = j;
    let mut k = j;
    while k < b.len() && (b[k] == b'_' || b[k].is_ascii_alphanumeric()) { k += 1; }
    if k == start { ctx.console_writeln("gsh: '$' must be followed by a name"); return Err(()); }
    let name = &b[start..k];
    // Reserved parameter words resolve BEFORE variables, so they can never be shadowed
    // (`let` refuses these names outright): $args (all arguments, space-joined),
    // $argcount (how many), $self (the invoked name), $arg1..$arg9 (positional).
    match name {
        b"args" => { for a in 0..params.argc { if a > 0 { out.push(b' '); } out.push_bytes(params.argv[a].as_bytes()); } return Ok(k); }
        b"argcount" => { out.push_u32(params.argc as u32); return Ok(k); }
        b"self" => { out.push_bytes(params.name.as_bytes()); return Ok(k); }
        _ => {}
    }
    if name.len() == 4 && &name[..3] == b"arg" && (b'1'..=b'9').contains(&name[3]) {
        let idx = (name[3] - b'1') as usize;
        if idx >= params.argc {
            ctx.console_writeln_fmt(format_args!("gsh: $arg{} not provided ($argcount = {})", (name[3] - b'0') as u32, params.argc));
            return Err(());
        }
        out.push_bytes(params.argv[idx].as_bytes());
        return Ok(k);
    }
    match vars.lookup(name) {
        Some(vi) => { out.push_bytes(vars.value(vi)); Ok(k) }
        None => { ctx.console_writeln_fmt(format_args!("gsh: undefined variable '${}'", str_of(name))); Err(()) }
    }
}

/// Expand `$...` refs in a COMMAND line, PRESERVING quotes so `execute`'s tokenizer still works.
/// Single-quoted spans copy literally (no expansion); double-quoted spans keep their quotes and
/// expand `$` inside; a bare `$` expands. Loud on undefined refs / overflow.
fn expand_cmd(ctx: &ServiceContext, s: &str, vars: &Vars, params: &Params, out: &mut ExpBuf) -> Result<(), ()> {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\'' => { out.push(b'\''); i += 1; while i < b.len() && b[i] != b'\'' { out.push(b[i]); i += 1; } if i < b.len() { out.push(b'\''); i += 1; } }
            b'"' => {
                out.push(b'"'); i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'$' { i = push_ref(ctx, b, i, vars, params, out)?; } else { out.push(b[i]); i += 1; }
                }
                if i < b.len() { out.push(b'"'); i += 1; }
            }
            b'$' => { i = push_ref(ctx, b, i, vars, params, out)?; }
            c => { out.push(c); i += 1; }
        }
    }
    if out.overflow { ctx.console_writeln("gsh: expanded line too long"); return Err(()); }
    Ok(())
}

/// Expand a VALUE (the RHS of `let`/reassignment, or a `fail` message): a `'literal'`, an
/// interpolated `"..."`, or a bare word (whole, spaces kept) with `$` expanded. Quotes are consumed
/// (the value is their content). Loud on undefined refs / overflow.
fn expand_val(ctx: &ServiceContext, s: &str, vars: &Vars, params: &Params, out: &mut ExpBuf) -> Result<(), ()> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        out.push_bytes(&b[1..b.len() - 1]);
    } else if b.len() >= 2 && b[0] == b'"' && b[b.len() - 1] == b'"' {
        let inner = &b[1..b.len() - 1]; let mut i = 0;
        while i < inner.len() { if inner[i] == b'$' { i = push_ref(ctx, inner, i, vars, params, out)?; } else { out.push(inner[i]); i += 1; } }
    } else if is_arith(s) {
        // an integer arithmetic expression (value position, docs/scripting.md §3).
        match eval_arith(ctx, s, vars, params) { Some(v) => out.push_i64(v), None => return Err(()) }
    } else {
        let mut i = 0;
        while i < b.len() { if b[i] == b'$' { i = push_ref(ctx, b, i, vars, params, out)?; } else { out.push(b[i]); i += 1; } }
    }
    if out.overflow { ctx.console_writeln("gsh: value too long"); return Err(()); }
    Ok(())
}

/// A gsh identifier: starts with a letter or `_`, then letters/digits/`_`, bounded length.
/// A reserved parameter WORD ($args/$argcount/$self/$arg1..$arg9). These resolve before variables in
/// `push_ref`, so a binding that shadows one could never be read back - every binding path refuses them.
fn is_reserved_param_name(b: &[u8]) -> bool {
    matches!(b, b"args" | b"argcount" | b"self")
        || (b.len() == 4 && &b[..3] == b"arg" && (b'1'..=b'9').contains(&b[3]))
}

fn valid_var_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > VAR_NAME_MAX { return false; }
    if !(b[0] == b'_' || b[0].is_ascii_alphabetic()) { return false; }
    if !b.iter().all(|&c| c == b'_' || c.is_ascii_alphanumeric()) { return false; }
    if is_reserved_param_name(b) { return false; } // never let a binding shadow a reserved param (§26.4)
    true
}

/// `let [mut] <name> = <value>` - declare a binding.
fn stmt_let(ctx: &ShellCtx, cwd: &Cwd, rest: &str, vars: &mut Vars, params: &Params) -> Result<(), ShellError> {
    let (mutable, rest) = match rest.strip_prefix("mut ") { Some(r) => (true, r.trim_start()), None => (false, rest) };
    let (name, after) = split_first(rest);
    let after = after.trim_start();
    let value = match after.strip_prefix('=') {
        Some(v) => v.trim_start(),
        None => { ctx.console_writeln("gsh: let: expected '=' (let [mut] <name> = <value>)"); return Err(ShellError::Unknown); }
    };
    if !valid_var_name(name) { ctx.console_writeln_fmt(format_args!("gsh: invalid variable name '{}'", name)); return Err(ShellError::Unknown); }
    // `let x = $( cmd )` - capture command output as the value.
    if let Some(inner) = capture_form(value) {
        return capture_define(ctx, cwd, name, inner, mutable, vars);
    }
    let mut exp = ExpBuf::new();
    if expand_val(ctx, value, vars, params, &mut exp).is_err() { return Err(ShellError::Unknown); }
    let tainted = refs_secret(value, vars); // secret taint rides along on assignment (§8)
    match vars.define(name.as_bytes(), exp.as_bytes(), mutable) {
        Ok(()) => { if tainted { vars.mark_secret_name(name.as_bytes()); } Ok(()) }
        Err(e) => { var_err_msg(ctx, name, e); Err(ShellError::Unknown) }
    }
}

/// `<name> = <value>` - reassign a mutable binding.
fn stmt_reassign(ctx: &ShellCtx, cwd: &Cwd, name: &str, value: &str, vars: &mut Vars, params: &Params) -> Result<(), ShellError> {
    // `x = $( cmd )` - capture command output as the new value.
    if let Some(inner) = capture_form(value) {
        return capture_reassign(ctx, cwd, name, inner, vars);
    }
    let mut exp = ExpBuf::new();
    if expand_val(ctx, value, vars, params, &mut exp).is_err() { return Err(ShellError::Unknown); }
    let tainted = refs_secret(value, vars); // secret taint rides along on assignment (§8)
    match vars.reassign(name.as_bytes(), exp.as_bytes()) {
        Ok(()) => { if tainted { vars.mark_secret_name(name.as_bytes()); } Ok(()) }
        Err(e) => { var_err_msg(ctx, name, e); Err(ShellError::Unknown) }
    }
}

/// The outcome of one gsh statement: continue to the next, or stop the run (a `fail`).
enum StmtOutcome { Cont(Result<(), ShellError>), Stop(Result<(), ShellError>) }

/// Run one gsh statement: a `let`/reassignment/`fail`, or - after `$`-expansion - a plain command
/// handed to the existing `execute`. `vars` is the run's variable table; `params` its parameters.
fn run_stmt(ctx: &ShellCtx, cwd: &mut Cwd, stmt: &str, prev: Result<(), ShellError>, depth: u8, vars: &mut Vars, params: &Params, out: &mut Out) -> StmtOutcome {
    let (head, rest) = split_first(stmt);
    // `fail <msg>` - print loudly and stop the run with Err.
    if head == "fail" {
        let mut exp = ExpBuf::new();
        if expand_val(ctx, rest, vars, params, &mut exp).is_ok() {
            ctx.console_writeln_fmt(format_args!("fail: {}", str_of(exp.as_bytes())));
        } else {
            ctx.console_writeln("fail");
        }
        return StmtOutcome::Stop(Err(ShellError::Unknown));
    }
    // `let [mut] name = value`
    if head == "let" {
        return StmtOutcome::Cont(stmt_let(ctx, cwd, rest, vars, params));
    }
    // reassignment: the second token is exactly `=` (the one disambiguation rule, docs/scripting.md §3).
    if rest == "=" || rest.starts_with("= ") {
        let value = rest[1..].trim_start();
        return StmtOutcome::Cont(stmt_reassign(ctx, cwd, head, value, vars, params));
    }
    // Secret taint (§8): a secret value may NOT be echoed to the console. Refuse loudly; the value
    // never reaches expansion, so it cannot print. (write/assign/use are allowed - it is a guard rail
    // against the accidental echo, not a vault.)
    if head == "echo" && refs_secret(rest, vars) {
        ctx.console_writeln("gsh: refusing to echo a secret value - it stays off the console");
        return StmtOutcome::Cont(Err(ShellError::Unknown));
    }
    // a plain command: `$`-expand, then run it exactly as the flat runner did.
    let mut exp = ExpBuf::new();
    if expand_cmd(ctx, stmt, vars, params, &mut exp).is_err() {
        return StmtOutcome::Cont(Err(ShellError::Unknown));
    }
    StmtOutcome::Cont(execute(ctx, exp.as_bytes(), cwd, prev, depth, out))
}

// ── Slice 2: conditions (comparisons, `in`, command, `result`) + `if`/`else if`/`else` blocks. ──

const IF_DEPTH_MAX: usize = 32;

/// Parse a byte slice as a signed integer (optional leading `-`). `None` if it is not an integer.
fn parse_i64(b: &[u8]) -> Option<i64> {
    if b.is_empty() { return None; }
    let (neg, digits) = if b[0] == b'-' { (true, &b[1..]) } else { (false, b) };
    if digits.is_empty() { return None; }
    let mut n: i64 = 0;
    for &c in digits {
        if !c.is_ascii_digit() { return None; }
        n = n.checked_mul(10)?.checked_add((c - b'0') as i64)?;
    }
    Some(if neg { -n } else { n })
}

fn is_cmp_op(t: &str) -> bool { matches!(t, "==" | "!=" | "<" | ">" | "<=" | ">=") }

/// True if `cond` contains a top-level comparison operator or an `in` membership token - i.e. it is a
/// value condition, not a bare `fnname args` function call. Used to tell `if x == y` from `if myfn x`.
fn cond_has_operator(cond: &str) -> bool {
    let mut i = 0usize;
    let cb = cond.as_bytes();
    while i < cb.len() {
        while i < cb.len() && cb[i].is_ascii_whitespace() { i += 1; }
        if i >= cb.len() { break; }
        let (tok, end) = raw_token(cond, i);
        if tok == "in" || is_cmp_op(tok) { return true; }
        i = end;
    }
    false
}

/// Compare two already-expanded operands with `op`. Numeric if BOTH parse as integers, else a
/// byte-wise (lexicographic) comparison. `None` on a bad operator.
fn compare(l: &[u8], r: &[u8], op: &str) -> Option<bool> {
    use core::cmp::Ordering;
    let ord = match (parse_i64(l), parse_i64(r)) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => l.cmp(r),
    };
    Some(match op {
        "==" => ord == Ordering::Equal,
        "!=" => ord != Ordering::Equal,
        "<"  => ord == Ordering::Less,
        ">"  => ord == Ordering::Greater,
        "<=" => ord != Ordering::Greater,
        ">=" => ord != Ordering::Less,
        _ => return None,
    })
}

/// Does the previous statement's result match a result tag (`Ok`, `Err` = any failure, or a specific
/// variant)? `None` if `tag` is not a known result kind.
fn result_matches(prev: Result<(), ShellError>, tag: &[u8]) -> Option<bool> {
    Some(match tag {
        b"Ok" => prev.is_ok(),
        b"Err" => prev.is_err(),
        b"FileNotFound" => matches!(prev, Err(ShellError::FileNotFound)),
        b"Denied" => matches!(prev, Err(ShellError::Denied)),
        b"AssertFailed" => matches!(prev, Err(ShellError::AssertFailed)),
        b"Unknown" => matches!(prev, Err(ShellError::Unknown)),
        _ => return None,
    })
}

/// Read one raw token (KEEPING any surrounding quotes) from `s` at `from`; returns `(token, end)`.
fn raw_token(s: &str, from: usize) -> (&str, usize) {
    let b = s.as_bytes();
    let mut i = from;
    while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
    let start = i;
    if i < b.len() && (b[i] == b'\'' || b[i] == b'"') {
        let q = b[i]; i += 1;
        while i < b.len() && b[i] != q { i += 1; }
        if i < b.len() { i += 1; }
    } else {
        while i < b.len() && !b[i].is_ascii_whitespace() { i += 1; }
    }
    (&s[start..i], i)
}

/// `$x in w1 w2 ...` - true if the expanded `lhs` equals any expanded word in `words`.
fn membership(ctx: &ServiceContext, lhs: &[u8], words: &str, vars: &Vars, params: &Params) -> bool {
    let b = words.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i >= b.len() { break; }
        let (raw, end) = raw_token(words, i);
        i = end;
        let mut wb = ExpBuf::new();
        if expand_val(ctx, raw, vars, params, &mut wb).is_ok() && wb.as_bytes() == lhs { return true; }
    }
    false
}

/// True if `s` is an integer arithmetic expression: a whitespace-separated `+ - * / %` operator or a
/// `(`/`)` grouping token appears. (A single operand, or `$dir/sub` with no spaces, is NOT arithmetic
/// - the space rule keeps paths and math distinct, docs/scripting.md §3.)
fn is_arith(s: &str) -> bool {
    s.split_ascii_whitespace().any(|t| matches!(t, "+" | "-" | "*" | "/" | "%" | "(" | ")"))
}

fn arith_prec(op: u8) -> u8 { match op { b'*' | b'/' | b'%' => 2, b'+' | b'-' => 1, _ => 0 } }

/// Apply a binary operator, checked. `None` on overflow or divide/modulo by zero (a loud error).
fn arith_apply(a: i64, b: i64, op: u8) -> Option<i64> {
    match op {
        b'+' => a.checked_add(b),
        b'-' => a.checked_sub(b),
        b'*' => a.checked_mul(b),
        b'/' => if b == 0 { None } else { a.checked_div(b) },
        b'%' => if b == 0 { None } else { a.checked_rem(b) },
        _ => None,
    }
}

/// Resolve an operand token (an integer literal, or a `$var`/`$param` that expands to an integer) to
/// an `i64`. A non-integer operand is a loud error (`None`).
fn arith_operand(ctx: &ServiceContext, tok: &str, vars: &Vars, params: &Params) -> Option<i64> {
    let bytes = if tok.as_bytes().first() == Some(&b'$') {
        let mut eb = ExpBuf::new();
        if expand_val(ctx, tok, vars, params, &mut eb).is_err() { return None; }
        // parse from a copy (eb borrows can't outlive), so read into a small stack buffer
        return match parse_i64(eb.as_bytes()) {
            Some(v) => Some(v),
            None => { ctx.console_writeln_fmt(format_args!("gsh: '{}' is not an integer", tok)); None }
        };
    } else {
        tok.as_bytes()
    };
    match parse_i64(bytes) {
        Some(v) => Some(v),
        None => { ctx.console_writeln_fmt(format_args!("gsh: '{}' is not an integer", tok)); None }
    }
}

/// Evaluate an integer arithmetic expression with `+ - * / %` and `( )` grouping (usual precedence,
/// left-associative), checked. Shunting-yard over fixed operand/operator stacks - iterative, no
/// native recursion (§9). Loud (`None`) on overflow, divide-by-zero, a non-integer operand, an
/// unbalanced paren, or too-complex an expression.
fn eval_arith(ctx: &ServiceContext, expr: &str, vars: &Vars, params: &Params) -> Option<i64> {
    const AST: usize = 32;
    let mut nums = [0i64; AST]; let mut ns = 0usize;
    let mut ops = [0u8; AST]; let mut os = 0usize;
    // pop one operator and apply it to the top two operands.
    fn reduce(nums: &mut [i64], ns: &mut usize, op: u8, ctx: &ServiceContext) -> bool {
        if *ns < 2 { ctx.console_writeln("gsh: malformed arithmetic"); return false; }
        let b = nums[*ns - 1]; let a = nums[*ns - 2]; *ns -= 2;
        match arith_apply(a, b, op) {
            Some(v) => { nums[*ns] = v; *ns += 1; true }
            None => { ctx.console_writeln("gsh: arithmetic overflow or divide by zero"); false }
        }
    }
    for tok in expr.split_ascii_whitespace() {
        let tb = tok.as_bytes();
        if tok == "(" {
            if os >= AST { ctx.console_writeln("gsh: expression too complex"); return None; }
            ops[os] = b'('; os += 1;
        } else if tok == ")" {
            loop {
                if os == 0 { ctx.console_writeln("gsh: unbalanced ')'"); return None; }
                os -= 1;
                let op = ops[os];
                if op == b'(' { break; }
                if !reduce(&mut nums, &mut ns, op, ctx) { return None; }
            }
        } else if tb.len() == 1 && matches!(tb[0], b'+' | b'-' | b'*' | b'/' | b'%') {
            let op = tb[0];
            while os > 0 && ops[os - 1] != b'(' && arith_prec(ops[os - 1]) >= arith_prec(op) {
                os -= 1;
                let o = ops[os];
                if !reduce(&mut nums, &mut ns, o, ctx) { return None; }
            }
            if os >= AST { ctx.console_writeln("gsh: expression too complex"); return None; }
            ops[os] = op; os += 1;
        } else {
            let v = arith_operand(ctx, tok, vars, params)?;
            if ns >= AST { ctx.console_writeln("gsh: expression too long"); return None; }
            nums[ns] = v; ns += 1;
        }
    }
    while os > 0 {
        os -= 1;
        let op = ops[os];
        if op == b'(' { ctx.console_writeln("gsh: unbalanced '('"); return None; }
        if !reduce(&mut nums, &mut ns, op, ctx) { return None; }
    }
    if ns != 1 { ctx.console_writeln("gsh: malformed arithmetic"); return None; }
    Some(nums[0])
}

/// Evaluate a condition to a bool. A condition is: `!<cond>` (negated), `<lhs> in <words...>`
/// (membership), `<lhs> <op> <rhs>` (comparison; `result` compares by kind), or a command (true iff
/// it returns `Ok`). A command condition does NOT update `result` - only real statements do.
fn eval_cond(ctx: &ShellCtx, cwd: &mut Cwd, cond: &str, vars: &Vars, params: &Params, prev: Result<(), ShellError>, depth: u8) -> bool {
    // Strip leading `!` ITERATIVELY (a parity flag), never by native recursion: a long `!!!...` run
    // would otherwise recurse once per `!` and overflow the bounded user stack (§26.6.1 - the
    // no-native-recursion rule every other gsh construct obeys). Then evaluate the bare condition once.
    let mut c = cond.trim();
    let mut negate = false;
    while let Some(rest) = c.strip_prefix('!') { negate = !negate; c = rest.trim(); }
    eval_cond_bare(ctx, cwd, c, vars, params, prev, depth) ^ negate
}

/// Evaluate a condition that has had any leading `!` already stripped (see `eval_cond`). `cond` is trimmed.
fn eval_cond_bare(ctx: &ShellCtx, cwd: &mut Cwd, cond: &str, vars: &Vars, params: &Params, prev: Result<(), ShellError>, depth: u8) -> bool {
    if cond.is_empty() { ctx.console_writeln("gsh: empty condition"); return false; }
    // Scan tokens for `in` (membership) or a comparison operator, so either side may be a multi-token
    // arithmetic expression (`$i + 1 > $max`), not just a single token (docs/scripting.md §3-§4).
    let cb = cond.as_bytes();
    let mut i = 0usize;
    let mut cmp: Option<(usize, usize, &str)> = None; // (op_start, op_end, op)
    let mut inpos: Option<(usize, usize)> = None;      // (in_start, in_end)
    while i < cb.len() {
        while i < cb.len() && cb[i].is_ascii_whitespace() { i += 1; }
        if i >= cb.len() { break; }
        let start = i;
        let (tok, end) = raw_token(cond, i);
        if tok == "in" { inpos = Some((start, end)); break; }
        if is_cmp_op(tok) { cmp = Some((start, end, tok)); break; }
        i = end;
    }
    // membership: `<lhs> in w1 w2 ...`
    if let Some((s, e)) = inpos {
        let mut lb = ExpBuf::new();
        if expand_val(ctx, cond[..s].trim(), vars, params, &mut lb).is_err() { return false; }
        return membership(ctx, lb.as_bytes(), cond[e..].trim_start(), vars, params);
    }
    // comparison: `<lhs> <op> <rhs>`
    if let Some((s, e, op)) = cmp {
        let lhs = cond[..s].trim();
        let rhs = cond[e..].trim();
        // `result` compares by kind (Ok / Err / specific variant), with == / != only.
        if lhs == "result" || rhs == "result" {
            let tag = if lhs == "result" { rhs } else { lhs };
            return match result_matches(prev, tag.as_bytes()) {
                Some(m) => match op {
                    "==" => m,
                    "!=" => !m,
                    _ => { ctx.console_writeln("gsh: result compares only with == / !="); false }
                },
                None => { ctx.console_writeln_fmt(format_args!("gsh: '{}' is not a result kind (Ok/Err/FileNotFound/Denied/AssertFailed/Unknown)", tag)); false }
            };
        }
        let mut lb = ExpBuf::new();
        let mut rb = ExpBuf::new();
        if expand_val(ctx, lhs, vars, params, &mut lb).is_err() { return false; }
        if expand_val(ctx, rhs, vars, params, &mut rb).is_err() { return false; }
        return match compare(lb.as_bytes(), rb.as_bytes(), op) {
            Some(x) => x,
            None => { ctx.console_writeln("gsh: bad comparison operator"); false }
        };
    }
    // command condition: expand + run, true iff Ok (result is NOT updated by a condition).
    let mut exp = ExpBuf::new();
    if expand_cmd(ctx, cond, vars, params, &mut exp).is_err() { return false; }
    execute(ctx, exp.as_bytes(), cwd, prev, depth, &mut Out::Console).is_ok()
}

/// Skip ASCII whitespace from `i`.
fn skip_ws(b: &[u8], mut i: usize) -> usize { while i < b.len() && b[i].is_ascii_whitespace() { i += 1; } i }

/// Skip statement separators: whitespace, `;`, and whole-line `#` comments.
fn skip_seps(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b';') { i += 1; }
        if i < b.len() && b[i] == b'#' { while i < b.len() && b[i] != b'\n' { i += 1; } continue; }
        return i;
    }
}

/// True if `b[pos..]` begins with keyword `kw` followed by a word boundary (whitespace, `{`, or end).
fn matches_kw(b: &[u8], pos: usize, kw: &[u8]) -> bool {
    if pos + kw.len() > b.len() || &b[pos..pos + kw.len()] != kw { return false; }
    let after = pos + kw.len();
    after >= b.len() || b[after].is_ascii_whitespace() || b[after] == b'{'
}

/// Find the next UNQUOTED `{` at/after `i` (quote state resets at a newline, §2).
fn find_open_brace(b: &[u8], mut i: usize) -> Option<usize> {
    let mut quote: u8 = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 { if c == b'\n' || c == quote { quote = 0; } i += 1; continue; }
        match c { b'\'' | b'"' => quote = c, b'{' => return Some(i), _ => {} }
        i += 1;
    }
    None
}

/// Given `open` at a `{`, find the position of its matching `}` (quote-aware brace counting).
fn find_matching_brace(b: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    let mut depth = 1usize;
    let mut quote: u8 = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 { if c == b'\n' || c == quote { quote = 0; } i += 1; continue; }
        match c {
            b'\'' | b'"' => quote = c,
            b'{' => depth += 1,
            b'}' => { depth -= 1; if depth == 0 { return Some(i); } }
            _ => {}
        }
        i += 1;
    }
    None
}

/// After a TAKEN if/else-if block's `}` (at `pos`), skip any trailing `else if {...}` / `else {...}`
/// (a taken branch means no further branch runs). Returns the position just past the whole chain.
fn skip_else_chain(b: &[u8], mut pos: usize) -> usize {
    loop {
        let p = skip_ws(b, pos);
        if !matches_kw(b, p, b"else") { return pos; }
        let after_else = skip_ws(b, p + 4);
        let is_elif = matches_kw(b, after_else, b"if");
        let cond_start = if is_elif { after_else + 2 } else { after_else };
        let open = match find_open_brace(b, cond_start) { Some(o) => o, None => return b.len() };
        let end = match find_matching_brace(b, open) { Some(e) => e, None => return b.len() };
        pos = end + 1;
        if !is_elif { return pos; } // a plain `else` terminates the chain
    }
}

/// What a `for` loop iterates: literal/`$var` WORDS in the buffer, an integer RANGE, or the script's
/// PARAMS (`$args`). The advancing state lives in the frame - no materialized list (a big `range` never
/// becomes text).
#[derive(Clone, Copy)]
enum ForIter {
    Words { pos: usize, end: usize }, // byte positions of the remaining word list (after `in`)
    Range { cur: i64, end: i64 },
    Params { idx: usize },
    /// `for line in (producer)` - the producer's output was captured to a temp file `/.fl<id>~`
    /// (`id` = the loop's `{` position, unique + stable); `off` is the read cursor. Each step reads
    /// the next line at `off`. Kept in a FILE, not a buffer, so the (Copy) iterator stays tiny and no
    /// 16 KiB capture lives in the executor frame. The temp is deleted on exhaustion + on `break`.
    FileLines { off: u32, id: u32 },
}

/// A block frame's kind. `If`/`else` closes by skipping its else-chain; a `switch` arm closes by
/// jumping past the whole switch (carrying its end); a function `Call` closes by returning to the
/// caller (carrying the resume position) and dropping the call's scope; `For`/`Loop` close by
/// advancing (re-running the body) or, when exhausted / at the cap, by exiting past the body. Both
/// loops carry `base`/`abase` (the var-table/arena base restored each iteration, so a `let` in the
/// body is fresh each pass) and `body_end` (where `break` jumps to).
#[derive(Clone, Copy)]
enum BlockKind {
    If,
    SwitchArm(usize),
    Call(usize),
    For { var: usize, body: usize, body_end: usize, base: usize, abase: usize, it: ForIter },
    Loop { body: usize, body_end: usize, base: usize, abase: usize, iter: u32 },
    /// `if <function> { … }` (Slice: function-valued conditions). The function was called like any
    /// Call; on return we branch on its result instead of resuming: Ok (XOR `negate`) enters the
    /// if-body at `body` (via an `If` frame, so the body's `}` skips the else-chain), else we take the
    /// else-chain from just past `body_end`. Carries the same scope-drop as a Call on return.
    IfCall { body: usize, body_end: usize, negate: bool },
    /// `let [mut] <name> = $(myfn …)` - capture a function's OUTPUT into a variable. The function was
    /// called like any Call, but its body's output was routed to the capture buffer (via `out`); on
    /// return we bind `name` (byte range in the script) to that buffer instead of resuming a caller,
    /// then continue at `resume` (just past the `let` statement). Same scope-drop as a Call.
    CaptureCall { name_off: usize, name_len: usize, mutable: bool, resume: usize },
}

/// The result of processing an `if` or `switch` construct. `CallThen` = the `if` condition is a
/// function call (`if myfn args { … }`): the executor must RUN it (a control-flow jump, not a value)
/// and branch on its result. `cond_off`/`cond_len` bound the call text (`myfn args`) in the script.
enum Step {
    Enter(usize, BlockKind),
    Done(usize),
    Malformed(usize),
    CallThen { fi: usize, cond_off: usize, cond_len: usize, body: usize, body_end: usize, negate: bool },
}

/// Handle an `if`/`else if`/`else` chain starting just after the `if` keyword (at `pos`). Evaluates
/// each condition in turn: on the first true one, returns `Enter(body, If)` (the executor runs that
/// block, then its `}` skips the rest of the chain); if none is true, takes a trailing `else` if
/// present, else returns `Done(next)`.
fn handle_if(b: &[u8], mut pos: usize, ctx: &ShellCtx, cwd: &mut Cwd, vars: &Vars, params: &Params, prev: Result<(), ShellError>, depth: u8, ft: &FnTable) -> Step {
    loop {
        let open = match find_open_brace(b, pos) { Some(o) => o, None => { ctx.console_writeln("gsh: if: missing '{'"); return Step::Malformed(b.len()); } };
        let end = match find_matching_brace(b, open) { Some(e) => e, None => { ctx.console_writeln("gsh: if: unbalanced braces"); return Step::Malformed(b.len()); } };
        let cond = str_of(trim_bytes(&b[pos..open]));
        // A FUNCTION-valued condition: `[!] fnname [args]` with no comparison / `in` operator. The
        // executor must RUN the function (a Call jump) and branch on its result - `eval_cond` can't
        // (it runs builtins via `execute`, not functions). Detected here, before `eval_cond`, so it
        // works for the leading `if` and for any `else if`. A comparison / `in` is NOT a call.
        {
            let (negate, core) = match cond.strip_prefix('!') { Some(r) => (true, r.trim()), None => (false, cond) };
            let (w0, _) = split_first(core);
            if !w0.is_empty() && !cond_has_operator(core) {
                if let Some(fi) = ft.lookup(b, w0.as_bytes()) {
                    let coff = core.as_ptr() as usize - b.as_ptr() as usize;
                    return Step::CallThen { fi, cond_off: coff, cond_len: core.len(), body: open + 1, body_end: end, negate };
                }
            }
        }
        if eval_cond(ctx, cwd, cond, vars, params, prev, depth) {
            return Step::Enter(open + 1, BlockKind::If);
        }
        // false: skip this block; look for `else` / `else if`.
        pos = end + 1;
        let p = skip_ws(b, pos);
        if !matches_kw(b, p, b"else") { return Step::Done(pos); }
        let after_else = skip_ws(b, p + 4);
        if matches_kw(b, after_else, b"if") { pos = after_else + 2; continue; } // else if -> re-loop
        // plain `else` -> take it (no prior branch was true).
        let eopen = match find_open_brace(b, after_else) { Some(o) => o, None => { ctx.console_writeln("gsh: else: missing '{'"); return Step::Malformed(b.len()); } };
        return Step::Enter(eopen + 1, BlockKind::If);
    }
}

/// True if the switch value matches any pattern word in `patterns`: `_` is the default (matches
/// anything); a `switch result` matches result kinds; otherwise it is expanded-word equality.
fn arm_matches(ctx: &ServiceContext, patterns: &str, is_result: bool, val: &[u8], prev: Result<(), ShellError>, vars: &Vars, params: &Params) -> bool {
    let b = patterns.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() { i += 1; }
        if i >= b.len() { break; }
        let (raw, end) = raw_token(patterns, i);
        i = end;
        if raw == "_" { return true; }
        if is_result {
            if result_matches(prev, raw.as_bytes()) == Some(true) { return true; }
        } else {
            let mut wb = ExpBuf::new();
            if expand_val(ctx, raw, vars, params, &mut wb).is_ok() && wb.as_bytes() == val { return true; }
        }
    }
    false
}

/// Handle `switch <val> { <pat...> { block } ... _ { block } }` starting just after the `switch`
/// keyword (at `pos`). Matches the value against each arm's patterns; the first match's block is
/// entered (its `}` then jumps past the whole switch). No fallthrough; `_` is the default; a
/// `switch result` matches by result kind. No native recursion - arm scanning is brace-seeking.
fn handle_switch(b: &[u8], pos: usize, ctx: &ServiceContext, vars: &Vars, params: &Params, prev: Result<(), ShellError>) -> Step {
    let body_open = match find_open_brace(b, pos) { Some(o) => o, None => { ctx.console_writeln("gsh: switch: missing '{'"); return Step::Malformed(b.len()); } };
    let switch_end = match find_matching_brace(b, body_open) { Some(e) => e, None => { ctx.console_writeln("gsh: switch: unbalanced braces"); return Step::Malformed(b.len()); } };
    let val_src = str_of(trim_bytes(&b[pos..body_open]));
    let is_result = val_src == "result";
    let mut valbuf = ExpBuf::new();
    if !is_result && expand_val(ctx, val_src, vars, params, &mut valbuf).is_err() {
        return Step::Malformed(switch_end + 1);
    }
    let mut ap = body_open + 1;
    while ap < switch_end {
        ap = skip_seps(b, ap);
        if ap >= switch_end { break; }
        let arm_open = match find_open_brace(b, ap) { Some(o) if o < switch_end => o, _ => { ctx.console_writeln("gsh: switch: arm missing '{'"); break; } };
        let patterns = str_of(trim_bytes(&b[ap..arm_open]));
        let arm_end = match find_matching_brace(b, arm_open) { Some(e) => e, None => return Step::Malformed(switch_end + 1) };
        if arm_matches(ctx, patterns, is_result, valbuf.as_bytes(), prev, vars, params) {
            return Step::Enter(arm_open + 1, BlockKind::SwitchArm(switch_end));
        }
        ap = arm_end + 1;
    }
    Step::Done(switch_end + 1) // no arm matched
}

/// Read a simple (non-block) statement starting at `start`: up to an unquoted `;`, newline, `{`,
/// `}`, or an inline `#` comment. Returns `(trimmed statement, resume position)`; a `{`/`}` is NOT
/// consumed (the executor handles braces), `;`/newline/comment ARE stepped past.
fn read_statement(b: &[u8], start: usize) -> (&[u8], usize) {
    let mut i = start;
    let mut quote: u8 = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == b'\n' { return (trim_bytes(&b[start..i]), i + 1); }
            if c == quote { quote = 0; }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => { quote = c; i += 1; }
            b';' | b'\n' => return (trim_bytes(&b[start..i]), i + 1),
            b'{' | b'}' => return (trim_bytes(&b[start..i]), i),
            b'#' if i == start || b[i - 1].is_ascii_whitespace() => {
                let s = trim_bytes(&b[start..i]);
                let mut k = i;
                while k < b.len() && b[k] != b'\n' { k += 1; }
                return (s, if k < b.len() { k + 1 } else { k });
            }
            _ => { i += 1; }
        }
    }
    (trim_bytes(&b[start..i]), i)
}

// ── `fmt` (utilities/39_fmt.md): the .gsh formatter. One canonical layout, applied in place. A
// STREAMING, token-level RE-EMITTER: it reads the script in chunks and formats each (indent by brace
// depth, one statement per line, K&R braces, single inter-token space, `#`-comment spacing, blank-line
// collapse), emitting to a closure - holding only ONE partial statement + the indent depth across
// chunk boundaries. Constant memory, NO file-size limit. It never evaluates, only re-lays-out, so it
// is semantics-preserving + idempotent. fmt and minify are one operation, opposite emit policies. ──

/// Why a `fmt` run stopped. `UnitTooLong` = a single statement/header exceeds the hold buffer; `Write`
/// = the output sink (temp file) rejected a write.
enum FmtErr { Unparseable, UnitTooLong, Write }

const FMT_TAB: usize = 4;       // spaces per block depth
const FMT_HOLD: usize = 4096;   // max size of ONE statement/header/comment carried across a chunk
const FMT_RCHUNK: usize = 4096; // source read chunk

/// Emit a run of bytes to the sink; `Write` if the sink rejected it.
fn femit(emit: &mut dyn FnMut(&[u8]) -> bool, b: &[u8]) -> Result<(), FmtErr> {
    if emit(b) { Ok(()) } else { Err(FmtErr::Write) }
}

/// Emit `depth` levels of indentation (capped so a pathological nesting can't run away).
fn femit_indent(emit: &mut dyn FnMut(&[u8]) -> bool, depth: usize) -> Result<(), FmtErr> {
    const SPACES: [u8; 64] = [b' '; 64];
    femit(emit, &SPACES[..(depth * FMT_TAB).min(64)])
}

/// Emit one already-trimmed statement/header, collapsing runs of whitespace OUTSIDE quotes to a
/// single space (gsh tokenizes on whitespace) while copying quoted content verbatim. Same discipline
/// as the minifier's collapse - the shared whitespace policy of the two tools.
fn femit_stmt(emit: &mut dyn FnMut(&[u8]) -> bool, s: &[u8]) -> Result<(), FmtErr> {
    let mut i = 0usize;
    let mut prev_ws = false;
    while i < s.len() {
        let c = s[i];
        if c == b'\'' || c == b'"' {
            let start = i; i += 1;
            while i < s.len() && s[i] != c { i += 1; }
            if i < s.len() { i += 1; } // include the closing quote
            femit(emit, &s[start..i])?;
            prev_ws = false;
        } else if c.is_ascii_whitespace() {
            while i < s.len() && s[i].is_ascii_whitespace() { i += 1; }
            if !prev_ws { femit(emit, b" ")?; prev_ws = true; }
        } else {
            let start = i;
            while i < s.len() && !s[i].is_ascii_whitespace() && s[i] != b'\'' && s[i] != b'"' { i += 1; }
            femit(emit, &s[start..i])?;
            prev_ws = false;
        }
    }
    Ok(())
}

/// Skip inter-statement layout (spaces, tabs, `\r`, `;`, newlines). Returns the resume position and
/// whether a blank line (2+ newlines) was crossed - so one blank line can be preserved as a paragraph
/// break while runs collapse to a single one.
fn fmt_skip_layout(b: &[u8], pos: usize) -> (usize, bool) {
    let mut i = pos;
    let mut nl = 0usize;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b';' => i += 1,
            b'\n' => { nl += 1; i += 1; }
            _ => break,
        }
    }
    (i, nl >= 2)
}

/// Find the next `{` from `pos` (quote-aware); `None` if none appears before the window end. Unlike
/// `find_open_brace` it does NOT stop at a `}` - an `else if <cond>` header contains none.
fn fmt_find_brace_win(b: &[u8], mut pos: usize) -> Option<usize> {
    let mut q: u8 = 0;
    while pos < b.len() {
        let c = b[pos];
        if q != 0 { if c == q { q = 0; } }
        else if c == b'\'' || c == b'"' { q = c; }
        else if c == b'{' { return Some(pos); }
        pos += 1;
    }
    None
}

/// How a window statement scan ended.
#[derive(Clone, Copy)]
enum ScanEnd { Brace(usize), Term(usize), End } // Brace(`{` pos) / Term(resume) / End (window ran out)

/// Scan one statement from `start` within a window, preserving a trailing `#` comment. `End` means the
/// window ran out before a terminator (`;`/newline/`{`/`}`/complete comment) - a partial unit to hold.
/// A `{`/`}` is not consumed (the caller handles them).
fn fmt_scan_window(b: &[u8], start: usize) -> (&[u8], &[u8], ScanEnd) {
    let mut i = start;
    let mut quote: u8 = 0;
    while i < b.len() {
        let c = b[i];
        if quote != 0 {
            if c == b'\n' { return (trim_bytes(&b[start..i]), &b[start..start], ScanEnd::Term(i + 1)); }
            if c == quote { quote = 0; }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' => { quote = c; i += 1; }
            b';' | b'\n' => return (trim_bytes(&b[start..i]), &b[start..start], ScanEnd::Term(i + 1)),
            b'{' => return (trim_bytes(&b[start..i]), &b[start..start], ScanEnd::Brace(i)),
            b'}' => return (trim_bytes(&b[start..i]), &b[start..start], ScanEnd::Term(i)),
            b'#' if i == start || b[i - 1].is_ascii_whitespace() => {
                let stmt = trim_bytes(&b[start..i]);
                let cs = i + 1;
                let mut k = cs;
                while k < b.len() && b[k] != b'\n' { k += 1; }
                if k >= b.len() { return (stmt, trim_bytes(&b[cs..k]), ScanEnd::End); } // comment ran off window
                return (stmt, trim_bytes(&b[cs..k]), ScanEnd::Term(k + 1));
            }
            _ => i += 1,
        }
    }
    (trim_bytes(&b[start..i]), &b[start..start], ScanEnd::End) // no terminator in window -> partial
}

/// Format the window `win`, emitting formatted runs via `emit`. Tracks `depth`/`first` across calls.
/// Returns bytes safely CONSUMED; when `!eof` and a unit needs data past the window (partial statement,
/// a `}` whose `else`/`{` look-ahead isn't present, a partial comment) it stops BEFORE that unit so the
/// caller can hold it. Errors: `Unparseable` (stray `}`), `Write` (sink failed).
fn fmt_walk_window(win: &[u8], eof: bool, depth: &mut usize, first: &mut bool,
                   emit: &mut dyn FnMut(&[u8]) -> bool) -> Result<usize, FmtErr> {
    let mut pos = 0usize;
    loop {
        let entry = pos; // loop entry (before layout) - the point to hold from
        let (np, blank) = fmt_skip_layout(win, pos);
        if np >= win.len() { return Ok(np); } // consumed trailing layout (not emitted)
        let c = win[np];
        if c == b'}' {
            let after = np + 1;
            let p = skip_ws(win, after);
            // Decide `} else {` vs a plain close only when the window shows enough PAST the `}` to
            // confirm the `else` keyword (4 chars + a boundary char = 5). If it ends inside/right after
            // a possible `else`, HOLD - otherwise a `} else {` split across a read is mis-emitted as a
            // plain `}` plus a new `else {` block (the chunk-boundary idempotency bug).
            if p + 5 > win.len() && !eof { return Ok(entry); }
            if matches_kw(win, p, b"else") {
                match fmt_find_brace_win(win, p + 4) {
                    Some(ob) => {
                        if *depth == 0 { return Err(FmtErr::Unparseable); }
                        *depth -= 1;
                        femit_indent(emit, *depth)?; femit(emit, b"}")?; femit(emit, b" else")?;
                        let hdr = trim_bytes(&win[p + 4..ob]); // "" plain else / "if <cond>"
                        if !hdr.is_empty() { femit(emit, b" ")?; femit_stmt(emit, hdr)?; }
                        femit(emit, b" {\n")?;
                        *depth += 1; *first = false;
                        pos = ob + 1;
                    }
                    None => {
                        if !eof { return Ok(entry); } // else-header `{` not here yet -> hold
                        if *depth == 0 { return Err(FmtErr::Unparseable); }
                        *depth -= 1; femit_indent(emit, *depth)?; femit(emit, b"}\n")?; *first = false;
                        pos = after;
                    }
                }
            } else {
                if *depth == 0 { return Err(FmtErr::Unparseable); }
                *depth -= 1; femit_indent(emit, *depth)?; femit(emit, b"}\n")?; *first = false;
                pos = after;
            }
            continue;
        }
        let (stmt, comment, end) = fmt_scan_window(win, np);
        if let ScanEnd::End = end { if !eof { return Ok(entry); } } // partial unit -> hold
        if blank && !*first { femit(emit, b"\n")?; } // preserved paragraph break (never before a `}`)
        *first = false;
        match end {
            ScanEnd::Brace(bp) => {
                femit_indent(emit, *depth)?;
                if !stmt.is_empty() { femit_stmt(emit, stmt)?; femit(emit, b" ")?; }
                femit(emit, b"{\n")?;
                *depth += 1;
                pos = bp + 1;
            }
            _ => { // Term, or End at eof: a simple statement (+ optional trailing comment)
                if !stmt.is_empty() || !comment.is_empty() {
                    femit_indent(emit, *depth)?;
                    if !stmt.is_empty() { femit_stmt(emit, stmt)?; }
                    if !comment.is_empty() {
                        if !stmt.is_empty() { femit(emit, b" ")?; }
                        femit(emit, b"# ")?; femit(emit, comment)?;
                    }
                    femit(emit, b"\n")?;
                }
                pos = match end { ScanEnd::Term(r) => r, _ => win.len() };
            }
        }
    }
}

/// Stream `path` through the formatter, calling `emit` for each formatted run - constant memory (reads
/// in chunks, holds only ONE partial statement). `UnitTooLong` if a single statement exceeds the hold;
/// `Unparseable` if a block is left unclosed. No file-size limit.
fn fmt_stream_pass(ctx: &ShellCtx, path: &[u8], emit: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), FmtErr> {
    let mut work = [0u8; FMT_HOLD + FMT_RCHUNK];
    let mut hold = 0usize;
    let mut depth = 0usize;
    let mut first = true;
    let mut src_off = 0u64;
    loop {
        let got = fs_read_at(ctx, path, src_off, &mut work[hold..hold + FMT_RCHUNK]).unwrap_or(0);
        src_off += got as u64;
        let avail = hold + got;
        let eof = got == 0;
        let consumed = fmt_walk_window(&work[..avail], eof, &mut depth, &mut first, emit)?;
        if eof { break; }
        let tail = avail - consumed;
        if tail >= FMT_HOLD { return Err(FmtErr::UnitTooLong); } // one statement bigger than the hold
        work.copy_within(consumed..avail, 0);
        hold = tail;
    }
    if depth != 0 { return Err(FmtErr::Unparseable); } // unclosed block
    Ok(())
}

// ── Loops (§5): `for <var> in <words|range|$args> { … }` and unbounded `loop { … }`. ──

/// Parse the source of a `for` (the text between `in` and `{`) into an iterator: `range N` / `range A
/// B` counts; `$args` alone walks the params; anything else is a whitespace-separated word list (each
/// word `$`-expanded per step).
fn parse_for_iter(b: &[u8], rest_start: usize, rest_end: usize) -> ForIter {
    let s = skip_ws(b, rest_start);
    if matches_kw(b, s, b"range") {
        let mut nums = [0i64; 2];
        let mut nn = 0usize;
        let mut i = s + 5;
        while i < rest_end && nn < 2 {
            while i < rest_end && b[i].is_ascii_whitespace() { i += 1; }
            if i >= rest_end { break; }
            let ts = i;
            while i < rest_end && !b[i].is_ascii_whitespace() { i += 1; }
            match parse_i64(&b[ts..i]) { Some(v) => { nums[nn] = v; nn += 1; } None => break }
        }
        match nn {
            1 => ForIter::Range { cur: 0, end: nums[0] },
            2 => ForIter::Range { cur: nums[0], end: nums[1] },
            _ => ForIter::Range { cur: 0, end: 0 }, // malformed -> empty
        }
    } else if trim_bytes(&b[rest_start..rest_end]) == b"$args" {
        ForIter::Params { idx: 0 }
    } else {
        ForIter::Words { pos: rest_start, end: rest_end }
    }
}

/// Advance a `for` iterator by one: if a next item exists, set the loop var (`var`) to it and return
/// the advanced iterator; else `None` (loop done). Words are `$`-expanded in the current scope.
fn for_step(ctx: &ShellCtx, b: &[u8], vars: &mut Vars, var: usize, it: ForIter, params: &Params) -> Option<ForIter> {
    match it {
        ForIter::Range { cur, end } => {
            if cur >= end { return None; }
            let mut eb = ExpBuf::new();
            eb.push_i64(cur);
            vars.set_slot(var, eb.as_bytes()).ok()?;
            Some(ForIter::Range { cur: cur + 1, end })
        }
        ForIter::Params { idx } => {
            if idx >= params.argc { return None; }
            let a = params.argv[idx];
            vars.set_slot(var, a.as_bytes()).ok()?;
            Some(ForIter::Params { idx: idx + 1 })
        }
        ForIter::Words { pos, end } => {
            let mut i = pos;
            while i < end && b[i].is_ascii_whitespace() { i += 1; }
            if i >= end { return None; }
            let s = i;
            while i < end && !b[i].is_ascii_whitespace() { i += 1; }
            let mut eb = ExpBuf::new();
            if expand_val(ctx, str_of(&b[s..i]), vars, params, &mut eb).is_err() { return None; }
            vars.set_slot(var, eb.as_bytes()).ok()?;
            Some(ForIter::Words { pos: i, end })
        }
        ForIter::FileLines { off, id } => forlines_step(ctx, vars, var, off, id),
    }
}

/// The temp-file path for a `for line in (producer)` loop: `/.fl<id>~` (id = the loop's `{` position,
/// unique + stable). Written into `buf`; returns the used slice.
fn forlines_temp(id: u32, buf: &mut [u8; 24]) -> &[u8] {
    buf[..4].copy_from_slice(b"/.fl");
    let mut n = 4usize;
    let mut d = [0u8; 10];
    let mut di = 0usize;
    let mut v = id;
    if v == 0 { d[0] = b'0'; di = 1; } else { while v > 0 { d[di] = b'0' + (v % 10) as u8; di += 1; v /= 10; } }
    while di > 0 { di -= 1; buf[n] = d[di]; n += 1; }
    buf[n] = b'~'; n += 1;
    &buf[..n]
}

/// One step of a `for line in (producer)` loop: read the next line of the temp file at `off`, set the
/// loop var, and advance. `#[inline(never)]` so its `IO_CHUNK` read buffer stays off the common
/// `for_step` frame (Range/Words don't pay for it). On EOF (or a set-var error) the temp is deleted
/// and the loop ends. A line is bytes up to `\n`; a final line without a trailing `\n` still counts;
/// a trailing `\n` does not yield an extra empty line.
#[inline(never)]
fn forlines_step(ctx: &ShellCtx, vars: &mut Vars, var: usize, off: u32, id: u32) -> Option<ForIter> {
    let mut tb = [0u8; 24];
    let temp = forlines_temp(id, &mut tb);
    let mut rbuf = [0u8; IO_CHUNK];
    let n = fs_read_at(ctx, temp, off as u64, &mut rbuf).unwrap_or(0);
    if n == 0 { let _ = fs_request(ctx, OP_DELETE, temp, &[]); return None; } // exhausted -> clean up
    let mut k = 0usize;
    while k < n && rbuf[k] != b'\n' { k += 1; }
    let (line_end, next_off) = if k < n { (k, off + k as u32 + 1) } else { (n, off + n as u32) };
    if vars.set_slot(var, &rbuf[..line_end]).is_err() {
        let _ = fs_request(ctx, OP_DELETE, temp, &[]);
        return None;
    }
    Some(ForIter::FileLines { off: next_off, id })
}

/// Capture `inner` (a producer) to the `for line in (…)` temp file. `#[inline(never)]` so the 16 KiB
/// `ReportBuf` lives ONLY here, not in the executor frame. Delete-first is idempotent (clears a temp
/// leaked by an errored prior run). Empty output -> no file (an empty loop). Loud + `Err` on a refused
/// producer (run_captured said why), an over-16-KiB output, or a write failure.
#[inline(never)]
fn forlines_capture(ctx: &ShellCtx, cwd: &Cwd, inner: &str, temp: &[u8]) -> Result<(), ()> {
    let _ = fs_request(ctx, OP_DELETE, temp, &[]);
    let mut rb = ReportBuf::new();
    let ok = { let mut o = Out::File(&mut rb); run_captured(ctx, cwd, inner, &mut o) };
    if !ok { return Err(()); }
    if rb.overflow { ctx.console_writeln("gsh: for line: producer output too large (16 KiB cap)"); return Err(()); }
    let data = rb.bytes();
    if data.is_empty() { return Ok(()); } // no file -> forlines_step's first read returns None -> empty loop
    if !fs_write_new(ctx, temp, data.len() as u64) { ctx.console_writeln("gsh: for line: capture write failed"); return Err(()); }
    let mut w = 0usize;
    while w < data.len() {
        let m = (data.len() - w).min(IO_CHUNK); // IO_CHUNK is 508-aligned, so each offset is block-aligned
        if !fs_write_at(ctx, temp, w as u64, &data[w..w + m]) {
            let _ = fs_request(ctx, OP_DELETE, temp, &[]);
            ctx.console_writeln("gsh: for line: capture write failed");
            return Err(());
        }
        w += m;
    }
    Ok(())
}

/// Run + remove every `defer`red command whose scope depth >= `min_depth`, LIFO (§5). Called on a
/// function's return (`min_depth` = that function's scope) and at script end / `fail` (`min_depth` =
/// 0 = all). A deferred command runs like any statement; its result does NOT affect the script's
/// control flow - defers are cleanup, run even on `fail`.
fn run_defers(ctx: &ShellCtx, cwd: &mut Cwd, b: &[u8], defers: &mut [(usize, usize, usize)], ndefer: &mut usize, min_depth: usize, vars: &mut Vars, params: &Params, out: &mut Out, sdepth: u8) {
    loop {
        let mut idx = None;
        let mut i = *ndefer;
        while i > 0 { i -= 1; if defers[i].2 >= min_depth { idx = Some(i); break; } }
        let i = match idx { Some(i) => i, None => break };
        let (off, len, _) = defers[i];
        for k in i..*ndefer - 1 { defers[k] = defers[k + 1]; }
        *ndefer -= 1;
        let s = str_of(&b[off..off + len]);
        out.put(ctx, "defer> ");
        out.line(ctx, s);
        let _ = run_stmt(ctx, cwd, s, Ok(()), sdepth, vars, params, &mut Out::Console);
    }
}

// ── Functions (§7): `fn name params { body }`, called like a command, bounded recursion. ──

const FN_MAX: usize = 24;

/// Index of the `fn` definitions in a script (built by a one-pass pre-scan, so a call may precede its
/// definition, §7). Stores only OFFSETS into the resident script buffer - tiny, no name copies.
struct FnTable {
    name_off: [u16; FN_MAX],
    name_len: [u8; FN_MAX],
    params_off: [u16; FN_MAX], // param-list span (after the name, up to the `{`)
    params_end: [u16; FN_MAX],
    body_start: [u16; FN_MAX], // just after the `{`
    body_end: [u16; FN_MAX],   // at the matching `}`
    count: usize,
}
impl FnTable {
    fn new() -> Self {
        FnTable { name_off: [0; FN_MAX], name_len: [0; FN_MAX], params_off: [0; FN_MAX],
                  params_end: [0; FN_MAX], body_start: [0; FN_MAX], body_end: [0; FN_MAX], count: 0 }
    }
    fn lookup(&self, b: &[u8], name: &[u8]) -> Option<usize> {
        (0..self.count).find(|&i| &b[self.name_off[i] as usize..self.name_off[i] as usize + self.name_len[i] as usize] == name)
    }
}

/// One pass over the buffer, recording every top-level `fn name params { … }`. Skips over the bodies
/// of `fn`/`if`/`switch` blocks so a `fn` nested in a block is not indexed (functions are top-level).
fn prescan_fns(ctx: &ServiceContext, b: &[u8]) -> FnTable {
    let mut t = FnTable::new();
    let mut pos = 0usize;
    while pos < b.len() {
        pos = skip_seps(b, pos);
        if pos >= b.len() { break; }
        if b[pos] == b'}' { pos += 1; continue; }
        if matches_kw(b, pos, b"fn") {
            let ns = skip_ws(b, pos + 2);
            let mut ne = ns;
            while ne < b.len() && !b[ne].is_ascii_whitespace() && b[ne] != b'{' { ne += 1; }
            let open = match find_open_brace(b, ne) { Some(o) => o, None => { ctx.console_writeln("gsh: fn: missing '{'"); break; } };
            let end = match find_matching_brace(b, open) { Some(e) => e, None => { ctx.console_writeln("gsh: fn: unbalanced braces"); break; } };
            if ne > ns && t.count < FN_MAX {
                if t.lookup(b, &b[ns..ne]).is_some() {
                    ctx.console_writeln_fmt(format_args!("gsh: function '{}' already defined (import it 'as' another name)", str_of(&b[ns..ne])));
                } else {
                    let i = t.count;
                    t.name_off[i] = ns as u16; t.name_len[i] = (ne - ns) as u8;
                    t.params_off[i] = ne as u16; t.params_end[i] = open as u16;
                    t.body_start[i] = (open + 1) as u16; t.body_end[i] = end as u16;
                    t.count += 1;
                }
            } else if t.count >= FN_MAX {
                ctx.console_writeln_fmt(format_args!("gsh: too many functions (max {})", FN_MAX));
            }
            pos = end + 1;
            continue;
        }
        // Not a fn - step over this statement, and if it opens a block, over the whole block.
        let (_, next) = read_statement(b, pos);
        if next < b.len() && b[next] == b'{' {
            pos = find_matching_brace(b, next).map(|e| e + 1).unwrap_or(b.len());
        } else {
            pos = next;
        }
    }
    t
}

/// Bind a function call's args to its params in a FRESH scope: expand each arg in the CALLER's scope,
/// then `enter_scope` and define the params as immutable locals. Loud + `false` on a bad arg, too few
/// args, or call-depth overflow (recursion bound). `#[inline(never)]` - the arg buffer stays off the
/// executor's hot loop frame.
#[inline(never)]
fn dispatch_call(ctx: &ServiceContext, b: &[u8], stmt: &str, ft: &FnTable, fi: usize, vars: &mut Vars, params: &Params) -> bool {
    // Expand the call's args (everything after the fn name), in the caller's scope, into argbuf.
    let mut argbuf = [0u8; 512];
    let mut aoff = [0u16; PARAM_MAX];
    let mut alen = [0u16; PARAM_MAX];
    let mut nargs = 0usize;
    let (_name, rest) = split_first(stmt);
    let rb = rest.as_bytes();
    let mut i = 0usize;
    let mut w = 0usize;
    while i < rb.len() && nargs < PARAM_MAX {
        while i < rb.len() && rb[i].is_ascii_whitespace() { i += 1; }
        if i >= rb.len() { break; }
        let (raw, end) = raw_token(rest, i);
        i = end;
        let mut eb = ExpBuf::new();
        if expand_val(ctx, raw, vars, params, &mut eb).is_err() { return false; }
        let bytes = eb.as_bytes();
        if w + bytes.len() > argbuf.len() { ctx.console_writeln("gsh: call args too long"); return false; }
        aoff[nargs] = w as u16;
        argbuf[w..w + bytes.len()].copy_from_slice(bytes);
        w += bytes.len();
        alen[nargs] = bytes.len() as u16;
        nargs += 1;
    }
    // Open the function scope, then bind params positionally.
    if vars.enter_scope().is_err() { ctx.console_writeln("gsh: call depth too deep (unbounded recursion?)"); return false; }
    let (ps, pe) = (ft.params_off[fi] as usize, ft.params_end[fi] as usize);
    let mut pi = 0usize;
    let mut j = ps;
    while j < pe {
        while j < pe && b[j].is_ascii_whitespace() { j += 1; }
        if j >= pe { break; }
        let s = j;
        while j < pe && !b[j].is_ascii_whitespace() { j += 1; }
        let pname = &b[s..j];
        if pi >= nargs {
            ctx.console_writeln_fmt(format_args!("gsh: missing argument for parameter '{}'", str_of(pname)));
            vars.exit_scope();
            return false;
        }
        let av = &argbuf[aoff[pi] as usize..aoff[pi] as usize + alen[pi] as usize];
        if let Err(e) = vars.define(pname, av, false) {
            var_err_msg(ctx, str_of(pname), e);
            vars.exit_scope();
            return false;
        }
        pi += 1;
    }
    true
}

/// Execute a script body (already in memory): split into commands, run each, then print a
/// per-command PASS/FAIL summary and the `run: ran N, failed M` tally. Shared by `run` (file
/// source) and `selfcheck` (the embedded suite, run straight from rodata - NOT written to disk,
/// so it is **not** bound by `MAX_FILE_BYTES`/the single-message file transfer, only by the
/// embedded const). `#[inline(never)]`: holds the verdict array and drives `execute` in a loop
/// (the user stack is tight - see the pipe stack-overflow lesson).
/// The report (the `> <cmd>` echoes, the summary, the tally) goes to `out` - `Out::Console` for a
/// normal run, or `Out::File(&mut ReportBuf)` for `selfcheck/run … save <path>`, where the utility
/// writes its OWN file. Each sub-command's own output still goes to the console (it is produced
/// inside `execute`). The `save` path is a DIRECT file write, NOT a pipe: `run`/`selfcheck` stay
/// non-producers (capturing one through a pipe nests a 64 KiB `Stream` and overflows the stack,
/// HW-proven - [[project-shell-stack-pipe]]). The `ReportBuf` is a modest bounded buffer, so it +
/// a sub-pipeline's transient buffers fit the user stack - the whole point of saving directly.
#[inline(never)]
/// Parse `let [mut] <name> = $( inner )` for the `$(fn)` capture fast path: returns (name, mutable,
/// inner) if the statement is a `let` whose WHOLE value is a `$( )` capture, else None (the ordinary
/// let / producer-capture path handles it). `name` must be a single bare word.
fn let_capture_form(s: &str) -> Option<(&str, bool, &str)> {
    let rest = s.strip_prefix("let")?;
    if !rest.starts_with(char::is_whitespace) { return None; }
    let rest = rest.trim_start();
    let (mutable, rest) = match rest.strip_prefix("mut") {
        Some(r) if r.starts_with(char::is_whitespace) => (true, r.trim_start()),
        _ => (false, rest),
    };
    let eq = rest.find('=')?;
    let name = rest[..eq].trim();
    if name.is_empty() || name.contains(char::is_whitespace) { return None; }
    let inner = capture_form(rest[eq + 1..].trim())?;
    Some((name, mutable, inner))
}

/// `quiet` suppresses the per-statement `> stmt` transcript and the end-of-run summary block - for
/// a LIBRARY command (`health`), whose user asked for a dashboard, not a test report. Errors still
/// print (each failing statement reports itself) and the Result still carries failure (§26.7 loud).
/// `run`/`selfcheck` pass `false`: an orchestrated script run IS a report.
fn run_lines(ctx: &ShellCtx, cwd: &mut Cwd, src: &[u8], depth: u8, out: &mut Out, params: &Params, quiet: bool) -> Result<(), ShellError> {
    // Per-run interpreter state: a bounded variable table, allocated once HERE (above `execute`) and
    // threaded by &mut into `run_stmt` - it never reaches `execute`/`pipe_run`'s frame. No heap (§26.6).
    let mut vars = Vars::new();
    let mut ran = 0u32;
    let mut failed = 0u32;
    let mut last: Result<(), ShellError> = Ok(());
    // Per-statement verdicts + spans for the end-of-run summary. With control flow, the executed
    // statements are no longer a simple prefix of the source, so record each one's (offset, len) as it
    // runs. Bounded; statements past the cap still run and count in the totals, they just get no
    // summary line (loud, not silent - §26.6).
    let mut verdict = [true; RUN_MAX_CMDS];
    let mut soff = [0u16; RUN_MAX_CMDS];
    let mut slng = [0u16; RUN_MAX_CMDS];
    let mut nrec = 0usize;
    let b = src;
    let ft = prescan_fns(ctx, b); // index `fn` definitions so a call may precede its definition (§7)
    let sdepth = depth + 1; // statements/conditions run one level deeper (a nested `run` is refused)
    // Explicit position-based executor (no native recursion, §9): a flat cursor over the resident
    // buffer plus a `{`/`}` depth counter. `if` seeks over untaken blocks; a taken block's `}` skips
    // the rest of its else-chain. Nesting is handled by brace-scanning, not by the native stack.
    let mut pos = 0usize;
    // Explicit block-frame stack (no native recursion, §9): each open `if`/`else` block or
    // `switch`-arm block is a frame. On its `}` an if-block skips its else-chain; a switch-arm block
    // jumps past the whole switch. Nesting is handled by this stack + brace-scanning, not the native
    // stack.
    let mut frames = [BlockKind::If; IF_DEPTH_MAX];
    let mut sp = 0usize;
    // `defer`red commands: (buffer offset, len, scope depth). Run LIFO on scope exit (§5).
    let mut defers: [(usize, usize, usize); DEFER_MAX] = [(0, 0, 0); DEFER_MAX];
    let mut ndefer = 0usize;
    // `$(fn)` capture: while a CaptureCall frame is active, `capturing` is true and each statement's
    // command output is routed to `fncap` (a bounded 4 KiB buffer) instead of the console; on the
    // function's return the buffer becomes the `let` variable's value. One buffer -> one capture at a
    // time (a nested `$(fn)` is refused loudly).
    let mut fncap = FnCapBuf::new();
    let mut capturing = false;
    // Apply a Step from handle_if/handle_switch to the executor state. A macro (not a fn) so it mutates
    // the frame stack / cursor in place. `CallThen` is the function-valued condition (`if myfn { … }`):
    // RUN the function under an `IfCall` frame; the branch happens when that frame's `}` is reached.
    macro_rules! process_step {
        ($st:expr) => {
            match $st {
                Step::Enter(body_, kind_) => {
                    if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; pos = b.len(); }
                    else { frames[sp] = kind_; sp += 1; pos = body_; }
                }
                Step::Done(next_) => { pos = next_; }
                Step::Malformed(next_) => { last = Err(ShellError::Unknown); failed += 1; pos = next_; }
                Step::CallThen { fi: fi_, cond_off: co_, cond_len: cl_, body: bd_, body_end: be_, negate: ng_ } => {
                    if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; pos = b.len(); }
                    else {
                        let stmt_ = str_of(&b[co_..co_ + cl_]);
                        if dispatch_call(ctx, b, stmt_, &ft, fi_, &mut vars, params) {
                            frames[sp] = BlockKind::IfCall { body: bd_, body_end: be_, negate: ng_ };
                            sp += 1;
                            pos = ft.body_start[fi_] as usize;
                        } else {
                            last = Err(ShellError::Unknown); failed += 1; pos = be_ + 1;
                        }
                    }
                }
            }
        };
    }
    loop {
        pos = skip_seps(b, pos);
        if pos >= b.len() { break; }
        // `}` closes the current block.
        if b[pos] == b'}' {
            if sp == 0 { ctx.console_writeln("gsh: unexpected '}'"); failed += 1; break; }
            match frames[sp - 1] {
                BlockKind::If => { sp -= 1; pos = skip_else_chain(b, pos + 1); }
                BlockKind::SwitchArm(end) => { sp -= 1; pos = end + 1; }
                BlockKind::Call(ret) => { // function body done: run its defers, drop scope, resume
                    sp -= 1;
                    run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                    vars.exit_scope();
                    pos = ret;
                }
                BlockKind::For { var, body, body_end, base, abase, it } => {
                    vars.reset_to(base, abase); // drop this pass's body locals
                    match for_step(ctx, b, &mut vars, var, it, params) {
                        Some(next_it) => { frames[sp - 1] = BlockKind::For { var, body, body_end, base, abase, it: next_it }; pos = body; }
                        None => { sp -= 1; pos = body_end + 1; }
                    }
                }
                BlockKind::Loop { body, body_end, base, abase, iter } => {
                    vars.reset_to(base, abase);
                    if iter + 1 >= LOOP_CAP {
                        ctx.console_writeln_fmt(format_args!("gsh: loop hit the {} iteration cap - stopping (needs a break)", LOOP_CAP));
                        sp -= 1; pos = body_end + 1;
                    } else {
                        frames[sp - 1] = BlockKind::Loop { body, body_end, base, abase, iter: iter + 1 };
                        pos = body;
                    }
                }
                BlockKind::IfCall { body, body_end, negate } => {
                    // The function that was the `if` condition just returned. Drop its scope + defers
                    // like a Call, then BRANCH on its result instead of resuming.
                    sp -= 1;
                    run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                    vars.exit_scope();
                    if last.is_ok() ^ negate {
                        // true -> enter the if-body via an If frame (its `}` skips the else-chain, as usual).
                        if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; pos = b.len(); }
                        else { frames[sp] = BlockKind::If; sp += 1; pos = body; }
                    } else {
                        // false -> take the else-chain just past the if-body, if any (mirrors handle_if).
                        let p = skip_ws(b, body_end + 1);
                        if matches_kw(b, p, b"else") {
                            let ae = skip_ws(b, p + 4);
                            if matches_kw(b, ae, b"if") {
                                process_step!(handle_if(b, ae + 2, ctx, cwd, &vars, params, last, sdepth, &ft));
                            } else {
                                match find_open_brace(b, ae) {
                                    Some(eo) => { if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; pos = b.len(); } else { frames[sp] = BlockKind::If; sp += 1; pos = eo + 1; } }
                                    None => { ctx.console_writeln("gsh: else: missing '{'"); failed += 1; pos = b.len(); }
                                }
                            }
                        } else {
                            pos = body_end + 1;
                        }
                    }
                }
                BlockKind::CaptureCall { name_off, name_len, mutable, resume } => {
                    // The captured function returned: drop its scope + defers like a Call, stop
                    // capturing, and bind its OUTPUT (now in fncap) to the `let` variable.
                    sp -= 1;
                    run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                    vars.exit_scope();
                    capturing = false;
                    if fncap.overflow {
                        ctx.console_writeln("gsh: $(fn) output too large to capture (4 KiB)");
                        last = Err(ShellError::Unknown); failed += 1;
                    } else {
                        let name = str_of(&b[name_off..name_off + name_len]);
                        let r = vars.define(name.as_bytes(), trim_bytes(fncap.bytes()), mutable);
                        match r {
                            Ok(()) => last = Ok(()),
                            Err(e) => { var_err_msg(ctx, name, e); last = Err(ShellError::Unknown); failed += 1; }
                        }
                    }
                    fncap.reset();
                    pos = resume;
                }
            }
            continue;
        }
        // a stray `{` outside an `if`/`else`/`switch` is malformed (a literal `{` must be quoted).
        if b[pos] == b'{' {
            ctx.console_writeln("gsh: unexpected '{'");
            pos = find_matching_brace(b, pos).map(|e| e + 1).unwrap_or(b.len());
            last = Err(ShellError::Unknown); failed += 1;
            continue;
        }
        // an `if` or `switch` construct.
        if matches_kw(b, pos, b"if") || matches_kw(b, pos, b"switch") {
            let step = if matches_kw(b, pos, b"if") {
                handle_if(b, pos + 2, ctx, cwd, &vars, params, last, sdepth, &ft)
            } else {
                handle_switch(b, pos + 6, ctx, &vars, params, last)
            };
            process_step!(step);
            continue;
        }
        // a `for` loop: for <var> in <words | range N | range A B | $args> { body }
        if matches_kw(b, pos, b"for") {
            let vs = skip_ws(b, pos + 3);
            let mut ve = vs;
            while ve < b.len() && !b[ve].is_ascii_whitespace() { ve += 1; }
            let in_pos = skip_ws(b, ve);
            if ve <= vs || !matches_kw(b, in_pos, b"in") {
                ctx.console_writeln("gsh: for: expected 'for <var> in <list> { … }'");
                failed += 1;
                pos = find_open_brace(b, pos + 3).and_then(|o| find_matching_brace(b, o)).map(|e| e + 1).unwrap_or(b.len());
                continue;
            }
            let rest_start = skip_ws(b, in_pos + 2);
            let open = match find_open_brace(b, rest_start) { Some(o) => o, None => { ctx.console_writeln("gsh: for: missing '{'"); failed += 1; pos = b.len(); continue; } };
            let end = match find_matching_brace(b, open) { Some(e) => e, None => { ctx.console_writeln("gsh: for: unbalanced braces"); failed += 1; pos = b.len(); continue; } };
            let var = match vars.set_loop_var(&b[vs..ve], b"") {
                Ok(i) => i,
                Err(e) => { var_err_msg(ctx, str_of(&b[vs..ve]), e); failed += 1; pos = end + 1; continue; }
            };
            let base = vars.count;
            let abase = vars.alen;
            // `for line in (producer) { … }` - capture the producer's output to a temp file, iterate
            // its lines (docs/scripting.md). A parenthesized iter is the producer form; anything else
            // is the existing range / $args / word-list.
            let rest = trim_bytes(&b[rest_start..open]);
            let it0 = if rest.len() >= 2 && rest[0] == b'(' && rest[rest.len() - 1] == b')' {
                let inner = trim_bytes(&rest[1..rest.len() - 1]);
                let mut tb = [0u8; 24];
                let temp = forlines_temp(open as u32, &mut tb);
                match forlines_capture(ctx, cwd, str_of(inner), temp) {
                    Ok(()) => ForIter::FileLines { off: 0, id: open as u32 },
                    Err(()) => { failed += 1; pos = end + 1; continue; } // loud already; skip the loop
                }
            } else {
                parse_for_iter(b, rest_start, open)
            };
            match for_step(ctx, b, &mut vars, var, it0, params) {
                Some(next_it) => {
                    if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; break; }
                    frames[sp] = BlockKind::For { var, body: open + 1, body_end: end, base, abase, it: next_it };
                    sp += 1;
                    pos = open + 1;
                }
                None => { pos = end + 1; } // empty iteration: skip the body entirely
            }
            continue;
        }
        // an unbounded `loop { body }` - repeats until `break` (LOOP_CAP is the loud backstop).
        if matches_kw(b, pos, b"loop") {
            let open = match find_open_brace(b, pos + 4) { Some(o) => o, None => { ctx.console_writeln("gsh: loop: missing '{'"); failed += 1; pos = b.len(); continue; } };
            let end = match find_matching_brace(b, open) { Some(e) => e, None => { ctx.console_writeln("gsh: loop: unbalanced braces"); failed += 1; pos = b.len(); continue; } };
            if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: block nesting too deep"); failed += 1; break; }
            frames[sp] = BlockKind::Loop { body: open + 1, body_end: end, base: vars.count, abase: vars.alen, iter: 0 };
            sp += 1;
            pos = open + 1;
            continue;
        }
        // a stray `else` (its `if` was taken and the chain already skipped) - malformed; skip its block.
        if matches_kw(b, pos, b"else") {
            ctx.console_writeln("gsh: unexpected 'else'");
            let after = skip_ws(b, pos + 4);
            let cs = if matches_kw(b, after, b"if") { after + 2 } else { after };
            pos = find_open_brace(b, cs).and_then(|o| find_matching_brace(b, o)).map(|e| e + 1).unwrap_or(b.len());
            last = Err(ShellError::Unknown); failed += 1;
            continue;
        }
        // `import` / `from … import` - resolved at LOAD time (resolve_imports); a no-op at runtime.
        if matches_kw(b, pos, b"import") || matches_kw(b, pos, b"from") {
            let (_, next) = read_statement(b, pos);
            pos = if next > pos { next } else { pos + 1 };
            continue;
        }
        // a `fn` DEFINITION - skip it inline (pre-scanned; runs only when called).
        if matches_kw(b, pos, b"fn") {
            pos = find_open_brace(b, pos).and_then(|o| find_matching_brace(b, o)).map(|e| e + 1).unwrap_or(b.len());
            continue;
        }
        // a simple statement (let / reassignment / fail / return / a function call / a command).
        let (stmt, next) = read_statement(b, pos);
        if next <= pos { pos += 1; continue; } // defensive: never stall
        let stmt_off = stmt.as_ptr() as usize - b.as_ptr() as usize;
        pos = next;
        if stmt.is_empty() { continue; }
        let s = str_of(stmt);
        let (head, hrest) = split_first(s);
        // `return [cmd]` - end the current function early; its result is `cmd`'s (else the last result).
        if head == "return" {
            if !hrest.is_empty() {
                let mut eb = ExpBuf::new();
                last = if expand_cmd(ctx, hrest, &vars, params, &mut eb).is_ok() {
                    execute(ctx, eb.as_bytes(), cwd, last, sdepth, &mut Out::Console)
                } else { Err(ShellError::Unknown) };
            }
            // Unwind to the nearest enclosing Call frame, discarding any if/switch frames inside it.
            let mut found = false;
            while sp > 0 {
                sp -= 1;
                match frames[sp] {
                    BlockKind::Call(ret) => {
                        run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                        vars.exit_scope();
                        pos = ret;
                        found = true;
                        break;
                    }
                    // A function used as an `if` condition (IfCall) or a `$( )` capture (CaptureCall) is
                    // a function boundary too, but its return needs branch/bind logic that `return`
                    // cannot reproduce here. Refuse it LOUDLY (never leak the scope): exit cleanly, mark
                    // the run failed, and stop.
                    BlockKind::IfCall { body_end, .. } => {
                        run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                        vars.exit_scope();
                        ctx.console_writeln("gsh: 'return' inside a function used as an 'if' condition is not supported");
                        last = Err(ShellError::Unknown); failed += 1; pos = body_end + 1;
                        found = true;
                        break;
                    }
                    BlockKind::CaptureCall { resume, .. } => {
                        run_defers(ctx, cwd, b, &mut defers, &mut ndefer, vars.sp, &mut vars, params, out, sdepth);
                        vars.exit_scope();
                        capturing = false; fncap.reset();
                        ctx.console_writeln("gsh: 'return' inside a captured function is not supported");
                        last = Err(ShellError::Unknown); failed += 1; pos = resume;
                        found = true;
                        break;
                    }
                    _ => {}
                }
            }
            if !found { ctx.console_writeln("gsh: 'return' outside a function"); }
            continue;
        }
        // `break` / `continue` - affect the nearest enclosing loop (never across a function boundary).
        if head == "break" || head == "continue" {
            let is_break = head == "break";
            let mut done = false;
            let mut i = sp;
            while i > 0 {
                i -= 1;
                match frames[i] {
                    BlockKind::For { body_end, it, .. } => {
                        if is_break {
                            // exiting a for-line loop for good -> delete its captured temp file.
                            if let ForIter::FileLines { id, .. } = it {
                                let mut tb = [0u8; 24];
                                let t = forlines_temp(id, &mut tb);
                                let _ = fs_request(ctx, OP_DELETE, t, &[]);
                            }
                            sp = i; pos = body_end + 1;               // pop loop + inner frames, exit past `}`
                        } else { sp = i + 1; pos = body_end; }        // keep loop; jump to `}` -> next iteration
                        done = true;
                        break;
                    }
                    BlockKind::Loop { body_end, .. } => {
                        if is_break { sp = i; pos = body_end + 1; }   // pop loop + inner frames, exit past `}`
                        else { sp = i + 1; pos = body_end; }           // keep loop; jump to `}` -> next iteration
                        done = true;
                        break;
                    }
                    // A loop can't be broken across a function boundary - a plain call, or a function
                    // used as an `if` condition (IfCall) or a `$( )` capture (CaptureCall).
                    BlockKind::Call(_) | BlockKind::IfCall { .. } | BlockKind::CaptureCall { .. } => break,
                    _ => {}                       // if/switch - discarded on the way out
                }
            }
            if !done { ctx.console_writeln_fmt(format_args!("gsh: '{}' outside a loop", head)); }
            continue;
        }
        // `defer <command>` - register cleanup to run when this scope exits (LIFO, even on fail, §5).
        if head == "defer" {
            if hrest.is_empty() {
                ctx.console_writeln("gsh: defer needs a command");
            } else if ndefer >= DEFER_MAX {
                ctx.console_writeln_fmt(format_args!("gsh: too many defers (max {})", DEFER_MAX));
            } else {
                let off = hrest.as_ptr() as usize - b.as_ptr() as usize;
                defers[ndefer] = (off, hrest.len(), vars.sp);
                ndefer += 1;
            }
            continue;
        }
        // a FUNCTION CALL - the head names a defined function; run its body in a fresh scope. A
        // function is NOT a pipe producer (it writes to the console, not a pipe), so `name | …` is a
        // command/producer pipe - never a call. Guard on the absence of a pipe so a function can't
        // shadow a piped producer (e.g. defining `fn count` must not break `echo x | count`).
        if !s.contains('|') {
            if let Some(fi) = ft.lookup(b, head.as_bytes()) {
                if sp >= IF_DEPTH_MAX { ctx.console_writeln("gsh: call/block nesting too deep"); failed += 1; break; }
                if dispatch_call(ctx, b, s, &ft, fi, &mut vars, params) {
                    frames[sp] = BlockKind::Call(next); // resume after the call when the body returns
                    sp += 1;
                    pos = ft.body_start[fi] as usize;
                } else {
                    last = Err(ShellError::Unknown);
                }
                continue;
            }
        }
        // `let [mut] x = $(myfn …)` - capture a FUNCTION's output into the variable. Run the function
        // via the Call machinery under a CaptureCall frame, with its body output routed to `fncap`; on
        // its return we bind `x`. (A `$(producer)` capture is NOT a function - it falls through to
        // run_stmt's existing producer-capture path below.)
        if let Some((name, mutable, inner)) = let_capture_form(s) {
            let (w0, _) = split_first(inner);
            if let Some(fi) = ft.lookup(b, w0.as_bytes()) {
                if capturing {
                    ctx.console_writeln("gsh: nested $(fn) capture is not supported");
                    last = Err(ShellError::Unknown); failed += 1; pos = next;
                } else if sp >= IF_DEPTH_MAX {
                    ctx.console_writeln("gsh: call/block nesting too deep"); failed += 1; break;
                } else if dispatch_call(ctx, b, inner, &ft, fi, &mut vars, params) {
                    let name_off = name.as_ptr() as usize - b.as_ptr() as usize;
                    frames[sp] = BlockKind::CaptureCall { name_off, name_len: name.len(), mutable, resume: next };
                    sp += 1;
                    capturing = true;
                    fncap.reset();
                    pos = ft.body_start[fi] as usize;
                } else {
                    last = Err(ShellError::Unknown); failed += 1; pos = next;
                }
                continue;
            }
        }
        // Echo the statement so the transcript shows what produced each result (not in quiet
        // mode: a library command's user wants the output, not a transcript of the script).
        if !quiet {
            out.put(ctx, "> ");
            out.line(ctx, s);
        }
        let (res, stop) = {
            // While a $(fn) capture is active, the command's OUTPUT goes to the capture buffer, not
            // the console (the transcript `> stmt` above still goes to `out`).
            let mut cmd_out = if capturing { Out::FnCap(&mut fncap) } else { Out::Console };
            match run_stmt(ctx, cwd, s, last, sdepth, &mut vars, params, &mut cmd_out) {
                StmtOutcome::Cont(r) => (r, false),
                StmtOutcome::Stop(r) => (r, true),
            }
        };
        last = res;
        if nrec < RUN_MAX_CMDS { verdict[nrec] = last.is_ok(); soff[nrec] = stmt_off as u16; slng[nrec] = stmt.len() as u16; }
        nrec += 1;
        ran += 1;
        if last.is_err() { failed += 1; }
        if stop { break; }
    }
    // Script exit (normal end OR `fail`): run any remaining defers - LIFO, across all scopes (§5).
    run_defers(ctx, cwd, b, &mut defers, &mut ndefer, 0, &mut vars, params, out, sdepth);
    // End-of-run summary: PASS/FAIL per EXECUTED statement, from the recorded spans.
    // "FAIL  " is deliberately not the word "FAILED" the harness greens on absence of.
    // Quiet (library command): no report - each failing section already printed its own error,
    // and the Err below still surfaces in `result`.
    if !quiet {
        out.line(ctx, "--- summary ---");
        let shown = nrec.min(RUN_MAX_CMDS);
        for j in 0..shown {
            out.put(ctx, if !verdict[j] { "FAIL  " } else { "PASS  " });
            out.line(ctx, str_of(&b[soff[j] as usize..soff[j] as usize + slng[j] as usize]));
        }
        out.line_fmt(ctx, format_args!("run: ran {}, failed {}", ran, failed));
    }
    if failed == 0 { Ok(()) } else { Err(ShellError::Unknown) }
}

/// Run `src` and, if `save` is `Some`, stream the report to that file (the utility writes its own
/// file - direct, not a pipe). Bare → report to the console. Shared by `run`/`selfcheck`. This
/// dispatcher is tiny on purpose: the 32 KiB `ReportBuf` lives ONLY in `run_and_save`, called only
/// on the save path - so a bare run/selfcheck does NOT carry 32 KiB of unused frame (which would
/// tip its already-heavy `| assert` sub-pipelines over the user-stack ceiling).
fn run_with_optional_save(ctx: &ShellCtx, cwd: &mut Cwd, src: &[u8], depth: u8, save: Option<&str>, params: &Params)
    -> Result<(), ShellError>
{
    match save {
        None => run_lines(ctx, cwd, src, depth, &mut Out::Console, params, false),
        Some(spath) => run_and_save(ctx, cwd, src, depth, spath, params),
    }
}

/// The save path: accumulate the run report into a bounded `ReportBuf` and write it to `spath`
/// (direct file write, no pipe). `#[inline(never)]` so the 32 KiB buffer exists only while a save
/// is actually running, not in the frame of every bare run.
#[inline(never)]
fn run_and_save(ctx: &ShellCtx, cwd: &mut Cwd, src: &[u8], depth: u8, spath: &str, params: &Params)
    -> Result<(), ShellError>
{
    let mut pbuf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, spath, &mut pbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut ppath = [0u8; PATH_MAX];
    let pl = path.len();
    ppath[..pl].copy_from_slice(path);
    let path = &ppath[..pl];

    let mut rb = ReportBuf::new();
    let result = {
        let mut out = Out::File(&mut rb);
        run_lines(ctx, cwd, src, depth, &mut out, params, false)
    }; // `out` (the &mut rb borrow) ends here, so `rb` is readable below
    if rb.overflow {
        ctx.console_writeln_fmt(format_args!(
            "save: report exceeded {} KiB - saved truncated to {}", REPORT_MAX / 1024, str_of(path)));
    }
    if !save_report(ctx, path, rb.bytes()) {
        ctx.console_writeln_fmt(format_args!("save: could not write {} (storage, or bad path?)", str_of(path)));
        return Err(ShellError::Unknown);
    }
    ctx.console_writeln_fmt(format_args!("saved report ({} bytes) to {}", rb.bytes().len(), str_of(path)));
    result
}

/// Write a report buffer to `path`, streaming to a multi-block file (the report exceeds one
/// message). Quiet (the caller prints the human message); returns success. Reuses the same
/// `WriteFile` / `WriteNew`+`WriteAt` shape as the pipe `write` sink, with no intermediate copy.
fn save_report(ctx: &ShellCtx, path: &[u8], data: &[u8]) -> bool {
    // Bounded fs request (wall-clock): a chaos report is saved right after the storm may have hammered
    // fs, so the write must time out gracefully rather than hang the shell (the max-carnage aggregate
    // report is small → this single-message path).
    if data.len() <= IO_CHUNK {
        return matches!(fs_request_bounded(ctx, OP_WRITE_FILE, path, data, SAVE_FS_MAX_SECS)
            .as_ref().map(|r| r.payload_bytes().first().copied()), Some(Some(FS_OK)));
    }
    if !fs_write_new(ctx, path, data.len() as u64) { return false; }
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + IO_CHUNK).min(data.len());
        if !fs_write_at(ctx, path, off as u64, &data[off..end]) { return false; }
        off = end;
    }
    true
}

/// Cap on per-command summary lines `run` records (the verdict array). Commands past this still
/// run and count in the totals; only their individual PASS/FAIL line is omitted.
const RUN_MAX_CMDS: usize = 256;

/// The self-check suite, embedded in the shell binary (so it ships with the boot image - no
/// host-side `dd` of a data disk). Run straight from rodata, so it can be far larger than an
/// on-disk file (`MAX_FILE_BYTES` - a file is one ≤4 KiB IPC message; rodata is not).
const SELFCHECK_GS: &str = include_str!("../../../scripts/selfcheck.gsh");

/// The system library: gsh scripts baked into the image (rodata) and resolved PATH-like - typing a
/// library name runs its script. This is the OS's "coreutils in gsh": features that grow by userspace
/// COMPOSITION of the existing utilities, not new kernel or service surface (§26.2). Add a script to
/// `scripts/lib/`, `include_str!` it here, and it becomes a command. Like `run`/`selfcheck`, a library
/// command runs ONE script layer via `run_lines`, so it is prompt-level only (refused inside another
/// script - two nested interpreter frames would blow the bounded user stack, [[project-shell-stack-pipe]]).
const LIBRARY: &[(&str, &str)] = &[
    ("health",  include_str!("../../../scripts/lib/health.gsh")),
    ("watch",   include_str!("../../../scripts/lib/watch.gsh")),
    ("size",    include_str!("../../../scripts/lib/size.gsh")),
    ("online",  include_str!("../../../scripts/lib/online.gsh")),
    ("busiest", include_str!("../../../scripts/lib/busiest.gsh")),
];

// audit U6: baked scripts must stay under the u16 offset ceiling `prescan_fns` uses (64 KiB), or the
// fn/summary offsets wrap silently and dispatch the wrong bodies. Fail the build, not at runtime.
const _: () = assert!(SELFCHECK_GS.len() < 65536, "selfcheck.gsh exceeds the 64 KiB baked-script ceiling");
const _: () = {
    let mut i = 0;
    while i < LIBRARY.len() {
        assert!(LIBRARY[i].1.len() < 65536, "a library script exceeds the 64 KiB baked-script ceiling");
        i += 1;
    }
};

/// The baked source of library command `name`, or `None` if `name` is not a library command.
fn library_script(name: &str) -> Option<&'static str> {
    LIBRARY.iter().find(|(n, _)| *n == name).map(|&(_, src)| src)
}

/// `selfcheck` - run the embedded self-check suite IN MEMORY (straight from rodata via
/// `run_lines`; no file write, so it is not capped by `MAX_FILE_BYTES`). The one-USB hardware
/// checkpoint - flash the boot image, (`drives flash` a drive if it's raw, so the file-command
/// tests have somewhere to write), then `selfcheck`. Re-runnable (the suite creates and deletes
/// its own files). Refused inside a script (it runs one - no nesting).
#[inline(never)]
fn cmd_selfcheck(ctx: &ShellCtx, cwd: &mut Cwd, depth: u8, arg: &str) -> Result<(), ShellError> {
    if depth > 0 {
        ctx.console_writeln("selfcheck: not available inside a script (it runs one)");
        return Err(ShellError::Unknown);
    }
    // Optional `save <path>`: stream the run REPORT to a file (the utility writes its own file -
    // direct, not a pipe, so the orchestrator can save without the nested-capture stack overflow).
    let save = if arg.is_empty() {
        None
    } else {
        match arg.strip_prefix("save") {
            Some(r) if r.starts_with(char::is_whitespace) && !r.trim().is_empty() => Some(r.trim()),
            _ => {
                ctx.console_writeln("usage: selfcheck [save <path>]");
                return Err(ShellError::Unknown);
            }
        }
    };
    ctx.console_writeln_fmt(format_args!(
        "selfcheck: running the embedded suite ({} bytes, in memory) - needs a flashed drive for the file tests...",
        SELFCHECK_GS.len()));
    run_with_optional_save(ctx, cwd, SELFCHECK_GS.as_bytes(), depth, save, &Params::empty("selfcheck"))
}

/// `assert ok <cmd>` / `assert fails <cmd>` - the **result** form: run `<cmd>` and check that it
/// succeeded (`ok`) or failed (`fails`). The assertion holds → `Ok` + `assert: ok`; it doesn't →
/// `Err(AssertFailed)` + a `FAILED` line. This is the negative-test surface (§22's negative cases
/// on hardware): `assert fails read /nope` verifies the guardrail refuses. The *content* form
/// (`… | assert contains X`) is the pipe sink `assert_stream`.
fn cmd_assert(ctx: &ShellCtx, cwd: &mut Cwd, rest: &str, depth: u8) -> Result<(), ShellError> {
    let (verb, cmd) = split_first(rest);
    match verb {
        "ok" | "fails" => {
            if cmd.is_empty() {
                ctx.console_writeln("usage: assert ok <command>  |  assert fails <command>");
                return Err(ShellError::Unknown);
            }
            // Run the command (its own output/errors print as usual), then judge its Result.
            let r = execute(ctx, cmd.as_bytes(), cwd, Ok(()), depth + 1, &mut Out::Console);
            let held = if verb == "ok" { r.is_ok() } else { r.is_err() };
            assert_verdict(ctx, held, verb, cmd)
        }
        // `assert fails-with <Variant> <cmd>` - pin the SPECIFIC failure (precise negative test).
        "fails-with" => {
            let (variant, inner) = split_first(cmd);
            if variant.is_empty() || inner.is_empty() {
                ctx.console_writeln("usage: assert fails-with <Variant> <command>  (e.g. FileNotFound, Denied)");
                return Err(ShellError::Unknown);
            }
            let r = execute(ctx, inner.as_bytes(), cwd, Ok(()), depth + 1, &mut Out::Console);
            let held = matches!(r, Err(e) if e.name() == variant);
            assert_verdict(ctx, held, "fails-with", variant)
        }
        "contains" | "lacks" | "empty" => {
            ctx.console_writeln_fmt(format_args!(
                "assert: '{}' checks a pipe - use: <producer> | assert {} …", verb, verb));
            Err(ShellError::Unknown)
        }
        _ => {
            ctx.console_writeln(
                "usage: assert ok|fails <command>   or   <producer> | assert contains|lacks|empty …");
            Err(ShellError::Unknown)
        }
    }
}

// ---------------------------------------------------------------------------
// Per-utility help + version (0_conventions.md). Every utility self-documents:
// `<util> help` prints usage with a real example per row; `<util> version` prints the
// version + creator credit. The format lives in ONE place (`help_block`) so all utilities
// render identically and a tweak updates every one at once.
// ---------------------------------------------------------------------------

const UTIL_VERSION: &str = "0.4.0";

/// Utilities that self-document (gates the `help`/`version` intercept in `execute`).
const UTILS: &[&str] = &[
    "help", "result", "run", "assert", "selfcheck",
    "echo", "input", "clear", "about", "version", "mem", "cores", "date", "net", "ping", "sock", "uptime", "wait", "whatis", "status", "observe", "caps", "roster",
    "spawn", "kill", "restart", "reboot", "chaos", "drives", "ls", "cd", "read", "write", "edit", "fcap",
    "mkdir", "copy", "move", "rename", "delete", "find", "tree", "match", "count", "sort",
    "first", "last",
    // record-pipe verbs (pipe-only stages; see docs/records.md)
    "where", "select", "to", "from", "sum", "min", "max", "avg",
];
fn is_util(name: &str) -> bool { UTILS.contains(&name) }

/// `<util> version` - version number, then creator credit.
fn util_version(ctx: &ServiceContext, util: &str) {
    ctx.console_writeln_fmt(format_args!("{} {}", util, UTIL_VERSION));
    ctx.console_writeln("Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.");
}

/// One usage row: (signature with `<placeholders>`, description, a real example).
type Row = (&'static str, &'static str, &'static str);

/// Render the standard help block: `<title> <ver> - <desc>`, each usage row followed by a
/// real example, then (for a top-level utility) the version/help footer.
fn help_block(ctx: &ServiceContext, title: &str, desc: &str, rows: &[Row], footer: bool) {
    // PAGE IT WHEN IT DOES NOT FIT. `help` (the full list) has paged for a long time; a single
    // command's help never did, because no command's help was taller than a screen. `trace`'s is: it
    // documents six views and eight columns, and on a 34-row console the top scrolled away for good
    // on a framebuffer with no scrollback. The fix is not to write less - the column notes are the
    // useful part - it is to reuse the pager that already exists.
    let lines = help_block_lines(rows, footer);
    let (rows_avail, _) = ctx.console_dims();
    // Unknown geometry is not "no terminal": a failed lookup returns 0, and `edit` and `trace` both
    // assume 24 rather than dropping the feature.
    let rows_avail = if rows_avail == 0 { 24 } else { rows_avail as usize };
    if lines + 3 <= rows_avail {
        help_block_render(ctx, title, desc, rows, footer, 0, lines);
        return;
    }
    line_pager(ctx, lines, rows_avail,
        &|c| {
            c.console_write_fmt(format_args!("{} {} - {}\x1b[K\n", title, UTIL_VERSION, desc));
            c.console_write("\x1b[K\n");
            c.console_write("usage:\x1b[K\n");
            3
        },
        &|c, i| help_block_line(c, rows, footer, i), &|_| {});
}

/// How many scrolling lines a help block has (the header is pinned, so it does not count).
fn help_block_lines(rows: &[Row], footer: bool) -> usize {
    let mut n = 0usize;
    for (_, _, ex) in rows { n += if ex.is_empty() { 1 } else { 2 }; }
    if footer { n += 2; }
    n
}

/// Render scrolling line `i` of a help block, erasing its tail for the pager's in-place repaint.
///
/// CLAMPED TO THE CONSOLE WIDTH, and that is load-bearing rather than cosmetic. The pager counts
/// LOGICAL lines and paints one per screen row; a line longer than the terminal wraps onto a second
/// row, so every wrapped row pushes the frame down, scrolls the pinned header off the top and makes
/// the whole thing look like it started in the middle. That is exactly what `trace help` did on a
/// 102-column display while looking perfect on serial, which has no width at all.
///
/// The rows are short now, but content should not be able to break the frame - so an over-long line
/// is cut and marked with a `>` rather than silently wrapped. Visible truncation is a bug report; a
/// broken pager is a mystery.
fn help_block_line(ctx: &ServiceContext, rows: &[Row], footer: bool, i: usize) {
    let mut n = 0usize;
    for (sig, d, ex) in rows {
        if n == i {
            help_write_clamped(ctx, format_args!("  {:<28}  {}", sig, d));
            return;
        }
        n += 1;
        if !ex.is_empty() {
            if n == i {
                help_write_clamped(ctx, format_args!("      e.g. {}", ex));
                return;
            }
            n += 1;
        }
    }
    if footer {
        if n == i     { ctx.console_write("  version\x1b[K\n"); return; }
        if n + 1 == i { ctx.console_write("  help\x1b[K\n"); }
    }
}

/// Write one pager line, cut to the console width so it occupies exactly one screen row.
fn help_write_clamped(ctx: &ServiceContext, args: core::fmt::Arguments) {
    let (_, cols) = ctx.console_dims();
    let cols = if cols == 0 { 80 } else { cols as usize };
    let mut buf = [0u8; 256];
    let mut w = ClampWriter { buf: &mut buf, n: 0 };
    let _ = core::fmt::write(&mut w, args);
    let n = w.n;
    let keep = n.min(cols.saturating_sub(1)).min(256);
    if let Ok(text) = core::str::from_utf8(&buf[..keep]) {
        ctx.console_write(text);
        if keep < n { ctx.console_write(">"); }
    }
    ctx.console_write("\x1b[K\n");
}

/// A fixed-buffer `fmt::Write` sink. Bounded, no heap (26.6.1).
struct ClampWriter<'a> { buf: &'a mut [u8; 256], n: usize }
impl core::fmt::Write for ClampWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.buf.len().saturating_sub(self.n);
        let take = s.len().min(room);
        self.buf[self.n..self.n + take].copy_from_slice(&s.as_bytes()[..take]);
        self.n += take;
        Ok(())
    }
}

/// The unpaged rendering, for a block that fits.
fn help_block_render(ctx: &ServiceContext, title: &str, desc: &str, rows: &[Row], footer: bool,
                     from: usize, to: usize) {
    ctx.console_writeln_fmt(format_args!("{} {} - {}", title, UTIL_VERSION, desc));
    ctx.console_writeln("");
    ctx.console_writeln("usage:");
    let mut n = 0usize;
    for (sig, d, ex) in rows {
        if n >= from && n < to { ctx.console_writeln_fmt(format_args!("  {:<28}  {}", sig, d)); }
        n += 1;
        if !ex.is_empty() {
            if n >= from && n < to { ctx.console_writeln_fmt(format_args!("      e.g. {}", ex)); }
            n += 1;
        }
    }
    if footer {
        ctx.console_writeln_fmt(format_args!("  {} version", title));
        ctx.console_writeln_fmt(format_args!("  {} help", title));
    }
}

/// `<util> help` - usage with examples. Returns false for an unknown name.
fn util_help(ctx: &ServiceContext, util: &str) -> bool {
    match util {
        "help" => help_block(ctx, "help", "list all commands (or get help on one)", &[
            ("help", "the full categorised command list", "help"),
            ("<command> help", "usage + examples for one command", "status help"),
        ], true),
        "trace" => help_block(ctx, "trace", "who waits on whom, who can reach whom, what just happened", &[
            ("trace blocked", "every task stuck on another task", "trace blocked"),
            ("trace chain <name|slot>", "the same, as a tree from one task", "trace chain fs"),
            ("trace deps <service>", "what it can call, as a tree", "trace deps shell"),
            ("trace endpoints", "every live endpoint and its owner", "trace endpoints"),
            ("trace endpoint <id>", "who owns one, and who can reach it", "trace endpoint 112"),
            ("trace ipc", "recent IPC exchanges, oldest first", "trace ipc"),
            ("trace failures", "the same, only timeouts and lost peers", "trace failures"),
            ("trace status", "ring size, events recorded, events dropped", "trace status"),
            ("", "", ""),
            ("trace <view> help", "what that view's output MEANS, column by column", "trace ipc help"),
        ], true),
        "result" => help_block(ctx, "result", "show the previous command's result (Ok / Err)", &[
            ("result", "Ok if the last command succeeded, else Err(<reason>)", "result"),
        ], true),
        "run" => help_block(ctx, "run", "run a script of commands from a file", &[
            ("run <path>", "execute each line/command as if typed; reports ran N, failed M", "run /suite.gsh"),
            ("run <path> save <out>", "also write the run report to a file (the utility owns the file)", "run /suite.gsh save /report.txt"),
            ("# … (in the file)", "lines starting with # are comments; ';' separates commands", "run /test.gsh"),
        ], true),
        "fmt" => help_block(ctx, "fmt", "format a .gsh script to the GodspeedOS standard (in place)", &[
            ("fmt <path>", "format the script IN PLACE - one canonical layout, no options", "fmt /script.gsh"),
            ("fmt check <path>", "Ok if already canonical, else loud + Err; never writes", "fmt check /script.gsh"),
            ("fmt <a>,<b>,...", "format (or check) several files - comma-separated, done one at a time", "fmt /x.gsh,/y.gsh"),
        ], true),
        "selfcheck" => help_block(ctx, "selfcheck", "run the built-in self-check suite (needs a flashed drive)", &[
            ("selfcheck", "run the embedded suite in memory; reports ran N, failed M", "selfcheck"),
            ("selfcheck save <out>", "run it and write the report to a file (then read/edit/grep it)", "selfcheck save /report.txt"),
        ], true),
        "roster" => help_block(ctx, "roster", "example record-producing service (a typed table you can pipe)", &[
            ("roster", "render the table directly (name / role / seat)", "roster"),
            ("roster | where <col><op><val>", "filter rows - it is a record source for the pipe verbs", "roster | where role=core"),
            ("roster | select <cols> | to json", "project columns / render as JSON at the edge", "roster | select name seat | to json"),
        ], true),
        "assert" => help_block(ctx, "assert", "verify a result or output; Ok if it holds, else Err", &[
            ("assert ok <command>", "the command must succeed", "assert ok read /notes.txt"),
            ("assert fails <command>", "the command must fail (negative test)", "assert fails read /nope"),
            ("assert fails-with <V> <command>", "must fail with the named Err variant", "assert fails-with FileNotFound read /nope"),
            ("<producer> | assert contains <text>", "piped output must contain <text>", "roster | where role=core | assert contains Matthew"),
            ("… | assert lacks <text> / empty", "must NOT contain / must be empty", "ls / | assert lacks secret"),
        ], true),
        "echo" => help_block(ctx, "echo", "print text", &[
            ("echo <text>", "print text verbatim", "echo hello world"),
        ], true),
        "input" => help_block(ctx, "input", "read one line from the user (a producer; capture with $( ))", &[
            ("input \"prompt\"", "prompt, then read a visible line", "let name = $(input \"Name: \")"),
            ("input secret \"prompt\"", "invisible entry; the value is tainted (never echoed to console)", "let pw = $(input secret \"Password: \")"),
        ], true),
        "clear" => help_block(ctx, "clear", "clear the screen", &[
            ("clear", "clear the screen and home the cursor", "clear"),
        ], true),
        "about" => help_block(ctx, "about", "system identity + credits", &[
            ("about", "name, version + arch, core count, creator", "about"),
        ], true),
        "version" => help_block(ctx, "version", "the GodspeedOS version + architecture + build stamp", &[
            ("version", "GodspeedOS <version> <arch> (<git-sha>)", "version"),
        ], true),
        "wait" => help_block(ctx, "wait", "do nothing for N seconds (q aborts)", &[
            ("wait <seconds>", "pause for N wall-clock seconds; q/Esc aborts with Err", "wait 2"),
        ], true),
        "whatis" => help_block(ctx, "whatis", "what runs when a name is typed (kind + origin)", &[
            ("whatis <name>", "built-in / library script / pipe stage / service (live task+core)", "whatis ls"),
            // The one collision in the vocabulary: whatis's argument domain CONTAINS the words
            // `help` and `version`, and the universal `<util> help|version` rule answers first -
            // so this help text itself carries the answer you were asking for.
            ("whatis help|version", "answer for whatis itself (as every utility does); both names are shell built-ins", "whatis help"),
        ], true),
        "mem" => help_block(ctx, "mem", "physical memory usage", &[
            ("mem", "used / total / free physical memory", "mem"),
        ], true),
        "cores" => help_block(ctx, "cores", "CPU core count + timer-tick rate", &[
            ("cores", "how many CPU cores are up", "cores"),
            ("cores ticks", "each core's timer ticks/s (5s RTC-paced sample)", "cores ticks"),
        ], true),
        "date" => help_block(ctx, "date", "date + time (from the hardware clock, or the network on the RTC-less Pi)", &[
            ("date", "full timestamp (weekday date time)", "date"),
            ("date epoch", "seconds since 1970-01-01", "date epoch"),
            ("date sync", "sync the clock from the internet NOW (it also syncs itself)", "date sync"),
        ], true),
        "net" => help_block(ctx, "net", "network status, DNS, and ARP host discovery", &[
            ("net", "IP, gateway (+MAC), and whether the gateway pings", "net"),
            ("net dns <host>", "resolve a hostname to an IPv4 address", "net dns example.com"),
            ("net stats", "dump the NIC's raw registers (chip state: RE/RCR/RX ring)", "net stats"),
            ("net arp <ip>", "resolve one host's MAC by ARP", "net arp 192.168.4.1"),
            ("net scan", "ARP-sweep the local /24 for live hosts", "net scan"),
            ("net renew", "re-run DHCP/ARP after plugging in a cable (recover without a reboot)", "net renew"),
            ("net | write <path>", "snapshot the status to a file", "net | write /netstat.txt"),
        ], true),
        "ping" => help_block(ctx, "ping", "continuous ICMP echo to a raw IPv4 address (no DNS)", &[
            ("ping <ip>", "ping continuously (round-trip time + TTL per reply); q quits, then stats", "ping 192.168.4.1"),
            ("ping count <N> <ip>", "send N echoes then stop and print statistics", "ping count 4 8.8.8.8"),
            ("ping bytes <N> <ip>", "set the ICMP data size (default 32, max 1024)", "ping bytes 64 8.8.8.8"),
            ("ping count <N> <ip> | write <path>", "capture a bounded run to a file", "ping count 4 8.8.8.8 | write /ping.txt"),
        ], true),
        "sock" => help_block(ctx, "sock", "a UDP socket as a capability (demo)", &[
            ("sock", "open a socket cap, send a datagram through it, report the round-trip", "sock"),
        ], true),
        "uptime" => help_block(ctx, "uptime", "how long the system has been up", &[
            ("uptime", "uptime (Nd HH:MM:SS) + seconds since boot", "uptime"),
            ("uptime | to json|yaml", "piped: a record with 'uptime' + 'seconds'", "uptime | to yaml"),
            ("uptime | select seconds", "piped: just the total seconds", "uptime | select seconds"),
        ], true),
        "status" => help_block(ctx, "status", "list all live tasks", &[
            ("status", "slot, name, core, state of every task", "status"),
        ], true),
        "observe" => help_block(ctx, "observe", "live system metrics view (records when piped)", &[
            ("observe", "full-screen live view (q to quit)", "observe"),
            ("observe now", "one-shot metrics frame", "observe now"),
            ("observe now | <verb>", "piped: records + a 'ticks' (cpu-time) column", "observe now | sort reverse ticks"),
        ], true),
        "caps" => help_block(ctx, "caps", "show a service's capabilities (records when piped)", &[
            ("caps", "this shell's own capabilities", "caps"),
            ("caps <service>", "capabilities held by <service>", "caps logger"),
            ("caps [service] | <verb>", "piped: records resource/rights", "caps logger | where rights contains send"),
        ], true),
        "spawn" => help_block(ctx, "spawn", "start a service", &[
            ("spawn <svc>", "start the service <svc>", "spawn pong"),
            ("spawn <svc>,<svc>,...", "start several at once (comma-separated, NO spaces)", "spawn ping,pong"),
        ], true),
        "kill" => help_block(ctx, "kill", "stop a service (it self-heals - the supervisor respawns it)", &[
            ("kill <svc>", "kill one service; it recovers (only the kernel never dies)", "kill pong"),
            ("kill <svc>,<svc>,...", "kill several at once (comma-separated, NO spaces)", "kill ehci,xhci,fs"),
            ("kill all-services", "nuke EVERY service - drivers, storage, net, logger, supervisor, and this shell last; each self-heals", "kill all-services"),
            ("(per-service rules still apply)", "supervisor is killable + kernel-respawned; spawn/restart of it stay refused", "kill supervisor"),
        ], true),
        "restart" => help_block(ctx, "restart", "restart a service", &[
            ("restart <name>", "restart (re-placed per contract)", "restart pong"),
            ("restart <name> <core>", "restart on core <core>", "restart pong 2"),
            ("restart <a>,<b>,...", "restart several, each per its own contract (no core override)", "restart fs,logger"),
        ], true),
        "reboot" => help_block(ctx, "reboot", "hardware reset", &[
            ("reboot", "reset the machine", "reboot"),
        ], true),
        "chaos" => help_block(ctx, "chaos", "bounded resilience exerciser - stress one invariant, report a verdict", &[
            ("chaos kill-storm <svc> [rounds]", "kill a service N times; verify it recovers each time", "chaos kill-storm supervisor 20"),
            ("chaos kill-storm <svc> [n] save <path>", "also write the report to a file (recorded in memory, written at the end)", "chaos kill-storm fs 20 save /chaos.txt"),
            ("  <svc> = supervisor | block-driver | fs", "recoverable targets: the supervisor respawns the services, the kernel respawns the supervisor - only the kernel can't be killed", "chaos kill-storm supervisor 10"),
            ("chaos flood-storm <svc> [rounds]", "saturate a service's IPC queue with try_send; verify it drains + stays alive (the other axis: 'overwhelmed', not 'gone')", "chaos flood-storm fs 5"),
            ("chaos mem-pressure [rounds]", "spawn a mem-pressure that allocs to its limit, kill it, confirm the memory is reclaimed (alloc-to-limit + no leak, S7)", "chaos mem-pressure 5"),
            ("chaos spawn-storm [count]", "spawn mem-pressure tasks until the task-pool/memory ceiling REFUSES one (loud Err, no panic), then kill all + confirm full reclaim", "chaos spawn-storm"),
            ("chaos max-carnage <all-services|svc|svc,svc> <n> [yes]", "the chaos monkey: 'all-services' = RANDOM carnage over the whole restartable set each round (supervisor a normal victim, nothing protected-last); or aim at one / a comma-list. A TARGET AND A ROUNDS COUNT ARE REQUIRED - there is no uncapped default (a firehose is a big N; q aborts early). Under system-wide mem-pressure + spawn-storm; proves the system RECOVERS from any kill order. 'q' aborts (via SERIAL if it storms the USB keyboard drivers)", "chaos max-carnage all-services 5000"),
            ("chaos link-flap [n]", "networking-specific: simulate a cable unplug/replug n times (a report override, no hardware touch); net-stack notices the loss and self-configures on the up edge. tests LINK recovery, not process death. 'q' aborts", "chaos link-flap 3"),
        ], true),
        "drives" => help_block(ctx, "drives", "manage attached disks (records when piped)", &[
            ("drives", "list attached drive(s)", "drives"),
            ("drives | <verb>", "piped: records index/label/status/size_mib/free_mib", "drives | where free_mib>0"),
            ("drives flash [drive] [label]", "format a drive as GSFS (ERASES)", "drives flash 0 data"),
            ("drives label [drive] <name>", "name / rename a drive", "drives label 0 archive"),
            ("drives reset [drive]", "un-format a drive back to raw", "drives reset 0"),
            ("drives check [drive]", "verify (fsck): rebuild bitmap/free, report CRC failures", "drives check"),
            ("drives scrub [drive]", "read-only integrity sweep: verify every block's CRC, report (changes nothing)", "drives scrub"),
        ], true),
        "ls" => help_block(ctx, "ls", "list a directory (records when piped)", &[
            ("ls", "list the current directory", "ls"),
            ("ls <path>", "list the directory at <path>", "ls /docs"),
            ("ls [path] | <verb>", "piped: emits records name/type/size", "ls | where size>0"),
            ("ls | select … / sort …", "project / order the listing", "ls | sort reverse size"),
        ], true),
        "cd" => help_block(ctx, "cd", "change current directory", &[
            ("cd <path>", "move to <path> (no arg → root)", "cd /docs"),
            ("cd -", "move to the previous directory", "cd -"),
        ], true),
        "read" => help_block(ctx, "read", "print a file", &[
            ("read <path>", "print the contents of <path>", "read /docs/notes.txt"),
        ], true),
        "write" => help_block(ctx, "write", "create, overwrite, append, or prepend a file", &[
            ("write <path>", "create an empty file", "write /docs/todo.txt"),
            ("write <path> <text>", "create/overwrite with text", "write /docs/todo.txt \"buy milk\""),
            ("write append <path> <text>", "add text to the end (create if missing)", "write append /docs/todo.txt \"eggs\""),
            ("write prepend <path> <text>", "add text to the front (create if missing)", "write prepend /docs/todo.txt \"# list\""),
            ("<producer> | write [append|prepend] <path>", "save piped output to a file", "about | write /about.txt"),
        ], true),
        "edit" => help_block(ctx, "edit", "full-screen text editor (^S save, ^Q quit)", &[
            ("edit <path>", "open <path> for editing (creates it on save if new)", "edit /notes.txt"),
        ], true),
        "mkdir" => help_block(ctx, "mkdir", "create a directory", &[
            ("mkdir <path>", "create the directory <path>", "mkdir /docs"),
            ("mkdir <path> parents", "create missing parent dirs too", "mkdir /a/b/c parents"),
            ("mkdir <a>,<b>,...", "create several directories (comma-separated)", "mkdir /docs,/tmp"),
        ], true),
        "copy" => help_block(ctx, "copy", "copy a file or a whole subtree", &[
            ("copy <src> <dst>", "copy file <src> to <dst>", "copy /docs/a.txt /docs/b.txt"),
            ("copy <src> <dst> recursive", "copy directory <src> and everything under it", "copy /docs /backup recursive"),
        ], true),
        "move" => help_block(ctx, "move", "relocate a file or directory", &[
            ("move <src> <dst>", "move <src> to <dst>", "move /docs/a.txt /archive/a.txt"),
        ], true),
        "rename" => help_block(ctx, "rename", "rename an entry in place", &[
            ("rename <path> <newname>", "rename <path> to <newname>", "rename /docs/a.txt b.txt"),
        ], true),
        "delete" => help_block(ctx, "delete", "remove a file, empty directory, or whole subtree", &[
            ("delete <path>", "remove the file/empty dir <path>", "delete /docs/old.txt"),
            ("delete <path> recursive", "remove directory <path> and everything under it", "delete /docs recursive"),
            ("delete <a>,<b>,...", "remove several (comma-separated; recursive applies to all)", "delete /a.txt,/b.txt"),
        ], true),
        "find" => help_block(ctx, "find", "search the tree by name (substring/glob; records when piped)", &[
            ("find <name>", "matches names containing <name>", "find report"),
            ("find <glob>", "glob match: * = any run, ? = one char", "find *.txt"),
            ("find <pattern> <path>", "search only under <path>", "find *.txt /docs"),
            ("find … | <verb>", "piped: records name/type/path", "find *.txt | where type=file"),
        ], true),
        "tree" => help_block(ctx, "tree", "print the directory hierarchy", &[
            ("tree", "tree of the current directory", "tree"),
            ("tree <path>", "tree rooted at <path>", "tree /docs"),
        ], true),
        "match" => help_block(ctx, "match", "keep the lines that match a pattern", &[
            ("<producer> | match <pattern>", "keep piped lines matching <pattern>", "read /log | match error"),
            ("match <pattern> <path>", "keep lines of <path> that match", "match error /log"),
            ("match except <pattern> [path]", "keep the lines that do NOT match", "read /log | match except debug"),
            ("match \"<two words>\" …", "quote a multi-word pattern", "match \"out of memory\" /log"),
        ], true),
        "count" => help_block(ctx, "count", "count lines/words/bytes (byte stream) or ROWS (record stream)", &[
            ("<producer> | count", "count piped bytes, or rows of a record stream", "status | count"),
            ("count <path>", "count a file", "count /log"),
        ], true),
        "sort" => help_block(ctx, "sort", "order the lines (ascending, or reverse)", &[
            ("<producer> | sort", "sort piped lines", "find *.txt | sort"),
            ("sort <path>", "sort a file's lines", "sort /names.txt"),
            ("sort reverse [path]", "sort descending", "read /names.txt | sort reverse"),
        ], true),
        "first" => help_block(ctx, "first", "keep the first N lines (default 10)", &[
            ("<producer> | first [N]", "first N piped lines", "find *.txt | first 5"),
            ("first [N] <path>", "first N lines of a file", "first 20 /log"),
        ], true),
        "last" => help_block(ctx, "last", "keep the last N lines (default 10)", &[
            ("<producer> | last [N]", "last N piped lines", "read /log | last 20"),
            ("last [N] <path>", "last N lines of a file", "last 20 /log"),
        ], true),
        "where" => help_block(ctx, "where", "keep records whose field matches (record-pipe stage)", &[
            ("<records> | where <col><op><val>", "ops: = != > < >= <=, and the word `contains`", "status | where mem>0"),
            ("… | where state=BlockRecv", "textual when either side is non-numeric", "status | where state=BlockRecv"),
            ("… | where <col> contains <text>", "substring match - a WORD, because a symbol for it was unreadable", "caps logger | where rights contains send"),
        ], true),
        "select" => help_block(ctx, "select", "keep only some columns, in order (record-pipe stage)", &[
            ("<records> | select <col> [col…]", "project the named columns", "status | select name core state"),
        ], true),
        "to" => help_block(ctx, "to", "render records to a format (record-pipe stage)", &[
            ("<records> | to json", "JSON array of objects", "status | to json"),
            ("<records> | to yaml", "YAML list of mappings", "status | where mem>0 | to yaml"),
            ("<records> | to grid", "the plain table - useful when a producer draws something else", "trace deps fs | to grid"),
        ], true),
        "from" => help_block(ctx, "from", "parse text into records (record-pipe stage)", &[
            ("<text> | from json", "parse a flat JSON array of objects", "read /svc.json | from json"),
            ("read x.json | from json | …", "bridge text → records, then filter", "read /svc.json | from json | where core=1"),
        ], true),
        c @ ("sum" | "min" | "max" | "avg") => help_block(ctx, c, "reduce a numeric column of a record stream (record-pipe stage)", &[
            ("<records> | sum <col>", "total / min / max / mean of a numeric column", "status | sum mem"),
            ("… | avg <col>", "a non-numeric or missing column is loud, never a silent 0", "status | avg queue"),
        ], true),
        _ => return false,
    }
    true
}

/// `<util> <sub> help` - focused help for a subcommand. Returns false if not a subcommand.
fn sub_help(ctx: &ServiceContext, util: &str, sub: &str) -> bool {
    match (util, sub) {
        ("date", "epoch") => help_block(ctx, "date epoch", "seconds since 1970-01-01", &[
            ("date epoch", "print epoch seconds (not POSIX 'unix')", "date epoch"),
        ], false),
        ("date", "sync") => help_block(ctx, "date sync", "sync the clock from the internet NOW", &[
            ("date sync", "the clock already syncs itself once the network is up, and re-tries about once a minute while it is unset; this asks for it immediately instead of waiting (q aborts)", "date sync"),
        ], false),
        ("net", "dns") => help_block(ctx, "net dns", "resolve a hostname to an IPv4 address", &[
            ("net dns <host>", "DNS A-record lookup via net-stack (slirp resolver)", "net dns example.com"),
        ], false),
        ("net", "arp") => help_block(ctx, "net arp", "resolve one host's MAC by ARP", &[
            ("net arp <ip>", "broadcast a who-has and print the responder's MAC", "net arp 192.168.4.1"),
        ], false),
        ("net", "scan") => help_block(ctx, "net scan", "ARP-sweep the local /24 for live hosts", &[
            ("net scan", "list every host on your /24 that answers ARP", "net scan"),
        ], false),
        ("net", "renew") => help_block(ctx, "net renew", "reconfigure the network without a reboot", &[
            ("net renew", "re-run DHCP + ARP (recover a link that came up after boot)", "net renew"),
        ], false),
        ("observe", "now") => help_block(ctx, "observe now", "one-shot metrics frame", &[
            ("observe now", "print a single metrics frame and return", "observe now"),
        ], false),
        ("write", "append") => help_block(ctx, "write append", "append to a file (create if missing)", &[
            ("write append <path> <text>", "add <text> to the end of <path>", "write append /log started"),
        ], false),
        ("write", "prepend") => help_block(ctx, "write prepend", "prepend to a file (create if missing)", &[
            ("write prepend <path> <text>", "add <text> to the front of <path> (rewrites the file)", "write prepend /log \"# header\""),
        ], false),
        ("match", "except") => help_block(ctx, "match except", "keep the lines that do NOT match", &[
            ("match except <pattern> [path]", "drop matching lines, keep the rest", "read /log | match except debug"),
        ], false),
        ("sort", "reverse") => help_block(ctx, "sort reverse", "order the lines descending", &[
            ("sort reverse [path]", "sort Z→A / high→low", "read /names.txt | sort reverse"),
        ], false),
        ("drives", "flash") => help_block(ctx, "drives flash", "format a drive as GSFS (ERASES it; asks y/N)", &[
            ("drives flash", "format the only drive, no label", "drives flash"),
            ("drives flash <label>", "format + name it", "drives flash data"),
            ("drives flash <drive> <label>", "format drive <drive>, name it", "drives flash 0 data"),
        ], false),
        ("drives", "label") => help_block(ctx, "drives label", "name / rename a drive", &[
            ("drives label <name>", "name the only drive", "drives label archive"),
            ("drives label <drive> <name>", "name drive <drive>", "drives label 0 archive"),
        ], false),
        ("drives", "reset") => help_block(ctx, "drives reset", "un-format a drive back to raw (ERASES; asks y/N)", &[
            ("drives reset", "un-format the only drive", "drives reset"),
            ("drives reset <drive>", "un-format drive <drive>", "drives reset 0"),
        ], false),
        ("drives", "check") => help_block(ctx, "drives check", "fsck: verify integrity + rebuild the bitmap/free count (does NOT erase)", &[
            ("drives check", "check the only drive", "drives check"),
            ("drives check <drive>", "check drive <drive>", "drives check 0"),
        ], false),
        ("drives", "scrub") => help_block(ctx, "drives scrub", "read-only integrity sweep: verify every block's CRC, report (changes nothing; run periodically)", &[
            ("drives scrub", "scrub the only drive", "drives scrub"),
            ("drives scrub <drive>", "scrub drive <drive>", "drives scrub 0"),
        ], false),
        _ => return false,
    }
    true
}

/// One rendered line of `help`, as static data so the pager can index it (and the
/// whole table lives in rodata, not on the shell's tight stack - §26.6). `Sec`/`Text`
/// are full-width lines; `Row` is the aligned "  command  description" form.
enum HelpRow {
    Gap,
    Sec(&'static str),
    Text(&'static str),
    Row(&'static str, &'static str),
}
use HelpRow::*;
static HELP: &[HelpRow] = &[
    Gap,
    Sec("Console"),
    Row("help", "show this message"),
    Row("<prefix> Tab", "complete a command; if several match, press the shown digit to pick"),
    Row("arrows/Home/End/Del", "edit the line in place; Up/Down recall history; Esc clears"),
    Row("clear", "clear the screen"),
    Row("echo <text>", "print text"),
    Row("result", "the last command's result (Ok / Err)"),
    Row("run <script> [save <out>]", "run a script (.gsh); `save` writes the report to a file"),
    Row("selfcheck [save <out>]", "run the built-in self-check suite; `save` writes the report"),
    Row("fcap", "file-as-capability self-check (diagnostic; fcap help)"),
    Row("assert ok|fails <cmd>", "verify success/failure (also: … | assert contains X)"),
    Gap,
    Sec("System"),
    Row("about", "identity: version + arch, cores, credits"),
    Row("version", "GodspeedOS version + architecture + build stamp"),
    Row("cores", "CPU core count"),
    Row("mem", "physical memory usage"),
    Row("date [epoch]", "date + time; 'epoch' = secs since 1970"),
    Row("uptime", "how long the system has been up (records when piped)"),
    Row("wait <seconds>", "pause N seconds, q aborts (paces scripts - watch is built on it)"),
    Row("whatis <name>", "what a name is: built-in / library script / pipe stage / service"),
    Row("net", "network status: IP, gateway, ping"),
    Row("ping", "continuous ICMP echo (q quits): ping 8.8.8.8"),
    Gap,
    Sec("Services"),
    Row("status", "list all live tasks"),
    Row("observe [now]", "live view (q to quit) / one-shot frame"),
    Row("caps [service]", "capabilities (default: this shell)"),
    Row("roster", "example record service (a typed table; try roster | where role=core)"),
    Row("spawn <svc>[,svc,...]", "start a service or a comma-list"),
    Row("kill <svc>[,svc,...] | all-services", "stop a service, a comma-list, or every service"),
    Row("restart <name>[,name,...] [core]", "restart a service or a comma-list"),
    Gap,
    Sec("Storage"),
    Row("drives [flash|label|reset|check]", "manage attached disks (drives help)"),
    Row("ls [path]", "list a directory"),
    Row("cd [path|-]", "change directory (- = previous)"),
    Row("read <path>", "print a file"),
    Row("write [append|prepend] <path>", "create/overwrite/append/prepend (also: <prod> | write …)"),
    Row("edit <path>", "full-screen text editor (^S save, ^Q quit)"),
    Row("mkdir <path>[,path,...] [parents]", "create a directory or a comma-list"),
    Row("copy <src> <dst> [recursive]", "copy a file or subtree"),
    Row("move <src> <dst>", "relocate a file/dir"),
    Row("rename <path> <name>", "rename an entry in place"),
    Row("delete <path>[,path,...] [recursive]", "remove a file/dir/subtree or a comma-list"),
    Row("find <pattern> [path]", "search by name (substring or *? glob)"),
    Row("tree [path]", "print the directory hierarchy"),
    Row("match <pattern> [path]", "keep lines matching (also: <prod> | match)"),
    Row("count [path]", "count lines/words/bytes (also: <prod> | count)"),
    Row("sort [reverse] [path]", "order lines (also: <prod> | sort)"),
    Row("first / last [N] [path]", "keep first/last N lines (also: <prod> |)"),
    Gap,
    Sec("Pipes"),
    Row("<producer> | [filter |…] <sink>", "compose stages (Appendix D)"),
    Row("  e.g. read /f | upper", "filter a file through a service"),
    Row("  e.g. tree / | write /out", "capture output to a file"),
    Row("  e.g. greet | upper | write /g", "producer | filter | sink"),
    Gap,
    Sec("Records (typed pipes - docs/records.md)"),
    Row("status | where mem>0", "filter the task table by field (=,!=,>,<,~)"),
    Row("status | select name state", "keep only some columns"),
    Row("status | sort [reverse] mem", "order rows by a column"),
    Row("status | to json | to yaml", "render the table (default: a grid)"),
    Gap,
    Sec("Power"),
    Row("reboot", "hardware reset"),
    Row("chaos kill-storm <svc> [n]", "bounded resilience test: kill a service N times, verify it recovers"),
    Gap,
    Sec("Library (gsh scripts, baked in - type the name to run)"),
    Row("health", "one-shot health dashboard (cores, mem, uptime, net, drives)"),
    Row("watch <command>", "re-run a command every 2s until q (watch mem)"),
    Row("size [path]", "total bytes of the files under a tree (size /docs)"),
    Row("online", "probe the network live: DNS + internet, ok/FAIL per layer"),
    Row("busiest [column]", "service table ranked by mem (or restarts / queue)"),
    Gap,
    Text("Type '<command> help' for usage + examples, '<command> version' for the version."),
];

/// Render help line `idx` (0 = the versioned header, then `HELP[idx-1]`). When `clear_eol`
/// the line ends with `ESC[K` (erase to end of line) before the newline - the pager repaints
/// each row in place over the old frame, so a shorter line must wipe the longer one's tail.
fn help_render_line(ctx: &ServiceContext, idx: usize, clear_eol: bool) {
    let eol = if clear_eol { "\x1b[K" } else { "" };
    if idx == 0 {
        // Rule 6 (0_conventions.md): help output's first line is `<util> <version>`.
        ctx.console_write_fmt(format_args!("help {} - GodspeedOS shell commands", UTIL_VERSION));
    } else {
        match &HELP[idx - 1] {
            Gap => {}
            Sec(s) | Text(s) => ctx.console_write(s),
            // One "  command  description" row, left-justified to a fixed width so the
            // description columns line up (ASCII-only - renders the same on TV and serial).
            Row(cmd, desc) => ctx.console_write_fmt(format_args!("  {:<21}  {}", cmd, desc)),
        }
    }
    ctx.console_write(eol);
    ctx.console_write("\n");
}

fn cmd_help(ctx: &ServiceContext, depth: u8) -> Result<(), ShellError> {
    let total = HELP.len() + 1; // +1 for the header line
    // Page only for a direct interactive `help` (depth 0). When help is run from a
    // script, `assert`, or `selfcheck` (depth > 0) there is no human to press keys -
    // the pager would block the run - so just dump it. The framebuffer console has no
    // scrollback, so an interactive help longer than the screen scrolls its top off
    // forever; page it then (a serial terminal has its own scrollback, but paging there
    // is harmless and consistent). rows==0 means geometry is unknown → just print it.
    let (rows, _cols) = ctx.console_dims();
    let rows = rows as usize;
    // UNKNOWN GEOMETRY IS NOT "NO TERMINAL". A failed `console_dims` returns 0, and this treated that
    // as a reason to dump sixty lines past the top of the screen - the pager silently disappearing
    // because a lookup missed. `edit` handles the same zero by assuming 24 rows and carrying on; this
    // now does the same, so a future failure degrades instead of removing a feature.
    //
    // `depth > 0` stays a real reason to skip: nested help is being rendered into someone else's
    // output (a pipe, `help | write`), where a pager would be wrong rather than merely unhelpful.
    let rows = if rows == 0 { 24 } else { rows };
    if depth > 0 || total <= rows {
        for i in 0..total { help_render_line(ctx, i, false); }
        return Ok(());
    }
    help_pager(ctx, total, rows);
    Ok(())
}

/// Render the full `help` reference as plain text to `out` - the pipe-producer path
/// (`help | write /help.txt`). Mirrors `help_render_line`'s content but with no pager, no cursor
/// escapes, and no `ESC[K`: just the categorised command list, capturable to a file.
fn help_to_out(ctx: &ServiceContext, out: &mut Out) {
    out.line_fmt(ctx, format_args!("help {} - GodspeedOS shell commands", UTIL_VERSION));
    for row in HELP {
        match row {
            Gap => out.line(ctx, ""),
            Sec(s) | Text(s) => out.line(ctx, s),
            Row(cmd, desc) => out.line_fmt(ctx, format_args!("  {:<21}  {}", cmd, desc)),
        }
    }
}

/// `less`-style pager for `help`: render a screenful from `top`, a status line, then
/// read a key and scroll. Space / PageDown page; Up/Down (or j/k) move a line; b /
/// PageUp page back; g/G jump to top/bottom; q / Esc / Enter quit.
///
/// Repaint is done **in place** to avoid the flicker and cost of a full clear: the cursor
/// is hidden for the session (`ESC[?25l`) so the bulk redraw skips the per-character cursor
/// toggle, each frame homes (`ESC[H`) instead of clearing to black, every row erases its own
/// tail (`ESC[K`), and `ESC[J` wipes anything below the status line on a short last page.
/// This is the same write-only repaint the fast boot-time scroll uses, so scrolling is smooth
/// rather than a black flash + full reprint. Bounded: at most `total` lines, clamped each step.
fn help_pager(ctx: &ServiceContext, total: usize, rows: usize) {
    line_pager(ctx, total, rows, &|_| 0, &|c, i| help_render_line(c, i, true), &|_| {});
}

/// The pager, over ANY indexable set of lines.
///
/// This was `help`-shaped: it called `help_render_line` directly, so the one screenful-at-a-time
/// reader in the system could only ever read `help`. `trace ipc` needs exactly the same thing and
/// there is no reason for a second copy of it, so the caller now supplies how to render line `i`.
/// Everything else - the in-place repaint, the key handling, the clamping - is unchanged.
fn line_pager(ctx: &ServiceContext, total: usize, rows: usize,
              pinned: &dyn Fn(&ServiceContext) -> usize,
              render: &dyn Fn(&ServiceContext, usize),
              end_frame: &dyn Fn(&ServiceContext)) {
    // A PINNED region, repainted at the top of every frame and never scrolled.
    //
    // The pager homes to row 1 and paints over whatever was there, so anything printed BEFORE it is
    // gone the moment the first frame lands - which is exactly what happened to `trace ipc`'s legend
    // on a framebuffer console: printed, then immediately overwritten, and invisible. A table's column
    // header has the same problem one page in, for the same reason: it was line 0 of the scrolling
    // region, so page 2 lost the column names.
    //
    // Both belong in a region the pager owns and repaints. `pinned` returns how many lines it drew, so
    // the scrolling area sizes itself; `help` pins nothing and returns 0.
    let page = rows.saturating_sub(1).max(1); // leave one row for the status line
    let mut top = 0usize;
    ctx.console_write("\x1b[?25l"); // hide the cursor for the whole pager session
    loop {
        ctx.console_write("\x1b[H"); // home - repaint over the old frame, no clear-to-black
        let pin_lines = pinned(ctx);
        let page = page.saturating_sub(pin_lines).max(1);
        let max_top = total.saturating_sub(page);
        let end = (top + page).min(total);
        if top > max_top { top = max_top; }
        for i in top..end { render(ctx, i); }
        // Status line (no trailing newline so it parks at the bottom). Scroll keys lead,
        // since holding Up/Down scrolls smoothly (typematic auto-repeat). ESC[J after it
        // wipes any rows left over from a taller previous frame (e.g. the short last page).
        // Everything the frame buffered goes out before the status line, so the status line is last
        // on screen as well as last in the code.
        end_frame(ctx);
        ctx.console_write_fmt(format_args!(
            // SAY WHAT ACTUALLY WORKS. `j`/`k`, `b` and Enter were all handled and none of them were
            // mentioned - a reader who tries `j` because it is muscle memory finds it works, which
            // means the line was under-reporting the tool rather than describing it.
            "[ lines {}-{} of {} ]  up/down or j/k: scroll  space: page down  b: page up  g/G: top/end  q: quit",
            top + 1, end, total));
        ctx.console_write("\x1b[J");
        // Read one command key (arrows/PageUp/Down arrive as escape sequences).
        let mut down = 0i64; // signed line delta to apply; isize via i64 to allow page jumps
        let mut quit = false;
        let mut to_top = false;
        let mut to_bottom = false;
        match ctx.console_read() {
            b' ' | b'f' => down = page as i64,
            b'b' => down = -(page as i64),
            b'j' | b'\r' | b'\n' => down = 1,
            b'k' => down = -1,
            b'g' => to_top = true,
            b'G' => to_bottom = true,
            b'q' | 0x03 => quit = true,
            0x1B => match read_escape_byte(ctx) {
                None => quit = true, // bare ESC quits
                Some(b'[') | Some(b'O') => match pager_csi(ctx) {
                    PagerKey::LineDown => down = 1,
                    PagerKey::LineUp => down = -1,
                    PagerKey::PageDown => down = page as i64,
                    PagerKey::PageUp => down = -(page as i64),
                    PagerKey::Top => to_top = true,
                    PagerKey::Bottom => to_bottom = true,
                    PagerKey::Other => {}
                },
                Some(_) => {}
            },
            _ => {}
        }
        if quit { break; }
        let page = page.saturating_sub(pin_lines).max(1);
        let max_top = total.saturating_sub(page);
        if to_top { top = 0; }
        else if to_bottom { top = max_top; }
        else {
            let nt = top as i64 + down;
            top = nt.clamp(0, max_top as i64) as usize;
        }
    }
    // Restore the cursor and leave a clean screen; the prompt comes from the main loop.
    ctx.console_write("\x1b[?25h\x1b[2J\x1b[H");
}

/// Keys the pager recognises from a terminal escape sequence.
enum PagerKey { LineUp, LineDown, PageUp, PageDown, Top, Bottom, Other }

/// Parse the body of an escape sequence (after `ESC [` or `ESC O`) into a `PagerKey`.
/// Mirrors `handle_csi`'s reader but maps to scrolling: arrows, Home/End, PageUp/Down.
fn pager_csi(ctx: &ServiceContext) -> PagerKey {
    const CSI_MAX: usize = 8;
    let mut param: u16 = 0;
    let mut final_byte = 0u8;
    for _ in 0..CSI_MAX {
        let c = ctx.console_read();
        if c.is_ascii_digit() { param = param.saturating_mul(10).saturating_add((c - b'0') as u16); }
        else if c == b';' { continue; }
        else { final_byte = c; break; }
    }
    match final_byte {
        b'A' => PagerKey::LineUp,
        b'B' => PagerKey::LineDown,
        b'H' => PagerKey::Top,    // Home
        b'F' => PagerKey::Bottom, // End
        b'~' => match param {
            1 | 7 => PagerKey::Top,    // Home
            4 | 8 => PagerKey::Bottom, // End
            5 => PagerKey::PageUp,
            6 => PagerKey::PageDown,
            _ => PagerKey::Other,
        },
        _ => PagerKey::Other,
    }
}

/// Clear the screen. Emits ANSI erase-display + cursor-home: the framebuffer
/// console honours `ESC[2J` (clear + home) and `ESC[H`, and a serial terminal
/// does too, so both surfaces clear. The shell loop reprints the prompt after.
fn cmd_clear(ctx: &ServiceContext) -> Result<(), ShellError> {
    ctx.console_write("\x1b[2J\x1b[H");
    Ok(())
}

/// Print the rest of the line verbatim.
/// Max bytes read by `input` (one console line). Bounded (§26.6); chars past this are dropped.
const INPUT_MAX: usize = 256;

/// Read one console line into `buf` (until Enter). Printable chars are echoed UNLESS `secret`
/// (invisible entry, like `sudo`). Backspace erases the last char (and un-echoes it for a visible
/// line). Returns bytes read. Blocks for a real user - `input` is interactive (docs/scripting.md §8).
fn read_input_line(ctx: &ServiceContext, secret: bool, buf: &mut [u8]) -> usize {
    let mut len = 0usize;
    loop {
        let c = ctx.console_read();
        match c {
            b'\r' | b'\n' => { ctx.console_write("\r\n"); break; }
            0x7f | 0x08 => { if len > 0 { len -= 1; if !secret { ctx.console_write("\x08 \x08"); } } }
            b if (0x20..0x7f).contains(&b) => {
                if len < buf.len() {
                    buf[len] = b; len += 1;
                    if !secret { let one = [b]; if let Ok(t) = core::str::from_utf8(&one) { ctx.console_write(t); } }
                }
            }
            _ => {} // ignore control / escape bytes
        }
    }
    len
}

/// `input [secret] "prompt"` - print the prompt to the CONSOLE, read one line, emit it to `out`
/// (captured by `$( )`, or piped). `secret` = invisible entry; the captured value is tainted at the
/// `let`/reassign site. Only the typed value goes to `out`, so `$(input …)` captures the reply, not
/// the prompt.
fn cmd_input(ctx: &ServiceContext, prompt: &str, out: &mut Out, secret: bool) -> Result<(), ShellError> {
    let p = strip_quotes(prompt.trim());
    if !p.is_empty() { ctx.console_write(p); }
    let mut buf = [0u8; INPUT_MAX];
    let n = read_input_line(ctx, secret, &mut buf);
    out.put_bytes(ctx, &buf[..n]);
    Ok(())
}

/// Parse `input [secret [sealed]] "prompt"` and read one console line into `out`. `sealed` is a
/// reserved escalation (docs/scripting.md §8); until its consumer exists it is treated as `secret`.
fn run_input(ctx: &ServiceContext, arg: &str, out: &mut Out) {
    let a = arg.trim();
    let (first, rest) = split_first(a);
    let (secret, prompt) = if first == "secret" {
        let (second, rest2) = split_first(rest);
        if second == "sealed" {
            ctx.console_writeln("input: 'sealed' is reserved (treated as 'secret' for now)");
            (true, rest2)
        } else { (true, rest) }
    } else { (false, a) };
    let _ = cmd_input(ctx, prompt, out, secret);
}

/// Does a `$( )` capture read a secret (`input secret …`)? Its value is tainted.
fn capture_is_secret(inner: &str) -> bool {
    let (first, rest) = split_first(inner.trim());
    first == "input" && split_first(rest).0 == "secret"
}

/// Does `text` reference a secret-tainted variable via `$name`? Single-quoted `$` is literal (no
/// expansion), so it does not count. Used to refuse echoing a secret and to propagate the taint
/// across an assignment (§8).
fn refs_secret(text: &str, vars: &Vars) -> bool {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut quote = 0u8;
    while i < b.len() {
        let c = b[i];
        if c == b'\'' { quote = if quote == b'\'' { 0 } else if quote == 0 { b'\'' } else { quote }; i += 1; continue; }
        if c == b'"' { quote = if quote == b'"' { 0 } else if quote == 0 { b'"' } else { quote }; i += 1; continue; }
        if c == b'$' && quote != b'\'' {
            let s = i + 1;
            let mut j = s;
            while j < b.len() && (b[j] == b'_' || b[j].is_ascii_alphanumeric()) { j += 1; }
            if j > s && vars.is_secret_name(&b[s..j]) { return true; }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

fn cmd_echo(ctx: &ServiceContext, text: &str, out: &mut Out) -> Result<(), ShellError> {
    out.line(ctx, text);
    Ok(())
}

/// One-line identity for the system. A pipe source (`about | write /about.txt`): renders through
/// `Out`, so it captures to a file as readily as it prints.
fn cmd_about(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    out.line(ctx, "GodspeedOS: a capability-based microkernel");
    out.line(ctx, "  Small enough to understand. Rigorous enough to trust.");
    // Same facts as `version` (and the same ARCH const), so a single `about` gives the whole identity -
    // what it is, which build, which machine. `version` stays the RAW fact for piping; this is prose.
    out.line_fmt(ctx, format_args!("  Version {} {} ({})",
                                   env!("CARGO_PKG_VERSION"), ARCH, env!("GODSPEED_GIT_SHA")));
    out.line_fmt(ctx, format_args!("  Running on {} core(s).", ctx.inspect_core_count()));
    out.line(ctx, "  Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.");
    Ok(())
}

/// The architecture this build targets, named the way the project names it (`docs/multi-arch.md`)
/// rather than in Rust's `target_arch` spelling - notably **arm32** for 32-bit ARMv7, which is what the
/// Raspberry Pi 2 port is called everywhere else in the tree (`docs/arm32-status.md`,
/// `kernel/src/arch/arm/`). One source tree now builds for several ISAs, so a version string without
/// the architecture cannot say which machine produced it - the same reason `uname -m` exists.
const ARCH: &str = if cfg!(target_arch = "x86_64") { "x86_64" }
    else if cfg!(target_arch = "arm") { "arm32" }
    else if cfg!(target_arch = "aarch64") { "aarch64" }
    else if cfg!(target_arch = "riscv64") { "riscv64" }
    else if cfg!(target_arch = "riscv32") { "riscv32" }
    else if cfg!(target_arch = "loongarch64") { "loongarch64" }
    else if cfg!(target_arch = "s390x") { "s390x" }
    else { "unknown-arch" };

/// `version` - the GodspeedOS version, architecture, and build stamp:
/// `GodspeedOS <ver> <arch> (<git-sha>)`. Distinct from `<util> version` (which reports a single
/// utility's version); this is the whole system's version fact (conventions rule 7 - a raw fact).
/// Pipeable like `about` (`version | write /ver.txt`). The SHA is stamped at build time by `build.rs`;
/// a build with no git reports `unknown`. The architecture is reported because the same source builds
/// for several ISAs, so a serial log or bug report must say which one it came from.
fn cmd_version_os(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    out.line_fmt(ctx, format_args!("GodspeedOS {} {} ({})",
                                   env!("CARGO_PKG_VERSION"), ARCH, env!("GODSPEED_GIT_SHA")));
    Ok(())
}

/// The record-pipe-ONLY stage verbs: names that run inside a pipe and nowhere else. `whatis` reports
/// them as their own kind ("why does typing `where` bare fail?" is a real confusion this dissolves).
/// The dual commands (sort/match/count/first/last run bare on files too) are NOT here - bare, they
/// are built-ins. Keep in sync with pipe_run's stage dispatch.
const PIPE_ONLY_VERBS: &[&str] = &["where", "select", "to", "from", "sum", "min", "max", "avg"];

/// Service names `whatis` recognises when no live task bears the name: the managed set plus the
/// demo/on-demand services. Purely descriptive (a lookup miss here means "unknown", not an error in
/// anything) - keep roughly in sync with the supervisor's managed set + the shell's spawn targets.
const KNOWN_SERVICES: &[&str] = &[
    "supervisor", "block-driver", "fs", "logger", "shell", "xhci", "ehci", "nic-driver", "net-stack",
    "ping", "pong", "greet", "roster", "chaos", "observe", "mem-pressure",
];

/// `whatis <name>` - what runs when this name is typed at the prompt: a shell built-in, a library
/// script (gsh, baked into the image), a record-pipe stage, or a standalone service (with live
/// task/core when running - identity is the name; the task/core is just where it lives right now).
/// An unknown name is a loud `Err` (so `assert fails whatis banana` holds). This is the honest
/// replacement for POSIX `which`: there is no $PATH and no executable path to return - a name's
/// truth here is its KIND and ORIGIN, which is also an authority answer (26.9: a built-in runs in
/// the shell's domain, a service in its own). One line, pipeable (rule 12).
///
/// Lens: the answer is about the NAME AT THE PROMPT. `ping` is both a built-in command and a demo
/// service; typing `ping` runs the built-in, so that is the answer. (`whatis version`/`whatis help`
/// cannot be asked: the universal `<util> version|help` intercept answers for whatis itself.)
fn cmd_whatis(ctx: &ServiceContext, name: &str, out: &mut Out) -> Result<(), ShellError> {
    if name.is_empty() {
        ctx.console_writeln("usage: whatis <name>   e.g. whatis ls");
        return Err(ShellError::Unknown);
    }
    if PIPE_ONLY_VERBS.contains(&name) {
        out.line_fmt(ctx, format_args!("{}: record-pipe stage (runs only inside a pipe)", name));
        return Ok(());
    }
    if is_util(name) {
        out.line_fmt(ctx, format_args!("{}: shell built-in", name));
        return Ok(());
    }
    if library_script(name).is_some() {
        out.line_fmt(ctx, format_args!("{}: library script (gsh, baked into the image)", name));
        return Ok(());
    }
    if let Some(slot) = slot_of(ctx, name) {
        let st = ctx.task_stat(slot);
        out.line_fmt(ctx, format_args!("{}: standalone service (running - task {}, core {})", name, slot, st.core));
        return Ok(());
    }
    if KNOWN_SERVICES.contains(&name) {
        out.line_fmt(ctx, format_args!("{}: standalone service (not running)", name));
        return Ok(());
    }
    out.line_fmt(ctx, format_args!("{}: unknown", name));
    Err(ShellError::Unknown)
}

/// Physical-memory usage, straight from the kernel's frame allocator (held via
/// the INTROSPECT cap). Frames are 4 KiB pages: KiB = frames*4, MiB = frames/256.
/// The percentage is computed in hundredths (two decimals, integer math) so the
/// microkernel's tiny footprint shows as e.g. 0.03% rather than rounding to 0%.
fn cmd_mem(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    let total = ctx.inspect_kernel_total_frames();
    let free = ctx.inspect_kernel_free_frames();
    let used = total.saturating_sub(free);
    let pct_h = if total > 0 { used * 10000 / total } else { 0 }; // 0.01% units
    out.line_fmt(ctx, format_args!(
        "mem: {} KiB used / {} MiB total ({}.{:02}% used, {} MiB free)",
        used * 4, total / 256, pct_h / 100, pct_h % 100, free / 256));
    Ok(())
}

fn cmd_reboot(ctx: &ShellCtx) -> ! {
    // SAY IT FIRST, THEN DO IT. Nothing goes between the command and the action.
    //
    // This used to make two round trips before printing anything - `time_floor` to the clock service,
    // then `clock_floor_persist`, which talks to `time` AND `fs` - so a slow or busy dependency turned
    // `reboot` into a command that sat there silently. Right after a chaos storm, when those services
    // are restarting, that is exactly when someone wants to reboot and exactly when it looks dead.
    //
    // The work is also redundant now: `time` owns the clock floor and persists it at the moment the
    // clock is SET, so there is nothing for the shell to write on the way down. Two owners for one
    // piece of state was the older bug; this is the second half of removing it.
    //
    // What remains is what the user asked for: a line, then the reset. If the SoC fails to reset, the
    // kernel's reset path says so on the console - it is bounded and reports rather than hanging
    // silently.
    ctx.console_writeln("rebooting...");
    ctx.reboot()
}

/// `cores` - how many cores are up. `cores ticks` - each core's SCHEDULER-QUANTUM rate.
///
/// The counter (`CORE_TOTAL_TICKS`) advances on a timer tick **and** on every `yield`, so for an
/// idle core it reads as the timer rate, while a busy-polling service reads as its loop rate. That
/// conflation is a feature here: it is exactly what exposed xhci pegging a core at ~85k/s on the
/// T630 while the truly idle cores sat at 0.
///
/// The rate form is how the Phase 2a idle-tick slowdown (`docs/power.md` §14) is *measured* rather
/// than assumed: an idle AP re-arms its timer at a long interval, so it should read far lower than
/// the BSP, which deliberately keeps the normal period as the timed-wake/console heartbeat. A core
/// running a busy-poll driver (e.g. `ehci`) never idles, so it reads at the full rate - which makes
/// this a direct, side-by-side read of what still costs power.
///
/// Paced by the RTC (`epoch_secs_monotonic`), never the TSC: this hardware's TSC-Hz calibration is
/// unreliable, so a cycle-based interval would report a confidently wrong rate. It `sleep`s between
/// polls rather than spinning, so the measurement does not perturb what it is measuring.
fn cmd_cores(ctx: &ServiceContext, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let n = ctx.inspect_core_count();
    if arg != "ticks" {
        out.line_fmt(ctx, format_args!("cores: {}", n));
        return Ok(());
    }

    const MAXC: usize = 16;
    const SAMPLE_SECS: i64 = 5;
    let ncores = (n as usize).min(MAXC);
    let mut before = [0u64; MAXC];
    for c in 0..ncores {
        before[c] = ctx.inspect_core_total_ticks(c as u32);
    }
    let t0 = ctx.epoch_secs_monotonic();
    out.line_fmt(ctx, format_args!("sampling {}s (RTC-paced)...", SAMPLE_SECS));
    while ctx.epoch_secs_monotonic() - t0 < SAMPLE_SECS {
        ctx.sleep(1); // granularity is one scheduler quantum; parks instead of spinning
    }
    let elapsed = (ctx.epoch_secs_monotonic() - t0).max(1) as u64;

    // Show the raw sampled count alongside the rate: a slowed idle core can tick below 1/s, which
    // integer division would flatten to a bare "0" and hide the very signal being measured.
    out.line(ctx, "core  quanta/s  sampled   (quantum = timer tick OR yield)");
    for c in 0..ncores {
        let delta = ctx
            .inspect_core_total_ticks(c as u32)
            .saturating_sub(before[c]);
        out.line_fmt(
            ctx,
            format_args!(
                "  C{}   {:>5}   {:>5}{}",
                c,
                delta / elapsed,
                delta,
                if c == 0 { "   (BSP - keeps the normal period)" } else { "" }
            ),
        );
    }
    Ok(())
}

/// Where the last-known-good time is recorded. Deliberately a plain visible file, not a hidden one: it is
/// a fact about this machine an operator may want to read or delete (§26.4 - keep the mechanism visible).
const CLOCK_FLOOR_PATH: &[u8] = b"/clock.last";
/// Budget for the floor's disk I/O. The floor is best-effort by design, so a slow or still-mounting `fs`
/// must cost a shrug, never a wedge: an UNBOUNDED request here would hang the boot prompt before the
/// first `gsh>` (the exact hazard the history loader documents) and hang `date` with no q escape.
const CLOCK_FS_SECS: i64 = 2;

/// Render a duration the way a person reads one ("4m", "2h", "3d") for the clock's freshness note.
struct HumanSecs(i64);
impl core::fmt::Display for HumanSecs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = self.0.max(0);
        if s < 60 { write!(f, "{}s", s) }
        else if s < 3_600 { write!(f, "{}m", s / 60) }
        else if s < 86_400 { write!(f, "{}h", s / 3_600) }
        else { write!(f, "{}d", s / 86_400) }
    }
}

/// A tiny fixed-size formatting buffer for the floor file (decimal epoch seconds). §26.6.1: use
/// `format_args!` rather than hand-rolling digits; the bound is the array, not a heap.
struct EpochBuf { buf: [u8; 24], len: usize }
impl core::fmt::Write for EpochBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let n = b.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
        Ok(())
    }
}

/// Record "the machine was running at this time" to disk, and raise the kernel's floor to match. Called at
/// EXPLICIT moments (a successful `date sync`, and just before `reboot`) - never as a side effect of
/// displaying the time. `quiet` suppresses the console note for the reboot path, where the machine is
/// about to reset and a failure to record a floor is not worth a line in the operator's face.
///
/// Every verdict here is checked. The kernel can refuse the floor (implausible, or no cap at all - on x86
/// the shell is never granted it), and `fs` answers a write with a status byte that can say FS_ERR or
/// "no filesystem". Treating "a reply arrived" as "it worked" is how a refused privileged operation gets
/// reported to the operator as done (§26.7, invariant 12).
fn clock_floor_persist(ctx: &ShellCtx, epoch: u32, quiet: bool) -> bool {
    if !time_floor_set(ctx, epoch as i64) {
        if !quiet { ctx.console_writeln("date: the time service refused the clock floor - not recorded"); }
        return false;
    }
    let mut b = EpochBuf { buf: [0u8; 24], len: 0 };
    let _ = core::fmt::write(&mut b, format_args!("{}", epoch));
    match fs_request_bounded(ctx, OP_WRITE_FILE, CLOCK_FLOOR_PATH, &b.buf[..b.len], CLOCK_FS_SECS) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => true,
        _ => {
            // No disk, no filesystem, or a write that failed: the floor simply will not survive this power
            // cycle. That is the honest degraded state (next boot knows nothing), not a silent success.
            if !quiet { ctx.console_writeln("date: could not record the clock floor (no filesystem?)"); }
            false
        }
    }
}

/// Read a whole small file into `dst`, returning the byte count, or `None` if it is absent / unreadable /
/// `fs` is not serving. **The ONE place that parses an OP_READ_FILE reply.**
///
/// `fs` answers `[FS_OK, len:u32 LE, data..]`. Every caller that hand-parses that shape gets three
/// chances to be wrong, and one of them already was: checking the status byte against `1` fails on every
/// SUCCESS (FS_OK is 0; 1 is FS_ERR), and skipping only one byte splices the length prefix into the data.
/// That bug made a feature silently inert on every boot. Parsing it once, here, removes the failure mode
/// for the next caller instead of leaving it lying around (§26.4 - one visible mechanism, not N copies).
fn fs_read_file(ctx: &ShellCtx, path: &[u8], dst: &mut [u8], max_secs: i64) -> Option<usize> {
    let r = fs_request_bounded(ctx, OP_READ_FILE, path, &[], max_secs)?;
    let p = r.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 5 { return None; }   // FS_NOTFOUND / FS_ERR / no filesystem
    let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
    let end = (5 + n).min(p.len());
    let m = (end - 5).min(dst.len());
    dst[..m].copy_from_slice(&p[5..5 + m]);
    Some(m)
}

/// Seed the kernel's clock floor from the last-known time on disk, at startup. The floor is a BOUND, never
/// a reading: it is not shown as the time and no "estimate" is derived from it (a machine powered off for
/// six months would otherwise display a six-month-old timestamp indistinguishable from a measured one).
/// Its whole job is to let the kernel REFUSE a clock value from before we last ran.
fn clock_floor_seed(ctx: &ShellCtx) {
    let mut buf = [0u8; 24];
    let n = match fs_read_file(ctx, CLOCK_FLOOR_PATH, &mut buf, CLOCK_FS_SECS) {
        Some(n) if n > 0 => n,
        _ => return,                   // no record yet / no fs - no floor this boot, which is the honest state
    };
    if let Ok(s) = core::str::from_utf8(&buf[..n]) {
        if let Some(v) = parse_u32(s.trim()) {
            if !time_floor_set(ctx, v as i64) {
                ctx.console_writeln("shell: the time service refused the recorded clock floor - ignoring it");
            }
        }
    }
}

/// Wall-clock date+time. Default renders a full timestamp with weekday AND where the time came from,
/// e.g. `Sat 2026-06-06 22:05:09  (ntp, synced 4m ago)`; `date epoch` prints seconds since 1970-01-01 as a
/// bare pipeable number; `date sync` fetches the time over the network. Deliberately these three forms -
/// no format strings or timezones (§26.2: minimal surface). The subcommand is `epoch`, not `unix`: this is
/// not POSIX, so the vocabulary doesn't borrow its name. Displaying the time NEVER writes to disk - the
/// floor is recorded at explicit moments only (`date sync`, and before `reboot`).
fn cmd_date(ctx: &ShellCtx, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    // `date sync` - fetch the time from the network (SNTP) via net-stack and set the wall clock. The Pi 2
    // has no battery-backed RTC, so `date` reads zeros until this runs (also done automatically at boot).
    if arg == "sync" {
        // Seed the floor HERE, where it is used: it exists to let the kernel refuse a fetched time from
        // before we last ran, so the moment we are about to fetch one is exactly when it must be known.
        // Reading it at boot instead put fs I/O on the startup path at fs's slowest moment - see the note
        // in service_main for what that cost.
        clock_floor_seed(ctx);
        out.line_fmt(ctx, format_args!("Asking the network for the time now (SNTP)... (q aborts)"));
        let msg = Message::from_bytes(&[10u8]);
        // The budget must cover net-stack's WORST case, not a guess: op 10 can run SNTP_TRIES rounds of a
        // DANCE_SECS drain (plus a DNS attempt) before it can honestly answer "no time". Timing out early
        // and RE-SENDING would queue a second full sync behind the first, and net-stack's serve loop is
        // single-threaded - so every other client op (net/ping/dns) would block behind our own retry.
        // `net renew`, the sibling that also triggers the boot dance, uses 30 s for exactly this reason.
        const SYNC_SECS: i64 = 30;
        let outcome = match ctx.request_with_reply_abortable("net-stack", &msg, SYNC_SECS) {
            ReqOutcome::Timeout if ctx.reacquire_by_name("net-stack") =>
                ctx.request_with_reply_abortable("net-stack", &msg, SYNC_SECS),
            other => other,
        };
        // An abort is the USER's decision, not a network failure - blaming the cable for it is a lie.
        if let ReqOutcome::Aborted = outcome {
            out.line_fmt(ctx, format_args!("date sync: aborted"));
            return Ok(());
        }
        let synced = match &outcome {
            ReqOutcome::Reply(r) if r.payload_bytes().first() == Some(&1) && r.payload_bytes().len() >= 5 => {
                let p = r.payload_bytes();
                Some(u32::from_le_bytes([p[1], p[2], p[3], p[4]]))
            }
            _ => None,
        };
        let epoch = match synced {
            Some(e) => e,
            None => {
                out.line_fmt(ctx, format_args!("date sync: no time from the network (is the cable in?)"));
                return Ok(());
            }
        };
        // The floor is NOT recorded here. `net-stack` hands the epoch to `time`, and `time` persists
        // its own floor at the moment the clock is set - it owns the clock, so it owns the clock's
        // state (§3.8). The shell writing it as well was a second owner for one piece of state, and a
        // second owner is how the two drift.
        let _ = epoch;
        // fall through to display the freshly-set time
    }
    let dt = Datetime::from_epoch_secs(time_now(ctx).unwrap_or(0));
    let source = time_source(ctx);
    if arg == "epoch" {
        // Raw fact, pipeable (conventions rule 7): the number only, no provenance decoration.
        out.line_fmt(ctx, format_args!("{}", dt.epoch_secs()));
        return Ok(());
    }
    if source == ClockSource::Unset {
        // Say we do not know, rather than printing a number we cannot stand behind. If a floor was
        // recorded, report it AS A FLOOR - "we ran at least this late" is true; "it is now that" is not.
        // SAY WHY, not just "unknown". The three sources are tried in order - a hardware clock, the
        // network, then the floor on disk - so naming the one that is missing is the difference between
        // a user waiting for something that will happen and a user waiting for something that will not.
        // Unknown counts as NOT linked here: this only decides whether to promise the clock will
        // arrive, and promising it on a link we could not confirm is the worse error.
        let linked = net_link_up(ctx) == Some(true);
        out.line_fmt(ctx, format_args!(
            "the clock is not set: this board has no RTC, so the time can only come from the network"));
        if linked {
            // Precise about WHEN, because the retry is request-driven: net-stack re-asks at most once a
            // minute, and only while it is handling a network request. On a machine nobody is using it
            // does not fire on its own - net-stack deliberately has no idle tick (an earlier one stole
            // client messages; see the note at its serve loop). So `date sync` is the reliable "now".
            out.line_fmt(ctx, format_args!(
                "        the network is up; it re-tries on network activity, or 'date sync' asks now"));
        } else {
            out.line_fmt(ctx, format_args!(
                "        no network link - plug the cable in and it will set itself, or run 'date sync'"));
        }
        if let Some(f) = time_floor(ctx) {
            let fd = Datetime::from_epoch_secs(f);
            out.line_fmt(ctx, format_args!(
                "        (last known {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC from {} - a floor, not a reading)",
                fd.year, fd.month, fd.day, fd.hour, fd.minute, fd.second,
                core::str::from_utf8(CLOCK_FLOOR_PATH).unwrap_or("disk")));
        }
        return Ok(());
    }
    let wd = WEEKDAYS[(dt.weekday() as usize) % 7];
    // The time, WHICH SCALE it is on, and where it came from.
    //
    // The scale is only stated when it is actually known. NTP serves UTC by definition, so a
    // network-set clock is labelled `UTC` - without it the reading is ambiguous and a reader in any
    // other zone silently reads it as local and concludes the clock is wrong by their offset. A
    // hardware RTC carries NO such guarantee (firmware may keep it in local time or in UTC, and
    // nothing here can tell which), so it gets no scale label rather than a guessed one - the same
    // rule as the rest of this command: say what is known, never invent the rest.
    // A FLOOR-DERIVED CLOCK IS NOT AN RTC, and used to be labelled as one. On a board with no RTC the
    // time can be carried over from `/clock.last` and then advanced correctly, which makes it a genuine
    // reading and a LOWER BOUND: it cannot know how long the machine was powered off, so it is behind by
    // exactly that unknown amount. Calling it `rtc` claims hardware this board does not have; calling it
    // unset denies a time being displayed. It gets its own words.
    match (source, time_synced_secs_ago(ctx)) {
        (_, Some(ago)) => out.line_fmt(ctx, format_args!(
            "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC  (ntp, synced {} ago)",
            wd, dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second, HumanSecs(ago))),
        (ClockSource::Floor, None) => out.line_fmt(ctx, format_args!(
            "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC  (carried over from the last boot - AT LEAST this late; 'date sync' for the true time)",
            wd, dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second)),
        (_, None) => out.line_fmt(ctx, format_args!(
            "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02}  (rtc, scale unknown)",
            wd, dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second)),
    }
    Ok(())
}

/// `net` - network status + DNS, brokered from the `net-stack` service (utilities/40_net.md). Dispatches
/// `net` (status) vs `net dns <host>` (resolve a hostname). A pipe PRODUCER: `net | write /f`.
/// Parse "a.b.c.d" into 4 octets (no_std, no allocation). None if not a well-formed IPv4 literal.
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0usize;
    for part in s.split('.') {
        if n >= 4 || part.is_empty() || part.len() > 3 { return None; }
        let mut v: u32 = 0;
        for b in part.bytes() {
            if !b.is_ascii_digit() { return None; }
            v = v * 10 + (b - b'0') as u32;
        }
        if v > 255 { return None; }
        out[n] = v as u8;
        n += 1;
    }
    if n == 4 { Some(out) } else { None }
}

/// Pace continuous `ping` ~1 s by the WALL CLOCK (RTC seconds), returning early with `true` on q/Q/ESC.
/// The TSC is unreliable for this on some hardware: the AMD T630's CPUID-based TSC calibration is wrong,
/// so a TSC interval collapsed to ~0 and the ping FLOODED (200 lines in a blink). The RTC second is
/// portable and never floods - it just waits for the wall-clock second to tick over.
fn ping_wait_or_quit(ctx: &ServiceContext) -> bool {
    // Deglitched monotonic seconds, not the raw RTC: a single CMOS misread (the T630's "4383d" glitch)
    // would otherwise skip or stall a pace interval.
    let start = ctx.epoch_secs_monotonic();
    loop {
        if let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' || b == 0x1b { return true; }
        }
        if ctx.epoch_secs_monotonic() != start { return false; }   // wall-clock second ticked (~1 s)
        ctx.yield_cpu();
    }
}

/// Longest `wait` accepted, in seconds. Refused loudly past this (bounded behaviour, §26.6) - a
/// pacing pause is seconds-to-minutes; an hour-plus wait at the prompt is almost certainly a typo.
const WAIT_MAX_SECS: u32 = 3600;

/// `wait <seconds>` - do nothing for N wall-clock seconds; `q`/`Q`/`Esc` aborts (returns `Err`, so a
/// script can `if !wait 2 { break }`). The pacing primitive the library's `watch` loop is built on
/// (generalising `ping_wait_or_quit` above): RTC-monotonic seconds, never the TSC (miscalibrated for
/// wall-time on the T630). This is a USER-COMMANDED delay - the user (or their script) chose the
/// cadence and holds the q escape - not a service coordinating with a peer, so Commandment VIII
/// (wait on truth, not time) is not in play; services must still never pace dependencies this way.
fn cmd_wait(ctx: &ServiceContext, arg: &str) -> Result<(), ShellError> {
    let secs = match parse_u32(arg) {
        Some(s) if (1..=WAIT_MAX_SECS).contains(&s) => s as i64,
        Some(_) => {
            ctx.console_writeln_fmt(format_args!("wait: 1..{} seconds (a longer wait is a typo, not a plan)", WAIT_MAX_SECS));
            return Err(ShellError::Unknown);
        }
        None => { ctx.console_writeln("usage: wait <seconds>   (q aborts)   e.g. wait 2"); return Err(ShellError::Unknown); }
    };
    // Whole-second granularity: waits until the monotonic epoch has advanced by `secs` (so `wait 1`
    // ends at the next second boundary - up to a second early, never late).
    let start = ctx.epoch_secs_monotonic();
    loop {
        if let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' || b == 0x1b { return Err(ShellError::Unknown); } // aborted
        }
        if ctx.epoch_secs_monotonic() - start >= secs { return Ok(()); }
        ctx.yield_cpu();
    }
}

/// `ping [bytes N] [count N] <ip>` - a Windows-style continuous ICMP echo to a raw IPv4, via net-stack.
/// One `Reply from ...` line per echo (round-trip time + TTL), `q` quits, then a statistics summary.
/// `count N` sends N and stops; `bytes N` sets the ICMP data size (default 32). No DNS - raw IP only.
fn cmd_ping(ctx: &ServiceContext, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let usage = "usage: ping [bytes N] [count N] <ip>   e.g. ping 8.8.8.8   ping bytes 64 192.168.4.1   (q quits)";
    let mut bytes: usize = 32;
    let mut count: Option<u32> = None;
    let mut ip_str: &str = "";
    let mut toks = arg.split_whitespace();
    while let Some(t) = toks.next() {
        match t {
            "bytes" | "size" => match toks.next().and_then(|s| s.parse::<usize>().ok()) {
                Some(v) => bytes = v,
                None => { ctx.console_writeln(usage); return Ok(()); }
            },
            "count" | "n" => match toks.next().and_then(|s| s.parse::<u32>().ok()) {
                Some(v) => count = Some(v),
                None => { ctx.console_writeln(usage); return Ok(()); }
            },
            other => ip_str = other,
        }
    }
    if ip_str.is_empty() { ctx.console_writeln(usage); return Ok(()); }
    let ip = match parse_ipv4(ip_str) {
        Some(ip) => ip,
        None => { out.line_fmt(ctx, format_args!("ping: '{}' is not an IPv4 address - try a raw IP like 8.8.8.8 (names need DNS)", ip_str)); return Ok(()); }
    };
    let b = bytes.min(1024);                          // matches net-stack's PING_MAX_PAYLOAD
    let bl = (b as u16).to_le_bytes();
    let msg = Message::from_bytes(&[3, ip[0], ip[1], ip[2], ip[3], bl[0], bl[1]]);
    // Continuous mode shows the q hint up front so it is obvious BEFORE the replies start scrolling.
    if count.is_none() {
        out.line_fmt(ctx, format_args!("Pinging {}.{}.{}.{} with {} bytes of data (press q to quit):", ip[0], ip[1], ip[2], ip[3], b));
    } else {
        out.line_fmt(ctx, format_args!("Pinging {}.{}.{}.{} with {} bytes of data:", ip[0], ip[1], ip[2], ip[3], b));
    }

    let mut sent = 0u32; let mut recv = 0u32;
    let mut rmin = u16::MAX; let mut rmax = 0u16; let mut rsum = 0u64; let mut vcount = 0u32;
    while count.map_or(true, |c| sent < c) {
        sent += 1;
        // ABORTABLE per echo, so q quits DURING the wait for a reply, not only in the pace between echoes
        // (a blocking request_with_reply here left q feeling unresponsive). Reacquire once on a timeout.
        let outcome = match ctx.request_with_reply_abortable("net-stack", &msg, 5) {
            ReqOutcome::Timeout if ctx.reacquire_by_name("net-stack") => ctx.request_with_reply_abortable("net-stack", &msg, 5),
            other => other,
        };
        match outcome {
            ReqOutcome::Reply(r) => {
                let p = r.payload_bytes();
                if p.first() == Some(&1) && p.len() >= 4 {
                    let rtt = u16::from_le_bytes([p[1], p[2]]);   // MICROSECONDS (net-stack reports us now)
                    let ttl = p[3];
                    recv += 1;
                    // us under a millisecond (LAN), ms.d above it (WAN). 0 = below the clock's resolution.
                    if rtt == 0 {
                        out.line_fmt(ctx, format_args!("Reply from {}.{}.{}.{}: bytes={} time<1us TTL={}", ip[0], ip[1], ip[2], ip[3], b, ttl));
                    } else if rtt < 1000 {
                        out.line_fmt(ctx, format_args!("Reply from {}.{}.{}.{}: bytes={} time={}us TTL={}", ip[0], ip[1], ip[2], ip[3], b, rtt, ttl));
                    } else {
                        out.line_fmt(ctx, format_args!("Reply from {}.{}.{}.{}: bytes={} time={}ms TTL={}", ip[0], ip[1], ip[2], ip[3], b, (rtt as u32 + 500) / 1000, ttl));
                    }
                    if rtt < rmin { rmin = rtt; }
                    if rtt > rmax { rmax = rtt; }
                    rsum += rtt as u64; vcount += 1;
                } else if p.first() == Some(&2) {
                    // "LINK NOT CONFIRMED", not "no link". The distinction is the whole difference
                    // between a hedge and a false statement: "cable unplugged?" is a QUESTION and
                    // stays, because it is a fair thing to suggest; "no link" was an ASSERTION, and
                    // it was made on a machine whose cable was plainly in and whose link was up.
                    //
                    // What the stack actually establishes to send this code is that it could not
                    // CONFIRM a link - which a genuinely unplugged cable and a driver that did not
                    // answer inside its deadline produce identically, and this layer cannot tell
                    // them apart. So it names both and asserts neither.

                    // net-stack reports the NIC link is down - keep pinging at the same cadence so it is
                    // clearly still trying, and it resumes the moment the cable is back.
                    out.line_fmt(ctx, format_args!("No reply from {}.{}.{}.{}: link not confirmed - cable unplugged, or the NIC driver did not answer", ip[0], ip[1], ip[2], ip[3]));
                } else {
                    out.line_fmt(ctx, format_args!("Request timed out."));
                }
            }
            ReqOutcome::Aborted => { sent = sent.saturating_sub(1); break; }   // q pressed mid-echo
            // Do NOT quit on a slow/unavailable net-stack - a continuous ping has no end. Print and keep
            // going; the user quits with q. (With the shorter ping budget this is now rare.)
            ReqOutcome::Timeout => { out.line_fmt(ctx, format_args!("No reply from {}.{}.{}.{}: net-stack not responding", ip[0], ip[1], ip[2], ip[3])); }
        }
        if count.map_or(false, |c| sent >= c) { break; }   // last echo done: no trailing interval
        if ping_wait_or_quit(ctx) { break; }                // ~1 s pace (RTC), q/ESC quits
    }

    let lost = sent.saturating_sub(recv);
    let loss = if sent > 0 { lost * 100 / sent } else { 0 };
    out.line_fmt(ctx, format_args!(""));
    out.line_fmt(ctx, format_args!("Ping statistics for {}.{}.{}.{}:", ip[0], ip[1], ip[2], ip[3]));
    out.line_fmt(ctx, format_args!("    Packets: Sent = {}, Received = {}, Lost = {} ({}% loss)", sent, recv, lost, loss));
    if vcount > 0 {
        let avg = rsum / vcount as u64;
        // Same unit for the whole summary, chosen by the average (a session's replies cluster together):
        // us for a LAN-scale ping, integer ms for a WAN-scale one.
        if avg < 1000 {
            out.line_fmt(ctx, format_args!("Approximate round trip times in microseconds:"));
            out.line_fmt(ctx, format_args!("    Minimum = {}us, Maximum = {}us, Average = {}us", rmin, rmax, avg));
        } else {
            out.line_fmt(ctx, format_args!("Approximate round trip times in milliseconds:"));
            out.line_fmt(ctx, format_args!("    Minimum = {}ms, Maximum = {}ms, Average = {}ms",
                (rmin as u64 + 500) / 1000, (rmax as u64 + 500) / 1000, (avg + 500) / 1000));
        }
    } else if recv > 0 {
        out.line_fmt(ctx, format_args!("    (round-trip time unavailable - this host's TSC clock is uncalibrated)"));
    }
    // Result model: any reply proves the path works (Ok); NO reply - whether all echoes were lost or
    // the probe was q-aborted before one arrived - is a failed probe (Err), so `if ping count 2 ...`
    // (the library's `online`) and `assert fails ping ...` see the truth and an aborted probe never
    // reads as a false success (audit U4). The stats above stay the human diagnosis.
    if recv == 0 { Err(ShellError::Unknown) } else { Ok(()) }
}

/// `net stats` - dump the NIC's raw registers (chip state) to the console. Queries nic-driver ([5]);
/// the reply is chip-tagged (0 = RTL8168, 1 = e1000). Reads only - shows CR (RE/TE), config, ring
/// bases, and each RX descriptor's OWN/len, so you can see whether the receiver is even enabled and
/// whether frames are sitting in the ring.
fn net_stats_dump(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    let req = Message::from_bytes(&[5u8]);
    let reply = match net_query(ctx, "nic-driver", &req, 3) {
        NetQ::Reply(r) => r,
        NetQ::Aborted => { ctx.console_writeln("net: aborted"); return Ok(()); }
        NetQ::Timeout => { ctx.console_writeln("net: nic-driver did not answer the register dump"); return Ok(()); }
    };
    let p = reply.payload_bytes();
    if p.first() == Some(&0) && p.len() >= 43 {
        let cr = p[1]; let c9346 = p[2]; let phy = p[3]; let rx_idx = p[4];
        let imr = u16::from_le_bytes([p[5], p[6]]);
        let isr = u16::from_le_bytes([p[7], p[8]]);
        let rms = u16::from_le_bytes([p[9], p[10]]);
        let rcr = u32::from_le_bytes([p[11], p[12], p[13], p[14]]);
        let tcr = u32::from_le_bytes([p[15], p[16], p[17], p[18]]);
        let tnpds = u32::from_le_bytes([p[19], p[20], p[21], p[22]]);
        let rdsar = u32::from_le_bytes([p[23], p[24], p[25], p[26]]);
        let spd = if phy & 0x10 != 0 { "1000M" } else if phy & 0x08 != 0 { "100M" }
                  else if phy & 0x04 != 0 { "10M" } else { "?" };
        out.line(ctx, "NIC registers (RTL8168):");
        out.line_fmt(ctx, format_args!("  CR        0x{:02x}   RE={} TE={} RST={}", cr, (cr>>3)&1, (cr>>2)&1, (cr>>4)&1));
        out.line_fmt(ctx, format_args!("  9346CR    0x{:02x}   {}", c9346, if c9346 == 0xC0 { "unlocked" } else { "locked" }));
        out.line_fmt(ctx, format_args!("  PHYSTATUS 0x{:02x}   link={} spd={} dup={}", phy, (phy>>1)&1, spd, if phy&1!=0 {"full"} else {"half"}));
        out.line_fmt(ctx, format_args!("  IMR       0x{:04x}", imr));
        out.line_fmt(ctx, format_args!("  ISR       0x{:04x}", isr));
        out.line_fmt(ctx, format_args!("  RMS       0x{:04x}   ({} bytes)", rms, rms));
        out.line_fmt(ctx, format_args!("  RCR       0x{:08x}   AAP={} APM={} AM={} AB={}", rcr, rcr&1, (rcr>>1)&1, (rcr>>2)&1, (rcr>>3)&1));
        out.line_fmt(ctx, format_args!("  TCR       0x{:08x}", tcr));
        out.line_fmt(ctx, format_args!("  TNPDS.lo  0x{:08x}   TX ring base", tnpds));
        out.line_fmt(ctx, format_args!("  RDSAR.lo  0x{:08x}   RX ring base", rdsar));
        out.line_fmt(ctx, format_args!("  RX ring (rx_idx={}):", rx_idx));
        // HOW MANY DESCRIPTORS THE DRIVER ACTUALLY SENT, not four. The reply carries one 4-byte word
        // per RX descriptor after a 27-byte header, and the ring grew from 4 to 8 - so a fixed four
        // here would silently show half the ring, which is the quiet half of the same bug that
        // panicked the driver on the sending side.
        for i in 0..(p.len() - 27) / 4 {
            let o = 27 + i * 4;
            let d = u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]);
            out.line_fmt(ctx, format_args!("    [{}] opts1=0x{:08x}  OWN={} len={}", i, d, (d>>31)&1, d & 0x3FFF));
        }
    } else if p.first() == Some(&1) && p.len() >= 25 {
        let g = |o: usize| u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]);
        out.line(ctx, "NIC registers (Intel e1000):");
        out.line_fmt(ctx, format_args!("  CTRL   0x{:08x}", g(1)));
        out.line_fmt(ctx, format_args!("  STATUS 0x{:08x}   LU={}", g(5), (g(5)>>1)&1));
        out.line_fmt(ctx, format_args!("  RCTL   0x{:08x}   EN={}", g(9), (g(9)>>1)&1));
        out.line_fmt(ctx, format_args!("  TCTL   0x{:08x}", g(13)));
        out.line_fmt(ctx, format_args!("  RDH    0x{:08x}", g(17)));
        out.line_fmt(ctx, format_args!("  RDT    0x{:08x}", g(21)));
    } else {
        ctx.console_writeln("net: no register dump available for this NIC");
    }
    Ok(())
}

fn cmd_net(ctx: &ServiceContext, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let arg = arg.trim();
    if arg == "dns" {
        ctx.console_writeln("net: usage: net dns <hostname>  (e.g. net dns example.com)");
        return Err(ShellError::Unknown);
    }
    if let Some(host) = arg.strip_prefix("dns ") {
        let host = host.trim();
        if host.is_empty() {
            ctx.console_writeln("net: usage: net dns <hostname>  (e.g. net dns example.com)");
            return Err(ShellError::Unknown);
        }
        return net_dns(ctx, host, out);
    }
    if arg == "stats" {
        return net_stats_dump(ctx, out);
    }
    if arg == "arp" {
        ctx.console_writeln("net: usage: net arp <ip>   (e.g. net arp 192.168.4.1)");
        return Err(ShellError::Unknown);
    }
    if let Some(ips) = arg.strip_prefix("arp ") {
        return net_arp(ctx, ips.trim(), out);
    }
    if arg == "scan" {
        return net_scan(ctx, out);
    }
    if arg == "renew" {
        return net_renew(ctx, out);
    }
    if arg == "lease" {
        return net_lease(ctx, out);
    }
    if !arg.is_empty() {
        ctx.console_writeln("net: unknown subcommand - try net, net dns <host>, net stats, net arp <ip>, net scan, net renew, or net help");
        return Err(ShellError::Unknown);
    }
    net_status(ctx, out)
}

/// `net renew` - re-run net-stack's DHCP/ARP/ICMP dance (op 8) so a link that came up AFTER boot (a
/// cable plugged in later) reconfigures the stack without a reboot. Bounded + abortable with q.
fn net_renew(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    out.line_fmt(ctx, format_args!("renewing (DHCP + ARP + ping the gateway, press q to abort)"));
    let req = Message::from_bytes(&[8u8]);
    let outcome = match ctx.request_with_reply_abortable("net-stack", &req, 30) {
        ReqOutcome::Timeout if ctx.reacquire_by_name("net-stack") => ctx.request_with_reply_abortable("net-stack", &req, 30),
        other => other,
    };
    match outcome {
        ReqOutcome::Reply(r) => {
            let p = r.payload_bytes();
            // status: our_ip(4) gateway(4) gw_mac(6) flags(1) dns(4); flags bit 0 = gateway resolved.
            if p.len() >= 15 && (p[14] & 1) != 0 {
                out.line_fmt(ctx, format_args!("network up - {}.{}.{}.{}, gateway {}.{}.{}.{}{}",
                    p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7],
                    if p[14] & 2 != 0 { " (ping ok)" } else { "" }));
            } else if p.len() >= 15 {
                out.line_fmt(ctx, format_args!("still no gateway - is the cable in and the link up?"));
            } else {
                out.line_fmt(ctx, format_args!("net: net-stack gave a short reply"));
            }
        }
        ReqOutcome::Aborted => out.line_fmt(ctx, format_args!("net renew: aborted")),
        ReqOutcome::Timeout => out.line_fmt(ctx, format_args!("net: net-stack unavailable")),
    }
    Ok(())
}

/// `net arp <ip>` - resolve one host's hardware address by ARP (net-stack op 6).
fn net_arp(ctx: &ServiceContext, ip_str: &str, out: &mut Out) -> Result<(), ShellError> {
    let ip = match parse_ipv4(ip_str) {
        Some(ip) => ip,
        None => { out.line_fmt(ctx, format_args!("net arp: '{}' is not an IPv4 address", ip_str)); return Ok(()); }
    };
    out.line_fmt(ctx, format_args!("resolving {}.{}.{}.{} (press q to abort)", ip[0], ip[1], ip[2], ip[3]));
    let req = Message::from_bytes(&[6, ip[0], ip[1], ip[2], ip[3]]);
    // ABORTABLE (q). Reacquire once on a clean timeout (net-stack may have restarted).
    let outcome = match ctx.request_with_reply_abortable("net-stack", &req, 8) {
        ReqOutcome::Timeout if ctx.reacquire_by_name("net-stack") => ctx.request_with_reply_abortable("net-stack", &req, 8),
        other => other,
    };
    match outcome {
        ReqOutcome::Reply(r) => {
            let p = r.payload_bytes();
            if p.first() == Some(&1) && p.len() >= 7 {
                out.line_fmt(ctx, format_args!("{}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    ip[0], ip[1], ip[2], ip[3], p[1], p[2], p[3], p[4], p[5], p[6]));
            } else {
                out.line_fmt(ctx, format_args!("{}.{}.{}.{}: no ARP reply (not on this subnet, or down)", ip[0], ip[1], ip[2], ip[3]));
            }
        }
        ReqOutcome::Aborted => out.line_fmt(ctx, format_args!("net arp: aborted")),
        ReqOutcome::Timeout => out.line_fmt(ctx, format_args!("net: net-stack did not answer the ARP query")),
    }
    Ok(())
}

/// `net scan` - ARP-sweep the local /24 (derived from our own IP) and list the hosts that answer.
/// ARP-based, so it is fast and LAN-reliable. net-stack does the whole sweep in one op (op 7) and
/// returns a 32-byte up-bitmap - one round trip per host, not a per-host poll from the shell.
fn net_scan(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    // Reacquire net-stack by name on a clean miss/timeout - it may have come up late (its boot dance
    // stalls ~26s on a dead link) and not be in our cap cache yet, exactly as net_arp does. Without this,
    // running `net scan` before any other net command reported a bogus "net-stack unavailable".
    let status0 = Message::from_bytes(&[0u8]);
    let our0 = match ctx.request_with_reply_abortable("net-stack", &status0, 5) {
        ReqOutcome::Timeout if ctx.reacquire_by_name("net-stack") => ctx.request_with_reply_abortable("net-stack", &status0, 5),
        other => other,
    };
    let our = match our0 {
        ReqOutcome::Reply(r) => { let p = r.payload_bytes(); if p.len() >= 4 { [p[0], p[1], p[2], p[3]] } else { [0u8; 4] } }
        ReqOutcome::Aborted  => { out.line_fmt(ctx, format_args!("net scan: aborted")); return Ok(()); }
        ReqOutcome::Timeout  => { out.line_fmt(ctx, format_args!("net: net-stack unavailable")); return Ok(()); }
    };
    out.line_fmt(ctx, format_args!("Scanning {}.{}.{}.0/24 for live hosts (press q to abort):", our[0], our[1], our[2]));
    // Walk the /24 host-by-host (net-stack op 6 = one ARP resolve), driven FROM THE SHELL so an abort
    // actually STOPS the work: q ends the loop and net-stack is only ever mid-ONE resolve (fast), never
    // wedged finishing a 254-host sweep (which is why a batch op 7 left the NEXT command stuck). Each
    // host's wait is itself abortable, so q lands instantly, and responders print as they are found.
    let mut found = 0u32;
    for x in 1..=254u16 {
        let req = Message::from_bytes(&[6, our[0], our[1], our[2], x as u8]);
        match ctx.request_with_reply_abortable("net-stack", &req, 3) {
            ReqOutcome::Reply(r) => {
                let p = r.payload_bytes();
                if p.first() == Some(&1) && p.len() >= 7 {
                    out.line_fmt(ctx, format_args!("  {}.{}.{}.{}", our[0], our[1], our[2], x));
                    found += 1;
                }
            }
            ReqOutcome::Aborted => { out.line_fmt(ctx, format_args!("net scan: aborted ({} found)", found)); return Ok(()); }
            ReqOutcome::Timeout => {}   // this host did not answer in time; move on
        }
    }
    out.line_fmt(ctx, format_args!("{} host(s) responded.", found));
    Ok(())
}

/// `net dns <host>` - resolve a hostname to an IPv4 address. net-stack sends the DNS query to slirp's
/// resolver; DNS depends on the host's own resolver, so "no answer" is a legitimate result, not a bug.
fn net_dns(ctx: &ServiceContext, host: &str, out: &mut Out) -> Result<(), ShellError> {
    // Request byte 0 = 1 (DNS), then the hostname. net-stack replies 5 bytes: [ok, ip0, ip1, ip2, ip3].
    let hb = host.as_bytes();
    if hb.len() > 255 {
        ctx.console_writeln("net: hostname too long");
        return Err(ShellError::Unknown);
    }
    let mut req = [0u8; 256];
    req[0] = 1;
    req[1..1 + hb.len()].copy_from_slice(hb);
    let msg = Message::from_bytes(&req[..1 + hb.len()]);
    // A DNS resolve waits on the server, which can take a moment. Route it through net_query (not a
    // blocking send) so it is ABORTABLE: net_query polls q each round and advertises "press q to abort"
    // if the reply does not come in the first second - so a slow or wedged resolve is escapable, not a
    // silent hang.
    ctx.console_writeln("net: resolving ...");
    let reply = match net_query(ctx, "net-stack", &msg, 8) {
        NetQ::Reply(r)   => r,
        // A q-aborted resolve did NOT succeed, so it is Err (not Ok): a probe's Result is its verdict,
        // and `online`'s `if net dns ...` must not print a false "dns ok" for an aborted probe (audit U4).
        NetQ::Aborted    => { ctx.console_writeln("net: aborted"); return Err(ShellError::Unknown); }
        NetQ::Timeout    => { ctx.console_writeln("net: net-stack did not answer the resolve"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if p.len() >= 5 && p[0] == 1 {
        out.line_fmt(ctx, format_args!("{} is {}.{}.{}.{}", host, p[1], p[2], p[3], p[4]));
        Ok(())
    } else if p.first() == Some(&2) {
        out.line_fmt(ctx, format_args!("{}: the DNS server replied but returned no A record", host));
        // A resolve that did not resolve is an Err OUTCOME (the printed line stays the diagnosis).
        // "No answer" is still a legitimate result, not a bug - but on the Result model it is a
        // failed probe, so `if net dns example.com { ... }` and `assert fails ...` can see the truth
        // (the library's `online` verdicts ride on exactly this).
        Err(ShellError::Unknown)
    } else {
        // Diagnostic: how many frames net-stack collected while waiting, and how many were UDP. Tells
        // us "no reply arrived" (0 UDP) from "a reply arrived but did not match our port" (UDP > 0).
        let (fr, ud) = if p.len() >= 7 { (p[5], p[6]) } else { (0, 0) };
        let to = if p.len() >= 8 { p[7] } else { 0 };
        // NAME THE LAYER THAT IS MISSING, not the one that stayed quiet. A lookup made with no lease
        // has no gateway to send through, so nothing was ever put on the wire - and blaming the DNS
        // server for not answering a question it never received points the reader at the network when
        // the answer is on this machine. Only when there is no such reason do we report what we saw.
        match net_unconfigured_reason(ctx) {
            Some(why) => out.line_fmt(ctx, format_args!("{}: not resolved - {}", host, why)),
            None => out.line_fmt(ctx, format_args!(
                "{}: no reply from the DNS server ({} frames, {} UDP, {} timeouts)", host, fr, ud, to)),
        }
        Err(ShellError::Unknown)
    }
}

/// `net` (bare) - the network status: IP, gateway (+MAC), and whether the gateway pings. Raw facts,
/// no verdict (utilities/0_conventions.md rule 7).
/// The outcome of a `net` query that a keypress can interrupt.
enum NetQ { Reply(Message), Timeout, Aborted }

/// A bounded request to `peer` that a `q`/`Q`/ESC keypress ABORTS - so a slow or stuck `net` can always
/// be escaped back to the prompt. Sends the (idempotent) query once per second, checking the console for
/// an abort key between tries, up to `max_secs`. Returns the reply, a timeout, or Aborted. (Safe under
/// the piped shell-test: it waits for the prompt between commands, so no input is pending during `net`.)
fn net_query(ctx: &ServiceContext, peer: &str, msg: &Message, max_secs: i64) -> NetQ {
    // Drain any STALE reply left in our endpoint by a PRIOR command before we send ours - otherwise the
    // request_with_reply below reads that leftover as if it were our answer. A q-aborted continuous `ping`
    // leaves its last net-stack reply (a 4-byte [alive,rtt,ttl]) here; without this drain the next `net`
    // reads it and prints a bogus DNS / "gave a short reply". Same class as the `net scan -> 0.0.0.0` bug;
    // the abortable request variants already drain, but net_query (a deadline loop) did not.
    while ctx.try_recv().is_some() {}
    for i in 0..=max_secs {
        while let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' || b == 0x1b { return NetQ::Aborted; }
        }
        if let Some(r) = ctx.request_with_reply_deadline(peer, msg, 1) { return NetQ::Reply(r); }
        // Only tell the user about q if the reply DIDN'T come in the first second (a stall) - so a fast
        // query stays clean, but a wedged one advertises how to escape it.
        if i == 0 { ctx.console_writeln("net: waiting for a reply - press q to abort"); }
        let _ = ctx.reacquire_by_name(peer);   // best-effort: the caller retries regardless
    }
    NetQ::Timeout
}

/// Is the ethernet link up right now? Asked of `nic-driver` (byte 7 of its `[3]` status), which reads
/// the PHY rather than a cached boot-time flag.
///
/// Used only to explain an UNSET clock: "the network is up and being asked" and "there is no cable" are
/// different situations for the user, and one of them is not going to resolve itself. A short deadline
/// and a pessimistic default - if nic-driver does not answer we do not claim a link.
/// Why a network request could not possibly have worked, or `None` if no such reason is known.
///
/// This exists because "no reply from the DNS server" was being printed for lookups the DNS server
/// never heard. On a stack with no lease there is no gateway and no route, so the query goes nowhere -
/// and the honest counters said so plainly, `0 frames, 0 UDP, 0 timeouts`, which is what "we never
/// sent anything anybody could answer" looks like. Reporting that as a silent remote host accuses the
/// wrong party and sends the reader looking at the network instead of at their own configuration.
///
/// `None` means "no better explanation available", NOT "everything is fine": if net-stack does not
/// answer the status query, or answers something too short to read, we do not know why the lookup
/// failed and must not invent a reason. The caller falls back to reporting exactly what it observed.
///
/// The order is the order the layers come up in, so the FIRST missing one is named rather than the
/// last: a machine with no cable is told about the cable, not about its gateway.
/// Ask net-stack for its status, reacquiring the peer once if the cached cap has gone stale.
///
/// §14.3 puts this obligation on the CLIENT: a service that restarted issues a new endpoint, and the
/// cap we were holding is dead. `net_link_up` and `console_dims` already do this; the lease query did
/// not, and the difference was visible - net-stack restarted three times under chaos (adopted at
/// slots 16, 17, then 18), after which this shell reported "no lease" while net-stack was resolving
/// DNS and SNTP perfectly well over the lease it still held. The network was fine; the question was
/// being asked down a dead cap.
fn net_status_reply(ctx: &ServiceContext) -> Option<Message> {
    let req = Message::from_bytes(&[0u8]);
    if let Some(r) = ctx.request_with_reply_deadline("net-stack", &req, 3) {
        return Some(r);
    }
    if ctx.reacquire_by_name("net-stack") {
        return ctx.request_with_reply_deadline("net-stack", &req, 3);
    }
    None
}

fn net_unconfigured_reason(ctx: &ServiceContext) -> Option<&'static str> {
    match net_link_up(ctx) {
        Some(true) => {}
        Some(false) => return Some("no link (cable unplugged?)"),
        // Say what actually happened. Blaming the cable here is how a driver problem spent a whole
        // session looking like a hardware one.
        None => return Some("cannot reach nic-driver (no answer) - link state unknown"),
    }
    let r = net_status_reply(ctx)?;
    let p = r.payload_bytes();
    if p.len() < 15 {
        return None;                      // cannot read the status - say nothing rather than guess
    }
    // Flag byte: bit 0 = gateway resolved by ARP, bit 2 = DHCP granted this address.
    if p[14] & 4 == 0 {
        return Some("no address yet - DHCP has not completed");
    }
    if p[14] & 1 == 0 {
        return Some("no route - the gateway has not answered ARP");
    }
    None
}

/// Link state as the DRIVER reports it, or `None` when the driver could not be reached.
///
/// THREE outcomes, not two. This used to return `bool` and fold "the driver did not answer" into
/// `false`, which the caller then printed as "no link (cable unplugged?)" - a confident diagnosis of
/// the one thing it had NOT established, with the cable plugged in the whole time. Reported from a
/// Pi 2, where every ping said the cable was out until a chaos run restarted things.
///
/// The two causes need opposite responses (check the cable, versus the driver is unreachable) and
/// §26.7 forbids a failed query from quietly becoming an answer, so they stay distinguishable here.
fn net_link_up(ctx: &ServiceContext) -> Option<bool> {
    let req = Message::from_bytes(&[3u8]);
    // Reacquire and retry ONCE before giving up (§14.3). A cap to a service that has since restarted
    // is stale, and nothing refreshes it on our behalf: the request just fails, for as long as this
    // shell lives. That is consistent with what the Pi 2 showed - net-stack saw the link come up and
    // auto-configured while the shell, asking the same driver a second later, got nothing.
    if let Some(r) = ctx.request_with_reply_deadline("nic-driver", &req, 2) {
        return read_link(r.payload_bytes());
    }
    if ctx.reacquire_by_name("nic-driver") {
        if let Some(r) = ctx.request_with_reply_deadline("nic-driver", &req, 2) {
            return read_link(r.payload_bytes());
        }
    }
    None
}

/// Read a `[3]` status reply: `[ok, mac(6), link]`.
///
/// HONOUR THE OK BYTE. The driver replies with an all-zero buffer when its own query to the device
/// failed - `ok = 0` AND `link = 0` - and reading only the link byte turns "I could not find out"
/// into "the cable is out". That is how ping came to report an unplugged cable on a Pi 2 with the
/// cable plainly in: the shell was faithfully relaying a zero that never meant what it read as.
///
/// The protocol already carries the distinction; nothing here needed inventing, only reading.
fn read_link(p: &[u8]) -> Option<bool> {
    if p.len() < 8 || p[0] == 0 {
        return None;                    // driver could not determine it - not the same as "down"
    }
    Some(p[7] != 0)
}

/// `net lease` - ONE WORD: `ok`, `none`, or nothing at all if net-stack does not answer.
///
/// Exists for `selfcheck`. The suite can compare a whole line against a word list (`if $line in ok`)
/// but has no substring test, so a status block full of formatted prose is not something it can assert
/// on without inventing grammar - which is what the first version of that check did.
///
/// Three outcomes, deliberately distinct:
/// - `ok`   - DHCP granted the address, OR there is no link so there is nothing to lease
/// - `none` - the link is up and we are on the fallback address, which routes nowhere
/// - (silence) - net-stack did not reply, so the caller can retry rather than conclude anything
fn net_lease(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    match net_link_up(ctx) {
        Some(true) => {}
        Some(false) => {
            out.line(ctx, "ok");        // no cable: nothing to lease, and not a fault
            return Ok(());
        }
        // NOT "ok". We did not establish there is no link, so reporting the healthy no-cable answer
        // would hide an unreachable driver behind a pass (§26.7).
        None => {
            out.line(ctx, "cannot reach nic-driver (no answer) - link state unknown");
            return Ok(());
        }
    }
    match net_status_reply(ctx) {
        Some(r) => {
            let p = r.payload_bytes();
            // bit 2 of the flag byte = DHCP granted this address (net-stack publishes it).
            out.line(ctx, if p.len() >= 15 && p[14] & 4 != 0 { "ok" } else { "none" });
            Ok(())
        }
        // SILENT on no reply, on purpose. net-stack blocks its serve loop for the length of a DHCP
        // dance, and a machine whose link has just come up is legitimately in one - saying `none`
        // there would report a fault that is not there. The caller retries; if it never answers, the
        // caller's own bound is what fails, and that failure is real.
        None => Ok(()),
    }
}

fn net_status(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    // Diagnostic FIRST (independent of net-stack, so it shows even if net-stack is down): the NIC the
    // KERNEL discovered - vendor:device and which register BAR it mapped. This is which chip nic-driver
    // should be driving (Phase 4).
    let vd = ctx.nic_vendor_device();
    let chip = if vd == 0x8168_10EC { "RTL8168" } else if vd == 0x100E_8086 { "e1000" }
               else if vd == 0 { "none" } else { "unknown" };
    out.line_fmt(ctx, format_args!(
        "nic      {:04x}:{:04x}  mmio {:#x}  ({})", vd & 0xFFFF, vd >> 16, ctx.nic_mmio_base(), chip));

    // Query nic-driver directly (the shell holds ACQUIRE_ANY) for its MAC + link/TX/RX - proves whether
    // MMIO reaches the NIC (Phase 4). Abortable: press q if it stalls.
    let mut nic_link_up = true;   // the LIVE link (p[7]); ties net-stack's gateway/ping lines to reality
    let nreq = Message::from_bytes(&[3u8]);
    match net_query(ctx, "nic-driver", &nreq, 3) {
        NetQ::Aborted => { ctx.console_writeln("net: aborted"); return Ok(()); }
        NetQ::Timeout => {} // no nic diagnostic this time - fall through to the net-stack status
        NetQ::Reply(r) => {
            let p = r.payload_bytes();
            if p.len() >= 7 {
                out.line_fmt(ctx, format_args!(
                    "nic-mac  {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  reset {}",
                    p[1], p[2], p[3], p[4], p[5], p[6],
                    if p[0] == 1 { "ok" } else { "TIMEOUT (MMIO not reaching the chip)" }));
            }
            // Extended status (RTL8168 Stage B, 15 bytes): live link + TX/RX counts, so the TV shows the
            // whole bring-up story without the serial log.
            if p.len() >= 15 {
                nic_link_up = p[7] != 0;   // remember the live link for the net-stack lines below
                let rx_len = u16::from_le_bytes([p[9], p[10]]);
                let tx_cnt = u16::from_le_bytes([p[11], p[12]]);
                let rx_cnt = u16::from_le_bytes([p[13], p[14]]);
                // Speed/duplex from the 32-byte hardware status; a 15-byte (e1000/older) reply omits it.
                let (spd, dup) = if p.len() >= 32 {
                    (match p[15] & 0x03 { 3 => "1000M", 2 => "100M", 1 => "10M", _ => "?" },
                     if p[15] & 0x04 != 0 { "full" } else { "half" })
                } else { ("", "") };
                out.line_fmt(ctx, format_args!(
                    "nic-link {} {} {}  |  tx {} ({} sent)  |  rx {}B ({} recv)",
                    if p[7] != 0 { "UP" } else { "down (no cable/PHY)" }, spd, dup,
                    if p[8] != 0 { "ok" } else { "TIMEOUT" }, tx_cnt, rx_len, rx_cnt));
            }
            // Chip hardware tally counters (RTL8168 DTCCR dump) - Layer-1 GROUND TRUTH: the NIC's OWN
            // cumulative counts, read off silicon regardless of net-stack. RxOk climbing between two
            // `net`s => the receiver is alive; flat => the NIC is not receiving (a Layer-1 fault, not
            // a scheduling one). RxBcast answers "do we receive broadcasts?" directly.
            if p.len() >= 32 {
                let rx_ok  = u32::from_le_bytes([p[16], p[17], p[18], p[19]]);
                let tx_ok  = u32::from_le_bytes([p[20], p[21], p[22], p[23]]);
                let rx_brd = u32::from_le_bytes([p[24], p[25], p[26], p[27]]);
                let rx_er  = u16::from_le_bytes([p[28], p[29]]);
                let miss   = u16::from_le_bytes([p[30], p[31]]);
                out.line_fmt(ctx, format_args!(
                    "nic-hw   RxOk={} TxOk={} RxBcast={} RxErr={} Miss={}",
                    rx_ok, tx_ok, rx_brd, rx_er, miss));
            }
        }
    }

    // net-stack is NOT a wired send-peer, so the first request can miss the cap cache. The shell holds
    // ACQUIRE_ANY, so reacquire by name and retry, then give up loudly (Commandment VIII / IX). The
    // request body is ignored by net-stack - the embedded reply cap IS the ask (§8.2).
    // Abortable, bounded (3s): net-stack can wedge (e.g. on a degraded NIC); press q to escape a stall.
    let req = Message::from_bytes(&[0u8]);
    let reply = match net_query(ctx, "net-stack", &req, 3) {
        NetQ::Reply(r) => r,
        NetQ::Aborted => { ctx.console_writeln("net: aborted"); return Ok(()); }
        NetQ::Timeout => {
            ctx.console_writeln("net: net-stack unavailable (no reply within 3s)");
            return Err(ShellError::Unknown);
        }
    };
    let p = reply.payload_bytes();
    if p.len() < 15 {
        ctx.console_writeln("net: net-stack gave a short reply");
        return Err(ShellError::Unknown);
    }
    // 15-byte record: ip[0..4], gateway ip[4..8], gateway mac[8..14], flags[14] (bit0 gw resolved,
    // bit1 ping ok). Formatting is the shell's job; net-stack reports raw facts.
    // Reflect the LIVE link, not the frozen record. If the cable is out, EVERY net-stack line is degraded -
    // showing the stale (often fallback, e.g. 10.0.2.x) IP/gateway/DNS as if current is the "stale info" bug.
    if nic_link_up {
        let flags = p[14];
        out.line_fmt(ctx, format_args!("ip       {}.{}.{}.{}", p[0], p[1], p[2], p[3]));
        if flags & 1 != 0 {
            out.line_fmt(ctx, format_args!(
                "gateway  {}.{}.{}.{} at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                p[4], p[5], p[6], p[7], p[8], p[9], p[10], p[11], p[12], p[13]));
        } else {
            out.line(ctx, "gateway  unresolved");
        }
        out.line(ctx, if flags & 2 != 0 { "ping     ok" } else { "ping     no" });
        // Whether the address was GRANTED or guessed. A stack that cannot receive gets no offer and
        // falls back, so this line is where a broken receive path becomes visible without sending
        // anything - which is what lets `selfcheck` gate on it.
        out.line(ctx, if flags & 4 != 0 { "lease    ok (DHCP)" }
                      else { "lease    NONE (fallback address - not routable)" });
        if p.len() >= 19 {
            out.line_fmt(ctx, format_args!("dns      {}.{}.{}.{}", p[15], p[16], p[17], p[18]));
        }
    } else {
        out.line(ctx, "ip       unassigned (link down - cable unplugged?)");
        out.line(ctx, "gateway  unresolved");
        out.line(ctx, "ping     no");
        // Printed in BOTH branches, and "n/a" rather than "no" with the cable out. That wording is what
        // lets `selfcheck` gate on a single assertion - `lacks 'lease    no'` - with no conditional:
        // it passes when a lease is held AND when there is no link to get one on, and fails only in the
        // case that is genuinely wrong, a live link with no lease. A machine without a cable is not a
        // failing machine.
        out.line(ctx, "lease    ok (no link - nothing to lease)");
        out.line(ctx, "dns      unresolved");
    }
    Ok(())
}

/// A name-addressed request to net-stack, with the reacquire-on-miss prime (net-stack is not a wired
/// send-peer; the shell holds ACQUIRE_ANY). Mirrors `fs_request`.
fn netstack_request(ctx: &ServiceContext, payload: &[u8]) -> Option<Message> {
    let msg = Message::from_bytes(payload);
    match ctx.request_with_reply("net-stack", &msg) {
        Some(r) => Some(r),
        None => if ctx.reacquire_by_name("net-stack") { ctx.request_with_reply("net-stack", &msg) } else { None },
    }
}

/// Open a UDP socket: net-stack mints a socket cap and grants it to us (mirrors `fc_open`).
fn sock_open(ctx: &ServiceContext) -> Option<CapHandle> {
    let r = netstack_request(ctx, &[2])?;
    if r.payload_bytes().first() == Some(&1) { ctx.take_pending_cap() } else { None }
}

/// Invoke a socket cap - send a datagram through it and receive the response (mirrors `fc_invoke`).
fn sock_invoke(ctx: &ServiceContext, sock: CapHandle, right: u8, payload: &[u8]) -> Option<Message> {
    while ctx.try_recv().is_some() {}   // clear any stale late-reply a prior aborted invoke left behind
    let self_grant = ctx.self_grant_handle()?;
    let reply = ctx.derive_cap(self_grant)?;
    if ctx.resource_invoke(sock, right, reply, &Message::from_bytes(payload)).is_err() {
        ctx.remove_cap(reply);
        return None;
    }
    // Await the reply FAILURE-AWARE (Commandment VIII): a bare `recv` would hang forever if net-stack
    // died after receiving the invocation but before replying. Reclaim the reply slot on every outcome.
    // Same rule as the SDK: on a REPLY the cap is already gone (the send embedded it, and §8.5 removes
    // an embedded cap from the sender's table), so removing it here removes whatever the kernel has
    // since placed in that slot - which is how the file cap was being deleted. Reclaim it only on the
    // paths where the send never delivered it.
    let outcome = ctx.recv_abortable_deadline(FILTER_WAIT_SECS);
    match outcome {
        ReqOutcome::Reply(m) => Some(m),
        _ => { ctx.remove_cap(reply); None }
    }
}

/// Build a minimal DNS A-query for `host` into `buf`; returns the length. Just enough to elicit a UDP
/// response - the `sock` demo reports the round-trip, it does not parse DNS.
fn dns_query_bytes(host: &str, buf: &mut [u8]) -> usize {
    buf[0] = 0x13; buf[1] = 0x37;           // id
    buf[2] = 0x01; buf[3] = 0x00;           // recursion desired
    buf[4] = 0x00; buf[5] = 0x01;           // qdcount = 1
    for b in buf[6..12].iter_mut() { *b = 0; }
    let mut pos = 12;
    for label in host.as_bytes().split(|&b| b == b'.') {
        if label.is_empty() || pos + 1 + label.len() >= buf.len() - 5 { break; }
        buf[pos] = label.len() as u8; pos += 1;
        buf[pos..pos + label.len()].copy_from_slice(label); pos += label.len();
    }
    buf[pos] = 0; pos += 1;                  // qname terminator
    buf[pos] = 0x00; buf[pos + 1] = 0x01;    // QTYPE A
    buf[pos + 2] = 0x00; buf[pos + 3] = 0x01; // QCLASS IN
    pos + 4
}

/// `sock` - demonstrate a UDP socket as a CAPABILITY (utilities/41_sock.md). Opens a socket cap from
/// net-stack, sends a datagram through it, and reports the round-trip - proving a socket is a real
/// kernel capability the client holds and invokes (§7.10), not an ambient channel. A pipe producer.
fn cmd_sock(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    let sock = match sock_open(ctx) {
        Some(c) => c,
        None => { ctx.console_writeln("sock: net-stack would not open a socket (no NIC?)"); return Err(ShellError::Unknown); }
    };
    // Send a datagram through the socket cap to the DNS server (a DNS query is just data that gets a
    // reply); we report the round-trip, which proves the cap does real UDP I/O.
    let mut query = [0u8; 64];
    let qlen = dns_query_bytes("example.com", &mut query);
    let mut payload = [0u8; 96];
    payload[0] = 10; payload[1] = 0; payload[2] = 2; payload[3] = 3;   // dest ip 10.0.2.3
    payload[4] = 0; payload[5] = 53;                                    // dest port 53
    payload[6..6 + qlen].copy_from_slice(&query[..qlen]);
    match sock_invoke(ctx, sock, RIGHT_WRITE, &payload[..6 + qlen]) {
        Some(resp) => out.line_fmt(ctx, format_args!(
            "sock: UDP socket cap - sent {} bytes to 10.0.2.3:53, received {} bytes back (a round-trip through a capability)",
            qlen, resp.payload_bytes().len())),
        None => out.line(ctx, "sock: socket cap invocation returned nothing (no NIC, or nothing answered)"),
    }
    ctx.remove_cap(sock);
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════════════
// Structured records - the typed `Table` model now lives in the SDK (`godspeed_sdk::record`,
// docs/records.md) so any service can build/filter/render records, not just the shell. Imported
// at the top of this file. The shell keeps only the *shell-specific* glue: an `OutSink` that
// bridges the SDK renderers to the console/capture `Out`, and the producer builders below.
// ════════════════════════════════════════════════════════════════════════════════

/// Bridges the SDK's `RecordSink` (byte-oriented) to the shell's `Out` (console or capture),
/// carrying the `ctx` the shell's writers need. `t.to_json(&mut OutSink { ctx, out })`.
struct OutSink<'a, 'o> {
    ctx: &'a ServiceContext,
    out: &'a mut Out<'o>,
}
impl RecordSink for OutSink<'_, '_> {
    fn put(&mut self, bytes: &[u8]) { self.out.put_bytes(self.ctx, bytes); }
}

/// Build the live-task table that `status` produces (columns: slot, name, core, state, mem,
/// queue, restarts). The structured form of what `status` used to print directly.
#[inline(never)]
fn build_status_table(ctx: &ServiceContext) -> Table {
    let mut t = Table::new(&["slot", "name", "core", "state", "mem", "queue", "restarts"]);
    for slot in 0u32..256 {
        let s = ctx.task_stat(slot);
        if !s.valid { continue; }
        let name = t.intern(&s.name[..s.name_len.min(31)]);
        let state = t.intern(s.state_str().as_bytes());
        t.add_row(&[
            Value::Int(slot as u64), name, Value::Int(s.core as u64), state,
            Value::Int(s.mem_used), Value::Int(s.queue_depth as u64), Value::Int(s.restart_count),
        ]);
    }
    t
}

/// `uptime` as a record producer: one row, columns `uptime` (human `Nd HH:MM:SS`) and `seconds`
/// (total seconds since boot). Bare `uptime` renders the grid; `uptime | to json|yaml` renders the
/// row; `uptime | select seconds` etc. work like any record stream. The clock is a wall-clock RTC
/// delta (now − boot, InspectKernel queries 11/12), so it's correct on any APIC timer mode.
#[inline(never)] // keep this builder's frame out of pipe_run's 64 KiB Stream frame, like every sibling
                 // record-builder (build_ls/caps/drives/find/observe/status); a byte pipe overflows the
                 // user stack otherwise (the PUSER-PF lesson). Audit L7 - it was the lone omission.
fn build_uptime_table(ctx: &ServiceContext) -> Table {
    let secs = ctx.uptime_secs() as u64;
    let (d, h, m, s) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60, secs % 60);
    let mut buf = [0u8; 32];
    let mut w = BarW { b: &mut buf, n: 0 };
    let _ = core::fmt::write(&mut w, format_args!("{}d {:02}:{:02}:{:02}", d, h, m, s));
    let n = w.n;
    let mut t = Table::new(&["uptime", "seconds"]);
    let human = t.intern(&buf[..n]);
    t.add_row(&[human, Value::Int(secs)]);
    t
}

/// `uptime` - how long the system has been up. Bare renders the one-row grid; pipeable as records
/// (`uptime | to json|yaml`). See `build_uptime_table` / `utilities/37_uptime.md`.
fn cmd_uptime(ctx: &ServiceContext) -> Result<(), ShellError> {
    let t = build_uptime_table(ctx);
    let mut o = Out::Console;
    t.to_grid(&mut OutSink { ctx, out: &mut o });
    Ok(())
}

/// `random [n]` - one (or n, bounded 1..64) hardware-random u32 from the SoC RNG (the BCM2835 RNG on the
/// Pi 2), printed as hex + decimal. Reports loudly if the machine exposes no hardware RNG.
fn cmd_random(ctx: &ServiceContext, arg: &str) -> Result<(), ShellError> {
    // Bare `random` = 1; a given count must be a number - reject junk LOUDLY, not silently as 1
    // (userspace-audit Audit 5, A5-U3; matches cmd_gpio's loud rejection).
    let a = arg.trim();
    let n = if a.is_empty() { 1 } else {
        match a.parse::<u32>() {
            Ok(v) => v.clamp(1, 64),
            Err(_) => { ctx.console_writeln("random: count must be a number 1..64"); return Ok(()); }
        }
    };
    for _ in 0..n {
        match ctx.hw_random() {
            Some(v) => ctx.console_writeln_fmt(format_args!("{:#010x}  {}", v, v)),
            None => { ctx.console_writeln("random: no hardware RNG on this machine"); break; }
        }
    }
    Ok(())
}

/// `gpio <input|output|high|low|read> <pin>` - drive a SoC GPIO pin (the Pi 2's BCM2835). WORDS not flags
/// (utility convention). GPIO pins carry the UART console + SD card, so this is the operator's rope: pin
/// 0..53, at your own risk. ARM-only; reports loudly elsewhere.
fn cmd_gpio(ctx: &ServiceContext, verb: &str, pin_s: &str) -> Result<(), ShellError> {
    let op = match verb {
        "input" | "in"        => 0u32,
        "output" | "out"      => 1,
        "high" | "set" | "on" => 2,
        "low" | "clear" | "off" => 3,
        "read" | "get"        => 4,
        _ => { ctx.console_writeln("usage: gpio <input|output|high|low|read> <pin 0..53>"); return Ok(()); }
    };
    let pin = match pin_s.trim().parse::<u32>() {
        Ok(p) if p <= 53 => p,
        _ => { ctx.console_writeln("gpio: pin must be 0..53"); return Ok(()); }
    };
    let r = ctx.gpio(op, pin);
    if r < 0 {
        ctx.console_writeln("gpio: not available on this machine (Pi 2 only)");
    } else if op == 4 {
        ctx.console_writeln_fmt(format_args!("gpio {} = {}", pin, r));
    } else {
        ctx.console_writeln_fmt(format_args!("gpio {} {}", pin, verb));
    }
    Ok(())
}

/// `observe now` as a record producer: the task roster plus the metric `status` omits - `ticks`,
/// each task's cumulative `run_ticks` (timer ticks it has spent running since boot). That column
/// is what distinguishes `observe` (how busy) from `status` (who's alive): `observe now | sort
/// reverse ticks` is the native "top". It is a *snapshot*-honest value - cumulative ticks, not an
/// instantaneous % (a rate needs two samples, which only the live view has; observe's per-task
/// "CPU%" is really its core's utilisation, not per-task, so it would not sort meaningfully).
///
/// Only the one-shot `observe now` is pipeable. Bare `observe` is the continuous live view - it
/// owns the screen and never yields a discrete stream - so piping it is a loud refusal, not a
/// silent hang (the same hazard the stage-1 producer whitelist guards, docs/pipes.md). `#[inline
/// (never)]`: like the sibling builders, its `Table` must not inflate `pipe_run`'s frame.
#[inline(never)]
fn build_observe_table(ctx: &ServiceContext, arg: &str) -> Option<Table> {
    if split_first(arg).0 != "now" {
        ctx.console_writeln("observe: the live view can't be piped - use 'observe now | …'");
        return None;
    }
    let mut t = Table::new(&["slot", "name", "core", "state", "mem", "queue", "restarts", "ticks"]);
    for slot in 0u32..256 {
        let s = ctx.task_stat(slot);
        if !s.valid { continue; }
        let name = t.intern(&s.name[..s.name_len.min(31)]);
        let state = t.intern(s.state_str().as_bytes());
        t.add_row(&[
            Value::Int(slot as u64), name, Value::Int(s.core as u64), state,
            Value::Int(s.mem_used), Value::Int(s.queue_depth as u64), Value::Int(s.restart_count),
            Value::Int(s.run_ticks),
        ]);
    }
    Some(t)
}

/// Producers that emit a structured TABLE rather than text. These are inherently tabular
/// (uniform rows), so in a pipe they emit records - composed with `where`/`select`/`sort <col>`,
/// not the text filters. Bare (un-piped) each still prints its normal text. `status` (task
/// roster), `ls` (dir listing), `caps` (held capabilities), `drives` (attached disks), `find`
/// (search hits) are shell-side, so no wire codec is needed - they pass by value like `status`.
fn is_record_producer(name: &str) -> bool {
    matches!(name, "status" | "ls" | "caps" | "drives" | "find" | "observe" | "uptime" | "trace")
}

/// `ls` as a record producer: directory entries as a table (`name` / `type` / `size`). Mirrors
/// `cmd_ls`'s fs parse but emits rows instead of formatted text; `size` is `Int` for files and
/// `Empty` for directories (a dir has no byte size). Errors print and return `None` (abort pipe).
///
/// `#[inline(never)]` (and on all the sibling builders): each holds a multi-KB `Table` (and
/// `build_find_table` a `PathStack` too) on its stack. Inlined into `pipe_run`, those frames
/// would inflate *every* pipeline's stack - even byte-only ones like `greet | sort` that never
/// build a record - and overflow the bounded user stack. Out-of-line, the big frame exists only
/// while the builder actually runs.
#[inline(never)]
fn build_ls_table(ctx: &ShellCtx, cwd: &Cwd, arg: &str) -> Option<Table> {
    let mut buf = [0u8; PATH_MAX];
    let path = resolve_or_err(ctx, cwd, arg, &mut buf)?;
    let reply = match fs_request_q(ctx, OP_LIST_DIR, path, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return None,
        ReqOutcome::Timeout => { ctx.console_writeln("ls: storage unavailable"); return None; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return None; }
    if p.first() == Some(&FS_NOTFOUND) || p.len() < 2 {
        ctx.console_writeln_fmt(format_args!("ls: not a directory: {}", str_of(path)));
        return None;
    }
    let count = p[1] as usize;
    let mut t = Table::new(&["name", "type", "size"]);
    let mut i = 2usize;
    for _ in 0..count {
        if i >= p.len() { break; }
        let nl = p[i] as usize;
        i += 1;
        if i + nl + 1 + 8 > p.len() { break; }
        let name = t.intern(&p[i..i + nl]);
        let is_dir = p[i + nl] != 0;
        let size = u64_le(&p[i + nl + 1..i + nl + 9]);
        i += nl + 1 + 8;
        let kind = t.intern(if is_dir { b"dir" } else { b"file" });
        let sz = if is_dir { Value::Empty } else { Value::Int(size) };
        t.add_row(&[name, kind, sz]);
    }
    Some(t)
}

/// `caps` as a record producer: one row per held capability - `resource` (the target,
/// named for stable kernel resources, else `endpoint#N`) and `rights` (the spelled-out
/// right words). Mirrors `cmd_caps`'s decoding. `name` empty → this shell's own caps.
#[inline(never)]
fn build_caps_table(ctx: &ServiceContext, name: &str) -> Option<Table> {
    let name = if name.is_empty() { "shell" } else { name };
    let slot = match slot_of(ctx, name) {
        Some(s) => s,
        None => { ctx.console_writeln("caps: no such live service"); return None; }
    };
    let mut caps = [CapInfo::default(); 64];
    let n = ctx.task_caps(slot, &mut caps);
    let mut t = Table::new(&["resource", "rights"]);
    for cap in caps.iter().take(n) {
        let mut rb = [0u8; 32];
        let rlen = cap_resource_name(cap.resource_id, &mut rb);
        let res = t.intern(&rb[..rlen]);
        let mut gb = [0u8; 48];
        let glen = cap_rights_str(cap.rights, &mut gb);
        let rights = t.intern(&gb[..glen]);
        t.add_row(&[res, rights]);
    }
    Some(t)
}

/// Write a capability's resource name into `buf`, returning its length. Stable kernel
/// resources by id (matching `cmd_caps`), everything else as `endpoint#N`.
fn cap_resource_name(id: u64, buf: &mut [u8]) -> usize {
    let mut p = 0usize;
    match id {
        1 => write_bytes(buf, &mut p, b"log_write"),
        2 => write_bytes(buf, &mut p, b"spawn"),
        3 => write_bytes(buf, &mut p, b"console_read"),
        4 => write_bytes(buf, &mut p, b"console_push"),
        5 => write_bytes(buf, &mut p, b"introspect"),
        6 => write_bytes(buf, &mut p, b"service_control"),
        other => { write_bytes(buf, &mut p, b"endpoint#"); write_u32(buf, &mut p, other as u32); }
    }
    p
}

/// Write the spelled-out rights (space-separated, no trailing space) into `buf` (§7.4).
fn cap_rights_str(r: u8, buf: &mut [u8]) -> usize {
    let mut p = 0usize;
    let words: [(u8, &[u8]); 6] = [
        (0x01, b"read"), (0x02, b"write"), (0x04, b"send"),
        (0x08, b"recv"), (0x10, b"grant"), (0x20, b"revoke"),
    ];
    for (bit, word) in words {
        if r & bit != 0 {
            if p > 0 { write_bytes(buf, &mut p, b" "); }
            write_bytes(buf, &mut p, word);
        }
    }
    p
}

/// `drives` as a record producer: one row per attached drive - `index`, `label`, `status`
/// (`GSFS`/`raw`), `size_mib`, and `free_mib` (`Empty` for a raw, unformatted drive). Single
/// drive in step 3; mirrors `drives_list`. Sizes are in MiB (so the column name carries the
/// unit - a bare number cell can't).
#[inline(never)]
fn build_drives_table(ctx: &ShellCtx) -> Option<Table> {
    drain_stale_fs_replies(ctx);   // start from a clean channel (see the fn: replies carry no request id)
    let reply = match fs_raw(ctx, &[OP_DRIVES_INFO], FS_ANSWER_SECS) {
        Some(r) => r,
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); return None; }
    };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 28 {
        // Name what actually came back. "no disk found" was a GUESS at the cause dressed as a fact: the
        // reply may be a status code (FS_NOFS / FS_UNAVAIL / FS_ERR) that means something quite different
        // from an absent disk, and a short reply means the protocol went wrong, not that storage is gone.
        // Reporting the raw shape is the difference between diagnosing this in one boot and guessing at it
        // for several (§26.7 - say what failed, not what you suppose it means).
        ctx.console_writeln_fmt(format_args!(
            "drives: unexpected reply from fs - status {} len {} (want status {}, len >= 28)",
            p.first().copied().unwrap_or(255), p.len(), FS_OK));
        return None;
    }
    let mounted = p[1] != 0;
    // A capacity of ZERO is not a drive of size zero - it is NO DRIVE.
    //
    // `fs` was already reporting "capacity 0 sectors, mounted false" correctly and the shell printed
    // a row for it anyway - "0  -  raw  0 MiB  - not formatted -" - which reads as a blank disk that
    // is present rather than one that is absent. The service told the truth and the display
    // contradicted it.
    //
    // Checked here as well as in the device-first path, because this answer is ALWAYS available: it
    // needs no extra peer and no second query, so it holds even when the direct block-driver query
    // cannot be reached (which is exactly what happened on the Pi 4).
    // NOTE: a zero capacity means NO DRIVE, and `drives_list` reports it as such. This builder feeds
    // a different consumer and its Table API is not shaped for a one-line message, so it is left
    // alone deliberately rather than guessed at - the user-visible `drives` path is the one that was
    // lying, and that is fixed there.
    let mib = u64_le(&p[2..10]) / 2048;
    let mut t = Table::new(&["index", "label", "status", "size_mib", "free_mib"]);
    if mounted {
        let total = u64_le(&p[10..18]);
        let next = u64_le(&p[18..26]);
        let free_mib = total.saturating_sub(next) / 2048;
        let ll = (p[27] as usize).min(LABEL_MAX);
        let lab = &p[28..28 + ll];
        let label = if lab.is_empty() { t.intern(b"-") } else { t.intern(lab) };
        let status = t.intern(b"GSFS");
        t.add_row(&[Value::Int(0), label, status, Value::Int(mib), Value::Int(free_mib)]);
    } else {
        let label = t.intern(b"-");
        let status = t.intern(b"raw");
        t.add_row(&[Value::Int(0), label, status, Value::Int(mib), Value::Empty]);
    }
    Some(t)
}

/// `find` as a record producer: one row per match - `name`, `type` (`file`/`dir`), and the
/// full `path`. Same bounded depth-first walk as `cmd_find`, emitting rows instead of printing
/// the path. `arg` is the producer tail (`<pattern> [start]`).
#[inline(never)]
fn build_find_table(ctx: &ShellCtx, cwd: &Cwd, arg: &str) -> Option<Table> {
    let (target, start) = split_first(arg);
    if target.is_empty() { ctx.console_writeln("usage: find <name> [path]"); return None; }
    let start = if start.is_empty() { "/" } else { start };
    let mut sbuf = [0u8; PATH_MAX];
    let start_abs = resolve_or_err(ctx, cwd, start, &mut sbuf)?;
    let mut stack = PathStack::new();
    stack.push(start_abs);
    let tb = target.as_bytes();
    let is_glob = tb.iter().any(|&b| b == b'*' || b == b'?');
    let mut t = Table::new(&["name", "type", "path", "size"]);
    let mut dir = [0u8; PATH_MAX];
    while let Some(dlen) = stack.pop(&mut dir) {
        let reply = match fs_request_q(ctx, OP_LIST_DIR, &dir[..dlen], &[]) {
            ReqOutcome::Reply(r) => r,
            ReqOutcome::Aborted => return None,
            ReqOutcome::Timeout => { ctx.console_writeln("find: storage unavailable"); return None; }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return None; }
        if p.first() != Some(&FS_OK) || p.len() < 2 { continue; }
        let count = p[1] as usize;
        let mut i = 2usize;
        for _ in 0..count {
            if i >= p.len() { break; }
            let nl = p[i] as usize;
            i += 1;
            if i + nl + 1 + 8 > p.len() { break; }
            let name = &p[i..i + nl];
            let is_dir = p[i + nl] != 0;
            let size = u64_le(&p[i + nl + 1..i + nl + 9]);   // per-entry size, same layout ls reads
            i += nl + 1 + 8;
            let mut child = [0u8; PATH_MAX];
            if let Some(clen) = join_path(&dir[..dlen], name, &mut child) {
                let hit = if is_glob { glob_match(tb, name) } else { contains(name, tb) };
                if hit {
                    let nv = t.intern(name);
                    let tv = t.intern(if is_dir { b"dir" } else { b"file" });
                    let pv = t.intern(&child[..clen]);
                    // Files carry their byte size (`find * | where size>1000`, the library's `size`
                    // sum); a dir's row leaves it Empty, exactly as ls's records do.
                    let sz = if is_dir { Value::Empty } else { Value::Int(size) };
                    t.add_row(&[nv, tv, pv, sz]);
                }
                if is_dir { stack.push(&child[..clen]); }
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "find: search truncated - more than {} directories pending (bounded walk)", FIND_QCAP));
    }
    Some(t)
}


/// What flows through a pipe: either a byte buffer (text streams) or a typed Table (records).
/// `from`/`to` convert between them; the dispatcher routes each stage by command AND by which
/// of these it is currently holding (so `sort` is a line-sort on Bytes, a column-sort on a
/// Table). This is the byte↔record unification.
enum Stream {
    Bytes(Cap),
    Table(Table),
}

/// The unified pipe dispatcher: `A | B | C …`, threading a `Stream` that may transition between
/// bytes and records via `from`/`to`. Stage 1 produces; middle stages transform; the last stage
/// sinks (`write`) or, if it isn't a sink, the final stream is rendered to the console. Replaces
/// the separate byte and record pipelines. (docs/pipes.md, docs/records.md)
///
/// `#[inline(never)]`: holds a 64 KiB `Stream` on its frame, so it must never be inlined into
/// `execute` (which would carry that 64 KiB into every command's frame, and via a nested
/// `run → execute` chain overflow the user stack).
#[inline(never)]
fn pipe_run(ctx: &ShellCtx, cwd: &Cwd, line: &str, out: &mut Out) -> Result<(), ShellError> {
    // HIGH-WATER MARK, reported only when it moves. This file's own header says the user stack is
    // 64 KiB and that this frame already sits near it, and the Pi 4 twice killed the shell inside a
    // pipe with `ELR_EL1 = 0x0` - a branch to address zero, which is what a smashed frame's saved LR
    // looks like. That is a HYPOTHESIS, and the shell was the one service with no way to confirm or
    // kill it: `fs` prints its deepest block call, this printed nothing.
    //
    // Reported on a new maximum rather than every pipe, so a `selfcheck` of 357 cases does not become
    // 357 log lines, and the number that matters - the worst frame reached - is still never missed.
    {
        let used = ctx.stack_used();
        let prev = ctx.pipe_stack_hwm.get();
        if used > prev {
            ctx.pipe_stack_hwm.set(used);
            ctx.log_fmt(format_args!(
                "shell: pipe stack high-water {} of {} bytes ({}% of the user stack)",
                used, USER_STACK_BYTES, used * 100 / USER_STACK_BYTES.max(1)
            ));
        }
    }
    let mut stages = [""; MAX_STAGES];
    let mut n = 0usize;
    for part in line.split('|') {
        let s = part.trim();
        if s.is_empty() { ctx.console_writeln("usage: <producer> | <stage> [| …]"); return Err(ShellError::Unknown); }
        if n >= MAX_STAGES { ctx.console_writeln_fmt(format_args!("pipe: too many stages (max {})", MAX_STAGES)); return Err(ShellError::Unknown); }
        stages[n] = s;
        n += 1;
    }
    if n < 2 { ctx.console_writeln("usage: <producer> | <stage> [| …]"); return Err(ShellError::Unknown); }

    // Stage 1 - produce a Stream.
    let (c0, _) = split_first(stages[0]);
    let mut s = if is_record_producer(c0) {
        let arg = split_first(stages[0]).1;
        let t = match c0 {
            "ls"      => match build_ls_table(ctx, cwd, arg)    { Some(t) => t, None => return Err(ShellError::Unknown) },
            "caps"    => match build_caps_table(ctx, arg)       { Some(t) => t, None => return Err(ShellError::Unknown) },
            "drives"  => match build_drives_table(ctx)          { Some(t) => t, None => return Err(ShellError::Unknown) },
            "find"    => match build_find_table(ctx, cwd, arg)  { Some(t) => t, None => return Err(ShellError::Unknown) },
            "observe" => match build_observe_table(ctx, arg)    { Some(t) => t, None => return Err(ShellError::Unknown) },
            "uptime"  => build_uptime_table(ctx),
            // `trace ipc` / `trace failures` are record sources; the other subcommands are readers
            // of live kernel state that print a tree, and a tree is not a table. Piping one of those
            // is refused loudly rather than quietly yielding the wrong thing.
            "trace"   => match split_first(arg).0 {
                "ipc"      => match build_trace_table(ctx, false) { Some(t) => t, None => return Err(ShellError::Unknown) },
                "deps"     => match build_deps_table(ctx, split_first(arg).1.trim()) { Some(t) => t, None => return Err(ShellError::Unknown) },
                "endpoints" => build_endpoints_table(ctx),
                "failures" => match build_trace_table(ctx, true)  { Some(t) => t, None => return Err(ShellError::Unknown) },
                other      => {
                    ctx.console_writeln_fmt(format_args!(
                        "trace: '{}' is not a record source - pipe 'trace ipc' or 'trace failures'", other));
                    return Err(ShellError::Unknown);
                }
            },
            _         => build_status_table(ctx),
        };
        // Loud on the record bound (§3.12/§26.6): a producer that overran rows/arena is reported,
        // never silently truncated - the same bar the text pipe buffer holds.
        if t.overflow() {
            ctx.console_writeln_fmt(format_args!(
                "{}: result exceeded the record bound ({} rows / {} bytes) - truncated",
                c0, REC_MAX_ROWS, REC_ARENA));
        }
        Stream::Table(t)
    } else if is_record_producer_service(c0) {
        // A SERVICE that emits records: drain its binary wire encoding (Table::encode, §
        // docs/records.md) and decode it back into a Table - no JSON round-trip. The transport
        // is the same byte drain as a text service; the bytes are records, decoded here.
        let mut cap = Cap::new();
        if !drain_service(ctx, c0, None, &mut cap) { return Err(ShellError::Unknown); }
        match Table::decode(cap.bytes()) {
            Ok(t) => Stream::Table(t),
            Err(why) => { ctx.console_writeln_fmt(format_args!("{}: bad record stream - {}", c0, why)); return Err(ShellError::Unknown); }
        }
    } else if is_producer_builtin(c0) {
        let mut cap = Cap::new();
        run_producer(ctx, cwd, stages[0], &mut Out::Capture(&mut cap));
        if cap.overflow { ctx.console_writeln("pipe: producer output exceeded the pipe buffer (truncated)"); }
        Stream::Bytes(cap)
    } else if is_pipe_producer_service(c0) {
        let mut cap = Cap::new();
        if !drain_service(ctx, c0, None, &mut cap) { return Err(ShellError::Unknown); }
        Stream::Bytes(cap)
    } else if c0 == "result" || c0 == "assert" {
        // The classic mix-up: piping into the *outcome* channel. `result`/`assert` read a
        // command's Ok/Err, not its piped output. Point at the right idiom instead of the
        // generic "not a pipe source".
        ctx.console_writeln_fmt(format_args!(
            "pipe: '{}' checks a command's outcome, not piped output. Run the command, then 'result', or use 'assert ok <command>'", c0));
        return Err(ShellError::Unknown);
    } else {
        ctx.console_writeln_fmt(format_args!("pipe: '{}' cannot start a pipe because it's not a pipe source", c0));
        return Err(ShellError::Unknown);
    };

    // Stages 2..n - transform, with the last optionally a sink (`write` or `assert`).
    for i in 1..n {
        let last = i == n - 1;
        let (cmd, arg) = split_first(stages[i]);
        if cmd == "write" {
            if !last { ctx.console_writeln("pipe: write must be the last stage"); return Err(ShellError::Unknown); }
            match &s {
                Stream::Bytes(c) => pipe_write(ctx, cwd, arg, c.bytes()),
                Stream::Table(t) => {
                    let mut c = Cap::new();
                    { let mut o = Out::Capture(&mut c); t.to_grid(&mut OutSink { ctx, out: &mut o }); }
                    pipe_write(ctx, cwd, arg, c.bytes());
                }
            }
            return Ok(());
        }
        if cmd == "assert" {
            // The verifying sink: judge the stream and return Ok/Err so a script's `run` (and
            // `result`) sees the verdict. Must be last - it consumes the stream.
            if !last { ctx.console_writeln("pipe: assert must be the last stage"); return Err(ShellError::Unknown); }
            return assert_stream(ctx, &s, arg);
        }
        if cmd == "result" {
            // `result` reads the outcome channel, not a stream - same mix-up as `<cmd> | result`.
            ctx.console_writeln("pipe: 'result' checks a command's outcome, not piped output. Run the command, then 'result', or use 'assert ok <command>'");
            return Err(ShellError::Unknown);
        }
        if !pipe_transform(ctx, stages[i], cmd, &mut s) { return Err(ShellError::Unknown); }
    }
    // No sink - render the final stream to `out` (the console, or a capture buffer for `$( )`).
    match &s {
        Stream::Bytes(c) => out.put_bytes(ctx, c.bytes()),
        Stream::Table(t) => t.to_grid(&mut OutSink { ctx, out }),
    }
    Ok(())
}

/// `… | assert <check> [text]` - the verifying pipe sink. Materialises the stream to text (a
/// `Table` renders to its grid) and checks it, returning `Ok` if the assertion holds, else
/// `Err(AssertFailed)`. Prints a terse verdict so a `run` transcript shows pass/fail per line.
/// Checks: `contains <text>`, `lacks <text>` (negation), `empty`. (Content correctness; the
/// `assert ok/fails <cmd>` *result* form is handled in `cmd_assert`, no pipe.)
///
/// `#[inline(never)]`: holds a 64 KiB `Cap` (to materialise a `Table`), so it must not fold into
/// `pipe_run`'s frame (which already carries a 64 KiB `Stream`) - the inline-frame stack rule.
#[inline(never)]
fn assert_stream(ctx: &ServiceContext, s: &Stream, arg: &str) -> Result<(), ShellError> {
    let mut tmp = Cap::new();
    let bytes: &[u8] = match s {
        Stream::Bytes(c) => c.bytes(),
        Stream::Table(t) => {
            { let mut o = Out::Capture(&mut tmp); t.to_grid(&mut OutSink { ctx, out: &mut o }); }
            tmp.bytes()
        }
    };
    let (check, rest) = split_first(arg);
    let want = strip_quotes(rest);
    let held = match check {
        "contains" => contains(bytes, want.as_bytes()),
        "lacks"    => !contains(bytes, want.as_bytes()),
        "empty"    => trim_bytes(bytes).is_empty(),
        _ => {
            ctx.console_writeln_fmt(format_args!("assert: unknown check '{}' (try: contains, lacks, empty)", check));
            return Err(ShellError::Unknown);
        }
    };
    assert_verdict(ctx, held, check, want)
}

/// Print the verdict (`assert: ok` / `assert: FAILED - …`) and map it to a `Result`.
fn assert_verdict(ctx: &ServiceContext, held: bool, check: &str, detail: &str) -> Result<(), ShellError> {
    if held {
        ctx.console_writeln("assert: ok");
        Ok(())
    } else {
        if detail.is_empty() {
            ctx.console_writeln_fmt(format_args!("assert: FAILED ({})", check));
        } else {
            ctx.console_writeln_fmt(format_args!("assert: FAILED ({} '{}')", check, detail));
        }
        Err(ShellError::AssertFailed)
    }
}

/// Write a (possibly large) byte buffer to the console. `console_write` drops anything over
/// 256 bytes, so split into ≤256-byte pieces. Output is ASCII (json/yaml/text), so chunk
/// boundaries never split a multi-byte char.
/// Bytes per console burst (≤ 256, the `console_write` syscall cap).
const CONSOLE_BURST: usize = 256;
/// Yields between bursts when pacing bulk output - see `console_write_chunked`.
const CONSOLE_PACE_YIELDS: u32 = 2;

/// Write `bytes` to the console in ≤256-byte bursts, **pacing** between bursts so the HOST
/// serial side can drain. A big one-shot dump (a long chaos report, `read` of a large file)
/// otherwise overruns the host UART / USB-serial receive buffer and bytes are lost mid-stream -
/// the kernel's THRE poll is deliberately bounded (it drops a byte rather than wedge a core with
/// IF=0, `arch/x86_64`). Yielding lets the host drain between bursts. Only the serial mirror is at
/// risk (the framebuffer is locked per-string, so the TV is fine); this rescues the serial mirror.
/// Output ≤ one burst never yields, so the prompt and short lines stay snappy.
fn console_write_chunked(ctx: &ServiceContext, bytes: &[u8]) {
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + CONSOLE_BURST).min(bytes.len());
        ctx.console_write(str_of(&bytes[i..end]));
        i = end;
        if i < bytes.len() {
            for _ in 0..CONSOLE_PACE_YIELDS { ctx.yield_cpu(); }
        }
    }
}

/// Apply one non-sink stage to the stream in place. Dispatches by command AND by whether the
/// stream is currently Bytes or a Table; a mismatch is a loud error (false). `from`/`to` flip
/// between the two worlds.
fn pipe_transform(ctx: &ServiceContext, stage: &str, cmd: &str, s: &mut Stream) -> bool {
    match cmd {
        // text → records
        "from" => {
            let (_, fmt) = split_first(stage);
            let (fmt, _) = split_first(fmt);
            let bytes = match s { Stream::Bytes(c) => c, Stream::Table(_) => {
                ctx.console_writeln("from: input is already records"); return false; } };
            let t = match fmt {
                "json" => match Table::from_json(bytes.bytes()) {
                    Ok(t) => t,
                    Err(why) => { ctx.console_writeln_fmt(format_args!("from json: {}", why)); return false; }
                },
                _ => { ctx.console_writeln("from: unknown format (try: from json)"); return false; }
            };
            *s = Stream::Table(t);
            true
        }
        // records → text
        "to" => {
            let (_, fmt) = split_first(stage);
            let (fmt, _) = split_first(fmt);
            let t = match s { Stream::Table(t) => t, Stream::Bytes(_) => {
                ctx.console_writeln("to: input is text, not records (parse with 'from json' first)"); return false; } };
            let mut c = Cap::new();
            {
                let mut o = Out::Capture(&mut c);
                let mut sink = OutSink { ctx, out: &mut o };
                match fmt {
                    "json" => t.to_json(&mut sink),
                    "yaml" => t.to_yaml(&mut sink),
                    // The GRID, as a pipe stage. It existed as the console rendering of every record
                    // source but could not be ASKED for, so a producer that draws something else on
                    // the console - `trace deps` draws a tree - had no way to offer the table. One
                    // producer, three renderings, and the choice belongs to the reader.
                    "grid" => t.to_grid(&mut sink),
                    _ => { ctx.console_writeln("to: unknown format (try: to json | to yaml | to grid)"); return false; }
                }
            }
            *s = Stream::Bytes(c);
            true
        }
        // record filters (Table only)
        "where" => match s {
            Stream::Table(t) => match parse_predicate(split_first(stage).1) {
                // filter() returns false on an unknown column; like the original, the pipeline
                // continues (unchanged table) after the loud notice.
                Some((col, op, val)) => {
                    if !t.filter(col, op, val) {
                        ctx.console_writeln_fmt(format_args!("where: no such column '{}'", col));
                    }
                    true
                }
                None => { ctx.console_writeln("where: need a predicate like name=shell or mem>0"); false }
            },
            Stream::Bytes(_) => { ctx.console_writeln("where: needs records (try 'from json')"); false }
        },
        "select" => match s {
            Stream::Table(t) => {
                let mut sa = [""; MAX_ARGS];
                let sc = tokenize(stage, &mut sa);
                if sc < 2 { ctx.console_writeln("usage: … | select <col> [col …]"); return false; }
                if t.select(&sa[1..sc]) { true }
                else { ctx.console_writeln("select: no such column (check the column names)"); false }
            }
            Stream::Bytes(_) => { ctx.console_writeln("select: needs records (try 'from json')"); false }
        },
        // sort is dual: column-sort on a Table, line-sort on Bytes
        "sort" => match s {
            Stream::Table(t) => {
                let mut sa = [""; MAX_ARGS];
                let sc = tokenize(stage, &mut sa);
                let (mut col, mut rev) = ("", false);
                for a in &sa[1..sc] { if *a == "reverse" { rev = true; } else if col.is_empty() { col = a; } }
                if col.is_empty() { ctx.console_writeln("usage: … | sort [reverse] <col>"); return false; }
                if t.sort(col, rev) { true }
                else { ctx.console_writeln_fmt(format_args!("sort: no such column '{}'", col)); false }
            }
            Stream::Bytes(_) => byte_filter(ctx, stage, s),
        },
        // count is dual: ROW count on a Table (a bare number), the line/word/byte summary on Bytes
        "count" => match s {
            Stream::Table(t) => {
                let mut c = Cap::new();
                { let mut o = Out::Capture(&mut c); o.line_fmt(ctx, format_args!("{}", t.nrows())); }
                *s = Stream::Bytes(c);
                true
            }
            Stream::Bytes(_) => byte_filter(ctx, stage, s),
        },
        // numeric-column reducers (Table only): sum / min / max / avg -> a bare number
        "sum" | "min" | "max" | "avg" => match s {
            Stream::Table(t) => {
                let mut sa = [""; MAX_ARGS];
                let sc = tokenize(stage, &mut sa);
                if sc < 2 { ctx.console_writeln_fmt(format_args!("usage: … | {} <col>", cmd)); return false; }
                let op = match cmd { "sum" => AggOp::Sum, "min" => AggOp::Min, "max" => AggOp::Max, _ => AggOp::Avg };
                match t.aggregate(sa[1], op) {
                    Ok(v) => {
                        let mut c = Cap::new();
                        { let mut o = Out::Capture(&mut c); o.line_fmt(ctx, format_args!("{}", v)); }
                        *s = Stream::Bytes(c);
                        true
                    }
                    Err(AggErr::NoColumn) => { ctx.console_writeln_fmt(format_args!("{}: no such column '{}'", cmd, sa[1])); false }
                    Err(AggErr::NonNumeric) => { ctx.console_writeln_fmt(format_args!("{}: column '{}' is not numeric (never a silent 0)", cmd, sa[1])); false }
                }
            }
            Stream::Bytes(_) => { ctx.console_writeln_fmt(format_args!("{}: needs records (a numeric column) - try 'from json'", cmd)); false }
        },
        // byte filters (Bytes only)
        "match" | "first" | "last" => match s {
            Stream::Bytes(_) => byte_filter(ctx, stage, s),
            Stream::Table(_) => { ctx.console_writeln_fmt(format_args!("{}: this is a record stream - use 'where'/'select'/'sort <col>', or 'to json' for text", cmd)); false }
        },
        // anything else: a service filter stage (Bytes only)
        _ => match s {
            Stream::Bytes(c) => {
                let mut next = Cap::new();
                if !drain_service(ctx, cmd, Some(c.bytes()), &mut next) { return false; }
                *s = Stream::Bytes(next);
                true
            }
            Stream::Table(_) => { ctx.console_writeln_fmt(format_args!("pipe: '{}' needs text (render with 'to json' first)", cmd)); false }
        },
    }
}

/// Run a built-in byte filter (match/count/sort/first/last) over the stream's bytes, replacing
/// it with the filtered output. Caller guarantees the stream is `Bytes`.
fn byte_filter(ctx: &ServiceContext, stage: &str, s: &mut Stream) -> bool {
    let mut next = Cap::new();
    if let Stream::Bytes(c) = s {
        run_filter_builtin(ctx, stage, c.bytes(), &mut Out::Capture(&mut next));
    }
    *s = Stream::Bytes(next);
    true
}

/// `roster` (bare) - render the example record service's table directly: the same data a pipe
/// sees (`roster | where role=core`). Spawns roster, drains its binary wire encoding (`Table::
/// encode`), decodes it back into a `Table`, and renders the grid. `#[inline(never)]` - it holds a
/// 64 KiB `Cap` on the user stack (USER_STACK_PAGES is tight; see [[project-shell-stack-pipe]]).
#[inline(never)]
fn cmd_roster(ctx: &ServiceContext) -> Result<(), ShellError> {
    let mut cap = Cap::new();
    if !drain_service(ctx, "roster", None, &mut cap) { return Err(ShellError::Unknown); }
    match Table::decode(cap.bytes()) {
        Ok(t) => { let mut o = Out::Console; t.to_grid(&mut OutSink { ctx, out: &mut o }); Ok(()) }
        Err(why) => {
            ctx.console_writeln_fmt(format_args!("roster: bad record stream - {}", why));
            Err(ShellError::Unknown)
        }
    }
}

/// Task slots to scan. Matches the other slot-scanning commands in this file.
const TRACE_SLOTS: u32 = 256;

/// `trace` - why is this task not progressing? (`utilities/46_trace.md`)
///
/// **A READER, not a tracer.** It records nothing, enables nothing, and has no switch, because every
/// fact it prints is state the kernel already keeps for CORRECTNESS: `CALL_AWAIT_EP` exists so a dead
/// replier wakes its caller with `ReplyDead` (8.6), and the chain of who-awaits-whom IS the causal
/// chain of a stuck system. So the cost when unused is zero - nothing runs until asked - and no kernel
/// responsibility is added: introspection over IPC state, both already inside MISCIS (4.3).
///
/// What it deliberately does NOT do: interpret a message. The kernel sees an opaque byte array, so
/// this prints `awaiting endpoint 7`, never `fs.read("/etc/config")`. Naming an operation is protocol
/// knowledge and belongs to the service that owns the protocol (4.4, 26.10).
/// `trace <view> help` - what ONE view's output means.
///
/// The top-level help listed every view AND every column, which made it taller than a console and
/// turned a reference into something you had to page through to find one line. A view's columns are
/// only interesting once you are looking at that view, so they live with it. `trace help` is now the
/// map; this is the detail, one screen at a time, and neither needs a pager.
fn trace_sub_help(ctx: &ServiceContext, view: &str) -> bool {
    match view {
        "blocked" => help_block(ctx, "trace blocked", "every task stuck on ANOTHER task", &[
            ("slot / name", "the blocked task's scheduler slot and service name", ""),
            ("blocked", "how it waits: `call` (awaiting a reply) or `recv`", ""),
            ("awaiting", "the endpoint id it waits on - `trace endpoint <id>`", ""),
            ("held_by", "the service that owns it, i.e. owes the answer", ""),
            ("silence", "nothing is stuck. Idle on your OWN endpoint is not stuck", ""),
        ], false),
        "chain" => help_block(ctx, "trace chain", "who one task is stuck behind, as a tree", &[
            ("<name|slot>", "a service name, or a task slot - digits are a slot", "trace chain 7"),
            ("reading it", "each line is who the line above waits for", ""),
            ("root: own endpoint", "idle - waiting for work. The chain ends fine here", ""),
            ("root: runnable", "it is running right now", ""),
        ], false),
        "deps" => help_block(ctx, "trace deps", "what a service can call, as a tree", &[
            ("indent", "a child is a service its parent holds a SEND cap to", ""),
            ("N calls", "exchanges still IN THE RING - a window, not a total", ""),
            ("(write list)", "the operations seen, by name", ""),
            ("N FAILED", "of those, how many did not end in a reply", ""),
            ("0 calls", "authority held but unused here - worth asking why", ""),
            ("reply#NNN", "a cap to an endpoint no task owns: a return address", ""),
            ("as a table", "| to grid - its header names every filterable column", "trace deps fs | to grid"),
            ("declared peers", "NOT shown: they live in the kernel, unreadable here", ""),
        ], false),
        "endpoints" => help_block(ctx, "trace endpoints", "every live endpoint and its owner", &[
            ("endpoint", "the id to pass to `trace endpoint <id>`", "trace endpoint 112"),
            ("queue", "messages waiting - non-zero on an idle service is work arriving", ""),
            ("primary only", "a reply mailbox has no name, hence reply#NNN elsewhere", ""),
        ], false),
        "endpoint" => help_block(ctx, "trace endpoint", "who owns an endpoint, and who can reach it", &[
            ("owned by task N", "a live service's endpoint", ""),
            ("a kernel resource", "ids 1-5 are log_write, spawn, console_read, ...", "trace endpoint 4"),
            ("NO LIVE OWNER", "its task died, or it is a reply-only mailbox", ""),
            ("holder / rights", "every live task holding a cap, and its rights", ""),
        ], false),
        "ipc" | "failures" => help_block(ctx, "trace ipc", "recent IPC exchanges, oldest first", &[
            ("seq", "the CALLER'S own count. A gap = an event that never arrived", ""),
            ("sec", "when the RING saw it, from the oldest row. Not a latency", ""),
            ("caller / peer", "who called, and who was called, by name", ""),
            ("op", "the operation by name; a number = unnamed in this shell", ""),
            ("outcome", "REPLY TIMEOUT PEER_LOST QUEUE_FULL ABORTED", ""),
            ("TIMEOUT", "no answer in time. The peer is alive as far as we know", ""),
            ("PEER_LOST", "the send failed, or the peer died mid-call", ""),
            ("QUEUE_FULL", "the peer is ALIVE and congested - not absent", ""),
            ("in flight", "not here: one row per exchange, written when it ENDS", ""),
            ("filtering", "it is a record source", "trace ipc | where outcome=TIMEOUT"),
        ], false),
        "status" => help_block(ctx, "trace status", "the ring itself", &[
            ("ring N events", "capacity. Fixed, no heap", ""),
            ("N recorded", "events accepted since the sink last started", ""),
            ("N DROPPED", "overwritten before being read", ""),
            ("the OTHER loss", "a send that failed never arrived, so it is NOT counted", ""),
            ("", "it shows only as a gap in a caller's seq", ""),
        ], false),
        _ => return false,
    }
    true
}

fn cmd_trace(ctx: &ServiceContext, arg: &str) -> Result<(), ShellError> {
    let mut it = arg.split_whitespace();
    let sub = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("");
    match sub {
        "" | "help" => { util_help(ctx, "trace"); Ok(()) }
        // The SHARED version printer, like every other utility. This hand-rolled its own with a
        // private `TRACE_VERSION` constant, so `trace version` said 0.1.0 while every help header
        // said 0.4.0 - one utility reporting two versions of itself, which is the one thing a
        // `version` command exists to be trusted about. It also skipped the creator credit that
        // conventions rule 5 requires and the suite asserts for every other utility.
        "version" => { util_version(ctx, "trace"); Ok(()) }
        // `trace <view> help` - the detail for one view, before the view itself runs.
        v if rest == "help" && trace_sub_help(ctx, v) => Ok(()),
        "blocked" => trace_blocked(ctx),
        "ipc" => trace_events(ctx, false),
        "failures" => trace_events(ctx, true),
        "status" => trace_status(ctx),
        "deps" => trace_deps(ctx, rest),
        "endpoint" => trace_endpoint(ctx, rest),
        "endpoints" => trace_endpoints(ctx),
        // ONE view, EITHER subject. This was two subcommands - `trace task <slot>` and `trace service
        // <name>` - which named the SUBJECT KIND while every other subcommand names the VIEW. That
        // read as two different things right up until you noticed they printed identical output from
        // identical code, and it left `trace service fs` and `trace deps fs` looking like siblings
        // when only one of them is named for what it shows.
        //
        // The argument disambiguates itself: all digits is a slot, anything else is a name. No flag,
        // no second verb, and nothing a caller has to remember beyond "chain of what?".
        "chain" => {
            if rest.is_empty() {
                ctx.console_writeln("trace chain: needs a service name or a task slot, e.g. `trace chain fs` or `trace chain 7`");
                return Err(ShellError::Unknown);
            }
            match rest.parse::<u32>() {
                Ok(slot) => trace_chain(ctx, slot),
                _ => match trace_slot_of_name(ctx, rest) {
                    Some(slot) => trace_chain(ctx, slot),
                    None => {
                        ctx.console_writeln_fmt(format_args!("trace chain: no live task named '{}'", rest));
                        Err(ShellError::Unknown)
                    }
                },
            }
        }
        _ => {
            ctx.console_writeln_fmt(format_args!("unknown: trace {}", sub));
            Err(ShellError::Unknown)
        }
    }
}

/// Ask `logger` for its recent trace events (`utilities/46_trace.md` mechanism B).
///
/// The ring lives in that service, not the kernel - so this is an ordinary request/reply to an
/// ordinary service, and a logger that is dead or absent is answered with a sentence rather than a
/// hang (Commandment VIII: a missing dependency RETURNS, loudly).
/// Build the trace ring's events as a `Table`.
///
/// A TABLE and not printed text, so `trace ipc` is a record source like `status` or `ls`: it renders
/// as a grid on the console, pages when it is taller than the screen, and pipes into the record verbs
/// (`trace ipc | where peer=fs`, `| to json`, `| to yaml`, `| count`). One producer, three uses -
/// the alternative was a printer plus a separate serialiser that would drift apart.
fn build_trace_table(ctx: &ServiceContext, failures_only: bool) -> Option<Table> {
    // Ask for exactly what a record `Table` can hold - not a screenful, and not the whole ring.
    //
    // The ring keeps 192 events and one reply message could carry ~180 of them, but `REC_MAX_ROWS` is
    // 64, so asking for more only produced a loud "result exceeded the record bound - truncated" on
    // every single run. A bound announced once in `trace status` is information; the same bound
    // announced on every command is noise that trains you to ignore it. So the newest 64 are what a
    // dump shows, and `trace status` remains the place that says how much history exists.
    let req = [godspeed_sdk::trace::TRACE_OP_DUMP, REC_MAX_ROWS as u8];
    let reply = match trace_ask(ctx, &req) {
        Some(r) => r,
        None => {
            ctx.console_writeln("trace: the logger service did not answer in 3 attempts (it holds the ring)");
            return None;
        }
    };
    let b = reply.payload_bytes();
    if b.is_empty() {
        ctx.console_writeln("trace: logger returned nothing");
        return None;
    }
    let n = b[0] as usize;
    let ev = godspeed_sdk::trace::EV_LEN;
    let pl = godspeed_sdk::trace::PEER_LEN;
    // COLUMN NAMES SAY WHAT THEY HOLD. They were `eseq / t+s / peer / op / event`, which needed the
    // legend to be readable at all; these need it only for the detail:
    //   caller  - who made the call, as that service DECLARED itself (`ctx.trace_as`). A service
    //             cannot ask what it is called (identity is not ambient), and the kernel's unforgeable
    //             answer - `Message.sender_ep` - is deliberately kernel-internal, so a traced service
    //             says. That costs nothing in trust: the whole event is already its testimony.
    //   seq     - the EMITTER'S own event number. Not global, so a mixed dump interleaves several
    //             sequences and can look unsorted - it is not, rows are in ring order, oldest first.
    //             A GAP in one service's numbering is that service's dropped events.
    //   sec     - seconds since the oldest row shown. The stored value is an epoch second, which says
    //             nothing on its own; the GAP between rows is what a stall looks like.
    //   peer    - the service that was CALLED, by name (the emitter knew it; see `sdk::trace`).
    //   op      - that protocol's OPCODE, from the byte the emitting service says it lives in
    //             means. `utilities/46_trace.md` 7 worried that "byte 0 is the opcode" is a
    //             CONVENTION, and that a protocol putting something else there would produce a
    //             misleading column. It does: `fs` PREPENDS a correlation tag to every block-driver
    //             request (`treq[0] = tag; treq[1..] = req`), so those rows carry a tag and the real
    //             opcode sits at byte 1. Calling the column `op` asserted a meaning the data does not
    //             have; `byte0` states a fact and leaves the interpretation to whoever knows the
    //             protocol - which is the same discipline the kernel follows about payloads.
    //   outcome - how the exchange ENDED. One row per exchange, so this is its whole story.
    let mut t = Table::new(&["seq", "sec", "caller", "peer", "op", "outcome"]);
    let base = if n > 0 && b.len() >= 9 {
        u32::from_le_bytes([b[5], b[6], b[7], b[8]])
    } else {
        0
    };
    for i in 0..n {
        let o = 1 + i * ev;
        if o + ev > b.len() { break; }
        let seq = u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]);
        let at  = u32::from_le_bytes([b[o+4], b[o+5], b[o+6], b[o+7]]);
        let cname = &b[o+8..o+8+pl];
        let clen = cname.iter().position(|&c| c == 0).unwrap_or(pl);
        let pname = &b[o+8+pl..o+8+2*pl];
        let plen = pname.iter().position(|&c| c == 0).unwrap_or(pl);
        let op  = b[o+8+2*pl];
        let kind = b[o+9+2*pl];
        let kname = match kind {
            godspeed_sdk::trace::KIND_REQUEST    => "REQUEST",
            godspeed_sdk::trace::KIND_REPLY      => "REPLY",
            godspeed_sdk::trace::KIND_TIMEOUT    => "TIMEOUT",
            godspeed_sdk::trace::KIND_PEER_LOST  => "PEER_LOST",
            godspeed_sdk::trace::KIND_QUEUE_FULL => "QUEUE_FULL",
            godspeed_sdk::trace::KIND_ABORTED    => "ABORTED",
            _ => "?",
        };
        // `trace failures` is the same ring, FILTERED - not a second recording path. QUEUE_FULL is a
        // failure to get there, so it belongs; ABORTED is the user changing their mind, so it does not.
        if failures_only && !matches!(kind, godspeed_sdk::trace::KIND_TIMEOUT
                                          | godspeed_sdk::trace::KIND_PEER_LOST
                                          | godspeed_sdk::trace::KIND_QUEUE_FULL) {
            continue;
        }
        // The peer NAME came from the emitter, which knew it: it called `request_with_reply("fs", ..)`.
        // No lookup, no endpoint, no cap slot - a slot is local to the emitter and means nothing here.
        // (The EMITTER is not named: a service has no ambient identity to assert, which is the
        // capability model working, not a gap. See `sdk::trace`.)
        let peer = t.intern(&pname[..plen]);
        // A service that never declared itself reads `?`, not a guess (see `sdk::trace`).
        let caller = if clen == 0 { t.intern(b"?") } else { t.intern(&cname[..clen]) };
        let k = t.intern(kname.as_bytes());
        let opv = trace_op_cell(&mut t, &pname[..plen], op);
        t.add_row(&[Value::Int(seq as u64), Value::Int(at.saturating_sub(base) as u64),
                    caller, peer, opv, k]);
    }
    Some(t)
}

/// A frame accumulator for the pager: many lines, few console writes.
///
/// `console_write` is ONE SYSCALL per call, capped at 256 bytes, and each call is a message to the
/// `console` service whose queue is 16 deep. A paged frame is a legend, a header, a screenful of rows
/// and a status line - about sixty writes if each is sent on its own, per KEYPRESS. Holding `j`
/// outruns the sink, the queue backs up, and the keyboard appears dead while the machine is fine.
///
/// This is the same lesson the console service itself learned (`docs/console-service.md`): a repaint
/// is one batch, not one message per line. Text accumulates here and goes out in full 256-byte
/// writes, so a frame costs about a dozen syscalls instead of sixty.
///
/// Bounded and no heap: one fixed buffer, flushed when full and at the end of each frame.
struct FrameBuf {
    buf: [u8; 256],
    n: usize,
}
impl FrameBuf {
    fn new() -> Self { Self { buf: [0u8; 256], n: 0 } }
    fn put(&mut self, ctx: &ServiceContext, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let room = self.buf.len() - self.n;
            let take = rest.len().min(room);
            self.buf[self.n..self.n + take].copy_from_slice(&rest[..take]);
            self.n += take;
            rest = &rest[take..];
            if self.n == self.buf.len() { self.flush(ctx); }
        }
    }
    fn flush(&mut self, ctx: &ServiceContext) {
        if self.n == 0 { return; }
        if let Ok(text) = core::str::from_utf8(&self.buf[..self.n]) { ctx.console_write(text); }
        self.n = 0;
    }
}
/// A bounded line buffer for one rendered grid row.
///
/// The pager repaints IN PLACE (no clear-to-black), so every line it draws must erase its own tail or
/// a shorter row leaves the previous frame's characters hanging off the end - which on a framebuffer
/// console reads as garbage rather than as a short row. `Table::grid_row` writes plain text and its
/// own newline, so the row is buffered here and re-emitted as `text + ESC[K + newline`.
///
/// 256 bytes: a console line is at most a couple of hundred columns and a trace row is about fifty.
/// An over-long row is truncated rather than smearing - bounded, and the visible effect is a clipped
/// line, not a corrupted screen.
struct LineBuf {
    b: [u8; 256],
    n: usize,
}
impl LineBuf {
    fn new() -> Self { Self { b: [0u8; 256], n: 0 } }
    /// Append the buffered line, with an erase-to-end-of-line, to the FRAME - not to the console.
    ///
    /// It used to write straight through, which cost TWO syscalls per row (the text, then the erase).
    /// A screenful is then well over a hundred console messages per keypress against a 16-deep queue,
    /// and holding a scroll key outruns the sink: the keyboard looks dead while the machine is fine.
    fn flush_into(&mut self, ctx: &ServiceContext, f: &mut FrameBuf) {
        let end = self.n.min(self.b.len());
        // Trim the newline `grid_row`/`grid_header` appended; we supply our own after the erase.
        let end = if end > 0 && self.b[end - 1] == b'\n' { end - 1 } else { end };
        f.put(ctx, &self.b[..end]);
        f.put(ctx, b"\x1b[K\n");
        self.n = 0;
    }
}
impl RecordSink for LineBuf {
    fn put(&mut self, bytes: &[u8]) {
        let room = self.b.len().saturating_sub(self.n);
        let n = bytes.len().min(room);
        self.b[self.n..self.n + n].copy_from_slice(&bytes[..n]);
        self.n += n;
    }
}

/// An opcode's NAME, for the protocols this shell can name. `None` when it cannot.
///
/// A number tells you nothing without a lookup table, and a reader who has to hold `11 = read` in
/// their head is doing work the tool should have done. The names cost nothing: they are as short as
/// the numbers once rendered, and they make the column filterable in words - `trace ipc | where
/// op=read`.
///
/// WHERE THIS KNOWLEDGE COMES FROM matters. For `fs` it is the shell's OWN opcode constants - it is
/// an fs client and builds these requests itself, so the names cannot drift from what it sends. For
/// `block-driver` the shell is not a client, so the small table below is a DISPLAY convenience
/// mirroring `services/block-driver`'s constants; if it ever drifts, the effect is a wrong word for a
/// known op, which is why anything unrecognised renders as the bare number instead of a guess.
fn trace_op_name(peer: &[u8], op: u8) -> Option<&'static str> {
    match peer {
        b"fs" => Some(match op {
            OP_WRITE_FILE  => "write",     OP_READ_FILE   => "read",
            OP_STAT_FILE   => "stat",      OP_MKDIR       => "mkdir",
            OP_LIST_DIR    => "list",      OP_RENAME      => "rename",
            OP_DELETE      => "delete",    OP_MOVE        => "move",
            OP_MKDIR_P     => "mkdir-p",   OP_DELETE_TREE => "delete-tree",
            OP_DRIVES_INFO => "drives",    OP_FLASH       => "flash",
            OP_LABEL       => "label",     OP_RESET       => "reset",
            OP_WRITE_NEW   => "write-new", OP_WRITE_AT    => "write-at",
            OP_READ_AT     => "read-at",   OP_CHECK       => "check",
            OP_SCRUB       => "scrub",     OP_OPEN        => "open",
            _ => return None,
        }),
        // Mirrors `services/block-driver/src/main.rs`. See the note above on why this is allowed to
        // be a copy and what happens if it is wrong.
        b"block-driver" => Some(match op {
            1 => "read", 2 => "write", 3 => "capacity", 4 => "zero", 5 => "flush",
            _ => return None,
        }),
        _ => None,
    }
}

/// Render `op` for the grid: its name where we know one, the bare number where we do not.
fn trace_op_cell(t: &mut Table, peer: &[u8], op: u8) -> Value {
    if let Some(name) = trace_op_name(peer, op) {
        return t.intern(name.as_bytes());
    }
    // Unknown protocol or unknown op: the number, as text, so the column stays one type. A mixed
    // Int/Str column would filter and serialise inconsistently.
    let mut b = [0u8; 3];
    let mut n = 0;
    let (h, tens, ones) = (op / 100, (op / 10) % 10, op % 10);
    if h > 0 { b[n] = b'0' + h; n += 1; }
    if h > 0 || tens > 0 { b[n] = b'0' + tens; n += 1; }
    b[n] = b'0' + ones; n += 1;
    t.intern(&b[..n])
}

/// `trace deps <service>` - what a service is WIRED to call, and what it has actually called.
///
/// # Why this is built from capabilities and not from a contract
///
/// The obvious source for "what does fs depend on" is its contract's `ipc_send` list. That is a
/// DECLARATION - a statement of intent that the kernel validated once at spawn. This reads the live
/// capability table instead (`task_caps`), and resolves each endpoint cap back to the task that owns
/// that endpoint. So a row is not "the toml says it may call block-driver"; it is "this service is
/// holding, right now, a SEND capability whose endpoint block-driver owns". Authority as it actually
/// stands, which is what 26.9 asks to be inspectable.
///
/// The second column is the interesting one. A peer it HOLDS authority for but has never called is
/// authority it does not appear to need - the exact thing a least-privilege review is looking for
/// (3.1), and invisible from either source alone.
///
/// `calls` counts only what the ring still holds, so it is a recent window and not a lifetime total.
/// A `0` there means "not in the last 64 events", never "never".
/// `#[inline(never)]`, like every sibling table builder: this frame holds a multi-KB `Table` plus the
/// peer and op scratch arrays, and inlining it into `pipe_run` would add all of that to EVERY
/// pipeline's frame - including byte-only ones that never build a record. The shell's user stack is
/// 256 KiB and `pipe_run` already sits near it.
#[inline(never)]
fn build_deps_table(ctx: &ServiceContext, name: &str) -> Option<Table> {
    if name.is_empty() {
        ctx.console_writeln("usage: trace deps <service>");
        return None;
    }
    let slot = match slot_of(ctx, name) {
        Some(s) => s,
        None => {
            ctx.console_writeln_fmt(format_args!("trace deps: no live service named '{}'", name));
            return None;
        }
    };

    // 1. Authority: every endpoint cap this service holds that carries SEND, resolved to its owner.
    let mut caps = [CapInfo::default(); 64];
    let n = ctx.task_caps(slot, &mut caps);
    let mut peers  = [[0u8; 24]; 16];
    let mut plens  = [0usize; 16];
    let mut grantable = [false; 16];
    let mut np     = 0usize;
    for cap in caps.iter().take(n) {
        // Rights bitfield (sdk CapInfo): READ=1 WRITE=2 SEND=4 RECV=8 GRANT=16 REVOKE=32.
        //
        // EVERY send capability, and the GRANT bit REPORTED rather than used as a filter.
        //
        // This filtered `SEND|GRANT` out, because a reply capability carries GRANT (it is derived from
        // the caller's self-grant) and counting those had `logger` "calling" the shell. But a peer the
        // SUPERVISOR provides at spawn carries GRANT too - it must, or the supervisor could not
        // re-delegate it - so the filter also deleted real wiring: `net-stack` showed no `nic-driver`
        // dependency at all, right after a ping that demonstrably used it.
        //
        // The two are indistinguishable from here: same right, same shape, and the kernel does not
        // record which is which. Dropping them hides real dependencies; including them silently shows
        // return addresses as dependencies. So they are included and MARKED, and the legend says a
        // grantable edge may be either. An honest ambiguity beats a confident wrong answer in either
        // direction (26.7).
        if cap.rights & 4 == 0 { continue; }
        // A stable kernel resource (log_write, spawn, ...) is not a peer; only endpoints resolve.
        let owner = match trace_owner_of(ctx, cap.resource_id) { Some(o) => o, None => continue };
        let h = ctx.task_stat(owner);
        let l = h.name_len.min(24);
        if l == 0 { continue; }
        // Its own endpoint is not a dependency.
        if &h.name[..l] == name.as_bytes() { continue; }
        let mut dup = false;
        for i in 0..np { if plens[i] == l && peers[i][..l] == h.name[..l] { dup = true; break; } }
        if dup || np == peers.len() { continue; }
        peers[np][..l].copy_from_slice(&h.name[..l]);
        plens[np] = l;
        grantable[np] = cap.rights & 16 != 0;
        np += 1;
    }

    // 2. Observation: what the ring saw this caller do. Absent ring, the authority half still stands.
    let mut calls  = [0u32; 16];
    let mut failed = [0u32; 16];
    let mut opbuf  = [[0u8; 40]; 16];
    let mut oplen  = [0usize; 16];
    if let Some(ev) = build_trace_table(ctx, false) {
        for r in 0..ev.nrows() {
            let (c, pr, op, oc) = (ev.cell_bytes(r, 2), ev.cell_bytes(r, 3),
                                   ev.cell_bytes(r, 4), ev.cell_bytes(r, 5));
            if c != name.as_bytes() { continue; }
            let mut idx = None;
            for i in 0..np { if plens[i] == pr.len() && peers[i][..plens[i]] == *pr { idx = Some(i); break; } }
            // A peer seen in the ring but holding no cap now: it was reacquired or the peer restarted.
            // Record it rather than dropping it - a call that happened is a fact.
            let i = match idx {
                Some(i) => i,
                None if np < peers.len() && !pr.is_empty() && pr.len() <= 24 => {
                    peers[np][..pr.len()].copy_from_slice(pr);
                    plens[np] = pr.len();
                    np += 1;
                    np - 1
                }
                None => continue,
            };
            calls[i] += 1;
            if oc != b"REPLY" { failed[i] += 1; }
            // Collect distinct op names, space separated, bounded by the cell.
            let mut seen = false;
            let cur = &opbuf[i][..oplen[i]];
            let mut k = 0;
            while k < cur.len() {
                let mut e = k;
                while e < cur.len() && cur[e] != b' ' { e += 1; }
                if &cur[k..e] == op { seen = true; break; }
                k = e + 1;
            }
            if !seen && oplen[i] + op.len() + 1 < opbuf[i].len() {
                if oplen[i] > 0 { opbuf[i][oplen[i]] = b' '; oplen[i] += 1; }
                let l = oplen[i];
                opbuf[i][l..l + op.len()].copy_from_slice(op);
                oplen[i] += op.len();
            }
        }
    }

    let mut t = Table::new(&["depth", "parent", "peer", "grant", "calls", "failed", "ops"]);
    for i in 0..np {
        let pv = t.intern(&peers[i][..plens[i]]);
        let par = t.intern(name.as_bytes());
        let ov = if oplen[i] == 0 { t.intern(b"-") } else { t.intern(&opbuf[i][..oplen[i]]) };
        let gv = t.intern(if grantable[i] { b"grantable" } else { b"-" });
        t.add_row(&[Value::Int(1), par, pv, gv, Value::Int(calls[i] as u64),
                    Value::Int(failed[i] as u64), ov]);
    }
    // Then walk BELOW each direct peer, so the answer is the service's transitive reach rather than
    // one level of it - `shell -> fs -> block-driver` is the storage stack, and seeing only the first
    // arrow answers a smaller question than the one being asked.
    // The ancestry of each branch: the root, then whatever that branch has descended through. Only
    // this is used to stop a cycle - see `deps_level`.
    let mut path  = [[0u8; 24]; 8];
    let mut pl    = [0usize; 8];
    let l0 = name.len().min(24);
    path[0][..l0].copy_from_slice(&name.as_bytes()[..l0]);
    pl[0] = l0;
    for i in 0..np {
        path[1][..plens[i]].copy_from_slice(&peers[i][..plens[i]]);
        pl[1] = plens[i];
        deps_level(ctx, &mut t, &peers[i][..plens[i]], 2, &mut path, &mut pl, 2);
    }
    Some(t)
}

/// One level of `trace deps`, appended to `t` at `depth` under `parent`.
///
/// Split from the root call so the walk can RECUR without the root's ring pass being redone at every
/// level: the observed counts come from a single scan of the event table, which the caller does once
/// and threads down. A level that finds nothing simply adds no rows.
#[inline(never)]
fn deps_level(ctx: &ServiceContext, t: &mut Table, parent: &[u8], depth: u64,
              path: &mut [[u8; 24]; 8], plens: &mut [usize; 8], pdepth: usize) {
    if depth > DEPS_MAX_DEPTH || t.nrows() >= DEPS_MAX_ROWS { return; }
    let pname = match core::str::from_utf8(parent) { Ok(s) => s, Err(_) => return };
    let slot = match slot_of(ctx, pname) { Some(s) => s, None => return };
    let mut caps = [CapInfo::default(); 64];
    let n = ctx.task_caps(slot, &mut caps);
    // Children of THIS parent, deduplicated. A service can hold several caps to one peer (a wired one
    // and a copy it derived), and a row per CAP drew `fs` twice under `time` and `block-driver` twice
    // under `fs`. Distinctness of children and absence of cycles are different problems: this is the
    // first, the `path` check below is the second, and collapsing them into one "seen" set is what
    // flattened the tree in the previous version.
    for cap in caps.iter().take(n) {
        // EVERY send cap, GRANT or not - see the note in `build_deps_table` on why filtering GRANT
        // was wrong.
        if cap.rights & 4 == 0 { continue; }
        // AN ENDPOINT WITH NO LIVE OWNER IS THE MOST INTERESTING ROW HERE, and it used to be dropped
        // in silence - which is how `net-stack` appeared to have no `nic-driver` dependency at all.
        // A service holding a send cap to something that no longer exists is a fact worth printing:
        // it is a stale cap, and the next send on it returns EndpointDead (14.3).
        let owner = match trace_owner_of(ctx, cap.resource_id) {
            Some(o) => o,
            None => {
                if t.nrows() < DEPS_MAX_ROWS {
                    // NAMED FOR WHAT IT IS, AND STILL QUERYABLE. `task_stat` reports a task's PRIMARY
                    // endpoint, and a reply capability targets the caller's REPLY-only endpoint - so a
                    // cap that resolves to no live task is, in practice, a return address. Bare
                    // `endpoint#119` made a reader stop and decode an id that means nothing to them;
                    // `reply#119` says what it is AND keeps the id, which `trace endpoint 119` can
                    // answer. A number is only noise when it is a dead end.
                    let mut nb = [0u8; 24];
                    let mut q = 0usize;
                    write_bytes(&mut nb, &mut q, b"reply#");
                    write_u32(&mut nb, &mut q, cap.resource_id as u32);
                    let pv = t.intern(&nb[..q]);
                    let par = t.intern(parent);
                    let stale = t.intern(b"-");
                    let gv = t.intern(if cap.rights & 16 != 0 { b"grantable" } else { b"-" });
                    t.add_row(&[Value::Int(depth), par, pv, gv, Value::Int(0), Value::Int(0), stale]);
                }
                continue;
            }
        };
        let h = ctx.task_stat(owner);
        let l = h.name_len.min(24);
        if l == 0 || &h.name[..l] == parent { continue; }
        // CYCLE GUARD ON THE PATH, not on the whole tree.
        //
        // This was a global "seen" set, and it was wrong: `block-driver` is a direct peer of the shell
        // AND a peer of `fs`, so once it was drawn under the shell it could never be drawn under `fs`,
        // and `trace deps shell` showed `fs  24 calls` with nothing beneath it. That is precisely the
        // transitive reach the tree exists to show, deleted by an over-eager guard.
        //
        // A node MAY appear under several parents - a dependency graph is a DAG and that is what a DAG
        // looks like. What must never happen is a node appearing as its own ancestor, which is the
        // actual cycle, so the check is against this branch's ancestry alone.
        let mut cyclic = false;
        for i in 0..pdepth { if plens[i] == l && path[i][..l] == h.name[..l] { cyclic = true; break; } }
        if cyclic { continue; }
        // The EDGE must be unique, not merely unique within this call. A node can be reached twice
        // (`time` directly, and again through `net-stack`), and each visit ran its own dedup - so the
        // table ended up holding `time -> fs` twice and the renderer, which finds children by parent
        // NAME, drew both under every occurrence of `time`. Deduplicate the pair against the table
        // itself and the question does not arise.
        let mut dup = false;
        for r in 0..t.nrows() {
            if t.cell_bytes(r, 1) == parent && t.cell_bytes(r, 2) == &h.name[..l] { dup = true; break; }
        }
        if dup { continue; }
        if t.nrows() >= DEPS_MAX_ROWS { return; }
        let pv = t.intern(&h.name[..l]);
        let par = t.intern(parent);
        let dash = t.intern(b"-");
        let gv = t.intern(if cap.rights & 16 != 0 { b"grantable" } else { b"-" });
        t.add_row(&[Value::Int(depth), par, pv, gv, Value::Int(0), Value::Int(0), dash]);
        let mut child = [0u8; 24];
        child[..l].copy_from_slice(&h.name[..l]);
        if pdepth < path.len() {
            path[pdepth][..l].copy_from_slice(&h.name[..l]);
            plens[pdepth] = l;
            deps_level(ctx, t, &child[..l], depth + 1, path, plens, pdepth + 1);
        }
    }
}

/// How deep the dependency walk goes, and how many edges it will draw.
///
/// Bounded because a walk over live capability state has no natural end (26.6): a cycle would run
/// forever without the guard in `deps_level`, and even an acyclic graph could be wide. Three levels
/// is `shell -> fs -> block-driver`, which is the whole storage stack; a fourth has never existed
/// here. Both limits are visible in the output rather than silently applied - see `trace_deps`.
///
/// 48 edges, not 24: at 24 the notice fired on EVERY `trace deps shell`, and a bound that always
/// trips is a bound set too low - it stops being information and becomes furniture the reader learns
/// to ignore, which is worse than no notice at all.
const DEPS_MAX_DEPTH: u64 = 3;
const DEPS_MAX_ROWS: usize = 48;

/// What the columns mean, printed above the grid on the CONSOLE path only.
///
/// Not in the pipe path: `trace ipc | to json` must emit records and nothing else, so a legend there
/// would be corrupting the stream with prose.
fn trace_legend(ctx: &ServiceContext, f: &mut FrameBuf, n: usize) {
    f.put(ctx, b"--------------------------------- legend ---------------------------------\x1b[K\n");
    f.put(ctx, b"seq     the caller's own count - a gap is an event that never arrived\x1b[K\n");
    f.put(ctx, b"sec     when the ring recorded it, from the oldest row shown - 1s steps\x1b[K\n");
    f.put(ctx, b"caller  who called | peer  who was called\x1b[K\n");
    f.put(ctx, b"op      what was asked for - a number when this shell cannot name it\x1b[K\n");
    f.put(ctx, b"outcome REPLY | TIMEOUT | PEER_LOST | QUEUE_FULL | ABORTED\x1b[K\n");
    let mut nb = [0u8; 24];
    let mut q = 0usize;
    write_u32(&mut nb, &mut q, n as u32);
    f.put(ctx, b"----------------------------- IPC events (");
    f.put(ctx, &nb[..q]);
    f.put(ctx, b") -----------------------------\x1b[K\n");
}

/// Lines [`trace_legend`] draws, so the pager can size its scrolling area.
const TRACE_LEGEND_LINES: usize = 8;

fn trace_events(ctx: &ServiceContext, failures_only: bool) -> Result<(), ShellError> {
    let t = match build_trace_table(ctx, failures_only) {
        Some(t) => t,
        None    => return Err(ShellError::Unknown),
    };
    if t.nrows() == 0 {
        ctx.console_writeln(if failures_only { "trace: no failure events recorded" }
                            else { "trace: no events recorded (is any service granted ipc_send=[\"logger\"]?)" });
        return Ok(());
    }
    // PAGE when it does not fit, exactly as `help` does - a ring dump is routinely taller than the
    // screen, and the framebuffer console has no scrollback, so the top would otherwise be gone
    // forever. Unknown geometry is not "no terminal": a failed `console_dims` returns 0, and `edit`
    // already treats that as 24 rows rather than dropping the feature.
    let (rows, _cols) = ctx.console_dims();
    let rows = if rows == 0 { 24 } else { rows as usize };
    let w = t.grid_widths();
    // 2 legend lines + 1 column header + 1 status line.
    if t.nrows() + TRACE_LEGEND_LINES + 2 <= rows {
        // The unpaged path batches too: it is the same screenful, drawn once.
        let mut f = FrameBuf::new();
        trace_legend(ctx, &mut f, t.nrows());
        let mut lb = LineBuf::new();
        t.grid_header(&mut lb, &w);
        lb.flush_into(ctx, &mut f);
        for r in 0..t.nrows() {
            t.grid_row(&mut lb, r, &w);
            lb.flush_into(ctx, &mut f);
        }
        f.flush(ctx);
        return Ok(());
    }
    // PINNED: the legend and the column header are repainted at the top of every frame. They used to
    // be printed before the pager started, which put them exactly where its first `ESC[H` repaint
    // lands - so on a framebuffer console the legend flashed and vanished. The column header had the
    // same fate one page in, having been line 0 of the scrolling region.
    //
    // ONE FRAME, A DOZEN SYSCALLS. Every line here goes into a shared `FrameBuf` and out in 256-byte
    // writes, flushed just before the status line. Writing each line straight to the console cost two
    // syscalls per row - over a hundred console messages per keypress against a 16-deep queue - so
    // holding a scroll key outran the sink and the keyboard went unresponsive.
    let frame = core::cell::RefCell::new(FrameBuf::new());
    line_pager(ctx, t.nrows(), rows,
        &|c| {
            let mut f = frame.borrow_mut();
            trace_legend(c, &mut f, t.nrows());
            let mut lb = LineBuf::new();
            t.grid_header(&mut lb, &w);
            lb.flush_into(c, &mut f);
            TRACE_LEGEND_LINES + 1 // the legend block, plus the column header
        },
        &|c, i| {
            let mut f = frame.borrow_mut();
            let mut lb = LineBuf::new();
            t.grid_row(&mut lb, i, &w);
            lb.flush_into(c, &mut f);
        },
        &|c| frame.borrow_mut().flush(c));
    Ok(())
}

/// `trace status` - ring capacity, events accepted, events DROPPED.
///
/// The drop count is the point. A ring that silently discards is the failure this project just fixed
/// in the x86 keyboard path; one that reports what it lost is an instrument you can trust the rest of
/// (invariant 12).
/// Ask the ring-holding service, RETRYING a busy sink.
///
/// A single attempt was reporting a service that is alive and busy as "unavailable". The sink takes
/// events and control requests on one 16-deep endpoint, so a burst of events can fill it and reject
/// the reader - which is congestion, not absence, and the two must not be reported the same way
/// (the same distinction `KIND_QUEUE_FULL` exists for). Bounded: three attempts, then it says so.
fn trace_ask(ctx: &ServiceContext, req: &[u8]) -> Option<Message> {
    for _ in 0..3 {
        if let Some(r) = ctx.request_with_reply("logger", &Message::from_bytes(req)) {
            return Some(r);
        }
        // REACQUIRE BETWEEN ATTEMPTS. A busy sink is transient and a yield is the right answer, but a
        // RESTARTED one never recovers by waiting: the cap is stale and every retry fails identically.
        // After a chaos storm restarted `logger` forty times, this loop failed three times in a row
        // and reported a live service as unreachable (14.3 - reacquire by name, then retry).
        let _ = ctx.reacquire_by_name("logger");
        ctx.yield_cpu();
    }
    None
}

/// Draw the edges under `parent` as a tree, the way `tree` draws directories.
///
/// # Why this walks the EDGES instead of printing rows in order
///
/// The table is an edge list, and its rows are not in depth-first order - the walk collects a
/// service's direct peers first, then descends into each. Printing rows sequentially with an indent
/// would therefore have drawn a tree whose shape was an artefact of collection order. Following
/// `parent -> peer` at render time is correct for any order, which is also what makes the same table
/// safe to pipe.
///
/// Connectors are `tree`'s: `|--` for a child with siblings after it, `\--` for the last, and an
/// ancestor that was not its parent's last child leaves a `|` continuation so the lines join up.
/// Bounded by construction: `DEPS_MAX_DEPTH` levels and a fixed prefix buffer, so a cycle in the
/// data (there cannot be one - `deps_level` guards names) still could not run away here.
fn deps_draw(ctx: &ServiceContext, t: &Table, parent: &[u8], prefix: &mut [u8; 48],
             plen: usize, depth: usize, anc: &mut [[u8; 24]; 8], nanc: usize) {
    if depth as u64 >= DEPS_MAX_DEPTH + 1 { return; }
    // Find this parent's children, so the LAST one can draw the closing connector.
    // Children of this parent - EXCLUDING reply addresses, which are counted for the closing note
    // instead. They are in the record (a piper can see them) but they are not dependencies, and a
    // `reply` node under every service is noise a reader has to look past on every line.
    let mut kids = [0usize; DEPS_MAX_ROWS];
    let mut nk = 0usize;
    for r in 0..t.nrows() {
        if t.cell_bytes(r, 1) != parent { continue; }
        if t.cell_bytes(r, 2).starts_with(b"reply#") { continue; }
        if nk < kids.len() { kids[nk] = r; nk += 1; }
    }
    for k in 0..nk {
        let r = kids[k];
        let last = k + 1 == nk;
        let peer   = t.cell_bytes(r, 2);
        let grant  = t.cell_bytes(r, 3);
        let calls  = t.cell_int(r, 4).unwrap_or(0);
        let failed = t.cell_int(r, 5).unwrap_or(0);
        let ops    = t.cell_bytes(r, 6);
        // The `grant` bit stays in the RECORD for anyone who filters on it, but not in the tree: with
        // reply addresses now named, a grantable edge to a live service is a supervisor-wired peer,
        // and a marker on nearly every row is decoration rather than information.
        let _ = grant;
        let pre  = core::str::from_utf8(&prefix[..plen]).unwrap_or("");
        let conn = if last { "\u{2514}\u{2500}\u{2500} " } else { "\u{251c}\u{2500}\u{2500} " };
        let pname = core::str::from_utf8(peer).unwrap_or("?");
        let oname = core::str::from_utf8(ops).unwrap_or("-");
        if calls == 0 && peer == godspeed_sdk::trace::SINK_NAME.as_bytes() {
            // THE SINK ALWAYS READS 0, and that is not idle authority - it is the most-used capability
            // on this row. Emissions to the ring are deliberately never recorded (an observer that
            // records itself fills the ring with its own questions), so without this the line would
            // say "granted and unused" about the one capability that makes tracing work at all - and a
            // reader tidying up unused authority would revoke exactly the wrong thing.
            ctx.console_write_fmt(format_args!(
                "{}{}{}  (trace sink - its own traffic is never recorded)\x1b[K\n", pre, conn, pname));
        } else if calls == 0 {
            ctx.console_write_fmt(format_args!("{}{}{}\x1b[K\n", pre, conn, pname));
        } else if failed == 0 {
            ctx.console_write_fmt(format_args!(
                "{}{}{}  {} calls  ({})\x1b[K\n", pre, conn, pname, calls, oname));
        } else {
            ctx.console_write_fmt(format_args!(
                "{}{}{}  {} calls  {} FAILED  ({})\x1b[K\n", pre, conn, pname, calls, failed, oname));
        }
        // Extend the prefix for this child's own children: a continuation bar when it has siblings
        // below it, blanks when it was the last.
        let piece: &[u8] = if last { b"    " } else { "\u{2502}   ".as_bytes() };
        if plen + piece.len() <= prefix.len() {
            prefix[plen..plen + piece.len()].copy_from_slice(piece);
            let mut child = [0u8; 24];
            let cl = peer.len().min(child.len());
            child[..cl].copy_from_slice(&peer[..cl]);
            // A node already on this line of descent is a CYCLE (`time` reaches `net-stack`, which
            // reaches `time`). Draw it, then stop: expanding it again would repeat a subtree the
            // reader has already seen, and the graph is a DAG only if you ignore that edge.
            let mut seen_anc = false;
            for i in 0..nanc { if anc[i][..cl] == child[..cl] && anc[i][cl..].iter().all(|&b| b == 0) { seen_anc = true; break; } }
            if !seen_anc && nanc < anc.len() {
                // CLEAR the slot first. A sibling branch may have left a longer name here ("net-stack"),
                // and writing "time" over it leaves "stack" trailing - so the all-zero tail check
                // failed and the ancestor was not recognised, which let a cycle expand one more time.
                anc[nanc] = [0u8; 24];
                anc[nanc][..cl].copy_from_slice(&peer[..cl]);
                deps_draw(ctx, t, &child[..cl], prefix, plen + piece.len(), depth + 1, anc, nanc + 1);
            }
        }
    }
}

/// `trace endpoints` - every live task's endpoint, the map from names to the ids.
///
/// The missing fourth source. Ids arrive from `caps <service>` (as `endpoint#N`), from `trace
/// blocked`'s `awaiting` column and from `trace deps`' reply list - all of which hand you ONE id in
/// context. None of them answers "what endpoints exist", so the map from a service name to its number
/// had to be assembled by hand before `trace endpoint <id>` could be used deliberately.
///
/// A record source like every other view, so it filters and pipes: `trace endpoints | where
/// name contains fs`, `| to json`.
///
/// Only PRIMARY endpoints appear, because that is what the kernel reports per task. A reply-only
/// mailbox has no name here - which is precisely why an unresolvable cap shows as `reply#NNN` in a
/// dependency tree rather than as a service.
#[inline(never)]
fn build_endpoints_table(ctx: &ServiceContext) -> Table {
    let mut t = Table::new(&["slot", "name", "endpoint", "state", "queue"]);
    for slot in 0u32..256 {
        let st = ctx.task_stat(slot);
        if !st.valid || st.state == 4 { continue; }
        let ep = ctx.task_own_endpoint(slot);
        if ep == 0 { continue; }   // a task with no endpoint cannot be talked to; nothing to list
        let name  = t.intern(&st.name[..st.name_len.min(31)]);
        let state = t.intern(st.state_str().as_bytes());
        t.add_row(&[Value::Int(slot as u64), name, Value::Int(ep),
                    state, Value::Int(st.queue_depth as u64)]);
    }
    t
}

/// `trace endpoints` on the console.
fn trace_endpoints(ctx: &ServiceContext) -> Result<(), ShellError> {
    let t = build_endpoints_table(ctx);
    if t.nrows() == 0 {
        ctx.console_writeln("trace endpoints: no live task owns an endpoint");
        return Ok(());
    }
    ctx.console_writeln("--------------------------------- legend ---------------------------------");
    ctx.console_writeln("endpoint  the id to pass to `trace endpoint <id>`; queue = messages waiting");
    ctx.console_writeln("only PRIMARY endpoints - a reply mailbox has no name, hence reply#NNN");
    ctx.console_writeln("------------------------------ live endpoints -----------------------------");
    let mut o = Out::Console;
    t.to_grid(&mut OutSink { ctx, out: &mut o });
    Ok(())
}

/// `trace endpoint <id>` - what an endpoint is, and WHO CAN REACH IT.
///
/// The inverse of `trace deps`, and the question the capability model makes worth asking: `deps` says
/// who a service calls, this says who holds authority over a given endpoint. There was no way to ask
/// it before - `caps <service>` answers one holder at a time, and you cannot enumerate holders from a
/// resource.
///
/// It also turns the `reply#NNN` rows in a dependency tree from a dead end into a lookup. An endpoint
/// with no live owner is not a mystery once you can see it is held by two services and owned by none:
/// it is a return address whose task has moved on.
///
/// Bounded: a scan of the task table (256 slots, the same bound `status` uses) reading each task's
/// caps. No allocation, no recursion.
fn trace_endpoint(ctx: &ServiceContext, arg: &str) -> Result<(), ShellError> {
    let id: u64 = match arg.trim().trim_start_matches('#').parse() {
        Ok(v) => v,
        Err(_) => {
            ctx.console_writeln("trace endpoint: needs an endpoint id, e.g. `trace endpoint 119`");
            return Err(ShellError::Unknown);
        }
    };
    // A STABLE KERNEL RESOURCE IS NOT AN ENDPOINT. Ids 1-5 are `log_write`, `spawn`, `console_read`,
    // `console_push`, `introspect`; calling id 4 "an endpoint with no live owner" was true of the
    // lookup and false about the thing - it has no owner because nothing owns it, by design.
    let mut rb = [0u8; 32];
    let rlen = cap_resource_name(id, &mut rb);
    let rname = core::str::from_utf8(&rb[..rlen]).unwrap_or("?");
    if !rname.starts_with("endpoint#") {
        ctx.console_writeln_fmt(format_args!(
            "{} (id {}) - a kernel resource, not an endpoint: it has no owning task", rname, id));
        return trace_endpoint_holders(ctx, id);
    }
    match trace_owner_of(ctx, id) {
        Some(o) => {
            let h = ctx.task_stat(o);
            ctx.console_writeln_fmt(format_args!(
                "endpoint {} - owned by task {} \"{}\" ({})", id, o, h.name_str(), h.state_str()));
        }
        None => ctx.console_writeln_fmt(format_args!(
            "endpoint {} - NO LIVE OWNER. Either its task died, or it is a reply-only endpoint (a \
             task's reply mailbox is not its primary one, and only primaries are named here)", id)),
    }
    trace_endpoint_holders(ctx, id)
}

/// Every live task holding a capability to `id`. Split out so the kernel-resource path above can
/// reuse it - "who can reach this" is the same question whether the target is an endpoint or not.
fn trace_endpoint_holders(ctx: &ServiceContext, id: u64) -> Result<(), ShellError> {
    let mut t = Table::new(&["holder", "slot", "rights"]);
    for slot in 0u32..256 {
        let st = ctx.task_stat(slot);
        if !st.valid || st.state == 4 { continue; }
        let mut caps = [CapInfo::default(); 64];
        let n = ctx.task_caps(slot, &mut caps);
        for cap in caps.iter().take(n) {
            if cap.resource_id != id { continue; }
            let name = t.intern(&st.name[..st.name_len.min(31)]);
            let mut gb = [0u8; 48];
            let glen = cap_rights_str(cap.rights, &mut gb);
            let rights = t.intern(&gb[..glen]);
            t.add_row(&[name, Value::Int(slot as u64), rights]);
        }
    }
    if t.nrows() == 0 {
        ctx.console_writeln("no live task holds a capability to it");
        return Ok(());
    }
    ctx.console_writeln("held by:");
    let mut o = Out::Console;
    t.to_grid(&mut OutSink { ctx, out: &mut o });
    Ok(())
}

/// `trace deps <service>` on the console: the dependency graph, drawn as a tree.
///
/// # A tree and a record stream are the same data
///
/// The table this renders holds one row per EDGE (`parent`, `peer`, `depth`), which is what a tree
/// is. So the console draws the edges as a tree and a pipe gets the rows - one producer, two
/// renderings, exactly as `trace ipc` does for its grid, its pager and its pipe. Neither form is a
/// lossy summary of the other, which is what makes it safe to have both:
///
///   trace deps shell                      -> the tree
///   trace deps shell | where peer=fs      -> the edges into fs
///   trace deps shell | to json            -> the graph, machine-readable
fn trace_deps(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    let t = match build_deps_table(ctx, name) {
        Some(t) => t,
        None    => return Err(ShellError::Unknown),
    };
    if t.nrows() == 0 {
        ctx.console_writeln_fmt(format_args!(
            "trace deps: '{}' holds no send capability to another service - it calls no one", name));
        return Ok(());
    }
    ctx.console_write("--------------------------------- legend ---------------------------------\x1b[K\n");
    ctx.console_write("indent  who calls whom: a child is a service its parent holds a SEND cap to\x1b[K\n");
    ctx.console_write("calls   how many the ring still holds - a recent window, never a lifetime\x1b[K\n");
    ctx.console_write("0 calls authority held but unused here (worth asking why it is granted)\x1b[K\n");
    // THE TREE HIDES ITS OWN COLUMNS. A reader looking at an indented list has no way to know the rows
    // are records, so the filter examples in the footer arrive out of nowhere.
    //
    // POINT AT THE GRID, do not list the columns here. The first version copied them into this line
    // and immediately got it wrong: it named five of the seven, dropping `grant` and `failed`, so a
    // reader would never have learned that `where failed>0` works. The grid header IS the list, it
    // cannot drift from itself, and a copy that can drift is a second truth (26.4).
    ctx.console_write("as a table (its header names every column you can filter on): | to grid\x1b[K\n");
    ctx.console_write_fmt(format_args!(
        "------------------------- {} dependencies -------------------------\x1b[K\n", name));

    ctx.console_writeln(name);
    let mut anc = [[0u8; 24]; 8];
    let nl = name.len().min(24);
    anc[0][..nl].copy_from_slice(&name.as_bytes()[..nl]);
    deps_draw(ctx, &t, name.as_bytes(), &mut [0u8; 48], 0, 0, &mut anc, 1);

    // Reply addresses, reported ONCE rather than as a node under every service. Nothing is hidden -
    // they are rows in the table and `trace deps <svc> | to grid` shows them - but they are return
    // addresses, not dependencies, and the tree is for the dependencies.
    // COUNT, and point at the view that lists them. This used to print the ids inline, and at ten of
    // them the line was already a paragraph - and worse, it lied twice: a 96-byte id buffer silently
    // dropped the tenth id while the count still said ten, and the same endpoint appeared more than
    // once because a reply address is reached under several parents.
    //
    // A summary line must not grow with the data. The rows are already IN the record, with the parent
    // that holds each one, so the honest answer is a count plus the pipe that renders them properly.
    let mut replies = 0usize;
    for r in 0..t.nrows() { if t.cell_bytes(r, 2).starts_with(b"reply#") { replies += 1; } }
    if replies > 0 {
        ctx.console_writeln_fmt(format_args!(
            "({} reply address(es) hidden - `trace deps {} | where peer contains reply` lists them)",
            replies, name));
    }
    // The walk is bounded (26.6), and a bound reached in silence is a lie about completeness.
    if t.nrows() >= DEPS_MAX_ROWS {
        ctx.console_writeln_fmt(format_args!(
            "trace deps: stopped at {} edges - the graph is larger than this view", DEPS_MAX_ROWS));
    }
    Ok(())
}

fn trace_status(ctx: &ServiceContext) -> Result<(), ShellError> {
    let req = [godspeed_sdk::trace::TRACE_OP_STATUS];
    let reply = match trace_ask(ctx, &req) {
        Some(r) => r,
        None => {
            ctx.console_writeln("trace: the logger service did not answer in 3 attempts (it holds the ring)");
            return Err(ShellError::Unknown);
        }
    };
    let b = reply.payload_bytes();
    if b.len() < 24 {
        ctx.console_writeln("trace: logger returned a short status");
        return Err(ShellError::Unknown);
    }
    let cap = u64::from_le_bytes([b[0],b[1],b[2],b[3],b[4],b[5],b[6],b[7]]);
    let total = u64::from_le_bytes([b[8],b[9],b[10],b[11],b[12],b[13],b[14],b[15]]);
    let dropped = u64::from_le_bytes([b[16],b[17],b[18],b[19],b[20],b[21],b[22],b[23]]);
    ctx.console_writeln_fmt(format_args!(
        "trace: ring {} events; {} recorded; {} DROPPED (oldest overwritten before being read)",
        cap, total, dropped));
    ctx.console_writeln("trace: the ring lives in the `logger` service - the kernel records nothing.");
    Ok(())
}

/// The first live task with this name, or `None`.
fn trace_slot_of_name(ctx: &ServiceContext, want: &str) -> Option<u32> {
    for slot in 0..TRACE_SLOTS {
        let s = ctx.task_stat(slot);
        if !s.valid { continue; }
        if &s.name[..s.name_len.min(31)] == want.as_bytes() { return Some(slot); }
    }
    None
}

/// The live task owning `endpoint`, or `None` if nothing owns it - which is itself the answer when a
/// task awaits an endpoint whose service has died and not yet come back.
fn trace_owner_of(ctx: &ServiceContext, endpoint: u64) -> Option<u32> {
    if endpoint == 0 { return None; }
    for slot in 0..TRACE_SLOTS {
        let s = ctx.task_stat(slot);
        if !s.valid { continue; }
        if ctx.task_own_endpoint(slot) == endpoint { return Some(slot); }
    }
    None
}

/// How a task is stuck, as one word. `call` is the interesting one: blocked in a synchronous CALL with
/// the awaited endpoint recorded, which is what makes the chain walkable at all.
fn trace_block_kind(state: u8, awaits: u64) -> &'static str {
    if awaits != 0 { return "call"; }
    match state {
        2 => "recv",
        3 => "send",
        _ => "-",
    }
}

/// `trace blocked` - every task blocked on another task, as a record table (so it pipes).
fn trace_blocked(ctx: &ServiceContext) -> Result<(), ShellError> {
    let mut t = Table::new(&["slot", "name", "blocked", "awaiting", "held_by"]);
    let mut n = 0u32;
    for slot in 0..TRACE_SLOTS {
        let s = ctx.task_stat(slot);
        if !s.valid { continue; }
        let awaits = ctx.task_awaits_endpoint(slot);
        // A task blocked on RECV with nothing awaited is IDLE, not stuck - it is waiting for work,
        // which is what a healthy service does all day. Listing those would bury the handful that
        // matter under every service on the machine.
        if awaits == 0 && s.state != 3 { continue; }
        let name = t.intern(&s.name[..s.name_len.min(31)]);
        let kind = t.intern(trace_block_kind(s.state, awaits).as_bytes());
        let held = match trace_owner_of(ctx, awaits) {
            Some(o) => {
                let h = ctx.task_stat(o);
                t.intern(&h.name[..h.name_len.min(31)])
            }
            None if awaits != 0 => t.intern(b"NO LIVE OWNER"),
            None => t.intern(b"-"),
        };
        t.add_row(&[Value::Int(slot as u64), name, kind, Value::Int(awaits), held]);
        n += 1;
    }
    if n == 0 {
        ctx.console_writeln("no task is blocked on another task.");
        return Ok(());
    }
    { let mut o = Out::Console; t.to_grid(&mut OutSink { ctx, out: &mut o }); }
    if t.overflow() {
        ctx.console_writeln_fmt(format_args!("trace: more than {} rows shown (bounded)", REC_MAX_ROWS));
    }
    Ok(())
}

/// `trace chain <name|slot>` - who a task is stuck behind right now, as a tree.
///
/// Walks `awaited endpoint -> owning task -> what IT awaits`. Bounded two ways, because a stuck system
/// is precisely where an unbounded walk would hang: a depth cap, and a repeat-visit check that names
/// the cycle. 8.9 says the kernel does not detect deadlock - so a cycle surfacing HERE, in an operator
/// tool, is exactly where it belongs.
fn trace_chain(ctx: &ServiceContext, root: u32) -> Result<(), ShellError> {
    let s = ctx.task_stat(root);
    if !s.valid {
        ctx.console_writeln_fmt(format_args!("trace: slot {} holds no live task", root));
        return Err(ShellError::Unknown);
    }
    let mut seen = [u32::MAX; 16];
    let mut nseen = 0usize;
    let mut cur = root;
    let mut depth = 0usize;
    loop {
        let st = ctx.task_stat(cur);
        if !st.valid { break; }
        let awaits = ctx.task_awaits_endpoint(cur);
        let name = core::str::from_utf8(&st.name[..st.name_len.min(31)]).unwrap_or("?");
        let pad = [b' '; 48];
        let ind = (depth * 3).min(pad.len());
        let indent = core::str::from_utf8(&pad[..ind]).unwrap_or("");
        ctx.console_writeln_fmt(format_args!(
            "{}{}task {} \"{}\" {} ({})",
            indent, if depth > 0 { "`- " } else { "" }, cur, name, st.state_str(),
            trace_block_kind(st.state, awaits)));

        if awaits == 0 {
            let runnable = st.state == 0 || st.state == 1;
            ctx.console_writeln_fmt(format_args!(
                "{}   root: awaits no task - {}", indent,
                if runnable { "it is runnable, so the chain is not stuck here" }
                else { "blocked on its own endpoint, waiting for work" }));
            break;
        }
        ctx.console_writeln_fmt(format_args!("{}   awaiting endpoint {}", indent, awaits));

        let next = match trace_owner_of(ctx, awaits) {
            Some(nx) => nx,
            None => {
                ctx.console_writeln_fmt(format_args!(
                    "{}   root: endpoint {} has NO LIVE OWNER - the peer died; this task wakes with ReplyDead",
                    indent, awaits));
                break;
            }
        };
        if seen[..nseen].contains(&next) {
            ctx.console_writeln_fmt(format_args!(
                "{}   CYCLE: task {} is already in this chain - these tasks await each other (8.9)",
                indent, next));
            break;
        }
        if nseen < seen.len() { seen[nseen] = cur; nseen += 1; } else {
            ctx.console_writeln_fmt(format_args!("{}   (chain longer than {} - stopping)", indent, seen.len()));
            break;
        }
        cur = next;
        depth += 1;
    }
    Ok(())
}

fn cmd_status(ctx: &ServiceContext) -> Result<(), ShellError> {
    let t = build_status_table(ctx);
    { let mut o = Out::Console; t.to_grid(&mut OutSink { ctx, out: &mut o }); }
    if t.overflow() {
        ctx.console_writeln_fmt(format_args!("status: more than {} rows shown (bounded)", REC_MAX_ROWS));
    }
    Ok(())
}

/// `caps <service>` - list the capabilities a service holds. A thin broker over
/// the kernel's `task_caps` introspection (held via the INTROSPECT cap). Makes
/// authority visible on the box itself (§26.9): for each cap, the resource it
/// targets and the rights it carries.
fn cmd_caps(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    let slot = match slot_of(ctx, name) {
        Some(s) => s,
        None => {
            ctx.console_writeln("caps: no such live service");
            return Err(ShellError::FileNotFound);
        }
    };
    let mut caps = [CapInfo::default(); 64];
    let n = ctx.task_caps(slot, &mut caps);

    let mut hdr = [0u8; 48];
    let mut hp = 0usize;
    write_bytes(&mut hdr, &mut hp, b"caps for ");
    write_bytes(&mut hdr, &mut hp, name.as_bytes());
    write_bytes(&mut hdr, &mut hp, b":");
    ctx.console_writeln(core::str::from_utf8(&hdr[..hp]).unwrap_or("caps:"));

    if n == 0 {
        ctx.console_writeln("  (none)");
        return Ok(());
    }
    // Legend: left column is the resource the cap targets, right column the rights
    // it grants (§7.4). log_write/spawn/console_read/console_push/introspect are
    // kernel resources; endpoint#N is an IPC endpoint.
    ctx.console_writeln("  RESOURCE (target)  RIGHTS (read/write/send/recv/grant/revoke)");
    for cap in caps.iter().take(n) {
        let mut buf = [b' '; 64];
        let mut pos = 0usize;
        write_bytes(&mut buf, &mut pos, b"  ");
        // Resource name (stable kernel resources by id; others by number).
        match cap.resource_id {
            1 => write_bytes(&mut buf, &mut pos, b"log_write"),
            2 => write_bytes(&mut buf, &mut pos, b"spawn"),
            3 => write_bytes(&mut buf, &mut pos, b"console_read"),
            4 => write_bytes(&mut buf, &mut pos, b"console_push"),
            5 => write_bytes(&mut buf, &mut pos, b"introspect"),
            6 => write_bytes(&mut buf, &mut pos, b"service_control"),
            id => {
                write_bytes(&mut buf, &mut pos, b"endpoint#");
                write_u32(&mut buf, &mut pos, id as u32);
            }
        }
        while pos < 18 { buf[pos] = b' '; pos += 1; }
        // Rights spelled out (§7.4) so no decoding is needed.
        let r = cap.rights;
        if r & 0x01 != 0 { write_bytes(&mut buf, &mut pos, b"read "); }
        if r & 0x02 != 0 { write_bytes(&mut buf, &mut pos, b"write "); }
        if r & 0x04 != 0 { write_bytes(&mut buf, &mut pos, b"send "); }
        if r & 0x08 != 0 { write_bytes(&mut buf, &mut pos, b"recv "); }
        if r & 0x10 != 0 { write_bytes(&mut buf, &mut pos, b"grant "); }
        if r & 0x20 != 0 { write_bytes(&mut buf, &mut pos, b"revoke "); }
        ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or("?"));
    }
    Ok(())
}

/// Scheduler slot of a live service by name, scanned once (no wait). `None` if
/// not found.
fn slot_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 /* Dead */ && st.name_str() == name {
            return Some(slot);
        }
    }
    None
}

/// The restart count of the live service named `name` (None if not running). A respawn increments it
/// (a fresh instance reads previous + 1), so a value strictly greater than a pre-kill reading proves a
/// NEW instance came up - the recovery signal `chaos kill-storm` waits on.
fn gen_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 /* Dead */ && st.name_str() == name {
            return Some(st.restart_count as u32);
        }
    }
    None
}

/// `observe now` - broker a one-shot static metrics frame.
///
/// `observe` is a least-authority service: it holds only INTROSPECT + log caps,
/// never the shell's spawn/kill/restart. The shell spawns it; it prints one frame
/// via its own caps and parks. Kill any parked prior instance first (one-shot
/// observe has no graceful self-exit in v1), so at most one lingers.
fn cmd_observe_now(ctx: &ServiceContext) -> Result<(), ShellError> {
    let _ = ctx.kill("observe-now");
    if ctx.spawn("observe-now").is_err() {
        ctx.console_writeln("observe: failed to spawn observe-now");
        return Err(ShellError::Unknown);
    }
    // observe-now's frame is serial-bound (~100+ ms) and prints asynchronously, so
    // returning immediately would put the next prompt ABOVE the frame. Wait until
    // observe-now finishes and parks (BlockRecv) so the prompt lands below it.
    // Bounded against a child that never parks. (The console service will make
    // output ordering automatic; this is the interim fix.)
    if let Some(slot) = find_running_slot(ctx, "observe-now") {
        let mut parked = false;
        for _ in 0..1_000_000u32 {
            ctx.yield_cpu();
            let st = ctx.task_stat(slot);
            // state 2 = BlockedOnRecv → finished printing; invalid → gone.
            if !st.valid || st.state == 2 {
                parked = true;
                break;
            }
        }
        // REAP IT HERE, not on the next `observe now`. A parked one-shot is still a LIVE task: it
        // holds its slot and its frames, and anything scanning the task table sees a service. Chaos
        // does exactly that (deliberately - it keeps no hardcoded victim list, because one goes stale
        // the moment the running set changes), so a parked `observe-now` was being killed as though it
        // were a service, spending a victim slot that a real service should have had and reporting a
        // kill that measured nothing.
        //
        // The old comment called this "at most one lingers", which is true and was never the point:
        // one lingering task is enough to corrupt what the storm is measuring. The right owner of the
        // cleanup is the shell, which spawned it and already holds the kill authority - it was simply
        // doing it one invocation too late.
        //
        // Only when the park was actually OBSERVED. If the wait timed out, the child is still printing
        // and killing it would truncate the frame the user asked for - so leave it, and the defensive
        // kill at the top of this function reaps it next time. That is the case the old code was for.
        if parked {
            let _ = ctx.kill("observe-now");
        }
    }
    Ok(())
}

/// This shell's own core (slot scan by name; 0 if not found). Used to place the live `observe`
/// painter on a different core so its repaint can't starve the shell's `q`-poll.
fn observe_shell_core(ctx: &ServiceContext) -> u32 {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.name_str() == "shell" { return st.core as u32; }
    }
    0
}

/// `observe` (live) - broker the full-screen foreground view (Stage 2c).
///
/// The shell is the capability-broker (Appendix B.3): it lends the keyboard to
/// the foreground child by owning `q` ourselves. We spawn `observe-live` (which paints the
/// screen - hides the cursor, suppresses echo, repaints - but does NOT read input), then poll
/// the console for `q` and kill it when pressed. The shell, not the child, reads the keyboard
/// here (one reader, no race), and both we and the child SLEEP between polls so core 0 halts
/// while `observe` is up - otherwise a busy wait would peg the core and make every task on it
/// read as ~100% in observe's own display. Then we restore the screen and our read loop resumes.
/// Per-poll sleep for the live view's `q` loop, in TSC cycles (~30 ms at 2 GHz; QEMU's 1-tick
/// fallback makes it ~one quantum). The same idea as the painter's POLL_SLEEP_CYCLES: sleep, do
/// not spin, so the observer never becomes the load it is displaying.
const OBSERVE_QPOLL_MS: u64 = 30;

fn cmd_observe_live(ctx: &ServiceContext) -> Result<(), ShellError> {
    let _ = ctx.kill("observe-live"); // clear any stale instance
    // Pin the painter to a DIFFERENT core than this shell. Its framebuffer-heavy repaint must not
    // share a core with this q-poll loop, or it starves `q` (the "stuck" that showed up once the
    // legend made the repaint heavier - and why it was flaky before: round-robin sometimes
    // co-located them). Fall back to round-robin only if the targeted spawn fails.
    let shell_core = observe_shell_core(ctx);
    let ncores = ctx.inspect_core_count();
    let spawned = if ncores >= 2 {
        let last = ncores - 1;
        let target = if last == shell_core { 0 } else { last };
        ctx.spawn_on("observe-live", target).is_ok()
    } else {
        false
    };
    if !spawned && ctx.spawn("observe-live").is_err() {
        ctx.console_writeln("observe: failed to spawn observe-live");
        return Err(ShellError::Unknown);
    }
    if let Some(slot) = find_running_slot(ctx, "observe-live") {
        // Own `q` while the child paints. The bound is a paranoid safety net so a hung child can
        // never wedge the shell forever; normally we break on `q` (or if the child dies).
        for _ in 0..u32::MAX {
            // Poll `q` on a short SLEEP, so this core IDLES between polls. This used to be a
            // yield_cpu busy-loop - a workaround for the AMD T630's garbage CPUID TSC calibration,
            // under which ctx.sleep stretched to seconds and `q` looked dead - but that root cause
            // is fixed (the kernel PIT-calibrates tsc_ticks_per_quantum on TSC-Deadline hardware;
            // QEMU uses the 1-tick fallback). The busy-yield PEGGED this core, so observe reported
            // its own observer: the shell's core sat at ~99-100% for as long as you watched - the
            // very artifact the painter's own sleep exists to avoid. ~30 ms per poll keeps `q`
            // latency imperceptible while the core halts between polls.
            ctx.sleep_ms(OBSERVE_QPOLL_MS);
            let mut quit = false;
            while let Some(b) = ctx.try_console_read() {
                if b == b'q' || b == b'Q' { quit = true; }
            }
            if quit { break; }
            // Break if the painter died OR its slot was reused by another spawn (name mismatch),
            // not just on `!valid` - else a reused slot would freeze the frame (audit U8).
            let st = ctx.task_stat(slot);
            if !st.valid || st.state == 4 || st.name_str() != "observe-live" { break; }
        }
    }
    let _ = ctx.kill("observe-live"); // reap the live painter (it never exits on its own)
    // The painter is usually killed MID-repaint (each frame is ~100 ms of serial paint), leaving a
    // PARTIAL frame and the cursor mid-screen - that was the smear, and why it regressed: on a busier
    // core the paint takes longer, so a q lands mid-frame more often. `observe now` paints from the
    // CURSOR (not home) and does not clear, so it must be aimed: HOME first, repaint one complete static
    // frame OVER the partial one, then erase any rows left below. The cursor ends on a fresh line below
    // the whole frame, so the prompt lands cleanly under the snapshot - every time, the way you liked it.
    // Echo stays OFF - the shell, not the kernel, owns echo.
    ctx.console_echo(false);
    ctx.console_write("\x1b[H");
    // `observe now` paints only the body; reprint the live view's title bar above it so the exit
    // snapshot is the WHOLE frame - top not cut off, a faithful freeze of what you were watching. These
    // two strings are byte-for-byte the painter's (services/observe title bar); \x1b[K clears whatever
    // the partial frame left on these two rows.
    ctx.console_write("observe - live                                      (q to quit)\x1b[K\r\n");
    ctx.console_write("================================================================\x1b[K\r\n");
    let r = cmd_observe_now(ctx);
    ctx.console_write("\x1b[J\x1b[?25h");
    r
}

/// Slot of a just-spawned, still-live service by name (not a killed/dead one),
/// waiting briefly for it to appear. `None` if it never shows up.
fn find_running_slot(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for _ in 0..2000u32 {
        ctx.yield_cpu();
        for slot in 0..256u32 {
            let st = ctx.task_stat(slot);
            if st.valid && st.state != 4 /* Dead */ && st.name_str() == name {
                return Some(slot);
            }
        }
    }
    None
}

/// Services the shell refuses to *casually* `kill`/`restart` at the command layer (§6.1), explaining
/// why before the syscall is even tried. Just `supervisor`: it IS restartable (Phase 6 - the kernel
/// respawns it on death), but a casual `kill supervisor` is refused so it is not fumbled away by
/// accident; deliberate supervisor chaos goes through `chaos kill-storm supervisor`. Ordinary
/// restartable services (block-driver, fs, ...) are freely killable - the supervisor respawns them.
const CORE_SERVICES: [&str; 1] = ["supervisor"];

/// Shown when `spawn`/`restart` targets the supervisor - "Not applicable" makes it clear the command is
/// refused *because* of what the target is, not because it failed. (`kill supervisor` is ALLOWED now -
/// the kernel respawns it; only spawning a duplicate or restarting the restart authority are refused.)
const PROTECTED_MSG: &str =
    "Not applicable. The supervisor is the restart authority - it cannot be spawned or restarted directly. To recycle it: 'kill supervisor' (the kernel respawns it) or 'chaos kill-storm supervisor'";

/// Shown when spawn/kill/restart targets an observe variant - they are brokered by
/// the `observe` / `observe now` commands, not raw service operations.
const OBSERVE_HINT: &str =
    "observe runs from a command: type 'observe' (live) or 'observe now' (snapshot)";

fn is_core_service(name: &str) -> bool {
    CORE_SERVICES.contains(&name)
}

/// `observe`'s variants are brokered by the `observe` / `observe now` commands -
/// not meant to be raw-spawned (the bare `observe` service is a serial-streaming
/// dev build that scrolls forever and ignores `q`).
fn is_observe_variant(name: &str) -> bool {
    matches!(name, "observe" | "observe-now" | "observe-live")
}

/// The one thing the shell refuses to operate on from within itself: the shell task
/// issuing the command. `xhci`/`ehci` USED to be guarded here too, but they respawn and
/// re-enumerate their devices on death - proven across millions of `chaos max-carnage`
/// rounds with the session intact - so killing a USB host driver only blips input for
/// ~a second, it does not brick the session. They are killable now (the operator's call).
/// Returns the reason to show, or `None` if `name` is safe to operate on.
fn session_critical_msg(name: &str) -> Option<&'static str> {
    match name {
        "shell" => Some("Not applicable. that is this shell - the session you are typing in"),
        _       => None,
    }
}

/// Print `prefix` followed by `name` as one console line.
fn report(ctx: &ServiceContext, prefix: &str, name: &str) {
    let mut buf = [0u8; 96];
    let mut pos = 0usize;
    write_bytes(&mut buf, &mut pos, prefix.as_bytes());
    write_bytes(&mut buf, &mut pos, name.as_bytes());
    ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or(prefix));
}

fn cmd_spawn(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    // `spawn a,b,c` is a comma list; a single name behaves EXACTLY as before. (No `all-services` for
    // spawn - system services are supervisor-owned; this is for demo/app services like `ping,pong`.)
    if name.contains(',') {
        let mut n = 0usize;
        for s in name.split(',') {
            if s.is_empty() || n >= 16 { continue; }
            n += 1;
            let _ = spawn_one(ctx, s);   // report each; a failure/duplicate does NOT abort the rest
        }
        return Ok(());
    }
    spawn_one(ctx, name)
}

/// Start ONE service, with the observe / core-service / already-running guards. Used directly for a bare
/// `spawn <svc>` and per-segment by the comma-list path in cmd_spawn.
fn spawn_one(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return Err(ShellError::Unknown);
    }
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return Err(ShellError::Denied);
    }
    if slot_of(ctx, name).is_some() {
        report(ctx, "already running: ", name);
        return Err(ShellError::Unknown);
    }
    match ctx.spawn(name) {
        Ok(())  => { report(ctx, "spawned: ", name); Ok(()) }
        Err(_)  => { report(ctx, "spawn failed (unknown service?): ", name); Err(ShellError::Unknown) }
    }
}

/// `spawncap <name>` - **Phase-0 diagnostic** (`docs/naming-design.md`). Spawns a service via the
/// new `SpawnReturningEndpoint` syscall, which hands the caller a `SEND|GRANT` cap to the new
/// service's endpoint, then proves that cap routes by sending a probe message through it. This is
/// the seam that will let the supervisor build a userspace `name → cap` map; it does NOT change how
/// services are wired today (purely additive). Folded into the supervisor / removed in a later phase.
fn cmd_spawncap(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return Err(ShellError::Denied);
    }
    match ctx.spawn_returning_endpoint(name, 0xFFFF) {
        Some(h) => {
            let r = ctx.try_send_by_handle(h, &Message::from_bytes(&[0x01]));
            ctx.remove_cap(h);   // reclaim the probe endpoint cap (no leak)
            match r {
                Ok(())  => { ctx.console_writeln_fmt(format_args!("spawncap: {} - endpoint cap acquired; send Ok", name)); Ok(()) }
                Err(_)  => { ctx.console_writeln_fmt(format_args!("spawncap: {} - cap acquired but send failed", name)); Err(ShellError::Unknown) }
            }
        },
        None => {
            ctx.console_writeln_fmt(format_args!(
                "spawncap: could not acquire endpoint cap for {} (cap not held / spawn failed / no endpoint)", name));
            Err(ShellError::Unknown)
        }
    }
}

/// `spawnwired` - **Phase-0b diagnostic** (`docs/naming-design.md`). Spawns `pong` and acquires its
/// endpoint cap (Phase 0a), then spawns `greet` wiring it to pong **via that passed cap** as
/// `send_peer[0]` - NOT by name. `greet` sends its lines to `send_peer[0]`, so `pong` logs
/// "pong: received …". This proves the kernel installs a caller-supplied cap into the child and the
/// child uses it - the seam by which the supervisor (not the kernel) owns naming. Removed / folded
/// into the supervisor in a later phase.
fn cmd_spawnwired(ctx: &ServiceContext) -> Result<(), ShellError> {
    let pong = match ctx.spawn_returning_endpoint("pong", 0xFFFF) {
        Some(h) => h,
        None => { ctx.console_writeln("spawnwired: could not spawn pong / acquire its endpoint cap"); return Err(ShellError::Unknown); }
    };
    match ctx.spawn_with_caps("greet", 0xFFFF, &[("pong", pong)]) {
        Ok(_)  => { ctx.console_writeln("spawnwired: greet wired to pong via a passed cap (watch for pong: received)"); Ok(()) }
        Err(_) => { ctx.console_writeln("spawnwired: spawn_with_caps(greet) failed"); Err(ShellError::Unknown) }
    }
}

/// Maximum stages in one pipeline (§26.6 bounded).
const MAX_STAGES: usize = 8;

/// Run a SERVICE stage. `input == None` → a producer (`greet`): spawn it wired to the shell
/// and drain its output. `input == Some(bytes)` → a filter/sink (`upper`): also send it the
/// input first (the whole buffer as one ≤4 KiB message + an EOT). Output is drained from the
/// shell's endpoint until the service sends an EOT (0x04), then the service is reaped. Whole-
/// buffer messaging (≤ one message each way) keeps the bounded queues deadlock-free (§8.9).
fn drain_service(ctx: &ServiceContext, svc: &str, input: Option<&[u8]>, out: &mut Cap) -> bool {
    // A stage crossing a service boundary is one IPC message until streaming chunks across
    // many. Refuse a larger buffer LOUDLY rather than silently clipping it to 4 KiB (§3.12).
    if let Some(inp) = input {
        if inp.len() > PIPE_MSG_MAX {
            ctx.console_writeln_fmt(format_args!(
                "pipe: stage too large ({} bytes) for the '{}' filter - max {} KiB until pipe streaming",
                inp.len(), svc, PIPE_MSG_MAX / 1024));
            return false;
        }
    }
    // Wire the service to send its output to the SHELL's own endpoint.
    if ctx.spawn_pipe(svc, "shell").is_err() {
        ctx.console_writeln_fmt(format_args!("pipe: failed to spawn '{}'", svc));
        return false;
    }
    if let Some(inp) = input {
        // Filter/sink: resolve the service's input endpoint (it must register) and feed it.
        match lookup_sink(ctx, svc) {
            Some(h) => {
                // Report a failed feed loudly rather than silently draining nothing (§26.7): if the
                // filter died after registering, the user must see it, not get a silent empty result.
                let fed = ctx.send_by_handle(h, &Message::from_bytes(inp)).is_ok()
                    && ctx.send_by_handle(h, &Message::from_bytes(&[PIPE_EOT])).is_ok();
                // The sink cap is done after the feed (the drain reads on our OWN endpoint), so reclaim
                // it - else every pipe leaks a cap slot and a pipe-heavy run (selfcheck) fills the
                // 64-slot cap table, making live services look unreachable ("storage unavailable").
                ctx.remove_cap(h);
                if !fed {
                    ctx.console_writeln_fmt(format_args!(
                        "pipe: failed to send input to '{}' (it died after registering?)", svc));
                    let _ = ctx.kill(svc);
                    return false;
                }
            }
            None => {
                // Distinct, honest wording: a registration TIMEOUT (filter never became ready) is
                // not "not a filter". The new phrasing also tells stale-image runs apart - if this
                // text ever changes on hardware, the new shell is running (§26.7 loud failure).
                ctx.console_writeln_fmt(format_args!(
                    "pipe: '{}' never registered an input endpoint (waited ~{}s) - not a filter, or it failed to start",
                    svc, FILTER_WAIT_SECS));
                let _ = ctx.kill(svc);
                return false;
            }
        }
    }
    // Drain the service's output until EOT, FAILURE-AWARE (Commandment VIII): the `512` bounds the
    // message COUNT, but each wait is a per-message deadline + q-abort, not a bare blocking `recv` -
    // a filter that registers its endpoint then wedges or page-faults BEFORE sending EOT (or any
    // output) must not hang the prompt forever with the keyboard dead. Timeout => it died mid-stream;
    // Aborted => the user pressed q. Both stop the drain loudly (§26.7) rather than blocking.
    for _ in 0..512 {
        match ctx.recv_abortable_deadline(FILTER_WAIT_SECS) {
            ReqOutcome::Reply(msg) => {
                let p = msg.payload_bytes();
                if p == [PIPE_EOT] { break; }
                out.push(p);
                if out.overflow { break; }
            }
            ReqOutcome::Aborted => { ctx.console_writeln("pipe: aborted"); break; }
            ReqOutcome::Timeout => {
                ctx.console_writeln_fmt(format_args!(
                    "pipe: '{}' stopped sending without EOT (waited ~{}s) - it may have failed mid-stream",
                    svc, FILTER_WAIT_SECS));
                break;
            }
        }
    }
    let _ = ctx.kill(svc);
    if out.overflow { ctx.console_writeln("pipe: pipe output exceeded the buffer (truncated)"); }
    true
}

/// Split `s` into (first word, rest-trimmed).
fn split_first(s: &str) -> (&str, &str) {
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim_start()),
        None => (s, ""),
    }
}

/// Built-ins that emit text and can be the producer side of a pipe.
// `ls` and `find` are intentionally absent: they are record producers (`is_record_producer`),
// handled on the record path in `pipe_run` before this is consulted, so listing them here would
// be dead. `tree` stays text - a hierarchy is not a flat table.
fn is_producer_builtin(name: &str) -> bool {
    // Text emitters that can start a pipe (captured via `Out`). The info commands (about/mem/cores/
    // date/help) join read/echo/tree so "anything that displays text can be saved to a file".
    // No `cat`: `read` is the one file reader (utilities/18_read.md - `read` replaces POSIX `cat`,
    // whose name describes a different operation; this OS does not carry POSIX vocabulary).
    //
    // NOT `selfcheck`/`run`: an orchestrator runs the suite's OWN sub-pipelines, so capturing it
    // nests a pipe_run (64 KiB Stream) inside a pipe_run - two coexisting 64 KiB buffers overflow
    // the tight user stack (HW-proven shell crash, [[project-shell-stack-pipe]]). They refuse
    // loudly as non-producers instead. To capture a big file for `edit`, append a simple producer
    // a few times: `help | write /big.txt; help | write append /big.txt; …`.
    matches!(name, "read" | "echo" | "tree" | "input"
                 | "about" | "version" | "whatis" | "mem" | "cores" | "date" | "net" | "ping" | "sock" | "help")
}

/// Producer SERVICES that emit without needing input, so they can start a pipe (and follow the
/// EOT end-of-stream protocol). A non-producer service in stage 1 would block the shell on
/// `recv` (there is no non-blocking recv in v1), so the set is an explicit whitelist.
fn is_pipe_producer_service(name: &str) -> bool {
    matches!(name, "greet")
}

/// Producer SERVICES that emit **records** (the binary wire codec, `Table::encode`) rather than
/// text. Stage 1 drains the service's bytes and `Table::decode`s them straight into a Table -
/// no `from json` round-trip. Checked before the text producer-service whitelist.
fn is_record_producer_service(name: &str) -> bool {
    matches!(name, "roster")
}

/// Run a producer built-in (`cmd args`) with its output going to `out`.
fn run_producer(ctx: &ShellCtx, cwd: &Cwd, cmdline: &str, out: &mut Out) {
    let (cmd, arg) = split_first(cmdline);
    match cmd {
        "echo"         => { let _ = cmd_echo(ctx, arg, out); }
        "read"         => { let _ = cmd_read(ctx, cwd, arg, out); }
        // "ls" and "find" are record producers (handled on the record path), not text here.
        "tree"         => { let _ = cmd_tree(ctx, cwd, arg, out); }
        // Info/display commands - text emitters, capturable to a file.
        "about"        => { let _ = cmd_about(ctx, out); }
        "version"      => { let _ = cmd_version_os(ctx, out); }
        "whatis"       => { let _ = cmd_whatis(ctx, arg, out); }
        "mem"          => { let _ = cmd_mem(ctx, out); }
        "cores"        => { let _ = cmd_cores(ctx, "", out); }
        "date"         => { let _ = cmd_date(ctx, arg, out); }
        "net"          => { let _ = cmd_net(ctx, arg, out); }
        "ping"         => { let _ = cmd_ping(ctx, arg, out); }
        "sock"         => { let _ = cmd_sock(ctx, out); }
        "help"         => help_to_out(ctx, out),
        "input"        => run_input(ctx, arg, out),
        _ => {}
    }
}

/// Run `inner` (a command or pipeline) with its output written to `out` - the machinery behind
/// `$( )` value capture (docs/scripting.md §3). A pipeline routes through `pipe_run` (whose final
/// stream renders to `out`); a bare producer builtin captures directly. A bare producer SERVICE
/// drains through a local `Cap` (no coexisting pipe buffer, so it fits). A non-producer bare command
/// is refused loudly. `out` is a small (16 KiB `ReportBuf`-backed) sink so it does NOT stack up
/// against `pipe_run`'s own 64 KiB buffers on the pipeline path - the nested-capture overflow trap
/// ([[project-shell-stack-pipe]]). Returns true on success.
fn run_captured(ctx: &ShellCtx, cwd: &Cwd, inner: &str, out: &mut Out) -> bool {
    let inner = inner.trim();
    if inner.is_empty() { ctx.console_writeln("gsh: $( ) needs a command"); return false; }
    // A PIPELINE capture would stack its 128 KiB of pipe buffers on top of the interpreter's live
    // frame and overflow the bounded 256 KiB user stack (the nested-capture trap,
    // [[project-shell-stack-pipe]]). Refuse it loudly and point at the file-staging idiom: run the
    // pipeline to a file, then capture the file with `$(read …)` (materialize, then capture).
    if inner.contains('|') {
        ctx.console_writeln("gsh: $( ) cannot capture a pipeline (bounded stack). Stage it: 'greet | count | write /t.txt' then 'let n = $(read /t.txt)'");
        return false;
    }
    let (c0, _) = split_first(inner);
    if is_producer_builtin(c0) {
        run_producer(ctx, cwd, inner, out);
        return true;
    }
    if is_pipe_producer_service(c0) {
        // A bare producer service has no coexisting pipe_run Stream, so a 64 KiB drain Cap fits.
        let mut cap = Cap::new();
        if !drain_service(ctx, c0, None, &mut cap) { return false; }
        out.put_bytes(ctx, cap.bytes());
        return true;
    }
    ctx.console_writeln_fmt(format_args!(
        "gsh: cannot capture '{}' with $( ) - pipe it (e.g. '{} | count') or use a producer", c0, c0));
    false
}

/// If `v` is exactly a single `$( ... )` capture spanning the whole value, return the inner command.
fn capture_form(v: &str) -> Option<&str> {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() < 3 || b[0] != b'$' || b[1] != b'(' { return None; }
    let mut depth = 0usize;
    let mut i = 1usize;
    while i < b.len() {
        match b[i] { b'(' => depth += 1, b')' => { depth -= 1; if depth == 0 { break; } }, _ => {} }
        i += 1;
    }
    // the matching ')' must be the last char - otherwise it is not a whole-value capture.
    if depth == 0 && i == b.len() - 1 { Some(&v[2..i]) } else { None }
}

/// `let [mut] name = $( cmd )` - define a binding from captured command output (trailing whitespace
/// trimmed). `#[inline(never)]`: the 16 KiB capture buffer lives ONLY here, off the common let path.
/// A ReportBuf (16 KiB), not a Cap (64 KiB), so on the `$(pipe)` path it does not overflow the stack
/// against pipe_run's own 64 KiB buffers. A value larger than the var arena is refused by `define`.
#[inline(never)]
fn capture_define(ctx: &ShellCtx, cwd: &Cwd, name: &str, inner: &str, mutable: bool, vars: &mut Vars) -> Result<(), ShellError> {
    let mut rb = ReportBuf::new();
    let ok = { let mut o = Out::File(&mut rb); run_captured(ctx, cwd, inner, &mut o) };
    if !ok { return Err(ShellError::Unknown); }
    match vars.define(name.as_bytes(), trim_bytes(rb.bytes()), mutable) {
        Ok(()) => { if capture_is_secret(inner) { vars.mark_secret_name(name.as_bytes()); } Ok(()) }
        Err(e) => { var_err_msg(ctx, name, e); Err(ShellError::Unknown) }
    }
}

/// `name = $( cmd )` - reassign a mutable binding from captured command output.
#[inline(never)]
fn capture_reassign(ctx: &ShellCtx, cwd: &Cwd, name: &str, inner: &str, vars: &mut Vars) -> Result<(), ShellError> {
    let mut rb = ReportBuf::new();
    let ok = { let mut o = Out::File(&mut rb); run_captured(ctx, cwd, inner, &mut o) };
    if !ok { return Err(ShellError::Unknown); }
    match vars.reassign(name.as_bytes(), trim_bytes(rb.bytes())) {
        Ok(()) => { if capture_is_secret(inner) { vars.mark_secret_name(name.as_bytes()); } Ok(()) }
        Err(e) => { var_err_msg(ctx, name, e); Err(ShellError::Unknown) }
    }
}

/// Which way a `write` puts its data: replace the file, add to the end, or add to the front.
/// Plain `write` / `… | write` is `Overwrite`; `append`/`prepend` are the explicit additive keywords.
#[derive(Clone, Copy, PartialEq)]
enum WriteMode { Overwrite, Append, Prepend }

/// Parse a leading `append` / `prepend` keyword (each only when followed by whitespace or end, so a
/// path like `appendix.txt` stays a path) from a write arg. Returns the mode + the remaining arg.
fn parse_write_mode(arg: &str) -> (WriteMode, &str) {
    if let Some(r) = arg.strip_prefix("append") {
        if r.is_empty() || r.starts_with(char::is_whitespace) { return (WriteMode::Append, r.trim_start()); }
    }
    if let Some(r) = arg.strip_prefix("prepend") {
        if r.is_empty() || r.starts_with(char::is_whitespace) { return (WriteMode::Prepend, r.trim_start()); }
    }
    (WriteMode::Overwrite, arg)
}

const WRITE_TMP: &[u8] = b"/.write.tmp"; // append/prepend staging file (root → no dirname math)

/// Read exactly `out.len()` bytes from `path` at byte `off`, looping `read_at`. False on short read.
fn read_file_exact(ctx: &ShellCtx, path: &[u8], off: usize, out: &mut [u8]) -> bool {
    let mut done = 0usize;
    let mut tmp = [0u8; IO_CHUNK];
    while done < out.len() {
        match fs_read_at(ctx, path, (off + done) as u64, &mut tmp) {
            Some(n) if n > 0 => {
                let take = n.min(out.len() - done);
                out[done..done + take].copy_from_slice(&tmp[..take]);
                done += take;
            }
            _ => return false,
        }
    }
    true
}

/// Deadline-bounded twin of `read_file_exact` for the startup history load: each chunk read is capped at
/// `max_secs` (RTC) via `fs_read_at_bounded`, so a wedged/slow fs times out per chunk instead of blocking
/// the shell before its input loop. False on short read, timeout, or any miss - the caller treats that as
/// "no history" (§26.7). Only the startup load uses this; ordinary file commands keep the unbounded path.
fn read_file_exact_bounded(ctx: &ShellCtx, path: &[u8], off: usize, out: &mut [u8], max_secs: i64) -> bool {
    let mut done = 0usize;
    let mut tmp = [0u8; IO_CHUNK];
    while done < out.len() {
        match fs_read_at_bounded(ctx, path, (off + done) as u64, &mut tmp, max_secs) {
            Some(n) if n > 0 => {
                let take = n.min(out.len() - done);
                out[done..done + take].copy_from_slice(&tmp[..take]);
                done += take;
            }
            _ => return false,
        }
    }
    true
}

/// Overwrite `p` (resolved path) with `data`. Small payload → one WriteFile; larger → write_new +
/// streamed write_at chunks (so a piped payload up to the capture buffer reaches the file).
fn stream_overwrite(ctx: &ShellCtx, p: &[u8], data: &[u8]) {
    if data.len() <= IO_CHUNK {
        match fs_request(ctx, OP_WRITE_FILE, p, data) {
            Some(r) if r.payload_bytes().first() == Some(&FS_OK) =>
                ctx.console_writeln_fmt(format_args!("piped {} bytes → {}", data.len(), str_of(p))),
            Some(r) if no_fs(ctx, r.payload_bytes()) => {}
            Some(_) => ctx.console_writeln("pipe: write failed (bad path, or parent missing?)"),
            None    => ctx.console_writeln("pipe: storage unavailable"),
        }
        return;
    }
    if !fs_write_new(ctx, p, data.len() as u64) {
        ctx.console_writeln("pipe: write failed (bad path, or parent missing?)");
        return;
    }
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + IO_CHUNK).min(data.len());
        if !fs_write_at(ctx, p, off as u64, &data[off..end]) {
            ctx.console_writeln("pipe: write failed mid-stream");
            return;
        }
        off = end;
    }
    ctx.console_writeln_fmt(format_args!("piped {} bytes → {}", data.len(), str_of(p)));
}

/// Append or prepend `new` to file `p`, streaming through a temp file: the original is read (via
/// `read_at`) while the combined content `[old|new]` (append) or `[new|old]` (prepend) is written
/// to `WRITE_TMP`, which then atomically replaces the target. Constant memory (one IO_CHUNK
/// scratch), any file size. `prepend` is a **full-file rewrite** - there is no insert-at-front in
/// the filesystem - so it costs the same as rewriting the file (honest, §26.7). True on success.
#[inline(never)]
fn fs_stream_combine(ctx: &ShellCtx, p: &[u8], new: &[u8], prepend: bool) -> bool {
    // A FAILED STAT IS NOT AN EMPTY FILE. `fs_stat` returns `None` for BOTH "absent" and "fs error",
    // so `.unwrap_or(0)` made an errored stat on an existing file read as size 0: the combine loop
    // below then copied only the NEW bytes, never entered the old-content branch, and the caller
    // deleted the original and moved the temp in. An `append` that silently became an overwrite, and
    // returned true.
    //
    // Absent is legitimate (appending to a file that does not exist creates it). Unreachable is not,
    // so the two are now told apart by asking once more: a reply that arrives says the file is
    // genuinely absent, no reply at all says `fs` could not answer.
    let old_size = match fs_stat(ctx, p) {
        Some((sz, _)) => sz as usize,
        None if fs_request(ctx, OP_STAT_FILE, p, &[]).is_some() => 0,
        None => {
            ctx.console_writeln("write: cannot stat the target - ABORTING, nothing was changed");
            return false;
        }
    };
    let total = old_size + new.len();
    if total == 0 {
        return matches!(fs_request(ctx, OP_WRITE_FILE, p, &[]).as_ref()
            .map(|r| r.payload_bytes().first().copied()), Some(Some(FS_OK)));
    }
    if !fs_write_new(ctx, WRITE_TMP, total as u64) { return false; }
    // Ordered segments: prepend = [new (mem) | old (disk)]; append = [old (disk) | new (mem)].
    let (first_len, first_is_new) = if prepend { (new.len(), true) } else { (old_size, false) };
    let mut off = 0usize;
    let mut chunk = [0u8; IO_CHUNK];
    while off < total {
        let n = (total - off).min(IO_CHUNK);
        let mut i = 0usize;
        while i < n {
            let g = off + i;
            let (seg_is_new, local, remaining) = if g < first_len {
                (first_is_new, g, first_len - g)
            } else {
                let s = g - first_len;
                let second_is_new = !first_is_new;
                let second_len = if second_is_new { new.len() } else { old_size };
                (second_is_new, s, second_len - s)
            };
            let take = remaining.min(n - i);
            if seg_is_new {
                chunk[i..i + take].copy_from_slice(&new[local..local + take]);
            } else if !read_file_exact(ctx, p, local, &mut chunk[i..i + take]) {
                return false;
            }
            i += take;
        }
        if !fs_write_at(ctx, WRITE_TMP, off as u64, &chunk[..n]) { return false; }
        off += n;
    }
    let _ = fs_request(ctx, OP_DELETE, p, &[]);
    matches!(fs_request(ctx, OP_MOVE, WRITE_TMP, p).as_ref()
        .map(|r| r.payload_bytes().first().copied()), Some(Some(FS_OK)))
}

/// The `write` pipe sink: `… | write [append|prepend] <path>`. Parses the mode (plain overwrites),
/// resolves the path, and writes the captured/rendered `data`.
fn pipe_write(ctx: &ShellCtx, cwd: &Cwd, arg: &str, data: &[u8]) {
    let (mode, parg) = parse_write_mode(arg);
    let (pstr, _) = split_first(parg);
    if pstr.is_empty() { ctx.console_writeln("pipe: write needs a file path"); return; }
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pstr, &mut buf) { Some(p) => p, None => return };
    let mut pbuf = [0u8; PATH_MAX];
    let pl = path.len();
    pbuf[..pl].copy_from_slice(path);
    let p = &pbuf[..pl];
    match mode {
        WriteMode::Overwrite => stream_overwrite(ctx, p, data),
        WriteMode::Append | WriteMode::Prepend => {
            let prepend = mode == WriteMode::Prepend;
            if fs_stream_combine(ctx, p, data, prepend) {
                ctx.console_writeln_fmt(format_args!(
                    "{} {} bytes → {}", if prepend { "prepended" } else { "appended" }, data.len(), str_of(p)));
            } else {
                ctx.console_writeln_fmt(format_args!(
                    "pipe: write {} failed (storage, or bad path?)", if prepend { "prepend" } else { "append" }));
            }
        }
    }
}

/// Look up a just-spawned service's endpoint via the kernel name directory, retrying while it registers.
fn lookup_sink(ctx: &ServiceContext, sink: &str) -> Option<CapHandle> {
    // A freshly-spawned filter registers its input endpoint only once it actually RUNS - which on
    // real multi-core hardware is up to ~1 s after spawn (it's on another core and hasn't been
    // scheduled yet). Retry until it appears, bounded by REAL wall-clock time (the RTC).
    //
    // Use the RTC, NOT `inspect_core_total_ticks`. CORE_TOTAL_TICKS is a scheduler-quanta counter,
    // not a clock: after a storm (chaos max-carnage) it advanced ~100x slower than wall-time, so a
    // "~5 s" tick budget actually ran for ~8 minutes (the T630 selfcheck stall). The RTC is a true
    // clock and immune to scheduler weirdness, so we also `yield_cpu` cooperatively while waiting.
    // Path C (Phase 4): the sink resolves via the kernel name-directory (SEND|GRANT, so the cap can
    // be delegated to the producer); it is populated synchronously at the sink's spawn, so this
    // normally succeeds on the first iteration - the bounded wait is just a guard.
    let t0 = ctx.epoch_secs_monotonic();
    loop {
        if let Some(h) = ctx.acquire_send_grant_cap(sink) { return Some(h); }
        if ctx.epoch_secs_monotonic() - t0 >= FILTER_WAIT_SECS { return None; }
        ctx.yield_cpu();
    }
}

/// How long `lookup_sink` waits (in 10 ms timer ticks, §9.1) for a freshly-spawned filter to
/// register its input endpoint. ~5 s - comfortably over the observed worst-case first-run latency
/// (~1 s) on the T630 under selfcheck load, with margin.
const FILTER_WAIT_SECS: i64 = 5;

/// Does this line READ a secret (`input secret ...`, incl. `$(input secret ...)`)? Such a line is never
/// saved to the recall ring or /.gsh_history (§8 secret taint) - a password recovered on up-arrow would
/// defeat invisible entry. Erring toward not-saving is safe: a false positive drops one recall entry.
fn line_reads_secret(line: &[u8]) -> bool {
    line.windows(12).any(|w| w == b"input secret")
}

fn cmd_kill(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    // `kill all-services` (nuke every service but this shell) or `kill a,b,c` (a comma list) are
    // multi-target; a single name behaves EXACTLY as before. Per-service guards live in kill_one.
    if name == "all-services" || name.contains(',') {
        return kill_list(ctx, name);
    }
    kill_one(ctx, name)
}

/// Kill a bounded set: the `all-services` keyword (-> CHAOS_RESTARTABLE + the shell: a complete system
/// reset where everything dies and recovers, only the kernel never dies) or a comma-separated list. Each
/// segment runs the full kill_one guard path; a denied / dead / absent one does NOT abort the rest. ORDER:
/// everything except supervisor and shell first, then supervisor (so the kernel-respawned supervisor
/// reconciles AFTER the nuke and respawns the fresh prompt, not mid-storm), then the shell LAST via its
/// self-kill (it never returns), so every per-service report has printed before the session re-inits.
fn kill_list(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    let mut segs: [&str; 16] = [""; 16];
    let mut n = 0usize;
    if name == "all-services" {
        ctx.console_writeln("kill all-services: nuking every service - including this shell (a fresh prompt returns)");
        for &s in CHAOS_RESTARTABLE.iter() { if n < segs.len() { segs[n] = s; n += 1; } }
        if n < segs.len() { segs[n] = "shell"; n += 1; }   // shell dies LAST (the third pass below)
    } else {
        for s in name.split(',') { if !s.is_empty() && n < segs.len() { segs[n] = s; n += 1; } }
    }
    for &s in segs[..n].iter() { if s != "supervisor" && s != "shell" { let _ = kill_one(ctx, s); } }
    for &s in segs[..n].iter() { if s == "supervisor" { let _ = kill_one(ctx, s); } }
    for &s in segs[..n].iter() { if s == "shell" { let _ = kill_one(ctx, s); } } // never returns if present
    Ok(())
}

/// Kill ONE service, with all the session / authority guards. Used directly for a bare `kill <svc>` and
/// per-segment by kill_list.
fn kill_one(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    // The supervisor is killable from the shell now (the operator's call): the KERNEL respawns it
    // (Phase 6) and it reconciles - adopts the running services by name, respawns any that died. Only
    // `spawn`/`restart` of the supervisor stay refused (a duplicate or a self-restart of the restart
    // authority is nonsensical); a bare `kill` is the clean recycle path.
    if is_core_service(name) && name != "supervisor" {
        ctx.console_writeln(PROTECTED_MSG);
        return Err(ShellError::Denied);
    }
    if name == "shell" {
        // The shell is restartable now ("nothing escapes"): self-kill, and the supervisor respawns a
        // fresh prompt. The kernel's self-kill path defers our stack/PML4 reclaim (it is exactly how
        // every page fault already kills the running task), and our death notifies the supervisor,
        // which respawns us. The in-flight session is lost - a re-init, not a resume (§14.2/§25). We
        // yield forever after the kill so we never execute again as the dead instance.
        ctx.console_writeln("kill shell: restarting this session - a fresh prompt is coming (in-flight state is lost)…");
        match ctx.kill("shell") {
            Ok(())  => loop { ctx.yield_cpu(); },
            Err(_)  => { ctx.console_writeln("kill shell: failed"); return Err(ShellError::Unknown); }
        }
    }
    if let Some(msg) = session_critical_msg(name) {
        ctx.console_writeln(msg);
        return Err(ShellError::Denied);
    }
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return Err(ShellError::Unknown);
    }
    if slot_of(ctx, name).is_none() {
        report(ctx, "not running: ", name);
        return Err(ShellError::Unknown);
    }
    if name == "supervisor" {
        ctx.console_writeln("kill supervisor: the kernel respawns it (Phase 6); it reconciles - adopts the running services, respawns any that died");
    }
    match ctx.kill(name) {
        Ok(())  => { report(ctx, "killed: ", name); Ok(()) }
        Err(_)  => { report(ctx, "kill failed: ", name); Err(ShellError::Unknown) }
    }
}

fn cmd_restart(ctx: &ServiceContext, name: &str, core: Option<u32>) -> Result<(), ShellError> {
    // `restart a,b,c` restarts each per its OWN contract placement. A single `[core]` override is
    // SINGLE-service only - a list plus one core is ambiguous, so the core arg is ignored for a list.
    if name.contains(',') {
        let mut n = 0usize;
        for s in name.split(',') {
            if s.is_empty() || n >= 16 { continue; }
            n += 1;
            let _ = restart_one(ctx, s, None);   // report each; a failure does NOT abort the rest
        }
        return Ok(());
    }
    restart_one(ctx, name, core)
}

/// Restart ONE service. Used directly for a bare `restart <svc> [core]` and per-segment by the comma-list
/// path in cmd_restart (which passes core=None, since a list + one core is ambiguous).
fn restart_one(ctx: &ServiceContext, name: &str, core: Option<u32>) -> Result<(), ShellError> {
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return Err(ShellError::Denied);
    }
    if let Some(msg) = session_critical_msg(name) {
        ctx.console_writeln(msg);
        return Err(ShellError::Denied);
    }
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return Err(ShellError::Unknown);
    }
    match ctx.restart(name, core) {
        Ok(()) => { report(ctx, "restarted: ", name); Ok(()) }
        Err(_) => { report(ctx, "restart failed: ", name); Err(ShellError::Unknown) }
    }
}

// ── chaos - a BOUNDED resilience exerciser (not a generic firehose) ──────────────────────────────
// Each mode stresses ONE named invariant through the shell's EXISTING capabilities (no new kernel
// surface) and reports a loud verdict (§26.6). It can storm ANYTHING restartable - including the
// `supervisor`, which the kernel respawns (Phase 6) - because the only unkillable thing is the
// kernel; the verdict is about KERNEL survival (a panic would reboot before the report could print).
// Ships `kill-storm` + `max-carnage`; flooding/memory-pressure are future modes.

/// Services the supervisor AUTO-restarts on unexpected death (its death-notification loop -
/// services/supervisor). Only these recover from a bare `kill`, so only these make sense as a
/// kill-storm target.
// Directly-restartable services: their OWN death notifies the supervisor, which respawns them
// immediately (the supervisor itself is kernel-respawned). chaos confirms recovery for these each
// round + labels them "recovered"; kill-storm may target them. The only unkillable thing is the
// kernel; the shell is excluded only because chaos runs *inside* it. (xhci/ehci/logger are
// directly-restartable so max-carnage can't leave them dead.)
// `time`, `control` and `dwc2` were MISSING here, and this list is not cosmetic: it is the expansion
// of `kill all-services` and a hard refusal gate for `chaos kill-storm`. So the storm skipped them
// and `chaos kill-storm time` was refused outright - Commandment II ("nothing escapes") silently
// false for the three services whose omission from a DIFFERENT list was the C5-1 finding.
//
// This is the same fact as the supervisor's MANAGED and the kernel's two by-name sets, stated a
// fourth time. It stays a literal for now because the shell cannot see the supervisor's list, but
// the honest fix is to derive it from live tasks the way `chaos` derives its own exclusions - which
// is exactly why chaos has no roster to drift.
const CHAOS_RESTARTABLE: [&str; 11] = ["supervisor", "block-driver", "fs", "xhci", "ehci", "logger",
                                       "nic-driver", "net-stack", "time", "control", "dwc2"];
const CHAOS_DEFAULT_ROUNDS: u32 = 20;
const CHAOS_MAX_ROUNDS: u32 = 100;        // bounded (§26.6) - a deliberate cap, not a firehose
// Per-round recovery wait is bounded by REAL wall-clock time (RTC seconds), not a yield count. A
// yield count is not portable: it was generous in QEMU but too short on the T630 for the heavier,
// kernel-driven SUPERVISOR respawn, so `chaos kill-storm supervisor` undercounted recoveries there
// (the supervisor *did* recover every time - observe showed it - but chaos gave up waiting). 8 s is
// generous; the loop breaks early the instant a new generation appears, so fast targets (fs) stay fast.
const CHAOS_RECOVER_SECS: i64 = 8;
const CHAOS_POLL_EVERY: u32 = 64;         // yields between gen/clock polls (a task_stat scan isn't free)
// After the storm, the target's task has respawned (recovery detected via its task generation),
// but a heavy service like `fs` is not yet *serving* - it still has to re-mount and re-register,
// and its restart log burst is still draining off the serial line. Settle before reporting (so the
// report isn't shredded by that burst on the bounded-THRE serial path) and before saving (so an
// `fs`-target save can actually reach a re-registered fs). Bounded (§26.6); §14.3 retry pattern.
const CHAOS_SETTLE_YIELDS: u32 = 60_000;  // let the just-restarted target re-register + serial drain
// Wall-clock budget (seconds) to keep retrying the report save while `fs` finishes re-mounting after a
// storm. A heavy `max-carnage` kills `fs` AND its `block-driver` many times, so fs may take several
// seconds to serve again; we reacquire + retry until it does, bounded so it never hangs.
const CHAOS_SAVE_TOTAL_SECS: i64 = 30;

/// Save `data` to the already-resolved absolute path `ppath`, retrying for up to
/// `CHAOS_SAVE_TOTAL_SECS` of WALL-CLOCK time while `fs` finishes re-mounting after a chaos storm -
/// reacquiring a fresh `fs` cap each round (it may have just respawned). Bounded: `save_report` is
/// itself wall-clock-bounded, so this never hangs; it gives up gracefully when fs won't stabilise.
fn chaos_save_retry(ctx: &ShellCtx, ppath: &[u8], data: &[u8]) -> bool {
    let t0 = ctx.epoch_secs_monotonic();
    loop {
        let _ = ctx.reacquire_by_name("fs");
        if save_report(ctx, ppath, data) { return true; }
        if ctx.epoch_secs_monotonic() - t0 >= CHAOS_SAVE_TOTAL_SECS { return false; }
        for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    }
}

/// Wait (real wall-clock bounded, RTC) for `name` to be ALIVE (present in the task table). Used
/// before a kill so a round isn't wasted killing a task that is still mid-respawn. Yields cooperatively.
fn chaos_wait_alive(ctx: &ServiceContext, name: &str) {
    let t0 = ctx.epoch_secs_monotonic();
    let mut k = 0u32;
    while slot_of(ctx, name).is_none() {
        ctx.yield_cpu();
        k += 1;
        if k % CHAOS_POLL_EVERY == 0 && ctx.epoch_secs_monotonic() - t0 >= CHAOS_RECOVER_SECS { break; }
    }
}

/// Wait (real wall-clock bounded, RTC - not a yield count, which isn't portable across QEMU/hardware)
/// for `name` to reach a generation different from `og` - proof a fresh instance came up (§7.5). Yields
/// cooperatively so the recoverer (sharing core 0) runs. Returns true on recovery, false on timeout.
fn chaos_wait_recovery(ctx: &ServiceContext, name: &str, og: u32) -> bool {
    let t0 = ctx.epoch_secs_monotonic();
    let mut k = 0u32;
    loop {
        ctx.yield_cpu();
        k += 1;
        if k % CHAOS_POLL_EVERY == 0 {
            if let Some(g) = gen_of(ctx, name) { if g != og { return true; } }
            if ctx.epoch_secs_monotonic() - t0 >= CHAOS_RECOVER_SECS { return false; }
        }
    }
}

fn cmd_chaos(ctx: &ShellCtx, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
    // Tokenize the raw line ourselves - `chaos kill-storm <svc> [rounds] [save <path>]` runs past
    // the shell's MAX_ARGS=4 tokenizer (6 tokens), so we can't rely on the shared `args` array.
    let mut tok: [&str; 8] = [""; 8];
    let mut ntok = 0;
    for t in rest.split_whitespace() {
        if ntok == tok.len() { break; }
        tok[ntok] = t; ntok += 1;
    }
    if ntok == 0 || tok[0] == "help" {
        ctx.console_writeln("chaos - bounded resilience exerciser. modes:");
        ctx.console_writeln("  kill-storm  <svc> [n]   kill a service n times; verify recovery");
        ctx.console_writeln("  flood-storm <svc> [n]   saturate its queue; verify it drains");
        ctx.console_writeln("  mem-pressure      [n]   a mem-pressure allocs to its limit, then reclaim");
        ctx.console_writeln("  spawn-storm       [n]   spawn mem-pressure tasks to the ceiling; loud refusal");
        ctx.console_writeln("  max-carnage <all-services|svc|svc,svc,...> <n>  all-services = RANDOM storm, or aim/list; TARGET + ROUNDS required");
        ctx.console_writeln("                          ('q' aborts; SERIAL only if the run kills the keyboard)");
        ctx.console_writeln("  link-flap         [n]   simulate a cable unplug/replug; net-stack self-configures (net only)");
        ctx.console_writeln("  svc: supervisor | block-driver | fs | logger | xhci | ehci | shell | nic-driver | net-stack");
        return Ok(());
    }
    match tok[0] {
        "kill-storm"   => chaos_kill_storm(ctx, cwd, &tok, ntok),
        "flood-storm"  => chaos_flood_storm(ctx, cwd, &tok, ntok),
        "mem-pressure" => chaos_mem_pressure(ctx, cwd, &tok, ntok),
        "spawn-storm"  => chaos_spawn_storm(ctx, cwd, &tok, ntok),
        "max-carnage"  => {
            // EVERY run needs a TARGET and a positive ROUNDS count. There is NO uncapped default - a
            // firehose is just a big N (`all-services 5000`) and `q` aborts a long run early. `help` = usage.
            // tok[1] = target, tok[2] = rounds.
            if ntok >= 2 && tok[1] == "help" {
                ctx.console_writeln("usage: chaos max-carnage <all-services | svc | svc,svc,...> <rounds>");
                ctx.console_writeln("  all-services   RANDOM carnage over the whole restartable set each round (the honest");
                ctx.console_writeln("                 chaos-monkey: supervisor a normal victim, nothing protected-last)");
                ctx.console_writeln("  <service>      aim every round at one service (e.g. fs, logger)");
                ctx.console_writeln("  svc,svc,...    a comma-separated list: kill EVERY listed service each round (cascade stress)");
                ctx.console_writeln("  <rounds>       REQUIRED for every form - the run is bounded (a firehose is a big N; q aborts early)");
                ctx.console_writeln("  yes            optional 4th word: skip the [y/N] confirm (the warning still prints)");
                ctx.console_writeln("  all run system-wide mem-pressure + spawn-storm. 'q' aborts (SERIAL if the run kills the kbd).");
                ctx.console_writeln("  e.g. chaos max-carnage all-services 5000 | chaos max-carnage fs 50 | chaos max-carnage fs,logger 100");
                Ok(())
            } else {
                // A run needs a TARGET (all-services / a service / a comma-list) AND a positive ROUNDS count.
                // No target (bare, or only a number), or a missing / zero rounds -> refuse LOUDLY (invariant 12).
                let no_target = ntok < 2 || tok[1].bytes().all(|b| b.is_ascii_digit());
                let rounds = if ntok >= 3 { parse_u32(tok[2]).unwrap_or(0) } else { 0 };
                if no_target || rounds == 0 {
                    ctx.console_writeln("usage: chaos max-carnage <all-services | svc | svc,svc,...> <rounds>");
                    ctx.console_writeln("  every run needs a target AND a rounds count - there is no uncapped default.");
                    ctx.console_writeln("  e.g. chaos max-carnage all-services 5000   (the firehose - a big N; q aborts early)");
                    ctx.console_writeln("       chaos max-carnage fs 50");
                    ctx.console_writeln("       chaos max-carnage fs,logger 100");
                    ctx.console_writeln("  add 'yes' as a 4th word to skip the confirm (unattended runs):");
                    ctx.console_writeln("       chaos max-carnage all-services 100 yes");
                    return Ok(());
                }
                // Validate the target(s) before launching (a bad name would storm nothing), loudly (invariant 12).
                let target = tok[1];
                if target.contains(',') {
                    // A comma-list (e.g. "nic-driver,net-stack") is a MULTI-TARGET run: EVERY listed service
                    // is killed each round (semantics B). Each segment must be a live service.
                    for seg in target.split(',') {
                        if seg.is_empty() { continue; }
                        if slot_of(ctx, seg).is_none() {
                            ctx.console_writeln_fmt(format_args!("max-carnage: no live service '{}' in the list", seg));
                            ctx.console_writeln("  every comma-separated target must be a live service");
                            ctx.console_writeln("  (block-driver | fs | logger | xhci | ehci | shell | supervisor | nic-driver | net-stack)");
                            return Ok(());
                        }
                    }
                } else if target != "all-services" && slot_of(ctx, target).is_none() {
                    ctx.console_writeln_fmt(format_args!("max-carnage: no live service '{}'.", target));
                    ctx.console_writeln("  target: all-services, one service, or a comma-separated list");
                    ctx.console_writeln("  (block-driver | fs | logger | xhci | ehci | shell | supervisor | nic-driver | net-stack)");
                    return Ok(());
                }
                // An optional 4th word skips the confirm: `chaos max-carnage all-services 100 yes`.
                // A WORD, not `-y` - utilities/0_conventions.md 4: "Subcommands are words, never
                // single-letter flags", so that a word means the same thing across every utility.
                let preconfirmed = ntok >= 4 && tok[3] == "yes";
                chaos_launch(ctx, target, rounds, preconfirmed)
            }
        }
        "link-flap"    => chaos_link_flap(ctx, &tok, ntok),
        other => {
            ctx.console_writeln_fmt(format_args!(
                "chaos: unknown mode '{}' (try: chaos kill-storm <service> [rounds])", other));
            Err(ShellError::Unknown)
        }
    }
}

/// Yield for up to `secs` of RTC wall-clock, returning true the instant q/Q/ESC is pressed (abort).
/// Bounded + portable (RTC, not the T630-broken TSC).
fn hold_or_abort(ctx: &ServiceContext, secs: i64) -> bool {
    let t0 = ctx.epoch_secs_monotonic();
    while ctx.epoch_secs_monotonic() - t0 < secs {
        while let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' || b == 0x1b { return true; }
        }
        ctx.yield_cpu();
    }
    false
}

/// `chaos link-flap [N]` - a networking-SPECIFIC chaos: simulate a cable unplug/replug N times (default 1)
/// WITHOUT physical access, exercising net-stack's LINK-recovery path (a different failure surface than the
/// process-death that kill-storm / max-carnage test). It forces the nic-driver's REPORTED link DOWN (a
/// report override, ops [6]/[7]/[8] - no hardware touch, no SLU/reset), holds a beat so net-stack's ~1s link
/// poll catches the loss and stalls ping, forces it UP so net-stack self-configures on the up edge (re-runs
/// its DHCP/ARP dance), then CLEARS the override so a real later unplug is never masked. net-stack reacts
/// autonomously (its reaction is in the serial log + a subsequent `net`/`ping`); this drives the physical-
/// layer event and paces it. q-abortable, and an abort always clears the override. This is the FIRST
/// service-specific chaos - standard chaos (kill/flood/storm) is universal; a service's own failure surface
/// (net-stack's link) is its own scenario (do not build a speculative framework, §26.2).
fn chaos_link_flap(ctx: &ServiceContext, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    if ntok >= 2 && tok[1] == "help" {
        ctx.console_writeln("chaos link-flap [N] - simulate a cable unplug/replug N times (default 1)");
        ctx.console_writeln("  forces the NIC link DOWN then UP (a report override, no hardware touch) so net-stack");
        ctx.console_writeln("  notices the loss and self-configures on the up edge. tests LINK recovery, not process death.");
        ctx.console_writeln("  (press q to abort; an abort clears the override)");
        return Ok(());
    }
    let cycles = if ntok >= 2 { parse_u32(tok[1]).unwrap_or(1).max(1) } else { 1 };
    if slot_of(ctx, "nic-driver").is_none() {
        ctx.console_writeln("chaos link-flap: no live nic-driver (is the NIC up?)");
        return Ok(());
    }
    // Hold each edge long enough for net-stack's ~1s link poll to catch it - this simulates the duration of
    // a real cable event (whose PHY settle is itself seconds on hardware). net-stack self-configures on its
    // own; the log shows it. q-abortable throughout, and any exit clears the override.
    const HOLD_SECS: i64 = 3;
    let down = Message::from_bytes(&[6]);
    let up   = Message::from_bytes(&[7]);
    let clr  = Message::from_bytes(&[8]);
    for cycle in 1..=cycles {
        ctx.console_writeln_fmt(format_args!(
            "chaos link-flap: cycle {}/{} - forcing link DOWN (press q to abort)", cycle, cycles));
        match net_query(ctx, "nic-driver", &down, 3) {
            NetQ::Aborted => {
                let _ = net_query(ctx, "nic-driver", &clr, 2);
                ctx.console_writeln("chaos link-flap: aborted (link override cleared)");
                return Ok(());
            }
            // CHECK THE ANSWER. The driver replies `[0]` when its backend has no force-link override -
            // which is every ARM port, where the NIC is in-kernel and there is nothing to override. This
            // used to be discarded, so the trial announced "forcing link DOWN ... done" having done
            // nothing: a chaos run that reads as exercising link recovery and exercises none of it. A
            // test that cannot fail is not a test (Commandment II), and one that reports success is
            // worse than one that is absent.
            NetQ::Reply(r) if r.payload_bytes().first() == Some(&0) => {
                // Two writes rather than one continued literal. A `\` continuation inside a string is
                // easy to lose to a scripted edit, and when it goes the SOURCE INDENTATION becomes part
                // of the message - which is exactly what shipped: runs of spaces mid-sentence on the
                // console. Short literals cannot do that.
                ctx.console_writeln(
                    "chaos link-flap: NOT SUPPORTED by this NIC backend - nothing was forced.");
                ctx.console_writeln(
                    "  The in-kernel ARM NICs have no link override. Unplug the cable to test link \
recovery for real.");
                return Ok(());
            }
            _ => {}
        }
        if hold_or_abort(ctx, HOLD_SECS) {
            let _ = net_query(ctx, "nic-driver", &clr, 2);
            ctx.console_writeln("chaos link-flap: aborted (link override cleared)");
            return Ok(());
        }
        ctx.console_writeln("chaos link-flap: forcing link UP - net-stack should self-configure");
        let _ = net_query(ctx, "nic-driver", &up, 3);
        if hold_or_abort(ctx, HOLD_SECS) {
            let _ = net_query(ctx, "nic-driver", &clr, 2);
            ctx.console_writeln("chaos link-flap: aborted (link override cleared)");
            return Ok(());
        }
    }
    // Clear the override so the REAL link state is reported again (a real unplug must not stay masked).
    let _ = net_query(ctx, "nic-driver", &clr, 2);
    ctx.console_writeln_fmt(format_args!(
        "chaos link-flap: done ({} cycle(s)); override cleared - net now reflects the real link", cycles));
    Ok(())
}

/// `chaos max-carnage` - launch the `chaos` service, which takes over the console (the foreground
/// primitive, syscall 40), runs the storm with the SHELL itself a target now, and on `q` hands the
/// keyboard back + self-terminates. The shell goes "muted" (see the main loop) for the duration. Kill
/// any prior instance first - one-shot, no graceful self-exit race - exactly like `observe now`.
fn chaos_launch(
    ctx: &ServiceContext,
    target: &str,
    rounds: u32,
    // `yes` given on the command line: the operator has already made the decision, so ASK nothing.
    // The warning still prints in full - it is the reason the confirm existed, and an unattended run
    // is exactly when the log needs to say what was about to happen.
    preconfirmed: bool,
) -> Result<(), ShellError> {
    // Loud pre-flight warning + confirm, TAILORED to the target in three cases. all-services storms EVERY
    // driver, so the keyboard dies for sure (serial only). A single USB host driver (xhci/ehci) kills the
    // keyboard ONLY if it is the controller yours is on - we cannot know which, so we state the proviso.
    // Anything else leaves the keyboard alive. The keyboard works HERE, pre-storm, so the confirm lands.
    let target_all = target == "all-services";
    // USB in a comma-list kills the keyboard too, so warn serial for it as well (not just a bare xhci/ehci).
    let target_usb = target.split(',').any(|s| s == "xhci" || s == "ehci");
    ctx.console_writeln("");
    ctx.console_writeln("============ MAXIMUM CARNAGE - READ THIS ============");
    if target_all {
        ctx.console_writeln(" This storm KILLS the USB keyboard drivers (xhci/");
        ctx.console_writeln(" ehci), so your keyboard goes DEAD mid-run and 'q'");
        ctx.console_writeln(" on the keyboard will NOT stop the run.");
        ctx.console_writeln("");
        ctx.console_writeln(" The ONLY way to abort is 'q' in a SERIAL console");
        ctx.console_writeln(" (PuTTY on COM1). Connect serial before continuing.");
    } else if target_usb {
        ctx.console_writeln_fmt(format_args!(" This kills the {} USB driver. If that is the", target));
        ctx.console_writeln(" controller your keyboard is on, it goes DEAD: abort");
        ctx.console_writeln(" with 'q' in a SERIAL console (PuTTY/COM1). If not,");
        ctx.console_writeln(" the keyboard stays alive and 'q' there aborts.");
        ctx.console_writeln(" Use serial if you are not sure.");
    } else {
        ctx.console_writeln(" This storms one service plus system-wide memory +");
        ctx.console_writeln(" task-pool pressure, to prove the KERNEL survives.");
        ctx.console_writeln(" Your keyboard is NOT a target and stays alive, so");
        ctx.console_writeln(" 'q' on the keyboard aborts.");
    }
    ctx.console_writeln("");
    ctx.console_writeln("=====================================================");
    if preconfirmed {
        // Say that the confirm was WAIVED rather than silently skipping it. A log that looks like a
        // prompt was answered when nobody was there is the kind of quiet ambiguity invariant 12 is
        // about - and this is the line that explains, later, why a destructive run started unattended.
        ctx.console_writeln(" Start maximum carnage? [y/N]: yes (given on the command line)");
    } else {
        ctx.console_write(" Start maximum carnage? [y/N]: ");
        // Line-edited confirm (read_confirm): the operator can BACKSPACE a typo before Enter, and the
        // decision is the FINAL line - a mistyped `y` corrected to `n` cancels, not proceeds.
        if !read_confirm(ctx) {
            ctx.console_writeln("max-carnage: cancelled.");
            return Ok(());
        }
    }
    let _ = ctx.kill("chaos");
    if ctx.spawn("chaos").is_err() {
        ctx.console_writeln("chaos: failed to spawn the chaos service");
        return Err(ShellError::Unknown);
    }
    // Send the round count (always > 0 - the shell requires it) AND the target (all-services | service |
    // comma-list). Best-effort: chaos waits briefly for it; if it never arrives chaos runs a safe no-op
    // (0 rounds). Reclaim the cap (no leak).
    if let Some(cap) = ctx.acquire_send_cap("chaos") {
        // rounds(4) + target string. The target may be a comma-separated list (e.g. "nic-driver,net-stack"),
        // so the buffer is sized for a bounded list, not one name.
        let mut buf = [0u8; 4 + 128];
        buf[..4].copy_from_slice(&rounds.to_le_bytes());
        let tb = target.as_bytes(); let n = tb.len().min(128);
        buf[4..4 + n].copy_from_slice(&tb[..n]);
        // TRY_SEND from the SHELL, because the shell is the user's only way back in. A blocking send
        // here hands `chaos` the power to hang the prompt just by having a full queue, which is the one
        // thing nothing above the kernel may do. A refused send is reported and the bounded wait below
        // then reports the real symptom - chaos never took the foreground - instead of the shell simply
        // never returning (§8.9, §26.7).
        if ctx.try_send_by_handle(cap, &Message::from_bytes(&buf[..4 + n])).is_err() {
            ctx.console_writeln("chaos: could not be reached (its queue is full or it is restarting)");
        }
        ctx.remove_cap(cap);
    }
    // Wait (bounded) for chaos to TAKE the console foreground before returning. Otherwise the shell loops
    // back and blocks in console_read BEFORE chaos claims, then sits blocked there for the whole run (never
    // its muted-poll path); on chaos's release that read just re-blocks with no byte, so no fresh `gsh>`
    // repaints on the framebuffer until the user presses Enter (the intermittent "no prompt after chaos
    // done" glitch). Once chaos owns the foreground the shell's loop goes muted and reliably reprints the
    // prompt on regain. Bounded (chaos waits up to 2 s for this count first), so a chaos that never claims
    // still returns and the shell carries on.
    let t0 = ctx.epoch_secs_monotonic();
    while ctx.is_console_foreground() {
        ctx.yield_cpu();
        if ctx.epoch_secs_monotonic() - t0 >= 3 { break; }
    }
    Ok(())
}

/// `chaos kill-storm <svc> [rounds] [save <path>]` - kill the service `rounds` times; each round,
/// wait for the supervisor's death-notification loop to respawn it (a higher restart generation = a
/// new instance) and count it recovered. Returns `Ok` only if every round recovered; the kernel
/// never panicking is proven by the command *returning at all* (a panic reboots). Bounded + loud
/// (§26.6), capability-clean: only `kill` (SERVICE_CONTROL) + `task_stat` (INTROSPECT), both held.
///
/// **The report avoids a catch-22.** Each round is recorded in MEMORY only - chaos never touches fs
/// during the storm, so `chaos kill-storm fs` does not write its log to the very thing it is killing.
/// At the end the report is built in a bounded buffer and printed to the **console** (fs-independent,
/// captured by the serial log); an optional `save <path>` then materialises it to a file once the
/// target has recovered (best-effort - if fs was the target and is down, it falls back to the console).
#[inline(never)]
fn chaos_kill_storm(ctx: &ShellCtx, cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    if ntok < 2 {
        // The service list is PRINTED FROM THE ARRAY, not retyped. This line used to read
        // "(service: supervisor | block-driver | fs)" - three names against the eight the gate
        // actually held, so the usage text was wrong the day it was written and drifted further
        // every time the array grew.
        ctx.console_write("usage: chaos kill-storm <service> [rounds] [save <path>]   (service:");
        for (i, s) in CHAOS_RESTARTABLE.iter().enumerate() {
            ctx.console_write(if i == 0 { " " } else { " | " });
            ctx.console_write(s);
        }
        ctx.console_writeln(")");
        return Err(ShellError::Unknown);
    }
    let svc = tok[1];
    if !CHAOS_RESTARTABLE.contains(&svc) {
        ctx.console_writeln_fmt(format_args!(
            "chaos: '{}' is not a recoverable target - only supervisor/block-driver/fs recover on death (the supervisor respawns the services; the kernel respawns the supervisor). The kernel itself cannot be killed.", svc));
        return Err(ShellError::Unknown);
    }
    // Parse [rounds] and [save <path>] in any order after the service. `rounds` is a bare number;
    // `save` is followed by a path. Both optional.
    let mut rounds = CHAOS_DEFAULT_ROUNDS;
    let mut save: Option<&str> = None;
    let mut i = 2;
    while i < ntok {
        if tok[i] == "save" && i + 1 < ntok { save = Some(tok[i + 1]); i += 2; }
        else if let Some(n) = parse_u32(tok[i]) { rounds = n; i += 1; }
        else { i += 1; }
    }
    let rounds = rounds.clamp(1, CHAOS_MAX_ROUNDS);
    if slot_of(ctx, svc).is_none() {
        ctx.console_writeln_fmt(format_args!("chaos: '{}' is not running", svc));
        return Err(ShellError::Unknown);
    }

    ctx.console_writeln_fmt(format_args!(
        "chaos kill-storm {}: {} rounds - kill, then wait for the supervisor to respawn it...", svc, rounds));

    // Per-round results, tracked in MEMORY (no fs while we storm). Bounded by CHAOS_MAX_ROUNDS.
    let mut old_g = [0u32; CHAOS_MAX_ROUNDS as usize];
    let mut new_g = [0u32; CHAOS_MAX_ROUNDS as usize];
    let mut ok_r  = [false; CHAOS_MAX_ROUNDS as usize];
    let mut recovered = 0u32;
    for r in 0..rounds as usize {
        // Ensure the target is ALIVE before we read its generation and kill it (it may still be
        // mid-respawn from the previous round - esp. the supervisor, Phase 6). Then kill, and wait
        // for a NEW generation (a respawn bumps it, §7.5) - both bounded by real wall-clock time.
        chaos_wait_alive(ctx, svc);
        let og = gen_of(ctx, svc).unwrap_or(0);
        old_g[r] = og;
        let _ = ctx.kill(svc);                     // recovered by the supervisor (services) or the kernel (supervisor, Phase 6)
        if chaos_wait_recovery(ctx, svc, og) {
            new_g[r] = gen_of(ctx, svc).unwrap_or(0); ok_r[r] = true; recovered += 1;
        }
    }

    // Build the report in a bounded buffer (at the END - nothing was written to fs during the storm).
    use core::fmt::Write as _;
    let mut rb = ReportBuf::new();
    let _ = writeln!(rb, "=== chaos kill-storm {}: report ===", svc);
    let recoverer = if svc == "supervisor" { "kernel-respawned" } else { "supervisor-respawned" };
    let _ = writeln!(rb, "target: {} ({}); rounds: {}", svc, recoverer, rounds);
    for r in 0..rounds as usize {
        if ok_r[r] {
            let _ = writeln!(rb, "round {:>3}: killed gen {} -> recovered gen {}", r + 1, old_g[r], new_g[r]);
        } else {
            let _ = writeln!(rb, "round {:>3}: killed gen {} -> NOT RECOVERED (wait bound exceeded)", r + 1, old_g[r]);
        }
    }
    let _ = writeln!(rb, "recovered: {}/{}; kernel: alive (no panic - this command returned)", recovered, rounds);
    let _ = writeln!(rb, "verdict: {}", if recovered == rounds { "PASS" } else { "FAIL" });
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    // Settle: let the just-restarted target finish re-mounting/re-registering and let its restart
    // log burst drain off the serial line, so the report below survives on the wire (the bounded-THRE
    // serial path drops bytes under a cross-core flood) and an `fs`-target save can reach a live fs.
    for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }

    // Always print to the console - fs-independent, so even an `fs` storm reports cleanly.
    console_write_chunked(ctx, rb.bytes());
    // Optionally materialise to a file, now that the target has recovered. Best-effort with a bounded
    // retry: if fs was the target it may still be finishing its remount, so retry the save a few times
    // (yielding between) until it re-registers. If it never comes back in budget, the console report stands.
    if let Some(path) = save {
        let mut pbuf = [0u8; PATH_MAX];
        if let Some(p) = resolve_or_err(ctx, cwd, path, &mut pbuf) {
            let mut ppath = [0u8; PATH_MAX];
            let pl = p.len(); ppath[..pl].copy_from_slice(p);
            if chaos_save_retry(ctx, &ppath[..pl], rb.bytes()) {
                ctx.console_writeln_fmt(format_args!("chaos: report saved to {}", str_of(&ppath[..pl])));
            } else {
                ctx.console_writeln_fmt(format_args!(
                    "chaos: could not save to {} (fs unavailable - it may have been the target; the report above stands)", str_of(&ppath[..pl])));
            }
        }
    }
    if recovered == rounds { Ok(()) } else { Err(ShellError::Unknown) }
}

/// `chaos flood-storm <svc> [rounds]` - saturate a service's IPC queue with a burst of **`try_send`**
/// (never blocking `send`, §8.9 - blocking into a full queue would hang the shell flooding itself),
/// then confirm the service DRAINS it and stays alive. The other resilience axis from kill-storm: not
/// "service gone" but "service overwhelmed" (§8.5 bounded 16-deep queues, §26.6). Each round bursts
/// until the kernel returns `QueueFull` (proving the bound), yields to let the target drain, then
/// re-sends to confirm it recovered. Capability path: a SEND cap acquired by name (`AcquireSendCap`) -
/// floodable = any running service with a registered recv endpoint. Verdict PASS = the service
/// survived every flood and still accepts messages; the kernel never panicking is proven by the
/// command returning at all (a panic reboots). Bounded + loud (§26.6): fixed per-round burst, fixed
/// rounds, fixed report buffer; console-only (no fs dependency).
#[inline(never)]
fn chaos_flood_storm(ctx: &ServiceContext, _cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    const FLOOD_BURST_MAX:    u32 = 64; // cap per-round sends; > queue depth (16) so saturation shows
    /// How long to let the target drain before re-checking, IN MILLISECONDS.
    ///
    /// This was 40 YIELDS, and a count is not a duration - the same fault fixed in `nic-driver`'s
    /// transmit wait tonight. `yield_cpu` returns as soon as the scheduler comes back, so on a quiet
    /// core forty of them elapse in microseconds, and how much WALL CLOCK they cover depends entirely
    /// on what else is runnable.
    ///
    /// That decided the verdict by accident. A service blocked in `recv` is woken by the send itself
    /// and drains within a yield or two, so `logger` passed. A service that idles on a TIMER - `xhci`
    /// with no controller sleeps between drains, and its 5 ms floors to one 10 ms tick - needs real
    /// time, and the check sampled long before it woke. It was reported as "did NOT drain, CLOGGED
    /// (still full) - flood-endpoint disease" when it was not clogged at all: the tool measured too
    /// early and then named a disease after what it saw.
    ///
    /// 50 ms is several of those ticks, still imperceptible in a five-round storm, and it is a CLOCK
    /// (Commandment VIII). A target that has not drained after it is genuinely stuck.
    const FLOOD_DRAIN_MS: u64 = 50;

    if ntok < 2 {
        ctx.console_writeln("usage: chaos flood-storm <service> [rounds]   (any running service with a recv endpoint, e.g. fs | logger | block-driver)");
        return Err(ShellError::Unknown);
    }
    let svc = tok[1];
    let mut rounds = CHAOS_DEFAULT_ROUNDS;
    let mut i = 2;
    while i < ntok { if let Some(n) = parse_u32(tok[i]) { rounds = n; } i += 1; }
    let rounds = rounds.clamp(1, CHAOS_MAX_ROUNDS);

    if slot_of(ctx, svc).is_none() {
        ctx.console_writeln_fmt(format_args!("chaos: '{}' is not running", svc));
        return Err(ShellError::Unknown);
    }
    // A SEND cap to the target's recv endpoint, acquired by name. None = no reachable endpoint
    // (not registered, or a pure sender with nothing to flood).
    let mut handle = match ctx.acquire_send_cap(svc) {
        Some(h) => h,
        None => {
            ctx.console_writeln_fmt(format_args!(
                "chaos: cannot flood '{}' - no reachable recv endpoint (not registered, or a pure sender)", svc));
            return Err(ShellError::Unknown);
        }
    };

    ctx.console_writeln_fmt(format_args!(
        "chaos flood-storm {}: {} rounds - saturate its queue (try_send), then confirm it drains + stays alive...", svc, rounds));

    let msg = Message::from_bytes(&[0x01]); // minimal benign payload; the target drains + drops it
    let mut depth = [0u32;  CHAOS_MAX_ROUNDS as usize]; // sends that landed before QueueFull
    let mut sat_r = [false; CHAOS_MAX_ROUNDS as usize]; // queue actually saturated (hit QueueFull)
    let mut ok_r  = [false; CHAOS_MAX_ROUNDS as usize]; // service DRAINED this round (a re-send LANDED)
    let mut clog_r = [false; CHAOS_MAX_ROUNDS as usize]; // saturated but did NOT drain (re-send still QueueFull)
    let mut survived = 0u32;
    let mut died_at: Option<u32> = None;

    for r in 0..rounds as usize {
        // 1. Burst until the queue saturates (QueueFull) or we hit the cap (the service kept up).
        let mut sent = 0u32;
        let mut died = false;
        while sent < FLOOD_BURST_MAX {
            match ctx.try_send_by_handle(handle, &msg) {
                Ok(())                      => sent += 1,
                Err(IpcError::QueueFull)    => { sat_r[r] = true; break; }
                Err(IpcError::EndpointDead) => { died = true; break; }
                Err(_)                      => break,
            }
        }
        depth[r] = sent;
        // 2. Let the target drain (the flood + any respawn settle).
        ctx.sleep_ms(FLOOD_DRAIN_MS);   // a real settle window - see FLOOD_DRAIN_MS
        if died {
            // The flood killed the service (or it had already died). Record it and reacquire the
            // respawned instance for the next round.
            if died_at.is_none() { died_at = Some(r as u32 + 1); }
            if let Some(nh) = ctx.acquire_send_cap(svc) { ctx.remove_cap(handle); handle = nh; }
            continue;
        }
        // 3. Did it DRAIN? After the yield a fresh send must LAND (Ok) - proof a slot freed, i.e. the service
        // actually recv'd. QueueFull means the queue is STILL full: the service did NOT drain (it is clogged -
        // the flood-endpoint disease), which is a FAIL, not a pass. EndpointDead = it died. (Counting
        // QueueFull as "survived" here was a real bug - it let a permanently-clogged service pass.)
        match ctx.try_send_by_handle(handle, &msg) {
            Ok(())                      => { ok_r[r] = true; survived += 1; }
            Err(IpcError::QueueFull)    => { clog_r[r] = true; } // still full: did NOT drain (clogged)
            Err(IpcError::EndpointDead) => {
                if died_at.is_none() { died_at = Some(r as u32 + 1); }
                if let Some(nh) = ctx.acquire_send_cap(svc) { ctx.remove_cap(handle); handle = nh; }
            }
            Err(_)                      => {}
        }
    }

    // Report - bounded buffer, console-only (flooding needs no fs).
    use core::fmt::Write as _;
    let mut rb = ReportBuf::new();
    let _ = writeln!(rb, "=== chaos flood-storm {}: report ===", svc);
    let _ = writeln!(rb, "target: {}; rounds: {}; burst cap: {}/round", svc, rounds, FLOOD_BURST_MAX);
    for r in 0..rounds as usize {
        if ok_r[r] {
            if sat_r[r] {
                let _ = writeln!(rb, "round {:>3}: saturated at depth {} -> drained, alive", r + 1, depth[r]);
            } else {
                let _ = writeln!(rb, "round {:>3}: {} sends, service kept up (no QueueFull) -> alive", r + 1, depth[r]);
            }
        } else if clog_r[r] {
            let _ = writeln!(rb, "round {:>3}: saturated at depth {} -> did NOT drain, CLOGGED (still full) - flood-endpoint disease", r + 1, depth[r]);
        } else {
            let _ = writeln!(rb, "round {:>3}: depth {} -> service DIED (EndpointDead) - flood not absorbed", r + 1, depth[r]);
        }
    }
    // Final responsiveness check: is the service still accepting after the whole storm?
    let final_alive = match ctx.acquire_send_cap(svc) {
        Some(fh) => {
            let alive = !matches!(ctx.try_send_by_handle(fh, &msg), Err(IpcError::EndpointDead));
            ctx.remove_cap(fh);   // reclaim the probe cap
            alive
        }
        None     => false,
    };
    ctx.remove_cap(handle);   // reclaim the flood handle before returning (no leak across calls)
    let _ = writeln!(rb, "survived: {}/{}; final responsive: {}; kernel: alive (no panic - this command returned)",
                     survived, rounds, if final_alive { "yes" } else { "no" });
    if let Some(d) = died_at {
        let _ = writeln!(rb, "note: first flood-induced death at round {} (if restartable, it respawned)", d);
    }
    let pass = survived == rounds && final_alive;
    let _ = writeln!(rb, "verdict: {}", if pass {
        "PASS (queue saturated + service drained + stayed alive)"
    } else {
        "FAIL (a flood was not absorbed - a round clogged without draining, or the service died)"
    });
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    console_write_chunked(ctx, rb.bytes());
    if pass { Ok(()) } else { Err(ShellError::Unknown) }
}

/// `chaos mem-pressure [rounds]` - on-device memory pressure (§22 S7) through the shell's legitimate
/// caps. Each round spawns the `mem-pressure` victim (which allocates 4 MiB chunks up to its contract limit,
/// then AllocDenied - asserting the §10.3/§10.4 "denied is sticky" invariant in the hog itself), watches
/// the kernel's free-frame count drop while the hog holds its allocation, then KILLS the hog and
/// confirms the frames return to baseline. v1 reclaims memory only at death, so the kill IS the "free";
/// the no-leak check is "the frames come back". Verdict PASS = every round allocated a real chunk AND
/// fully reclaimed it, and the kernel never panicked. Bounded + loud (§26.6): fixed rounds, RTC-bounded
/// polls (break early on success), fixed report buffer, console-only.
#[inline(never)]
fn chaos_mem_pressure(ctx: &ServiceContext, _cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    const MEM_DROP_MIN:  u64 = 4096; // >= 16 MiB held counts as "allocated" (limit 32 MiB = 8192 frames)
    const MEM_SLACK:     u64 = 1024; // 4 MiB tolerance for "reclaimed to baseline" (absorbs system noise)
    const MEM_WAIT_SECS: i64 = 5;    // per-poll wall-clock bound (RTC); polls break early on success

    let mut rounds = CHAOS_DEFAULT_ROUNDS;
    let mut i = 1;
    while i < ntok { if let Some(n) = parse_u32(tok[i]) { rounds = n; } i += 1; }
    let rounds = rounds.clamp(1, CHAOS_MAX_ROUNDS);

    let total    = ctx.inspect_kernel_total_frames();
    let baseline = ctx.inspect_kernel_free_frames();

    ctx.console_writeln_fmt(format_args!(
        "chaos mem-pressure: {} rounds - spawn mem-pressure (allocs to its limit), then kill it and confirm the memory returns...", rounds));

    let mut grabbed = [0u32;  CHAOS_MAX_ROUNDS as usize]; // frames the hog held (baseline - low)
    let mut leaked  = [0i64;  CHAOS_MAX_ROUNDS as usize]; // baseline - recovered (>0 = not fully reclaimed)
    let mut ok_r    = [false; CHAOS_MAX_ROUNDS as usize];
    let mut clean   = 0u32;

    for r in 0..rounds as usize {
        // 1. Spawn the hog; it allocs to its limit on a round-robin core.
        let _ = ctx.spawn("mem-pressure");
        // 2. Wait for the allocation to land - free frames drop. RTC-bounded; breaks early on success.
        let t0 = ctx.epoch_secs_monotonic();
        let mut low = baseline;
        loop {
            ctx.yield_cpu();
            let f = ctx.inspect_kernel_free_frames();
            if f < low { low = f; }
            if baseline.saturating_sub(low) >= MEM_DROP_MIN { break; }
            if ctx.epoch_secs_monotonic() - t0 >= MEM_WAIT_SECS { break; }
        }
        let dropped = baseline.saturating_sub(low);
        grabbed[r] = dropped.min(u32::MAX as u64) as u32;
        // 3. Kill the hog - the only way v1 reclaims its memory (§10.5).
        let _ = ctx.kill("mem-pressure");
        // 4. Wait for reclaim - free frames return toward baseline. RTC-bounded.
        let t1 = ctx.epoch_secs_monotonic();
        let mut hi = low;
        loop {
            ctx.yield_cpu();
            let f = ctx.inspect_kernel_free_frames();
            if f > hi { hi = f; }
            if hi + MEM_SLACK >= baseline { break; }
            if ctx.epoch_secs_monotonic() - t1 >= MEM_WAIT_SECS { break; }
        }
        let leak = baseline as i64 - hi as i64;
        leaked[r] = leak;
        ok_r[r] = dropped >= MEM_DROP_MIN && leak <= MEM_SLACK as i64;
        if ok_r[r] { clean += 1; }
    }

    use core::fmt::Write as _;
    let mut rb = ReportBuf::new();
    let _ = writeln!(rb, "=== chaos mem-pressure: report ===");
    let _ = writeln!(rb, "rounds: {}; mem-pressure limit 32 MiB; system frames: {} total, {} free at baseline", rounds, total, baseline);
    for r in 0..rounds as usize {
        let leak = leaked[r].max(0);
        let _ = writeln!(rb, "round {:>3}: hog held {:>6} frames (~{} MiB) -> after kill, {} frames not back ({})",
            r + 1, grabbed[r], grabbed[r] / 256, leak, if ok_r[r] { "reclaimed" } else { "CHECK" });
    }
    let _ = writeln!(rb, "clean cycles (alloc-to-limit + full reclaim): {}/{}", clean, rounds);
    let _ = writeln!(rb, "kernel: alive (no panic - this command returned)");
    let pass = clean == rounds;
    let _ = writeln!(rb, "verdict: {}", if pass {
        "PASS (memory pressure absorbed + reclaimed)"
    } else {
        "FAIL (no alloc, or memory not reclaimed)"
    });
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    console_write_chunked(ctx, rb.bytes());
    if pass { Ok(()) } else { Err(ShellError::Unknown) }
}

/// Count currently-live, named tasks (valid + not Dead). Bounded scan of the task table.
fn count_live(ctx: &ServiceContext) -> u32 {
    let mut n = 0u32;
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 && !st.name_str().is_empty() { n += 1; }
    }
    n
}

/// Count currently-live tasks with a given name (there can be many - e.g. a swarm of mem-pressure tasks).
fn count_named(ctx: &ServiceContext, name: &str) -> u32 {
    let mut n = 0u32;
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 && st.name_str() == name { n += 1; }
    }
    n
}

/// `chaos spawn-storm [count]` - the GLOBAL-ceiling test (§26.6 bounded behaviour). Spawns mem-pressure
/// victims in a tight loop - each grabs its 32 MiB once scheduled - to slam BOTH global ceilings at
/// once: the task-slot pool (224 kstack slots) and the system frame allocator. Keeps spawning until a
/// spawn is REFUSED (the ceiling, whichever binds first on this machine) or `count`, proving the limit
/// is enforced LOUDLY - a returned `Err`, never a panic. (mem-pressure tests ONE task's limit; this
/// tests the whole system's.) Then kills every hog and confirms full reclaim - the leak-fix's stress
/// test at scale. Verdict PASS = the swarm spawned, the ceiling held without a panic, every hog died,
/// memory returned to baseline, and no pre-existing service was lost. Bounded + loud: hard spawn cap,
/// RTC-bounded reclaim wait, q aborts.
#[inline(never)]
fn chaos_spawn_storm(ctx: &ServiceContext, _cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    const SPAWN_STORM_DEFAULT: u32 = 256;  // aim past most machines' ceilings; the loop stops at the wall
    const SPAWN_STORM_MAX:     u32 = 512;
    const SPAWN_DROP_MIN:      u64 = 4096; // >= 16 MiB dropped = the hog's alloc landed (truth, not a timer)
    const SPAWN_SETTLE_SECS:   i64 = 2;    // per-spawn RTC bound; the drop-poll breaks early on success
    const KILL_SETTLE_SECS:    i64 = 2;    // per-kill RTC bound; waits on the hog COUNT dropping, not a pad
    const RECLAIM_SECS:        i64 = 12;   // RTC bound for the final reclaim wait
    const RECLAIM_SLACK:       u64 = 2048; // 8 MiB tolerance for "back to baseline" (absorbs noise)

    let mut count = SPAWN_STORM_DEFAULT;
    let mut i = 1;
    while i < ntok { if let Some(n) = parse_u32(tok[i]) { count = n; } i += 1; }
    let count = count.clamp(1, SPAWN_STORM_MAX);

    let total       = ctx.inspect_kernel_total_frames();
    let baseline    = ctx.inspect_kernel_free_frames();
    let live_before = count_live(ctx);

    ctx.console_writeln_fmt(format_args!(
        "chaos spawn-storm: spawn up to {} mem-pressure tasks to slam the task-pool + memory ceiling, then kill them all + confirm reclaim. q to quit.", count));

    // 1. Spawn until a spawn is REFUSED (the ceiling) or `count` or q.
    let mut spawned   = 0u32;
    let mut refused_at = 0u32;   // spawn index that got refused (0 = never; reached `count`)
    let mut aborted   = false;
    for n in 0..count {
        if let Some(b) = ctx.try_console_read() { if b == b'q' || b == b'Q' { aborted = true; break; } }
        let before = ctx.inspect_kernel_free_frames();   // free frames before this hog allocates
        if ctx.spawn("mem-pressure").is_err() {
            refused_at = n + 1;   // the ceiling held - graceful refusal, no panic
            break;
        }
        spawned += 1;
        // Wait on TRUTH - the hog's allocation landing (free frames drop by its request) - not a fixed
        // pad. RTC-bounded so a hog that cannot alloc near the ceiling times out instead of hanging us.
        let t = ctx.epoch_secs_monotonic();
        loop {
            ctx.yield_cpu();
            if before.saturating_sub(ctx.inspect_kernel_free_frames()) >= SPAWN_DROP_MIN { break; }
            if ctx.epoch_secs_monotonic() - t >= SPAWN_SETTLE_SECS { break; }
        }
    }

    let low       = ctx.inspect_kernel_free_frames();   // memory floor under the swarm
    let live_peak = count_live(ctx);
    let hogs_peak = count_named(ctx, "mem-pressure");

    // 2. Kill every hog (loop until none remain, confirmed by the task table). Bounded by a safety cap.
    let mut killed = 0u32;
    while slot_of(ctx, "mem-pressure").is_some() && killed < SPAWN_STORM_MAX + 16 {
        let remaining = count_named(ctx, "mem-pressure");
        let _ = ctx.kill("mem-pressure");
        killed += 1;
        // Wait on TRUTH - the hog COUNT dropping (this kill was reaped to Dead) - not a fixed pad. Bounded.
        let t = ctx.epoch_secs_monotonic();
        loop {
            ctx.yield_cpu();
            if count_named(ctx, "mem-pressure") < remaining { break; }
            if ctx.epoch_secs_monotonic() - t >= KILL_SETTLE_SECS { break; }
        }
    }

    // 3. Wait for reclaim - free frames return to ~baseline (deferred kstacks drain on timer ticks).
    let t0 = ctx.epoch_secs_monotonic();
    let mut hi = low;
    loop {
        ctx.yield_cpu();
        let f = ctx.inspect_kernel_free_frames();
        if f > hi { hi = f; }
        if hi + RECLAIM_SLACK >= baseline { break; }
        if ctx.epoch_secs_monotonic() - t0 >= RECLAIM_SECS { break; }
    }
    let recovered  = hi;
    let live_after = count_live(ctx);
    let hogs_after = count_named(ctx, "mem-pressure");

    use core::fmt::Write as _;
    let mut rb = ReportBuf::new();
    let _ = writeln!(rb, "=== chaos spawn-storm: report ===");
    if aborted { let _ = writeln!(rb, "stopped early (you pressed q)"); }
    let _ = writeln!(rb, "system frames: {} total, {} free at baseline; live tasks before: {}", total, baseline, live_before);
    if refused_at > 0 {
        let _ = writeln!(rb, "ceiling: HIT at spawn #{} - the kernel REFUSED the spawn (loud Err, no panic). peak hogs {}, memory floor {} frames", refused_at, hogs_peak, low);
    } else {
        let _ = writeln!(rb, "ceiling: not reached - spawned all {} hogs (peak hogs {}), memory floor {} frames (machine had the headroom)", spawned, hogs_peak, low);
    }
    let _ = writeln!(rb, "peak live tasks: {}", live_peak);
    let _ = writeln!(rb, "killed {} hogs; reclaim: {} free now ({} below baseline), hogs left {}", killed, recovered, baseline.saturating_sub(recovered), hogs_after);
    let _ = writeln!(rb, "live tasks after: {} (baseline was {})", live_after, live_before);
    let _ = writeln!(rb, "kernel: alive (no panic - this command returned)");
    let reclaimed = recovered + RECLAIM_SLACK >= baseline && hogs_after == 0;
    let pass = !aborted && spawned > 0 && reclaimed && live_after >= live_before;
    let _ = writeln!(rb, "verdict: {}", if aborted {
        "ABORTED"
    } else if pass {
        "PASS (ceiling held loudly + full reclaim + no service lost)"
    } else {
        "FAIL (no reclaim, hogs left, or a service went missing)"
    });
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    console_write_chunked(ctx, rb.bytes());
    if pass { Ok(()) } else { Err(ShellError::Unknown) }
}


// ---------------------------------------------------------------------------
// File commands - ls / read / write / mkdir / cd (utilities/16..20). Shell built-ins
// that send the fs file API to `fs` over IPC; `fs` holds + enforces all disk authority.
// The shell tracks the current location (a drive+directory pointer) and resolves
// relative / `.` / `..` paths to an absolute path before sending - fs only walks
// absolute paths from root (it has no notion of "current directory").
// ---------------------------------------------------------------------------

/// The current directory on the (single) drive - an absolute path like "/" or "/etc". Also
/// remembers the *previous* directory so `cd -` can toggle back (both default to root).
struct Cwd {
    buf: [u8; PATH_MAX],
    len: usize,
    prev: [u8; PATH_MAX],
    prev_len: usize,
}

impl Cwd {
    fn root() -> Self {
        let mut buf = [0u8; PATH_MAX];
        buf[0] = b'/';
        let mut prev = [0u8; PATH_MAX];
        prev[0] = b'/';
        Cwd { buf, len: 1, prev, prev_len: 1 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("/")
    }
    /// Move to `path`, saving the directory we're leaving as the previous (for `cd -`). Only
    /// ever called on a *successful* cd, so `prev` always names a directory that existed.
    fn set(&mut self, path: &[u8]) {
        self.prev[..self.len].copy_from_slice(&self.buf[..self.len]);
        self.prev_len = self.len;
        let n = path.len().min(PATH_MAX);
        self.buf[..n].copy_from_slice(&path[..n]);
        self.len = n.max(1);
        if self.len == 0 { self.buf[0] = b'/'; self.len = 1; }
    }
}

/// Resolve `input` against the current directory `cwd` into a normalized absolute path
/// in `out`. Handles absolute (`/a`), relative (`a/b`), `.` and `..`. Returns the length,
/// or None if it would overflow `out`.
fn resolve_path(cwd: &str, input: &str, out: &mut [u8; PATH_MAX]) -> Option<usize> {
    out[0] = b'/';
    let mut len = 1usize;
    // Seed with the current directory unless the input is absolute.
    if !input.starts_with('/') {
        for comp in cwd.split('/').filter(|c| !c.is_empty()) {
            push_comp(out, &mut len, comp)?;
        }
    }
    for comp in input.split('/').filter(|c| !c.is_empty()) {
        match comp {
            "." => {}
            ".." => pop_comp(out, &mut len),
            _ => push_comp(out, &mut len, comp)?,
        }
    }
    Some(len)
}

/// Append a path component, inserting a '/' separator unless `out` already ends with one.
fn push_comp(out: &mut [u8; PATH_MAX], len: &mut usize, comp: &str) -> Option<()> {
    let cb = comp.as_bytes();
    let need = if out[*len - 1] == b'/' { cb.len() } else { cb.len() + 1 };
    if *len + need > PATH_MAX { return None; }
    if out[*len - 1] != b'/' { out[*len] = b'/'; *len += 1; }
    out[*len..*len + cb.len()].copy_from_slice(cb);
    *len += cb.len();
    Some(())
}

/// Remove the last path component (the `..` case), never going above root "/".
fn pop_comp(out: &mut [u8; PATH_MAX], len: &mut usize) {
    // Find the last '/' in out[..len]; truncate there (or to root).
    let mut i = *len;
    while i > 1 {
        i -= 1;
        if out[i] == b'/' { *len = i.max(1); return; }
    }
    *len = 1; // back to root
}

/// Resolve `input` against `cwd`; on overflow print an error and return None.
fn resolve_or_err<'a>(ctx: &ServiceContext, cwd: &Cwd, input: &str, out: &'a mut [u8; PATH_MAX]) -> Option<&'a [u8]> {
    match resolve_path(cwd.as_str(), input, out) {
        Some(n) => Some(&out[..n]),
        None => { ctx.console_writeln("path too long"); None }
    }
}

/// Send an fs file-API request `[op, path_len, path, data]` and return the reply.
/// The next correlation tag for an fs request.
///
/// Replies were matched to requests by ARRIVAL ORDER alone, which holds only while nothing is ever
/// overtaken. After a USB stick replug the device is slow, a `.gsh_history` write is still in flight when
/// the next command's request goes out, and the replies come back one behind - so `ls` read the write's
/// one-byte `[FS_OK]`, saw a reply too short to be a listing, and reported a storage error about a
/// filesystem that was perfectly fine. The channel then stayed one behind indefinitely, which is why the
/// SECOND `ls` always worked and why every storage-layer fix left the symptom untouched.
///
/// A tag makes the match structural instead of circumstantial: the client stamps each request, fs echoes
/// it, and an answer to a different question is recognisable as one. Cycles 1..=255 and never uses 0, so
/// a zero byte can only be an untagged sender - a mismatch that fails loudly rather than aliasing a real
/// tag. Wrapping is harmless: correlation only needs to distinguish requests that can be in flight at the
/// same time, and there are at most a handful.
/// Ask the `time` service a question. One reacquire-and-retry, for the reason `block-driver` learned
/// in arm32 slice 3c: `find_send_slot` does not resolve a name, so a peer that restarted - or that
/// started after us - is unreachable until we ask again, and `request_with_reply` returns None
/// INSTANTLY when the slot was never wired. That reads as "the clock is broken" rather than "we never
/// looked it up", which is the kind of silence this system forbids.
fn time_rpc(ctx: &ShellCtx, body: &[u8]) -> Option<Message> {
    // Bounded, and on the INPUT PATH: `time_source` runs in the shell's main loop before
    // `console_read`, so an unbounded wait here is a dead prompt with no `q` and no hint - and the
    // same call sits in front of `reboot`, putting the escape hatch behind the hang.
    const TIME_SECS: i64 = 2;
    if let Some(r) = ctx.request_with_reply_deadline("time", &Message::from_bytes(body), TIME_SECS) {
        return Some(r);
    }
    let _ = ctx.reacquire_by_name("time");
    ctx.request_with_reply_deadline("time", &Message::from_bytes(body), TIME_SECS)
}

/// `time_source`, but distinguishing NO ANSWER from an answer that simply is not NTP yet.
///
/// The difference decides whether asking again is cheap or expensive, and only the expensive case
/// hurts. An answer costs a round trip; a NON-answer costs the full `time_rpc` budget - two seconds,
/// then a reacquire and another two - and that bill is paid IN FRONT OF THE KEYBOARD READ, so the
/// prompt is dead for four seconds at a time. Polling that every iteration for the first thirty
/// seconds of every boot is what made a networkless machine feel broken at the prompt, and it is what
/// desynchronised `osdev test shell` badly enough to fail 127 of 132 checks: the harness fired
/// commands into a 64-byte serial ring that nothing was draining.
fn time_source_probe(ctx: &ShellCtx) -> Option<ClockSource> {
    let r = time_rpc(ctx, &[1])?;              // None = `time` did not answer at all
    let p = r.payload_bytes();
    if p.len() < 10 || p[0] == 0 { return Some(ClockSource::Unset); }
    Some(match p[9] {
        1 => ClockSource::Rtc,
        2 => ClockSource::Ntp,
        3 => ClockSource::Floor,
        _ => ClockSource::Unset,
    })
}

/// Where the current wall-clock reading came from, per the `time` service (clock slice 2).
fn time_source(ctx: &ShellCtx) -> ClockSource {
    match time_rpc(ctx, &[1]) {   // OP_NOW -> [ok, epoch(8), source]
        Some(r) => {
            let p = r.payload_bytes();
            if p.len() < 10 || p[0] == 0 { return ClockSource::Unset; }
            match p[9] {
                1 => ClockSource::Rtc,
                2 => ClockSource::Ntp,
                3 => ClockSource::Floor,
                _ => ClockSource::Unset,
            }
        }
        None => ClockSource::Unset,
    }
}

/// The clock's floor, per the `time` service (clock slice 2). `None` if it could not be asked - an
/// unanswered question is not the same as a floor of zero, and conflating them would let a failed
/// lookup silently authorise a time before the last boot.
fn time_floor(ctx: &ShellCtx) -> Option<i64> {
    let r = time_rpc(ctx, &[3])?;   // OP_FLOOR_GET -> [ok, floor(8)]
    let p = r.payload_bytes();
    if p.len() < 9 || p[0] == 0 { return None; }
    let mut b = [0u8; 8];
    b.copy_from_slice(&p[1..9]);
    Some(i64::from_le_bytes(b))
}

/// Raise the clock floor, via the `time` service. Returns false if it refused (the floor only rises)
/// or could not be reached - both are real failures and the caller reports them.
fn time_floor_set(ctx: &ShellCtx, epoch: i64) -> bool {
    let mut body = [0u8; 9];
    body[0] = 4;                                     // OP_FLOOR_SET
    body[1..9].copy_from_slice(&epoch.to_le_bytes());
    match time_rpc(ctx, &body) {
        Some(r) => r.payload_bytes().first() == Some(&1),
        None => false,
    }
}

/// The wall clock's current reading, per the `time` service. `None` when it could not be asked - the
/// caller must not substitute a guess, because a rendered date is indistinguishable from a known one.
fn time_now(ctx: &ShellCtx) -> Option<i64> {
    let r = time_rpc(ctx, &[1])?;                    // OP_NOW -> [ok, epoch(8), source, age(8)]
    let p = r.payload_bytes();
    if p.len() < 10 || p[0] == 0 { return None; }
    let mut b = [0u8; 8];
    b.copy_from_slice(&p[1..9]);
    Some(i64::from_le_bytes(b))
}

/// Seconds since the network last set the clock, or `None` if it never did this boot. Rides on OP_NOW
/// so the age cannot disagree with the reading it describes.
fn time_synced_secs_ago(ctx: &ShellCtx) -> Option<i64> {
    let r = time_rpc(ctx, &[1])?;
    let p = r.payload_bytes();
    if p.len() < 18 || p[0] == 0 { return None; }
    let mut b = [0u8; 8];
    b.copy_from_slice(&p[10..18]);
    match i64::from_le_bytes(b) { a if a < 0 => None, a => Some(a) }
}

fn next_fs_tag(ctx: &ShellCtx) -> u8 {
    // C6-1: this counter used to be `static FS_TAG: AtomicU8` - unowned global mutable state
    // (Invariant 9) wearing a thread-safe type. Nothing here is concurrent; the shell is one task on
    // one core. It was a static because that was easier than giving it a home, which is how this
    // violation always arrives.
    //
    // Its home is the fs channel, and the fs channel belongs to the shell, so it lives in `ShellCtx`.
    let t = ctx.fs_tag.get().wrapping_add(1);
    let t = if t == 0 { 1 } else { t };   // never 0: a zero tag can only be an untagged sender
    ctx.fs_tag.set(t);
    t
}

/// How many overtaken replies to discard before giving up. A desync is at most a few requests deep (the
/// shell has one command in flight plus a history write), so this is a bound on a bug, not a workload -
/// hitting it means something is wrong in a way that waiting longer will not fix (§26.6).
const FS_STALE_MAX: u32 = 16;

/// Take the reply that answers OUR request, discarding any that answer an earlier one.
///
/// Discarding is safe precisely because the sender has moved on: a reply whose tag we are no longer
/// waiting for belongs to a request whose caller has already returned. Keeping it would only let it be
/// mistaken for a later answer, which is the failure this exists to end.
/// Wait for the reply carrying `tag`, discarding replies belonging to requests already abandoned.
///
/// THE DEADLINE IS FOR THE WHOLE WAIT, not for each attempt. Every discarded reply used to start a
/// fresh full-length wait, so the real bound was FS_STALE_MAX x max_secs - sixteen times what the
/// caller asked for, and on an interactive command that is indistinguishable from a hang.
///
/// Hardware showed it: after a 98-round chaos storm the block protocol came back out of step
/// ("fs: block read at lba 7702 got a MALFORMED reply ... protocol desync"), the shell discarded one
/// stale reply, waited for a tag that was never coming, and the operator pulled the power at about
/// seventy seconds. The guard detected the desync correctly and then had nowhere to go.
///
/// A bound that multiplies is not a bound (§26.6), and the Rule Above The Rules is that a dependency
/// which cannot answer must produce a loud failure rather than silence.
fn fs_take_tagged(ctx: &ShellCtx, tag: u8, first: ReqOutcome, max_secs: i64) -> ReqOutcome {
    let t0 = ctx.epoch_secs_monotonic();
    let mut outcome = first;
    for _ in 0..FS_STALE_MAX {
        match outcome {
            ReqOutcome::Reply(r) => {
                let p = r.payload_bytes();
                if p.first() == Some(&tag) { return ReqOutcome::Reply(Message::from_bytes(&p[1..])); }
                // Say it - a discarded reply is proof the correlation is load-bearing, and a silent
                // guard cannot tell us whether it ever fires (§26.4).
                ctx.log_fmt(format_args!(
                    "shell: discarded an fs reply for tag {} while awaiting {} (an earlier request was overtaken)",
                    p.first().copied().unwrap_or(0), tag));
                // Wait again WITHOUT re-sending: the request is already with fs, and sending it twice
                // would ask for the work twice. But only for the time the caller has LEFT.
                let spent = ctx.epoch_secs_monotonic().saturating_sub(t0);
                let left = max_secs.saturating_sub(spent);
                if left <= 0 {
                    ctx.console_writeln("fs: no reply for this request - the storage protocol is out of step");
                    return ReqOutcome::Timeout;
                }
                outcome = ctx.recv_abortable_deadline(left);
            }
            other => return other,
        }
    }
    // Sixteen stale replies in one wait is not a slow disk, it is a protocol that has lost its place.
    // Say so: the caller reports a failed operation either way, but only this knows WHY.
    ctx.console_writeln("fs: too many out-of-order replies - the storage protocol is out of step");
    ReqOutcome::Timeout
}

/// Send an already-formed fs request body (one that does not fit the `[op, plen, path, data]` shape -
/// a bare opcode, or `drives label`), TAGGED, and return the reply body with the tag stripped.
///
/// These sites used to build and send their own `Message` and so would have shipped an untagged request,
/// which fs would read as `tag = <opcode>` and dispatch on the byte after it - a silent misparse. Routing
/// them through one helper is what makes "every name-addressed request carries a tag" true rather than
/// mostly true (Commandment III: one path, not two).
/// How long `fs` gets to answer, BY WHAT IT WAS ASKED TO DO.
///
/// Every wait in this file was `3600`, with comments calling it "effectively unbounded - q is the
/// real exit". That is not a bound, and making the USER the timeout is the one thing nothing above
/// the kernel may do: a missing, dead or slow dependency must RETURN with a loud "unavailable". The
/// callers already handle that outcome correctly - they were simply never reached, because nobody
/// sits for an hour. It hung `tree` once and `write` once, and both looked like a dead machine.
///
/// Split by operation rather than flattened, because one number is wrong at one end or the other:
/// short enough to bound a round trip aborts a whole-disk scan, and long enough for the scan is an
/// afternoon for a round trip. The first attempt at this fixed ONE helper and left the one with
/// forty callers, which is why `write` still hung after `tree` was fixed.
const FS_ANSWER_SECS: i64 = 20;   // a round trip: milliseconds healthy, ~1 s across an fs respawn
const FS_TREE_SECS:   i64 = 120;  // recursive delete - bounded by how much there is to remove
/// fsck and scrub walk the FILE TREE from the root (`check_subtree` / `scrub_subtree` in `fs`), not
/// every block of the volume - the measured run reported "verified 34 blocks across 17 files, 8
/// dirs", and the slowest observed `check` was ~14 s. 60 s is four times that.
///
/// This was 600, on my assumption that they swept the whole disk. I did not check, and the cost was
/// the user sitting in front of a `drives scrub` that would not answer for ten minutes - a bound so
/// loose it is indistinguishable from the hang it replaced.
const FS_FSCK_SECS:   i64 = 60;
/// Format and reset DO rewrite the volume, and are the one thing here that legitimately takes
/// minutes on a 15 GiB stick.
const FS_FORMAT_SECS: i64 = 600;

fn fs_raw(ctx: &ShellCtx, body: &[u8], max_secs: i64) -> Option<Message> {
    let tag = next_fs_tag(ctx);
    let mut req = [0u8; 4096];
    req[0] = tag;
    let n = body.len().min(req.len() - 1);
    req[1..1 + n].copy_from_slice(&body[..n]);
    let msg = Message::from_bytes(&req[..1 + n]);
    drain_stale_fs_replies(ctx);
    // A9-4: same abortable, reacquiring call `fs_request` uses.
    //
    // This was the plain `request_with_reply`, the one fs helper that was neither q-abortable nor
    // reacquiring - so `drives`, `drives flash`, `reset` and `label` hung on a slow `fs` with no way
    // out, and never recovered from an `fs` restart because nothing re-looked-up the name (§14.3).
    // Every one of those is a command the operator runs precisely when storage is misbehaving, which
    // is exactly when `fs` is most likely to be slow or restarting.
    let first = ctx.request_with_reply_abortable("fs", &msg, max_secs);
    match fs_take_tagged(ctx, tag, first, max_secs) {
        ReqOutcome::Reply(r) => Some(r),
        _ => None,
    }
}

fn fs_request(ctx: &ShellCtx, op: u8, path: &[u8], data: &[u8]) -> Option<Message> {
    let pl = path.len().min(255);
    let mut req = [0u8; 4096];
    let tag = next_fs_tag(ctx);
    req[0] = tag;
    req[1] = op;
    req[2] = pl as u8;
    req[3..3 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 3 - pl);
    req[3 + pl..3 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..3 + pl + dn]);
    // DELETE_TREE is the one genuinely slow operation routed through here; the rest are round trips.
    let secs = if op == OP_DELETE_TREE { FS_TREE_SECS } else { FS_ANSWER_SECS };
    drain_stale_fs_replies(ctx);          // an earlier abandoned reply must not be read as ours
    // Bounded by `secs` (see FS_ANSWER_SECS): it was the same "effectively unbounded, q is the real
    // exit" budget the interactive path used, and it bounds the wait for a REPLACEMENT reply after
    // discarding an overtaken one. Still ABORTABLE, so `q` remains an EARLY exit while `fs` is merely
    // slow - it is no longer the only exit.
    let first = ctx.request_with_reply_abortable("fs", &msg, secs);
    if matches!(first, ReqOutcome::Aborted) {
        return None; // the user pressed q - that is an answer, not a failure to retry
    }
    // Wait for a REPLACEMENT only if a reply actually arrived (possibly an overtaken one).
    //
    // This is the stale-cap hang. When `fs` restarts, the shell's cached cap goes EndpointDead and
    // the send fails outright - no reply, and none coming. Feeding that into an hour-long wait for
    // a "replacement" meant the reacquire-and-retry immediately below was NEVER REACHED: the
    // recovery path existed, was correct, and sat behind an hour-long wait for something that could
    // not arrive. Reproduced in QEMU with `kill fs` then `drives`: one gen-mismatch line and then
    // silence, with q and every later command ignored.
    //
    // A missing reply is not a late reply. Only the latter is worth waiting for.
    if matches!(first, ReqOutcome::Reply(_)) {
        if let ReqOutcome::Reply(r) = fs_take_tagged(ctx, tag, first, secs) {
            return Some(r);
        }
    }
    // No reply usually means `fs` restarted and our cached cap is now EndpointDead (Phase D,
    // §14.3). Reacquire a fresh `fs` cap by name and retry once; if `fs` hasn't
    // finished re-registering yet, this returns None and the next command retries.
    // Bracket the reacquire. The hang sits between the failed send and the retry, and two wrong
    // diagnoses have already come from reasoning about which call blocks instead of proving it.
    // "reacquiring" without "reacquired" = this call; neither = the send never returned; both = the
    // retry below.
    ctx.print("  [diag] fs send failed - reacquiring by name\r\n");
    let got = ctx.reacquire_by_name("fs");
    ctx.print(if got { "  [diag] reacquired fs - retrying\r\n" } else { "  [diag] reacquire FAILED\r\n" });
    if got {
        drain_stale_fs_replies(ctx);
        // The retry is a NEW request and needs its own tag - reusing the first one would accept the
        // dead instance's late reply as this one's answer, which is the whole class of bug being closed.
        let tag2 = next_fs_tag(ctx);
        let mut req2 = req;
        req2[0] = tag2;
        let msg2 = Message::from_bytes(&req2[..3 + pl + dn]);
        // Same rule on the retry: never wait out an hour for a reply that was never sent.
        let second = ctx.request_with_reply_abortable("fs", &msg2, secs);
        if matches!(second, ReqOutcome::Aborted) {
            return None;
        }
        if !matches!(second, ReqOutcome::Reply(_)) {
            return None; // fs still unreachable - the next command retries (§14.3)
        }
        if let ReqOutcome::Reply(r) = fs_take_tagged(ctx, tag2, second, secs) {
            return Some(r);
        }
    }
    None
}

/// Wall-clock budget (seconds) for the chaos-report save's fs request. The save runs right after a
/// chaos storm that may have hammered `fs` + its `block-driver`, so the reply could be slow or never
/// come; this bounds it so the save can fail gracefully (console report stands) instead of hanging.
const SAVE_FS_MAX_SECS: i64 = 8;
/// Short deadline for the best-effort history write-through (§26.7): a quick try, then shrug. A slow or
/// mid-restart fs must never freeze the prompt, so this is far shorter than the report save's 8 s.
const HIST_SAVE_SECS: i64 = 2;
/// Short deadline for the best-effort history *load* at startup (§26.7). The load runs before the input
/// loop, so a fs that is alive-but-not-serving (respawned but still re-mounting) would otherwise hang the
/// prompt forever on an unbounded request - the whole shell wedges and the keyboard looks dead. Bound it:
/// a quick try, then "no history", so the prompt + `console_read` always come up regardless of fs health.
const HIST_LOAD_SECS: i64 = 2;

/// `fs_request` for the report save: the reply wait is bounded by `SAVE_FS_MAX_SECS` of wall-clock
/// time (RTC), so a still-restarting `fs` can't block the shell forever (the bug behind `chaos
/// max-carnage … save` hanging). Reacquire + retry once on a miss, then give up.
fn fs_request_bounded(ctx: &ShellCtx, op: u8, path: &[u8], data: &[u8], max_secs: i64) -> Option<Message> {
    let pl = path.len().min(255);
    let mut req = [0u8; 4096];
    let tag = next_fs_tag(ctx);
    req[0] = tag;
    req[1] = op;
    req[2] = pl as u8;
    req[3..3 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 3 - pl);
    req[3 + pl..3 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..3 + pl + dn]);
    drain_stale_fs_replies(ctx);          // an earlier abandoned reply must not be read as ours
    let first = ctx.request_with_reply_deadline("fs", &msg, max_secs).map_or(ReqOutcome::Timeout, ReqOutcome::Reply);
    if let ReqOutcome::Reply(r) = fs_take_tagged(ctx, tag, first, max_secs) {
        return Some(r);
    }
    // TIMED OUT. The request was already SENT, so `fs` will reply into our endpoint whether we are still
    // listening or not - and an unclaimed reply is not harmless: it sits in the queue and the NEXT fs
    // request reads IT instead of its own answer. That is not hypothetical. A 2 s read of a non-existent
    // `/clock.last` at boot (fs still mounting) timed out, and its late 1-byte `[FS_NOTFOUND]` was then
    // consumed by `drives`, which reported "no disk found" on a healthy, mounted 15 GB disk. The bound was
    // right; abandoning the reply without reclaiming it was the bug.
    //
    // So spend a SHORT grace collecting the late reply purely to discard it. The abortable request path
    // solves the same problem with a drain at its own top; this path had no equivalent.
    // Timed out. Do NOT try to reclaim the late reply here - that is the race described in
    // `drain_stale_fs_replies`. The next request drains it instead, which is decisive.
    if ctx.reacquire_by_name("fs") {
        drain_stale_fs_replies(ctx);
        let tag2 = next_fs_tag(ctx);
        let mut req2 = req;
        req2[0] = tag2;
        let msg2 = Message::from_bytes(&req2[..3 + pl + dn]);
        let again = ctx.request_with_reply_deadline("fs", &msg2, max_secs).map_or(ReqOutcome::Timeout, ReqOutcome::Reply);
        if let ReqOutcome::Reply(r) = fs_take_tagged(ctx, tag2, again, max_secs) { return Some(r); }
    }
    None
}

/// Discard anything already queued on our endpoint BEFORE sending an fs request.
///
/// This is the only reliable cure for a desynchronised reply channel, and it belongs at the START of a
/// request rather than at the end of a failed one. An fs reply carries no request identity, so a reply
/// abandoned by an earlier caller - one that timed out, or that the user aborted with `q` - is
/// indistinguishable from ours once it is sitting in the queue. Trying to reclaim it AFTER the fact is a
/// race that cannot be won: if the late reply has not arrived yet, we move on and the NEXT command eats
/// it. Draining first is decisive, because at the instant we are about to send, every queued message is
/// by definition somebody else's leftover.
///
/// Safe here because the shell is a pure CLIENT of fs on this endpoint - it does not serve requests on it.
/// That is exactly why `request_with_reply_abortable` already drains at its own top; interactive commands
/// have been relying on it. The bounded and unbounded paths had no equivalent, which is how a single
/// abandoned reply at boot turned into `drives` reporting a healthy disk as absent, and then into the
/// shell announcing "flash FAILED" for a format that was still running.
///
/// Bounded: at most a handful of discards, so a peer stuck emitting messages cannot spin us here.
fn drain_stale_fs_replies(ctx: &ServiceContext) {
    for _ in 0..8 {
        if ctx.try_recv().is_none() { return; }
        // SEC-35, client half: a discarded message may carry an EMBEDDED CAP, and the kernel has
        // already installed it and queued its slot. Dropping the message does not drop the cap - it
        // leaves an entry in a FIFO that `take_pending_cap()` reads from, so the NEXT `open` receives
        // the cap belonging to this discarded reply. That is how `fcap`'s "read-only" handle came to
        // name an earlier open's READ|WRITE cap and a write under it succeeded.
        //
        // Draining here keeps the queue's meaning honest: at most the caps of messages we actually
        // kept. Removing it also reclaims the slot, so a discarded grant is not a leak either.
        while let Some(h) = ctx.take_pending_cap() {
            ctx.remove_cap(h);
        }
    }
}

/// `fs_request` for INTERACTIVE commands (`ls`, `cd`, `read`, `find`, ...): q-abortable, and after a
/// short lingering threshold it prints a "(q to quit)" hint so the user can bail on a slow op instead
/// of waiting blind. A fast reply prints NOTHING (no nag on a snappy op). Mirrors the net commands'
/// abort convention (`ReqOutcome`): `Reply(r)` = answered, `Aborted` = user pressed q (hint already
/// shown), `Timeout` = fs unreachable. On a Timeout (send failed - `fs` restarted, cached cap went
/// EndpointDead, Phase D §14.3) it reacquires `fs` by name and retries once. The plain blocking
/// `fs_request` stays for internal/cleanup ops (deletes, tests) the user never waits on interactively.
fn fs_request_q(ctx: &ShellCtx, op: u8, path: &[u8], data: &[u8]) -> ReqOutcome {
    const HINT_SECS: i64 = 2;    // print "(q to quit)" only if the wait lingers past this
    // How long `fs` gets to answer before the shell declares storage unavailable.
    //
    // **This was 3600, with the comment "effectively unbounded - fs replies fast now; q is the real
    // exit".** That is not a bound, and "q is the real exit" makes the USER the timeout: a `tree`
    // whose LIST_DIR never came back sat for an hour looking hung, and the reacquire-and-retry path
    // below gives it a second hour. It is the rule above the rules - nothing above the kernel may
    // hang; a missing, dead or slow dependency must RETURN with a loud "unavailable". Every caller
    // already handles that outcome properly ("tree: storage unavailable"); they were simply never
    // reached.
    //
    // 20 s is generous for what this helper actually carries. Its ONLY callers are READ_FILE,
    // STAT_FILE and LIST_DIR - operations that complete in milliseconds on a healthy mount, and
    // whose worst legitimate case is an `fs` that died and is being respawned (~1 s). The whole-disk
    // work that genuinely takes minutes - check, scrub, flash - does not come through here.
    //
    // The retry doubles it, so a truly dead `fs` costs 40 s and then says so, instead of costing an
    // afternoon and saying nothing.
    const MAX_SECS:  i64 = FS_ANSWER_SECS; // one source for this bound, not a second copy of 20
    let pl = path.len().min(255);
    let mut req = [0u8; 4096];
    let tag = next_fs_tag(ctx);
    req[0] = tag;
    req[1] = op;
    req[2] = pl as u8;
    req[3..3 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 3 - pl);
    req[3 + pl..3 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..3 + pl + dn]);
    let first = ctx.request_with_reply_qhint("fs", &msg, HINT_SECS, MAX_SECS, || ctx.console_writeln("  (q to quit)"));
    match fs_take_tagged(ctx, tag, first, MAX_SECS) {
        // Send failed (stale cap after an fs restart): reacquire by name and retry once, still hinted.
        // A fresh tag for the fresh request - see `fs_request`.
        ReqOutcome::Timeout if ctx.reacquire_by_name("fs") => {
            let tag2 = next_fs_tag(ctx);
            let mut req2 = req;
            req2[0] = tag2;
            let msg2 = Message::from_bytes(&req2[..3 + pl + dn]);
            let again = ctx.request_with_reply_qhint("fs", &msg2, HINT_SECS, MAX_SECS, || ctx.console_writeln("  (q to quit)"));
            fs_take_tagged(ctx, tag2, again, MAX_SECS)
        }
        other => other,
    }
}

/// Send a BARE single-opcode request (no path, no data) to `fs`, q-abortable with a hint - for the
/// whole-disk operations (`drives check`, `drives scrub`) that legitimately run for minutes.
///
/// These used a plain `request_with_reply`, which parks the shell in the syscall for the WHOLE operation:
/// it cannot poll the console, so `q` is never seen and the only way out is cutting the power - which is
/// exactly what happened on the Pi 2, whose FUA-per-write stick makes a full-tree fsck genuinely slow.
/// Conventions rule 9 (a blocking command stays q-abortable) is not optional for the longest commands in
/// the system; those are the ones that need it most. Sends exactly `[op]`, matching what fs expects here
/// (`fs_request_q` would append a path-length byte).
fn fs_op_q(ctx: &ShellCtx, op: u8) -> ReqOutcome {
    const HINT_SECS: i64 = 2;    // print "(q to quit)" only once the wait lingers
    const MAX_SECS:  i64 = FS_FSCK_SECS; // check/scrub walk the TREE, not the volume - a real bound
    let tag = next_fs_tag(ctx);
    let msg = Message::from_bytes(&[tag, op]);
    drain_stale_fs_replies(ctx);
    let first = ctx.request_with_reply_qhint("fs", &msg, HINT_SECS, MAX_SECS, || ctx.console_writeln("  (q to quit)"));
    match fs_take_tagged(ctx, tag, first, MAX_SECS) {
        ReqOutcome::Timeout if ctx.reacquire_by_name("fs") => {
            let tag2 = next_fs_tag(ctx);
            let msg2 = Message::from_bytes(&[tag2, op]);
            let again = ctx.request_with_reply_qhint("fs", &msg2, HINT_SECS, MAX_SECS, || ctx.console_writeln("  (q to quit)"));
            fs_take_tagged(ctx, tag2, again, MAX_SECS)
        }
        other => other,
    }
}

/// Stat a path: `Some((size, is_dir))` if it exists, `None` otherwise. Used by the streaming
/// read/copy paths to learn a file's size before chunking through it.
fn fs_stat(ctx: &ShellCtx, path: &[u8]) -> Option<(u64, bool)> {
    let reply = fs_request(ctx, OP_STAT_FILE, path, &[])?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 {
        Some((u64::from_le_bytes([p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9]]), p[10] == 1))
    } else {
        None
    }
}

/// Read up to `IO_CHUNK` bytes from `path` at byte `offset` into `out`; returns bytes read
/// (0 at EOF). One message - the building block for streaming a large file.
fn fs_read_at(ctx: &ShellCtx, path: &[u8], offset: u64, out: &mut [u8]) -> Option<usize> {
    let mut tail = [0u8; 12];
    tail[..8].copy_from_slice(&offset.to_le_bytes());
    tail[8..12].copy_from_slice(&(IO_CHUNK as u32).to_le_bytes());
    let reply = fs_request(ctx, OP_READ_AT, path, &tail)?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        let n = end - 5;
        out[..n].copy_from_slice(&p[5..end]);
        Some(n)
    } else {
        None
    }
}

/// Deadline-bounded twin of `fs_stat` for the startup history load: the reply wait is capped at
/// `max_secs` (RTC) via `fs_request_bounded`, so an alive-but-not-serving fs (respawned, still
/// re-mounting) cannot hang the prompt. `None` on timeout/miss/absent, treated as "no file".
fn fs_stat_bounded(ctx: &ShellCtx, path: &[u8], max_secs: i64) -> Option<(u64, bool)> {
    let reply = fs_request_bounded(ctx, OP_STAT_FILE, path, &[], max_secs)?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 {
        Some((u64::from_le_bytes([p[2], p[3], p[4], p[5], p[6], p[7], p[8], p[9]]), p[10] == 1))
    } else {
        None
    }
}

/// Deadline-bounded twin of `fs_read_at` for the startup history load - same per-chunk deadline
/// discipline as `fs_stat_bounded`, so a stalled fs times out instead of blocking the shell.
fn fs_read_at_bounded(ctx: &ShellCtx, path: &[u8], offset: u64, out: &mut [u8], max_secs: i64) -> Option<usize> {
    let mut tail = [0u8; 12];
    tail[..8].copy_from_slice(&offset.to_le_bytes());
    tail[8..12].copy_from_slice(&(IO_CHUNK as u32).to_le_bytes());
    let reply = fs_request_bounded(ctx, OP_READ_AT, path, &tail, max_secs)?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        let n = end - 5;
        out[..n].copy_from_slice(&p[5..end]);
        Some(n)
    } else {
        None
    }
}

/// Create/truncate `path` to hold `total` bytes (allocates the whole extent). Pairs with
/// `fs_write_at` to stream a large file.
fn fs_write_new(ctx: &ShellCtx, path: &[u8], total: u64) -> bool {
    matches!(fs_request(ctx, OP_WRITE_NEW, path, &total.to_le_bytes()),
             Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// Write `chunk` into `path` at block-aligned byte `offset`.
fn fs_write_at(ctx: &ShellCtx, path: &[u8], offset: u64, chunk: &[u8]) -> bool {
    let mut tail = [0u8; 8 + IO_CHUNK];
    tail[..8].copy_from_slice(&offset.to_le_bytes());
    let n = chunk.len().min(IO_CHUNK);
    tail[8..8 + n].copy_from_slice(&chunk[..n]);
    matches!(fs_request(ctx, OP_WRITE_AT, path, &tail[..8 + n]),
             Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// True if `fs` replied "no filesystem" - print the standard hint and consume it.
fn no_fs(ctx: &ServiceContext, p: &[u8]) -> bool {
    match p.first() {
        Some(&FS_NOFS) => {
            ctx.console_writeln("no filesystem - run 'drives flash' first");
            true
        }
        Some(&FS_UNAVAIL) => {
            // Present-but-unreadable disk: the data may still be intact, so flashing would DESTROY it.
            // Deliberately does NOT advise 'drives flash' (the whole point of the FS_UNAVAIL distinction).
            ctx.console_writeln("storage unavailable - do NOT run 'drives flash' (data may be intact; awaiting storage recovery)");
            true
        }
        _ => false,
    }
}

/// `ls [path]` - list a directory.
fn cmd_ls(ctx: &ShellCtx, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request_q(ctx, OP_LIST_DIR, path, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("ls: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_NOTFOUND) {
        ctx.console_writeln_fmt(format_args!("ls: not a directory: {}", str_of(path)));
        return Err(ShellError::FileNotFound);
    }
    // A short or error reply is NOT "not a directory". Lumping the two together is how a storage I/O
    // error - the stick pulled and replugged - came out as a claim about the path, sending the operator
    // to look at `/` when the problem was the device. Name what actually happened (§26.7).
    if p.first() == Some(&FS_ERR) || p.len() < 2 {
        ctx.console_writeln_fmt(format_args!(
            "ls: could not read {} - storage error (the device may still be settling after a replug; try again)",
            str_of(path)));
        return Err(ShellError::Unknown);
    }
    let count = p[1] as usize;
    out.line_fmt(ctx, format_args!("{}  ({} entries)", str_of(path), count));
    if count > 0 { out.line(ctx, "  NAME                  TYPE   SIZE"); }
    let mut i = 2usize;
    for _ in 0..count {
        if i >= p.len() { break; }
        let nl = p[i] as usize;
        i += 1;
        if i + nl + 1 + 8 > p.len() { break; }
        let name = core::str::from_utf8(&p[i..i + nl]).unwrap_or("?");
        let is_dir = p[i + nl] != 0;
        let size = u64_le(&p[i + nl + 1..i + nl + 9]);
        i += nl + 1 + 8;
        if is_dir {
            out.line_fmt(ctx, format_args!("  {:<20}  dir    -", name));
        } else {
            out.line_fmt(ctx, format_args!("  {:<20}  file   {} B", name, size));
        }
    }
    if count == 0 { out.line(ctx, "  (empty)"); }
    Ok(())
}

/// `read <path>` - print a file's contents. The first command on the Ok/Err `Result` model:
/// `Ok(())` when the file was read, `Err(FileNotFound)` when it does not exist, `Err(Unknown)`
/// for other failures (bad path, storage unavailable) until those get their own variants. The
/// human-readable detail is still printed; the `Result` is the category.
/// Open `path` via fs (`OP_OPEN`) and return the **file capability** the reply embeds, or `None`.
fn fc_open(ctx: &ShellCtx, path: &[u8], rights: u8) -> Option<CapHandle> {
    let r = fs_request(ctx, OP_OPEN, path, &[rights])?;
    if r.payload_bytes().first() == Some(&FS_OK) {
        let h = ctx.take_pending_cap();
        // FCAP-RESTART INSTRUMENTATION (temporary). What the shell BELIEVES it just got. Compare the
        // rights here against what fs minted for the same handle: if they disagree, the
        h
    } else { None }
}

/// Invoke a file cap (§7.10): the kernel validates `file` holds `right`, badges the request, and
/// routes it to fs; fs replies on our endpoint. `None` means the kernel rejected the invocation
/// (the cap lacks `right` - non-escalation - or is stale/revoked), so no reply comes back.
fn fc_invoke(ctx: &ServiceContext, file: CapHandle, right: u8, payload: &[u8]) -> Option<Message> {
    while ctx.try_recv().is_some() {}   // clear any stale late-reply a prior aborted invoke left behind
    let self_grant = ctx.self_grant_handle()?;
    let reply = ctx.derive_cap(self_grant)?;
    if ctx.resource_invoke(file, right, reply, &Message::from_bytes(payload)).is_err() {
        ctx.remove_cap(reply); // kernel didn't consume it (validation failed) - don't leak the slot
        return None;
    }
    // Await the reply FAILURE-AWARE (Commandment VIII): a bare `recv` here would hang forever if fs
    // died after receiving the badged invocation but before replying. Reclaim the reply slot on every
    // outcome (the reply cap is one-shot; Aborted/Timeout means it was never consumed).
    // Same rule as the SDK: on a REPLY the cap is already gone (the send embedded it, and §8.5 removes
    // an embedded cap from the sender's table), so removing it here removes whatever the kernel has
    // since placed in that slot - which is how the file cap was being deleted. Reclaim it only on the
    // paths where the send never delivered it.
    let outcome = ctx.recv_abortable_deadline(FILTER_WAIT_SECS);
    match outcome {
        ReqOutcome::Reply(m) => Some(m),
        _ => { ctx.remove_cap(reply); None }
    }
}

/// `fcap` - self-contained demonstration AND self-check of file-as-capability (§7.10). It is a
/// DIAGNOSTIC, not a file tool: it creates its own throwaway file, exercises every property the
/// capability model promises against it, then deletes it - so it never touches a file of yours
/// and takes no argument. Each line is asserted by `osdev test file-cap` (§22 Test 14).
const FCAP_TMP: &[u8] = b"/.fcap-selftest";
const FCAP_TMP_RENAMED: &[u8] = b"/.fcap-selftest.renamed";
fn cmd_fcap_help(ctx: &ServiceContext) {
    ctx.console_writeln("fcap - file-as-capability self-check (a diagnostic, not a file tool)");
    ctx.console_writeln("");
    ctx.console_writeln("usage: fcap          run the self-check");
    ctx.console_writeln("       fcap help     this message");
    ctx.console_writeln("");
    ctx.console_writeln("It creates its own throwaway file, opens it as a real kernel capability,");
    ctx.console_writeln("and verifies the file-cap model end to end (it then deletes the file):");
    ctx.console_writeln("  - read/write THROUGH the cap (a file IS a capability, not a handle to one)");
    ctx.console_writeln("  - non-escalation: a read-only cap cannot write (kernel AND fs both refuse)");
    ctx.console_writeln("  - unforgeable: a fabricated handle is rejected");
    ctx.console_writeln("  - revocable: the cap goes stale on close and on rename (no silent rebind)");
    ctx.console_writeln("It takes no path and never touches your files. See CLAUDE.md 7.10 / Test 14.");
}
fn cmd_fcap(ctx: &ShellCtx, arg: &str) -> Result<(), ShellError> {
    if arg.trim() == "help" { cmd_fcap_help(ctx); return Ok(()); }
    if !arg.trim().is_empty() {
        ctx.console_writeln("fcap: takes no argument (it uses its own throwaway file). Try `fcap help`.");
        return Err(ShellError::Unknown);
    }
    let path = FCAP_TMP;
    let mut ok = true;
    let fail = |ctx: &ServiceContext, m: &str| { ctx.console_writeln(m); };

    // 0. Create our own throwaway file so we never touch a user's file. Seed it with >=7 bytes so
    //    the 7-byte "capdata" write-through-cap below fits the allocated extent (file-cap writes
    //    don't grow the file). Overwrites a stale one from an aborted run; deleted again at the end.
    if !matches!(fs_request(ctx, OP_WRITE_FILE, path, b"seeddata").as_ref().map(|r| r.payload_bytes().first().copied()),
                 Some(Some(FS_OK))) {
        ctx.console_writeln("fcap: FAIL create temp file (storage unavailable?)");
        return Err(ShellError::Unknown);
    }

    // 1. Open the file as a capability (fs mints a delegated resource + hands us the cap).
    let rw = match fc_open(ctx, path, RIGHT_READ | RIGHT_WRITE) {
        Some(c) => { ctx.console_writeln("fcap: opened rw (file cap)"); c }
        None    => { ctx.console_writeln("fcap: FAIL open rw"); let _ = fs_request(ctx, OP_DELETE, path, &[]); return Err(ShellError::Unknown); }
    };

    // 2. Write THROUGH the cap (FOP_WRITE needs WRITE, which rw holds).
    let mut wbuf = [0u8; 1 + 8 + 7];
    wbuf[0] = FOP_WRITE; // offset 0 (bytes 1..9 already zero); payload "capdata"
    wbuf[9..16].copy_from_slice(b"capdata");
    match fc_invoke(ctx, rw, RIGHT_WRITE, &wbuf) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => ctx.console_writeln("fcap: write via cap OK"),
        _ => { fail(ctx, "fcap: FAIL write via cap"); ok = false; }
    }

    // 3. Read it back THROUGH the cap.
    let mut rbuf = [0u8; 1 + 8 + 4];
    rbuf[0] = FOP_READ;
    rbuf[9..13].copy_from_slice(&7u32.to_le_bytes());
    match fc_invoke(ctx, rw, RIGHT_READ, &rbuf) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) && r.payload_bytes().len() >= 12
            && &r.payload_bytes()[5..12] == b"capdata" => ctx.console_writeln("fcap: read via cap OK"),
        _ => { fail(ctx, "fcap: FAIL read via cap"); ok = false; }
    }

    // 4. Open a READ-ONLY cap to the same file.
    let ro = match fc_open(ctx, path, RIGHT_READ) {
        Some(c) => c,
        None    => { fail(ctx, "fcap: FAIL open ro"); ctx.remove_cap(rw); let _ = fs_request(ctx, OP_DELETE, path, &[]); return Err(ShellError::Unknown); }
    };

    // 5. Non-escalation, kernel layer: invoking the RO cap declaring WRITE is rejected by the
    //    KERNEL (the cap lacks WRITE → CapInsufficientRights), so no reply comes back.
    match fc_invoke(ctx, ro, RIGHT_WRITE, &wbuf) {
        None    => ctx.console_writeln("fcap: ro-cap write rejected by kernel (non-escalation)"),
        Some(_) => { fail(ctx, "fcap: FAIL ro cap wrote (escalation!)"); ok = false; }
    }

    // 6. Non-escalation, fs layer: declare READ (kernel passes) but send a WRITE op - fs refuses
    //    because the op needs more than the badged right (op ≤ right, FS_DENIED).
    match fc_invoke(ctx, ro, RIGHT_READ, &wbuf) {
        Some(r) if r.payload_bytes().first() == Some(&FS_DENIED) => ctx.console_writeln("fcap: fs refused write under read right (op<=right)"),
        _ => { fail(ctx, "fcap: FAIL fs allowed write under read right"); ok = false; }
    }

    // 7. Unforgeable: a fabricated handle is not a capability.
    // 4000, not 60000, so THE KERNEL does the rejecting.
    //
    // 60000 does not fit the 12-bit slot field, and the SDK now rejects it at the wrapper before any
    // syscall happens. This check would then have passed without the kernel ever being asked - a test
    // that proves the SDK's range check rather than the unforgeability it claims to prove, which is
    // precisely the vacuous pass that let the fcap failure hide for so long.
    //
    // 4000 is a legal slot number that this task does not hold, so the request really is made and the
    // kernel really refuses it. The out-of-range case is the SDK's own concern and is not what this
    // line is for.
    match fc_invoke(ctx, CapHandle(4000), RIGHT_READ, &rbuf) {
        None    => ctx.console_writeln("fcap: forged handle rejected"),
        Some(_) => { fail(ctx, "fcap: FAIL forged handle accepted"); ok = false; }
    }

    // 8. Revocable: close the rw cap (fs revokes the resource), then a further use is stale.
    let _ = fc_invoke(ctx, rw, RIGHT_READ, &[FOP_CLOSE]);
    match fc_invoke(ctx, rw, RIGHT_READ, &rbuf) {
        None    => ctx.console_writeln("fcap: cap revoked after close"),
        Some(_) => { fail(ctx, "fcap: FAIL cap usable after close"); ok = false; }
    }

    // 9. Revocable on path rebinding (confused-deputy avoidance, §7.10): renaming the file makes
    //    the old path name something else, so fs revokes the still-open `ro` cap - it can never
    //    silently rebind to a different file later created at the old path.
    let _ = fs_request(ctx, OP_RENAME, path, b".fcap-selftest.renamed");
    match fc_invoke(ctx, ro, RIGHT_READ, &rbuf) {
        None    => ctx.console_writeln("fcap: cap revoked after rename"),
        Some(_) => { fail(ctx, "fcap: FAIL cap usable after rename"); ok = false; }
    }

    // Cleanup so `fcap` is leak-free and re-runnable (e.g. in selfcheck): drop both shell handles
    // (rw revoked at close, ro revoked at rename) and delete the throwaway file (now at the renamed
    // path). Otherwise each run orphans cap-table slots and leaves a stray file behind.
    ctx.remove_cap(ro);
    ctx.remove_cap(rw);
    let _ = fs_request(ctx, OP_DELETE, FCAP_TMP_RENAMED, &[]);

    if ok { ctx.console_writeln("fcap: all file-capability checks passed"); Ok(()) }
    else { Err(ShellError::Unknown) }
}

// ── edit: a full-screen text editor (utilities/36_edit.md) ───────────────────────────────────
//
// A modeless full-screen editor (title bar, text area, bottom status bar), modelled after
// Microsoft's `edit`. Files of ANY size are editable: this is a **bounded piece table** (no heap,
// §26.6). The original file stays on disk and is read in IO_CHUNK windows (`fs_read_at`) as you
// scroll - only the visible window is ever materialised (the "scroll millions of lines" property).
// Edits never touch the original: typed bytes go into a fixed `add` buffer, and the document is an
// ordered list of `Piece` spans into either the original file or the add buffer. Save streams the
// spans out to a temp file and atomically replaces the original, then RESETS the add buffer + span
// list - so the only bound is how much you edit *between saves*, not the file size. When the add
// buffer or span list fills, the edit is refused loudly (the status bar says to save), never
// silently dropped (§26.7). Rendering uses only the CSI subset the serial terminal AND the `console`
// service support: cursor position, erase-to-EOL, show/hide; reverse-video bars are SGR (pretty on
// serial, plain on a terminal that lacks it - the unsupported escape is ignored, never left as garbage).
const EDIT_COLS_MAX: usize = 256;          // bar-render scratch width cap; also caps render cols
const EDIT_TAB: usize = 4;                 // Tab inserts this many spaces
const EDIT_ADD_MAX: usize = 32 * 1024;     // add-buffer: new typed bytes between saves (save resets)
const EDIT_MAX_PIECES: usize = 1024;       // span-list size (save resets); each edit adds ≤2 spans
const EDIT_LINE_MAX: usize = 8192;         // bound on a single line's length for nav/render scans
const EDIT_TMP: &[u8] = b"/.edit.tmp";     // save staging file (root → no dirname math)

/// One span of the document: `len` bytes starting at `start` in either the original file on disk
/// (`add == false`) or the in-memory add buffer (`add == true`). The document is the ordered
/// concatenation of all live pieces. Edits never modify the original - they append typed bytes to
/// `add` and rewrite the span list.
#[derive(Clone, Copy)]
struct Piece { add: bool, start: u32, len: u32 }

/// A bounded piece-table editor. The original file stays on disk and is read in IO_CHUNK windows
/// (`cache`) on demand; typed bytes accumulate in `add`; the document is `pieces[..npieces]`.
/// Cursor/scroll are logical byte offsets into the document. Fixed-size - no heap (§26.6); when
/// `add` or the span list fills, the edit is refused (`full = true`) and the status bar says so
/// rather than silently dropping it (§26.7). A save streams the spans to disk and RESETS `add` +
/// the span list, so the only bound is how much is edited *between* saves, not the file size.
struct Editor {
    pieces:    [Piece; EDIT_MAX_PIECES],
    npieces:   usize,
    add:       [u8; EDIT_ADD_MAX],
    add_len:   usize,
    total:     usize,             // logical document length (maintained)
    path:      [u8; PATH_MAX],    // the file on disk (read source for Orig spans; save target)
    path_len:  usize,
    cache:     [u8; IO_CHUNK],    // one IO_CHUNK-aligned window of the original file
    cache_off: usize,             // original-file offset the window starts at
    cache_len: usize,             // valid bytes in `cache` (0 = empty/miss)
    cur:       usize,             // cursor, logical offset 0..=total
    top:       usize,             // first visible line, a logical offset at a line start
    left:      usize,             // horizontal scroll (column)
    rows:      usize,
    cols:      usize,
    modified:  bool,
    full:      bool,              // a recent edit was refused (add/pieces full) - drives the hint
}

impl Editor {
    fn new(rows: usize, cols: usize, orig_size: usize) -> Self {
        let mut ed = Editor {
            pieces: [Piece { add: false, start: 0, len: 0 }; EDIT_MAX_PIECES],
            npieces: 0,
            add: [0u8; EDIT_ADD_MAX],
            add_len: 0,
            total: 0,
            path: [0u8; PATH_MAX],
            path_len: 0,
            cache: [0u8; IO_CHUNK],
            cache_off: 0,
            cache_len: 0,
            cur: 0, top: 0, left: 0, rows, cols, modified: false, full: false,
        };
        if orig_size > 0 {
            ed.pieces[0] = Piece { add: false, start: 0, len: orig_size as u32 };
            ed.npieces = 1;
            ed.total = orig_size;
        }
        ed
    }

    /// Find the piece containing logical offset `pos`. Returns `(piece_index, offset_in_piece)`.
    /// For `pos == total` (end of document) returns `(npieces, 0)`.
    fn locate(&self, pos: usize) -> (usize, usize) {
        let mut acc = 0usize;
        for i in 0..self.npieces {
            let plen = self.pieces[i].len as usize;
            if pos < acc + plen { return (i, pos - acc); }
            acc += plen;
        }
        (self.npieces, 0)
    }

    /// Refill the window cache with the IO_CHUNK-aligned window of the original file containing
    /// original-file offset `abs`. On a read failure leaves `cache_len = 0`.
    fn refill(&mut self, ctx: &ShellCtx, abs: usize) {
        let win = (abs / IO_CHUNK) * IO_CHUNK;
        let mut pbuf = [0u8; PATH_MAX];
        let pl = self.path_len;
        pbuf[..pl].copy_from_slice(&self.path[..pl]);
        match fs_read_at(ctx, &pbuf[..pl], win as u64, &mut self.cache) {
            Some(n) => { self.cache_off = win; self.cache_len = n; }
            None    => { self.cache_len = 0; }
        }
    }

    /// Copy up to `n` logical bytes starting at document offset `logical` into `out`; returns the
    /// number actually copied (fewer than `n` only at end-of-document or on a read failure).
    /// Add-piece bytes come from memory; original-piece bytes come through the window cache.
    fn read_span(&mut self, ctx: &ShellCtx, logical: usize, n: usize, out: &mut [u8]) -> usize {
        let want = n.min(out.len());
        let mut produced = 0usize;
        let (mut pi, mut off) = self.locate(logical);
        while produced < want && pi < self.npieces {
            let p = self.pieces[pi];
            let avail = p.len as usize - off;
            let take = avail.min(want - produced);
            if p.add {
                let s = p.start as usize + off;
                out[produced..produced + take].copy_from_slice(&self.add[s..s + take]);
                produced += take;
            } else {
                let mut copied = 0usize;
                while copied < take {
                    let abs = p.start as usize + off + copied; // original-file offset
                    let hit = self.cache_len > 0 && abs >= self.cache_off && abs < self.cache_off + self.cache_len;
                    if !hit {
                        self.refill(ctx, abs);
                        if self.cache_len == 0 { break; }      // read failed
                    }
                    let in_win = abs - self.cache_off;
                    let m = (self.cache_len - in_win).min(take - copied);
                    out[produced + copied..produced + copied + m].copy_from_slice(&self.cache[in_win..in_win + m]);
                    copied += m;
                }
                produced += copied;
                if copied < take { break; }
            }
            pi += 1;
            off = 0;
        }
        produced
    }

    fn byte_at(&mut self, ctx: &ShellCtx, pos: usize) -> Option<u8> {
        if pos >= self.total { return None; }
        let mut b = [0u8; 1];
        if self.read_span(ctx, pos, 1, &mut b) == 1 { Some(b[0]) } else { None }
    }

    /// Insert `piece` at logical offset `logical`, splitting an existing piece if `logical` lands
    /// mid-piece. Returns false (changing nothing) if the span list has no room.
    fn insert_piece_at(&mut self, logical: usize, piece: Piece) -> bool {
        let (pi, off) = self.locate(logical);
        if off == 0 {
            if self.npieces + 1 > EDIT_MAX_PIECES { return false; }
            let mut i = self.npieces;
            while i > pi { self.pieces[i] = self.pieces[i - 1]; i -= 1; }
            self.pieces[pi] = piece;
            self.npieces += 1;
        } else {
            if self.npieces + 2 > EDIT_MAX_PIECES { return false; }
            let orig = self.pieces[pi];
            let left  = Piece { add: orig.add, start: orig.start, len: off as u32 };
            let right = Piece { add: orig.add, start: orig.start + off as u32, len: orig.len - off as u32 };
            let mut i = self.npieces + 1;
            while i > pi + 2 { self.pieces[i] = self.pieces[i - 2]; i -= 1; }
            self.pieces[pi]     = left;
            self.pieces[pi + 1] = piece;
            self.pieces[pi + 2] = right;
            self.npieces += 2;
        }
        true
    }

    fn insert(&mut self, b: u8) {
        if self.add_len >= EDIT_ADD_MAX { self.full = true; return; }
        let idx = self.add_len as u32;
        // Coalesce consecutive typing: if the piece just left of the cursor is an add-piece ending
        // exactly at the next add slot, extend it in place rather than minting a new span.
        let (pi, off) = self.locate(self.cur);
        if off == 0 && pi >= 1 {
            let prev = self.pieces[pi - 1];
            if prev.add && prev.start + prev.len == idx {
                self.add[self.add_len] = b;
                self.add_len += 1;
                self.pieces[pi - 1].len += 1;
                self.cur += 1; self.total += 1; self.modified = true; self.full = false;
                return;
            }
        }
        let piece = Piece { add: true, start: idx, len: 1 };
        if self.insert_piece_at(self.cur, piece) {
            self.add[self.add_len] = b;
            self.add_len += 1;
            self.cur += 1; self.total += 1; self.modified = true; self.full = false;
        } else {
            self.full = true;
        }
    }

    /// Remove one logical byte at offset `pos` (shrink or split the covering piece).
    fn remove_at(&mut self, pos: usize) {
        if pos >= self.total { return; }
        let (pi, off) = self.locate(pos);
        if pi >= self.npieces { return; }
        let p = self.pieces[pi];
        if p.len == 1 {
            let mut i = pi;
            while i + 1 < self.npieces { self.pieces[i] = self.pieces[i + 1]; i += 1; }
            self.npieces -= 1;
        } else if off == 0 {
            self.pieces[pi].start += 1;
            self.pieces[pi].len   -= 1;
        } else if off == p.len as usize - 1 {
            self.pieces[pi].len -= 1;
        } else {
            // split out the middle byte: left [0..off] | right [off+1..len]
            if self.npieces + 1 > EDIT_MAX_PIECES { self.full = true; return; }
            let right = Piece { add: p.add, start: p.start + off as u32 + 1, len: p.len - off as u32 - 1 };
            self.pieces[pi].len = off as u32;
            let mut i = self.npieces;
            while i > pi + 1 { self.pieces[i] = self.pieces[i - 1]; i -= 1; }
            self.pieces[pi + 1] = right;
            self.npieces += 1;
        }
        self.total -= 1; self.modified = true; self.full = false;
    }

    fn delete(&mut self)    { let c = self.cur; self.remove_at(c); }
    fn backspace(&mut self) { if self.cur > 0 { self.cur -= 1; let c = self.cur; self.remove_at(c); } }

    fn move_left(&mut self)  { if self.cur > 0 { self.cur -= 1; } }
    fn move_right(&mut self) { if self.cur < self.total { self.cur += 1; } }

    /// Logical offset of the start of the line containing `pos` (just after the previous '\n', or
    /// 0). Bounded by EDIT_LINE_MAX - a longer line falls back to that many bytes back.
    fn line_start(&mut self, ctx: &ShellCtx, pos: usize) -> usize {
        let mut i = pos;
        let mut steps = 0;
        while i > 0 && steps < EDIT_LINE_MAX {
            if self.byte_at(ctx, i - 1) == Some(b'\n') { return i; }
            i -= 1; steps += 1;
        }
        i
    }
    /// Logical offset of the '\n' ending the line containing `pos`, or `total` for the last line.
    fn line_end(&mut self, ctx: &ShellCtx, pos: usize) -> usize {
        let mut i = pos;
        let mut steps = 0;
        while i < self.total && steps < EDIT_LINE_MAX {
            if self.byte_at(ctx, i) == Some(b'\n') { return i; }
            i += 1; steps += 1;
        }
        i
    }
    /// Count of '\n' bytes in `[from, to)` - the number of line breaks between two offsets.
    fn lines_between(&mut self, ctx: &ShellCtx, from: usize, to: usize) -> usize {
        let mut n = 0; let mut i = from;
        while i < to { if self.byte_at(ctx, i) == Some(b'\n') { n += 1; } i += 1; }
        n
    }
    /// Advance `pos` forward by `k` line starts (stops at end-of-document).
    fn advance_lines(&mut self, ctx: &ShellCtx, mut pos: usize, k: usize) -> usize {
        for _ in 0..k {
            let le = self.line_end(ctx, pos);
            if le >= self.total { return pos; }
            pos = le + 1;
        }
        pos
    }

    fn move_home(&mut self, ctx: &ShellCtx) { let c = self.cur; self.cur = self.line_start(ctx, c); }
    fn move_end(&mut self, ctx: &ShellCtx)  { let c = self.cur; self.cur = self.line_end(ctx, c); }
    fn move_up(&mut self, ctx: &ShellCtx) {
        let c = self.cur;
        let ls = self.line_start(ctx, c);
        if ls == 0 { self.cur = 0; return; }
        let col = c - ls;
        let pls = self.line_start(ctx, ls - 1); // previous line's start
        let plen = (ls - 1) - pls;              // previous line length (excluding its '\n')
        self.cur = pls + col.min(plen);
    }
    fn move_down(&mut self, ctx: &ShellCtx) {
        let c = self.cur;
        let le = self.line_end(ctx, c);
        if le >= self.total { self.cur = self.total; return; }
        let ls = self.line_start(ctx, c);
        let col = c - ls;
        let nls = le + 1;                       // next line's start
        let nlen = self.line_end(ctx, nls) - nls;
        self.cur = nls + col.min(nlen);
    }
    fn page(&mut self, ctx: &ShellCtx, down: bool) {
        for _ in 0..self.rows.saturating_sub(3).max(1) {
            if down { self.move_down(ctx) } else { self.move_up(ctx) }
        }
    }
}

/// A bounded `fmt::Write` sink over a stack slice - used to format a status/title bar string
/// before padding it to the bar width. Drops anything past the slice (the bar is clipped anyway).
struct BarW<'a> { b: &'a mut [u8], n: usize }
impl core::fmt::Write for BarW<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &c in s.as_bytes() { if self.n < self.b.len() { self.b[self.n] = c; self.n += 1; } else { break; } }
        Ok(())
    }
}

fn edit_goto(ctx: &ServiceContext, row: usize, col: usize) {
    ctx.console_write_fmt(format_args!("\x1b[{};{}H", row, col));
}

/// Draw a full-width reverse-video bar: `text` (already formatted) left-justified, space-padded
/// to `width`. The caller positions the cursor first. `\x1b[7m`/`\x1b[0m` are reverse-video on a
/// serial terminal AND on the framebuffer console - the `console` SERVICE renders SGR 7 by inverting
/// the glyph blend ramp, so the bar looks the same on the TV as in a terminal. (It used to be dropped
/// on x86 and rendered only on ARM; there is one terminal now, and it is a service.)
fn edit_bar(ctx: &ServiceContext, text: &[u8], width: usize) {
    let mut line = [b' '; EDIT_COLS_MAX];
    let w = width.min(EDIT_COLS_MAX);
    let n = text.len().min(w);
    line[..n].copy_from_slice(&text[..n]);
    ctx.console_write("\x1b[7m");
    ctx.console_write(str_of(&line[..w]));
    ctx.console_write("\x1b[0m");
}

/// Repaint the whole screen for `ed`. Adjusts scroll so the cursor stays visible (a line at a
/// time, scanning only across the viewport - never the whole file), draws the title bar, the
/// visible text rows materialised from the piece table through the window cache, the status bar,
/// then parks the terminal cursor. Only the visible window is ever read - the iOS-scroll property.
fn edit_render(ctx: &ShellCtx, ed: &mut Editor, name: &[u8]) {
    use core::fmt::Write as _;
    let textrows = ed.rows.saturating_sub(2).max(1); // rows between the title and status bars
    let cols = ed.cols;

    // Vertical scroll: keep the cursor's line within [top, top+textrows). `top` stays a line start.
    let cur = ed.cur;
    let cls = ed.line_start(ctx, cur);
    let col = cur - cls;
    if cls < ed.top {
        ed.top = cls;
    } else {
        let n = ed.lines_between(ctx, ed.top, cls);
        if n >= textrows { ed.top = ed.advance_lines(ctx, ed.top, n - textrows + 1); }
    }
    // Horizontal scroll.
    if col < ed.left { ed.left = col; }
    if col >= ed.left + cols { ed.left = col + 1 - cols; }
    // The cursor's screen row, now guaranteed < textrows.
    let crow = ed.lines_between(ctx, ed.top, cls);

    ctx.console_write("\x1b[?25l"); // hide cursor while repainting (no flicker trail)

    // Title bar (row 1): name + a dirty marker. Full width (row-1 wrap is harmless).
    edit_goto(ctx, 1, 1);
    {
        let mut t = [0u8; EDIT_COLS_MAX];
        let mut w = BarW { b: &mut t, n: 0 };
        let _ = write!(w, " edit  {}{}", str_of(name), if ed.modified { "  * (modified)" } else { "" });
        let used = w.n;
        edit_bar(ctx, &t[..used], cols);
    }

    // Text rows (screen rows 2..=rows-1): one document line each, starting at `top`. Each line's
    // visible slice [left, left+cols) is read from the piece table into a row scratch.
    let mut ls = ed.top;
    for r in 0..textrows {
        edit_goto(ctx, 2 + r, 1);
        if ls <= ed.total {
            let le = ed.line_end(ctx, ls);
            let lstart = ls + ed.left;
            if lstart < le {
                let n = (le - lstart).min(cols).min(EDIT_COLS_MAX);
                let mut row = [0u8; EDIT_COLS_MAX];
                let got = ed.read_span(ctx, lstart, n, &mut row);
                ctx.console_write(str_of(&row[..got]));
            }
            ctx.console_write("\x1b[K"); // erase the rest of the row (no SGR → no wrap)
            ls = le + 1;
        } else {
            ctx.console_write("\x1b[K"); // past end of document → blank row
        }
    }

    // Status bar (last row): key hints + position. One cell short of full width so writing it on
    // the bottom row can never trigger an auto-wrap that scrolls the screen. No absolute "Ln" -
    // that would need a scan from offset 0 (O(file)); Col + byte offset are O(viewport).
    edit_goto(ctx, ed.rows, 1);
    {
        let mut t = [0u8; EDIT_COLS_MAX];
        let mut w = BarW { b: &mut t, n: 0 };
        if ed.full {
            let _ = write!(w, " edit buffer full - Ctrl-S to save & continue    Col {}   {} bytes",
                col + 1, ed.total);
        } else {
            let _ = write!(w, " Ctrl-S save   Ctrl-Q quit      Col {}   {} bytes   (buf {}/{})",
                col + 1, ed.total, ed.add_len, EDIT_ADD_MAX);
        }
        let used = w.n;
        edit_bar(ctx, &t[..used], cols.saturating_sub(1));
    }

    // Park the editing cursor (title is row 1, so the cursor line is screen row 2 + crow).
    edit_goto(ctx, 2 + crow, 1 + (col - ed.left));
    ctx.console_write("\x1b[?25h"); // show it
}

/// Save the document by streaming the piece spans to a temp file and atomically replacing the
/// target, then RESET the add buffer + span list (the per-session edit budget). Returns false on
/// any I/O failure, leaving `modified` set so the quit prompt still protects unsaved work.
fn edit_save(ctx: &ShellCtx, ed: &mut Editor) -> bool {
    let mut pbuf = [0u8; PATH_MAX];
    let pl = ed.path_len;
    pbuf[..pl].copy_from_slice(&ed.path[..pl]);
    let path = &pbuf[..pl];
    let total = ed.total;

    if total == 0 {
        // Empty document → write an empty file directly (one message).
        if !matches!(fs_request(ctx, OP_WRITE_FILE, path, &[])
            .as_ref().map(|r| r.payload_bytes().first().copied()), Some(Some(FS_OK))) {
            return false;
        }
    } else {
        if !fs_write_new(ctx, EDIT_TMP, total as u64) { return false; }
        let mut off = 0usize;
        let mut chunk = [0u8; IO_CHUNK];
        while off < total {
            let n = (total - off).min(IO_CHUNK);
            let got = ed.read_span(ctx, off, n, &mut chunk);
            if got == 0 { return false; } // short read of the source → fail loudly, keep `modified`
            if !fs_write_at(ctx, EDIT_TMP, off as u64, &chunk[..got]) { return false; }
            off += got;
        }
        // Atomic-ish replace: delete the target (ignore "not found" on a first save), move temp in.
        let _ = fs_request(ctx, OP_DELETE, path, &[]);
        let moved = matches!(fs_request(ctx, OP_MOVE, EDIT_TMP, path)
            .as_ref().map(|r| r.payload_bytes().first().copied()), Some(Some(FS_OK)));
        if !moved { return false; }
    }

    // Reset the budget: the saved file is now the original; one Orig span over it, add buffer empty.
    ed.npieces = 0;
    if total > 0 {
        ed.pieces[0] = Piece { add: false, start: 0, len: total as u32 };
        ed.npieces = 1;
    }
    ed.add_len = 0;
    ed.cache_len = 0;   // the on-disk file changed - invalidate the window
    ed.full = false;
    ed.modified = false;
    true
}

/// Decode a CSI sequence (after `ESC [`) into an editor cursor/edit action. Mirrors the shell's
/// line-editor `handle_csi`, but the actions move the document cursor instead of the prompt.
fn edit_csi(ctx: &ShellCtx, ed: &mut Editor) {
    let mut param: u16 = 0;
    let mut fb = 0u8;
    for _ in 0..8 {
        let c = ctx.console_read();
        if c.is_ascii_digit() { param = param.saturating_mul(10).saturating_add((c - b'0') as u16); }
        else if c == b';' { continue; }
        else { fb = c; break; }
    }
    match fb {
        b'A' => ed.move_up(ctx),
        b'B' => ed.move_down(ctx),
        b'C' => ed.move_right(),
        b'D' => ed.move_left(),
        b'H' => ed.move_home(ctx),
        b'F' => ed.move_end(ctx),
        b'~' => match param {
            1 | 7 => ed.move_home(ctx),
            4 | 8 => ed.move_end(ctx),
            3     => ed.delete(),          // forward Delete
            5     => ed.page(ctx, false),  // PageUp
            6     => ed.page(ctx, true),   // PageDown
            _ => {}
        },
        _ => {}
    }
}

/// Quit handler: clean if unsaved changes are handled. Returns `true` if the editor should exit.
/// With no unsaved changes, quits immediately; otherwise prompts on the status row (y = save then
/// quit, n = discard and quit, anything else = cancel and keep editing).
fn edit_try_quit(ctx: &ShellCtx, ed: &mut Editor) -> bool {
    if !ed.modified { return true; }
    edit_goto(ctx, ed.rows, 1);
    edit_bar(ctx, b" unsaved changes  -  y = save & quit,  n = discard & quit,  any other key = keep editing",
        ed.cols.saturating_sub(1));
    edit_goto(ctx, ed.rows, 1);
    match ctx.console_read() {
        b'y' | b'Y' => edit_save(ctx, ed),               // quit only if the save succeeds
        b'n' | b'N' => true,                              // discard and quit
        0x1B => { let _ = read_escape_byte(ctx); false }  // Esc (drain any sequence) → cancel
        _ => false,                                       // anything else → keep editing
    }
}

#[inline(never)] // big stack frame (the piece table + add buffer) - keep it off hot call paths
fn cmd_edit(ctx: &ShellCtx, cwd: &Cwd, arg: &str) -> Result<(), ShellError> {
    let arg = arg.trim();
    if arg.is_empty() {
        ctx.console_writeln("usage: edit <path>     e.g. edit /notes.txt");
        return Err(ShellError::Unknown);
    }
    let mut pbuf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut pbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut pcopy = [0u8; PATH_MAX];
    let pl = path.len();
    pcopy[..pl].copy_from_slice(path);
    let path = &pcopy[..pl];

    // Stat first (existence / kind / size). A directory is refused; a missing file opens empty
    // (created on first save); a file of ANY size opens - it's read in windows, never up front.
    let mut orig_size = 0usize;
    if let Some(stat) = fs_request(ctx, OP_STAT_FILE, path, &[]) {
        let sp = stat.payload_bytes();
        if no_fs(ctx, sp) { return Err(ShellError::Unknown); }
        let exists = sp.first() == Some(&FS_OK) && sp.len() >= 11 && sp[1] == 1;
        if exists {
            if sp[10] == 1 {
                ctx.console_writeln_fmt(format_args!("edit: {} is a directory", str_of(path)));
                return Err(ShellError::Unknown);
            }
            orig_size = u64::from_le_bytes([sp[2], sp[3], sp[4], sp[5], sp[6], sp[7], sp[8], sp[9]]) as usize;
        }
    } else {
        ctx.console_writeln("edit: storage unavailable");
        return Err(ShellError::Unknown);
    }

    let (rd, cd) = ctx.console_dims();
    let rows = if rd == 0 { 24 } else { rd as usize };
    let cols = if cd == 0 { 80 } else { cd as usize };
    let mut ed = Editor::new(rows, cols, orig_size);
    ed.path_len = pl;
    ed.path[..pl].copy_from_slice(path);

    let name = basename(path); // borrows pcopy, independent of `ed`
    ctx.console_write("\x1b[2J"); // clear the screen once on entry (every later frame repaints)

    loop {
        edit_render(ctx, &mut ed, name);
        match ctx.console_read() {
            0x13 => { let _ = edit_save(ctx, &mut ed); }                        // ^S (resets on success)
            0x11 => { if edit_try_quit(ctx, &mut ed) { break; } }              // ^Q
            0x1B => match read_escape_byte(ctx) {
                None        => { if edit_try_quit(ctx, &mut ed) { break; } }   // bare Esc → quit
                Some(b'[')  => edit_csi(ctx, &mut ed),
                Some(b'O')  => { let _ = ctx.console_read(); }                  // F-keys: consume, ignore
                Some(_)     => {}
            },
            b'\r' | b'\n' => ed.insert(b'\n'),
            0x7f | 0x08   => ed.backspace(),
            0x09          => { for _ in 0..EDIT_TAB { ed.insert(b' '); } }      // Tab → spaces
            b if (0x20..0x7f).contains(&b) => ed.insert(b),
            _ => {}
        }
    }

    // Restore the screen for the shell prompt: show the cursor and clear+home so `gsh> ` lands
    // cleanly at the top-left. Echo is already off (the shell owns it), so we leave it.
    ctx.console_write("\x1b[?25h\x1b[2J\x1b[H");
    Ok(())
}

fn cmd_read(ctx: &ShellCtx, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    // Stat first (one message) to learn the size, then STREAM the content in IO_CHUNK pieces
    // via read_at - so a file far larger than one IPC message reads back correctly without a
    // big buffer here.
    let stat = match fs_request_q(ctx, OP_STAT_FILE, path, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("read: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let sp = stat.payload_bytes();
    if no_fs(ctx, sp) { return Err(ShellError::Unknown); }
    let exists = sp.first() == Some(&FS_OK) && sp.len() >= 11 && sp[1] == 1;
    let is_dir = exists && sp[10] == 1;
    if !exists || is_dir {
        ctx.console_writeln_fmt(format_args!("read: not found: {}", str_of(path)));
        return Err(ShellError::FileNotFound);
    }
    let size = u64::from_le_bytes([sp[2], sp[3], sp[4], sp[5], sp[6], sp[7], sp[8], sp[9]]);
    let mut chunk = [0u8; IO_CHUNK];
    let mut off = 0u64;
    let mut last = b'\n';
    while off < size {
        let n = match fs_read_at(ctx, path, off, &mut chunk) {
            Some(n) if n > 0 => n,
            _ => { ctx.console_writeln("read: storage error"); return Err(ShellError::Unknown); }
        };
        out.put_bytes(ctx, &chunk[..n]);
        last = chunk[n - 1];
        off += n as u64;
    }
    if size == 0 || last != b'\n' { out.put(ctx, "\n"); }
    Ok(())
}

/// `write <path> [content]` overwrites; `write append|prepend <path> [content]` adds to the end /
/// front (creating the file if missing). `append`/`prepend` are *leading* keywords because write's
/// content is free-form - they can't trail the way `mkdir … parents` does (it would be swallowed as
/// content). Append/prepend stream through a temp file (`fs_stream_combine`), so they are not bound
/// by a small buffer; `prepend` is a full-file rewrite (no insert-at-front - honest, §26.7).
fn cmd_write(ctx: &ShellCtx, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
    let (mode, rest) = parse_write_mode(rest);
    if rest.is_empty() {
        ctx.console_writeln("usage: write [append|prepend] <path> [content]");
        return Err(ShellError::Unknown);
    }
    // Split off the first token (path); the remainder (with spaces) is the content. A
    // surrounding quote pair around the content is stripped (`write /f "two words"`).
    let (pstr, content) = match rest.split_once(char::is_whitespace) {
        Some((p, c)) => (p, strip_quotes(c.trim_start())),
        None => (rest, ""),
    };
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pstr, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    // Copy the path out before reusing buffers (path borrows `buf`).
    let mut pbuf = [0u8; PATH_MAX];
    let pl = path.len();
    pbuf[..pl].copy_from_slice(path);
    let p = &pbuf[..pl];
    if mode != WriteMode::Overwrite {
        let prepend = mode == WriteMode::Prepend;
        if fs_stream_combine(ctx, p, content.as_bytes(), prepend) {
            ctx.console_writeln_fmt(format_args!(
                "{} {} bytes to {}", if prepend { "prepended" } else { "appended" }, content.len(), str_of(p)));
            return Ok(());
        }
        ctx.console_writeln_fmt(format_args!(
            "write: {} failed (storage, or bad path?)", if prepend { "prepend" } else { "append" }));
        return Err(ShellError::Unknown);
    }
    let reply = match fs_request(ctx, OP_WRITE_FILE, p, content.as_bytes()) {
        Some(r) => r,
        None => { ctx.console_writeln("write: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let rp = reply.payload_bytes();
    if no_fs(ctx, rp) { return Err(ShellError::Unknown); }
    if rp.first() == Some(&FS_OK) {
        ctx.console_writeln_fmt(format_args!("wrote {} ({} bytes)", str_of(p), content.len()));
        Ok(())
    } else {
        ctx.console_writeln("write: failed (bad path, or parent missing?)");
        Err(ShellError::Unknown)
    }
}

// fmt's write / compare chunk buffer. MUST be a multiple of the fs payload block (DATA_PAYLOAD = 508):
// fs_write_at requires block-aligned offsets, and the streamed write flushes full buffers, so each
// offset is a multiple of this. 7*508 = 3556 (the fs's own streaming chunk). A non-multiple (e.g. 4096)
// makes the SECOND flush land on an unaligned offset and the write fails - only visible past one buffer.
const FMT_IOBUF: usize = 3556;

/// Stream-format `src` into a fresh temp `tmp` (2-pass: count the size for `OP_WRITE_NEW`, then
/// stream-write). Reads `src` and writes `tmp` - DIFFERENT files, never `src` twice at once. Returns
/// the formatted byte count. On failure deletes the temp; the caller leaves `src` untouched.
fn fmt_to_temp(ctx: &ShellCtx, src: &[u8], tmp: &[u8]) -> Result<u64, FmtErr> {
    let mut total = 0u64;
    {
        let mut count = |bytes: &[u8]| -> bool { total += bytes.len() as u64; true };
        fmt_stream_pass(ctx, src, &mut count)?; // no temp exists yet - safe to `?`
    }
    let _ = fs_request(ctx, OP_DELETE, tmp, &[]); // clear any stale temp
    if !fs_write_new(ctx, tmp, total) { return Err(FmtErr::Write); }
    let mut wlen = 0usize;
    let mut woff = 0u64;
    let mut werr = false;
    let r = {
        let mut wbuf = [0u8; FMT_IOBUF];
        let mut write = |bytes: &[u8]| -> bool {
            let mut o = 0usize;
            while o < bytes.len() {
                if wlen == wbuf.len() {
                    if !fs_write_at(ctx, tmp, woff, &wbuf[..wlen]) { werr = true; return false; }
                    woff += wlen as u64; wlen = 0;
                }
                let take = (bytes.len() - o).min(wbuf.len() - wlen);
                wbuf[wlen..wlen + take].copy_from_slice(&bytes[o..o + take]);
                wlen += take; o += take;
            }
            true
        };
        let rr = fmt_stream_pass(ctx, src, &mut write);
        if !werr && wlen > 0 && !fs_write_at(ctx, tmp, woff, &wbuf[..wlen]) { werr = true; } // final flush
        rr
    };
    if let Err(e) = r { let _ = fs_request(ctx, OP_DELETE, tmp, &[]); return Err(e); }
    if werr { let _ = fs_request(ctx, OP_DELETE, tmp, &[]); return Err(FmtErr::Write); }
    Ok(total)
}

/// Stream-compare two files; true iff byte-identical. Reads them SEQUENTIALLY (one then the other),
/// so it is safe even when one path is the source (no two concurrent reads of the same file).
fn fmt_compare_files(ctx: &ShellCtx, a: &[u8], b: &[u8]) -> bool {
    let mut off = 0u64;
    let mut ba = [0u8; FMT_IOBUF];
    let mut bb = [0u8; FMT_IOBUF];
    loop {
        let ka = fs_read_at(ctx, a, off, &mut ba).unwrap_or(0);
        let kb = fs_read_at(ctx, b, off, &mut bb).unwrap_or(0);
        if ka != kb { return false; }
        if ka == 0 { return true; }
        if ba[..ka] != bb[..ka] { return false; }
        off += ka as u64;
    }
}

/// `fmt <path>` - format a `.gsh` script to the GodspeedOS standard, IN PLACE, STREAMED (any size, no
/// cap). `fmt check <path>` - report (loud + `Err`) whether it is already canonical, without writing.
/// Guardrails (loud, file UNTOUCHED): won't-parse (unbalanced braces), or a single statement too long
/// to hold. The format write streams into a temp then renames, so a failure never damages the original.
fn cmd_fmt(ctx: &ShellCtx, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
    let rest = rest.trim();
    let (check, pathstr) = match rest.split_once(char::is_whitespace) {
        Some(("check", p)) => (true, p.trim()),
        _ => (false, rest),
    };
    if pathstr.is_empty() {
        ctx.console_writeln("usage: fmt <path>   |   fmt check <path>   (see: fmt help)");
        return Err(ShellError::Unknown);
    }
    // `fmt a,b` / `fmt check a,b`: process each file SEQUENTIALLY (never concurrently - a concurrent
    // same-file read once hung the shell). Report each, continue past a failure.
    if pathstr.contains(',') {
        let mut n = 0usize;
        for s in pathstr.split(',') {
            if s.is_empty() || n >= 16 { continue; }
            n += 1;
            let _ = fmt_one(ctx, cwd, check, s);
        }
        return Ok(());
    }
    fmt_one(ctx, cwd, check, pathstr)
}

/// Format or check ONE file. Bare `fmt [check] <path>`, and per-file (sequentially) for a comma-list.
fn fmt_one(ctx: &ShellCtx, cwd: &Cwd, check: bool, pathstr: &str) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pathstr, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut pcopy = [0u8; PATH_MAX];
    let pl = path.len(); pcopy[..pl].copy_from_slice(path); let p = &pcopy[..pl];

    const SUF: &[u8] = b".fmt~";
    if p.len() + SUF.len() > PATH_MAX { ctx.console_writeln("fmt: path too long"); return Err(ShellError::Unknown); }
    let mut tbuf = [0u8; PATH_MAX];
    tbuf[..p.len()].copy_from_slice(p);
    tbuf[p.len()..p.len() + SUF.len()].copy_from_slice(SUF);
    let tmp = &tbuf[..p.len() + SUF.len()];

    let total = match fmt_to_temp(ctx, p, tmp) {
        Ok(t) => t,
        Err(FmtErr::Unparseable) => { ctx.console_writeln_fmt(format_args!("fmt: {} won't parse (unbalanced braces?) - left untouched", str_of(p))); return Err(ShellError::Unknown); }
        Err(FmtErr::UnitTooLong) => { ctx.console_writeln_fmt(format_args!("fmt: {} has a statement too long to format - left untouched", str_of(p))); return Err(ShellError::Unknown); }
        Err(FmtErr::Write)       => { ctx.console_writeln_fmt(format_args!("fmt: write failed - {} left untouched", str_of(p))); return Err(ShellError::Unknown); }
    };

    if check {
        // Compare the freshly-formatted temp against the original (two DIFFERENT files, read
        // sequentially), then discard the temp. `check` never modifies the file.
        let canonical = fmt_compare_files(ctx, tmp, p);
        let _ = fs_request(ctx, OP_DELETE, tmp, &[]);
        if canonical { return Ok(()); } // silent Ok
        ctx.console_writeln_fmt(format_args!("fmt: {} is not canonical (run: fmt {})", str_of(p), str_of(p)));
        return Err(ShellError::Unknown);
    }

    // Commit: the temp holds the whole formatted output; delete the original, rename the temp in.
    let mut bstart = 0usize;
    for (i, &c) in p.iter().enumerate() { if c == b'/' { bstart = i + 1; } }
    let base = &p[bstart..];
    let _ = fs_request(ctx, OP_DELETE, p, &[]);
    if matches!(fs_request(ctx, OP_RENAME, tmp, base), Some(r) if r.payload_bytes().first() == Some(&FS_OK)) {
        ctx.console_writeln_fmt(format_args!("fmt {} ({} bytes)", str_of(p), total));
        Ok(())
    } else {
        ctx.console_writeln_fmt(format_args!("fmt: rename failed - formatted content is in {}.fmt~", str_of(p)));
        Err(ShellError::Unknown)
    }
}

/// `mkdir <path> [parents]` - create a directory (with `parents`, create missing parents).
fn cmd_mkdir(ctx: &ShellCtx, cwd: &Cwd, arg: &str, parents: bool) -> Result<(), ShellError> {
    // `mkdir a,b,c` creates each; `parents` applies to every segment. Report each, continue past a failure.
    if arg.contains(',') {
        let mut n = 0usize;
        for s in arg.split(',') {
            if s.is_empty() || n >= 16 { continue; }
            n += 1;
            let _ = mkdir_one(ctx, cwd, s, parents);
        }
        return Ok(());
    }
    mkdir_one(ctx, cwd, arg, parents)
}

/// Create ONE directory. Bare `mkdir <path> [parents]`, and per-segment for a comma-list.
fn mkdir_one(ctx: &ShellCtx, cwd: &Cwd, arg: &str, parents: bool) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let op = if parents { OP_MKDIR_P } else { OP_MKDIR };
    let reply = match fs_request(ctx, op, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("mkdir: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_OK) {
        ctx.console_writeln_fmt(format_args!("created {}", str_of(path)));
        Ok(())
    } else if parents {
        ctx.console_writeln("mkdir: failed (a component is in the way as a file?)");
        Err(ShellError::Unknown)
    } else {
        ctx.console_writeln("mkdir: failed (already exists, or parent missing? try 'mkdir <path> parents')");
        Err(ShellError::Unknown)
    }
}

/// `cd [path]` - change the current directory (validates it exists + is a directory).
fn cmd_cd(ctx: &ShellCtx, cwd: &mut Cwd, arg: &str) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    // `cd -` toggles to the previous directory (already an absolute, normalized path - use it
    // directly, then run the same stat-validated switch so a since-deleted dir errors loudly).
    let path: &[u8] = if arg == "-" {
        let pl = cwd.prev_len;
        buf[..pl].copy_from_slice(&cwd.prev[..pl]);
        &buf[..pl]
    } else {
        match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) }
    };
    // Root always exists - no need to stat it.
    if path == b"/" {
        cwd.set(b"/");
        ctx.console_writeln("/");
        return Ok(());
    }
    let reply = match fs_request_q(ctx, OP_STAT_FILE, path, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("cd: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    // STAT reply: [FS_OK, exists, size:u64, is_dir].
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 {
        if p[10] == 1 {
            cwd.set(path);
            ctx.console_writeln(cwd.as_str());
            Ok(())
        } else {
            ctx.console_writeln_fmt(format_args!("cd: not a directory: {}", str_of(path)));
            Err(ShellError::Unknown)
        }
    } else {
        ctx.console_writeln_fmt(format_args!("cd: no such directory: {}", str_of(path)));
        Err(ShellError::FileNotFound)
    }
}

/// `copy <src> <dst>` - copy a file by STREAMING it through fixed chunks (read_at/write_at),
/// so it copies files far larger than one IPC message with no whole-file buffer. File-only in
/// this cut (no recursive dirs - that's `copy … recursive`).
fn cmd_copy(ctx: &ShellCtx, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
    let mut sbuf = [0u8; PATH_MAX];
    let spath = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut sp = [0u8; PATH_MAX];
    let sl = spath.len();
    sp[..sl].copy_from_slice(spath);
    // Check the source exists and is a file (also surfaces the "no filesystem" hint).
    let stat = match fs_request(ctx, OP_STAT_FILE, &sp[..sl], &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("copy: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let stp = stat.payload_bytes();
    if no_fs(ctx, stp) { return Err(ShellError::Unknown); }
    let exists = stp.first() == Some(&FS_OK) && stp.len() >= 11 && stp[1] == 1;
    if !exists {
        ctx.console_writeln_fmt(format_args!("copy: source not found: {}", str_of(&sp[..sl])));
        return Err(ShellError::FileNotFound);
    }
    if stp[10] == 1 {
        ctx.console_writeln("copy: source is a directory (use 'copy <src> <dst> recursive')");
        return Err(ShellError::Unknown);
    }
    drop(stat);

    let mut dbuf = [0u8; PATH_MAX];
    let dpath = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut dp = [0u8; PATH_MAX];
    let dl = dpath.len();
    dp[..dl].copy_from_slice(dpath);
    match copy_file_streaming(ctx, &sp[..sl], &dp[..dl]) {
        Some(bytes) => {
            ctx.console_writeln_fmt(format_args!("copied {} → {} ({} bytes)", str_of(&sp[..sl]), str_of(&dp[..dl]), bytes));
            Ok(())
        }
        None => { ctx.console_writeln("copy: write failed (parent missing?)"); Err(ShellError::Unknown) }
    }
}

/// `copy <src> <dst> recursive` - copy a whole subtree. Reuses the SAME bounded walk
/// (`PathStack`) `find` uses (§26.6): pop a source dir, recreate it under `dst`, then for
/// each child either copy the file (read+write, existing ops) or push the subdir. No new fs
/// surface - copy already lives in the shell. Loud if the tree is wider than the walk's cap
/// (§3.12), and refuses to copy a directory into its own subtree (would never terminate).
fn cmd_copy_tree(ctx: &ShellCtx, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
    let mut sbuf = [0u8; PATH_MAX];
    let src_abs = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut sp = [0u8; PATH_MAX];
    let sl = src_abs.len();
    sp[..sl].copy_from_slice(src_abs);
    if &sp[..sl] == b"/" { ctx.console_writeln("copy: cannot copy the root directory"); return Err(ShellError::Unknown); }

    let mut dbuf = [0u8; PATH_MAX];
    let dst_abs = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut dp = [0u8; PATH_MAX];
    let dl = dst_abs.len();
    dp[..dl].copy_from_slice(dst_abs);
    // Dest inside src (or equal) → the walk would copy what it just created, forever.
    if dp[..dl] == sp[..sl] || (dl > sl && dp[..sl] == sp[..sl] && dp[sl] == b'/') {
        ctx.console_writeln("copy: cannot copy into itself");
        return Err(ShellError::Unknown);
    }

    // A plain file? Fall back to the single-file copy (this command is for subtrees).
    match stat_kind(ctx, &sp[..sl]) {
        Some(false) => { return cmd_copy(ctx, cwd, src, dst); }
        Some(true)  => {}
        None        => { ctx.console_writeln_fmt(format_args!("copy: source not found: {}", str_of(&sp[..sl]))); return Err(ShellError::FileNotFound); }
    }

    // Create the destination root, then walk the source breadth-first.
    if !mkdir_at(ctx, &dp[..dl]) {
        ctx.console_writeln("copy: cannot create destination (already exists?)");
        return Err(ShellError::Unknown);
    }
    let mut stack = PathStack::new();
    stack.push(&sp[..sl]);
    let (mut dirs, mut files) = (1u32, 0u32);
    while let Some(slen) = stack.pop(&mut sbuf) {
        let reply = match fs_request(ctx, OP_LIST_DIR, &sbuf[..slen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("copy: storage unavailable"); return Err(ShellError::Unknown); }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return Err(ShellError::Unknown); }
        if p.first() != Some(&FS_OK) || p.len() < 2 { continue; }
        let count = p[1] as usize;
        let mut i = 2usize;
        for _ in 0..count {
            if i >= p.len() { break; }
            let nl = p[i] as usize;
            i += 1;
            if i + nl + 1 + 8 > p.len() { break; }
            let name = &p[i..i + nl];
            let is_dir = p[i + nl] != 0;
            i += nl + 1 + 8; // name_len + name + is_dir + size:u64
            let mut schild = [0u8; PATH_MAX];
            let clen = match join_path(&sbuf[..slen], name, &mut schild) { Some(c) => c, None => continue };
            let mut dchild = [0u8; PATH_MAX];
            let dclen = match remap(&dp[..dl], &sp[..sl], &schild[..clen], &mut dchild) { Some(c) => c, None => continue };
            if is_dir {
                if mkdir_at(ctx, &dchild[..dclen]) { dirs += 1; }
                stack.push(&schild[..clen]);
            } else if copy_one(ctx, &schild[..clen], &dchild[..dclen]) {
                files += 1;
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "copy: truncated - tree wider than {} pending directories (bounded walk)", FIND_QCAP));
    }
    ctx.console_writeln_fmt(format_args!(
        "copied {} → {} ({} dirs, {} files)", str_of(&sp[..sl]), str_of(&dp[..dl]), dirs, files));
    Ok(())
}

/// Stat a path: `Some(is_dir)` if it exists, `None` if not (or storage is down).
fn stat_kind(ctx: &ShellCtx, path: &[u8]) -> Option<bool> {
    let reply = fs_request(ctx, OP_STAT_FILE, path, &[])?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 { Some(p[10] != 0) } else { None }
}

/// `mkdir <path>` via fs, treating success as true. Used by recursive copy to recreate dirs.
fn mkdir_at(ctx: &ShellCtx, path: &[u8]) -> bool {
    matches!(fs_request(ctx, OP_MKDIR, path, &[]), Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// Stream-copy a file `src`→`dst` of any size: stat the size, allocate `dst`, then chunk
/// through with `read_at`/`write_at` (one IO_CHUNK buffer, no whole-file buffer). Returns
/// `Some(bytes)` on success. The building block under both `copy` and recursive `copy`.
fn copy_file_streaming(ctx: &ShellCtx, src: &[u8], dst: &[u8]) -> Option<u64> {
    let (size, is_dir) = fs_stat(ctx, src)?;
    if is_dir { return None; }
    if !fs_write_new(ctx, dst, size) { return None; }
    let mut chunk = [0u8; IO_CHUNK];
    let mut off = 0u64;
    while off < size {
        let n = fs_read_at(ctx, src, off, &mut chunk)?;
        if n == 0 { break; }
        if !fs_write_at(ctx, dst, off, &chunk[..n]) { return None; }
        off += n as u64;
    }
    Some(size)
}

/// Copy one file `src`→`dst` by streaming. Returns true on success; logs on failure so a
/// single bad file in a subtree copy is visible but does not abort the whole walk (§3.12).
fn copy_one(ctx: &ShellCtx, src: &[u8], dst: &[u8]) -> bool {
    match copy_file_streaming(ctx, src, dst) {
        Some(_) => true,
        None => { ctx.console_writeln_fmt(format_args!("copy: skipped (copy failed): {}", str_of(src))); false }
    }
}

/// Map a source path under `src_root` onto `dst_root`: `dst_root + (s - src_root)`. `s` always
/// begins with `src_root` (it came from walking under it), so the suffix is the relative tail.
fn remap(dst_root: &[u8], src_root: &[u8], s: &[u8], out: &mut [u8; PATH_MAX]) -> Option<usize> {
    let suffix = &s[src_root.len()..]; // "" for the root itself, else "/sub/..."
    if dst_root.len() + suffix.len() > PATH_MAX { return None; }
    out[..dst_root.len()].copy_from_slice(dst_root);
    out[dst_root.len()..dst_root.len() + suffix.len()].copy_from_slice(suffix);
    Some(dst_root.len() + suffix.len())
}

/// `rename <path> <newname>` - rename an entry in place (not a move; newname is one
/// component). fs edits the directory entry; no blocks are read or freed.
fn cmd_rename(ctx: &ShellCtx, cwd: &Cwd, path: &str, newname: &str) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut pp = [0u8; PATH_MAX];
    let pl = abspath.len();
    pp[..pl].copy_from_slice(abspath);
    // fs_request appends `newname` after the path - exactly the OP_RENAME wire format.
    match fs_request(ctx, OP_RENAME, &pp[..pl], newname.as_bytes()) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("renamed {} → {}", str_of(&pp[..pl]), newname));
            Ok(())
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => Err(ShellError::Unknown),
        Some(_) => { ctx.console_writeln("rename: failed (not found, or name exists, or bad name)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("rename: storage unavailable"); Err(ShellError::Unknown) }
    }
}

/// `delete <path>` - remove a file or empty directory; `delete <path> recursive` removes a
/// whole subtree. fs does the work either way (plain = `OP_DELETE`, recursive =
/// `OP_DELETE_TREE`, a depth-bounded subtree free); it frees the blocks and reclaims them.
fn cmd_delete(ctx: &ShellCtx, cwd: &Cwd, arg: &str, recursive: bool) -> Result<(), ShellError> {
    // `delete /a,/b` deletes each; `recursive` applies to every segment. Report each, continue past one
    // that is missing / fails.
    if arg.contains(',') {
        let mut n = 0usize;
        for s in arg.split(',') {
            if s.is_empty() || n >= 16 { continue; }
            n += 1;
            let _ = delete_one(ctx, cwd, s, recursive);
        }
        return Ok(());
    }
    delete_one(ctx, cwd, arg, recursive)
}

/// Delete ONE path. Bare `delete <path> [recursive]`, and per-segment for a comma-list.
fn delete_one(ctx: &ShellCtx, cwd: &Cwd, arg: &str, recursive: bool) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    if path == b"/" {
        ctx.console_writeln("delete: cannot delete the root directory");
        return Err(ShellError::Unknown);
    }
    let mut pp = [0u8; PATH_MAX];
    let pl = path.len();
    pp[..pl].copy_from_slice(path);
    let op = if recursive { OP_DELETE_TREE } else { OP_DELETE };
    match fs_request(ctx, op, &pp[..pl], &[]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            let what = if recursive { "deleted (recursive)" } else { "deleted" };
            ctx.console_writeln_fmt(format_args!("{} {}", what, str_of(&pp[..pl])));
            Ok(())
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => Err(ShellError::Unknown),
        Some(_) if recursive => { ctx.console_writeln("delete: failed (not found, or tree too deep?)"); Err(ShellError::Unknown) }
        Some(_) => { ctx.console_writeln("delete: failed (not found, or directory not empty? use 'delete <path> recursive')"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("delete: storage unavailable"); Err(ShellError::Unknown) }
    }
}

/// `move <src> <dst>` - relocate an entry (same data; only the directory entries change).
fn cmd_move(ctx: &ShellCtx, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
    let mut sbuf = [0u8; PATH_MAX];
    let spath = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut sp = [0u8; PATH_MAX];
    let sl = spath.len();
    sp[..sl].copy_from_slice(spath);
    let mut dbuf = [0u8; PATH_MAX];
    let dpath = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut dp = [0u8; PATH_MAX];
    let dl = dpath.len();
    dp[..dl].copy_from_slice(dpath);
    // Guard against moving a directory into itself or its own subtree (would orphan it).
    if dp[..dl] == sp[..sl] || (dl > sl && dp[..sl] == sp[..sl] && dp[sl] == b'/') {
        ctx.console_writeln("move: cannot move into itself");
        return Err(ShellError::Unknown);
    }
    match fs_request(ctx, OP_MOVE, &sp[..sl], &dp[..dl]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("moved {} → {}", str_of(&sp[..sl]), str_of(&dp[..dl])));
            Ok(())
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => Err(ShellError::Unknown),
        Some(_) => { ctx.console_writeln("move: failed (not found, or dest exists?)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("move: storage unavailable"); Err(ShellError::Unknown) }
    }
}

/// `find <pattern> [path]` - search a subtree (default the whole filesystem, `/`) for entries
/// matching `<pattern>`, printing each match's full path. A plain word is a substring match; a
/// pattern with `*`/`?` is a glob (anchored, whole-name). This is whole-filesystem
/// enumeration done the disciplined way: a **tree walk** (the tree IS the index, §6.4),
/// client-side via LIST_DIR so results stream as found and `fs` needs no new op. The walk
/// is bounded (a fixed pending-directory stack) and **loud on truncation** (§26.6/§3.12);
/// the `fs_index` accelerator (persistence.md §6.5) is what we'd build if this walk ever
/// gets too slow on a huge tree - not before.
fn cmd_find(ctx: &ShellCtx, cwd: &Cwd, target: &str, start: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut sbuf = [0u8; PATH_MAX];
    let start_abs = match resolve_or_err(ctx, cwd, start, &mut sbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let mut stack = PathStack::new();
    stack.push(start_abs);

    let target = target.as_bytes();
    // A pattern with `*` or `?` is a glob (anchored, whole-name match); otherwise the friendly
    // default is a plain substring match (so `find report` still finds `report-final.txt`).
    let is_glob = target.iter().any(|&b| b == b'*' || b == b'?');
    let mut matches = 0u32;
    let mut dir = [0u8; PATH_MAX];
    while let Some(dlen) = stack.pop(&mut dir) {
        let reply = match fs_request_q(ctx, OP_LIST_DIR, &dir[..dlen], &[]) {
            ReqOutcome::Reply(r) => r,
            ReqOutcome::Aborted => return Ok(()),
            ReqOutcome::Timeout => { ctx.console_writeln("find: storage unavailable"); return Err(ShellError::Unknown); }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return Err(ShellError::Unknown); }
        if p.first() != Some(&FS_OK) || p.len() < 2 { continue; }
        let count = p[1] as usize;
        let mut i = 2usize;
        for _ in 0..count {
            if i >= p.len() { break; }
            let nl = p[i] as usize;
            i += 1;
            if i + nl + 1 + 8 > p.len() { break; }
            let name = &p[i..i + nl];
            let is_dir = p[i + nl] != 0;
            i += nl + 1 + 8; // name_len + name + is_dir + size:u64
            let mut child = [0u8; PATH_MAX];
            if let Some(clen) = join_path(&dir[..dlen], name, &mut child) {
                let hit = if is_glob { glob_match(target, name) } else { contains(name, target) };
                if hit {
                    // The matched paths are the pipe data; the summary below is metadata.
                    out.line(ctx, str_of(&child[..clen]));
                    matches += 1;
                }
                if is_dir {
                    stack.push(&child[..clen]);
                }
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "find: search truncated - more than {} directories pending (bounded walk)", FIND_QCAP));
    }
    ctx.console_writeln_fmt(format_args!("find: {} match(es)", matches));
    Ok(()) // a search that finds nothing still succeeded (0 matches is not an error)
}

/// Max depth tracked for box-drawing prefixes; deeper levels just keep a continuation bar.
const TREE_MAX_DEPTH: usize = 32;
/// Prefix scratch: up to `TREE_MAX_DEPTH` levels × the widest piece (`"│   "` = 6 bytes).
const TREE_PREFIX_MAX: usize = TREE_MAX_DEPTH * 6;

/// `tree [path]` - print the directory hierarchy with box-drawing connectors, like Unix `tree`
/// (default: the current directory). Same bounded-walk discipline as `find` (§26.6): a fixed-
/// capacity explicit stack, depth-first, no recursion, loud on overflow (§3.12). A directory's
/// whole subtree drains before its next sibling (LIFO + reverse-push), and each node records
/// whether it is its parent's *last* child so the prefix draws `├──`/`└──` and `│`/blank
/// continuation correctly. UTF-8: the `console` service decodes `├ └ │ ─` and renders light box glyphs;
/// a trailing `/` still marks directories (the console is monochrome - no colour to lean on).
/// `#[inline(never)]`: holds the ~12 KiB `TreeStack` + prefix scratch off the hot pipe frame
/// (it's a pipe producer; see [[project-shell-stack-pipe]]).
#[inline(never)]
fn cmd_tree(ctx: &ShellCtx, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let start = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    match stat_kind(ctx, start) {
        Some(true)  => {}
        Some(false) => { out.line(ctx, str_of(start)); out.line(ctx, ""); out.line(ctx, "0 directories, 1 file"); return Ok(()); }
        None        => { ctx.console_writeln_fmt(format_args!("tree: not found: {}", str_of(start))); return Err(ShellError::FileNotFound); }
    }
    let mut stack = TreeStack::new();
    stack.push(start, true, 0, true);
    let (mut dirs, mut files) = (0u32, 0u32);
    // `level_last[k]` = was the ancestor at depth k its parent's last child? (drives the prefix:
    // a non-last ancestor draws a `│` continuation, a last one draws blank). The DFS finishes a
    // subtree before its siblings, so this stays valid for every descendant.
    let mut level_last = [false; TREE_MAX_DEPTH];
    let mut pre = [0u8; TREE_PREFIX_MAX];
    while let Some((plen, is_dir, depth, is_last)) = stack.pop(&mut buf) {
        let d = depth as usize;
        if d == 0 {
            out.line(ctx, str_of(&buf[..plen])); // root: full path, no connector
        } else {
            // Build the prefix from the ancestors' last-child flags, then the connector.
            let mut pl = 0usize;
            for k in 1..d {
                let piece: &[u8] = if k < TREE_MAX_DEPTH && level_last[k] { "    ".as_bytes() } else { "│   ".as_bytes() };
                if pl + piece.len() <= pre.len() { pre[pl..pl + piece.len()].copy_from_slice(piece); pl += piece.len(); }
            }
            out.put(ctx, str_of(&pre[..pl]));
            out.put(ctx, if is_last { "└── " } else { "├── " });
            let name = basename(&buf[..plen]);
            if is_dir { out.line_fmt(ctx, format_args!("{}/", str_of(name))); }
            else      { out.line(ctx, str_of(name)); }
        }
        if d < TREE_MAX_DEPTH { level_last[d] = is_last; } // for this node's children
        if !is_dir { files += 1; continue; }
        if d > 0 { dirs += 1; }

        let reply = match fs_request_q(ctx, OP_LIST_DIR, &buf[..plen], &[]) {
            ReqOutcome::Reply(r) => r,
            ReqOutcome::Aborted => return Ok(()),
            ReqOutcome::Timeout => { ctx.console_writeln("tree: storage unavailable"); return Err(ShellError::Unknown); }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return Err(ShellError::Unknown); }
        if p.first() != Some(&FS_OK) || p.len() < 2 { continue; }
        // Record each child's offset, then push in REVERSE so they pop in directory order.
        let count = p[1] as usize;
        let mut offs = [0usize; TREE_FANOUT];
        let mut nc = 0usize;
        let mut i = 2usize;
        for _ in 0..count {
            if i >= p.len() || nc >= TREE_FANOUT { break; }
            let nl = p[i] as usize;
            if i + 1 + nl + 1 + 8 > p.len() { break; }
            offs[nc] = i;
            nc += 1;
            i += 1 + nl + 1 + 8;
        }
        for k in (0..nc).rev() {
            let off = offs[k];
            let nl = p[off] as usize;
            let cname = &p[off + 1..off + 1 + nl];
            let cdir = p[off + 1 + nl] != 0;
            let mut child = [0u8; PATH_MAX];
            if let Some(clen) = join_path(&buf[..plen], cname, &mut child) {
                // The last child read (forward order) is its parent's last → draws `└──`.
                stack.push(&child[..clen], cdir, depth + 1, k == nc - 1);
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "tree: truncated - more than {} pending entries (bounded walk)", TREE_CAP));
    }
    out.line(ctx, "");
    out.line_fmt(ctx, format_args!(
        "{} director{}, {} file{}",
        dirs, if dirs == 1 { "y" } else { "ies" }, files, if files == 1 { "" } else { "s" }));
    Ok(())
}

/// Last path component (`/a/b/c` → `c`); the whole path if it has no `/`.
fn basename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

/// Bounded DFS stack for `tree`: each slot carries a path, whether it is a directory, and its
/// depth (for indentation). Fixed capacity (§26.6); pushing past it sets `overflow` so `tree`
/// reports truncation rather than silently dropping part of the tree (§3.12).
const TREE_CAP: usize = 96;
const TREE_FANOUT: usize = 64; // max children read from one LIST_DIR reply (one block)
struct TreeStack {
    buf: [[u8; PATH_MAX]; TREE_CAP],
    len: [usize; TREE_CAP],
    is_dir: [bool; TREE_CAP],
    depth: [u16; TREE_CAP],
    is_last: [bool; TREE_CAP], // is this the last child of its parent? (drives └── vs ├──)
    top: usize,
    overflow: bool,
}
impl TreeStack {
    fn new() -> Self {
        TreeStack {
            buf: [[0u8; PATH_MAX]; TREE_CAP], len: [0; TREE_CAP],
            is_dir: [false; TREE_CAP], depth: [0; TREE_CAP], is_last: [false; TREE_CAP],
            top: 0, overflow: false,
        }
    }
    fn push(&mut self, p: &[u8], is_dir: bool, depth: u16, is_last: bool) {
        if self.top >= TREE_CAP || p.len() > PATH_MAX { self.overflow = true; return; }
        self.buf[self.top][..p.len()].copy_from_slice(p);
        self.len[self.top] = p.len();
        self.is_dir[self.top] = is_dir;
        self.depth[self.top] = depth;
        self.is_last[self.top] = is_last;
        self.top += 1;
    }
    fn pop(&mut self, out: &mut [u8; PATH_MAX]) -> Option<(usize, bool, u16, bool)> {
        if self.top == 0 { return None; }
        self.top -= 1;
        let l = self.len[self.top];
        out[..l].copy_from_slice(&self.buf[self.top][..l]);
        Some((l, self.is_dir[self.top], self.depth[self.top], self.is_last[self.top]))
    }
}

/// Join `dir` + `name` into an absolute child path (`/` separator, no double slash).
fn join_path(dir: &[u8], name: &[u8], out: &mut [u8; PATH_MAX]) -> Option<usize> {
    if dir.len() > PATH_MAX { return None; }
    out[..dir.len()].copy_from_slice(dir);
    let mut len = dir.len();
    if len == 0 || out[len - 1] != b'/' {
        if len >= PATH_MAX { return None; }
        out[len] = b'/';
        len += 1;
    }
    if len + name.len() > PATH_MAX { return None; }
    out[len..len + name.len()].copy_from_slice(name);
    Some(len + name.len())
}

/// True if `needle` appears as a contiguous substring of `haystack` (find's default match).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() { return true; }
    if needle.len() > haystack.len() { return false; }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Match `name` against a glob `pat`: `*` matches any run (incl. empty), `?` matches exactly
/// one character, everything else is literal. Anchored at both ends (a glob matches the whole
/// name, like a shell). Iterative backtracking - no recursion, no allocation (§26.6): on a
/// mismatch it falls back to extending the most recent `*`.
fn glob_match(pat: &[u8], name: &[u8]) -> bool {
    let (mut p, mut s) = (0usize, 0usize);
    let mut star: Option<usize> = None; // pattern index just past the last '*' seen
    let mut star_s = 0usize;            // name index that '*' is currently consuming up to
    while s < name.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == name[s]) {
            p += 1;
            s += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            star_s = s;
            p += 1;
        } else if let Some(sp) = star {
            // Mismatch: let the last '*' swallow one more character and retry.
            p = sp + 1;
            star_s += 1;
            s = star_s;
        } else {
            return false;
        }
    }
    // Trailing '*'s in the pattern can still match the (now empty) remainder.
    while p < pat.len() && pat[p] == b'*' { p += 1; }
    p == pat.len()
}

fn str_of(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap_or("?")
}

// ── match - keep the lines that match a pattern (the grep-equivalent) ────────────
// `match [except] <pattern> <path>` filters a file; `<producer> | match <pattern>` filters
// piped input. A built-in FILTER: it consumes input and emits the matching lines. Substring
// by default, `*`/`?` glob like `find` (shared `contains`/`glob_match`); `except` inverts.
// See utilities/27_match.md.

/// Filter `input`'s lines by `pattern`, writing each matching line (with its newline) to `out`.
/// Substring by default; a pattern with `*`/`?` is an anchored glob (same as `find`). `invert`
/// keeps the lines that do NOT match (the `except` form). Blank lines are skipped.
fn match_lines(ctx: &ServiceContext, input: &[u8], pattern: &[u8], invert: bool, out: &mut Out) {
    let is_glob = pattern.iter().any(|&b| b == b'*' || b == b'?');
    for line in input.split(|&b| b == b'\n') {
        if line.is_empty() { continue; }
        let matched = if is_glob { glob_match(pattern, line) } else { contains(line, pattern) };
        if matched != invert {
            out.put_bytes(ctx, line);
            out.put(ctx, "\n");
        }
    }
}

/// Parse a `match` invocation's args from index `start`: handles the leading `except` keyword
/// and returns `(invert, pattern, path)` - `path` is "" if absent. `None` if no pattern.
fn parse_match<'a>(args: &[&'a str], argc: usize, start: usize) -> Option<(bool, &'a str, &'a str)> {
    let mut i = start;
    // `except` is the keyword only when a pattern follows it (so `match except except` still
    // matches the literal word "except": first is the keyword, second is the pattern).
    let invert = argc > i + 1 && args[i] == "except";
    if invert { i += 1; }
    if argc <= i { return None; }
    let pattern = args[i];
    i += 1;
    let path = if argc > i { args[i] } else { "" };
    Some((invert, pattern, path))
}

/// `match [except] <pattern> <path>` - print the lines of `<path>` that match (or, with
/// `except`, that do not). The pipe form filters piped input instead; either way `match` is a
/// FILTER, never a pipe producer (use `read <path> | match …` to feed a pipeline from a file).
fn cmd_match(ctx: &ShellCtx, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let (invert, pattern, path) = match parse_match(args, argc, 1) {
        Some(t) => t,
        None => { ctx.console_writeln("usage: match [except] <pattern> <path>"); return Err(ShellError::Unknown); }
    };
    if path.is_empty() {
        ctx.console_writeln("match: a path is required (or pipe input: <producer> | match <pattern>)");
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request_q(ctx, OP_READ_FILE, abspath, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("match: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        match_lines(ctx, &p[5..end], pattern.as_bytes(), invert, &mut Out::Console);
        Ok(())
    } else {
        ctx.console_writeln_fmt(format_args!("match: not found: {}", str_of(abspath)));
        Err(ShellError::FileNotFound)
    }
}

/// Run a filter built-in (`match`, `count`) over `input`, writing its output to `out`. Used
/// when the filter sits **mid-pipe** or as the last stage - it runs in-process, so it is not
/// subject to the 4 KiB service-boundary cap and can filter a full 64 KiB stage buffer.
fn run_filter_builtin(ctx: &ServiceContext, stage: &str, input: &[u8], out: &mut Out) -> bool {
    let (cmd, _) = split_first(stage);
    match cmd {
        "count" => { write_count(ctx, input, out); true }
        "sort" => {
            let mut sargs = [""; MAX_ARGS];
            let sac = tokenize(stage, &mut sargs);
            let (reverse, _) = parse_sort(&sargs, sac, 1);
            write_sorted(ctx, input, reverse, out);
            true
        }
        "first" | "last" => {
            let mut targs = [""; MAX_ARGS];
            let tac = tokenize(stage, &mut targs);
            let (n, _) = parse_take(&targs, tac, 1);
            if cmd == "last" { write_last(ctx, input, n, out); } else { write_first(ctx, input, n, out); }
            true
        }
        _ => {
            // match (the default filter): tokenize for the `except` keyword + a quoted pattern.
            let mut margs = [""; MAX_ARGS];
            let mac = tokenize(stage, &mut margs);
            match parse_match(&margs, mac, 1) {
                Some((invert, pattern, _)) => {
                    match_lines(ctx, input, pattern.as_bytes(), invert, out);
                    true
                }
                None => { ctx.console_writeln("match: usage: <producer> | match [except] <pattern>"); false }
            }
        }
    }
}

// ── count - how many lines / words / bytes (the wc-equivalent) ───────────────────
// `count <path>` counts a file; `<producer> | count` counts piped input. Like `match` it is a
// built-in FILTER (in-process, no 4 KiB cap), but it consumes many lines and emits one summary
// line, so it usually ends a pipe. See utilities/28_count.md.

/// "" for a count of 1, "s" otherwise - readable singular/plural.
fn plural(n: usize) -> &'static str { if n == 1 { "" } else { "s" } }

/// Count `input`'s lines / words / bytes and write the labelled summary to `out`. Lines = newline
/// count, plus one for a final unterminated line. Words = runs of non-whitespace bytes.
fn write_count(ctx: &ServiceContext, input: &[u8], out: &mut Out) {
    let bytes = input.len();
    let mut lines = input.iter().filter(|&&b| b == b'\n').count();
    if !input.is_empty() && input.last() != Some(&b'\n') { lines += 1; }
    let mut words = 0usize;
    let mut in_word = false;
    for &b in input {
        if b.is_ascii_whitespace() { in_word = false; }
        else if !in_word { in_word = true; words += 1; }
    }
    out.line_fmt(ctx, format_args!(
        "{} line{}, {} word{}, {} byte{}",
        lines, plural(lines), words, plural(words), bytes, plural(bytes)));
}

/// `count <path>` - count the lines / words / bytes of a file. The pipe form `<producer> |
/// count` counts piped input instead; either way `count` consumes input (never a producer).
fn cmd_count(ctx: &ShellCtx, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let path = if argc >= 2 { args[1] } else { "" };
    if path.is_empty() {
        ctx.console_writeln("count: a path is required (or pipe input: <producer> | count)");
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request_q(ctx, OP_READ_FILE, abspath, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("count: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        write_count(ctx, &p[5..end], &mut Out::Console);
        Ok(())
    } else {
        ctx.console_writeln_fmt(format_args!("count: not found: {}", str_of(abspath)));
        Err(ShellError::FileNotFound)
    }
}

// ── sort - order the lines (ascending, or `reverse`) ─────────────────────────────
// `sort [reverse] <path>` sorts a file; `<producer> | sort [reverse]` sorts piped input. A
// built-in FILTER like match/count. See utilities/29_sort.md.

/// Most lines `sort` will order in one pass (§26.6 bounded). Beyond this it sorts the first
/// `SORT_MAX_LINES` and says so - never silently drops the rest. The index array is
/// `SORT_MAX_LINES × 16 bytes` on the stack.
const SORT_MAX_LINES: usize = 1024;

/// Pick `reverse` out of a `sort` invocation's args and return `(reverse, path)`. `reverse` is a
/// keyword wherever it appears (after the verb); the other arg is the path ("" if none).
fn parse_sort<'a>(args: &[&'a str], argc: usize, start: usize) -> (bool, &'a str) {
    let mut reverse = false;
    let mut path = "";
    for i in start..argc {
        if args[i] == "reverse" { reverse = true; }
        else if path.is_empty() { path = args[i]; }
    }
    (reverse, path)
}

/// Sort `input`'s lines lexicographically (by bytes) and write them to `out`, descending if
/// `reverse`. Blank lines are dropped; ties keep no defined order (`sort_unstable`). Bounded:
/// the first `SORT_MAX_LINES` are sorted, with a loud note if there are more.
fn write_sorted(ctx: &ServiceContext, input: &[u8], reverse: bool, out: &mut Out) {
    let mut lines: [(usize, usize); SORT_MAX_LINES] = [(0, 0); SORT_MAX_LINES];
    let mut n = 0usize;
    let mut overflow = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= input.len() {
        if i == input.len() || input[i] == b'\n' {
            if i > start {
                if n < SORT_MAX_LINES { lines[n] = (start, i); n += 1; } else { overflow = true; }
            }
            start = i + 1;
        }
        i += 1;
    }
    lines[..n].sort_unstable_by(|&(s1, e1), &(s2, e2)| input[s1..e1].cmp(&input[s2..e2]));
    let mut emit = |k: usize| {
        let (s, e) = lines[k];
        out.put_bytes(ctx, &input[s..e]);
        out.put(ctx, "\n");
    };
    if reverse { for k in (0..n).rev() { emit(k); } } else { for k in 0..n { emit(k); } }
    if overflow {
        ctx.console_writeln_fmt(format_args!(
            "sort: more than {} lines - sorted the first {} (bounded)", SORT_MAX_LINES, SORT_MAX_LINES));
    }
}

/// `sort [reverse] <path>` - print a file's lines in order. The pipe form `<producer> | sort`
/// sorts piped input instead; either way `sort` consumes input (never a producer).
fn cmd_sort(ctx: &ShellCtx, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let (reverse, path) = parse_sort(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln("sort: a path is required (or pipe input: <producer> | sort)");
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request_q(ctx, OP_READ_FILE, abspath, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln("sort: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        write_sorted(ctx, &p[5..end], reverse, &mut Out::Console);
        Ok(())
    } else {
        ctx.console_writeln_fmt(format_args!("sort: not found: {}", str_of(abspath)));
        Err(ShellError::FileNotFound)
    }
}

// ── first / last - keep the first or last N lines (the head/tail-equivalent) ──────
// `first [N] <path>` / `last [N] <path>` for a file; `<producer> | first [N]` for a pipe.
// Built-in FILTERS like match/count/sort. N defaults to 10. See utilities/30_first-last.md.

const TAKE_DEFAULT: usize = 10;
const TAKE_MAX: usize = 1024; // `last`'s ring keeps at most this many recent lines (§26.6)

/// Pick the count and path out of a `first`/`last` invocation's args: a numeric arg is N (else
/// the default), a non-numeric arg is the path ("" if none).
fn parse_take<'a>(args: &[&'a str], argc: usize, start: usize) -> (usize, &'a str) {
    let mut n = TAKE_DEFAULT;
    let mut path = "";
    for i in start..argc {
        if let Ok(num) = args[i].parse::<usize>() { n = num; }
        else if path.is_empty() { path = args[i]; }
    }
    (n, path)
}

/// Write the first `n` non-empty lines of `input` to `out` (one pass, no buffer).
fn write_first(ctx: &ServiceContext, input: &[u8], n: usize, out: &mut Out) {
    let mut emitted = 0usize;
    for line in input.split(|&b| b == b'\n') {
        if line.is_empty() { continue; }
        if emitted >= n { break; }
        out.put_bytes(ctx, line);
        out.put(ctx, "\n");
        emitted += 1;
    }
}

/// Write the last `n` non-empty lines of `input` to `out`. Keeps the most recent `TAKE_MAX`
/// line spans in a ring buffer, so it is correct even for input far larger than the ring; `n`
/// itself is capped at `TAKE_MAX` (loud if more was asked).
fn write_last(ctx: &ServiceContext, input: &[u8], n: usize, out: &mut Out) {
    let capped = n.min(TAKE_MAX);
    if n > TAKE_MAX {
        ctx.console_writeln_fmt(format_args!("last: capped at {} lines (asked {})", TAKE_MAX, n));
    }
    let mut ring: [(usize, usize); TAKE_MAX] = [(0, 0); TAKE_MAX];
    let mut total = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    while i <= input.len() {
        if i == input.len() || input[i] == b'\n' {
            if i > start { ring[total % TAKE_MAX] = (start, i); total += 1; }
            start = i + 1;
        }
        i += 1;
    }
    let take = capped.min(total);
    for k in (total - take)..total {
        let (s, e) = ring[k % TAKE_MAX];
        out.put_bytes(ctx, &input[s..e]);
        out.put(ctx, "\n");
    }
}

/// `first [N] <path>` / `last [N] <path>` - print a file's first/last N lines (default 10). The
/// pipe form `<producer> | first [N]` takes from piped input; either way it consumes input.
fn cmd_take(ctx: &ShellCtx, cwd: &Cwd, args: &[&str], argc: usize, last: bool) -> Result<(), ShellError> {
    let name = if last { "last" } else { "first" };
    let (n, path) = parse_take(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln_fmt(format_args!("{}: a path is required (or pipe: <producer> | {} [N])", name, name));
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request_q(ctx, OP_READ_FILE, abspath, &[]) {
        ReqOutcome::Reply(r) => r,
        ReqOutcome::Aborted => return Ok(()),
        ReqOutcome::Timeout => { ctx.console_writeln_fmt(format_args!("{}: storage unavailable", name)); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let cnt = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + cnt).min(p.len());
        if last { write_last(ctx, &p[5..end], n, &mut Out::Console); }
        else    { write_first(ctx, &p[5..end], n, &mut Out::Console); }
        Ok(())
    } else {
        ctx.console_writeln_fmt(format_args!("{}: not found: {}", name, str_of(abspath)));
        Err(ShellError::FileNotFound)
    }
}

/// Bounded stack of directory paths still to visit during a `find` walk (§26.6). Pushing
/// past the cap sets `overflow` so `find` reports the truncation rather than silently
/// missing part of the tree (§3.12).
const FIND_QCAP: usize = 32;
struct PathStack {
    buf: [[u8; PATH_MAX]; FIND_QCAP],
    len: [usize; FIND_QCAP],
    top: usize,
    overflow: bool,
}
impl PathStack {
    fn new() -> Self {
        PathStack { buf: [[0u8; PATH_MAX]; FIND_QCAP], len: [0; FIND_QCAP], top: 0, overflow: false }
    }
    fn push(&mut self, p: &[u8]) {
        if self.top >= FIND_QCAP || p.len() > PATH_MAX {
            self.overflow = true;
            return;
        }
        self.buf[self.top][..p.len()].copy_from_slice(p);
        self.len[self.top] = p.len();
        self.top += 1;
    }
    fn pop(&mut self, out: &mut [u8; PATH_MAX]) -> Option<usize> {
        if self.top == 0 { return None; }
        self.top -= 1;
        let l = self.len[self.top];
        out[..l].copy_from_slice(&self.buf[self.top][..l]);
        Some(l)
    }
}

// ---------------------------------------------------------------------------
// drives - manage attached disks (utilities/15_drives.md). A shell built-in that
// sends the drives API to `fs` over IPC; `fs` holds and enforces all disk authority.
// Step 3: the data primitives `flash` / `label` / list (boot layer + multi-drive later).
// ---------------------------------------------------------------------------

fn cmd_drives(ctx: &ShellCtx, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let sub = if argc >= 2 { args[1] } else { "" };
    match sub {
        ""        => drives_list(ctx),
        "flash"   => {
            // `drives flash [drive] [label] [force]` - the drive selector is optional (one drive).
            // `force` overrides fs's refusal to overwrite a foreign/bootable disk. It is a word the
            // operator has to type; there is no default that destroys a boot medium. Recognised
            // ANYWHERE after the subcommand, not just last: `drives flash 0 data force` has to work as
            // well as `drives flash data force`, and requiring a fixed position silently dropped the
            // override in the selector form - which then refused a disk the operator had just forced.
            let force = args[2..argc].iter().any(|a| *a == "force");
            let mut kept = [""; MAX_ARGS];
            let mut n = 0usize;
            for a in args[..argc].iter() {
                if *a == "force" { continue; }
                kept[n] = a;
                n += 1;
            }
            let (sel, label) = split_drive_value(&kept, n);
            if drive_sel_ok(ctx, sel) { drives_flash(ctx, label, force) } else { Err(ShellError::Unknown) }
        }
        "label"   => {
            // `drives label [drive] <name>` - selector optional; name required.
            let (sel, name) = split_drive_value(args, argc);
            if name.is_empty() { ctx.console_writeln("usage: drives label [drive] <name>"); Err(ShellError::Unknown) }
            else if drive_sel_ok(ctx, sel) { drives_label(ctx, name) } else { Err(ShellError::Unknown) }
        }
        "reset"   => {
            // `drives reset [drive] [force]` - un-format back to raw (optional selector, no value).
            // `force` is recognised anywhere after the subcommand, as for `flash`.
            let force = args[2..argc].iter().any(|a| *a == "force");
            let sel = if argc >= 3 && args[2] != "force" { args[2] } else { "" };
            if drive_sel_ok(ctx, sel) { drives_reset(ctx, force) } else { Err(ShellError::Unknown) }
        }
        "check"   => {
            // `drives check [drive]` - fsck: verify CRCs + rebuild the bitmap/free count.
            let sel = if argc >= 3 { args[2] } else { "" };
            if drive_sel_ok(ctx, sel) { drives_check(ctx) } else { Err(ShellError::Unknown) }
        }
        "scrub"   => {
            // `drives scrub [drive]` - READ-ONLY integrity sweep: verify every block's CRC,
            // report, change nothing (unlike `check`, which repairs). Phase K.
            let sel = if argc >= 3 { args[2] } else { "" };
            if drive_sel_ok(ctx, sel) { drives_scrub(ctx) } else { Err(ShellError::Unknown) }
        }
        // `drives help` / `drives version` and `drives <sub> help` are handled by the
        // generic per-utility intercept in `execute` (0_conventions.md).
        other     => {
            ctx.console_writeln_fmt(format_args!("drives: unknown subcommand '{}'", other));
            util_help(ctx, "drives");
            Err(ShellError::Unknown)
        }
    }
}

/// Split the operands after `drives <sub>` into (optional drive selector, value). The
/// value is the LAST operand; an operand before it is the drive selector. So
/// `drives flash data` → ("", "data") and `drives flash 0 data` → ("0", "data").
fn split_drive_value<'a>(args: &[&'a str], argc: usize) -> (&'a str, &'a str) {
    match argc {
        n if n >= 4 => (args[2], args[3]),
        3           => ("", args[2]),
        _           => ("", ""),
    }
}

/// Validate a drive selector for the single attached drive (step 3). Accepts empty,
/// `0`, or a label; rejects a numeric index other than 0 with a teaching message.
fn drive_sel_ok(ctx: &ServiceContext, sel: &str) -> bool {
    if sel.is_empty() || sel == "0" {
        return true;
    }
    if sel.bytes().all(|b| b.is_ascii_digit()) {
        ctx.console_writeln_fmt(format_args!("drives: no drive {} - only drive 0 is attached", sel));
        return false;
    }
    true // a label selector - single drive, accept
}


/// `drives` - list the attached drive (single-drive in step 3; index 0).
fn drives_list(ctx: &ShellCtx) -> Result<(), ShellError> {
    drain_stale_fs_replies(ctx);   // start from a clean channel (see the fn: replies carry no request id)
    let reply = match fs_raw(ctx, &[OP_DRIVES_INFO], FS_ANSWER_SECS) {
        Some(r) => r,
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 28 {
        // See build_drives_table: report the reply's actual shape rather than asserting a cause.
        ctx.console_writeln_fmt(format_args!(
            "drives: unexpected reply from fs - status {} len {} (want status {}, len >= 28)",
            p.first().copied().unwrap_or(255), p.len(), FS_OK));
        return Err(ShellError::Unknown);
    }
    let mounted = p[1] != 0;
    // A capacity of ZERO is not a drive of size zero - it is NO DRIVE.
    //
    // `fs` was already reporting "capacity 0 sectors, mounted false" correctly and the shell printed
    // a row for it anyway - "0  -  raw  0 MiB  - not formatted -" - which reads as a blank disk that
    // is present rather than one that is absent. The service told the truth and the display
    // contradicted it.
    //
    // Checked here as well as in the device-first path, because this answer is ALWAYS available: it
    // needs no extra peer and no second query, so it holds even when the direct block-driver query
    // cannot be reached (which is exactly what happened on the Pi 4).
    if u64_le(&p[2..10]) == 0 {
        ctx.console_writeln("  #  LABEL        STATUS   SIZE");
        ctx.console_writeln("  (no drive(s) attached)");
        return Ok(());
    }
    let mib = u64_le(&p[2..10]) / 2048;
    ctx.console_writeln("  #  LABEL        STATUS   SIZE");
    if mounted {
        let total = u64_le(&p[10..18]);
        let next = u64_le(&p[18..26]);
        let free_mib = total.saturating_sub(next) / 2048;
        let ll = (p[27] as usize).min(LABEL_MAX);
        let label = core::str::from_utf8(&p[28..28 + ll]).unwrap_or("?");
        let label = if label.is_empty() { "-" } else { label };
        ctx.console_writeln_fmt(format_args!(
            "  0  {:<11}  GSFS     {} MiB ({} MiB free)", label, mib, free_mib));
    } else {
        ctx.console_writeln_fmt(format_args!(
            "  0  {:<11}  raw      {} MiB  - not formatted -", "-", mib));
    }
    Ok(())
}

/// `drives flash [label]` - format the drive as GSFS after a `[y/N]` confirm. Destructive.
/// `drives flash [label] [force]` - format the drive as GSFS.
///
/// `force` overrides `fs`'s refusal to overwrite a disk that already carries a foreign partition table
/// or boot sector. That refusal exists because a machine with a single storage device boots from the
/// very disk being formatted (the Raspberry Pi's SD card holds the firmware, the kernel image AND would
/// be the GSFS target), and a confirmation prompt cannot convey that - `drives` shows it as an
/// unformatted raw disk either way. So the danger is named explicitly and the override is a word the
/// operator has to type.
fn drives_flash(ctx: &ShellCtx, label: &str, force: bool) -> Result<(), ShellError> {
    if label.len() > LABEL_MAX {
        ctx.console_writeln_fmt(format_args!("drives: label too long (max {})", LABEL_MAX));
        return Err(ShellError::Unknown);
    }
    ctx.console_write("This ERASES the drive. Continue? [y/N] ");
    if !read_confirm(ctx) {
        ctx.console_writeln("drives: aborted");
        return Err(ShellError::Unknown); // the requested format did not happen
    }
    let lb = label.as_bytes();
    let ll = lb.len().min(LABEL_MAX);
    let mut req = [0u8; 2 + LABEL_MAX];
    // The force flag rides in the op byte's high bit, so the request format is otherwise unchanged.
    req[0] = if force { OP_FLASH | 0x80 } else { OP_FLASH };
    req[1] = ll as u8;
    req[2..2 + ll].copy_from_slice(&lb[..ll]);
    // Start from a clean channel. Without this, a reply abandoned by an earlier command is read as this
    // format's result - and because a 15 GB format takes MINUTES, the answer arrives instantly and says
    // "flash FAILED" while fs is still formatting. Observed exactly that on hardware: fs logged
    // `flash requested`, never logged a failure, and the shell had already declared one.
    drain_stale_fs_replies(ctx);
    match fs_raw(ctx, &req[..2 + ll], FS_FORMAT_SECS) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: formatted as GSFS - mounted, ready to use now (no reboot)");
            Ok(())
        }
        Some(r) if r.payload_bytes().first() == Some(&FS_FOREIGN) => {
            ctx.console_writeln("drives: REFUSED - block 0 holds a partition table or boot sector, so this");
            ctx.console_writeln("  disk is not blank. Formatting replaces whatever is on it, and if a machine");
            ctx.console_writeln("  boots from this disk it will stop booting until it is re-imaged.");
            ctx.console_writeln("  To format it anyway: drives flash [drive] <label> force");
            Err(ShellError::Unknown)
        }
        Some(_) => { ctx.console_writeln("drives: flash FAILED (no disk, or disk too small)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives reset` - un-format the drive back to raw (zero the superblock). Destructive;
/// a quick clean slate for re-testing the raw→flash path. NOT a secure wipe.
fn drives_reset(ctx: &ShellCtx, force: bool) -> Result<(), ShellError> {
    ctx.console_write("This un-formats the drive back to raw (ERASES). Continue? [y/N] ");
    if !read_confirm(ctx) {
        ctx.console_writeln("drives: aborted");
        return Err(ShellError::Unknown);
    }
    // Reset zeroes block 0, which on a foreign disk is its partition table - same danger as flash.
    let op = if force { OP_RESET | 0x80 } else { OP_RESET };
    drain_stale_fs_replies(ctx);   // start from a clean channel (see the fn: replies carry no request id)
    match fs_raw(ctx, &[op], FS_FORMAT_SECS) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: reset to raw - 'drives flash' to use again");
            Ok(())
        }
        Some(r) if r.payload_bytes().first() == Some(&FS_FOREIGN) => {
            ctx.console_writeln("drives: REFUSED - block 0 holds a foreign partition table or boot sector.");
            ctx.console_writeln("  Zeroing it would destroy that disk's boot record. If you are certain:");
            ctx.console_writeln("  drives reset force");
            Err(ShellError::Unknown)
        }
        Some(_) => { ctx.console_writeln("drives: reset FAILED (no disk?)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives check` - fsck: walk the tree (the source of truth), rebuild the free bitmap + free
/// count from it, and verify every block's CRC. Repairs allocation drift non-destructively;
/// reports (does not delete) files/dirs whose blocks fail their CRC. No confirmation needed -
/// it never erases data. Reply: [FS_OK, files:u32, dirs:u32, bad:u32, used:u64, free:u64].
fn drives_check(ctx: &ShellCtx) -> Result<(), ShellError> {
    // q-abortable: a whole-disk pass can run for minutes on a slow stick, and a shell parked in an
    // unbounded request cannot see the keystroke that asks it to stop (conventions rule 9).
    match fs_op_q(ctx, OP_CHECK) {
        ReqOutcome::Aborted => {
            ctx.console_writeln("drives: aborted (the filesystem finishes its pass in the background)");
            Err(ShellError::Unknown)
        }
        ReqOutcome::Reply(r) => {
            let p = r.payload_bytes();
            if no_fs(ctx, p) { return Err(ShellError::Unknown); }
            if p.first() == Some(&FS_OK) && p.len() >= 29 {
                let u32a = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
                let u64a = |o: usize| u64::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3], p[o+4], p[o+5], p[o+6], p[o+7]]);
                let (files, dirs, bad, used, free) = (u32a(1), u32a(5), u32a(9), u64a(13), u64a(21));
                ctx.console_writeln_fmt(format_args!(
                    "check: {} files, {} dirs, {} bad; {} blocks used, {} free (bitmap + free count rebuilt from the tree)",
                    files, dirs, bad, used, free));
                if bad > 0 {
                    ctx.console_writeln_fmt(format_args!(
                        "check: WARNING - {} file(s)/dir(s) had unreadable (CRC-failed) blocks; see the log", bad));
                    Err(ShellError::Unknown)
                } else {
                    ctx.console_writeln("check: ok - filesystem is consistent");
                    Ok(())
                }
            } else {
                ctx.console_writeln("check: FAILED"); Err(ShellError::Unknown)
            }
        }
        _ => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives scrub` - READ-ONLY integrity sweep (Phase K): walk the tree, verify every block's
/// CRC, report, change NOTHING on disk (distinct from `check`, which repairs the bitmap). Run it
/// on a schedule to catch latent bit-rot early; without redundancy it detects but cannot repair.
/// Reply: [FS_OK, files:u32, dirs:u32, bad:u32, scanned:u64].
fn drives_scrub(ctx: &ShellCtx) -> Result<(), ShellError> {
    // q-abortable: a whole-disk pass can run for minutes on a slow stick, and a shell parked in an
    // unbounded request cannot see the keystroke that asks it to stop (conventions rule 9).
    match fs_op_q(ctx, OP_SCRUB) {
        ReqOutcome::Aborted => {
            ctx.console_writeln("drives: aborted (the filesystem finishes its pass in the background)");
            Err(ShellError::Unknown)
        }
        ReqOutcome::Reply(r) => {
            let p = r.payload_bytes();
            if no_fs(ctx, p) { return Err(ShellError::Unknown); }
            if p.first() == Some(&FS_OK) && p.len() >= 21 {
                let u32a = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
                let u64a = |o: usize| u64::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3], p[o+4], p[o+5], p[o+6], p[o+7]]);
                let (files, dirs, bad, scanned) = (u32a(1), u32a(5), u32a(9), u64a(13));
                ctx.console_writeln_fmt(format_args!(
                    "scrub: verified {} blocks across {} files, {} dirs; {} bad (read-only, nothing changed)",
                    scanned, files, dirs, bad));
                if bad > 0 {
                    ctx.console_writeln_fmt(format_args!(
                        "scrub: WARNING - {} file(s)/dir(s) had CRC-failed blocks (bit-rot); the data is lost, see the log", bad));
                    Err(ShellError::Unknown)
                } else {
                    ctx.console_writeln("scrub: ok - every block verified");
                    Ok(())
                }
            } else {
                ctx.console_writeln("scrub: FAILED"); Err(ShellError::Unknown)
            }
        }
        _ => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives label <name>` - name / rename the drive (rewrites the superblock).
fn drives_label(ctx: &ShellCtx, name: &str) -> Result<(), ShellError> {
    let nb = name.as_bytes();
    if nb.is_empty() || nb.len() > LABEL_MAX {
        ctx.console_writeln_fmt(format_args!("drives: label must be 1..{} chars", LABEL_MAX));
        return Err(ShellError::Unknown);
    }
    let ll = nb.len();
    let mut req = [0u8; 2 + LABEL_MAX];
    req[0] = OP_LABEL;
    req[1] = ll as u8;
    req[2..2 + ll].copy_from_slice(nb);
    drain_stale_fs_replies(ctx);   // start from a clean channel (see the fn: replies carry no request id)
    match fs_raw(ctx, &req[..2 + ll], FS_ANSWER_SECS) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("drives: labelled '{}'", name));
            Ok(())
        }
        Some(_) => { ctx.console_writeln("drives: label FAILED (no filesystem? run 'drives flash' first)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// Read one line from the console and return true iff it begins with y/Y. The kernel
/// echoes keystrokes, so the user sees their answer; default (empty / anything else) is No.
fn read_confirm(ctx: &ServiceContext) -> bool {
    // Line-edited y/N: accept characters with BACKSPACE editing and decide on the FINAL line at Enter,
    // so a mistyped answer can be corrected - `y` then backspace then `n` reads as N, not the committed
    // `y`. console_read does not echo, so we echo each printable char and a destructive backspace erase
    // ourselves. Bounded, no heap (a confirm answer is tiny).
    let mut buf = [0u8; 8];
    let mut len = 0usize;
    loop {
        let b = ctx.console_read();
        match b {
            b'\r' | b'\n' => { ctx.console_writeln(""); break; }
            0x08 | 0x7f => { if len > 0 { len -= 1; ctx.console_write("\x08 \x08"); } }
            0x20..=0x7e => {
                if len < buf.len() {
                    buf[len] = b; len += 1;
                    if let Ok(s) = core::str::from_utf8(&[b]) { ctx.console_write(s); }
                }
            }
            0x1b => match read_escape_byte(ctx) {
                // Bare ESC (the Escape key) CANCELS - back to the prompt, like the main line editor's ESC.
                // read_escape_byte does not hang on a bare ESC (it times the wait off the TSC).
                None => { ctx.console_writeln(""); return false; }
                // A nav key (arrow / Home: ESC [ ... or ESC O ...) - a confirm does not navigate, so drain
                // the rest of the sequence (to its final byte, 0x40..=0x7e) and ignore it, so no stray bytes
                // leak into the answer. The sequence's bytes are already queued (atomic keyboard push).
                Some(b'[') | Some(b'O') => {
                    for _ in 0..8 { let c = ctx.console_read(); if (0x40..=0x7e).contains(&c) { break; } }
                }
                Some(_) => {} // ESC + a lone byte: ignore both, keep waiting for y/n
            }
            _ => {}
        }
    }
    len > 0 && (buf[0] == b'y' || buf[0] == b'Y')
}

fn u64_le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

// ---------------------------------------------------------------------------
// Helpers - no-alloc string building into stack buffers.
// ---------------------------------------------------------------------------

fn write_bytes(buf: &mut [u8], pos: &mut usize, src: &[u8]) {
    let space = buf.len().saturating_sub(*pos);
    let n = src.len().min(space);
    buf[*pos..*pos + n].copy_from_slice(&src[..n]);
    *pos += n;
}

fn write_u32(buf: &mut [u8], pos: &mut usize, n: u32) {
    let mut tmp = [0u8; 10];
    let s = u32_to_str(n, &mut tmp);
    write_bytes(buf, pos, s.as_bytes());
}

fn u32_to_str(n: u32, buf: &mut [u8; 10]) -> &str {
    if n == 0 {
        buf[0] = b'0';
        return core::str::from_utf8(&buf[..1]).unwrap_or("0");
    }
    let mut i = 10usize;
    let mut v = n;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    core::str::from_utf8(&buf[i..]).unwrap_or("?")
}

fn parse_u32(s: &str) -> Option<u32> {
    let mut n = 0u32;
    for b in s.bytes() {
        if b < b'0' || b > b'9' { return None; }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u32)?;
    }
    Some(n)
}
