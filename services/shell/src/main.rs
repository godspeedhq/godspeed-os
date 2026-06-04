#![no_std]
#![no_main]

use godspeed_sdk::ServiceContext;

const MAX_LINE: usize = 128;
const MAX_ARGS: usize = 4;

// Entry point called by the kernel after spawning this service.
// ctx.log() appends a newline. The kernel echoes each console keystroke to the
// display (arch::console_push_byte), so we don't echo here — just accumulate
// bytes until \r or \n. (On a serial terminal, turn local echo OFF to avoid
// doubled characters.)
#[no_mangle]
pub extern "C" fn service_main(ctx: ServiceContext) -> ! {
    // Let the one-time boot logging flush first (logger/registry/supervisor
    // "ready", init "all spawns done") so the screen rests with `gs>` as the
    // last line. Each peer service logs once then yields; a generous yield count
    // guarantees they all get scheduler turns before we print the prompt. Nothing
    // logs after this in the bare-metal image, so `gs>` stays at the bottom.
    for _ in 0..256 {
        ctx.yield_cpu();
    }
    ctx.log("shell: ready (type 'help')");
    ctx.log("gs>");

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
                ctx.print("gs> ");
            }
            0x7f | 0x08 => {
                // backspace — remove last byte
                if line_len > 0 { line_len -= 1; }
            }
            0x03 => {
                // Ctrl-C — clear line
                ctx.log("^C");
                line_len = 0;
                ctx.print("gs> ");
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

fn execute(ctx: &ServiceContext, line: &[u8]) {
    let Ok(s) = core::str::from_utf8(line) else {
        ctx.log("shell: invalid input");
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
        "cores"   => cmd_cores(ctx),
        "status"  => cmd_status(ctx),
        "observe" => {
            if argc >= 2 && args[1] == "now" {
                cmd_observe_now(ctx);
            } else {
                ctx.log("observe: live view coming soon — try 'observe now'");
            }
        }
        "spawn"   => {
            if argc < 2 { ctx.log("usage: spawn <name>"); }
            else { cmd_spawn(ctx, args[1]); }
        }
        "kill"    => {
            if argc < 2 { ctx.log("usage: kill <name>"); }
            else { cmd_kill(ctx, args[1]); }
        }
        "restart" => {
            if argc < 2 { ctx.log("usage: restart <name> [core]"); }
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
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("unknown cmd"));
        }
    }
}

fn cmd_help(ctx: &ServiceContext) {
    ctx.log("GodspeedOS shell commands:");
    ctx.log("  help                   show this message");
    ctx.log("  cores                  show core count");
    ctx.log("  status                 list all live tasks");
    ctx.log("  observe now            show a static system-metrics frame");
    ctx.log("  spawn <name>           spawn a service");
    ctx.log("  kill <name>            kill a service");
    ctx.log("  restart <name> [core]  restart a service");
    ctx.log("  reboot                 hardware reset");
}

fn cmd_reboot(ctx: &ServiceContext) -> ! {
    ctx.log("rebooting...");
    ctx.reboot()
}

fn cmd_cores(ctx: &ServiceContext) {
    let n = ctx.inspect_core_count();
    let mut buf = [0u8; 32];
    let mut pos = 0usize;
    write_bytes(&mut buf, &mut pos, b"cores: ");
    write_u32(&mut buf, &mut pos, n);
    ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("?"));
}

fn cmd_status(ctx: &ServiceContext) {
    ctx.log("SLOT  NAME               CORE STATE");
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
        ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("?"));
    }
    if !found { ctx.log("  (no live tasks)"); }
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
        ctx.log("observe: failed to spawn observe-now");
    }
}

fn cmd_spawn(ctx: &ServiceContext, name: &str) {
    match ctx.spawn(name) {
        Ok(()) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"spawned: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("spawned"));
        }
        Err(_) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"spawn failed: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("spawn failed"));
        }
    }
}

/// `producer | sink` — broker a capability-mediated pipe. Spawn the consumer
/// (registers its endpoint), then spawn the producer with a SEND cap to that
/// endpoint delegated to it. The producer holds no ambient send authority.
fn cmd_pipe(ctx: &ServiceContext, producer: &str, sink: &str) {
    if producer.is_empty() || sink.is_empty() {
        ctx.log("usage: <producer> | <sink>");
        return;
    }
    if ctx.spawn(sink).is_err() {
        ctx.log("pipe: failed to spawn sink");
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
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("pipe wired"));
        }
        Err(_) => ctx.log("pipe: failed to spawn producer with delegated cap"),
    }
}

fn cmd_kill(ctx: &ServiceContext, name: &str) {
    match ctx.kill(name) {
        Ok(()) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"killed: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("killed"));
        }
        Err(_) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"kill failed: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("kill failed"));
        }
    }
}

fn cmd_restart(ctx: &ServiceContext, name: &str, core: Option<u32>) {
    match ctx.restart(name, core) {
        Ok(()) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"restarted: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("restarted"));
        }
        Err(_) => {
            let mut buf = [0u8; 64];
            let mut pos = 0usize;
            write_bytes(&mut buf, &mut pos, b"restart failed: ");
            write_bytes(&mut buf, &mut pos, name.as_bytes());
            ctx.log(core::str::from_utf8(&buf[..pos]).unwrap_or("restart failed"));
        }
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
