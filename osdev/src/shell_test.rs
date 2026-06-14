// GodspeedOS — Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

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
    // Default form is a full timestamp `Wkd YYYY-MM-DD HH:MM:SS`; `date epoch`
    // prints epoch seconds (digits, no date/time separators).
    send(&mut write_half, b"date\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains('-') && r.contains(':'), "date: full timestamp"),
        None    => { println!("shell-test: FAIL — timed out after date"); fail += 1; }
    }

    send(&mut write_half, b"date epoch\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.chars().any(|c| c.is_ascii_digit()), "date epoch: seconds since 1970"),
        None    => { println!("shell-test: FAIL — timed out after date epoch"); fail += 1; }
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
            // Positive least-privilege case: the shell brokers spawn, so it DOES
            // hold the spawn cap.
            check!(r.contains("spawn"), "caps: shell holds spawn (broker)");
        }
        None => { println!("shell-test: FAIL — timed out after caps"); fail += 3; }
    }

    // -----------------------------------------------------------------------
    // Least privilege (H10) — a non-spawning service must NOT hold the spawn cap.
    // `logger` never spawns, so after SPAWN was gated to {init, supervisor, shell,
    // probes}, `caps logger` lists no spawn. This is the negative regression test
    // that locks the gate in: if a future change re-grants spawn universally, the
    // word "spawn" reappears here and this fails.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"caps logger\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("caps for logger"), "caps logger: header");
            check!(!r.contains("spawn"), "least-privilege: logger does NOT hold spawn");
        }
        None => { println!("shell-test: FAIL — timed out after caps logger"); fail += 2; }
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
    // Per-utility help / version (0_conventions.md): every utility self-documents,
    // with a real example per usage row and a creator credit in `version`.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"write help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("write 0.1.0") && r.contains("overwrite"), "write help: header + version");
            check!(r.contains("<path>") && r.contains("e.g.") && r.contains("buy milk"), "write help: placeholder + real example");
        }
        None => { println!("shell-test: FAIL — timed out after `write help`  [×2]"); fail += 2; }
    }
    send(&mut write_half, b"ls version\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("ls 0.1.0") && r.contains("Created by Bankole Ogundero"), "ls version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `ls version`"); fail += 1; }
    }
    send(&mut write_half, b"drives flash help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("drives flash") && r.contains("drives flash 0 data"), "subcommand help: drives flash help + example"),
        None    => { println!("shell-test: FAIL — timed out after `drives flash help`"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Up-arrow history: run a command, then up-arrow + Enter (no retyping) must recall
    // AND re-run it. `cores` is used because its OUTPUT ("cores: N") differs from the
    // recalled command text ("cores"), so a match proves it actually ran, not just echoed.
    // The arrow arrives as the ESC [ A sequence (same bytes the USB keyboard now emits).
    // -----------------------------------------------------------------------
    send(&mut write_half, b"cores\r");
    let _ = collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5));
    send(&mut write_half, b"\x1b[A\r"); // Up arrow, then Enter
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "up-arrow history: recalled + ran the previous command"),
        None    => { println!("shell-test: FAIL — timed out after up-arrow history"); fail += 1; }
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

/// Step 3b: drive the `drives` shell command end to end against a RAW AHCI disk.
/// Boots the bare-metal build (which now spawns block-driver + fs before the shell),
/// then scripts: `drives` (raw) → `drives flash data` + confirm → `drives` (GSFS) →
/// `drives label archive` → `drives` (archive). Proves the OS formats its own disk
/// over IPC from a user command, and lists/relabels it — all with no reboot.
pub fn run_drives(image_path: &Path, persist_path: &str, smp: u32) {
    println!("drives-test: booting (smp={smp}) with a RAW AHCI disk — scripted mode");

    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        // Boot image on legacy IDE; the persistence disk ALONE on an AHCI controller
        // (→ block-driver port 0), RAW (not formatted) so `drives flash` does the work.
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ])
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("drives-test: QEMU launch failed at {qemu}: {e}");
        std::process::exit(1);
    });

    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("drives-test: could not connect to serial port {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone tcp stream");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop {
                match read_half.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]),
                }
            }
        });
    }

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut cursor = 0usize;
    macro_rules! check {
        ($ok:expr, $label:expr) => {
            if $ok { println!("drives-test: PASS — {}", $label); pass += 1; }
            else   { println!("drives-test: FAIL — {}", $label); fail += 1; }
        };
    }

    // Boot complete.
    if collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("drives-test: FAIL — timed out waiting for first gs>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // 1. `drives` — a raw, unformatted disk.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("raw") && r.contains("not formatted"), "drives: raw disk listed"),
        None    => { println!("drives-test: FAIL — timed out after `drives`"); fail += 1; }
    }

    // 2. `drives flash data` — confirm the [y/N], then format.
    send(&mut write_half, b"drives flash data\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            check!(true, "flash: destructive [y/N] confirm shown");
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(20)) {
                Some(r) => check!(r.contains("formatted as GSFS"), "flash: formatted over IPC"),
                None    => { println!("drives-test: FAIL — timed out after confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL — no [y/N] confirm  [×2]"); fail += 2; }
    }

    // 3. `drives` — now a mounted GSFS labelled 'data'.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => {
            check!(r.contains("GSFS"), "drives: now formatted GSFS");
            check!(r.contains("data"), "drives: label 'data' shown");
        }
        None => { println!("drives-test: FAIL — timed out after `drives` (2)  [×2]"); fail += 2; }
    }

    // 4. `drives label archive` — rename, then confirm it stuck.
    send(&mut write_half, b"drives label archive\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("labelled 'archive'"), "label: rename acknowledged"),
        None    => { println!("drives-test: FAIL — timed out after `drives label`"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("archive"), "label: new label 'archive' listed"),
        None    => { println!("drives-test: FAIL — timed out after `drives` (3)"); fail += 1; }
    }

    // 5. `drives reset` — un-format back to raw (confirm [y/N]), then list shows raw.
    send(&mut write_half, b"drives reset\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(15)) {
                Some(r) => check!(r.contains("reset to raw"), "reset: un-formatted to raw"),
                None    => { println!("drives-test: FAIL — timed out after reset confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL — reset: no [y/N] confirm"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("raw") && r.contains("not formatted"), "reset: drive now raw"),
        None    => { println!("drives-test: FAIL — timed out after `drives` (4)"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();

    println!("\ndrives-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// Step 4: drive the file commands (ls / read / write / mkdir / cd) end to end. Boots
/// bare-metal with a RAW AHCI disk, flashes it, then exercises the commands including
/// relative paths and `..` (the shell's current-directory + path resolution).
pub fn run_files(image_path: &Path, persist_path: &str, smp: u32) {
    println!("files-test: booting (smp={smp}) with a RAW AHCI disk — scripted mode");

    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ])
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("files-test: QEMU launch failed at {qemu}: {e}");
        std::process::exit(1);
    });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("files-test: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone tcp stream");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop {
                match read_half.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]),
                }
            }
        });
    }

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut cursor = 0usize;
    macro_rules! check {
        ($ok:expr, $label:expr) => {
            if $ok { println!("files-test: PASS — {}", $label); pass += 1; }
            else   { println!("files-test: FAIL — {}", $label); fail += 1; }
        };
    }
    // Send a command and capture output up to the next prompt.
    macro_rules! run {
        ($c:expr, $secs:expr) => {{
            send(&mut write_half, $c);
            collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs($secs))
        }};
    }

    if collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("files-test: FAIL — timed out waiting for first gs>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // Format the disk first (file commands need a filesystem).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(20)) {
            Some(r) => check!(r.contains("formatted as GSFS"), "setup: flashed GSFS"),
            None    => { println!("files-test: FAIL — flash timeout"); fail += 1; }
        }
    } else { println!("files-test: FAIL — no flash confirm"); fail += 1; }

    // mkdir + write + ls + read (absolute paths).
    match run!(b"mkdir /docs\r", 10) {
        Some(r) => check!(r.contains("created /docs"), "mkdir /docs"),
        None    => { println!("files-test: FAIL — mkdir timeout"); fail += 1; }
    }
    match run!(b"write /docs/note.txt hello world\r", 10) {
        Some(r) => check!(r.contains("wrote /docs/note.txt"), "write /docs/note.txt"),
        None    => { println!("files-test: FAIL — write timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("note.txt") && r.contains("file"), "ls /docs shows note.txt"),
        None    => { println!("files-test: FAIL — ls timeout"); fail += 1; }
    }
    match run!(b"read /docs/note.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "read /docs/note.txt"),
        None    => { println!("files-test: FAIL — read timeout"); fail += 1; }
    }

    // cd + relative path + `..`.
    match run!(b"cd /docs\r", 10) {
        Some(r) => check!(r.contains("/docs"), "cd /docs"),
        None    => { println!("files-test: FAIL — cd timeout"); fail += 1; }
    }
    match run!(b"write inside.txt nested-content\r", 10) {
        Some(r) => check!(r.contains("wrote /docs/inside.txt"), "write relative → /docs/inside.txt"),
        None    => { println!("files-test: FAIL — relative write timeout"); fail += 1; }
    }
    match run!(b"ls\r", 10) {
        Some(r) => check!(r.contains("note.txt") && r.contains("inside.txt"), "ls (cwd) shows both files"),
        None    => { println!("files-test: FAIL — ls cwd timeout"); fail += 1; }
    }
    match run!(b"mkdir sub\r", 10) {
        Some(r) => check!(r.contains("created /docs/sub"), "mkdir relative → /docs/sub"),
        None    => { println!("files-test: FAIL — mkdir relative timeout"); fail += 1; }
    }
    match run!(b"cd ..\r", 10) {
        Some(r) => check!(r.contains('/') && !r.contains("/docs"), "cd .. → root"),
        None    => { println!("files-test: FAIL — cd .. timeout"); fail += 1; }
    }
    match run!(b"read /docs/inside.txt\r", 10) {
        Some(r) => check!(r.contains("nested-content"), "read absolute after cd .."),
        None    => { println!("files-test: FAIL — final read timeout"); fail += 1; }
    }

    // copy + rename.
    match run!(b"copy /docs/note.txt /docs/note-copy.txt\r", 10) {
        Some(r) => check!(r.contains("copied"), "copy /docs/note.txt → note-copy.txt"),
        None    => { println!("files-test: FAIL — copy timeout"); fail += 1; }
    }
    match run!(b"read /docs/note-copy.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "read copy has same content"),
        None    => { println!("files-test: FAIL — read copy timeout"); fail += 1; }
    }
    match run!(b"rename /docs/note-copy.txt renamed.txt\r", 10) {
        Some(r) => check!(r.contains("renamed"), "rename note-copy.txt → renamed.txt"),
        None    => { println!("files-test: FAIL — rename timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("renamed.txt") && !r.contains("note-copy.txt"), "ls shows renamed, not old name"),
        None    => { println!("files-test: FAIL — ls after rename timeout"); fail += 1; }
    }

    // delete (GSFS0003: frees blocks, reclaims) — file then re-list shows it gone.
    match run!(b"delete /docs/renamed.txt\r", 10) {
        Some(r) => check!(r.contains("deleted"), "delete /docs/renamed.txt"),
        None    => { println!("files-test: FAIL — delete timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(!r.contains("renamed.txt"), "ls: deleted file is gone"),
        None    => { println!("files-test: FAIL — ls after delete timeout"); fail += 1; }
    }

    // move (relink) — into the /docs/sub directory created earlier.
    match run!(b"move /docs/note.txt /docs/sub/note.txt\r", 10) {
        Some(r) => check!(r.contains("moved"), "move /docs/note.txt → /docs/sub/note.txt"),
        None    => { println!("files-test: FAIL — move timeout"); fail += 1; }
    }
    match run!(b"ls /docs/sub\r", 10) {
        Some(r) => check!(r.contains("note.txt"), "ls /docs/sub shows moved file"),
        None    => { println!("files-test: FAIL — ls sub timeout"); fail += 1; }
    }
    match run!(b"read /docs/sub/note.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "moved file keeps its content"),
        None    => { println!("files-test: FAIL — read moved timeout"); fail += 1; }
    }

    // Directory growth: write 10 files into one directory (a dir block holds 8 entries),
    // forcing the directory to grow a second block — proving there's no per-dir cap.
    let _ = run!(b"mkdir /big\r", 10);
    for i in 1..=10 {
        let cmd = format!("write /big/f{} x\r", i);
        let _ = run!(cmd.as_bytes(), 10);
    }
    match run!(b"ls /big\r", 10) {
        Some(r) => {
            let n = (1..=10).filter(|i| r.contains(&format!("f{}", i))).count();
            check!(n == 10, "directory grew past 8 entries (no per-dir cap) — 10 files listed");
        }
        None => { println!("files-test: FAIL — ls /big timeout"); fail += 1; }
    }

    // find — whole-filesystem tree walk from root. Tree now: /docs/{inside.txt, sub/note.txt},
    // /big/{f1..f10}.
    match run!(b"find inside.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt") && r.contains("find: 1 match"), "find: locates /docs/inside.txt"),
        None    => { println!("files-test: FAIL — find timeout"); fail += 1; }
    }
    match run!(b"find note.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/sub/note.txt") && r.contains("find: 1 match"), "find: descends into subdir (/docs/sub/note.txt)"),
        None    => { println!("files-test: FAIL — find sub timeout"); fail += 1; }
    }
    match run!(b"find f5\r", 10) {
        Some(r) => check!(r.contains("/big/f5") && r.contains("find: 1 match"), "find: locates a file in a grown directory (/big/f5)"),
        None    => { println!("files-test: FAIL — find grown timeout"); fail += 1; }
    }
    match run!(b"find nope.txt\r", 10) {
        Some(r) => check!(r.contains("find: 0 match"), "find: reports 0 matches for a missing name"),
        None    => { println!("files-test: FAIL — find missing timeout"); fail += 1; }
    }
    // find substring: `.txt` matches inside.txt AND sub/note.txt (2). Exact match would be 0.
    match run!(b"find .txt\r", 10) {
        Some(r) => check!(r.contains("find: 2 match"), "find: substring match (.txt → 2 files)"),
        None    => { println!("files-test: FAIL — find substring timeout"); fail += 1; }
    }

    // ls shows file sizes: /docs/inside.txt holds "nested-content" = 14 bytes.
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("inside.txt") && r.contains("14 B"), "ls: shows file size (inside.txt 14 B)"),
        None    => { println!("files-test: FAIL — ls size timeout"); fail += 1; }
    }

    // mkdir parents: create a 3-deep chain in one call (none of /x, /x/y exist yet).
    match run!(b"mkdir /x/y/z parents\r", 10) {
        Some(r) => check!(r.contains("created /x/y/z"), "mkdir parents: created /x/y/z chain"),
        None    => { println!("files-test: FAIL — mkdir parents timeout"); fail += 1; }
    }
    match run!(b"ls /x/y\r", 10) {
        Some(r) => check!(r.contains("z") && r.contains("dir"), "mkdir parents: /x/y/z exists as a dir"),
        None    => { println!("files-test: FAIL — ls /x/y timeout"); fail += 1; }
    }
    // plain mkdir into a missing parent still fails (parents is opt-in).
    match run!(b"mkdir /no/such/dir\r", 10) {
        Some(r) => check!(r.contains("mkdir: failed"), "mkdir (no parents): fails on missing parent"),
        None    => { println!("files-test: FAIL — mkdir strict timeout"); fail += 1; }
    }

    // ── tree: indented hierarchy ───────────────────────────────────────────────────
    // Build /t/{a.txt, sub/b.txt}, then `tree /t` shows nesting + a correct summary.
    let _ = run!(b"mkdir /t\r", 10);
    let _ = run!(b"write /t/a.txt x\r", 10);
    let _ = run!(b"mkdir /t/sub\r", 10);
    let _ = run!(b"write /t/sub/b.txt y\r", 10);
    match run!(b"tree /t\r", 12) {
        Some(r) => {
            check!(r.contains("sub/"), "tree: marks a directory with '/'");
            check!(r.contains("    b.txt"), "tree: nests a grandchild (4-space indent)");
            check!(r.contains("1 directory, 2 files"), "tree: summary counts dirs + files");
        }
        None => { println!("files-test: FAIL — tree timeout"); fail += 1; }
    }

    // ── recursive copy + delete (non-empty directories) ───────────────────────────
    // Build a small subtree: /grove/{leaf1.txt, branch/leaf2.txt}.
    let _ = run!(b"mkdir /grove\r", 10);
    let _ = run!(b"write /grove/leaf1.txt apple\r", 10);
    let _ = run!(b"mkdir /grove/branch\r", 10);
    let _ = run!(b"write /grove/branch/leaf2.txt cherry\r", 10);

    // copy recursive: whole subtree → /orchard (2 dirs: root + branch; 2 files).
    match run!(b"copy /grove /orchard recursive\r", 12) {
        Some(r) => check!(r.contains("copied") && r.contains("2 dirs") && r.contains("2 files"),
                          "copy recursive: /grove → /orchard (2 dirs, 2 files)"),
        None    => { println!("files-test: FAIL — copy recursive timeout"); fail += 1; }
    }
    match run!(b"read /orchard/branch/leaf2.txt\r", 10) {
        Some(r) => check!(r.contains("cherry"), "copy recursive: nested file copied with content"),
        None    => { println!("files-test: FAIL — read deep copy timeout"); fail += 1; }
    }
    match run!(b"read /orchard/leaf1.txt\r", 10) {
        Some(r) => check!(r.contains("apple"), "copy recursive: top-level file copied"),
        None    => { println!("files-test: FAIL — read shallow copy timeout"); fail += 1; }
    }
    // Guard: copying a directory into its own subtree is refused (would never terminate).
    match run!(b"copy /grove /grove/inner recursive\r", 10) {
        Some(r) => check!(r.contains("cannot copy into itself"), "copy recursive: refuses copy into own subtree"),
        None    => { println!("files-test: FAIL — copy-into-self timeout"); fail += 1; }
    }
    // Plain delete still refuses a non-empty directory (recursive is opt-in).
    match run!(b"delete /grove\r", 10) {
        Some(r) => check!(r.contains("delete: failed"), "delete (non-recursive): refuses non-empty dir"),
        None    => { println!("files-test: FAIL — delete non-empty timeout"); fail += 1; }
    }
    // delete recursive: removes the whole source subtree.
    match run!(b"delete /grove recursive\r", 12) {
        Some(r) => check!(r.contains("deleted (recursive)"), "delete recursive: /grove subtree removed"),
        None    => { println!("files-test: FAIL — delete recursive timeout"); fail += 1; }
    }
    match run!(b"ls /\r", 10) {
        Some(r) => check!(!r.contains("grove") && r.contains("orchard"),
                          "delete recursive: /grove gone, /orchard (the copy) survives"),
        None    => { println!("files-test: FAIL — ls after recursive delete timeout"); fail += 1; }
    }
    // The copy is independent — its nested file is intact after the source was deleted.
    match run!(b"read /orchard/branch/leaf2.txt\r", 10) {
        Some(r) => check!(r.contains("cherry"), "copy is independent of the deleted source"),
        None    => { println!("files-test: FAIL — read copy after delete timeout"); fail += 1; }
    }

    // ── write append: add to a file, and create-on-append ──────────────────────────
    let _ = run!(b"write /applog AAA\r", 10);
    let _ = run!(b"write append /applog BBB\r", 10);
    match run!(b"read /applog\r", 10) {
        Some(r) => check!(r.contains("AAABBB"), "write append: appends without overwriting"),
        None    => { println!("files-test: FAIL — read appended timeout"); fail += 1; }
    }
    // append to a missing file creates it.
    let _ = run!(b"write append /freshlog ZZZ\r", 10);
    match run!(b"read /freshlog\r", 10) {
        Some(r) => check!(r.contains("ZZZ"), "write append: creates the file when missing"),
        None    => { println!("files-test: FAIL — read created-by-append timeout"); fail += 1; }
    }
    // a path literally starting with "append" is still a path, not the keyword.
    match run!(b"write appendix.txt hi\r", 10) {
        Some(r) => check!(r.contains("wrote") && r.contains("appendix.txt"), "write: 'appendix.txt' is a path, not the append keyword"),
        None    => { println!("files-test: FAIL — appendix path timeout"); fail += 1; }
    }

    // ── cd - : toggle to the previous directory ────────────────────────────────────
    let _ = run!(b"cd /docs\r", 10);
    let _ = run!(b"cd /big\r", 10);
    match run!(b"cd -\r", 10) {
        Some(r) => check!(r.contains("/docs"), "cd -: returns to the previous directory"),
        None    => { println!("files-test: FAIL — cd - timeout"); fail += 1; }
    }
    match run!(b"cd -\r", 10) {
        Some(r) => check!(r.contains("/big"), "cd -: toggles back to where we just were"),
        None    => { println!("files-test: FAIL — cd - toggle timeout"); fail += 1; }
    }
    let _ = run!(b"cd /\r", 10);

    child.kill().ok();
    child.wait().ok();
    println!("\nfiles-test: {pass} passed, {fail} failed");
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
