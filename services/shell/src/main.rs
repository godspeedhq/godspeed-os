#![no_std]
#![no_main]

use godspeed_sdk::{ServiceContext, CapInfo};

const MAX_LINE: usize = 128;
const MAX_ARGS: usize = 4;

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
    ctx.console_write("gs> ");

    let mut line_buf = [0u8; MAX_LINE];
    let mut line_len = 0usize;

    loop {
        let b = ctx.console_read();

        match b {
            b'\r' | b'\n' => {
                if line_len > 0 {
                    execute(&ctx, &line_buf[..line_len]);
                    line_len = 0;
                }
                ctx.console_write("gs> ");
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
                ctx.console_write("gs> ");
            }
            b if b >= 0x20 && b < 0x7f => {
                if line_len < MAX_LINE {
                    line_buf[line_len] = b;
                    line_len += 1;
                }
            }
            _ => {}
        }
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

fn execute(ctx: &ServiceContext, line: &[u8]) {
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
    ctx.console_writeln("Power");
    help_line(ctx, "reboot", "hardware reset");
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
        ctx.console_writeln_fmt(format_args!("{}", dt.unix_secs()));
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
    // restoring it (echo back on, cursor visible) so the shell stays usable.
    ctx.console_echo(true);
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
const CORE_SERVICES: [&str; 3] = ["init", "supervisor", "registry"];

/// Shown when spawn/kill/restart targets a core service — "Not applicable" makes
/// it clear the command is refused *because* the target is protected, not failed.
const PROTECTED_MSG: &str =
    "Not applicable. Core services (init, supervisor, registry) are protected";

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
