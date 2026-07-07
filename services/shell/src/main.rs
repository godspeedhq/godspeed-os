// SPDX-License-Identifier: GPL-2.0-only
#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapInfo, CapHandle, Message, IpcError};
use godspeed_sdk::record::{Table, Value, RecordSink, parse_predicate, AggOp, AggErr, REC_MAX_ROWS, REC_ARENA};

const MAX_LINE: usize = 128;
const MAX_ARGS: usize = 4;

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
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3;
const FS_DENIED: u8 = 4; // file-cap op needs a right the cap lacks (non-escalation, §7.3)
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
// One pipe stage's buffer. 64 KiB so a producer (`tree /`, `find …`) can capture a large
// listing without being clipped at the buffer. NOTE this is no longer the *binding* limit:
// an IPC message is 4 KiB (PIPE_MSG_MAX) and a file is ~3.5 KiB, so a buffer wider than those
// can only flow to a sink that can take it (today: none beyond 4 KiB). Lifting those is the
// streaming/multi-block work (docs/pipes.md). The buffer lives on the user stack; two coexist
// for a middle filter (input + output ≈ 128 KiB), which fits the 256 KiB user stack.
const CAP_MAX: usize = 64 * 1024;
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
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
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

    wait_for_input_ready(&ctx);

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
    let mut nav = 0usize;
    // The previous command's result (the Ok/Err model), reported by `result`. Threaded as
    // local session state - no global (services hold no global mutable state, §3.9).
    let mut last_result: Result<(), ShellError> = Ok(());
    // When a foreground app (e.g. the `chaos` service, syscall 40) owns the console, the shell goes
    // "muted": it stays quiet (no prompt, no read) so it can neither smear that app's screen nor
    // swallow its `q`, and prints a fresh prompt only when it regains the keyboard. `muted` tracks it.
    let mut muted = false;

    loop {
        // Muted: a foreground app owns the console. Yield + skip - don't draw, don't blocking-read. The
        // Phase-1 kernel gate only covers the non-blocking poll, so THIS loop gate is what keeps the
        // main (blocking) read path from stealing the foreground app's `q`. v1 busy-yields while muted;
        // park + wake-on-release is a later optimization.
        if !ctx.is_console_foreground() {
            ctx.yield_cpu();
            muted = true;
            continue;
        }
        if muted { ctx.console_write("gsh> "); muted = false; } // regained the keyboard: a fresh prompt

        let b = ctx.console_read();

        match b {
            b'\r' | b'\n' => {
                // We own echo now, so move to a fresh line ourselves (the kernel used
                // to echo the Enter as "\r\n").
                ctx.console_write("\r\n");
                if line.len > 0 {
                    hist.push(line.bytes());
                    last_result = execute(&ctx, line.bytes(), &mut cwd, last_result, 0, &mut Out::Console);
                    line.len = 0;
                    line.cur = 0;
                }
                nav = hist.len();
                ctx.console_write("gsh> ");
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
                ctx.console_write("gsh> ");
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
    ctx: &ServiceContext,
    cwd: &mut Cwd,
    last_result: &mut Result<(), ShellError>,
    line: &mut Line,
) {
    ctx.console_write("\r\n");
    *last_result = execute(ctx, b"help", cwd, *last_result, 0, &mut Out::Console);
    ctx.console_write("gsh> ");
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
// ~100 ms at ~2 GHz, in read_tsc cycles. We time the bare-ESC wait off the TSC, not the
// kernel monotonic tick (query 12), because the tick was found NOT to advance reliably on
// real hardware (it silently broke typematic auto-repeat on the T630). read_tsc is
// hardware-proven (§22 perf). A real escape sequence's bytes are already queued (the
// keyboard pushes them atomically), so this wait only bounds how long a bare Escape - which
// has nothing following - takes to resolve to "clear the line".
const ESC_WAIT_CYCLES: u64 = 200_000_000;
fn read_escape_byte(ctx: &ServiceContext) -> Option<u8> {
    if let Some(b) = ctx.try_console_read() { return Some(b); }
    let deadline = ctx.read_tsc().wrapping_add(ESC_WAIT_CYCLES);
    while ctx.read_tsc() < deadline {
        if let Some(b) = ctx.try_console_read() { return Some(b); }
        ctx.yield_cpu();
    }
    None
}

/// Handle a CSI sequence (everything after `ESC [`). Reads the optional numeric
/// parameter and the final byte, then dispatches the key. Covers the arrows (history +
/// cursor), Home/End, and the `~`-terminated navigation keys (Insert/Delete/Home/End/
/// PageUp/PageDown) and function keys an extended keyboard sends. Unknown sequences are
/// consumed and ignored - never smeared onto the line. Bounded: a final byte must arrive
/// within `CSI_MAX` bytes or we stop (defensive against a malformed serial stream).
fn handle_csi(ctx: &ServiceContext, line: &mut Line, hist: &History, nav: &mut usize) {
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
            if *nav > 0 { *nav -= 1; line.set(ctx, hist.get(*nav)); }
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
fn complete_tab(ctx: &ServiceContext, line: &mut Line, cwd: &Cwd) {
    if line.len == 0 { return; }
    line.end(ctx);
    // Current token starts after the last space (or line start); its pipe segment starts after the
    // last '|' before it. Computed as plain indices so no borrow of `line` outlives the dispatch.
    let bytes = line.bytes();
    let tok_start = bytes.iter().rposition(|&b| b == b' ').map(|s| s + 1).unwrap_or(0);
    let seg_start = bytes[..tok_start].iter().rposition(|&b| b == b'|').map(|i| i + 1).unwrap_or(0);
    // The token is the segment's COMMAND if only spaces sit between the segment start and it.
    let is_command = bytes[seg_start..tok_start].iter().all(|&b| b == b' ');

    if is_command {
        complete_from_list(ctx, line, tok_start, UTILS);          // command name (after a `|` too)
    } else if !complete_keyword(ctx, line, seg_start, tok_start) {
        complete_path(ctx, line, cwd, tok_start);                 // not a keyword → file path
    }
}

/// Commands whose FIRST argument (the token right after the command, within its pipe segment) is a
/// fixed keyword - completed only at that position. Pipe-stage verbs (`to`/`from`/`sort`/`match`) are
/// here too, so `… | to j⇥` → `json` and `… | sort r⇥` → `reverse`. Keep in sync with each command's
/// argument parsing (verified against utilities/*.md + the `cmd_*` parsers).
const SUBCMD_FIRST: &[(&str, &[&str])] = &[
    ("observe", &["now"]),
    ("date",    &["epoch"]),
    ("net",     &["dns", "stats", "arp", "scan"]),
    ("drives",  &["flash", "label", "reset", "check", "scrub"]),
    ("chaos",   &["kill-storm", "flood-storm", "mem-pressure", "spawn-storm", "max-carnage"]),
    ("write",   &["append", "prepend"]),
    ("sort",    &["reverse"]),
    ("match",   &["except"]),
    ("to",      &["json", "yaml"]),
    ("from",    &["json"]),
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
    if "chaos".as_bytes() == cmd && prior == 1 && words.clone().next() == Some("max-carnage".as_bytes()) {
        const TARGETS: &[&str] =
            &["all-services", "supervisor", "block-driver", "fs", "logger", "xhci", "ehci", "shell"];
        return complete_from_list(ctx, line, tok_start, TARGETS);
    }

    // `ping [count N] [bytes N] <ip>`: the option keywords may appear in either order before the IP, so
    // complete them at ANY position where the token prefix-matches one not already used (not just first).
    if "ping".as_bytes() == cmd {
        const PING_OPTS: &[&str] = &["count", "bytes"];
        let mut avail = [""; 2];
        let mut a = 0usize;
        for &k in PING_OPTS {
            let used = head.split(|&b| b == b' ').any(|w| w == k.as_bytes());
            if !used && a < avail.len() { avail[a] = k; a += 1; }
        }
        return complete_from_list(ctx, line, tok_start, &avail[..a]);
    }

    if let Some((_, cands)) = SUBCMD_FIRST.iter().find(|(c, _)| c.as_bytes() == cmd) {
        // First-argument keyword only: a later arg is a path/value (e.g. `write append /f`), not a key.
        return prior == 0 && complete_from_list(ctx, line, tok_start, cands);
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
    ctx.console_write("gsh> ");
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
fn complete_path(ctx: &ServiceContext, line: &mut Line, cwd: &Cwd, tok_start: usize) {
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
fn path_menu(ctx: &ServiceContext, line: &mut Line, base_len: usize, rbuf: &[u8; 512], hits: &[PathHit]) {
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
    ctx.console_write("gsh> ");
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
struct History {
    lines: [[u8; MAX_LINE]; HIST_MAX],
    lens: [usize; HIST_MAX],
    n: usize,
}
impl History {
    fn new() -> Self {
        History { lines: [[0u8; MAX_LINE]; HIST_MAX], lens: [0; HIST_MAX], n: 0 }
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
    fn redraw_tail(&self, ctx: &ServiceContext) {
        if self.cur < self.len {
            ctx.console_write(core::str::from_utf8(&self.buf[self.cur..self.len]).unwrap_or(""));
        }
        ctx.console_write("\x1b[K"); // erase whatever the old (possibly longer) tail left
        for _ in self.cur..self.len { ctx.console_write("\x08"); }
    }

    /// Insert a printable byte at the cursor.
    fn insert(&mut self, ctx: &ServiceContext, b: u8) {
        if self.len >= MAX_LINE { return; }
        let mut i = self.len;
        while i > self.cur { self.buf[i] = self.buf[i - 1]; i -= 1; }
        self.buf[self.cur] = b;
        self.len += 1;
        self.cur += 1;
        // Echo the inserted byte, then redraw the shifted tail behind it.
        let s = [b];
        ctx.console_write(core::str::from_utf8(&s).unwrap_or(""));
        self.redraw_tail(ctx);
    }

    /// Delete the character before the cursor (Backspace).
    fn backspace(&mut self, ctx: &ServiceContext) {
        if self.cur == 0 { return; }
        for i in self.cur..self.len { self.buf[i - 1] = self.buf[i]; }
        self.len -= 1;
        self.cur -= 1;
        ctx.console_write("\x08"); // step left onto the deleted cell
        self.redraw_tail(ctx);
    }

    /// Delete the character at the cursor (the Delete key - forward delete).
    fn delete(&mut self, ctx: &ServiceContext) {
        if self.cur >= self.len { return; }
        for i in (self.cur + 1)..self.len { self.buf[i - 1] = self.buf[i]; }
        self.len -= 1;
        self.redraw_tail(ctx);
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
        while self.cur > 0 { self.cur -= 1; ctx.console_write("\x08"); }
        ctx.console_write("\x1b[K");
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
fn wait_for_input_ready(ctx: &ServiceContext) {
    const MAX_SPINS: u32 = 50_000_000;
    for _ in 0..MAX_SPINS {
        if ctx.input_ready() {
            return;
        }
        ctx.yield_cpu();
    }
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
fn execute(ctx: &ServiceContext, line: &[u8], cwd: &mut Cwd, prev: Result<(), ShellError>, depth: u8, out: &mut Out) -> Result<(), ShellError> {
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
            // after the path are the script's params ($1.., $@, $#); $0 is the path.
            let save = if argc >= 4 && args[2] == "save" { Some(args[3]) } else { None };
            let params = if save.is_some() { Params::empty(args[1]) } else { parse_params(s, args[1], 2) };
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
        "mem"     => cmd_mem(ctx, out),
        "cores"   => cmd_cores(ctx, out),
        "date"    => cmd_date(ctx, if argc >= 2 { args[1] } else { "" }, out),
        "net"     => cmd_net(ctx, s["net".len()..].trim(), out),
        "ping"    => cmd_ping(ctx, s["ping".len()..].trim(), out),
        "sock"    => cmd_sock(ctx, out),
        "uptime"  => cmd_uptime(ctx),
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
            if argc < 2 { ctx.console_writeln("usage: spawn <name>"); Err(ShellError::Unknown) }
            else { cmd_spawn(ctx, args[1]) }
        }
        // Phase-0 naming-migration diagnostics (docs/naming-design.md).
        "spawncap" => {
            if argc < 2 { ctx.console_writeln("usage: spawncap <name>"); Err(ShellError::Unknown) }
            else { cmd_spawncap(ctx, args[1]) }
        }
        "spawnwired" => cmd_spawnwired(ctx),
        "kill"    => {
            if argc < 2 { ctx.console_writeln("usage: kill <name>"); Err(ShellError::Unknown) }
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
fn cmd_run(ctx: &ServiceContext, cwd: &mut Cwd, arg: &str, depth: u8, save: Option<&str>, params: &Params) -> Result<(), ShellError> {
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
fn stream_minify(ctx: &ServiceContext, path: &[u8], dst: &mut [u8]) -> (usize, bool) {
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
fn resolve_one_import(ctx: &ServiceContext, stmt: &[u8], is_from: bool, script: &mut [u8], code: &mut usize) {
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
fn resolve_imports(ctx: &ServiceContext, script: &mut [u8], code: &mut usize) {
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
enum VarErr { TableFull, ArenaFull, NameTooLong, Redeclare, Undeclared, Immutable, ValueTooLong }

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
    }
}

/// A gsh run's parameters: `$0` (script name), `$1..$9`, `$@` (all), `$#` (count). Zero-copy - the
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
/// then collect up to `PARAM_MAX` quote-aware tokens. `name` becomes `$0`.
fn parse_params<'a>(line: &'a str, name: &'a str, skip: usize) -> Params<'a> {
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
    match b[j] {
        b'@' => { for k in 0..params.argc { if k > 0 { out.push(b' '); } out.push_bytes(params.argv[k].as_bytes()); } return Ok(j + 1); }
        b'#' => { out.push_u32(params.argc as u32); return Ok(j + 1); }
        b'0' => { out.push_bytes(params.name.as_bytes()); return Ok(j + 1); }
        b'1'..=b'9' => {
            let idx = (b[j] - b'1') as usize;
            if idx >= params.argc {
                ctx.console_writeln_fmt(format_args!("gsh: ${} not provided ($# = {})", (b[j] - b'0') as u32, params.argc));
                return Err(());
            }
            out.push_bytes(params.argv[idx].as_bytes()); return Ok(j + 1);
        }
        _ => {}
    }
    let start = j;
    let mut k = j;
    while k < b.len() && (b[k] == b'_' || b[k].is_ascii_alphanumeric()) { k += 1; }
    if k == start { ctx.console_writeln("gsh: '$' must be followed by a name, digit, @ or #"); return Err(()); }
    let name = &b[start..k];
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
fn valid_var_name(name: &str) -> bool {
    let b = name.as_bytes();
    if b.is_empty() || b.len() > VAR_NAME_MAX { return false; }
    if !(b[0] == b'_' || b[0].is_ascii_alphabetic()) { return false; }
    b.iter().all(|&c| c == b'_' || c.is_ascii_alphanumeric())
}

/// `let [mut] <name> = <value>` - declare a binding.
fn stmt_let(ctx: &ServiceContext, cwd: &Cwd, rest: &str, vars: &mut Vars, params: &Params) -> Result<(), ShellError> {
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
fn stmt_reassign(ctx: &ServiceContext, cwd: &Cwd, name: &str, value: &str, vars: &mut Vars, params: &Params) -> Result<(), ShellError> {
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
fn run_stmt(ctx: &ServiceContext, cwd: &mut Cwd, stmt: &str, prev: Result<(), ShellError>, depth: u8, vars: &mut Vars, params: &Params, out: &mut Out) -> StmtOutcome {
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
fn eval_cond(ctx: &ServiceContext, cwd: &mut Cwd, cond: &str, vars: &Vars, params: &Params, prev: Result<(), ShellError>, depth: u8) -> bool {
    let cond = cond.trim();
    if cond.is_empty() { ctx.console_writeln("gsh: empty condition"); return false; }
    if let Some(rest) = cond.strip_prefix('!') {
        return !eval_cond(ctx, cwd, rest.trim(), vars, params, prev, depth);
    }
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
/// PARAMS (`$@`). The advancing state lives in the frame - no materialized list (a big `range` never
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
fn handle_if(b: &[u8], mut pos: usize, ctx: &ServiceContext, cwd: &mut Cwd, vars: &Vars, params: &Params, prev: Result<(), ShellError>, depth: u8, ft: &FnTable) -> Step {
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
fn fmt_stream_pass(ctx: &ServiceContext, path: &[u8], emit: &mut dyn FnMut(&[u8]) -> bool) -> Result<(), FmtErr> {
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

// ── Loops (§5): `for <var> in <words|range|$@> { … }` and unbounded `loop { … }`. ──

/// Parse the source of a `for` (the text between `in` and `{`) into an iterator: `range N` / `range A
/// B` counts; `$@` alone walks the params; anything else is a whitespace-separated word list (each
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
    } else if trim_bytes(&b[rest_start..rest_end]) == b"$@" {
        ForIter::Params { idx: 0 }
    } else {
        ForIter::Words { pos: rest_start, end: rest_end }
    }
}

/// Advance a `for` iterator by one: if a next item exists, set the loop var (`var`) to it and return
/// the advanced iterator; else `None` (loop done). Words are `$`-expanded in the current scope.
fn for_step(ctx: &ServiceContext, b: &[u8], vars: &mut Vars, var: usize, it: ForIter, params: &Params) -> Option<ForIter> {
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
fn forlines_step(ctx: &ServiceContext, vars: &mut Vars, var: usize, off: u32, id: u32) -> Option<ForIter> {
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
fn forlines_capture(ctx: &ServiceContext, cwd: &Cwd, inner: &str, temp: &[u8]) -> Result<(), ()> {
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
fn run_defers(ctx: &ServiceContext, cwd: &mut Cwd, b: &[u8], defers: &mut [(usize, usize, usize)], ndefer: &mut usize, min_depth: usize, vars: &mut Vars, params: &Params, out: &mut Out, sdepth: u8) {
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

fn run_lines(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, out: &mut Out, params: &Params) -> Result<(), ShellError> {
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
        // a `for` loop: for <var> in <words | range N | range A B | $@> { body }
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
            // is the existing range / $@ / word-list.
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
        // Echo the statement so the transcript shows what produced each result.
        out.put(ctx, "> ");
        out.line(ctx, s);
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
    out.line(ctx, "--- summary ---");
    let shown = nrec.min(RUN_MAX_CMDS);
    for j in 0..shown {
        out.put(ctx, if !verdict[j] { "FAIL  " } else { "PASS  " });
        out.line(ctx, str_of(&b[soff[j] as usize..soff[j] as usize + slng[j] as usize]));
    }
    out.line_fmt(ctx, format_args!("run: ran {}, failed {}", ran, failed));
    if failed == 0 { Ok(()) } else { Err(ShellError::Unknown) }
}

/// Run `src` and, if `save` is `Some`, stream the report to that file (the utility writes its own
/// file - direct, not a pipe). Bare → report to the console. Shared by `run`/`selfcheck`. This
/// dispatcher is tiny on purpose: the 32 KiB `ReportBuf` lives ONLY in `run_and_save`, called only
/// on the save path - so a bare run/selfcheck does NOT carry 32 KiB of unused frame (which would
/// tip its already-heavy `| assert` sub-pipelines over the user-stack ceiling).
fn run_with_optional_save(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, save: Option<&str>, params: &Params)
    -> Result<(), ShellError>
{
    match save {
        None => run_lines(ctx, cwd, src, depth, &mut Out::Console, params),
        Some(spath) => run_and_save(ctx, cwd, src, depth, spath, params),
    }
}

/// The save path: accumulate the run report into a bounded `ReportBuf` and write it to `spath`
/// (direct file write, no pipe). `#[inline(never)]` so the 32 KiB buffer exists only while a save
/// is actually running, not in the frame of every bare run.
#[inline(never)]
fn run_and_save(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, spath: &str, params: &Params)
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
        run_lines(ctx, cwd, src, depth, &mut out, params)
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
fn save_report(ctx: &ServiceContext, path: &[u8], data: &[u8]) -> bool {
    // Bounded fs request (wall-clock): a chaos report is saved right after the storm may have hammered
    // fs, so the write must time out gracefully rather than hang the shell (the max-carnage aggregate
    // report is small → this single-message path).
    if data.len() <= IO_CHUNK {
        return matches!(fs_request_bounded(ctx, OP_WRITE_FILE, path, data)
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

/// `selfcheck` - run the embedded self-check suite IN MEMORY (straight from rodata via
/// `run_lines`; no file write, so it is not capped by `MAX_FILE_BYTES`). The one-USB hardware
/// checkpoint - flash the boot image, (`drives flash` a drive if it's raw, so the file-command
/// tests have somewhere to write), then `selfcheck`. Re-runnable (the suite creates and deletes
/// its own files). Refused inside a script (it runs one - no nesting).
#[inline(never)]
fn cmd_selfcheck(ctx: &ServiceContext, cwd: &mut Cwd, depth: u8, arg: &str) -> Result<(), ShellError> {
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
fn cmd_assert(ctx: &ServiceContext, cwd: &mut Cwd, rest: &str, depth: u8) -> Result<(), ShellError> {
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

const UTIL_VERSION: &str = "0.1.0";

/// Utilities that self-document (gates the `help`/`version` intercept in `execute`).
const UTILS: &[&str] = &[
    "help", "result", "run", "assert", "selfcheck",
    "echo", "input", "clear", "about", "mem", "cores", "date", "net", "ping", "sock", "uptime", "status", "observe", "caps", "roster",
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
    ctx.console_writeln_fmt(format_args!("{} {} - {}", title, UTIL_VERSION, desc));
    ctx.console_writeln("");
    ctx.console_writeln("usage:");
    for (sig, d, ex) in rows {
        ctx.console_writeln_fmt(format_args!("  {:<28}  {}", sig, d));
        if !ex.is_empty() {
            ctx.console_writeln_fmt(format_args!("      e.g. {}", ex));
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
            ("about", "name, core count, creator", "about"),
        ], true),
        "mem" => help_block(ctx, "mem", "physical memory usage", &[
            ("mem", "used / total / free physical memory", "mem"),
        ], true),
        "cores" => help_block(ctx, "cores", "CPU core count", &[
            ("cores", "how many CPU cores are up", "cores"),
        ], true),
        "date" => help_block(ctx, "date", "date + time from the hardware clock", &[
            ("date", "full timestamp (weekday date time)", "date"),
            ("date epoch", "seconds since 1970-01-01", "date epoch"),
        ], true),
        "net" => help_block(ctx, "net", "network status, DNS, and ARP host discovery", &[
            ("net", "IP, gateway (+MAC), and whether the gateway pings", "net"),
            ("net dns <host>", "resolve a hostname to an IPv4 address", "net dns example.com"),
            ("net stats", "dump the NIC's raw registers (chip state: RE/RCR/RX ring)", "net stats"),
            ("net arp <ip>", "resolve one host's MAC by ARP", "net arp 192.168.4.1"),
            ("net scan", "ARP-sweep the local /24 for live hosts", "net scan"),
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
            ("caps [service] | <verb>", "piped: records resource/rights", "caps logger | where rights~send"),
        ], true),
        "spawn" => help_block(ctx, "spawn", "start a service", &[
            ("spawn <name>", "start the service <name>", "spawn pong"),
        ], true),
        "kill" => help_block(ctx, "kill", "stop a service", &[
            ("kill <name>", "stop the running service <name>", "kill pong"),
        ], true),
        "restart" => help_block(ctx, "restart", "restart a service", &[
            ("restart <name>", "restart (re-placed per contract)", "restart pong"),
            ("restart <name> <core>", "restart on core <core>", "restart pong 2"),
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
            ("chaos max-carnage <all-services|svc> [n]", "the chaos monkey: storm RANDOM services (all-services) or aim every round at ONE (e.g. fs), under system-wide mem-pressure + spawn-storm; proves the KERNEL survives. 'q' aborts (via SERIAL if it storms the USB keyboard drivers)", "chaos max-carnage all-services 50"),
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
            ("<records> | where <col><op><val>", "ops: = != > < >= <= ~ (contains)", "status | where mem>0"),
            ("… | where state=BlockRecv", "textual when either side is non-numeric", "status | where state=BlockRecv"),
        ], true),
        "select" => help_block(ctx, "select", "keep only some columns, in order (record-pipe stage)", &[
            ("<records> | select <col> [col…]", "project the named columns", "status | select name core state"),
        ], true),
        "to" => help_block(ctx, "to", "render records to a format (record-pipe stage)", &[
            ("<records> | to json", "JSON array of objects", "status | to json"),
            ("<records> | to yaml", "YAML list of mappings", "status | where mem>0 | to yaml"),
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
        ("net", "dns") => help_block(ctx, "net dns", "resolve a hostname to an IPv4 address", &[
            ("net dns <host>", "DNS A-record lookup via net-stack (slirp resolver)", "net dns example.com"),
        ], false),
        ("net", "arp") => help_block(ctx, "net arp", "resolve one host's MAC by ARP", &[
            ("net arp <ip>", "broadcast a who-has and print the responder's MAC", "net arp 192.168.4.1"),
        ], false),
        ("net", "scan") => help_block(ctx, "net scan", "ARP-sweep the local /24 for live hosts", &[
            ("net scan", "list every host on your /24 that answers ARP", "net scan"),
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
    Row("about", "identity + credits"),
    Row("cores", "CPU core count"),
    Row("mem", "physical memory usage"),
    Row("date [epoch]", "date + time; 'epoch' = secs since 1970"),
    Row("uptime", "how long the system has been up (records when piped)"),
    Row("net", "network status: IP, gateway, ping"),
    Row("ping", "continuous ICMP echo (q quits): ping 8.8.8.8"),
    Gap,
    Sec("Services"),
    Row("status", "list all live tasks"),
    Row("observe [now]", "live view (q to quit) / one-shot frame"),
    Row("caps [service]", "capabilities (default: this shell)"),
    Row("roster", "example record service (a typed table; try roster | where role=core)"),
    Row("spawn <name>", "start a service"),
    Row("kill <name>", "stop a service"),
    Row("restart <name> [core]", "restart a service"),
    Gap,
    Sec("Storage"),
    Row("drives [flash|label|reset|check]", "manage attached disks (drives help)"),
    Row("ls [path]", "list a directory"),
    Row("cd [path|-]", "change directory (- = previous)"),
    Row("read <path>", "print a file"),
    Row("write [append|prepend] <path>", "create/overwrite/append/prepend (also: <prod> | write …)"),
    Row("edit <path>", "full-screen text editor (^S save, ^Q quit)"),
    Row("mkdir <path> [parents]", "create a directory"),
    Row("copy <src> <dst> [recursive]", "copy a file or subtree"),
    Row("move <src> <dst>", "relocate a file/dir"),
    Row("rename <path> <name>", "rename an entry in place"),
    Row("delete <path> [recursive]", "remove a file/dir/subtree"),
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
    if depth > 0 || rows == 0 || total <= rows {
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
    let page = rows.saturating_sub(1).max(1); // leave one row for the status line
    let max_top = total.saturating_sub(page);
    let mut top = 0usize;
    ctx.console_write("\x1b[?25l"); // hide the cursor for the whole pager session
    loop {
        ctx.console_write("\x1b[H"); // home - repaint over the old frame, no clear-to-black
        let end = (top + page).min(total);
        for i in top..end { help_render_line(ctx, i, true); }
        // Status line (no trailing newline so it parks at the bottom). Scroll keys lead,
        // since holding Up/Down scrolls smoothly (typematic auto-repeat). ESC[J after it
        // wipes any rows left over from a taller previous frame (e.g. the short last page).
        ctx.console_write_fmt(format_args!(
            "[ lines {}-{} of {} ]  up/down: scroll  space: next page  g/G: top/end  q: quit",
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
    out.line(ctx, "GodspeedOS: a capability-based microkernel (v1 milestone)");
    out.line_fmt(ctx, format_args!("  running on {} core(s)", ctx.inspect_core_count()));
    out.line(ctx, "  Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.");
    Ok(())
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

fn cmd_reboot(ctx: &ServiceContext) -> ! {
    ctx.console_writeln("rebooting...");
    ctx.reboot()
}

fn cmd_cores(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    out.line_fmt(ctx, format_args!("cores: {}", ctx.inspect_core_count()));
    Ok(())
}

/// Wall-clock date+time from the hardware RTC. Default renders a full timestamp
/// with weekday, e.g. `Sat 2026-06-06 22:05:09`. `date epoch` prints seconds since
/// 1970-01-01 instead. Deliberately just these two forms - no clock-setting, format
/// strings, or timezones (§26.2: minimal surface). The subcommand is `epoch`, not
/// `unix`: this is not POSIX, so the vocabulary doesn't borrow its name.
fn cmd_date(ctx: &ServiceContext, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let dt = ctx.datetime();
    if arg == "epoch" {
        out.line_fmt(ctx, format_args!("{}", dt.epoch_secs()));
    } else {
        let wd = WEEKDAYS[(dt.weekday() as usize) % 7];
        out.line_fmt(ctx, format_args!(
            "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            wd, dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second));
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
        let reply = match ctx.request_with_reply("net-stack", &msg) {
            Some(r) => Some(r),
            None => if ctx.reacquire_by_name("net-stack") { ctx.request_with_reply("net-stack", &msg) } else { None },
        };
        match reply {
            Some(r) => {
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
                } else {
                    out.line_fmt(ctx, format_args!("Request timed out."));
                }
            }
            None => { out.line_fmt(ctx, format_args!("ping: net-stack unavailable")); break; }
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
    Ok(())
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
        for i in 0..4 {
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
    if !arg.is_empty() {
        ctx.console_writeln("net: unknown subcommand - try net, net dns <host>, net stats, net arp <ip>, net scan, or net help");
        return Err(ShellError::Unknown);
    }
    net_status(ctx, out)
}

/// `net arp <ip>` - resolve one host's hardware address by ARP (net-stack op 6).
fn net_arp(ctx: &ServiceContext, ip_str: &str, out: &mut Out) -> Result<(), ShellError> {
    let ip = match parse_ipv4(ip_str) {
        Some(ip) => ip,
        None => { out.line_fmt(ctx, format_args!("net arp: '{}' is not an IPv4 address", ip_str)); return Ok(()); }
    };
    let req = Message::from_bytes(&[6, ip[0], ip[1], ip[2], ip[3]]);
    let reply = match ctx.request_with_reply("net-stack", &req) {
        Some(r) => Some(r),
        None => if ctx.reacquire_by_name("net-stack") { ctx.request_with_reply("net-stack", &req) } else { None },
    };
    match reply {
        Some(r) => {
            let p = r.payload_bytes();
            if p.first() == Some(&1) && p.len() >= 7 {
                out.line_fmt(ctx, format_args!("{}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    ip[0], ip[1], ip[2], ip[3], p[1], p[2], p[3], p[4], p[5], p[6]));
            } else {
                out.line_fmt(ctx, format_args!("{}.{}.{}.{}: no ARP reply (not on this subnet, or down)", ip[0], ip[1], ip[2], ip[3]));
            }
        }
        None => out.line_fmt(ctx, format_args!("net: net-stack did not answer the ARP query")),
    }
    Ok(())
}

/// `net scan` - ARP-sweep the local /24 (derived from our own IP) and list the hosts that answer.
/// ARP-based, so it is fast and LAN-reliable. net-stack does the whole sweep in one op (op 7) and
/// returns a 32-byte up-bitmap - one round trip per host, not a per-host poll from the shell.
fn net_scan(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    let our = match ctx.request_with_reply("net-stack", &Message::from_bytes(&[0u8])) {
        Some(r) => { let p = r.payload_bytes(); if p.len() >= 4 { [p[0], p[1], p[2], p[3]] } else { [0u8; 4] } }
        None => { out.line_fmt(ctx, format_args!("net: net-stack unavailable")); return Ok(()); }
    };
    out.line_fmt(ctx, format_args!("Scanning {}.{}.{}.0/24 for live hosts (a few seconds):", our[0], our[1], our[2]));
    // The sweep runs inside net-stack (~one round trip per host); block for the single reply.
    let up = match ctx.request_with_reply("net-stack", &Message::from_bytes(&[7u8])) {
        Some(r) => { let p = r.payload_bytes(); let mut b = [0u8; 32]; if p.len() >= 32 { b.copy_from_slice(&p[..32]); } b }
        None => { out.line_fmt(ctx, format_args!("net: net-stack did not answer the scan")); return Ok(()); }
    };
    let mut found = 0u32;
    for x in 1..=254u16 {
        if up[(x >> 3) as usize] & (1 << (x & 7)) != 0 {
            out.line_fmt(ctx, format_args!("  {}.{}.{}.{}", our[0], our[1], our[2], x));
            found += 1;
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
        NetQ::Aborted    => { ctx.console_writeln("net: aborted"); return Ok(()); }
        NetQ::Timeout    => { ctx.console_writeln("net: net-stack did not answer the resolve"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if p.len() >= 5 && p[0] == 1 {
        out.line_fmt(ctx, format_args!("{} is {}.{}.{}.{}", host, p[1], p[2], p[3], p[4]));
    } else if p.first() == Some(&2) {
        out.line_fmt(ctx, format_args!("{}: the DNS server replied but returned no A record", host));
    } else {
        // Diagnostic: how many frames net-stack collected while waiting, and how many were UDP. Tells
        // us "no reply arrived" (0 UDP) from "a reply arrived but did not match our port" (UDP > 0).
        let (fr, ud) = if p.len() >= 7 { (p[5], p[6]) } else { (0, 0) };
        let to = if p.len() >= 8 { p[7] } else { 0 };
        out.line_fmt(ctx, format_args!("{}: no reply from the DNS server ({} frames, {} UDP, {} timeouts)", host, fr, ud, to));
    }
    Ok(())
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
    for i in 0..=max_secs {
        while let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' || b == 0x1b { return NetQ::Aborted; }
        }
        if let Some(r) = ctx.request_with_reply_deadline(peer, msg, 1) { return NetQ::Reply(r); }
        // Only tell the user about q if the reply DIDN'T come in the first second (a stall) - so a fast
        // query stays clean, but a wedged one advertises how to escape it.
        if i == 0 { ctx.console_writeln("net: waiting for a reply - press q to abort"); }
        ctx.reacquire_by_name(peer);
    }
    NetQ::Timeout
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
    if p.len() >= 19 {
        out.line_fmt(ctx, format_args!("dns      {}.{}.{}.{}", p[15], p[16], p[17], p[18]));
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
    let self_grant = ctx.self_grant_handle()?;
    let reply = ctx.derive_cap(self_grant)?;
    if ctx.resource_invoke(sock, right, reply, &Message::from_bytes(payload)).is_err() {
        ctx.remove_cap(reply);
        return None;
    }
    Some(ctx.recv())
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
    matches!(name, "status" | "ls" | "caps" | "drives" | "find" | "observe" | "uptime")
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
fn build_ls_table(ctx: &ServiceContext, cwd: &Cwd, arg: &str) -> Option<Table> {
    let mut buf = [0u8; PATH_MAX];
    let path = resolve_or_err(ctx, cwd, arg, &mut buf)?;
    let reply = match fs_request(ctx, OP_LIST_DIR, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("ls: storage unavailable"); return None; }
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
fn build_drives_table(ctx: &ServiceContext) -> Option<Table> {
    let reply = match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_DRIVES_INFO])) {
        Some(r) => r,
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); return None; }
    };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 28 {
        ctx.console_writeln("drives: no disk found");
        return None;
    }
    let mounted = p[1] != 0;
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
fn build_find_table(ctx: &ServiceContext, cwd: &Cwd, arg: &str) -> Option<Table> {
    let (target, start) = split_first(arg);
    if target.is_empty() { ctx.console_writeln("usage: find <name> [path]"); return None; }
    let start = if start.is_empty() { "/" } else { start };
    let mut sbuf = [0u8; PATH_MAX];
    let start_abs = resolve_or_err(ctx, cwd, start, &mut sbuf)?;
    let mut stack = PathStack::new();
    stack.push(start_abs);
    let tb = target.as_bytes();
    let is_glob = tb.iter().any(|&b| b == b'*' || b == b'?');
    let mut t = Table::new(&["name", "type", "path"]);
    let mut dir = [0u8; PATH_MAX];
    while let Some(dlen) = stack.pop(&mut dir) {
        let reply = match fs_request(ctx, OP_LIST_DIR, &dir[..dlen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("find: storage unavailable"); return None; }
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
            i += nl + 1 + 8;
            let mut child = [0u8; PATH_MAX];
            if let Some(clen) = join_path(&dir[..dlen], name, &mut child) {
                let hit = if is_glob { glob_match(tb, name) } else { contains(name, tb) };
                if hit {
                    let nv = t.intern(name);
                    let tv = t.intern(if is_dir { b"dir" } else { b"file" });
                    let pv = t.intern(&child[..clen]);
                    t.add_row(&[nv, tv, pv]);
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
fn pipe_run(ctx: &ServiceContext, cwd: &Cwd, line: &str, out: &mut Out) -> Result<(), ShellError> {
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
                    _ => { ctx.console_writeln("to: unknown format (try: to json | to yaml)"); return false; }
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
        for _ in 0..1_000_000u32 {
            ctx.yield_cpu();
            let st = ctx.task_stat(slot);
            // state 2 = BlockedOnRecv → finished printing; invalid → gone.
            if !st.valid || st.state == 2 {
                break;
            }
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
            // Poll `q` by YIELDING, not ctx.sleep. The tick-based sleep converts through the TSC
            // calibration, which is WRONG on the AMD T630 (CPUID exposes no usable TSC frequency) - a
            // ctx.sleep there can stretch to many seconds, so `q` appeared dead and the user had to
            // reboot. yield_cpu polls every scheduler round on ANY hardware; the painter is on another
            // core, so this does not starve it. Drain the console; quit on q/Q. Echo is off (child owns it).
            ctx.yield_cpu();
            let mut quit = false;
            while let Some(b) = ctx.try_console_read() {
                if b == b'q' || b == b'Q' { quit = true; }
            }
            if quit { break; }
            if !ctx.task_stat(slot).valid { break; } // child died unexpectedly
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

/// Shown when spawn/kill/restart targets a core service - "Not applicable" makes
/// it clear the command is refused *because* the target is protected, not failed.
/// Lists exactly `CORE_SERVICES` (just `supervisor`).
const PROTECTED_MSG: &str =
    "Not applicable. The supervisor is protected (the recovery authority); storm it deliberately via 'chaos kill-storm supervisor'";

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

/// Services the live console session depends on for I/O. Killing/restarting them
/// from the shell would brick the very session issuing the command - a USB host
/// driver (`xhci`/`ehci`, which carry whatever input devices are attached) or the
/// shell itself. Returns the reason to show, or `None` if `name` is safe to
/// operate on. (Not a §6.2 trusted-root guard - these are restartable in
/// principle, just not from the session that needs them.)
fn session_critical_msg(name: &str) -> Option<&'static str> {
    match name {
        "xhci"  => Some("Not applicable. xhci is a USB host driver - killing it disables any input device attached to it"),
        "ehci"  => Some("Not applicable. ehci is a USB host driver - killing it disables any input device attached to it"),
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
    // Drain the service's output until EOT (bounded - a conforming service always sends it).
    for _ in 0..512 {
        let msg = ctx.recv();
        let p = msg.payload_bytes();
        if p == [PIPE_EOT] { break; }
        out.push(p);
        if out.overflow { break; }
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
                 | "about" | "mem" | "cores" | "date" | "net" | "ping" | "sock" | "help")
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
fn run_producer(ctx: &ServiceContext, cwd: &Cwd, cmdline: &str, out: &mut Out) {
    let (cmd, arg) = split_first(cmdline);
    match cmd {
        "echo"         => { let _ = cmd_echo(ctx, arg, out); }
        "read"         => { let _ = cmd_read(ctx, cwd, arg, out); }
        // "ls" and "find" are record producers (handled on the record path), not text here.
        "tree"         => { let _ = cmd_tree(ctx, cwd, arg, out); }
        // Info/display commands - text emitters, capturable to a file.
        "about"        => { let _ = cmd_about(ctx, out); }
        "mem"          => { let _ = cmd_mem(ctx, out); }
        "cores"        => { let _ = cmd_cores(ctx, out); }
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
fn run_captured(ctx: &ServiceContext, cwd: &Cwd, inner: &str, out: &mut Out) -> bool {
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
fn capture_define(ctx: &ServiceContext, cwd: &Cwd, name: &str, inner: &str, mutable: bool, vars: &mut Vars) -> Result<(), ShellError> {
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
fn capture_reassign(ctx: &ServiceContext, cwd: &Cwd, name: &str, inner: &str, vars: &mut Vars) -> Result<(), ShellError> {
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
fn read_file_exact(ctx: &ServiceContext, path: &[u8], off: usize, out: &mut [u8]) -> bool {
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

/// Overwrite `p` (resolved path) with `data`. Small payload → one WriteFile; larger → write_new +
/// streamed write_at chunks (so a piped payload up to the capture buffer reaches the file).
fn stream_overwrite(ctx: &ServiceContext, p: &[u8], data: &[u8]) {
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
fn fs_stream_combine(ctx: &ServiceContext, p: &[u8], new: &[u8], prepend: bool) -> bool {
    let old_size = fs_stat(ctx, p).map(|(sz, _)| sz as usize).unwrap_or(0);
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
fn pipe_write(ctx: &ServiceContext, cwd: &Cwd, arg: &str, data: &[u8]) {
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
    let t0 = ctx.datetime().epoch_secs();
    loop {
        if let Some(h) = ctx.acquire_send_grant_cap(sink) { return Some(h); }
        if ctx.datetime().epoch_secs() - t0 >= FILTER_WAIT_SECS { return None; }
        ctx.yield_cpu();
    }
}

/// How long `lookup_sink` waits (in 10 ms timer ticks, §9.1) for a freshly-spawned filter to
/// register its input endpoint. ~5 s - comfortably over the observed worst-case first-run latency
/// (~1 s) on the T630 under selfcheck load, with margin.
const FILTER_WAIT_SECS: i64 = 5;

fn cmd_kill(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
    if is_core_service(name) {
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
    match ctx.kill(name) {
        Ok(())  => { report(ctx, "killed: ", name); Ok(()) }
        Err(_)  => { report(ctx, "kill failed: ", name); Err(ShellError::Unknown) }
    }
}

fn cmd_restart(ctx: &ServiceContext, name: &str, core: Option<u32>) -> Result<(), ShellError> {
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
const CHAOS_RESTARTABLE: [&str; 6] = ["supervisor", "block-driver", "fs", "xhci", "ehci", "logger"];
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
fn chaos_save_retry(ctx: &ServiceContext, ppath: &[u8], data: &[u8]) -> bool {
    let t0 = ctx.datetime().epoch_secs();
    loop {
        let _ = ctx.reacquire_by_name("fs");
        if save_report(ctx, ppath, data) { return true; }
        if ctx.datetime().epoch_secs() - t0 >= CHAOS_SAVE_TOTAL_SECS { return false; }
        for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    }
}

/// Wait (real wall-clock bounded, RTC) for `name` to be ALIVE (present in the task table). Used
/// before a kill so a round isn't wasted killing a task that is still mid-respawn. Yields cooperatively.
fn chaos_wait_alive(ctx: &ServiceContext, name: &str) {
    let t0 = ctx.datetime().epoch_secs();
    let mut k = 0u32;
    while slot_of(ctx, name).is_none() {
        ctx.yield_cpu();
        k += 1;
        if k % CHAOS_POLL_EVERY == 0 && ctx.datetime().epoch_secs() - t0 >= CHAOS_RECOVER_SECS { break; }
    }
}

/// Wait (real wall-clock bounded, RTC - not a yield count, which isn't portable across QEMU/hardware)
/// for `name` to reach a generation different from `og` - proof a fresh instance came up (§7.5). Yields
/// cooperatively so the recoverer (sharing core 0) runs. Returns true on recovery, false on timeout.
fn chaos_wait_recovery(ctx: &ServiceContext, name: &str, og: u32) -> bool {
    let t0 = ctx.datetime().epoch_secs();
    let mut k = 0u32;
    loop {
        ctx.yield_cpu();
        k += 1;
        if k % CHAOS_POLL_EVERY == 0 {
            if let Some(g) = gen_of(ctx, name) { if g != og { return true; } }
            if ctx.datetime().epoch_secs() - t0 >= CHAOS_RECOVER_SECS { return false; }
        }
    }
}

fn cmd_chaos(ctx: &ServiceContext, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
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
        ctx.console_writeln("  max-carnage <all-services|svc> [n]  all-services (random) or aim at one");
        ctx.console_writeln("                          ('q' aborts; SERIAL only if the run kills the keyboard)");
        ctx.console_writeln("  svc: supervisor | block-driver | fs | logger | xhci | ehci | shell");
        return Ok(());
    }
    match tok[0] {
        "kill-storm"   => chaos_kill_storm(ctx, cwd, &tok, ntok),
        "flood-storm"  => chaos_flood_storm(ctx, cwd, &tok, ntok),
        "mem-pressure" => chaos_mem_pressure(ctx, cwd, &tok, ntok),
        "spawn-storm"  => chaos_spawn_storm(ctx, cwd, &tok, ntok),
        "max-carnage"  => {
            // The target is required now: <all-services|service>. tok[1] = target, tok[2] = rounds.
            // No target, or an explicit `help`, prints the usage + the two modes.
            if ntok < 2 || tok[1] == "help" {
                ctx.console_writeln("usage: chaos max-carnage <all-services|service> [rounds]");
                ctx.console_writeln("  all-services   storm a RANDOM live service each round");
                ctx.console_writeln("  <service>      aim every round at one service (e.g. fs, logger)");
                ctx.console_writeln("  both run system-wide mem-pressure + spawn-storm. 'q' aborts (SERIAL if kbd dies).");
                Ok(())
            } else {
                // Validate the TARGET before launching. The syntax is <target> [rounds], so a bare number
                // like `max-carnage 1000` parses 1000 as the TARGET - without this it silently storms a
                // service "1000" that does not exist. Reject anything that is neither "all-services" nor a
                // live service, loudly (invariant 12), with a SPECIFIC hint for the numeric mix-up.
                let target = tok[1];
                if target != "all-services" && slot_of(ctx, target).is_none() {
                    if !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()) {
                        ctx.console_writeln_fmt(format_args!(
                            "max-carnage: '{}' is not a service. For {} rounds of EVERYTHING, run:", target, target));
                        ctx.console_writeln_fmt(format_args!("  chaos max-carnage all-services {}", target));
                    } else {
                        ctx.console_writeln_fmt(format_args!("max-carnage: no live service '{}'.", target));
                        ctx.console_writeln("  target: all-services, or a live service");
                        ctx.console_writeln("  (block-driver | fs | logger | xhci | ehci | shell | supervisor)");
                    }
                    return Ok(());
                }
                let rounds = if ntok >= 3 { parse_u32(tok[2]).unwrap_or(0) } else { 0 };
                chaos_launch(ctx, tok[1], rounds)
            }
        }
        other => {
            ctx.console_writeln_fmt(format_args!(
                "chaos: unknown mode '{}' (try: chaos kill-storm <service> [rounds])", other));
            Err(ShellError::Unknown)
        }
    }
}

/// `chaos max-carnage` - launch the `chaos` service, which takes over the console (the foreground
/// primitive, syscall 40), runs the storm with the SHELL itself a target now, and on `q` hands the
/// keyboard back + self-terminates. The shell goes "muted" (see the main loop) for the duration. Kill
/// any prior instance first - one-shot, no graceful self-exit race - exactly like `observe now`.
fn chaos_launch(ctx: &ServiceContext, target: &str, rounds: u32) -> Result<(), ShellError> {
    // Loud pre-flight warning + confirm, TAILORED to the target in three cases. all-services storms EVERY
    // driver, so the keyboard dies for sure (serial only). A single USB host driver (xhci/ehci) kills the
    // keyboard ONLY if it is the controller yours is on - we cannot know which, so we state the proviso.
    // Anything else leaves the keyboard alive. The keyboard works HERE, pre-storm, so the confirm lands.
    let target_all = target == "all-services";
    let target_usb = target == "xhci" || target == "ehci";
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
    ctx.console_write(" Start maximum carnage? [y/N]: ");
    let c = ctx.console_read();
    // Echo the keypress (console_read does not echo) so the operator sees what they typed - but do NOT
    // print the newline yet. Wait for ENTER first, THEN newline. Printing the newline right after the
    // keypress made it look like the choice had registered when it had not (you still had to press Enter).
    if let Ok(s) = core::str::from_utf8(&[c]) { ctx.console_write(s); }
    // Drain the rest of the line up to Enter (a stray key after the first won't bleed into the next
    // prompt). A bare Enter (the default = N = cancel) has nothing to drain.
    if c != b'\r' && c != b'\n' { loop { let n = ctx.console_read(); if n == b'\r' || n == b'\n' { break; } } }
    ctx.console_writeln("");
    if c != b'y' && c != b'Y' {
        ctx.console_writeln("max-carnage: cancelled.");
        return Ok(());
    }
    let _ = ctx.kill("chaos");
    if ctx.spawn("chaos").is_err() {
        ctx.console_writeln("chaos: failed to spawn the chaos service");
        return Err(ShellError::Unknown);
    }
    // Send the round count (0 = run until q) AND the target (all | service name). Best-effort: chaos waits
    // briefly for it, defaults to all / run-until-q if it doesn't arrive. Reclaim the cap (no leak).
    if let Some(cap) = ctx.acquire_send_cap("chaos") {
        let mut buf = [0u8; 4 + 24];
        buf[..4].copy_from_slice(&rounds.to_le_bytes());
        let tb = target.as_bytes(); let n = tb.len().min(24);
        buf[4..4 + n].copy_from_slice(&tb[..n]);
        let _ = ctx.send_by_handle(cap, &Message::from_bytes(&buf[..4 + n]));
        ctx.remove_cap(cap);
    }
    // Wait (bounded) for chaos to TAKE the console foreground before returning. Otherwise the shell loops
    // back and blocks in console_read BEFORE chaos claims, then sits blocked there for the whole run (never
    // its muted-poll path); on chaos's release that read just re-blocks with no byte, so no fresh `gsh>`
    // repaints on the framebuffer until the user presses Enter (the intermittent "no prompt after chaos
    // done" glitch). Once chaos owns the foreground the shell's loop goes muted and reliably reprints the
    // prompt on regain. Bounded (chaos waits up to 2 s for this count first), so a chaos that never claims
    // still returns and the shell carries on.
    let t0 = ctx.datetime().epoch_secs();
    while ctx.is_console_foreground() {
        ctx.yield_cpu();
        if ctx.datetime().epoch_secs() - t0 >= 3 { break; }
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
fn chaos_kill_storm(ctx: &ServiceContext, cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    if ntok < 2 {
        ctx.console_writeln("usage: chaos kill-storm <service> [rounds] [save <path>]   (service: supervisor | block-driver | fs)");
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
    const FLOOD_DRAIN_YIELDS: u32 = 40; // yields to let the target drain before we re-check

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
        for _ in 0..FLOOD_DRAIN_YIELDS { ctx.yield_cpu(); }
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
        let t0 = ctx.datetime().epoch_secs();
        let mut low = baseline;
        loop {
            ctx.yield_cpu();
            let f = ctx.inspect_kernel_free_frames();
            if f < low { low = f; }
            if baseline.saturating_sub(low) >= MEM_DROP_MIN { break; }
            if ctx.datetime().epoch_secs() - t0 >= MEM_WAIT_SECS { break; }
        }
        let dropped = baseline.saturating_sub(low);
        grabbed[r] = dropped.min(u32::MAX as u64) as u32;
        // 3. Kill the hog - the only way v1 reclaims its memory (§10.5).
        let _ = ctx.kill("mem-pressure");
        // 4. Wait for reclaim - free frames return toward baseline. RTC-bounded.
        let t1 = ctx.datetime().epoch_secs();
        let mut hi = low;
        loop {
            ctx.yield_cpu();
            let f = ctx.inspect_kernel_free_frames();
            if f > hi { hi = f; }
            if hi + MEM_SLACK >= baseline { break; }
            if ctx.datetime().epoch_secs() - t1 >= MEM_WAIT_SECS { break; }
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
    const SPAWN_SETTLE:        u32 = 300;  // yields after each spawn so the hog runs + grabs its 32 MiB
    const KILL_SETTLE:         u32 = 80;   // yields after each kill so reclaim drains
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
        if ctx.spawn("mem-pressure").is_err() {
            refused_at = n + 1;   // the ceiling held - graceful refusal, no panic
            break;
        }
        spawned += 1;
        for _ in 0..SPAWN_SETTLE { ctx.yield_cpu(); }   // let the hog grab its 32 MiB before the next spawn
    }

    let low       = ctx.inspect_kernel_free_frames();   // memory floor under the swarm
    let live_peak = count_live(ctx);
    let hogs_peak = count_named(ctx, "mem-pressure");

    // 2. Kill every hog (loop until none remain). Bounded by a safety cap.
    let mut killed = 0u32;
    while slot_of(ctx, "mem-pressure").is_some() && killed < SPAWN_STORM_MAX + 16 {
        let _ = ctx.kill("mem-pressure");
        killed += 1;
        for _ in 0..KILL_SETTLE { ctx.yield_cpu(); }
    }

    // 3. Wait for reclaim - free frames return to ~baseline (deferred kstacks drain on timer ticks).
    let t0 = ctx.datetime().epoch_secs();
    let mut hi = low;
    loop {
        ctx.yield_cpu();
        let f = ctx.inspect_kernel_free_frames();
        if f > hi { hi = f; }
        if hi + RECLAIM_SLACK >= baseline { break; }
        if ctx.datetime().epoch_secs() - t0 >= RECLAIM_SECS { break; }
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
fn fs_request(ctx: &ServiceContext, op: u8, path: &[u8], data: &[u8]) -> Option<Message> {
    let pl = path.len().min(255);
    let mut req = [0u8; 4096];
    req[0] = op;
    req[1] = pl as u8;
    req[2..2 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 2 - pl);
    req[2 + pl..2 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..2 + pl + dn]);
    if let Some(r) = ctx.request_with_reply("fs", &msg) {
        return Some(r);
    }
    // No reply usually means `fs` restarted and our cached cap is now EndpointDead (Phase D,
    // §14.3). Reacquire a fresh `fs` cap by name and retry once; if `fs` hasn't
    // finished re-registering yet, this returns None and the next command retries.
    if ctx.reacquire_by_name("fs") {
        return ctx.request_with_reply("fs", &msg);
    }
    None
}

/// Wall-clock budget (seconds) for the chaos-report save's fs request. The save runs right after a
/// chaos storm that may have hammered `fs` + its `block-driver`, so the reply could be slow or never
/// come; this bounds it so the save can fail gracefully (console report stands) instead of hanging.
const SAVE_FS_MAX_SECS: i64 = 8;

/// `fs_request` for the report save: the reply wait is bounded by `SAVE_FS_MAX_SECS` of wall-clock
/// time (RTC), so a still-restarting `fs` can't block the shell forever (the bug behind `chaos
/// max-carnage … save` hanging). Reacquire + retry once on a miss, then give up.
fn fs_request_bounded(ctx: &ServiceContext, op: u8, path: &[u8], data: &[u8]) -> Option<Message> {
    let pl = path.len().min(255);
    let mut req = [0u8; 4096];
    req[0] = op;
    req[1] = pl as u8;
    req[2..2 + pl].copy_from_slice(&path[..pl]);
    let dn = data.len().min(req.len() - 2 - pl);
    req[2 + pl..2 + pl + dn].copy_from_slice(&data[..dn]);
    let msg = Message::from_bytes(&req[..2 + pl + dn]);
    if let Some(r) = ctx.request_with_reply_deadline("fs", &msg, SAVE_FS_MAX_SECS) {
        return Some(r);
    }
    if ctx.reacquire_by_name("fs") {
        return ctx.request_with_reply_deadline("fs", &msg, SAVE_FS_MAX_SECS);
    }
    None
}

/// Stat a path: `Some((size, is_dir))` if it exists, `None` otherwise. Used by the streaming
/// read/copy paths to learn a file's size before chunking through it.
fn fs_stat(ctx: &ServiceContext, path: &[u8]) -> Option<(u64, bool)> {
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
fn fs_read_at(ctx: &ServiceContext, path: &[u8], offset: u64, out: &mut [u8]) -> Option<usize> {
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

/// Create/truncate `path` to hold `total` bytes (allocates the whole extent). Pairs with
/// `fs_write_at` to stream a large file.
fn fs_write_new(ctx: &ServiceContext, path: &[u8], total: u64) -> bool {
    matches!(fs_request(ctx, OP_WRITE_NEW, path, &total.to_le_bytes()),
             Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// Write `chunk` into `path` at block-aligned byte `offset`.
fn fs_write_at(ctx: &ServiceContext, path: &[u8], offset: u64, chunk: &[u8]) -> bool {
    let mut tail = [0u8; 8 + IO_CHUNK];
    tail[..8].copy_from_slice(&offset.to_le_bytes());
    let n = chunk.len().min(IO_CHUNK);
    tail[8..8 + n].copy_from_slice(&chunk[..n]);
    matches!(fs_request(ctx, OP_WRITE_AT, path, &tail[..8 + n]),
             Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// True if `fs` replied "no filesystem" - print the standard hint and consume it.
fn no_fs(ctx: &ServiceContext, p: &[u8]) -> bool {
    if p.first() == Some(&FS_NOFS) {
        ctx.console_writeln("no filesystem - run 'drives flash' first");
        true
    } else {
        false
    }
}

/// `ls [path]` - list a directory.
fn cmd_ls(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request(ctx, OP_LIST_DIR, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("ls: storage unavailable"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return Err(ShellError::Unknown); }
    if p.first() == Some(&FS_NOTFOUND) || p.len() < 2 {
        ctx.console_writeln_fmt(format_args!("ls: not a directory: {}", str_of(path)));
        return Err(ShellError::FileNotFound);
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
fn fc_open(ctx: &ServiceContext, path: &[u8], rights: u8) -> Option<CapHandle> {
    let r = fs_request(ctx, OP_OPEN, path, &[rights])?;
    if r.payload_bytes().first() == Some(&FS_OK) { ctx.take_pending_cap() } else { None }
}

/// Invoke a file cap (§7.10): the kernel validates `file` holds `right`, badges the request, and
/// routes it to fs; fs replies on our endpoint. `None` means the kernel rejected the invocation
/// (the cap lacks `right` - non-escalation - or is stale/revoked), so no reply comes back.
fn fc_invoke(ctx: &ServiceContext, file: CapHandle, right: u8, payload: &[u8]) -> Option<Message> {
    let self_grant = ctx.self_grant_handle()?;
    let reply = ctx.derive_cap(self_grant)?;
    if ctx.resource_invoke(file, right, reply, &Message::from_bytes(payload)).is_err() {
        ctx.remove_cap(reply); // kernel didn't consume it (validation failed) - don't leak the slot
        return None;
    }
    Some(ctx.recv())
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
fn cmd_fcap(ctx: &ServiceContext, arg: &str) -> Result<(), ShellError> {
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
    match fc_invoke(ctx, CapHandle(60000), RIGHT_READ, &rbuf) {
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
// silently dropped (§26.7). Rendering uses only the CSI subset the serial terminal AND the fbcon
// support (`arch/x86_64/fb.rs`): cursor position, erase-to-EOL, show/hide; reverse-video bars are
// SGR (pretty on serial, plain on the fbcon - it ignores the unsupported escape, no garbage).
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
    fn refill(&mut self, ctx: &ServiceContext, abs: usize) {
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
    fn read_span(&mut self, ctx: &ServiceContext, logical: usize, n: usize, out: &mut [u8]) -> usize {
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

    fn byte_at(&mut self, ctx: &ServiceContext, pos: usize) -> Option<u8> {
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
    fn line_start(&mut self, ctx: &ServiceContext, pos: usize) -> usize {
        let mut i = pos;
        let mut steps = 0;
        while i > 0 && steps < EDIT_LINE_MAX {
            if self.byte_at(ctx, i - 1) == Some(b'\n') { return i; }
            i -= 1; steps += 1;
        }
        i
    }
    /// Logical offset of the '\n' ending the line containing `pos`, or `total` for the last line.
    fn line_end(&mut self, ctx: &ServiceContext, pos: usize) -> usize {
        let mut i = pos;
        let mut steps = 0;
        while i < self.total && steps < EDIT_LINE_MAX {
            if self.byte_at(ctx, i) == Some(b'\n') { return i; }
            i += 1; steps += 1;
        }
        i
    }
    /// Count of '\n' bytes in `[from, to)` - the number of line breaks between two offsets.
    fn lines_between(&mut self, ctx: &ServiceContext, from: usize, to: usize) -> usize {
        let mut n = 0; let mut i = from;
        while i < to { if self.byte_at(ctx, i) == Some(b'\n') { n += 1; } i += 1; }
        n
    }
    /// Advance `pos` forward by `k` line starts (stops at end-of-document).
    fn advance_lines(&mut self, ctx: &ServiceContext, mut pos: usize, k: usize) -> usize {
        for _ in 0..k {
            let le = self.line_end(ctx, pos);
            if le >= self.total { return pos; }
            pos = le + 1;
        }
        pos
    }

    fn move_home(&mut self, ctx: &ServiceContext) { let c = self.cur; self.cur = self.line_start(ctx, c); }
    fn move_end(&mut self, ctx: &ServiceContext)  { let c = self.cur; self.cur = self.line_end(ctx, c); }
    fn move_up(&mut self, ctx: &ServiceContext) {
        let c = self.cur;
        let ls = self.line_start(ctx, c);
        if ls == 0 { self.cur = 0; return; }
        let col = c - ls;
        let pls = self.line_start(ctx, ls - 1); // previous line's start
        let plen = (ls - 1) - pls;              // previous line length (excluding its '\n')
        self.cur = pls + col.min(plen);
    }
    fn move_down(&mut self, ctx: &ServiceContext) {
        let c = self.cur;
        let le = self.line_end(ctx, c);
        if le >= self.total { self.cur = self.total; return; }
        let ls = self.line_start(ctx, c);
        let col = c - ls;
        let nls = le + 1;                       // next line's start
        let nlen = self.line_end(ctx, nls) - nls;
        self.cur = nls + col.min(nlen);
    }
    fn page(&mut self, ctx: &ServiceContext, down: bool) {
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
/// serial terminal and a no-op on the fbcon (→ plain text), so the bar reads cleanly on both.
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
fn edit_render(ctx: &ServiceContext, ed: &mut Editor, name: &[u8]) {
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
fn edit_save(ctx: &ServiceContext, ed: &mut Editor) -> bool {
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
fn edit_csi(ctx: &ServiceContext, ed: &mut Editor) {
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
fn edit_try_quit(ctx: &ServiceContext, ed: &mut Editor) -> bool {
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
fn cmd_edit(ctx: &ServiceContext, cwd: &Cwd, arg: &str) -> Result<(), ShellError> {
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

fn cmd_read(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    // Stat first (one message) to learn the size, then STREAM the content in IO_CHUNK pieces
    // via read_at - so a file far larger than one IPC message reads back correctly without a
    // big buffer here.
    let stat = match fs_request(ctx, OP_STAT_FILE, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("read: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_write(ctx: &ServiceContext, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
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
fn fmt_to_temp(ctx: &ServiceContext, src: &[u8], tmp: &[u8]) -> Result<u64, FmtErr> {
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
fn fmt_compare_files(ctx: &ServiceContext, a: &[u8], b: &[u8]) -> bool {
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
fn cmd_fmt(ctx: &ServiceContext, cwd: &Cwd, rest: &str) -> Result<(), ShellError> {
    let rest = rest.trim();
    let (check, pathstr) = match rest.split_once(char::is_whitespace) {
        Some(("check", p)) => (true, p.trim()),
        _ => (false, rest),
    };
    if pathstr.is_empty() {
        ctx.console_writeln("usage: fmt <path>   |   fmt check <path>   (see: fmt help)");
        return Err(ShellError::Unknown);
    }
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
fn cmd_mkdir(ctx: &ServiceContext, cwd: &Cwd, arg: &str, parents: bool) -> Result<(), ShellError> {
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
fn cmd_cd(ctx: &ServiceContext, cwd: &mut Cwd, arg: &str) -> Result<(), ShellError> {
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
    let reply = match fs_request(ctx, OP_STAT_FILE, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("cd: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_copy(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
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
fn cmd_copy_tree(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
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
fn stat_kind(ctx: &ServiceContext, path: &[u8]) -> Option<bool> {
    let reply = fs_request(ctx, OP_STAT_FILE, path, &[])?;
    let p = reply.payload_bytes();
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 { Some(p[10] != 0) } else { None }
}

/// `mkdir <path>` via fs, treating success as true. Used by recursive copy to recreate dirs.
fn mkdir_at(ctx: &ServiceContext, path: &[u8]) -> bool {
    matches!(fs_request(ctx, OP_MKDIR, path, &[]), Some(r) if r.payload_bytes().first() == Some(&FS_OK))
}

/// Stream-copy a file `src`→`dst` of any size: stat the size, allocate `dst`, then chunk
/// through with `read_at`/`write_at` (one IO_CHUNK buffer, no whole-file buffer). Returns
/// `Some(bytes)` on success. The building block under both `copy` and recursive `copy`.
fn copy_file_streaming(ctx: &ServiceContext, src: &[u8], dst: &[u8]) -> Option<u64> {
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
fn copy_one(ctx: &ServiceContext, src: &[u8], dst: &[u8]) -> bool {
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
fn cmd_rename(ctx: &ServiceContext, cwd: &Cwd, path: &str, newname: &str) -> Result<(), ShellError> {
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
fn cmd_delete(ctx: &ServiceContext, cwd: &Cwd, arg: &str, recursive: bool) -> Result<(), ShellError> {
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
fn cmd_move(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) -> Result<(), ShellError> {
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
fn cmd_find(ctx: &ServiceContext, cwd: &Cwd, target: &str, start: &str, out: &mut Out) -> Result<(), ShellError> {
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
        let reply = match fs_request(ctx, OP_LIST_DIR, &dir[..dlen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("find: storage unavailable"); return Err(ShellError::Unknown); }
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
/// continuation correctly. UTF-8: the fbcon decodes `├ └ │ ─` and renders light box glyphs;
/// a trailing `/` still marks directories (the console is monochrome - no colour to lean on).
/// `#[inline(never)]`: holds the ~12 KiB `TreeStack` + prefix scratch off the hot pipe frame
/// (it's a pipe producer; see [[project-shell-stack-pipe]]).
#[inline(never)]
fn cmd_tree(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) -> Result<(), ShellError> {
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

        let reply = match fs_request(ctx, OP_LIST_DIR, &buf[..plen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("tree: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_match(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
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
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("match: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_count(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let path = if argc >= 2 { args[1] } else { "" };
    if path.is_empty() {
        ctx.console_writeln("count: a path is required (or pipe input: <producer> | count)");
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("count: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_sort(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let (reverse, path) = parse_sort(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln("sort: a path is required (or pipe input: <producer> | sort)");
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("sort: storage unavailable"); return Err(ShellError::Unknown); }
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
fn cmd_take(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize, last: bool) -> Result<(), ShellError> {
    let name = if last { "last" } else { "first" };
    let (n, path) = parse_take(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln_fmt(format_args!("{}: a path is required (or pipe: <producer> | {} [N])", name, name));
        return Err(ShellError::Unknown);
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln_fmt(format_args!("{}: storage unavailable", name)); return Err(ShellError::Unknown); }
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

fn cmd_drives(ctx: &ServiceContext, args: &[&str], argc: usize) -> Result<(), ShellError> {
    let sub = if argc >= 2 { args[1] } else { "" };
    match sub {
        ""        => drives_list(ctx),
        "flash"   => {
            // `drives flash [drive] [label]` - the drive selector is optional (one drive).
            let (sel, label) = split_drive_value(args, argc);
            if drive_sel_ok(ctx, sel) { drives_flash(ctx, label) } else { Err(ShellError::Unknown) }
        }
        "label"   => {
            // `drives label [drive] <name>` - selector optional; name required.
            let (sel, name) = split_drive_value(args, argc);
            if name.is_empty() { ctx.console_writeln("usage: drives label [drive] <name>"); Err(ShellError::Unknown) }
            else if drive_sel_ok(ctx, sel) { drives_label(ctx, name) } else { Err(ShellError::Unknown) }
        }
        "reset"   => {
            // `drives reset [drive]` - un-format back to raw (optional selector, no value).
            let sel = if argc >= 3 { args[2] } else { "" };
            if drive_sel_ok(ctx, sel) { drives_reset(ctx) } else { Err(ShellError::Unknown) }
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
fn drives_list(ctx: &ServiceContext) -> Result<(), ShellError> {
    let reply = match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_DRIVES_INFO])) {
        Some(r) => r,
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); return Err(ShellError::Unknown); }
    };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 28 {
        ctx.console_writeln("drives: no disk found");
        return Err(ShellError::Unknown);
    }
    let mounted = p[1] != 0;
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
fn drives_flash(ctx: &ServiceContext, label: &str) -> Result<(), ShellError> {
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
    req[0] = OP_FLASH;
    req[1] = ll as u8;
    req[2..2 + ll].copy_from_slice(&lb[..ll]);
    match ctx.request_with_reply("fs", &Message::from_bytes(&req[..2 + ll])) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: formatted as GSFS - mounted, ready to use now (no reboot)");
            Ok(())
        }
        Some(_) => { ctx.console_writeln("drives: flash FAILED (no disk, or disk too small)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives reset` - un-format the drive back to raw (zero the superblock). Destructive;
/// a quick clean slate for re-testing the raw→flash path. NOT a secure wipe.
fn drives_reset(ctx: &ServiceContext) -> Result<(), ShellError> {
    ctx.console_write("This un-formats the drive back to raw (ERASES). Continue? [y/N] ");
    if !read_confirm(ctx) {
        ctx.console_writeln("drives: aborted");
        return Err(ShellError::Unknown);
    }
    match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_RESET])) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: reset to raw - 'drives flash' to use again");
            Ok(())
        }
        Some(_) => { ctx.console_writeln("drives: reset FAILED (no disk?)"); Err(ShellError::Unknown) }
        None    => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives check` - fsck: walk the tree (the source of truth), rebuild the free bitmap + free
/// count from it, and verify every block's CRC. Repairs allocation drift non-destructively;
/// reports (does not delete) files/dirs whose blocks fail their CRC. No confirmation needed -
/// it never erases data. Reply: [FS_OK, files:u32, dirs:u32, bad:u32, used:u64, free:u64].
fn drives_check(ctx: &ServiceContext) -> Result<(), ShellError> {
    match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_CHECK])) {
        Some(r) => {
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
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives scrub` - READ-ONLY integrity sweep (Phase K): walk the tree, verify every block's
/// CRC, report, change NOTHING on disk (distinct from `check`, which repairs the bitmap). Run it
/// on a schedule to catch latent bit-rot early; without redundancy it detects but cannot repair.
/// Reply: [FS_OK, files:u32, dirs:u32, bad:u32, scanned:u64].
fn drives_scrub(ctx: &ServiceContext) -> Result<(), ShellError> {
    match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_SCRUB])) {
        Some(r) => {
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
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); Err(ShellError::Unknown) }
    }
}

/// `drives label <name>` - name / rename the drive (rewrites the superblock).
fn drives_label(ctx: &ServiceContext, name: &str) -> Result<(), ShellError> {
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
    match ctx.request_with_reply("fs", &Message::from_bytes(&req[..2 + ll])) {
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
    let mut first = 0u8;
    loop {
        let b = ctx.console_read();
        match b {
            b'\r' | b'\n' => break,
            _ if first == 0 && b >= 0x20 && b < 0x7f => first = b,
            _ => {}
        }
    }
    first == b'y' || first == b'Y'
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
