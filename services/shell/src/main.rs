// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapInfo, CapHandle, Message};

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
const FS_OK: u8 = 0;
const FS_NOTFOUND: u8 = 2;
const FS_NOFS: u8 = 3;
const LABEL_MAX: usize = 31;
const PATH_MAX: usize = 120; // fits in MAX_LINE; path_len is u8

// ── pipe output capture ────────────────────────────────────────────────────────
// When a built-in is the *producer* side of a pipe (`read /f | upper`), its text is captured
// into one message-sized buffer instead of going to the console (§26.6: bounded; loud if the
// output overflows). The captured bytes are then sent to the sink (a service endpoint or the
// `write` built-in). Only produced *text* flows through `Out`; errors always go to the console.
// End-of-stream marker a producer service sends to a built-in sink (the shell draining a
// `service | write` pipe). A non-empty sentinel — the IPC path doesn't deliver an empty body.
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
// Largest file the `write` sink can store: one WriteFile message (fs MAX_FILE_BYTES = 7×512).
// A bigger captured buffer can't reach a file until fs grows multi-block files.
const PIPE_FILE_MAX: usize = 7 * 512; // 3584
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
}
impl Out<'_> {
    /// Write a string, no trailing newline.
    fn put(&mut self, ctx: &ServiceContext, s: &str) {
        match self {
            Out::Console => ctx.console_write(s),
            Out::Capture(c) => c.push(s.as_bytes()),
        }
    }
    /// Write raw bytes, no trailing newline (file content may not be clean UTF-8).
    fn put_bytes(&mut self, ctx: &ServiceContext, b: &[u8]) {
        match self {
            Out::Console => ctx.console_write(str_of(b)),
            Out::Capture(c) => c.push(b),
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
        }
    }
}

// Entry point called by the kernel after spawning this service.
// ctx.console_writeln() appends a newline. The kernel echoes each console keystroke to the
// display (arch::console_push_byte), so we don't echo here — just accumulate
// bytes until \r or \n. (On a serial terminal, turn local echo OFF to avoid
// doubled characters.)
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // The boot sequence (kernel + every service's logs, the xHCI enumeration) is
    // shown on the TV during startup — the user wants to see it come up. We log our
    // "ready" line into that stream, then wait for the input driver to report in
    // (the deterministic end-of-boot signal) before automatically clearing the TV
    // and presenting a clean prompt — no keypress, no timer.
    for _ in 0..256 {
        ctx.yield_cpu();
    }
    ctx.console_writeln("shell: ready (type 'help')");

    wait_for_input_ready(&ctx);

    // Boot is done: dismiss the boot screen on the TV (clear + stop mirroring logs
    // to it) and present a clean prompt. Serial keeps the full stream. This is also
    // the first `gs> ` the serial-driven shell-test waits on.
    ctx.console_boot_complete();

    // The shell owns echo from here on. The kernel's auto-echo (console_push_byte)
    // can only echo single bytes blindly, so it prints the `[` and `A` of an arrow
    // key's `ESC [ A` sequence before the shell consumes them — smearing "[A" onto
    // the line. We turn kernel echo OFF and echo printable bytes ourselves below, so
    // escape sequences are swallowed silently and line editing stays under our control.
    ctx.console_echo(false);
    ctx.console_write("gs> ");

    let mut line_buf = [0u8; MAX_LINE];
    let mut line_len = 0usize;
    // Current location on the (single) drive: the directory bare/relative paths target,
    // moved by `cd` (utilities/17_cd.md). Session state; resets to "/" each boot.
    let mut cwd = Cwd::root();
    // Command history for up/down-arrow recall. `nav == hist.len()` means the live line.
    let mut hist = History::new();
    let mut nav = 0usize;

    loop {
        let b = ctx.console_read();

        match b {
            b'\r' | b'\n' => {
                // We own echo now, so move to a fresh line ourselves (the kernel used
                // to echo the Enter as "\r\n").
                ctx.console_write("\r\n");
                if line_len > 0 {
                    hist.push(&line_buf[..line_len]);
                    execute(&ctx, &line_buf[..line_len], &mut cwd);
                    line_len = 0;
                }
                nav = hist.len();
                ctx.console_write("gs> ");
            }
            0x1B => {
                // Escape sequence. Arrow keys arrive as ESC [ A/B/C/D — from a serial
                // terminal directly, and the USB keyboard emits the same (sdk hid.rs).
                // Up/Down walk the history; Left/Right are not handled yet (no in-line
                // cursor movement). The two follow-up bytes are part of the sequence.
                let b1 = ctx.console_read();
                let b2 = ctx.console_read();
                if b1 == b'[' {
                    match b2 {
                        b'A' => { // Up — older command
                            if nav > 0 {
                                nav -= 1;
                                replace_line(&ctx, &mut line_buf, &mut line_len, hist.get(nav));
                            }
                        }
                        b'B' => { // Down — newer command (past the end → blank live line)
                            if nav < hist.len() {
                                nav += 1;
                                let line: &[u8] = if nav == hist.len() { &[] } else { hist.get(nav) };
                                replace_line(&ctx, &mut line_buf, &mut line_len, line);
                            }
                        }
                        _ => {}
                    }
                }
            }
            0x7f | 0x08 => {
                // backspace — remove last byte and erase it on the display, but
                // only if there is one. The kernel does not echo backspace (it
                // can't tell the line is empty), so a no-op here leaves the prompt
                // untouched. "\x08 \x08" = move back, overwrite with space, move back.
                if line_len > 0 {
                    line_len -= 1;
                    ctx.console_write("\x08 \x08");
                }
            }
            0x03 => {
                // Ctrl-C — clear line
                ctx.console_writeln("^C");
                line_len = 0;
                nav = hist.len();
                ctx.console_write("gs> ");
            }
            b if b >= 0x20 && b < 0x7f => {
                if line_len < MAX_LINE {
                    line_buf[line_len] = b;
                    line_len += 1;
                    // Echo the printable byte ourselves (kernel echo is off). Escape
                    // sequences never reach here — they're consumed in the 0x1B arm.
                    let s = [b];
                    ctx.console_write(core::str::from_utf8(&s).unwrap_or(""));
                }
            }
            _ => {}
        }
    }
}

/// Erase the current on-screen input, then set + print `new` as the line buffer (used by
/// up/down-arrow history recall). Erasing uses the same `\x08 \x08` the backspace path does.
fn replace_line(ctx: &ServiceContext, buf: &mut [u8; MAX_LINE], len: &mut usize, new: &[u8]) {
    for _ in 0..*len {
        ctx.console_write("\x08 \x08");
    }
    let n = new.len().min(MAX_LINE);
    buf[..n].copy_from_slice(&new[..n]);
    *len = n;
    if n > 0 {
        ctx.console_write(core::str::from_utf8(&buf[..n]).unwrap_or(""));
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

/// Wait until the input subsystem reports in — the deterministic end-of-boot
/// signal. The xHCI driver sets `input_ready` once it finishes, in every terminal
/// path (keyboard up, no keyboard, or no controller), and it is the last
/// subsystem to come up. So when it reports, the boot sequence — including the
/// asynchronous xHCI enumeration on another core — is genuinely done, and we can
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
/// or `"…"` is one argument with the surrounding pair stripped — **no escapes, no nesting, no
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

fn execute(ctx: &ServiceContext, line: &[u8], cwd: &mut Cwd) {
    let Ok(s) = core::str::from_utf8(line) else {
        ctx.console_writeln("shell: invalid input");
        return;
    };
    let s = s.trim();
    if s.is_empty() { return; }

    // Capability-mediated pipe: `producer | sink`. The shell brokers the channel
    // (Appendix D.3): spawn the consumer, then spawn the producer with a SEND cap
    // to the consumer's endpoint delegated to it — the producer has no ambient
    // authority of its own.
    if s.contains('|') {
        // One unified pipeline: threads bytes or records, with from/to bridging the two worlds.
        pipe_run(ctx, cwd, s);
        return;
    }

    let mut args = [""; MAX_ARGS];
    let argc = tokenize(s, &mut args);
    if argc == 0 { return; }

    // Per-utility `help` / `version` (0_conventions.md): every utility self-documents.
    // `<util> help` and `<util> version` are intercepted here for every utility; subcommand
    // help (`<util> <sub> help`, e.g. `drives flash help`) is intercepted just below.
    if argc == 2 && is_util(args[0]) {
        if args[1] == "version" { util_version(ctx, args[0]); return; }
        if args[1] == "help" { util_help(ctx, args[0]); return; }
    }
    if argc == 3 && args[2] == "help" && is_util(args[0]) {
        if sub_help(ctx, args[0], args[1]) { return; }
    }

    match args[0] {
        "help"    => cmd_help(ctx),
        "clear"   => cmd_clear(ctx),
        "echo"    => cmd_echo(ctx, strip_quotes(s["echo".len()..].trim()), &mut Out::Console),
        "about"   => cmd_about(ctx),
        "mem"     => cmd_mem(ctx),
        "cores"   => cmd_cores(ctx),
        "date"    => cmd_date(ctx, if argc >= 2 { args[1] } else { "" }),
        "status"  => cmd_status(ctx),
        "observe" => {
            if argc >= 2 && args[1] == "now" {
                cmd_observe_now(ctx);
            } else {
                cmd_observe_live(ctx);
            }
        }
        "caps"    => {
            // No argument → show the shell's OWN capabilities (authority is
            // explicit; the shell can inspect itself like any other service).
            if argc < 2 { cmd_caps(ctx, "shell"); }
            else { cmd_caps(ctx, args[1]); }
        }
        "spawn"   => {
            if argc < 2 { ctx.console_writeln("usage: spawn <name>"); }
            else { cmd_spawn(ctx, args[1]); }
        }
        "kill"    => {
            if argc < 2 { ctx.console_writeln("usage: kill <name>"); }
            else { cmd_kill(ctx, args[1]); }
        }
        "restart" => {
            if argc < 2 { ctx.console_writeln("usage: restart <name> [core]"); }
            else {
                let core = if argc >= 3 { parse_u32(args[2]) } else { None };
                cmd_restart(ctx, args[1], core);
            }
        }
        "reboot"  => cmd_reboot(ctx),
        "drives"  => cmd_drives(ctx, &args, argc),
        "ls"      => cmd_ls(ctx, cwd, if argc >= 2 { args[1] } else { "" }, &mut Out::Console),
        "read"    => {
            if argc < 2 { ctx.console_writeln("usage: read <path>"); }
            else { cmd_read(ctx, cwd, args[1], &mut Out::Console); }
        }
        "write"   => cmd_write(ctx, cwd, s["write".len()..].trim()),
        "mkdir"   => {
            // `mkdir <path>` or `mkdir <path> parents` (create missing parent dirs).
            if argc < 2 { ctx.console_writeln("usage: mkdir <path> [parents]"); }
            else { cmd_mkdir(ctx, cwd, args[1], argc >= 3 && args[2] == "parents"); }
        }
        "cd"      => cmd_cd(ctx, cwd, if argc >= 2 { args[1] } else { "/" }),
        "copy"    => {
            // `copy <src> <dst>` (file) or `copy <src> <dst> recursive` (whole subtree).
            if argc < 3 { ctx.console_writeln("usage: copy <src> <dst> [recursive]"); }
            else if argc >= 4 && args[3] == "recursive" { cmd_copy_tree(ctx, cwd, args[1], args[2]); }
            else { cmd_copy(ctx, cwd, args[1], args[2]); }
        }
        "rename"  => {
            if argc < 3 { ctx.console_writeln("usage: rename <path> <newname>"); }
            else { cmd_rename(ctx, cwd, args[1], args[2]); }
        }
        "delete"  => {
            // `delete <path>` (file/empty dir) or `delete <path> recursive` (whole subtree).
            if argc < 2 { ctx.console_writeln("usage: delete <path> [recursive]"); }
            else { cmd_delete(ctx, cwd, args[1], argc >= 3 && args[2] == "recursive"); }
        }
        "move"    => {
            if argc < 3 { ctx.console_writeln("usage: move <src> <dst>"); }
            else { cmd_move(ctx, cwd, args[1], args[2]); }
        }
        "find"    => {
            if argc < 2 { ctx.console_writeln("usage: find <name> [path]"); }
            else { cmd_find(ctx, cwd, args[1], if argc >= 3 { args[2] } else { "/" }, &mut Out::Console); }
        }
        "tree"    => cmd_tree(ctx, cwd, if argc >= 2 { args[1] } else { "" }, &mut Out::Console),
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
    "help",
    "echo", "clear", "about", "mem", "cores", "date", "status", "observe", "caps",
    "spawn", "kill", "restart", "reboot", "drives", "ls", "cd", "read", "write",
    "mkdir", "copy", "move", "rename", "delete", "find", "tree", "match", "count", "sort",
    "first", "last",
    // record-pipe verbs (pipe-only stages; see docs/records.md)
    "where", "select", "to", "from",
];
fn is_util(name: &str) -> bool { UTILS.contains(&name) }

/// `<util> version` — version number, then creator credit.
fn util_version(ctx: &ServiceContext, util: &str) {
    ctx.console_writeln_fmt(format_args!("{} {}", util, UTIL_VERSION));
    ctx.console_writeln("Created by Bankole Ogundero.");
}

/// One usage row: (signature with `<placeholders>`, description, a real example).
type Row = (&'static str, &'static str, &'static str);

/// Render the standard help block: `<title> <ver> — <desc>`, each usage row followed by a
/// real example, then (for a top-level utility) the version/help footer.
fn help_block(ctx: &ServiceContext, title: &str, desc: &str, rows: &[Row], footer: bool) {
    ctx.console_writeln_fmt(format_args!("{} {} — {}", title, UTIL_VERSION, desc));
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

/// `<util> help` — usage with examples. Returns false for an unknown name.
fn util_help(ctx: &ServiceContext, util: &str) -> bool {
    match util {
        "help" => help_block(ctx, "help", "list all commands (or get help on one)", &[
            ("help", "the full categorised command list", "help"),
            ("<command> help", "usage + examples for one command", "status help"),
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
        "status" => help_block(ctx, "status", "list all live tasks", &[
            ("status", "slot, name, core, state of every task", "status"),
        ], true),
        "observe" => help_block(ctx, "observe", "live system metrics view", &[
            ("observe", "full-screen live view (q to quit)", "observe"),
            ("observe now", "one-shot metrics frame", "observe now"),
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
        "drives" => help_block(ctx, "drives", "manage attached disks (records when piped)", &[
            ("drives", "list attached drive(s)", "drives"),
            ("drives | <verb>", "piped: records index/label/status/size_mib/free_mib", "drives | where free_mib>0"),
            ("drives flash [drive] [label]", "format a drive as GSFS (ERASES)", "drives flash 0 data"),
            ("drives label [drive] <name>", "name / rename a drive", "drives label 0 archive"),
            ("drives reset [drive]", "un-format a drive back to raw", "drives reset 0"),
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
        "write" => help_block(ctx, "write", "create, overwrite, or append to a file", &[
            ("write <path>", "create an empty file", "write /docs/todo.txt"),
            ("write <path> <text>", "create/overwrite with text", "write /docs/todo.txt \"buy milk\""),
            ("write append <path> <text>", "add text to the end (create if missing)", "write append /docs/todo.txt \"eggs\""),
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

/// `<util> <sub> help` — focused help for a subcommand. Returns false if not a subcommand.
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
        _ => return false,
    }
    true
}

fn cmd_help(ctx: &ServiceContext) {
    // Rule 6 (0_conventions.md): help output's first line is `<util> <version>`.
    ctx.console_writeln_fmt(format_args!("help {} — GodspeedOS shell commands", UTIL_VERSION));
    ctx.console_writeln("");
    ctx.console_writeln("Console");
    help_line(ctx, "help", "show this message");
    help_line(ctx, "clear", "clear the screen");
    help_line(ctx, "echo <text>", "print text");
    ctx.console_writeln("");
    ctx.console_writeln("System");
    help_line(ctx, "about", "identity + credits");
    help_line(ctx, "cores", "CPU core count");
    help_line(ctx, "mem", "physical memory usage");
    help_line(ctx, "date [epoch]", "date + time; 'epoch' = secs since 1970");
    ctx.console_writeln("");
    ctx.console_writeln("Services");
    help_line(ctx, "status", "list all live tasks");
    help_line(ctx, "observe [now]", "live view (q to quit) / one-shot frame");
    help_line(ctx, "caps [service]", "capabilities (default: this shell)");
    help_line(ctx, "spawn <name>", "start a service");
    help_line(ctx, "kill <name>", "stop a service");
    help_line(ctx, "restart <name> [core]", "restart a service");
    ctx.console_writeln("");
    ctx.console_writeln("Storage");
    help_line(ctx, "drives [flash|label|reset]", "manage attached disks (drives help)");
    help_line(ctx, "ls [path]", "list a directory");
    help_line(ctx, "cd [path|-]", "change directory (- = previous)");
    help_line(ctx, "read <path>", "print a file");
    help_line(ctx, "write [append] <path> [text]", "create/overwrite/append a file");
    help_line(ctx, "mkdir <path> [parents]", "create a directory");
    help_line(ctx, "copy <src> <dst> [recursive]", "copy a file or subtree");
    help_line(ctx, "move <src> <dst>", "relocate a file/dir");
    help_line(ctx, "rename <path> <name>", "rename an entry in place");
    help_line(ctx, "delete <path> [recursive]", "remove a file/dir/subtree");
    help_line(ctx, "find <pattern> [path]", "search by name (substring or *? glob)");
    help_line(ctx, "tree [path]", "print the directory hierarchy");
    help_line(ctx, "match <pattern> [path]", "keep lines matching (also: <prod> | match)");
    help_line(ctx, "count [path]", "count lines/words/bytes (also: <prod> | count)");
    help_line(ctx, "sort [reverse] [path]", "order lines (also: <prod> | sort)");
    help_line(ctx, "first / last [N] [path]", "keep first/last N lines (also: <prod> |)");
    ctx.console_writeln("");
    ctx.console_writeln("Pipes");
    help_line(ctx, "<producer> | [filter |…] <sink>", "compose stages (Appendix D)");
    help_line(ctx, "  e.g. read /f | upper", "filter a file through a service");
    help_line(ctx, "  e.g. tree / | write /out", "capture output to a file");
    help_line(ctx, "  e.g. greet | upper | write /g", "producer | filter | sink");
    ctx.console_writeln("");
    ctx.console_writeln("Records (typed pipes — docs/records.md)");
    help_line(ctx, "status | where mem>0", "filter the task table by field (=,!=,>,<,~)");
    help_line(ctx, "status | select name state", "keep only some columns");
    help_line(ctx, "status | sort [reverse] mem", "order rows by a column");
    help_line(ctx, "status | to json | to yaml", "render the table (default: a grid)");
    ctx.console_writeln("");
    ctx.console_writeln("Power");
    help_line(ctx, "reboot", "hardware reset");
    ctx.console_writeln("");
    ctx.console_writeln("Type '<command> help' for usage + examples, '<command> version' for the version.");
}

/// One "  command  description" row. The command is left-justified to a fixed
/// width with format padding so every description column lines up exactly — no
/// hand-counted spaces, and ASCII-only so it renders identically on the TV
/// framebuffer (whose font is ASCII) and a serial terminal.
fn help_line(ctx: &ServiceContext, cmd: &str, desc: &str) {
    ctx.console_writeln_fmt(format_args!("  {:<21}  {}", cmd, desc));
}

/// Clear the screen. Emits ANSI erase-display + cursor-home: the framebuffer
/// console honours `ESC[2J` (clear + home) and `ESC[H`, and a serial terminal
/// does too, so both surfaces clear. The shell loop reprints the prompt after.
fn cmd_clear(ctx: &ServiceContext) {
    ctx.console_write("\x1b[2J\x1b[H");
}

/// Print the rest of the line verbatim.
fn cmd_echo(ctx: &ServiceContext, text: &str, out: &mut Out) {
    out.line(ctx, text);
}

/// One-line identity for the system.
fn cmd_about(ctx: &ServiceContext) {
    ctx.console_writeln("GodspeedOS: a capability-based microkernel (v1 milestone)");
    ctx.console_writeln_fmt(format_args!("  running on {} core(s)", ctx.inspect_core_count()));
    ctx.console_writeln("  Created by Bankole Ogundero.");
}

/// Physical-memory usage, straight from the kernel's frame allocator (held via
/// the INTROSPECT cap). Frames are 4 KiB pages: KiB = frames*4, MiB = frames/256.
/// The percentage is computed in hundredths (two decimals, integer math) so the
/// microkernel's tiny footprint shows as e.g. 0.03% rather than rounding to 0%.
fn cmd_mem(ctx: &ServiceContext) {
    let total = ctx.inspect_kernel_total_frames();
    let free = ctx.inspect_kernel_free_frames();
    let used = total.saturating_sub(free);
    let pct_h = if total > 0 { used * 10000 / total } else { 0 }; // 0.01% units
    ctx.console_writeln_fmt(format_args!(
        "mem: {} KiB used / {} MiB total ({}.{:02}% used, {} MiB free)",
        used * 4, total / 256, pct_h / 100, pct_h % 100, free / 256));
}

fn cmd_reboot(ctx: &ServiceContext) -> ! {
    ctx.console_writeln("rebooting...");
    ctx.reboot()
}

fn cmd_cores(ctx: &ServiceContext) {
    let n = ctx.inspect_core_count();
    let mut buf = [0u8; 32];
    let mut pos = 0usize;
    write_bytes(&mut buf, &mut pos, b"cores: ");
    write_u32(&mut buf, &mut pos, n);
    ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or("?"));
}

/// Wall-clock date+time from the hardware RTC. Default renders a full timestamp
/// with weekday, e.g. `Sat 2026-06-06 22:05:09`. `date epoch` prints seconds since
/// 1970-01-01 instead. Deliberately just these two forms — no clock-setting, format
/// strings, or timezones (§26.2: minimal surface). The subcommand is `epoch`, not
/// `unix`: this is not POSIX, so the vocabulary doesn't borrow its name.
fn cmd_date(ctx: &ServiceContext, arg: &str) {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let dt = ctx.datetime();
    if arg == "epoch" {
        ctx.console_writeln_fmt(format_args!("{}", dt.epoch_secs()));
    } else {
        let wd = WEEKDAYS[(dt.weekday() as usize) % 7];
        ctx.console_writeln_fmt(format_args!(
            "{} {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            wd, dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second));
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Structured records — a typed TABLE is the canonical pipe model (docs/records.md).
//
// POSIX pipes flatten structured data to text, which is why grep/awk/cut/sed must re-parse
// it. Here a producer like `status` emits a TABLE (typed columns + rows); filters operate on
// fields (`where mem > 0`), and JSON/table are *renderings* of the model, not the model
// itself. The table is the language between utilities; formats are views at the edge.
//
// Bounded (§26.6): fixed columns, rows, and a string arena — all on the stack, no heap. For
// now the only producer is `status` and the chain runs entirely in-process (no service
// boundary, so no wire codec yet). Heterogeneous records, more producers, `select`/`sort by`,
// and `to yaml` are the documented next steps.
// ════════════════════════════════════════════════════════════════════════════════
const REC_MAX_COLS: usize = 8;
const REC_MAX_ROWS: usize = 64;
const REC_ARENA: usize = 4 * 1024; // backing store for Str values

/// A typed cell. `Str` points into the owning `Table`'s arena (no lifetimes, no heap).
#[derive(Clone, Copy)]
enum Value {
    Str { off: u32, len: u32 },
    Int(u64),
    Empty,
}

const REC_COL_NAME: usize = 24; // max column-name length

/// A bounded table: owned column names (so a `from json` parse can name columns dynamically,
/// not just compile-time producers), rows of `Value`, and a byte arena holding the `Str` cells.
/// The canonical structured-pipe value.
struct Table {
    col_names: [[u8; REC_COL_NAME]; REC_MAX_COLS],
    col_lens: [u8; REC_MAX_COLS],
    ncols: usize,
    rows: [[Value; REC_MAX_COLS]; REC_MAX_ROWS],
    nrows: usize,
    arena: [u8; REC_ARENA],
    alen: usize,
    overflow: bool,
}
impl Table {
    fn new(cols: &[&str]) -> Self {
        let mut t = Table {
            col_names: [[0u8; REC_COL_NAME]; REC_MAX_COLS], col_lens: [0; REC_MAX_COLS], ncols: 0,
            rows: [[Value::Empty; REC_MAX_COLS]; REC_MAX_ROWS], nrows: 0,
            arena: [0u8; REC_ARENA], alen: 0, overflow: false,
        };
        for c in cols { t.add_col(c.as_bytes()); }
        t
    }
    /// Add a column by name; returns its index (or `None` if full / name too long).
    fn add_col(&mut self, name: &[u8]) -> Option<usize> {
        if self.ncols >= REC_MAX_COLS || name.len() > REC_COL_NAME { self.overflow = true; return None; }
        let i = self.ncols;
        self.col_names[i][..name.len()].copy_from_slice(name);
        self.col_lens[i] = name.len() as u8;
        self.ncols += 1;
        Some(i)
    }
    fn col_name(&self, c: usize) -> &[u8] { &self.col_names[c][..self.col_lens[c] as usize] }
    /// Copy bytes into the arena and return a `Str` value (or `Empty` if the arena is full).
    fn intern(&mut self, s: &[u8]) -> Value {
        if self.alen + s.len() > REC_ARENA { self.overflow = true; return Value::Empty; }
        let off = self.alen as u32;
        self.arena[self.alen..self.alen + s.len()].copy_from_slice(s);
        self.alen += s.len();
        Value::Str { off, len: s.len() as u32 }
    }
    /// Append a row (values in column order). Loud-bounded: extra rows set `overflow`.
    fn add_row(&mut self, vals: &[Value]) {
        if self.nrows >= REC_MAX_ROWS { self.overflow = true; return; }
        for (i, v) in vals.iter().take(self.ncols).enumerate() { self.rows[self.nrows][i] = *v; }
        self.nrows += 1;
    }
    fn col_index(&self, name: &str) -> Option<usize> {
        (0..self.ncols).find(|&i| self.col_name(i) == name.as_bytes())
    }
    /// Resolve a cell's text: a `Str` from the arena, or the empty slice for non-strings.
    fn cell_str<'a>(&'a self, v: Value) -> &'a [u8] {
        match v {
            Value::Str { off, len } => &self.arena[off as usize..(off + len) as usize],
            _ => &[],
        }
    }
}

/// Render a table as an aligned text grid (the default view). Two passes: column widths, then
/// the header and rows. Output goes through `Out` so it works to the console or a capture.
/// String cells render in full (via the arena) — never through the 24-byte `fmt_cell` scratch —
/// so a long value like a `find` path is not silently clipped (§3.12). `saturating_sub` guards
/// the pad width defensively (a width pass and an output pass that ever disagreed must not
/// underflow into a multi-GB space run).
fn render_table(ctx: &ServiceContext, t: &Table, out: &mut Out) {
    let mut w = [0usize; REC_MAX_COLS];
    for c in 0..t.ncols { w[c] = t.col_name(c).len(); }
    for r in 0..t.nrows {
        for c in 0..t.ncols {
            let n = cell_width(t, t.rows[r][c]);
            if n > w[c] { w[c] = n; }
        }
    }
    // header
    for c in 0..t.ncols {
        out.put_bytes(ctx, t.col_name(c));
        pad(ctx, out, w[c].saturating_sub(t.col_name(c).len()) + 2);
    }
    out.put(ctx, "\n");
    // rows
    let mut scratch = [0u8; 24];
    for r in 0..t.nrows {
        for c in 0..t.ncols {
            let n = match t.rows[r][c] {
                Value::Str { .. } => { let s = t.cell_str(t.rows[r][c]); out.put_bytes(ctx, s); s.len() }
                v => { let n = fmt_cell(t, v, &mut scratch); out.put_bytes(ctx, &scratch[..n]); n }
            };
            pad(ctx, out, w[c].saturating_sub(n) + 2);
        }
        out.put(ctx, "\n");
    }
}

/// Display width of a cell: a string's full arena length, else its formatted (numeric) length.
fn cell_width(t: &Table, v: Value) -> usize {
    match v {
        Value::Str { len, .. } => len as usize,
        Value::Int(_) => { let mut b = [0u8; 24]; fmt_cell(t, v, &mut b) }
        Value::Empty => 0,
    }
}

/// Format one cell into `buf`, returning its length. Strings copy out; ints are decimal.
fn fmt_cell(t: &Table, v: Value, buf: &mut [u8; 24]) -> usize {
    match v {
        Value::Str { .. } => {
            let s = t.cell_str(v);
            let n = s.len().min(buf.len());
            buf[..n].copy_from_slice(&s[..n]);
            n
        }
        Value::Int(i) => {
            let mut tmp = [0u8; 20];
            let mut p = tmp.len();
            let mut x = i;
            loop { p -= 1; tmp[p] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
            let n = tmp.len() - p;
            buf[..n].copy_from_slice(&tmp[p..]);
            n
        }
        Value::Empty => 0,
    }
}

fn pad(ctx: &ServiceContext, out: &mut Out, n: usize) {
    for _ in 0..n { out.put(ctx, " "); }
}

/// Render a table as a JSON array of objects — `to json`, the interop/edge rendering. Task
/// names are simple ASCII (no escaping needed yet; a real escaper is a documented follow-up).
fn render_json(ctx: &ServiceContext, t: &Table, out: &mut Out) {
    out.put(ctx, "[\n");
    for r in 0..t.nrows {
        out.put(ctx, "  {");
        for c in 0..t.ncols {
            if c > 0 { out.put(ctx, ", "); }
            out.put(ctx, "\"");
            out.put_bytes(ctx, t.col_name(c));
            out.put(ctx, "\": ");
            match t.rows[r][c] {
                Value::Int(_) => {
                    let mut b = [0u8; 24];
                    let n = fmt_cell(t, t.rows[r][c], &mut b);
                    out.put_bytes(ctx, &b[..n]);
                }
                Value::Empty  => out.put(ctx, "null"),
                Value::Str { .. } => {
                    out.put(ctx, "\"");
                    out.put_bytes(ctx, t.cell_str(t.rows[r][c]));
                    out.put(ctx, "\"");
                }
            }
        }
        out.put(ctx, if r + 1 < t.nrows { "},\n" } else { "}\n" });
    }
    out.put(ctx, "]\n");
}

/// Apply `where <col> <op> <value>` to a table in place (keep matching rows). Ops: `=` `!=`
/// `>` `<` `~`(contains). Numeric comparison when both sides parse as numbers, else byte/text.
/// Unknown column or missing operands → loud no-op (the table is left unchanged).
fn table_where(ctx: &ServiceContext, t: &mut Table, col: &str, op: &str, val: &str) {
    let ci = match t.col_index(col) {
        Some(i) => i,
        None => { ctx.console_writeln_fmt(format_args!("where: no such column '{}'", col)); return; }
    };
    let mut keep = 0usize;
    for r in 0..t.nrows {
        if row_matches(t, r, ci, op, val) {
            if keep != r { t.rows[keep] = t.rows[r]; }
            keep += 1;
        }
    }
    t.nrows = keep;
}

/// Does row `r`'s column `ci` satisfy `<op> val`? Numeric if both are numbers, else textual.
fn row_matches(t: &Table, r: usize, ci: usize, op: &str, val: &str) -> bool {
    let cell = t.rows[r][ci];
    // Numeric path: cell is an Int (or numeric string) and val parses as a number.
    let cell_num = match cell {
        Value::Int(i) => Some(i),
        Value::Str { .. } => core::str::from_utf8(t.cell_str(cell)).ok().and_then(|s| s.parse::<u64>().ok()),
        Value::Empty => None,
    };
    if let (Some(cn), Ok(vn)) = (cell_num, val.parse::<u64>()) {
        return match op {
            "=" | "==" => cn == vn,
            "!=" => cn != vn,
            ">" => cn > vn,
            "<" => cn < vn,
            ">=" => cn >= vn,
            "<=" => cn <= vn,
            _ => false,
        };
    }
    // Textual path.
    let cs = t.cell_str(cell);
    let vb = val.as_bytes();
    match op {
        "=" | "==" => cs == vb,
        "!=" => cs != vb,
        "~" => contains(cs, vb),
        _ => false,
    }
}

/// Parse a compact predicate token `col<op>val` (e.g. `mem>0`, `state=BlockRecv`, `name!=x`).
/// The operator is the longest match (two-char `!=`/`>=`/`<=`/`==` before single `=`/`>`/`<`/
/// `~`); everything before it is the column, everything after the value. `None` if no operator.
fn parse_predicate(tok: &str) -> Option<(&str, &str, &str)> {
    for op in ["!=", ">=", "<=", "=="] {
        if let Some(i) = tok.find(op) { return Some((&tok[..i], op, &tok[i + op.len()..])); }
    }
    for op in ["=", ">", "<", "~"] {
        if let Some(i) = tok.find(op) { return Some((&tok[..i], &tok[i..i + 1], &tok[i + 1..])); }
    }
    None
}

/// `select <col…>` — keep only the named columns, in the given order. Returns false (loudly) on
/// an unknown column. Rows are rewritten in place; the arena (string storage) is untouched.
fn table_select(ctx: &ServiceContext, t: &mut Table, names: &[&str]) -> bool {
    let mut new_names = [[0u8; REC_COL_NAME]; REC_MAX_COLS];
    let mut new_lens = [0u8; REC_MAX_COLS];
    let mut map = [0usize; REC_MAX_COLS];
    let mut nc = 0usize;
    for &name in names {
        if name.is_empty() { continue; }
        match t.col_index(name) {
            Some(oi) if nc < REC_MAX_COLS => {
                new_names[nc] = t.col_names[oi];
                new_lens[nc] = t.col_lens[oi];
                map[nc] = oi;
                nc += 1;
            }
            Some(_) => {}
            None => { ctx.console_writeln_fmt(format_args!("select: no such column '{}'", name)); return false; }
        }
    }
    for r in 0..t.nrows {
        let old = t.rows[r];
        for i in 0..nc { t.rows[r][i] = old[map[i]]; }
        for i in nc..t.ncols { t.rows[r][i] = Value::Empty; }
    }
    t.col_names = new_names;
    t.col_lens = new_lens;
    t.ncols = nc;
    true
}

/// Resolve a value to its comparable bytes (arena slice for `Str`, empty otherwise).
fn val_str<'a>(v: Value, arena: &'a [u8]) -> &'a [u8] {
    match v { Value::Str { off, len } => &arena[off as usize..(off + len) as usize], _ => &[] }
}

/// Order two cells: numeric when both are ints, else by bytes.
fn cmp_values(a: Value, b: Value, arena: &[u8]) -> core::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(&y),
        _ => val_str(a, arena).cmp(val_str(b, arena)),
    }
}

/// `sort <col> [reverse]` — order rows by a column (numeric or text), descending with `reverse`.
fn table_sort(ctx: &ServiceContext, t: &mut Table, col: &str, reverse: bool) -> bool {
    let ci = match t.col_index(col) {
        Some(i) => i,
        None => { ctx.console_writeln_fmt(format_args!("sort: no such column '{}'", col)); return false; }
    };
    let n = t.nrows;
    let arena = &t.arena; // disjoint field borrow: rows is sorted mutably, arena read-only
    t.rows[..n].sort_unstable_by(|a, b| {
        let o = cmp_values(a[ci], b[ci], arena);
        if reverse { o.reverse() } else { o }
    });
    true
}

/// Render a table as YAML — a list of mappings (`to yaml`, the other edge format).
fn render_yaml(ctx: &ServiceContext, t: &Table, out: &mut Out) {
    let mut cell = [0u8; 24];
    for r in 0..t.nrows {
        for c in 0..t.ncols {
            out.put(ctx, if c == 0 { "- " } else { "  " });
            out.put_bytes(ctx, t.col_name(c));
            out.put(ctx, ": ");
            let n = fmt_cell(t, t.rows[r][c], &mut cell);
            out.put_bytes(ctx, &cell[..n]);
            out.put(ctx, "\n");
        }
    }
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

/// Producers that emit a structured TABLE rather than text. These are inherently tabular
/// (uniform rows), so in a pipe they emit records — composed with `where`/`select`/`sort <col>`,
/// not the text filters. Bare (un-piped) each still prints its normal text. `status` (task
/// roster), `ls` (dir listing), `caps` (held capabilities), `drives` (attached disks), `find`
/// (search hits) are shell-side, so no wire codec is needed — they pass by value like `status`.
fn is_record_producer(name: &str) -> bool {
    matches!(name, "status" | "ls" | "caps" | "drives" | "find")
}

/// `ls` as a record producer: directory entries as a table (`name` / `type` / `size`). Mirrors
/// `cmd_ls`'s fs parse but emits rows instead of formatted text; `size` is `Int` for files and
/// `Empty` for directories (a dir has no byte size). Errors print and return `None` (abort pipe).
///
/// `#[inline(never)]` (and on all the sibling builders): each holds a multi-KB `Table` (and
/// `build_find_table` a `PathStack` too) on its stack. Inlined into `pipe_run`, those frames
/// would inflate *every* pipeline's stack — even byte-only ones like `greet | sort` that never
/// build a record — and overflow the bounded user stack. Out-of-line, the big frame exists only
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

/// `caps` as a record producer: one row per held capability — `resource` (the target,
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

/// `drives` as a record producer: one row per attached drive — `index`, `label`, `status`
/// (`GSFS`/`raw`), `size_mib`, and `free_mib` (`Empty` for a raw, unformatted drive). Single
/// drive in step 3; mirrors `drives_list`. Sizes are in MiB (so the column name carries the
/// unit — a bare number cell can't).
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

/// `find` as a record producer: one row per match — `name`, `type` (`file`/`dir`), and the
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
            "find: search truncated — more than {} directories pending (bounded walk)", FIND_QCAP));
    }
    Some(t)
}

// ── from json — parse text into the table model (the byte→record bridge) ──────────
fn json_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t' || b[i] == b'\n' || b[i] == b'\r') { i += 1; }
    i
}

/// At a `"`, scan to the closing quote (a `\`-escaped char is skipped, not a terminator).
/// Returns (content start, content end, index past the closing quote). Escapes are passed
/// through literally — a real un-escaper is a documented follow-up.
fn json_string(b: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    if i >= b.len() || b[i] != b'"' { return None; }
    let start = i + 1;
    let mut j = start;
    while j < b.len() {
        if b[j] == b'\\' { j += 2; continue; }
        if b[j] == b'"' { return Some((start, j, j + 1)); }
        j += 1;
    }
    None
}

/// Parse a JSON array of flat objects into a Table — the `from json` bridge (text → records).
/// Bounded subset: `[ {"k": v, …}, … ]` with string / number / `true|false` / `null` values,
/// **no nesting**. The first object defines the columns; later objects fill known columns (new
/// keys are ignored). Strings intern into the table arena. Loud + `None` on malformed input.
fn parse_json_table(ctx: &ServiceContext, input: &[u8]) -> Option<Table> {
    let mut t = Table::new(&[]);
    let b = input;
    let mut i = json_ws(b, 0);
    if i >= b.len() || b[i] != b'[' { ctx.console_writeln("from json: expected a JSON array '[ … ]'"); return None; }
    i = json_ws(b, i + 1);
    if i < b.len() && b[i] == b']' { return Some(t); }
    let mut first_obj = true;
    loop {
        i = json_ws(b, i);
        if i >= b.len() || b[i] != b'{' { ctx.console_writeln("from json: expected an object '{ … }'"); return None; }
        i = json_ws(b, i + 1);
        let mut row = [Value::Empty; REC_MAX_COLS];
        // empty object?
        if i < b.len() && b[i] == b'}' { i += 1; } else {
            loop {
                i = json_ws(b, i);
                let (ks, ke, kn) = match json_string(b, i) {
                    Some(x) => x,
                    None => { ctx.console_writeln("from json: expected a \"key\""); return None; }
                };
                i = json_ws(b, kn);
                if i >= b.len() || b[i] != b':' { ctx.console_writeln("from json: expected ':'"); return None; }
                i = json_ws(b, i + 1);
                // value → Value
                let v;
                if i < b.len() && b[i] == b'"' {
                    let (vs, ve, vn) = json_string(b, i)?;
                    v = t.intern(&b[vs..ve]);
                    i = vn;
                } else if b[i..].starts_with(b"true") { v = Value::Int(1); i += 4; }
                else if b[i..].starts_with(b"false") { v = Value::Int(0); i += 5; }
                else if b[i..].starts_with(b"null") { v = Value::Empty; i += 4; }
                else if i < b.len() && (b[i] == b'-' || b[i].is_ascii_digit()) {
                    let s = i;
                    if b[i] == b'-' { i += 1; }
                    while i < b.len() && b[i].is_ascii_digit() { i += 1; }
                    // nested-unfriendly: a '.'/'e' (float) is stored as text
                    if i < b.len() && (b[i] == b'.' || b[i] == b'e' || b[i] == b'E') {
                        while i < b.len() && !matches!(b[i], b',' | b'}' | b' ' | b'\t' | b'\n' | b'\r') { i += 1; }
                        v = t.intern(&b[s..i]);
                    } else {
                        v = core::str::from_utf8(&b[s..i]).ok().and_then(|x| x.parse::<u64>().ok())
                            .map(Value::Int).unwrap_or(Value::Empty);
                    }
                } else {
                    ctx.console_writeln("from json: unsupported value (nested objects/arrays not supported)");
                    return None;
                }
                // map key → column (add while parsing the first object, else look up known cols)
                let key = &b[ks..ke];
                let ci = (0..t.ncols).find(|&c| t.col_name(c) == key);
                let ci = match ci {
                    Some(c) => Some(c),
                    None if first_obj => t.add_col(key),
                    None => None, // a key not seen in the first object — ignored
                };
                if let Some(ci) = ci { row[ci] = v; }
                i = json_ws(b, i);
                if i < b.len() && b[i] == b',' { i += 1; continue; }
                if i < b.len() && b[i] == b'}' { i += 1; break; }
                ctx.console_writeln("from json: expected ',' or '}'"); return None;
            }
        }
        t.add_row(&row);
        first_obj = false;
        i = json_ws(b, i);
        if i < b.len() && b[i] == b',' { i += 1; continue; }
        if i < b.len() && b[i] == b']' { return Some(t); }
        ctx.console_writeln("from json: expected ',' or ']'"); return None;
    }
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
fn pipe_run(ctx: &ServiceContext, cwd: &Cwd, line: &str) {
    let mut stages = [""; MAX_STAGES];
    let mut n = 0usize;
    for part in line.split('|') {
        let s = part.trim();
        if s.is_empty() { ctx.console_writeln("usage: <producer> | <stage> [| …]"); return; }
        if n >= MAX_STAGES { ctx.console_writeln_fmt(format_args!("pipe: too many stages (max {})", MAX_STAGES)); return; }
        stages[n] = s;
        n += 1;
    }
    if n < 2 { ctx.console_writeln("usage: <producer> | <stage> [| …]"); return; }

    // Stage 1 — produce a Stream.
    let (c0, _) = split_first(stages[0]);
    let mut s = if is_record_producer(c0) {
        let arg = split_first(stages[0]).1;
        let t = match c0 {
            "ls"     => match build_ls_table(ctx, cwd, arg)     { Some(t) => t, None => return },
            "caps"   => match build_caps_table(ctx, arg)        { Some(t) => t, None => return },
            "drives" => match build_drives_table(ctx)           { Some(t) => t, None => return },
            "find"   => match build_find_table(ctx, cwd, arg)   { Some(t) => t, None => return },
            _        => build_status_table(ctx),
        };
        // Loud on the record bound (§3.12/§26.6): a producer that overran rows/arena is reported,
        // never silently truncated — the same bar the text pipe buffer holds.
        if t.overflow {
            ctx.console_writeln_fmt(format_args!(
                "{}: result exceeded the record bound ({} rows / {} bytes) — truncated",
                c0, REC_MAX_ROWS, REC_ARENA));
        }
        Stream::Table(t)
    } else if is_producer_builtin(c0) {
        let mut cap = Cap::new();
        run_producer(ctx, cwd, stages[0], &mut Out::Capture(&mut cap));
        if cap.overflow { ctx.console_writeln("pipe: producer output exceeded the pipe buffer (truncated)"); }
        Stream::Bytes(cap)
    } else if is_pipe_producer_service(c0) {
        let mut cap = Cap::new();
        if !drain_service(ctx, c0, None, &mut cap) { return; }
        Stream::Bytes(cap)
    } else {
        ctx.console_writeln_fmt(format_args!("pipe: '{}' cannot start a pipe", c0));
        return;
    };

    // Stages 2..n — transform, with the last optionally a `write` sink.
    for i in 1..n {
        let last = i == n - 1;
        let (cmd, arg) = split_first(stages[i]);
        if cmd == "write" {
            if !last { ctx.console_writeln("pipe: write must be the last stage"); return; }
            match &s {
                Stream::Bytes(c) => pipe_write_file(ctx, cwd, arg, c.bytes()),
                Stream::Table(t) => {
                    let mut c = Cap::new();
                    render_table(ctx, t, &mut Out::Capture(&mut c));
                    pipe_write_file(ctx, cwd, arg, c.bytes());
                }
            }
            return;
        }
        if !pipe_transform(ctx, stages[i], cmd, &mut s) { return; }
    }
    // No `write` sink — render the final stream to the console.
    match &s {
        Stream::Bytes(c) => console_write_chunked(ctx, c.bytes()),
        Stream::Table(t) => render_table(ctx, t, &mut Out::Console),
    }
}

/// Write a (possibly large) byte buffer to the console. `console_write` drops anything over
/// 256 bytes, so split into ≤256-byte pieces. Output is ASCII (json/yaml/text), so chunk
/// boundaries never split a multi-byte char.
fn console_write_chunked(ctx: &ServiceContext, bytes: &[u8]) {
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + 256).min(bytes.len());
        ctx.console_write(str_of(&bytes[i..end]));
        i = end;
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
                "json" => match parse_json_table(ctx, bytes.bytes()) { Some(t) => t, None => return false },
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
            match fmt {
                "json" => render_json(ctx, t, &mut Out::Capture(&mut c)),
                "yaml" => render_yaml(ctx, t, &mut Out::Capture(&mut c)),
                _ => { ctx.console_writeln("to: unknown format (try: to json | to yaml)"); return false; }
            }
            *s = Stream::Bytes(c);
            true
        }
        // record filters (Table only)
        "where" => match s {
            Stream::Table(t) => match parse_predicate(split_first(stage).1) {
                Some((col, op, val)) => { table_where(ctx, t, col, op, val); true }
                None => { ctx.console_writeln("where: need a predicate like name=shell or mem>0"); false }
            },
            Stream::Bytes(_) => { ctx.console_writeln("where: needs records (try 'from json')"); false }
        },
        "select" => match s {
            Stream::Table(t) => {
                let mut sa = [""; MAX_ARGS];
                let sc = tokenize(stage, &mut sa);
                if sc < 2 { ctx.console_writeln("usage: … | select <col> [col …]"); return false; }
                table_select(ctx, t, &sa[1..sc])
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
                table_sort(ctx, t, col, rev)
            }
            Stream::Bytes(_) => byte_filter(ctx, stage, s),
        },
        // byte filters (Bytes only)
        "match" | "count" | "first" | "last" => match s {
            Stream::Bytes(_) => byte_filter(ctx, stage, s),
            Stream::Table(_) => { ctx.console_writeln_fmt(format_args!("{}: this is a record stream — use 'where'/'select'/'sort <col>', or 'to json' for text", cmd)); false }
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

fn cmd_status(ctx: &ServiceContext) {
    let t = build_status_table(ctx);
    render_table(ctx, &t, &mut Out::Console);
    if t.overflow {
        ctx.console_writeln_fmt(format_args!("status: more than {} rows shown (bounded)", REC_MAX_ROWS));
    }
}

/// `caps <service>` — list the capabilities a service holds. A thin broker over
/// the kernel's `task_caps` introspection (held via the INTROSPECT cap). Makes
/// authority visible on the box itself (§26.9): for each cap, the resource it
/// targets and the rights it carries.
fn cmd_caps(ctx: &ServiceContext, name: &str) {
    let slot = match slot_of(ctx, name) {
        Some(s) => s,
        None => {
            ctx.console_writeln("caps: no such live service");
            return;
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
        return;
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

/// `observe now` — broker a one-shot static metrics frame.
///
/// `observe` is a least-authority service: it holds only INTROSPECT + log caps,
/// never the shell's spawn/kill/restart. The shell spawns it; it prints one frame
/// via its own caps and parks. Kill any parked prior instance first (one-shot
/// observe has no graceful self-exit in v1), so at most one lingers.
fn cmd_observe_now(ctx: &ServiceContext) {
    let _ = ctx.kill("observe-now");
    if ctx.spawn("observe-now").is_err() {
        ctx.console_writeln("observe: failed to spawn observe-now");
        return;
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
}

/// `observe` (live) — broker the full-screen foreground view (Stage 2c).
///
/// The shell is the capability-broker (Appendix B.3): it lends the keyboard to
/// the foreground child by *not reading it* while the child runs, then takes it
/// back. We spawn `observe-live` (which owns the screen: hides the cursor,
/// suppresses echo, repaints, polls `q`), then wait — without touching
/// `console_read` — until it parks (q pressed → it restored the console and
/// parked) or dies. Then we clean up and our read loop resumes.
fn cmd_observe_live(ctx: &ServiceContext) {
    let _ = ctx.kill("observe-live"); // clear any stale instance
    if ctx.spawn("observe-live").is_err() {
        ctx.console_writeln("observe: failed to spawn observe-live");
        return;
    }
    if let Some(slot) = find_running_slot(ctx, "observe-live") {
        // Wait for the foreground child to finish. The bound is the child's
        // lifetime — it parks on `q` (state 2) or dies (invalid); the large count
        // is a paranoid safety net so a hung child can never wedge the shell
        // forever. We must NOT call console_read here: the child owns the keyboard.
        for _ in 0..u32::MAX {
            ctx.yield_cpu();
            let st = ctx.task_stat(slot);
            if !st.valid || st.state == 2 {
                break;
            }
        }
    }
    let _ = ctx.kill("observe-live"); // reap the parked instance
    // Defensive: restore the console even if the child died mid-view without
    // restoring it (cursor visible) so the shell stays usable. Echo stays OFF —
    // the shell, not the kernel, owns echo (it echoes printable bytes itself).
    ctx.console_echo(false);
    ctx.console_write("\x1b[?25h");
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

/// The trusted root (§6.1). The kernel refuses to kill these and refuses to spawn
/// a second instance; the shell explains why before the syscall is even tried.
// `registry` is no longer protected (H11 ph6): it is a restartable service, so the
// shell permits `kill registry` (the supervisor respawns it). init/supervisor remain
// the non-restartable trusted root.
const CORE_SERVICES: [&str; 2] = ["init", "supervisor"];

/// Shown when spawn/kill/restart targets a core service — "Not applicable" makes
/// it clear the command is refused *because* the target is protected, not failed.
/// Lists exactly `CORE_SERVICES`; `registry` is intentionally absent (H11 ph6:
/// it is restartable, so `kill registry` is permitted).
const PROTECTED_MSG: &str =
    "Not applicable. Core services (init, supervisor) are protected";

/// Shown when spawn/kill/restart targets an observe variant — they are brokered by
/// the `observe` / `observe now` commands, not raw service operations.
const OBSERVE_HINT: &str =
    "observe runs from a command: type 'observe' (live) or 'observe now' (snapshot)";

fn is_core_service(name: &str) -> bool {
    CORE_SERVICES.contains(&name)
}

/// `observe`'s variants are brokered by the `observe` / `observe now` commands —
/// not meant to be raw-spawned (the bare `observe` service is a serial-streaming
/// dev build that scrolls forever and ignores `q`).
fn is_observe_variant(name: &str) -> bool {
    matches!(name, "observe" | "observe-now" | "observe-live")
}

/// Services the live console session depends on for I/O. Killing/restarting them
/// from the shell would brick the very session issuing the command — a USB host
/// driver (`xhci`/`ehci`, which carry whatever input devices are attached) or the
/// shell itself. Returns the reason to show, or `None` if `name` is safe to
/// operate on. (Not a §6.2 trusted-root guard — these are restartable in
/// principle, just not from the session that needs them.)
fn session_critical_msg(name: &str) -> Option<&'static str> {
    match name {
        "xhci"  => Some("Not applicable. xhci is a USB host driver — killing it disables any input device attached to it"),
        "ehci"  => Some("Not applicable. ehci is a USB host driver — killing it disables any input device attached to it"),
        "shell" => Some("Not applicable. that is this shell — the session you are typing in"),
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

fn cmd_spawn(ctx: &ServiceContext, name: &str) {
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return;
    }
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return;
    }
    if slot_of(ctx, name).is_some() {
        report(ctx, "already running: ", name);
        return;
    }
    match ctx.spawn(name) {
        Ok(())  => report(ctx, "spawned: ", name),
        Err(_)  => report(ctx, "spawn failed (unknown service?): ", name),
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
                "pipe: stage too large ({} bytes) for the '{}' filter — max {} KiB until pipe streaming",
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
                ctx.console_writeln_fmt(format_args!("pipe: '{}' is not a filter (never registered)", svc));
                let _ = ctx.kill(svc);
                return false;
            }
        }
    }
    // Drain the service's output until EOT (bounded — a conforming service always sends it).
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
// be dead. `tree` stays text — a hierarchy is not a flat table.
fn is_producer_builtin(name: &str) -> bool {
    matches!(name, "read" | "cat" | "echo" | "tree")
}

/// Producer SERVICES that emit without needing input, so they can start a pipe (and follow the
/// EOT end-of-stream protocol). A non-producer service in stage 1 would block the shell on
/// `recv` (there is no non-blocking recv in v1), so the set is an explicit whitelist.
fn is_pipe_producer_service(name: &str) -> bool {
    matches!(name, "greet")
}

/// Run a producer built-in (`cmd args`) with its output going to `out`.
fn run_producer(ctx: &ServiceContext, cwd: &Cwd, cmdline: &str, out: &mut Out) {
    let (cmd, arg) = split_first(cmdline);
    match cmd {
        "echo"         => cmd_echo(ctx, arg, out),
        "read" | "cat" => cmd_read(ctx, cwd, arg, out),
        // "ls" and "find" are record producers (handled on the record path), not text here.
        "tree"         => cmd_tree(ctx, cwd, arg, out),
        _ => {}
    }
}

/// Write captured bytes to a file (the `write` sink). Overwrites, like plain `write`.
fn pipe_write_file(ctx: &ServiceContext, cwd: &Cwd, path_arg: &str, data: &[u8]) {
    let (pstr, _) = split_first(path_arg);
    if pstr.is_empty() { ctx.console_writeln("pipe: write needs a file path"); return; }
    // A file is one WriteFile message (fs MAX_FILE_BYTES). A bigger buffer can't be written
    // until fs supports multi-block files — say so plainly instead of a generic write failure.
    if data.len() > PIPE_FILE_MAX {
        ctx.console_writeln_fmt(format_args!(
            "pipe: {} bytes is too large to write to a file (max {} until multi-block files)",
            data.len(), PIPE_FILE_MAX));
        return;
    }
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pstr, &mut buf) { Some(p) => p, None => return };
    let mut pbuf = [0u8; PATH_MAX];
    let pl = path.len();
    pbuf[..pl].copy_from_slice(path);
    match fs_request(ctx, OP_WRITE_FILE, &pbuf[..pl], data) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) =>
            ctx.console_writeln_fmt(format_args!("piped {} bytes → {}", data.len(), str_of(&pbuf[..pl]))),
        Some(r) if no_fs(ctx, r.payload_bytes()) => {}
        Some(_) => ctx.console_writeln("pipe: write failed (bad path, or parent missing?)"),
        None    => ctx.console_writeln("pipe: storage unavailable"),
    }
}

/// Look up a just-spawned service's endpoint via the registry, retrying while it registers.
fn lookup_sink(ctx: &ServiceContext, sink: &str) -> Option<CapHandle> {
    for _ in 0..200 {
        if let Some(h) = ctx.registry_lookup(sink) { return Some(h); }
        for _ in 0..50 { ctx.yield_cpu(); }
    }
    None
}

fn cmd_kill(ctx: &ServiceContext, name: &str) {
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return;
    }
    if let Some(msg) = session_critical_msg(name) {
        ctx.console_writeln(msg);
        return;
    }
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return;
    }
    if slot_of(ctx, name).is_none() {
        report(ctx, "not running: ", name);
        return;
    }
    match ctx.kill(name) {
        Ok(())  => report(ctx, "killed: ", name),
        Err(_)  => report(ctx, "kill failed: ", name),
    }
}

fn cmd_restart(ctx: &ServiceContext, name: &str, core: Option<u32>) {
    if is_core_service(name) {
        ctx.console_writeln(PROTECTED_MSG);
        return;
    }
    if let Some(msg) = session_critical_msg(name) {
        ctx.console_writeln(msg);
        return;
    }
    if is_observe_variant(name) {
        ctx.console_writeln(OBSERVE_HINT);
        return;
    }
    match ctx.restart(name, core) {
        Ok(()) => report(ctx, "restarted: ", name),
        Err(_) => report(ctx, "restart failed: ", name),
    }
}

// ---------------------------------------------------------------------------
// File commands — ls / read / write / mkdir / cd (utilities/16..20). Shell built-ins
// that send the fs file API to `fs` over IPC; `fs` holds + enforces all disk authority.
// The shell tracks the current location (a drive+directory pointer) and resolves
// relative / `.` / `..` paths to an absolute path before sending — fs only walks
// absolute paths from root (it has no notion of "current directory").
// ---------------------------------------------------------------------------

/// The current directory on the (single) drive — an absolute path like "/" or "/etc". Also
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
    ctx.request_with_reply("fs", &Message::from_bytes(&req[..2 + pl + dn]))
}

/// True if `fs` replied "no filesystem" — print the standard hint and consume it.
fn no_fs(ctx: &ServiceContext, p: &[u8]) -> bool {
    if p.first() == Some(&FS_NOFS) {
        ctx.console_writeln("no filesystem — run 'drives flash' first");
        true
    } else {
        false
    }
}

/// `ls [path]` — list a directory.
fn cmd_ls(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_LIST_DIR, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("ls: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_NOTFOUND) || p.len() < 2 {
        ctx.console_writeln_fmt(format_args!("ls: not a directory: {}", str_of(path)));
        return;
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
}

/// `read <path>` — print a file's contents.
fn cmd_read(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_READ_FILE, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("read: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        out.put_bytes(ctx, &p[5..end]);
        if end == 0 || p[end - 1] != b'\n' { out.put(ctx, "\n"); }
    } else {
        // Errors are not pipe data — always to the console.
        ctx.console_writeln_fmt(format_args!("read: not found: {}", str_of(path)));
    }
}

/// `write <path> [content]` overwrites; `write append <path> [content]` appends (creating the
/// file if missing). `append` is a *leading* keyword because write's content is free-form — it
/// can't trail the way `mkdir … parents` does (it would be swallowed as content).
fn cmd_write(ctx: &ServiceContext, cwd: &Cwd, rest: &str) {
    // `append` counts as the keyword only when followed by whitespace or end-of-line, so a
    // path like "appendix.txt" is still treated as a path.
    let (append, rest) = match rest.strip_prefix("append") {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim_start()),
        _ => (false, rest),
    };
    if rest.is_empty() {
        ctx.console_writeln("usage: write [append] <path> [content]");
        return;
    }
    // Split off the first token (path); the remainder (with spaces) is the content. A
    // surrounding quote pair around the content is stripped (`write /f "two words"`).
    let (pstr, content) = match rest.split_once(char::is_whitespace) {
        Some((p, c)) => (p, strip_quotes(c.trim_start())),
        None => (rest, ""),
    };
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pstr, &mut buf) { Some(p) => p, None => return };
    // Copy the path out before reusing buffers (path borrows `buf`).
    let mut pbuf = [0u8; PATH_MAX];
    let pl = path.len();
    pbuf[..pl].copy_from_slice(path);
    if append {
        cmd_write_append(ctx, &pbuf[..pl], content.as_bytes());
        return;
    }
    let reply = match fs_request(ctx, OP_WRITE_FILE, &pbuf[..pl], content.as_bytes()) {
        Some(r) => r,
        None => { ctx.console_writeln("write: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) {
        ctx.console_writeln_fmt(format_args!("wrote {} ({} bytes)", str_of(&pbuf[..pl]), content.len()));
    } else {
        ctx.console_writeln("write: failed (bad path, or parent missing?)");
    }
}

/// Append `add` to file `path`, creating it if missing. Shell-side (no new fs surface): read
/// the current content, concatenate, write the whole file back. The combined size is bounded
/// by `fs`'s file-size limit, which rejects an over-large WriteFile loudly.
fn cmd_write_append(ctx: &ServiceContext, path: &[u8], add: &[u8]) {
    let mut data = [0u8; 4096];
    // Read existing content; an absent file just starts empty (append creates).
    let n_old = match fs_request(ctx, OP_READ_FILE, path, &[]) {
        Some(r) => {
            let p = r.payload_bytes();
            if no_fs(ctx, p) { return; }
            if p.first() == Some(&FS_OK) && p.len() >= 5 {
                let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
                let end = (5 + n).min(p.len());
                data[..end - 5].copy_from_slice(&p[5..end]);
                end - 5
            } else {
                0 // NOTFOUND → create a new file with just the appended text
            }
        }
        None => { ctx.console_writeln("write: storage unavailable"); return; }
    };
    if n_old + add.len() > data.len() {
        ctx.console_writeln("write: append would exceed the maximum file size");
        return;
    }
    data[n_old..n_old + add.len()].copy_from_slice(add);
    let total = n_old + add.len();
    match fs_request(ctx, OP_WRITE_FILE, path, &data[..total]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) =>
            ctx.console_writeln_fmt(format_args!("appended {} bytes to {} ({} total)", add.len(), str_of(path), total)),
        Some(r) if no_fs(ctx, r.payload_bytes()) => {}
        Some(_) => ctx.console_writeln("write: append failed (file-size limit, or bad path?)"),
        None    => ctx.console_writeln("write: storage unavailable"),
    }
}

/// `mkdir <path> [parents]` — create a directory (with `parents`, create missing parents).
fn cmd_mkdir(ctx: &ServiceContext, cwd: &Cwd, arg: &str, parents: bool) {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    let op = if parents { OP_MKDIR_P } else { OP_MKDIR };
    let reply = match fs_request(ctx, op, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("mkdir: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) {
        ctx.console_writeln_fmt(format_args!("created {}", str_of(path)));
    } else if parents {
        ctx.console_writeln("mkdir: failed (a component is in the way as a file?)");
    } else {
        ctx.console_writeln("mkdir: failed (already exists, or parent missing? try 'mkdir <path> parents')");
    }
}

/// `cd [path]` — change the current directory (validates it exists + is a directory).
fn cmd_cd(ctx: &ServiceContext, cwd: &mut Cwd, arg: &str) {
    let mut buf = [0u8; PATH_MAX];
    // `cd -` toggles to the previous directory (already an absolute, normalized path — use it
    // directly, then run the same stat-validated switch so a since-deleted dir errors loudly).
    let path: &[u8] = if arg == "-" {
        let pl = cwd.prev_len;
        buf[..pl].copy_from_slice(&cwd.prev[..pl]);
        &buf[..pl]
    } else {
        match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return }
    };
    // Root always exists — no need to stat it.
    if path == b"/" {
        cwd.set(b"/");
        ctx.console_writeln("/");
        return;
    }
    let reply = match fs_request(ctx, OP_STAT_FILE, path, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("cd: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    // STAT reply: [FS_OK, exists, size:u64, is_dir].
    if p.first() == Some(&FS_OK) && p.len() >= 11 && p[1] == 1 {
        if p[10] == 1 {
            cwd.set(path);
            ctx.console_writeln(cwd.as_str());
        } else {
            ctx.console_writeln_fmt(format_args!("cd: not a directory: {}", str_of(path)));
        }
    } else {
        ctx.console_writeln_fmt(format_args!("cd: no such directory: {}", str_of(path)));
    }
}

/// `copy <src> <dst>` — copy a file (read src, write dst). Shell-side, so it carries the
/// content through one message-sized buffer; file-only in this cut (no recursive dirs).
fn cmd_copy(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) {
    // Resolve + read the source.
    let mut sbuf = [0u8; PATH_MAX];
    let spath = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return };
    let mut sp = [0u8; PATH_MAX];
    let sl = spath.len();
    sp[..sl].copy_from_slice(spath);
    let reply = match fs_request(ctx, OP_READ_FILE, &sp[..sl], &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("copy: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() != Some(&FS_OK) || p.len() < 5 {
        ctx.console_writeln_fmt(format_args!("copy: source not found: {}", str_of(&sp[..sl])));
        return;
    }
    let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
    let end = (5 + n).min(p.len());
    let dn = end - 5;
    let mut data = [0u8; 4096];
    data[..dn].copy_from_slice(&p[5..end]);
    drop(reply);

    // Resolve + write the destination.
    let mut dbuf = [0u8; PATH_MAX];
    let dpath = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return };
    let mut dp = [0u8; PATH_MAX];
    let dl = dpath.len();
    dp[..dl].copy_from_slice(dpath);
    match fs_request(ctx, OP_WRITE_FILE, &dp[..dl], &data[..dn]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("copied {} → {} ({} bytes)", str_of(&sp[..sl]), str_of(&dp[..dl]), dn));
        }
        Some(_) => ctx.console_writeln("copy: write failed (parent missing?)"),
        None    => ctx.console_writeln("copy: storage unavailable"),
    }
}

/// `copy <src> <dst> recursive` — copy a whole subtree. Reuses the SAME bounded walk
/// (`PathStack`) `find` uses (§26.6): pop a source dir, recreate it under `dst`, then for
/// each child either copy the file (read+write, existing ops) or push the subdir. No new fs
/// surface — copy already lives in the shell. Loud if the tree is wider than the walk's cap
/// (§3.12), and refuses to copy a directory into its own subtree (would never terminate).
fn cmd_copy_tree(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) {
    let mut sbuf = [0u8; PATH_MAX];
    let src_abs = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return };
    let mut sp = [0u8; PATH_MAX];
    let sl = src_abs.len();
    sp[..sl].copy_from_slice(src_abs);
    if &sp[..sl] == b"/" { ctx.console_writeln("copy: cannot copy the root directory"); return; }

    let mut dbuf = [0u8; PATH_MAX];
    let dst_abs = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return };
    let mut dp = [0u8; PATH_MAX];
    let dl = dst_abs.len();
    dp[..dl].copy_from_slice(dst_abs);
    // Dest inside src (or equal) → the walk would copy what it just created, forever.
    if dp[..dl] == sp[..sl] || (dl > sl && dp[..sl] == sp[..sl] && dp[sl] == b'/') {
        ctx.console_writeln("copy: cannot copy into itself");
        return;
    }

    // A plain file? Fall back to the single-file copy (this command is for subtrees).
    match stat_kind(ctx, &sp[..sl]) {
        Some(false) => { cmd_copy(ctx, cwd, src, dst); return; }
        Some(true)  => {}
        None        => { ctx.console_writeln_fmt(format_args!("copy: source not found: {}", str_of(&sp[..sl]))); return; }
    }

    // Create the destination root, then walk the source breadth-first.
    if !mkdir_at(ctx, &dp[..dl]) {
        ctx.console_writeln("copy: cannot create destination (already exists?)");
        return;
    }
    let mut stack = PathStack::new();
    stack.push(&sp[..sl]);
    let (mut dirs, mut files) = (1u32, 0u32);
    let mut data = [0u8; 4096];
    while let Some(slen) = stack.pop(&mut sbuf) {
        let reply = match fs_request(ctx, OP_LIST_DIR, &sbuf[..slen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("copy: storage unavailable"); return; }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return; }
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
            } else if copy_one(ctx, &schild[..clen], &dchild[..dclen], &mut data) {
                files += 1;
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "copy: truncated — tree wider than {} pending directories (bounded walk)", FIND_QCAP));
    }
    ctx.console_writeln_fmt(format_args!(
        "copied {} → {} ({} dirs, {} files)", str_of(&sp[..sl]), str_of(&dp[..dl]), dirs, files));
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

/// Copy one file `src`→`dst` (read then write). Returns true on success; logs on failure so a
/// single bad file in a subtree copy is visible but does not abort the whole walk (§3.12).
fn copy_one(ctx: &ServiceContext, src: &[u8], dst: &[u8], data: &mut [u8; 4096]) -> bool {
    let reply = match fs_request(ctx, OP_READ_FILE, src, &[]) { Some(r) => r, None => return false };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 5 {
        ctx.console_writeln_fmt(format_args!("copy: skipped (read failed): {}", str_of(src)));
        return false;
    }
    let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
    let end = (5 + n).min(p.len());
    let dn = end - 5;
    data[..dn].copy_from_slice(&p[5..end]);
    drop(reply);
    match fs_request(ctx, OP_WRITE_FILE, dst, &data[..dn]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => true,
        _ => { ctx.console_writeln_fmt(format_args!("copy: skipped (write failed): {}", str_of(dst))); false }
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

/// `rename <path> <newname>` — rename an entry in place (not a move; newname is one
/// component). fs edits the directory entry; no blocks are read or freed.
fn cmd_rename(ctx: &ServiceContext, cwd: &Cwd, path: &str, newname: &str) {
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return };
    let mut pp = [0u8; PATH_MAX];
    let pl = abspath.len();
    pp[..pl].copy_from_slice(abspath);
    // fs_request appends `newname` after the path — exactly the OP_RENAME wire format.
    match fs_request(ctx, OP_RENAME, &pp[..pl], newname.as_bytes()) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("renamed {} → {}", str_of(&pp[..pl]), newname));
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => {}
        Some(_) => ctx.console_writeln("rename: failed (not found, or name exists, or bad name)"),
        None    => ctx.console_writeln("rename: storage unavailable"),
    }
}

/// `delete <path>` — remove a file or empty directory; `delete <path> recursive` removes a
/// whole subtree. fs does the work either way (plain = `OP_DELETE`, recursive =
/// `OP_DELETE_TREE`, a depth-bounded subtree free); it frees the blocks and reclaims them.
fn cmd_delete(ctx: &ServiceContext, cwd: &Cwd, arg: &str, recursive: bool) {
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    if path == b"/" {
        ctx.console_writeln("delete: cannot delete the root directory");
        return;
    }
    let mut pp = [0u8; PATH_MAX];
    let pl = path.len();
    pp[..pl].copy_from_slice(path);
    let op = if recursive { OP_DELETE_TREE } else { OP_DELETE };
    match fs_request(ctx, op, &pp[..pl], &[]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            let what = if recursive { "deleted (recursive)" } else { "deleted" };
            ctx.console_writeln_fmt(format_args!("{} {}", what, str_of(&pp[..pl])));
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => {}
        Some(_) if recursive => ctx.console_writeln("delete: failed (not found, or tree too deep?)"),
        Some(_) => ctx.console_writeln("delete: failed (not found, or directory not empty? use 'delete <path> recursive')"),
        None    => ctx.console_writeln("delete: storage unavailable"),
    }
}

/// `move <src> <dst>` — relocate an entry (same data; only the directory entries change).
fn cmd_move(ctx: &ServiceContext, cwd: &Cwd, src: &str, dst: &str) {
    let mut sbuf = [0u8; PATH_MAX];
    let spath = match resolve_or_err(ctx, cwd, src, &mut sbuf) { Some(p) => p, None => return };
    let mut sp = [0u8; PATH_MAX];
    let sl = spath.len();
    sp[..sl].copy_from_slice(spath);
    let mut dbuf = [0u8; PATH_MAX];
    let dpath = match resolve_or_err(ctx, cwd, dst, &mut dbuf) { Some(p) => p, None => return };
    let mut dp = [0u8; PATH_MAX];
    let dl = dpath.len();
    dp[..dl].copy_from_slice(dpath);
    // Guard against moving a directory into itself or its own subtree (would orphan it).
    if dp[..dl] == sp[..sl] || (dl > sl && dp[..sl] == sp[..sl] && dp[sl] == b'/') {
        ctx.console_writeln("move: cannot move into itself");
        return;
    }
    match fs_request(ctx, OP_MOVE, &sp[..sl], &dp[..dl]) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("moved {} → {}", str_of(&sp[..sl]), str_of(&dp[..dl])));
        }
        Some(r) if no_fs(ctx, r.payload_bytes()) => {}
        Some(_) => ctx.console_writeln("move: failed (not found, or dest exists?)"),
        None    => ctx.console_writeln("move: storage unavailable"),
    }
}

/// `find <pattern> [path]` — search a subtree (default the whole filesystem, `/`) for entries
/// matching `<pattern>`, printing each match's full path. A plain word is a substring match; a
/// pattern with `*`/`?` is a glob (anchored, whole-name). This is whole-filesystem
/// enumeration done the disciplined way: a **tree walk** (the tree IS the index, §6.4),
/// client-side via LIST_DIR so results stream as found and `fs` needs no new op. The walk
/// is bounded (a fixed pending-directory stack) and **loud on truncation** (§26.6/§3.12);
/// the `fs_index` accelerator (persistence.md §6.5) is what we'd build if this walk ever
/// gets too slow on a huge tree — not before.
fn cmd_find(ctx: &ServiceContext, cwd: &Cwd, target: &str, start: &str, out: &mut Out) {
    let mut sbuf = [0u8; PATH_MAX];
    let start_abs = match resolve_or_err(ctx, cwd, start, &mut sbuf) { Some(p) => p, None => return };
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
            None => { ctx.console_writeln("find: storage unavailable"); return; }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return; }
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
            "find: search truncated — more than {} directories pending (bounded walk)", FIND_QCAP));
    }
    ctx.console_writeln_fmt(format_args!("find: {} match(es)", matches));
}

/// `tree [path]` — print the directory hierarchy as an indented tree (default: the current
/// directory). Same bounded-walk discipline as `find` (§26.6): a fixed-capacity explicit
/// stack, depth-first, no recursion, loud on overflow (§3.12). Every child (file or dir) is
/// pushed so siblings nest correctly, and a directory's whole subtree drains before its next
/// sibling (LIFO + reverse-push). ASCII only (2 spaces per level, `/` marks directories) —
/// the framebuffer console renders no box-drawing glyphs.
fn cmd_tree(ctx: &ServiceContext, cwd: &Cwd, arg: &str, out: &mut Out) {
    let mut buf = [0u8; PATH_MAX];
    let start = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    match stat_kind(ctx, start) {
        Some(true)  => {}
        Some(false) => { out.line(ctx, str_of(start)); out.line(ctx, "0 directories, 1 file"); return; }
        None        => { ctx.console_writeln_fmt(format_args!("tree: not found: {}", str_of(start))); return; }
    }
    let mut stack = TreeStack::new();
    stack.push(start, true, 0);
    let (mut dirs, mut files) = (0u32, 0u32);
    while let Some((plen, is_dir, depth)) = stack.pop(&mut buf) {
        // Print this node: indent by depth; root shows its full path, deeper nodes their
        // basename; a trailing '/' marks a directory.
        for _ in 0..depth { out.put(ctx, "  "); }
        let name = if depth == 0 { &buf[..plen] } else { basename(&buf[..plen]) };
        if is_dir { out.line_fmt(ctx, format_args!("{}/", str_of(name))); }
        else      { out.line(ctx, str_of(name)); }
        if !is_dir { files += 1; continue; }
        if depth > 0 { dirs += 1; }

        let reply = match fs_request(ctx, OP_LIST_DIR, &buf[..plen], &[]) {
            Some(r) => r,
            None => { ctx.console_writeln("tree: storage unavailable"); return; }
        };
        let p = reply.payload_bytes();
        if no_fs(ctx, p) { return; }
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
                stack.push(&child[..clen], cdir, depth + 1);
            }
        }
    }
    if stack.overflow {
        ctx.console_writeln_fmt(format_args!(
            "tree: truncated — more than {} pending entries (bounded walk)", TREE_CAP));
    }
    out.line_fmt(ctx, format_args!(
        "{} director{}, {} file{}",
        dirs, if dirs == 1 { "y" } else { "ies" }, files, if files == 1 { "" } else { "s" }));
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
    top: usize,
    overflow: bool,
}
impl TreeStack {
    fn new() -> Self {
        TreeStack {
            buf: [[0u8; PATH_MAX]; TREE_CAP], len: [0; TREE_CAP],
            is_dir: [false; TREE_CAP], depth: [0; TREE_CAP], top: 0, overflow: false,
        }
    }
    fn push(&mut self, p: &[u8], is_dir: bool, depth: u16) {
        if self.top >= TREE_CAP || p.len() > PATH_MAX { self.overflow = true; return; }
        self.buf[self.top][..p.len()].copy_from_slice(p);
        self.len[self.top] = p.len();
        self.is_dir[self.top] = is_dir;
        self.depth[self.top] = depth;
        self.top += 1;
    }
    fn pop(&mut self, out: &mut [u8; PATH_MAX]) -> Option<(usize, bool, u16)> {
        if self.top == 0 { return None; }
        self.top -= 1;
        let l = self.len[self.top];
        out[..l].copy_from_slice(&self.buf[self.top][..l]);
        Some((l, self.is_dir[self.top], self.depth[self.top]))
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
/// name, like a shell). Iterative backtracking — no recursion, no allocation (§26.6): on a
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

// ── match — keep the lines that match a pattern (the grep-equivalent) ────────────
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
/// and returns `(invert, pattern, path)` — `path` is "" if absent. `None` if no pattern.
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

/// `match [except] <pattern> <path>` — print the lines of `<path>` that match (or, with
/// `except`, that do not). The pipe form filters piped input instead; either way `match` is a
/// FILTER, never a pipe producer (use `read <path> | match …` to feed a pipeline from a file).
fn cmd_match(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) {
    let (invert, pattern, path) = match parse_match(args, argc, 1) {
        Some(t) => t,
        None => { ctx.console_writeln("usage: match [except] <pattern> <path>"); return; }
    };
    if path.is_empty() {
        ctx.console_writeln("match: a path is required (or pipe input: <producer> | match <pattern>)");
        return;
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("match: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        match_lines(ctx, &p[5..end], pattern.as_bytes(), invert, &mut Out::Console);
    } else {
        ctx.console_writeln_fmt(format_args!("match: not found: {}", str_of(abspath)));
    }
}

/// Run a filter built-in (`match`, `count`) over `input`, writing its output to `out`. Used
/// when the filter sits **mid-pipe** or as the last stage — it runs in-process, so it is not
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

// ── count — how many lines / words / bytes (the wc-equivalent) ───────────────────
// `count <path>` counts a file; `<producer> | count` counts piped input. Like `match` it is a
// built-in FILTER (in-process, no 4 KiB cap), but it consumes many lines and emits one summary
// line, so it usually ends a pipe. See utilities/28_count.md.

/// "" for a count of 1, "s" otherwise — readable singular/plural.
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

/// `count <path>` — count the lines / words / bytes of a file. The pipe form `<producer> |
/// count` counts piped input instead; either way `count` consumes input (never a producer).
fn cmd_count(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) {
    let path = if argc >= 2 { args[1] } else { "" };
    if path.is_empty() {
        ctx.console_writeln("count: a path is required (or pipe input: <producer> | count)");
        return;
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("count: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        write_count(ctx, &p[5..end], &mut Out::Console);
    } else {
        ctx.console_writeln_fmt(format_args!("count: not found: {}", str_of(abspath)));
    }
}

// ── sort — order the lines (ascending, or `reverse`) ─────────────────────────────
// `sort [reverse] <path>` sorts a file; `<producer> | sort [reverse]` sorts piped input. A
// built-in FILTER like match/count. See utilities/29_sort.md.

/// Most lines `sort` will order in one pass (§26.6 bounded). Beyond this it sorts the first
/// `SORT_MAX_LINES` and says so — never silently drops the rest. The index array is
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
            "sort: more than {} lines — sorted the first {} (bounded)", SORT_MAX_LINES, SORT_MAX_LINES));
    }
}

/// `sort [reverse] <path>` — print a file's lines in order. The pipe form `<producer> | sort`
/// sorts piped input instead; either way `sort` consumes input (never a producer).
fn cmd_sort(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize) {
    let (reverse, path) = parse_sort(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln("sort: a path is required (or pipe input: <producer> | sort)");
        return;
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln("sort: storage unavailable"); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let n = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + n).min(p.len());
        write_sorted(ctx, &p[5..end], reverse, &mut Out::Console);
    } else {
        ctx.console_writeln_fmt(format_args!("sort: not found: {}", str_of(abspath)));
    }
}

// ── first / last — keep the first or last N lines (the head/tail-equivalent) ──────
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

/// `first [N] <path>` / `last [N] <path>` — print a file's first/last N lines (default 10). The
/// pipe form `<producer> | first [N]` takes from piped input; either way it consumes input.
fn cmd_take(ctx: &ServiceContext, cwd: &Cwd, args: &[&str], argc: usize, last: bool) {
    let name = if last { "last" } else { "first" };
    let (n, path) = parse_take(args, argc, 1);
    if path.is_empty() {
        ctx.console_writeln_fmt(format_args!("{}: a path is required (or pipe: <producer> | {} [N])", name, name));
        return;
    }
    let mut buf = [0u8; PATH_MAX];
    let abspath = match resolve_or_err(ctx, cwd, path, &mut buf) { Some(p) => p, None => return };
    let reply = match fs_request(ctx, OP_READ_FILE, abspath, &[]) {
        Some(r) => r,
        None => { ctx.console_writeln_fmt(format_args!("{}: storage unavailable", name)); return; }
    };
    let p = reply.payload_bytes();
    if no_fs(ctx, p) { return; }
    if p.first() == Some(&FS_OK) && p.len() >= 5 {
        let cnt = u32::from_le_bytes([p[1], p[2], p[3], p[4]]) as usize;
        let end = (5 + cnt).min(p.len());
        if last { write_last(ctx, &p[5..end], n, &mut Out::Console); }
        else    { write_first(ctx, &p[5..end], n, &mut Out::Console); }
    } else {
        ctx.console_writeln_fmt(format_args!("{}: not found: {}", name, str_of(abspath)));
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
// drives — manage attached disks (utilities/15_drives.md). A shell built-in that
// sends the drives API to `fs` over IPC; `fs` holds and enforces all disk authority.
// Step 3: the data primitives `flash` / `label` / list (boot layer + multi-drive later).
// ---------------------------------------------------------------------------

fn cmd_drives(ctx: &ServiceContext, args: &[&str], argc: usize) {
    let sub = if argc >= 2 { args[1] } else { "" };
    match sub {
        ""        => drives_list(ctx),
        "flash"   => {
            // `drives flash [drive] [label]` — the drive selector is optional (one drive).
            let (sel, label) = split_drive_value(args, argc);
            if drive_sel_ok(ctx, sel) { drives_flash(ctx, label); }
        }
        "label"   => {
            // `drives label [drive] <name>` — selector optional; name required.
            let (sel, name) = split_drive_value(args, argc);
            if name.is_empty() { ctx.console_writeln("usage: drives label [drive] <name>"); }
            else if drive_sel_ok(ctx, sel) { drives_label(ctx, name); }
        }
        "reset"   => {
            // `drives reset [drive]` — un-format back to raw (optional selector, no value).
            let sel = if argc >= 3 { args[2] } else { "" };
            if drive_sel_ok(ctx, sel) { drives_reset(ctx); }
        }
        // `drives help` / `drives version` and `drives <sub> help` are handled by the
        // generic per-utility intercept in `execute` (0_conventions.md).
        other     => {
            ctx.console_writeln_fmt(format_args!("drives: unknown subcommand '{}'", other));
            util_help(ctx, "drives");
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
        ctx.console_writeln_fmt(format_args!("drives: no drive {} — only drive 0 is attached", sel));
        return false;
    }
    true // a label selector — single drive, accept
}


/// `drives` — list the attached drive (single-drive in step 3; index 0).
fn drives_list(ctx: &ServiceContext) {
    let reply = match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_DRIVES_INFO])) {
        Some(r) => r,
        None => { ctx.console_writeln("drives: storage unavailable (no fs?)"); return; }
    };
    let p = reply.payload_bytes();
    if p.first() != Some(&FS_OK) || p.len() < 28 {
        ctx.console_writeln("drives: no disk found");
        return;
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
}

/// `drives flash [label]` — format the drive as GSFS after a `[y/N]` confirm. Destructive.
fn drives_flash(ctx: &ServiceContext, label: &str) {
    if label.len() > LABEL_MAX {
        ctx.console_writeln_fmt(format_args!("drives: label too long (max {})", LABEL_MAX));
        return;
    }
    ctx.console_write("This ERASES the drive. Continue? [y/N] ");
    if !read_confirm(ctx) {
        ctx.console_writeln("drives: aborted");
        return;
    }
    let lb = label.as_bytes();
    let ll = lb.len().min(LABEL_MAX);
    let mut req = [0u8; 2 + LABEL_MAX];
    req[0] = OP_FLASH;
    req[1] = ll as u8;
    req[2..2 + ll].copy_from_slice(&lb[..ll]);
    match ctx.request_with_reply("fs", &Message::from_bytes(&req[..2 + ll])) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: formatted as GSFS — mounted, ready to use now (no reboot)");
        }
        Some(_) => ctx.console_writeln("drives: flash FAILED (no disk, or disk too small)"),
        None    => ctx.console_writeln("drives: storage unavailable (no fs?)"),
    }
}

/// `drives reset` — un-format the drive back to raw (zero the superblock). Destructive;
/// a quick clean slate for re-testing the raw→flash path. NOT a secure wipe.
fn drives_reset(ctx: &ServiceContext) {
    ctx.console_write("This un-formats the drive back to raw (ERASES). Continue? [y/N] ");
    if !read_confirm(ctx) {
        ctx.console_writeln("drives: aborted");
        return;
    }
    match ctx.request_with_reply("fs", &Message::from_bytes(&[OP_RESET])) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln("drives: reset to raw — 'drives flash' to use again");
        }
        Some(_) => ctx.console_writeln("drives: reset FAILED (no disk?)"),
        None    => ctx.console_writeln("drives: storage unavailable (no fs?)"),
    }
}

/// `drives label <name>` — name / rename the drive (rewrites the superblock).
fn drives_label(ctx: &ServiceContext, name: &str) {
    let nb = name.as_bytes();
    if nb.is_empty() || nb.len() > LABEL_MAX {
        ctx.console_writeln_fmt(format_args!("drives: label must be 1..{} chars", LABEL_MAX));
        return;
    }
    let ll = nb.len();
    let mut req = [0u8; 2 + LABEL_MAX];
    req[0] = OP_LABEL;
    req[1] = ll as u8;
    req[2..2 + ll].copy_from_slice(nb);
    match ctx.request_with_reply("fs", &Message::from_bytes(&req[..2 + ll])) {
        Some(r) if r.payload_bytes().first() == Some(&FS_OK) => {
            ctx.console_writeln_fmt(format_args!("drives: labelled '{}'", name));
        }
        Some(_) => ctx.console_writeln("drives: label FAILED (no filesystem? run 'drives flash' first)"),
        None    => ctx.console_writeln("drives: storage unavailable (no fs?)"),
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
// Helpers — no-alloc string building into stack buffers.
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
