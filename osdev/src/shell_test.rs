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
            // status now renders a typed table (lowercase column names from the record model).
            check!(r.contains("name") && r.contains("state"), "status: table header present");
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
    // Structured records: status as a typed table → where filter + to json rendering.
    send(&mut write_half, b"status | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("\"state\":"), "status | to json: JSON objects"),
        None    => { println!("shell-test: FAIL — status|to json timeout"); fail += 1; }
    }
    // Compact predicate: where col<op>val (no spaces, no quotes needed).
    send(&mut write_half, b"status | where name=shell\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("shell") && !r.contains("logger"), "status | where name=shell: filters rows"),
        None    => { println!("shell-test: FAIL — status|where timeout"); fail += 1; }
    }
    send(&mut write_half, b"status | where name=shell | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\": \"shell\"") && !r.contains("\"logger\""), "status | where … | to json: filtered JSON"),
        None    => { println!("shell-test: FAIL — status|where|json timeout"); fail += 1; }
    }
    // select: project columns.
    send(&mut write_half, b"status | where name=shell | select name state\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("name") && r.contains("state") && !r.contains("restarts"), "status | select: projects columns"),
        None    => { println!("shell-test: FAIL — status|select timeout"); fail += 1; }
    }
    // to yaml: the other edge rendering.
    send(&mut write_half, b"status | where name=shell | to yaml\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("- slot:") && r.contains("name: shell"), "status | to yaml: YAML mapping list"),
        None    => { println!("shell-test: FAIL — status|to yaml timeout"); fail += 1; }
    }
    // sort by a column (just exercise the path; ordering of the full table is host-dependent).
    send(&mut write_half, b"status | sort name | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("shell"), "status | sort name: sorts the table"),
        None    => { println!("shell-test: FAIL — status|sort timeout"); fail += 1; }
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

    // observe now as a record producer (docs/records.md): the only PIPEABLE form (bare
    // `observe` is the live loop). Carries the `ticks` (cumulative cpu-time) column status omits.
    send(&mut write_half, b"observe now | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":"),
                          "observe record: now | to json carries the ticks column"),
        None    => { println!("shell-test: FAIL — observe now|to json timeout"); fail += 1; }
    }
    send(&mut write_half, b"observe now | select name ticks | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":") && !r.contains("\"core\":"),
                          "observe record: select name ticks projects the metric columns"),
        None    => { println!("shell-test: FAIL — observe now|select timeout"); fail += 1; }
    }
    // The live loop must REFUSE to be piped (it owns the screen and never yields a stream),
    // loudly — not hang the shell waiting on a recv that never comes.
    send(&mut write_half, b"observe | sort reverse ticks\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("live view can't be piped"),
                          "observe record: bare live observe refuses to be piped (loud)"),
        None    => { println!("shell-test: FAIL — observe pipe-refusal timeout"); fail += 1; }
    }
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

    // caps as a record producer (docs/records.md): piped, it emits resource/rights rows.
    send(&mut write_half, b"caps | to json\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"resource\":") && r.contains("\"rights\":"),
                          "caps record: to json renders resource/rights"),
        None    => { println!("shell-test: FAIL — caps|to json timeout"); fail += 1; }
    }
    // where on the resource column: the shell holds the spawn cap, so this keeps a row.
    send(&mut write_half, b"caps | where resource=spawn\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("spawn") && r.contains("resource"),
                          "caps record: where resource=spawn keeps the spawn cap"),
        None    => { println!("shell-test: FAIL — caps|where timeout"); fail += 1; }
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
    // Record-pipe verbs self-document too (utilities/31_records.md): they are pipe-only
    // stages, but `<verb> help` / `<verb> version` still resolve via the UTILS intercept.
    send(&mut write_half, b"where help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("where 0.1.0") && r.contains("status | where mem>0"), "where help: header + real example"),
        None    => { println!("shell-test: FAIL — timed out after `where help`"); fail += 1; }
    }
    send(&mut write_half, b"to help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("to 0.1.0") && r.contains("to json") && r.contains("to yaml"), "to help: header + json/yaml rows"),
        None    => { println!("shell-test: FAIL — timed out after `to help`"); fail += 1; }
    }
    send(&mut write_half, b"from version\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("from 0.1.0") && r.contains("Created by Bankole Ogundero"), "from version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `from version`"); fail += 1; }
    }
    send(&mut write_half, b"select help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("select 0.1.0") && r.contains("status | select name core state"), "select help: header + real example"),
        None    => { println!("shell-test: FAIL — timed out after `select help`"); fail += 1; }
    }
    // The top-level `help` command itself conforms now (0_conventions.md §3, last open item):
    // its categorised list carries the version header (rule 6), and `help help` / `help version`
    // resolve like any other utility.
    send(&mut write_half, b"help version\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("help 0.1.0") && r.contains("Created by Bankole Ogundero"), "help version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `help version`"); fail += 1; }
    }
    send(&mut write_half, b"help help\r");
    match collect_until(&buf, &mut cursor, b"gs>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("help 0.1.0") && r.contains("<command> help"), "help help: header + per-command hint"),
        None    => { println!("shell-test: FAIL — timed out after `help help`"); fail += 1; }
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
    // find glob: `*.txt` is anchored — matches names ENDING in .txt (inside.txt, note.txt = 2),
    // unlike the substring form which would also match a "txt" anywhere.
    match run!(b"find *.txt\r", 10) {
        Some(r) => check!(r.contains("find: 2 match"), "find: glob '*.txt' (anchored, 2 files)"),
        None    => { println!("files-test: FAIL — find glob *.txt timeout"); fail += 1; }
    }
    // find glob `?`: f? matches the 2-char names f1..f9 (9) but NOT f10 (3 chars) — proves
    // single-char `?` and whole-name anchoring. Substring 'f?' would match nothing literally.
    match run!(b"find f?\r", 10) {
        Some(r) => check!(r.contains("find: 9 match"), "find: glob 'f?' (9 of f1..f10, not f10)"),
        None    => { println!("files-test: FAIL — find glob f? timeout"); fail += 1; }
    }
    // glob is anchored both ends: `inside.*` matches inside.txt (1), not a mere substring.
    match run!(b"find inside.*\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt") && r.contains("find: 1 match"), "find: glob 'inside.*' (1 file)"),
        None    => { println!("files-test: FAIL — find glob inside.* timeout"); fail += 1; }
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

    // ── pipes: built-ins and services compose, both directions (Appendix D) ─────────
    // builtin producer → write-file sink: capture echo's output to a file.
    let _ = run!(b"echo piped-to-file | write /pipe1.txt\r", 10);
    match run!(b"read /pipe1.txt\r", 10) {
        Some(r) => check!(r.contains("piped-to-file"), "pipe: builtin | write file (echo → file)"),
        None    => { println!("files-test: FAIL — pipe echo|write timeout"); fail += 1; }
    }
    // builtin producer (find, glob) → write-file sink: capture a listing to a file.
    match run!(b"find *.txt /docs | write /found.txt\r", 12) {
        Some(r) => check!(r.contains("piped") && r.contains("found.txt"), "pipe: find | write file (wired)"),
        None    => { println!("files-test: FAIL — pipe find|write timeout"); fail += 1; }
    }
    match run!(b"read /found.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt"), "pipe: find | write captured the matches"),
        None    => { println!("files-test: FAIL — read found.txt timeout"); fail += 1; }
    }
    // builtin producer → service filter (terminal): echo's text through `upper`, whose
    // uppercased output the shell prints to the console (the pipe's final result).
    match run!(b"echo hello pipes | upper\r", 14) {
        Some(r) => check!(r.contains("HELLO PIPES"), "pipe: builtin | service (echo → upper → HELLO PIPES)"),
        None    => { println!("files-test: FAIL — pipe echo|upper timeout"); fail += 1; }
    }
    // service producer → write-file sink: capture `greet`'s output to a file. The shell is the
    // sink: it drains greet's stream (EOT marker = end) and writes it.
    match run!(b"greet | write /greetout.txt\r", 14) {
        Some(r) => check!(r.contains("piped") && r.contains("greetout.txt"), "pipe: service | write file (greet → file)"),
        None    => { println!("files-test: FAIL — pipe greet|write timeout"); fail += 1; }
    }
    match run!(b"read /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("hello from godspeed"), "pipe: service | write captured greet's output"),
        None    => { println!("files-test: FAIL — read greetout timeout"); fail += 1; }
    }
    // ── multi-stage (3 stages): producer | filter | sink ───────────────────────────
    // builtin producer → service filter → write sink: echo → upper → file.
    match run!(b"echo lower text | upper | write /up.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("up.txt"), "pipe: 3-stage echo | upper | write (wired)"),
        None    => { println!("files-test: FAIL — 3-stage echo|upper|write timeout"); fail += 1; }
    }
    match run!(b"read /up.txt\r", 10) {
        Some(r) => check!(r.contains("LOWER TEXT"), "pipe: 3-stage filtered through upper to file"),
        None    => { println!("files-test: FAIL — read up.txt timeout"); fail += 1; }
    }
    // service producer → service filter → write sink: greet → upper → file.
    match run!(b"greet | upper | write /gu.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("gu.txt"), "pipe: 3-stage greet | upper | write (wired)"),
        None    => { println!("files-test: FAIL — 3-stage greet|upper|write timeout"); fail += 1; }
    }
    match run!(b"read /gu.txt\r", 10) {
        Some(r) => check!(r.contains("HELLO FROM GODSPEED") && r.contains("NO AMBIENT AUTHORITY HERE"),
                          "pipe: 3-stage greet → upper → file (all lines uppercased)"),
        None    => { println!("files-test: FAIL — read gu.txt timeout"); fail += 1; }
    }

    // ── match: the grep-equivalent line filter (direct, pipe, except, glob, quoting) ────
    // /greetout.txt holds greet's 3 lines. Direct form: keep lines matching a substring.
    match run!(b"match capability /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("no ambient authority"),
                          "match: direct, keeps only the matching line"),
        None    => { println!("files-test: FAIL — match direct timeout"); fail += 1; }
    }
    // `except`: keep the lines that do NOT match.
    match run!(b"match except capability /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("no ambient authority")
                          && !r.contains("capability pipes work"),
                          "match except: keeps the non-matching lines"),
        None    => { println!("files-test: FAIL — match except timeout"); fail += 1; }
    }
    // Pipe filter (last stage): a service producer's lines through match, printed to console.
    match run!(b"greet | match ambient\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "match: as a pipe filter (greet | match ambient)"),
        None    => { println!("files-test: FAIL — greet|match timeout"); fail += 1; }
    }
    // Anchored glob (whole-line): `*here` keeps lines ending in "here".
    match run!(b"greet | match *here\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("capability pipes work"),
                          "match: glob (anchored *here)"),
        None    => { println!("files-test: FAIL — match glob timeout"); fail += 1; }
    }
    // 3-stage with match in the MIDDLE: greet | match except hello | write.
    match run!(b"greet | match except hello | write /mx.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("mx.txt"), "match: mid-pipe filter (3-stage wired)"),
        None    => { println!("files-test: FAIL — 3-stage match timeout"); fail += 1; }
    }
    match run!(b"read /mx.txt\r", 10) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("hello from godspeed"),
                          "match: mid-pipe dropped the 'hello' line"),
        None    => { println!("files-test: FAIL — read mx.txt timeout"); fail += 1; }
    }
    // Minimal quoting: a quoted multi-word pattern is one argument. Without quoting, "two" and
    // "words" would split and nothing would match the input "two words".
    match run!(b"echo two words | match \"two words\"\r", 14) {
        Some(r) => check!(r.contains("two words"), "match: quoted multi-word pattern (\"two words\")"),
        None    => { println!("files-test: FAIL — match quoting timeout"); fail += 1; }
    }

    // ── count: the wc-equivalent (lines / words / bytes) ────────────────────────────
    // Pipe sink: count a producer's lines. greet emits 3 lines.
    match run!(b"greet | count\r", 14) {
        Some(r) => check!(r.contains("3 lines"), "count: pipe sink (greet | count → 3 lines)"),
        None    => { println!("files-test: FAIL — greet|count timeout"); fail += 1; }
    }
    // Direct: count a file (greet's 3 lines were written to /greetout.txt earlier).
    match run!(b"count /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("3 lines"), "count: direct file count (/greetout.txt → 3 lines)"),
        None    => { println!("files-test: FAIL — count direct timeout"); fail += 1; }
    }
    // Singular forms: echo emits one line, one word.
    match run!(b"echo hello | count\r", 14) {
        Some(r) => check!(r.contains("1 line,") && r.contains("1 word,"), "count: singular (1 line, 1 word)"),
        None    => { println!("files-test: FAIL — echo|count timeout"); fail += 1; }
    }
    // Composition: filter then count (producer | match | count) — drop the 'hello' line, count 2.
    match run!(b"greet | match except hello | count\r", 16) {
        Some(r) => check!(r.contains("2 lines"), "count: composes after a filter (greet | match except hello | count)"),
        None    => { println!("files-test: FAIL — greet|match|count timeout"); fail += 1; }
    }

    // ── sort: order the lines (greet emits hello / capability / no-ambient, out of order) ──
    // Ascending (byte order c < h < n): "capability …" comes before "hello …".
    match run!(b"greet | sort\r", 14) {
        Some(r) => {
            let (cap, hel) = (r.find("capability pipes work"), r.find("hello from godspeed"));
            check!(cap.is_some() && hel.is_some() && cap < hel, "sort: ascending (capability before hello)");
        }
        None => { println!("files-test: FAIL — greet|sort timeout"); fail += 1; }
    }
    // Descending: "no ambient …" comes before "capability …".
    match run!(b"greet | sort reverse\r", 14) {
        Some(r) => {
            let (na, cap) = (r.find("no ambient authority here"), r.find("capability pipes work"));
            check!(na.is_some() && cap.is_some() && na < cap, "sort reverse: descending (no-ambient before capability)");
        }
        None => { println!("files-test: FAIL — greet|sort reverse timeout"); fail += 1; }
    }
    // Direct file sort (/greetout.txt holds greet's lines in original order).
    match run!(b"sort /greetout.txt\r", 10) {
        Some(r) => {
            let (cap, hel) = (r.find("capability pipes work"), r.find("hello from godspeed"));
            check!(cap.is_some() && hel.is_some() && cap < hel, "sort: direct file sort (capability before hello)");
        }
        None => { println!("files-test: FAIL — sort direct timeout"); fail += 1; }
    }
    // Composition: sort is a filter, count after it still sees 3 lines.
    match run!(b"greet | sort | count\r", 16) {
        Some(r) => check!(r.contains("3 lines"), "sort: composes (greet | sort | count → 3 lines)"),
        None    => { println!("files-test: FAIL — greet|sort|count timeout"); fail += 1; }
    }

    // ── first / last: keep the first/last N lines (greet emits hello / capability / no-ambient) ──
    match run!(b"greet | first 1\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && !r.contains("no ambient authority here"),
                          "first: keeps only the first line (first 1)"),
        None    => { println!("files-test: FAIL — greet|first timeout"); fail += 1; }
    }
    match run!(b"greet | last 1\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "last: keeps only the last line (last 1)"),
        None    => { println!("files-test: FAIL — greet|last timeout"); fail += 1; }
    }
    match run!(b"greet | first 2\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("capability pipes work")
                          && !r.contains("no ambient authority here"), "first 2: keeps the first two"),
        None    => { println!("files-test: FAIL — greet|first 2 timeout"); fail += 1; }
    }
    // Default count (no N) = 10, so all 3 greet lines pass.
    match run!(b"greet | last\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("no ambient authority here"),
                          "last: default N=10 (all 3 lines)"),
        None    => { println!("files-test: FAIL — greet|last default timeout"); fail += 1; }
    }
    // Direct form on a file.
    match run!(b"last 1 /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "last: direct file (last 1 /greetout.txt)"),
        None    => { println!("files-test: FAIL — last direct timeout"); fail += 1; }
    }
    // Composition: sort then take the first line → the alphabetically-first ("capability …").
    match run!(b"greet | sort | first 1\r", 16) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("hello from godspeed"),
                          "first: composes after sort (greet | sort | first 1)"),
        None    => { println!("files-test: FAIL — greet|sort|first timeout"); fail += 1; }
    }

    // ── from json: the byte↔record bridge (read text → parse → manipulate → render) ──
    let _ = run!(b"write /rec.json [{\"name\":\"alpha\",\"n\":1},{\"name\":\"beta\",\"n\":2}]\r", 10);
    // read (bytes) → from json (records) → default table render.
    match run!(b"read /rec.json | from json\r", 12) {
        Some(r) => check!(r.contains("alpha") && r.contains("beta") && r.contains("name"),
                          "from json: parses a json file into a table"),
        None    => { println!("files-test: FAIL — from json timeout"); fail += 1; }
    }
    // read → from json → where (record filter on parsed data) → to json.
    match run!(b"read /rec.json | from json | where n>1 | to json\r", 12) {
        Some(r) => check!(r.contains("beta") && !r.contains("alpha"),
                          "from json | where: filters parsed records"),
        None    => { println!("files-test: FAIL — from json|where timeout"); fail += 1; }
    }
    // read → from json → select → to json (column projection on parsed data).
    match run!(b"read /rec.json | from json | select name | to json\r", 12) {
        Some(r) => check!(r.contains("\"name\": \"alpha\"") && !r.contains("\"n\":"),
                          "from json | select: projects parsed columns"),
        None    => { println!("files-test: FAIL — from json|select timeout"); fail += 1; }
    }
    // round-trip across formats: json file → records → yaml → file → read back.
    let _ = run!(b"read /rec.json | from json | to yaml | write /rec.yaml\r", 12);
    match run!(b"read /rec.yaml\r", 10) {
        Some(r) => check!(r.contains("name: alpha") && r.contains("n: 2"),
                          "from json | to yaml | write: json→records→yaml round-trip"),
        None    => { println!("files-test: FAIL — json→yaml roundtrip timeout"); fail += 1; }
    }

    // ── a SERVICE producing records via the binary WIRE CODEC: `roster` builds a Table with the
    //    SDK, `encode`s it, and the shell `decode`s it straight back — NO `from json` round-trip
    //    (docs/records.md, sdk/rust/CLAUDE.md). Proves records cross a service boundary as records.
    match run!(b"roster | where role=core\r", 16) {
        Some(r) => check!(r.contains("vesta") && !r.contains("atlas"),
                          "roster codec: where filters records decoded from the service stream"),
        None    => { println!("files-test: FAIL — roster|where timeout"); fail += 1; }
    }
    match run!(b"roster | select name core | to json\r", 16) {
        Some(r) => check!(r.contains("\"name\": \"hermes\"") && r.contains("\"core\":") && !r.contains("\"role\""),
                          "roster codec: select + to json projects the decoded records"),
        None    => { println!("files-test: FAIL — roster|select timeout"); fail += 1; }
    }
    // It really is a record stream now (not text): a text filter is the loud, guided error.
    match run!(b"roster | match atlas\r", 16) {
        Some(r) => check!(r.contains("record stream") && r.contains("where"),
                          "roster codec: a text filter on the decoded records errors with guidance"),
        None    => { println!("files-test: FAIL — roster|match guard timeout"); fail += 1; }
    }

    // ── ls as a record producer: directory entries become typed rows (name/type/size) ──
    // A dedicated dir with known contents: two files of different size + one subdir.
    let _ = run!(b"mkdir /lsr\r", 10);
    let _ = run!(b"write /lsr/big.txt hello world\r", 10);  // 11 bytes
    let _ = run!(b"write /lsr/tiny.txt x\r", 10);           // 1 byte
    let _ = run!(b"mkdir /lsr/kids\r", 10);                 // a subdirectory
    // bare ls is still the plain text listing (record path is pipe-only).
    match run!(b"ls /lsr\r", 10) {
        Some(r) => check!(r.contains("big.txt") && r.contains("TYPE") && r.contains("dir"),
                          "ls record: bare ls is still the text listing"),
        None    => { println!("files-test: FAIL — ls /lsr timeout"); fail += 1; }
    }
    // where on the `type` column keeps only directories.
    match run!(b"ls /lsr | where type=dir\r", 12) {
        Some(r) => check!(r.contains("kids") && !r.contains("big.txt"),
                          "ls record: where type=dir keeps the subdir, drops files"),
        None    => { println!("files-test: FAIL — ls|where type=dir timeout"); fail += 1; }
    }
    // select projects just the name column (no type/size keys).
    match run!(b"ls /lsr | select name | to json\r", 12) {
        Some(r) => check!(r.contains("\"name\": \"big.txt\"") && !r.contains("\"type\"") && !r.contains("\"size\""),
                          "ls record: select name projects one column"),
        None    => { println!("files-test: FAIL — ls|select timeout"); fail += 1; }
    }
    // where type=file | to json renders file rows with a numeric size, no subdir.
    match run!(b"ls /lsr | where type=file | to json\r", 12) {
        Some(r) => check!(r.contains("\"type\": \"file\"") && r.contains("\"size\":") && !r.contains("kids"),
                          "ls record: where type=file | to json renders file rows"),
        None    => { println!("files-test: FAIL — ls|where|to json timeout"); fail += 1; }
    }
    // column sort works on the listing: reverse size puts big.txt (11) before tiny.txt (1).
    match run!(b"ls /lsr | where type=file | sort reverse size | to json\r", 12) {
        Some(r) => {
            let (big, tiny) = (r.find("big.txt"), r.find("tiny.txt"));
            check!(big.is_some() && tiny.is_some() && big < tiny,
                   "ls record: sort reverse size orders files by byte size");
        }
        None => { println!("files-test: FAIL — ls|sort size timeout"); fail += 1; }
    }
    // a text filter on a record stream is a loud, guided error (not silent, not wrong output).
    match run!(b"ls /lsr | match big\r", 12) {
        Some(r) => check!(r.contains("record stream") && r.contains("where"),
                          "ls record: text filter (match) on records errors with guidance"),
        None    => { println!("files-test: FAIL — ls|match guard timeout"); fail += 1; }
    }

    // ── drives as a record producer: the attached disk as a row (index/label/status/size) ──
    // The test disk is GSFS-formatted (we've been writing files to it).
    match run!(b"drives | to json\r", 12) {
        Some(r) => check!(r.contains("\"status\": \"GSFS\"") && r.contains("\"size_mib\":"),
                          "drives record: to json renders the GSFS drive row"),
        None    => { println!("files-test: FAIL — drives|to json timeout"); fail += 1; }
    }

    // ── find as a record producer: each hit is a row (name/type/path) ──────────────────
    // /lsr holds big.txt + tiny.txt (files) and kids (dir).
    match run!(b"find big /lsr | to json\r", 12) {
        Some(r) => check!(r.contains("big.txt") && r.contains("\"type\": \"file\"") && r.contains("/lsr/big.txt"),
                          "find record: to json renders name/type/path"),
        None    => { println!("files-test: FAIL — find|to json timeout"); fail += 1; }
    }
    // where on the type column: a `*` glob matches all three, where type=dir keeps only kids.
    match run!(b"find * /lsr | where type=dir\r", 12) {
        Some(r) => check!(r.contains("kids") && !r.contains("big.txt"),
                          "find record: where type=dir keeps the subdir, drops the files"),
        None    => { println!("files-test: FAIL — find|where type=dir timeout"); fail += 1; }
    }
    // select projects the path column for files only.
    match run!(b"find *.txt /lsr | select path\r", 12) {
        Some(r) => check!(r.contains("/lsr/big.txt") && r.contains("/lsr/tiny.txt"),
                          "find record: select path projects the matched paths"),
        None    => { println!("files-test: FAIL — find|select timeout"); fail += 1; }
    }

    // ── result: the Ok/Err Result model on `read` (the first command converted). `result` prints
    //    the previous command's result in Rust's shape — Ok, or Err(<Variant>). (/lsr/big.txt was
    //    created in the ls section above.)
    let _ = run!(b"read /lsr/big.txt\r", 10);             // exists → Ok
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Ok") && !r.contains("Err"), "result: Ok after a successful read"),
        None    => { println!("files-test: FAIL — result(ok) timeout"); fail += 1; }
    }
    let _ = run!(b"read /lsr/does_not_exist\r", 10);      // missing → Err(FileNotFound)
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: Err(FileNotFound) after a missing read"),
        None    => { println!("files-test: FAIL — result(err) timeout"); fail += 1; }
    }
    // a blank line is not a command, so it leaves the last result unchanged. (Note `result`
    // itself succeeds, so it would reset to Ok — hence a fresh failing read right before.)
    let _ = run!(b"read /lsr/still_missing\r", 10);       // Err(FileNotFound)
    let _ = run!(b"\r", 8);                               // blank — not a command
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: a blank line leaves the last result unchanged"),
        None    => { println!("files-test: FAIL — result(blank) timeout"); fail += 1; }
    }

    // ── run: execute a script of commands (the .gs runner). Authored on one line with `;`
    //    separators (no newline typing needed). The script reads a present then a missing file,
    //    printing `result` after each — so it exercises echo, execution, and pass/fail counting.
    let _ = run!(b"write /suite.gs read /lsr/big.txt ; result ; read /lsr/nope ; result\r", 10);
    match run!(b"run /suite.gs\r", 16) {
        Some(r) => {
            check!(r.contains("> read /lsr/big.txt") && r.contains("hello world"),
                   "run: echoes and executes a script command");
            check!(r.contains("Err(FileNotFound)"), "run: a failing line surfaces its Err");
            check!(r.contains("run: ran 4, failed 1"), "run: summary counts ran/failed");
        }
        None => { println!("files-test: FAIL — run timeout"); fail += 3; }
    }
    // a missing script reports not found (and `run` returns Err).
    match run!(b"run /no_such.gs\r", 10) {
        Some(r) => check!(r.contains("not found"), "run: a missing script reports not found"),
        None    => { println!("files-test: FAIL — run(missing) timeout"); fail += 1; }
    }
    // scripts cannot nest: a `run` line inside a script is refused.
    let _ = run!(b"write /nest.gs run /suite.gs\r", 10);
    match run!(b"run /nest.gs\r", 14) {
        Some(r) => check!(r.contains("cannot run another script"), "run: nested run is refused (stack-bounded)"),
        None    => { println!("files-test: FAIL — run(nest) timeout"); fail += 1; }
    }

    // ── assert: the verifying command. Content form (the pipe sink) is tested interactively —
    //    a `|` can't yet be authored into a script via `write` (the shell pipes the write line).
    match run!(b"roster | where role=core | assert contains vesta\r", 16) {
        Some(r) => check!(r.contains("assert: ok"), "assert: contains holds on matching output"),
        None    => { println!("files-test: FAIL — assert contains timeout"); fail += 1; }
    }
    match run!(b"roster | where role=worker | assert contains vesta\r", 16) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert: contains fails on non-matching output"),
        None    => { println!("files-test: FAIL — assert contains(fail) timeout"); fail += 1; }
    }
    match run!(b"roster | where role=core | assert lacks atlas\r", 16) {
        Some(r) => check!(r.contains("assert: ok"), "assert: lacks holds when text is absent"),
        None    => { println!("files-test: FAIL — assert lacks timeout"); fail += 1; }
    }
    // result form (negative tests): `fails` holds when the command errors.
    match run!(b"assert fails read /lsr/nope\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "assert: fails holds when the command errors (negative test)"),
        None    => { println!("files-test: FAIL — assert fails timeout"); fail += 1; }
    }
    match run!(b"assert ok read /lsr/big.txt\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "assert: ok holds when the command succeeds"),
        None    => { println!("files-test: FAIL — assert ok timeout"); fail += 1; }
    }
    match run!(b"assert fails read /lsr/big.txt\r", 12) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert: fails reports when a command unexpectedly succeeds"),
        None    => { println!("files-test: FAIL — assert fails(neg) timeout"); fail += 1; }
    }
    // a self-checking script: standalone asserts via `run`, aggregated. (No `|`, so it can be
    // authored with `write`.) Both hold → 0 failures.
    let _ = run!(b"write /check.gs assert ok read /lsr/big.txt ; assert fails read /lsr/nope\r", 10);
    match run!(b"run /check.gs\r", 16) {
        Some(r) => check!(r.contains("run: ran 2, failed 0"), "assert: a self-checking script passes (run aggregates)"),
        None    => { println!("files-test: FAIL — assert script timeout"); fail += 1; }
    }

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
