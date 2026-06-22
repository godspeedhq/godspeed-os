// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapInfo, CapHandle, Message, IpcError};
use godspeed_sdk::record::{Table, Value, RecordSink, parse_predicate, REC_MAX_ROWS, REC_ARENA};

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
}
impl Out<'_> {
    /// Write a string, no trailing newline.
    fn put(&mut self, ctx: &ServiceContext, s: &str) {
        match self {
            Out::Console => console_write_chunked(ctx, s.as_bytes()),
            Out::Capture(c) => c.push(s.as_bytes()),
            Out::File(r) => r.push(s.as_bytes()),
        }
    }
    /// Write raw bytes, no trailing newline (file content may not be clean UTF-8).
    fn put_bytes(&mut self, ctx: &ServiceContext, b: &[u8]) {
        match self {
            Out::Console => console_write_chunked(ctx, b),
            Out::Capture(c) => c.push(b),
            Out::File(r) => r.push(b),
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
const REPORT_MAX: usize = 16 * 1024;
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

    loop {
        let b = ctx.console_read();

        match b {
            b'\r' | b'\n' => {
                // We own echo now, so move to a fresh line ourselves (the kernel used
                // to echo the Enter as "\r\n").
                ctx.console_write("\r\n");
                if line.len > 0 {
                    hist.push(line.bytes());
                    last_result = execute(&ctx, line.bytes(), &mut cwd, last_result, 0);
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
                        // SS3 (F1–F4 = ESC O P/Q/R/S). F1 opens help; F2–F4 have no action.
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
    *last_result = execute(ctx, b"help", cwd, *last_result, 0);
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
/// it; several show the numbered menu (1–9 selects, Tab cycles). Operates from end-of-line so the menu
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
    ("drives",  &["flash", "label", "reset", "check", "scrub"]),
    ("chaos",   &["kill-storm", "flood-storm", "max-carnage"]),
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

/// Numbered menu for keyword candidates: a digit (1–9) commits, Tab cycles, any other key keeps the
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

/// Print the numbered candidate menu, then run the selection loop: a **digit** (1–9) commits that
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
fn execute(ctx: &ServiceContext, line: &[u8], cwd: &mut Cwd, prev: Result<(), ShellError>, depth: u8) -> Result<(), ShellError> {
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
        return pipe_run(ctx, cwd, s);
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
            cmd_read(ctx, cwd, args[1], &mut Out::Console)
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
            return if argc < 2 {
                ctx.console_writeln("usage: run <path> [save <path>]");
                Err(ShellError::Unknown)
            } else {
                // Optional `save <path>` streams the run REPORT to a file (the utility writes its
                // own file - direct, not a pipe; see cmd_selfcheck / docs/pipes.md).
                let save = if argc >= 4 && args[2] == "save" { Some(args[3]) } else { None };
                cmd_run(ctx, cwd, args[1], depth, save)
            };
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
        "echo"    => cmd_echo(ctx, strip_quotes(s["echo".len()..].trim()), &mut Out::Console),
        "about"   => cmd_about(ctx, &mut Out::Console),
        "mem"     => cmd_mem(ctx, &mut Out::Console),
        "cores"   => cmd_cores(ctx, &mut Out::Console),
        "date"    => cmd_date(ctx, if argc >= 2 { args[1] } else { "" }, &mut Out::Console),
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
        "ls"      => cmd_ls(ctx, cwd, if argc >= 2 { args[1] } else { "" }, &mut Out::Console),
        "edit"    => cmd_edit(ctx, cwd, s["edit".len()..].trim()),
        "write"   => cmd_write(ctx, cwd, s["write".len()..].trim()),
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
            else { cmd_find(ctx, cwd, args[1], if argc >= 3 { args[2] } else { "/" }, &mut Out::Console) }
        }
        "tree"    => cmd_tree(ctx, cwd, if argc >= 2 { args[1] } else { "" }, &mut Out::Console),
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
const SCRIPT_MAX: usize = 4096;

/// Trim leading/trailing ASCII whitespace from a byte slice (lines/commands in a script).
fn trim_bytes(b: &[u8]) -> &[u8] {
    let mut s = 0usize;
    let mut e = b.len();
    while s < e && b[s].is_ascii_whitespace() { s += 1; }
    while e > s && b[e - 1].is_ascii_whitespace() { e -= 1; }
    &b[s..e]
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
fn cmd_run(ctx: &ServiceContext, cwd: &mut Cwd, arg: &str, depth: u8, save: Option<&str>) -> Result<(), ShellError> {
    let mut pbuf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut pbuf) { Some(p) => p, None => return Err(ShellError::Unknown) };
    // Read the whole script into a fixed buffer, then drop the fs reply before executing anything.
    let mut script = [0u8; SCRIPT_MAX];
    let slen;
    {
        let reply = match fs_request(ctx, OP_READ_FILE, path, &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("run: storage unavailable"); return Err(ShellError::Unknown); }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return Err(ShellError::Unknown); }
        if !(p.first() == Some(&FS_OK) && p.len() >= 5) {
            ctx.console_writeln_fmt(format_args!("run: not found: {}", str_of(path)));
            return Err(ShellError::FileNotFound);
        }
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        let body = &p[5..end];
        let take = body.len().min(SCRIPT_MAX);
        if take < body.len() {
            ctx.console_writeln_fmt(format_args!("run: script truncated at {} bytes (max {})", take, SCRIPT_MAX));
        }
        script[..take].copy_from_slice(&body[..take]);
        slen = take;
    }
    run_with_optional_save(ctx, cwd, &script[..slen], depth, save)
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
fn run_lines(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, out: &mut Out) -> Result<(), ShellError> {
    let mut ran = 0u32;
    let mut failed = 0u32;
    let mut last: Result<(), ShellError> = Ok(());
    // Per-command verdicts, for the end-of-run summary. Bounded; commands past the cap still run
    // and count, they just don't get a summary line (loud, not silent - §26.6).
    let mut verdict = [true; RUN_MAX_CMDS];
    let mut vi = 0usize;
    for line in src.split(|&b| b == b'\n') {
        let line = trim_bytes(line);
        if line.is_empty() || line[0] == b'#' { continue; } // blank or whole-line comment
        for cmd in line.split(|&b| b == b';') {
            let cmd = trim_bytes(cmd);
            if cmd.is_empty() { continue; }
            // Echo the command so the transcript shows what produced each result.
            out.put(ctx, "> ");
            out.line(ctx, str_of(cmd));
            last = execute(ctx, cmd, cwd, last, depth + 1);
            if vi < RUN_MAX_CMDS { verdict[vi] = last.is_ok(); }
            vi += 1;
            ran += 1;
            if last.is_err() { failed += 1; }
        }
    }
    // End-of-run summary: PASS/FAIL per command (re-split identically, pairing with `verdict`).
    // "FAIL  " is deliberately not the word "FAILED" the harness greens on absence of.
    out.line(ctx, "--- summary ---");
    let mut j = 0usize;
    for line in src.split(|&b| b == b'\n') {
        let line = trim_bytes(line);
        if line.is_empty() || line[0] == b'#' { continue; }
        for cmd in line.split(|&b| b == b';') {
            let cmd = trim_bytes(cmd);
            if cmd.is_empty() { continue; }
            out.put(ctx, if j < RUN_MAX_CMDS && !verdict[j] { "FAIL  " } else { "PASS  " });
            out.line(ctx, str_of(cmd));
            j += 1;
        }
    }
    out.line_fmt(ctx, format_args!("run: ran {}, failed {}", ran, failed));
    if failed == 0 { Ok(()) } else { Err(ShellError::Unknown) }
}

/// Run `src` and, if `save` is `Some`, stream the report to that file (the utility writes its own
/// file - direct, not a pipe). Bare → report to the console. Shared by `run`/`selfcheck`. This
/// dispatcher is tiny on purpose: the 32 KiB `ReportBuf` lives ONLY in `run_and_save`, called only
/// on the save path - so a bare run/selfcheck does NOT carry 32 KiB of unused frame (which would
/// tip its already-heavy `| assert` sub-pipelines over the user-stack ceiling).
fn run_with_optional_save(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, save: Option<&str>)
    -> Result<(), ShellError>
{
    match save {
        None => run_lines(ctx, cwd, src, depth, &mut Out::Console),
        Some(spath) => run_and_save(ctx, cwd, src, depth, spath),
    }
}

/// The save path: accumulate the run report into a bounded `ReportBuf` and write it to `spath`
/// (direct file write, no pipe). `#[inline(never)]` so the 32 KiB buffer exists only while a save
/// is actually running, not in the frame of every bare run.
#[inline(never)]
fn run_and_save(ctx: &ServiceContext, cwd: &mut Cwd, src: &[u8], depth: u8, spath: &str)
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
        run_lines(ctx, cwd, src, depth, &mut out)
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
    run_with_optional_save(ctx, cwd, SELFCHECK_GS.as_bytes(), depth, save)
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
            let r = execute(ctx, cmd.as_bytes(), cwd, Ok(()), depth + 1);
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
            let r = execute(ctx, inner.as_bytes(), cwd, Ok(()), depth + 1);
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
    "echo", "clear", "about", "mem", "cores", "date", "uptime", "status", "observe", "caps", "roster",
    "spawn", "kill", "restart", "reboot", "chaos", "drives", "ls", "cd", "read", "write", "edit", "fcap",
    "mkdir", "copy", "move", "rename", "delete", "find", "tree", "match", "count", "sort",
    "first", "last",
    // record-pipe verbs (pipe-only stages; see docs/records.md)
    "where", "select", "to", "from",
];
fn is_util(name: &str) -> bool { UTILS.contains(&name) }

/// `<util> version` - version number, then creator credit.
fn util_version(ctx: &ServiceContext, util: &str) {
    ctx.console_writeln_fmt(format_args!("{} {}", util, UTIL_VERSION));
    ctx.console_writeln("Created by Bankole Ogundero.");
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
            ("chaos max-carnage [rounds] [save <path>]", "the chaos monkey: kill a RANDOM live service each round (everything but the shell); proves the KERNEL survives arbitrary carnage", "chaos max-carnage 30"),
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
        "count" => help_block(ctx, "count", "count lines, words, and bytes", &[
            ("<producer> | count", "count piped input", "find *.txt | count"),
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
fn cmd_echo(ctx: &ServiceContext, text: &str, out: &mut Out) -> Result<(), ShellError> {
    out.line(ctx, text);
    Ok(())
}

/// One-line identity for the system. A pipe source (`about | write /about.txt`): renders through
/// `Out`, so it captures to a file as readily as it prints.
fn cmd_about(ctx: &ServiceContext, out: &mut Out) -> Result<(), ShellError> {
    out.line(ctx, "GodspeedOS: a capability-based microkernel (v1 milestone)");
    out.line_fmt(ctx, format_args!("  running on {} core(s)", ctx.inspect_core_count()));
    out.line(ctx, "  Created by Bankole Ogundero.");
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
            Value::Int(s.mem_used), Value::Int(s.queue_depth as u64), Value::Int(s.generation as u64),
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
            Value::Int(s.mem_used), Value::Int(s.queue_depth as u64), Value::Int(s.generation as u64),
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
fn pipe_run(ctx: &ServiceContext, cwd: &Cwd, line: &str) -> Result<(), ShellError> {
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
    // No sink - render the final stream to the console.
    match &s {
        Stream::Bytes(c) => console_write_chunked(ctx, c.bytes()),
        Stream::Table(t) => { let mut o = Out::Console; t.to_grid(&mut OutSink { ctx, out: &mut o }); }
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
        // byte filters (Bytes only)
        "match" | "count" | "first" | "last" => match s {
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

/// The restart generation of the live service named `name` (None if not running). A restart bumps
/// the generation (§7.5), so a value strictly greater than a pre-kill reading proves a NEW instance
/// came up - the recovery signal `chaos kill-storm` waits on.
fn gen_of(ctx: &ServiceContext, name: &str) -> Option<u32> {
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 /* Dead */ && st.name_str() == name {
            return Some(st.generation as u32);
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
    if ctx.spawn("observe-live").is_err() {
        ctx.console_writeln("observe: failed to spawn observe-live");
        return Err(ShellError::Unknown);
    }
    if let Some(slot) = find_running_slot(ctx, "observe-live") {
        // Own `q` while the child paints. The bound is a paranoid safety net so a hung child can
        // never wedge the shell forever; normally we break on `q` (or if the child dies).
        for _ in 0..u32::MAX {
            // Sleep (don't busy-yield) so core 0 halts between polls. ~50 ms `q` latency.
            ctx.sleep(100_000_000);
            // Drain the console; quit on `q`/`Q` (other keys are discarded - observe takes no
            // other input). Echo is off (the child disabled it), so nothing smears the frame.
            let mut quit = false;
            while let Some(b) = ctx.try_console_read() {
                if b == b'q' || b == b'Q' { quit = true; }
            }
            if quit { break; }
            if !ctx.task_stat(slot).valid { break; } // child died unexpectedly
        }
    }
    let _ = ctx.kill("observe-live"); // reap the child (it never exits on its own)
    // Restore the console the child left in raw mode: show the cursor and drop below the last
    // frame so the prompt lands cleanly. Echo stays OFF - the shell, not the kernel, owns echo.
    ctx.console_echo(false);
    ctx.console_write("\x1b[?25h\r\n");
    Ok(())
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
/// (`registry` retired, Phase 4; `init` removed, Phase 5.)
const CORE_SERVICES: [&str; 1] = ["supervisor"];

/// Shown when spawn/kill/restart targets a core service - "Not applicable" makes
/// it clear the command is refused *because* the target is protected, not failed.
/// Lists exactly `CORE_SERVICES`; `registry` is intentionally absent (H11 ph6:
/// it is restartable, so `kill registry` is permitted).
const PROTECTED_MSG: &str =
    "Not applicable. Core services (init, supervisor) are protected";

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
        Some(h) => match ctx.try_send_by_handle(h, &Message::from_bytes(&[0x01])) {
            Ok(())  => { ctx.console_writeln_fmt(format_args!("spawncap: {} - endpoint cap acquired; send Ok", name)); Ok(()) }
            Err(_)  => { ctx.console_writeln_fmt(format_args!("spawncap: {} - cap acquired but send failed", name)); Err(ShellError::Unknown) }
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
                let _ = ctx.send_by_handle(h, &Message::from_bytes(inp));
                let _ = ctx.send_by_handle(h, &Message::from_bytes(&[PIPE_EOT]));
            }
            None => {
                // Distinct, honest wording: a registration TIMEOUT (filter never became ready) is
                // not "not a filter". The new phrasing also tells stale-image runs apart - if this
                // text ever changes on hardware, the new shell is running (§26.7 loud failure).
                ctx.console_writeln_fmt(format_args!(
                    "pipe: '{}' never registered an input endpoint (waited ~{}s) - not a filter, or it failed to start",
                    svc, FILTER_WAIT_TICKS / 100));
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
    matches!(name, "read" | "echo" | "tree"
                 | "about" | "mem" | "cores" | "date" | "help")
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
        "help"         => help_to_out(ctx, out),
        _ => {}
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

/// Look up a just-spawned service's endpoint via the registry, retrying while it registers.
fn lookup_sink(ctx: &ServiceContext, sink: &str) -> Option<CapHandle> {
    // A freshly-spawned filter registers its input endpoint only once it actually RUNS - which on
    // real multi-core hardware is up to ~1 s after spawn (it's on another core and hasn't been
    // scheduled yet). Retry the registry lookup until it appears, bounded by REAL time.
    //
    // CRITICAL - do NOT `yield_cpu` in this wait. `CORE_TOTAL_TICKS` counts scheduler *quanta*
    // (the timer IRQ **and** every `yield_current` - scheduler.rs), so yielding here inflates the
    // very counter we use as the clock: 50 yields/iteration drove it past the budget in ~4 ms and
    // the wait collapsed to nothing (the bug that bit the T630 twice - first as a yield-count wait,
    // then as a yield-polluted tick wait). `registry_lookup` already blocks on `recv` for the
    // registry's reply - a cooperative wait that lets the registry and the filter run and does NOT
    // bump the tick counter (`block_and_reschedule` doesn't). So a plain retry loop is both
    // cooperative (each iteration deschedules on the IPC) and real-time-bounded: with no yields the
    // counter advances ~only on the 100 Hz timer IRQ - a true wall-clock.
    // Path C (Phase 4): resolve the sink via the kernel name-directory (SEND|GRANT, so the cap can
    // be delegated to the producer) instead of the registry service. The directory is populated
    // synchronously at the sink's spawn, so this normally succeeds on the first iteration; the
    // bounded wall-clock retry stays as a guard for the rare not-yet-scheduled case.
    let core  = ctx.core_id();
    let start = ctx.inspect_core_total_ticks(core);
    loop {
        if let Some(h) = ctx.acquire_send_grant_cap(sink) { return Some(h); }
        if ctx.inspect_core_total_ticks(core).wrapping_sub(start) >= FILTER_WAIT_TICKS { return None; }
    }
}

/// How long `lookup_sink` waits (in 10 ms timer ticks, §9.1) for a freshly-spawned filter to
/// register its input endpoint. ~5 s - comfortably over the observed worst-case first-run latency
/// (~1 s) on the T630 under selfcheck load, with margin.
const FILTER_WAIT_TICKS: u64 = 500;

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
// kernel; the shell is excluded only because chaos runs *inside* it. (registry retired Phase 4; init
// removed Phase 5; xhci/ehci/logger made directly-restartable so max-carnage can't leave them dead.)
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
const CARNAGE_MAX_CAND: usize = 32;       // bounded snapshot of live killable tasks per round (§26.6)
const CARNAGE_PROGRESS_EVERY: u64 = 100;  // update the in-place progress line every N rounds

/// Save `data` to the already-resolved absolute path `ppath`, retrying for up to
/// `CHAOS_SAVE_TOTAL_SECS` of WALL-CLOCK time while `fs` finishes re-mounting after a chaos storm -
/// reacquiring a fresh `fs` cap each round (it may have just respawned). Bounded: `save_report` is
/// itself wall-clock-bounded, so this never hangs; it gives up gracefully when fs won't stabilise.
fn chaos_save_retry(ctx: &ServiceContext, ppath: &[u8], data: &[u8]) -> bool {
    let t0 = ctx.datetime().epoch_secs();
    loop {
        let _ = ctx.reacquire_via_registry("fs");
        if save_report(ctx, ppath, data) { return true; }
        if ctx.datetime().epoch_secs() - t0 >= CHAOS_SAVE_TOTAL_SECS { return false; }
        for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    }
}
const CARNAGE_MAX_SVC: usize = 16;        // distinct services tracked in the aggregate tally (~6–8 real)
// max-carnage takes NO round cap: it runs exactly the count you type. The report is per-SERVICE
// AGGREGATES (killed/recovered counts), constant memory regardless of round count, and each round
// reclaims the dead instance before respawning - so the round count is a loop counter, not a resource
// (§26.6 bounds resources, not counters; same reasoning as the unbounded supervisor respawn, §6.2).
// The only bound is the parsed `u32` and `q`, which aborts early. (kill-storm DOES cap rounds at
// CHAOS_MAX_ROUNDS because it stores per-round generation detail in fixed arrays.)

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
    if ntok == 0 {
        ctx.console_writeln("usage: chaos kill-storm <service> [rounds] [save <path>]   (service: supervisor | block-driver | fs)");
        ctx.console_writeln("       chaos flood-storm <service> [rounds]                (saturate its queue with try_send; verify it drains + stays alive)");
        ctx.console_writeln("       chaos max-carnage [rounds] [save <path>]            (kill a RANDOM live service each round - all but the shell)");
        return Err(ShellError::Unknown);
    }
    match tok[0] {
        "kill-storm"  => chaos_kill_storm(ctx, cwd, &tok, ntok),
        "flood-storm" => chaos_flood_storm(ctx, cwd, &tok, ntok),
        "max-carnage" => chaos_max_carnage(ctx, cwd, &tok, ntok),
        other => {
            ctx.console_writeln_fmt(format_args!(
                "chaos: unknown mode '{}' (try: chaos kill-storm <service> [rounds])", other));
            Err(ShellError::Unknown)
        }
    }
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
    let mut ok_r  = [false; CHAOS_MAX_ROUNDS as usize]; // service survived this round
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
            if let Some(nh) = ctx.acquire_send_cap(svc) { handle = nh; }
            continue;
        }
        // 3. Responsive? a send should land now that the queue has drained. EndpointDead = it died.
        match ctx.try_send_by_handle(handle, &msg) {
            Ok(()) | Err(IpcError::QueueFull) => { ok_r[r] = true; survived += 1; }
            Err(IpcError::EndpointDead)       => {
                if died_at.is_none() { died_at = Some(r as u32 + 1); }
                if let Some(nh) = ctx.acquire_send_cap(svc) { handle = nh; }
            }
            Err(_)                            => {}
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
        } else {
            let _ = writeln!(rb, "round {:>3}: depth {} -> service DIED (EndpointDead) - flood not absorbed", r + 1, depth[r]);
        }
    }
    // Final responsiveness check: is the service still accepting after the whole storm?
    let final_alive = match ctx.acquire_send_cap(svc) {
        Some(fh) => !matches!(ctx.try_send_by_handle(fh, &msg), Err(IpcError::EndpointDead)),
        None     => false,
    };
    let _ = writeln!(rb, "survived: {}/{}; final responsive: {}; kernel: alive (no panic - this command returned)",
                     survived, rounds, if final_alive { "yes" } else { "no" });
    if let Some(d) = died_at {
        let _ = writeln!(rb, "note: first flood-induced death at round {} (if restartable, it respawned)", d);
    }
    let pass = survived == rounds && final_alive;
    let _ = writeln!(rb, "verdict: {}", if pass {
        "PASS (queue saturated + service drained + stayed alive)"
    } else {
        "FAIL (a flood was not absorbed)"
    });
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }
    console_write_chunked(ctx, rb.bytes());
    if pass { Ok(()) } else { Err(ShellError::Unknown) }
}

/// xorshift64 - a tiny, fast PRNG. Not cryptographic; just enough to pick victims at random.
fn xorshift64(mut x: u64) -> u64 {
    x ^= x << 13; x ^= x >> 7; x ^= x << 17; x
}

/// One flood pass for `max-carnage`: get-or-reuse a cached SEND cap to `name`, then burst `try_send`
/// (never blocking `send`, §8.9) until the queue saturates (`QueueFull`), the service dies
/// (`EndpointDead`), or we hit the burst cap. Returns `(sends_landed, saturated, died)`, or `None` if
/// the service has no reachable recv endpoint (a pure sender, or the cap table is full). The cache -
/// one handle per service - bounds cap use to the handful of live services, so a million-round storm
/// keeps flooding instead of exhausting the 64-slot cap table after ~64 acquires.
fn carnage_flood(ctx: &ServiceContext, name: &str, cache: &mut Option<CapHandle>) -> Option<(u32, bool, bool)> {
    const BURST: u32 = 64; // > queue depth (16) so saturation shows
    let h = match *cache {
        Some(h) => h,
        None => match ctx.acquire_send_cap(name) {
            Some(h) => { *cache = Some(h); h }
            None    => return None,
        }
    };
    let msg = Message::from_bytes(&[0x01]); // minimal benign payload; the target drains + drops it
    let (mut sent, mut sat, mut died) = (0u32, false, false);
    while sent < BURST {
        match ctx.try_send_by_handle(h, &msg) {
            Ok(())                      => sent += 1,
            Err(IpcError::QueueFull)    => { sat  = true; break; }
            Err(IpcError::EndpointDead) => { died = true; *cache = None; break; }
            Err(_)                      => break,
        }
    }
    Some((sent, sat, died))
}

/// `chaos max-carnage [rounds] [save <path>]` - the chaos monkey. Each round, snapshot the LIVE task
/// set (exactly what `observe now` shows), pick one at **random**, and kill it - everything is fair
/// game **except the shell itself** (killing it would kill this command) and the **kernel** (which is
/// not a task and cannot be killed). Recoverable victims (supervisor/block-driver/fs) are confirmed
/// back up; the rest stay dead (nothing auto-restarts them - expected). The headline proof is that the
/// **kernel survives ANY sequence of random service deaths**: the command returning at all means no
/// panic (a panic reboots). Random source: the TSC, advanced by xorshift64. Bounded + loud (§26.6):
/// rounds clamped 1..=100, a fixed candidate snapshot per round.
#[inline(never)]
fn chaos_max_carnage(ctx: &ServiceContext, _cwd: &Cwd, tok: &[&str], ntok: usize) -> Result<(), ShellError> {
    // [rounds] [save] after tok[0] = "max-carnage". `save` is accepted but deliberately NOT honoured:
    // max-carnage destroys fs, so writing the report TO fs is a catch-22 that fights the storm (it
    // hung/wedged the session and even cost the keyboard). The console IS the record - it is the
    // kernel's framebuffer+serial, not a service, so chaos can't touch it, and the terminal captures it.
    let mut rounds = CHAOS_DEFAULT_ROUNDS;
    let mut save_requested = false;
    let mut i = 1;
    while i < ntok {
        if tok[i] == "save" {
            save_requested = true; i += 1;
            if i < ntok && tok[i].starts_with('/') { i += 1; } // skip a stray path
        } else if let Some(n) = parse_u32(tok[i]) { rounds = n; i += 1; }
        else { i += 1; }
    }
    let rounds = rounds.max(1) as u64;   // no upper cap - run exactly what was typed (q aborts)

    // RNG seed: the TSC (high-resolution, varies run to run), mixed with the wall clock. Never zero.
    let mut rng = ctx.read_tsc()
        ^ (ctx.datetime().epoch_secs() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if rng == 0 { rng = 0xDEAD_BEEF_CAFE_F00D; }

    // Wall-clock start (RTC), for the heartbeat's % + ETA. ETA is a running average: remaining rounds
    // ÷ the rate so far (done ÷ elapsed seconds) - blank until ≥1 s has elapsed, then it settles.
    let t0 = ctx.datetime().epoch_secs();

    ctx.console_writeln_fmt(format_args!(
        "chaos max-carnage: {} rounds - kill OR FLOOD a RANDOM live service each round (all but the shell). Press q to quit.", rounds));
    if save_requested {
        ctx.console_writeln("(note: max-carnage doesn't save to disk - it destroys fs, so a save would fight the storm. The report below IS the record.)");
    }

    // Per-SERVICE aggregate tally (bounded - a handful of distinct services, NOT per-round). Constant
    // memory regardless of round count, so the run goes as long as you like and the report is always
    // COMPLETE - never "the last N rounds", nothing silently truncated.
    let mut sv_name:   [[u8; 24]; CARNAGE_MAX_SVC] = [[0u8; 24]; CARNAGE_MAX_SVC];
    let mut sv_nlen:   [usize;    CARNAGE_MAX_SVC] = [0usize;    CARNAGE_MAX_SVC];
    let mut sv_killed: [u64;      CARNAGE_MAX_SVC] = [0u64;      CARNAGE_MAX_SVC];
    let mut sv_recov:  [u64;      CARNAGE_MAX_SVC] = [0u64;      CARNAGE_MAX_SVC];
    let mut sv_flooded:  [u64;               CARNAGE_MAX_SVC] = [0u64; CARNAGE_MAX_SVC];
    let mut sv_maxdepth: [u32;               CARNAGE_MAX_SVC] = [0u32; CARNAGE_MAX_SVC];
    let mut sv_floodkill:[u64;               CARNAGE_MAX_SVC] = [0u64; CARNAGE_MAX_SVC];
    let mut sv_floodcap: [Option<CapHandle>; CARNAGE_MAX_SVC] = [None; CARNAGE_MAX_SVC]; // cached send cap/svc
    let mut nsv = 0usize;
    let mut killed = 0u64;
    let mut flooded = 0u64;
    let mut flood_saturated = 0u64;
    let mut recoverable_killed = 0u64;
    let mut recovered = 0u64;
    let mut done = 0u64;
    let mut aborted = false;

    for _ in 0..rounds {
        // `q` aborts early. The kernel buffers the keypress (it survives a momentary input-driver
        // death), so a `q` pressed any time the keyboard was up is caught here between rounds.
        if let Some(b) = ctx.try_console_read() {
            if b == b'q' || b == b'Q' { aborted = true; break; }
        }

        // Snapshot the live, killable set: valid, not Dead, named, and NOT the shell. Bounded.
        let mut cand: [([u8; 24], usize, u32); CARNAGE_MAX_CAND] = [([0u8; 24], 0usize, 0u32); CARNAGE_MAX_CAND];
        let mut ncand = 0usize;
        for slot in 0..256u32 {
            let st = ctx.task_stat(slot);
            if !st.valid || st.state == 4 { continue; }       // skip empty / Dead
            let nm = st.name_str();
            if nm.is_empty() || nm == "shell" { continue; }    // never kill ourselves
            if ncand < CARNAGE_MAX_CAND {
                let b = nm.as_bytes(); let l = b.len().min(24);
                cand[ncand].0[..l].copy_from_slice(&b[..l]);
                cand[ncand].1 = l;
                cand[ncand].2 = st.generation as u32;
                ncand += 1;
            }
        }
        if ncand == 0 { break; }                               // nothing left but the shell
        done += 1;

        rng = xorshift64(rng);
        let pick = (rng % ncand as u64) as usize;
        let nl = cand[pick].1;
        let og = cand[pick].2;
        let mut nbuf = [0u8; 24];
        nbuf[..nl].copy_from_slice(&cand[pick].0[..nl]);
        let name = str_of(&nbuf[..nl]);

        // Find-or-add the service in the per-service aggregate FIRST, so kill + flood tally to the same
        // slot and share its cached flood cap (bounded).
        let mut idx = None;
        for s in 0..nsv { if sv_name[s][..sv_nlen[s]] == nbuf[..nl] { idx = Some(s); break; } }
        let idx = match idx {
            Some(s) => Some(s),
            None if nsv < CARNAGE_MAX_SVC => {
                sv_name[nsv][..nl].copy_from_slice(&nbuf[..nl]); sv_nlen[nsv] = nl;
                let s = nsv; nsv += 1; Some(s)
            }
            None => None,
        };

        // Roll the action - the creative mix: 0 = kill, 1 = flood, 2 = flood-then-kill,
        // 3 = kill-then-flood (flood the dead/respawning endpoint - EndpointDead back-pressure / the
        // §8.6 queue-drained-on-death case). Floods that find no endpoint just no-op (None).
        rng = xorshift64(rng);
        let action = rng % 4;
        if let Some(s) = idx {
            let mut fr1 = None;
            let mut fr2 = None;
            // A. flood first (flood / flood-then-kill).
            if action == 1 || action == 2 { fr1 = carnage_flood(ctx, name, &mut sv_floodcap[s]); }
            // B. kill (kill / flood-then-kill / kill-then-flood). A kill bumps the endpoint generation,
            //    so the cached flood cap is now stale - drop it so C/next round reacquires.
            if action == 0 || action == 2 || action == 3 {
                let _ = ctx.kill(name); killed += 1; sv_killed[s] += 1; sv_floodcap[s] = None;
            }
            // C. flood the corpse/respawn (kill-then-flood) - before the recovery wait, so it lands
            //    while the endpoint is actually dead or mid-respawn (back-pressure under restart).
            if action == 3 { fr2 = carnage_flood(ctx, name, &mut sv_floodcap[s]); }
            // D. confirm recovery for any kill (recoverable services; the rest revive on a supervisor respawn).
            if action == 0 || action == 2 || action == 3 {
                if CHAOS_RESTARTABLE.contains(&name) {
                    recoverable_killed += 1;
                    if chaos_wait_recovery(ctx, name, og) { recovered += 1; sv_recov[s] += 1; }
                }
            }
            // E. tally the flood(s).
            for fr in [fr1, fr2] {
                if let Some((depth, sat, died)) = fr {
                    flooded += 1; sv_flooded[s] += 1;
                    if depth > sv_maxdepth[s] { sv_maxdepth[s] = depth; }
                    if sat  { flood_saturated += 1; }
                    if died { sv_floodkill[s] += 1; }
                }
            }
        } else {
            // Aggregate full (>16 distinct live services - won't happen). Kill, untallied.
            let _ = ctx.kill(name); killed += 1;
        }

        // Live heartbeat so the screen isn't frozen for a long run. The console is the kernel's
        // framebuffer/serial (not a service), so it survives the carnage. `\r` rewrites the line in
        // place (no scroll); the trailing spaces clear the previous, shorter count.
        if done % CARNAGE_PROGRESS_EVERY == 0 {
            // done >= CARNAGE_PROGRESS_EVERY > 0 here, so the ETA only guards on elapsed.
            let pct = done * 100 / rounds;
            let elapsed = (ctx.datetime().epoch_secs() - t0).max(0) as u64;
            if elapsed > 0 {
                let eta = (rounds - done) * elapsed / done;   // remaining / (done/elapsed) rate, seconds
                ctx.console_write_fmt(format_args!(
                    "\rmax-carnage: {} / {} ({}%) - {} kills, {} floods - ETA {}m{:02}s - kernel alive - q to quit    ",
                    done, rounds, pct, killed, flooded, eta / 60, eta % 60));
            } else {
                ctx.console_write_fmt(format_args!(
                    "\rmax-carnage: {} / {} ({}%) - {} kills, {} floods - ETA --m--s - kernel alive - q to quit    ",
                    done, rounds, pct, killed, flooded));
            }
        }
    }
    ctx.console_writeln("");   // end the in-place heartbeat line before the report

    // Settle so any restart-log burst drains before we print the report.
    for _ in 0..CHAOS_SETTLE_YIELDS { ctx.yield_cpu(); }

    use core::fmt::Write as _;
    let mut rb = ReportBuf::new();
    let _ = writeln!(rb, "=== chaos max-carnage: report ===");
    if aborted { let _ = writeln!(rb, "stopped early at round {} (you pressed q)", done); }
    let _ = writeln!(rb, "rounds: {}; kills: {}, floods: {} ({} saturated the queue)", done, killed, flooded, flood_saturated);
    // Per-service aggregate - COMPLETE for any round count (bounded memory, never truncated).
    for s in 0..nsv {
        let name = str_of(&sv_name[s][..sv_nlen[s]]);
        if CHAOS_RESTARTABLE.contains(&name) {
            let _ = writeln!(rb, "  {:<14} killed {:>5}, recovered {:>5}, flooded {:>5} (peak depth {})",
                name, sv_killed[s], sv_recov[s], sv_flooded[s], sv_maxdepth[s]);
        } else {
            let _ = writeln!(rb, "  {:<14} killed {:>5}, flooded {:>5} (peak depth {})  (revives on a supervisor respawn)",
                name, sv_killed[s], sv_flooded[s], sv_maxdepth[s]);
        }
    }
    let total_floodkill: u64 = sv_floodkill.iter().sum();
    if total_floodkill > 0 {
        let _ = writeln!(rb, "floods that crashed a service (it respawned): {}", total_floodkill);
    }
    let _ = writeln!(rb, "directly-restarted recoveries confirmed: {}/{}", recovered, recoverable_killed);
    let _ = writeln!(rb, "kernel: SURVIVED {} kills + {} floods (no panic - this command returned)", killed, flooded);
    // Survivors live now - a built-in `observe now` so the final state is in the report itself. Bounded.
    let _ = write!(rb, "survivors (live now):");
    let mut nlive = 0u32;
    for slot in 0..256u32 {
        let st = ctx.task_stat(slot);
        if st.valid && st.state != 4 {
            let nm = st.name_str();
            if !nm.is_empty() {
                nlive += 1;
                if nlive <= 16 { let _ = write!(rb, " {}", nm); }
            }
        }
    }
    if nlive > 16 { let _ = write!(rb, " ..."); }
    let _ = writeln!(rb, "  ({} live)", nlive);
    // The test is that the KERNEL survives arbitrary carnage - proven by this report existing at all
    // (a panic would have rebooted before it printed). PASS = survived; a recoverable victim missing a
    // recovery (the supervisor-downtime edge case, §6.2) is reported per-service but does not fail it.
    let _ = writeln!(rb, "verdict: PASS (kernel survived)");
    if rb.overflow { let _ = writeln!(rb, "(report truncated at {} KiB)", REPORT_MAX / 1024); }

    // No save: max-carnage destroys fs, so the console IS the record (kernel-owned, captured by the
    // terminal). The verdict + survivor count above are exactly what's needed.
    console_write_chunked(ctx, rb.bytes());

    Ok(())   // reaching here at all proves the kernel survived the carnage
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
    // §14.3). Reacquire a fresh `fs` cap via the registry and retry once; if `fs` hasn't
    // finished re-registering yet, this returns None and the next command retries.
    if ctx.reacquire_via_registry("fs") {
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
    if ctx.reacquire_via_registry("fs") {
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
