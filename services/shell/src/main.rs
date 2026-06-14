// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapInfo, Message};

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
    if let Some(bar) = s.find('|') {
        cmd_pipe(ctx, s[..bar].trim(), s[bar + 1..].trim());
        return;
    }

    let mut args = [""; MAX_ARGS];
    let mut argc = 0usize;
    for word in s.split_ascii_whitespace() {
        if argc >= MAX_ARGS { break; }
        args[argc] = word;
        argc += 1;
    }
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
        "echo"    => cmd_echo(ctx, s["echo".len()..].trim()),
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
        "ls"      => cmd_ls(ctx, cwd, if argc >= 2 { args[1] } else { "" }),
        "read"    => {
            if argc < 2 { ctx.console_writeln("usage: read <path>"); }
            else { cmd_read(ctx, cwd, args[1]); }
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
            else { cmd_find(ctx, cwd, args[1], if argc >= 3 { args[2] } else { "/" }); }
        }
        "tree"    => cmd_tree(ctx, cwd, if argc >= 2 { args[1] } else { "" }),
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
    "echo", "clear", "about", "mem", "cores", "date", "status", "observe", "caps",
    "spawn", "kill", "restart", "reboot", "drives", "ls", "cd", "read", "write",
    "mkdir", "copy", "move", "rename", "delete", "find", "tree",
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
        "caps" => help_block(ctx, "caps", "show a service's capabilities", &[
            ("caps", "this shell's own capabilities", "caps"),
            ("caps <service>", "capabilities held by <service>", "caps logger"),
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
        "drives" => help_block(ctx, "drives", "manage attached disks", &[
            ("drives", "list attached drive(s)", "drives"),
            ("drives flash [drive] [label]", "format a drive as GSFS (ERASES)", "drives flash 0 data"),
            ("drives label [drive] <name>", "name / rename a drive", "drives label 0 archive"),
            ("drives reset [drive]", "un-format a drive back to raw", "drives reset 0"),
        ], true),
        "ls" => help_block(ctx, "ls", "list a directory", &[
            ("ls", "list the current directory", "ls"),
            ("ls <path>", "list the directory at <path>", "ls /docs"),
        ], true),
        "cd" => help_block(ctx, "cd", "change current directory", &[
            ("cd <path>", "move to <path> (no arg → root)", "cd /docs"),
            ("cd -", "move to the previous directory", "cd -"),
        ], true),
        "read" => help_block(ctx, "read", "print a file", &[
            ("read <path>", "print the contents of <path>", "read /docs/notes.txt"),
        ], true),
        "write" => help_block(ctx, "write", "create or overwrite a file", &[
            ("write <path>", "create an empty file", "write /docs/todo.txt"),
            ("write <path> <text>", "create/overwrite with text", "write /docs/todo.txt \"buy milk\""),
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
        "find" => help_block(ctx, "find", "search the tree for a name (substring match)", &[
            ("find <name>", "search everywhere; matches names containing <name>", "find report"),
            ("find <name> <path>", "search only under <path>", "find .txt /docs"),
        ], true),
        "tree" => help_block(ctx, "tree", "print the directory hierarchy", &[
            ("tree", "tree of the current directory", "tree"),
            ("tree <path>", "tree rooted at <path>", "tree /docs"),
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
    ctx.console_writeln("GodspeedOS shell commands");
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
    help_line(ctx, "write <path> [text]", "create/overwrite a file");
    help_line(ctx, "mkdir <path> [parents]", "create a directory");
    help_line(ctx, "copy <src> <dst> [recursive]", "copy a file or subtree");
    help_line(ctx, "move <src> <dst>", "relocate a file/dir");
    help_line(ctx, "rename <path> <name>", "rename an entry in place");
    help_line(ctx, "delete <path> [recursive]", "remove a file/dir/subtree");
    help_line(ctx, "find <name> [path]", "search the tree for a name");
    help_line(ctx, "tree [path]", "print the directory hierarchy");
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
fn cmd_echo(ctx: &ServiceContext, text: &str) {
    ctx.console_writeln(text);
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

fn cmd_status(ctx: &ServiceContext) {
    ctx.console_writeln("SLOT  NAME               CORE STATE");
    let mut found = false;
    for slot in 0u32..256 {
        let stat = ctx.task_stat(slot);
        if !stat.valid { continue; }
        found = true;
        let mut buf = [b' '; 80];
        let mut pos = 0usize;
        // slot (4)
        write_u32_padded(&mut buf, &mut pos, slot, 4);
        buf[pos] = b' '; buf[pos+1] = b' '; pos += 2;
        // name (18)
        let name_bytes = &stat.name[..stat.name_len.min(18)];
        write_bytes(&mut buf, &mut pos, name_bytes);
        while pos < 22 { buf[pos] = b' '; pos += 1; }
        // core (4)
        write_u32_padded(&mut buf, &mut pos, stat.core as u32, 4);
        buf[pos] = b' '; pos += 1;
        // state
        let st = stat.state_str().as_bytes();
        write_bytes(&mut buf, &mut pos, st);
        ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or("?"));
    }
    if !found { ctx.console_writeln("  (no live tasks)"); }
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

/// `producer | sink` — broker a capability-mediated pipe. Spawn the consumer
/// (registers its endpoint), then spawn the producer with a SEND cap to that
/// endpoint delegated to it. The producer holds no ambient send authority.
fn cmd_pipe(ctx: &ServiceContext, producer: &str, sink: &str) {
    if producer.is_empty() || sink.is_empty() {
        ctx.console_writeln("usage: <producer> | <sink>");
        return;
    }
    if ctx.spawn(sink).is_err() {
        ctx.console_writeln("pipe: failed to spawn sink");
        return;
    }
    match ctx.spawn_pipe(producer, sink) {
        Ok(()) => {
            let mut buf = [0u8; 96];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"pipe wired: ");
            write_bytes(&mut buf, &mut pos, producer.as_bytes());
            write_bytes(&mut buf, &mut pos, b" | ");
            write_bytes(&mut buf, &mut pos, sink.as_bytes());
            ctx.console_writeln(core::str::from_utf8(&buf[..pos]).unwrap_or("pipe wired"));
        }
        Err(_) => ctx.console_writeln("pipe: failed to spawn producer with delegated cap"),
    }
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
fn cmd_ls(ctx: &ServiceContext, cwd: &Cwd, arg: &str) {
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
    ctx.console_writeln_fmt(format_args!("{}  ({} entries)", str_of(path), count));
    if count > 0 { ctx.console_writeln("  NAME                  TYPE   SIZE"); }
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
            ctx.console_writeln_fmt(format_args!("  {:<20}  dir    -", name));
        } else {
            ctx.console_writeln_fmt(format_args!("  {:<20}  file   {} B", name, size));
        }
    }
    if count == 0 { ctx.console_writeln("  (empty)"); }
}

/// `read <path>` — print a file's contents.
fn cmd_read(ctx: &ServiceContext, cwd: &Cwd, arg: &str) {
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
        ctx.console_write(core::str::from_utf8(&p[5..end]).unwrap_or(""));
        if end == 0 || p[end - 1] != b'\n' { ctx.console_writeln(""); }
    } else {
        ctx.console_writeln_fmt(format_args!("read: not found: {}", str_of(path)));
    }
}

/// `write <path> [content]` — create/overwrite a file with the rest of the line.
fn cmd_write(ctx: &ServiceContext, cwd: &Cwd, rest: &str) {
    if rest.is_empty() {
        ctx.console_writeln("usage: write <path> [content]");
        return;
    }
    // Split off the first token (path); the remainder (with spaces) is the content.
    let (pstr, content) = match rest.split_once(char::is_whitespace) {
        Some((p, c)) => (p, c.trim_start()),
        None => (rest, ""),
    };
    let mut buf = [0u8; PATH_MAX];
    let path = match resolve_or_err(ctx, cwd, pstr, &mut buf) { Some(p) => p, None => return };
    // Copy the path out before reusing buffers (path borrows `buf`).
    let mut pbuf = [0u8; PATH_MAX];
    let pl = path.len();
    pbuf[..pl].copy_from_slice(path);
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

/// `find <name> [path]` — search a subtree (default the whole filesystem, `/`) for entries
/// named exactly `<name>`, printing each match's full path. This is whole-filesystem
/// enumeration done the disciplined way: a **tree walk** (the tree IS the index, §6.4),
/// client-side via LIST_DIR so results stream as found and `fs` needs no new op. The walk
/// is bounded (a fixed pending-directory stack) and **loud on truncation** (§26.6/§3.12);
/// the `fs_index` accelerator (persistence.md §6.5) is what we'd build if this walk ever
/// gets too slow on a huge tree — not before.
fn cmd_find(ctx: &ServiceContext, cwd: &Cwd, target: &str, start: &str) {
    let mut sbuf = [0u8; PATH_MAX];
    let start_abs = match resolve_or_err(ctx, cwd, start, &mut sbuf) { Some(p) => p, None => return };
    let mut stack = PathStack::new();
    stack.push(start_abs);

    let target = target.as_bytes();
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
                if contains(name, target) {
                    ctx.console_writeln(str_of(&child[..clen]));
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
fn cmd_tree(ctx: &ServiceContext, cwd: &Cwd, arg: &str) {
    let mut buf = [0u8; PATH_MAX];
    let start = match resolve_or_err(ctx, cwd, arg, &mut buf) { Some(p) => p, None => return };
    match stat_kind(ctx, start) {
        Some(true)  => {}
        Some(false) => { ctx.console_writeln(str_of(start)); ctx.console_writeln("0 directories, 1 file"); return; }
        None        => { ctx.console_writeln_fmt(format_args!("tree: not found: {}", str_of(start))); return; }
    }
    let mut stack = TreeStack::new();
    stack.push(start, true, 0);
    let (mut dirs, mut files) = (0u32, 0u32);
    while let Some((plen, is_dir, depth)) = stack.pop(&mut buf) {
        // Print this node: indent by depth; root shows its full path, deeper nodes their
        // basename; a trailing '/' marks a directory.
        for _ in 0..depth { ctx.console_write("  "); }
        let name = if depth == 0 { &buf[..plen] } else { basename(&buf[..plen]) };
        if is_dir { ctx.console_writeln_fmt(format_args!("{}/", str_of(name))); }
        else      { ctx.console_writeln(str_of(name)); }
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
    ctx.console_writeln_fmt(format_args!(
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

/// True if `needle` appears as a contiguous substring of `haystack` (find's match).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() { return true; }
    if needle.len() > haystack.len() { return false; }
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn str_of(b: &[u8]) -> &str {
    core::str::from_utf8(b).unwrap_or("?")
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

fn write_u32_padded(buf: &mut [u8], pos: &mut usize, n: u32, width: usize) {
    let mut tmp = [0u8; 10];
    let s = u32_to_str(n, &mut tmp);
    let pad = width.saturating_sub(s.len());
    for _ in 0..pad { if *pos < buf.len() { buf[*pos] = b' '; *pos += 1; } }
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
