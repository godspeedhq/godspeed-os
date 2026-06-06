//! Scripted smoke-test for `osdev test shell`.
//!
//! Boots the OS (bare-metal mode: pong + ping + observe + shell, no probes)
//! and communicates with the shell service over a TCP serial port.
//!
//! QEMU is launched with `-serial tcp::PORT,server` (no `nowait`): QEMU
//! waits until this test connects before starting execution, guaranteeing
//! every byte of serial output is captured.  COM2 is unused in this test.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub fn run(image_path: &Path, smp: u32) {
    println!("shell-test: booting OS (smp={smp}) — scripted mode");

    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");

    // COM1 → TCP server on a FRESH free port per run. A fixed port (was 5556)
    // collided with a stale QEMU left over from a previous/concurrent run, which
    // showed up as "could not connect". An ephemeral port can't collide — a leftover
    // QEMU is always on a different port.
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        // COM1 → TCP server; QEMU waits for a client before booting so we
        // never miss output that was written before we connected.
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",   // COM2 unused
        "-display", "none",
        "-no-reboot",
        "-no-shutdown",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("shell-test: QEMU launch failed at {qemu}: {e}");
        std::process::exit(1);
    });

    // Connect to QEMU's serial server; this triggers QEMU to begin booting.
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None    => {
            eprintln!("shell-test: could not connect to QEMU serial port {shell_port}");
            child.kill().ok();
            std::process::exit(1);
        }
    };

    let mut read_half  = stream.try_clone().expect("clone tcp stream for reading");
    let mut write_half = stream;

    // Background thread streams all bytes into a shared buffer.
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop {
                match read_half.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n)          => buf2.lock().unwrap().extend_from_slice(&tmp[..n]),
                }
            }
        });
    }

    let mut pass   = 0usize;
    let mut fail   = 0usize;
    let mut cursor = 0usize;

    macro_rules! check {
        ($ok:expr, $label:expr) => {
            if $ok {
                println!("shell-test: PASS — {}", $label);
                pass += 1;
            } else {
                println!("shell-test: FAIL — {}", $label);
                fail += 1;
            }
        };
    }

    // -----------------------------------------------------------------------
    // Step 1: wait for first gs> — boot complete, shell ready.
    // -----------------------------------------------------------------------
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(30)) {
        Some(boot_out) => {
            check!(boot_out.contains("shell: ready"), "boot: shell ready message");
        }
        None => {
            // Print what we did receive to help diagnose failures.
            let received = {
                let g = buf.lock().unwrap();
                String::from_utf8_lossy(&g).into_owned()
            };
            println!("shell-test: FAIL — timed out waiting for first gs>");
            println!("shell-test: received so far:\n{received}");
            child.kill().ok();
            child.wait().ok();
            std::process::exit(1);
        }
    }

    // -----------------------------------------------------------------------
    // help
    // -----------------------------------------------------------------------
    send(&mut write_half, b"help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("GodspeedOS shell commands"), "help: header");
            check!(r.contains("spawn"),   "help: spawn listed");
            check!(r.contains("restart"), "help: restart listed");
            check!(r.contains("status"),  "help: status listed");
        }
        None => {
            println!("shell-test: FAIL — timed out after `help`  [×4]");
            fail += 4;
        }
    }

    // -----------------------------------------------------------------------
    // cores
    // -----------------------------------------------------------------------
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains(&format!("cores: {smp}")), "cores: reports smp count");
        }
        None => {
            println!("shell-test: FAIL — timed out after `cores`");
            fail += 1;
        }
    }

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------
    send(&mut write_half, b"status\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("SLOT"), "status: header present");
            check!(
                r.contains("pong") || r.contains("ping") || r.contains("shell"),
                "status: tasks visible"
            );
        }
        None => {
            println!("shell-test: FAIL — timed out after `status`  [×2]");
            fail += 2;
        }
    }

    // -----------------------------------------------------------------------
    // starter-pack: echo / about / mem / caps (self)
    // -----------------------------------------------------------------------
    send(&mut write_half, b"echo PINGPONG42\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("PINGPONG42"), "echo: prints its argument"),
        None    => { println!("shell-test: FAIL — timed out after echo"); fail += 1; }
    }

    send(&mut write_half, b"about\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("GodspeedOS"), "about: identity line");
            check!(r.contains("Bankole Ogundero"), "about: creator credit");
        }
        None => { println!("shell-test: FAIL — timed out after about  [×2]"); fail += 2; }
    }

    send(&mut write_half, b"mem\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("mem:") && r.contains("total"), "mem: reports usage"),
        None    => { println!("shell-test: FAIL — timed out after mem"); fail += 1; }
    }

    // date — the RTC clock (QEMU emulates the MC146818 and returns host time).
    // Default form is a full timestamp `Wkd YYYY-MM-DD HH:MM:SS`; `date unix`
    // prints epoch seconds (digits, no date/time separators).
    send(&mut write_half, b"date\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains('-') && r.contains(':'), "date: full timestamp"),
        None    => { println!("shell-test: FAIL — timed out after date"); fail += 1; }
    }

    send(&mut write_half, b"date unix\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.chars().any(|c| c.is_ascii_digit()), "date unix: epoch seconds"),
        None    => { println!("shell-test: FAIL — timed out after date unix"); fail += 1; }
    }

    send(&mut write_half, b"caps\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("caps for shell"), "caps (no arg): shows this shell"),
        None    => { println!("shell-test: FAIL — timed out after caps (self)"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // unknown command
    // -----------------------------------------------------------------------
    send(&mut write_half, b"xyzzy\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("unknown: xyzzy"), "unknown command error"),
        None    => { println!("shell-test: FAIL — timed out after unknown command"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // observe now — the shell brokers a one-shot observe-now service that prints
    // a static metrics frame. Its output is ASYNCHRONOUS (the prompt returns
    // before observe-now is scheduled), so wait on observe's own summary line
    // rather than on gs>. This also exercises the gated introspection path
    // (observe-now holds the INTROSPECT cap; task_stat/inspect_* succeed).
    // -----------------------------------------------------------------------
    send(&mut write_half, b"observe now\r");
    match collect_until(&buf, &mut cursor, b"system state", Duration::from_secs(15)) {
        Some(r) => check!(r.contains("observe:"), "observe now: static frame printed"),
        None    => { println!("shell-test: FAIL — timed out waiting for observe now frame"); fail += 1; }
    }
    // The frame should carry the task table header (gated task_stat working).
    // Wait on RESTARTS (end of the header) so the chunk includes TASK + NAME.
    match collect_until(&buf, &mut cursor, b"RESTARTS", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("TASK") && r.contains("NAME"), "observe now: task table header"),
        None    => { println!("shell-test: FAIL — observe now: no task table"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // caps <service> — list a service's held capabilities (introspection path).
    // The shell holds the INTROSPECT cap, so it can read its own caps; introspect
    // itself must appear in the list.
    // -----------------------------------------------------------------------
    // The observe-now step stopped reading at the table header, so its trailing
    // `gs>` prompt is still in the stream — absorb it before issuing caps.
    let _ = collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5));
    send(&mut write_half, b"caps shell\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("caps for shell"), "caps: header");
            check!(r.contains("introspect"), "caps: lists introspect cap");
        }
        None => { println!("shell-test: FAIL — timed out after caps"); fail += 2; }
    }

    // -----------------------------------------------------------------------
    // Singleton guard — spawning an already-live service (here the trusted-root
    // supervisor) must be refused, so the shell can't create a duplicate TCB.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"spawn supervisor\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("Core services") && r.contains("protected"),
                          "spawn: trusted-root refused with reason"),
        None    => { println!("shell-test: FAIL — timed out after spawn supervisor"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Done.
    // -----------------------------------------------------------------------
    child.kill().ok();
    child.wait().ok();

    println!("\nshell-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Ask the OS for a free TCP port: bind `127.0.0.1:0` (kernel assigns an unused
/// ephemeral port), read it back, then drop the listener so QEMU can claim it as
/// its serial server. The drop→QEMU-bind window is negligible for a local harness,
/// and a fresh port per run removes the stale-QEMU collision class entirely.
/// Falls back to 5556 only if binding somehow fails.
fn pick_free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(5556)
}

/// Retry connecting to `127.0.0.1:port` every 100 ms until `timeout` expires.
/// QEMU needs ~50–200 ms to open the port after launch.
fn retry_tcp_connect(port: u16, timeout: Duration) -> Option<TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => {
                // Keep the stream in blocking mode; reads/writes block naturally.
                let _ = s.set_read_timeout(None);
                return Some(s);
            }
            Err(_) => {
                if Instant::now() >= deadline { return None; }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Block (polling every 50 ms) until `sentinel` appears in `buf[*cursor..]`
/// or `timeout` expires.  Advances `*cursor` past the sentinel on success.
fn collect_until(
    buf:      &Arc<Mutex<Vec<u8>>>,
    cursor:   &mut usize,
    sentinel: &[u8],
    timeout:  Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        {
            let g = buf.lock().unwrap();
            let slice = &g[*cursor..];
            if let Some(pos) = window_find(slice, sentinel) {
                let end   = *cursor + pos + sentinel.len();
                let chunk = String::from_utf8_lossy(&g[*cursor..end]).into_owned();
                *cursor   = end;
                return Some(chunk);
            }
        }
        if Instant::now() >= deadline { return None; }
        thread::sleep(Duration::from_millis(50));
    }
}

fn window_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() { return Some(0); }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn send(stream: &mut impl Write, data: &[u8]) {
    let _ = stream.write_all(data);
    let _ = stream.flush();
}
