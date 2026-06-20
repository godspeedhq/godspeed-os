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
    // Step 1: wait for first gsh> — boot complete, shell ready.
    // -----------------------------------------------------------------------
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(boot_out) => {
            check!(boot_out.contains("shell: ready"), "boot: shell ready message");
        }
        None => {
            // Print what we did receive to help diagnose failures.
            let received = {
                let g = buf.lock().unwrap();
                String::from_utf8_lossy(&g).into_owned()
            };
            println!("shell-test: FAIL — timed out waiting for first gsh>");
            println!("shell-test: received so far:\n{received}");
            child.kill().ok();
            child.wait().ok();
            std::process::exit(1);
        }
    }

    // -----------------------------------------------------------------------
    // help
    // -----------------------------------------------------------------------
    // `help` is now paged (the framebuffer console has no scrollback). Drive the pager:
    // page down through every screen (extra page-downs clamp at the bottom, harmless),
    // then `q` to quit. The accumulated byte stream still contains every section, and
    // reaching `gsh>` proves the pager exited cleanly back to the prompt.
    send(&mut write_half, b"help\r          q");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("GodspeedOS shell commands"), "help: header");
            check!(r.contains("spawn"),   "help: spawn listed (paged)");
            check!(r.contains("restart"), "help: restart listed (paged)");
            check!(r.contains("status"),  "help: status listed (paged)");
            check!(r.contains("up/down: scroll") && r.contains("q: quit"), "help: pager status line shown");
        }
        None => {
            println!("shell-test: FAIL — timed out after `help`  [×5]");
            fail += 5;
        }
    }

    // -----------------------------------------------------------------------
    // cores
    // -----------------------------------------------------------------------
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
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
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
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
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("\"state\":"), "status | to json: JSON objects"),
        None    => { println!("shell-test: FAIL — status|to json timeout"); fail += 1; }
    }
    // Compact predicate: where col<op>val (no spaces, no quotes needed).
    send(&mut write_half, b"status | where name=shell\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("shell") && !r.contains("logger"), "status | where name=shell: filters rows"),
        None    => { println!("shell-test: FAIL — status|where timeout"); fail += 1; }
    }
    send(&mut write_half, b"status | where name=shell | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\": \"shell\"") && !r.contains("\"logger\""), "status | where … | to json: filtered JSON"),
        None    => { println!("shell-test: FAIL — status|where|json timeout"); fail += 1; }
    }
    // select: project columns.
    send(&mut write_half, b"status | where name=shell | select name state\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("name") && r.contains("state") && !r.contains("restarts"), "status | select: projects columns"),
        None    => { println!("shell-test: FAIL — status|select timeout"); fail += 1; }
    }
    // to yaml: the other edge rendering.
    send(&mut write_half, b"status | where name=shell | to yaml\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("- slot:") && r.contains("name: shell"), "status | to yaml: YAML mapping list"),
        None    => { println!("shell-test: FAIL — status|to yaml timeout"); fail += 1; }
    }
    // uptime — record producer: bare grid, JSON/YAML rendering, version/help.
    send(&mut write_half, b"uptime\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        // Headers present AND a sane value: within the first hour of this short test boot it must
        // read "0d 00:MM:SS". If boot-time capture had failed (boot=0), now−boot would be ~19000
        // days since 1970 — so "0d 00:" also proves the RTC-delta wall clock is wired correctly.
        Some(r) => check!(r.contains("uptime") && r.contains("seconds") && r.contains("0d 00:"),
                          "uptime: one-row grid (uptime + seconds), sane wall-clock value"),
        None    => { println!("shell-test: FAIL — uptime timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"uptime\":") && r.contains("\"seconds\":"), "uptime | to json: record with uptime + seconds"),
        None    => { println!("shell-test: FAIL — uptime|to json timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | to yaml\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("seconds:") && r.contains("uptime:"), "uptime | to yaml: YAML mapping"),
        None    => { println!("shell-test: FAIL — uptime|to yaml timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | select seconds | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"seconds\":") && !r.contains("\"uptime\":"), "uptime | select seconds: projects the column"),
        None    => { println!("shell-test: FAIL — uptime|select timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("uptime 0.1.0"), "uptime version: number"),
        None    => { println!("shell-test: FAIL — uptime version timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("uptime") && r.contains("seconds since boot"), "uptime help: header + example"),
        None    => { println!("shell-test: FAIL — uptime help timeout"); fail += 1; }
    }
    // Phase-0 of moving naming out of the kernel (docs/naming-design.md): the new
    // SpawnReturningEndpoint syscall hands the caller a SEND|GRANT cap to the spawned service's
    // endpoint. `spawncap pong` spawns pong, gets the cap, and sends a probe through it — proving
    // the returned cap actually routes. The old name-wiring path is untouched (purely additive).
    send(&mut write_half, b"spawncap pong\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(6)) {
        Some(r) => check!(r.contains("endpoint cap acquired; send Ok"),
                          "spawncap: SpawnReturningEndpoint returns a routable endpoint cap"),
        None    => { println!("shell-test: FAIL — spawncap timeout"); fail += 1; }
    }
    // sort by a column (just exercise the path; ordering of the full table is host-dependent).
    send(&mut write_half, b"status | sort name | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("shell"), "status | sort name: sorts the table"),
        None    => { println!("shell-test: FAIL — status|sort timeout"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // starter-pack: echo / about / mem / caps (self)
    // -----------------------------------------------------------------------
    send(&mut write_half, b"echo PINGPONG42\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("PINGPONG42"), "echo: prints its argument"),
        None    => { println!("shell-test: FAIL — timed out after echo"); fail += 1; }
    }

    send(&mut write_half, b"about\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("GodspeedOS"), "about: identity line");
            check!(r.contains("Bankole Ogundero"), "about: creator credit");
        }
        None => { println!("shell-test: FAIL — timed out after about  [×2]"); fail += 2; }
    }

    send(&mut write_half, b"mem\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("mem:") && r.contains("total"), "mem: reports usage"),
        None    => { println!("shell-test: FAIL — timed out after mem"); fail += 1; }
    }

    // date — the RTC clock (QEMU emulates the MC146818 and returns host time).
    // Default form is a full timestamp `Wkd YYYY-MM-DD HH:MM:SS`; `date epoch`
    // prints epoch seconds (digits, no date/time separators).
    send(&mut write_half, b"date\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains('-') && r.contains(':'), "date: full timestamp"),
        None    => { println!("shell-test: FAIL — timed out after date"); fail += 1; }
    }

    send(&mut write_half, b"date epoch\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.chars().any(|c| c.is_ascii_digit()), "date epoch: seconds since 1970"),
        None    => { println!("shell-test: FAIL — timed out after date epoch"); fail += 1; }
    }

    send(&mut write_half, b"caps\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("caps for shell"), "caps (no arg): shows this shell"),
        None    => { println!("shell-test: FAIL — timed out after caps (self)"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // unknown command
    // -----------------------------------------------------------------------
    send(&mut write_half, b"xyzzy\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("unknown: xyzzy"), "unknown command error"),
        None    => { println!("shell-test: FAIL — timed out after unknown command"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Tab completion: single match fills in; multiple → numbered menu, digit selects.
    // -----------------------------------------------------------------------
    // `fc` + Tab → only `fcap` matches → it is filled in. (Ctrl-C clears the line afterward.)
    send(&mut write_half, b"fc\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("fcap"), "tab: single match completes (fc → fcap)"),
        None    => { println!("shell-test: FAIL — timed out after tab(fc)"); fail += 1; }
    }
    // `co` + Tab → cores / copy / count → numbered menu; digit `1` selects `cores`; Enter runs it.
    // The menu redraws its own `gsh> ` prompt, so the first collect ends at the menu; a second
    // collect captures the selection + the executed command's output.
    send(&mut write_half, b"co\t1\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(menu) => check!(menu.contains("1) cores") && menu.contains("copy"), "tab: numbered menu lists candidates"),
        None       => { println!("shell-test: FAIL — timed out waiting for tab menu"); fail += 1; }
    }
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(run) => check!(run.contains(&format!("cores: {smp}")), "tab: digit selects + runs the command (1 → cores)"),
        None      => { println!("shell-test: FAIL — timed out after tab selection"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // pipe errors: loud type-mismatch in `to`, a non-producer source, and the
    // result/assert outcome mix-up
    // -----------------------------------------------------------------------
    // `about` is now a TEXT producer, so `about | to json` must loudly refuse (text isn't records).
    send(&mut write_half, b"about | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("to: input is text, not records"),
                          "pipe: 'to json' on text loudly refuses"),
        None    => { println!("shell-test: FAIL — timed out after to-mismatch pipe"); fail += 1; }
    }
    // A genuine non-producer (an action command) still can't start a pipe. `cd` never runs — the
    // pipe is rejected before stage 1 — so there is no side effect.
    send(&mut write_half, b"cd | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("'cd' cannot start a pipe because it's not a pipe source"),
                          "pipe: non-producer source error"),
        None    => { println!("shell-test: FAIL — timed out after non-producer pipe"); fail += 1; }
    }
    // An ORCHESTRATOR (selfcheck/run) must refuse loudly as a non-producer — NOT run and overflow
    // the stack by nesting captures (the HW shell-crash this guards against). Rejected before it
    // runs, so no drive is touched.
    send(&mut write_half, b"selfcheck | write /x.txt\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("'selfcheck' cannot start a pipe because it's not a pipe source"),
                          "pipe: orchestrator refused as non-producer (no nested-capture crash)"),
        None    => { println!("shell-test: FAIL — timed out after orchestrator pipe"); fail += 1; }
    }
    send(&mut write_half, b"status | result\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("checks a command's outcome, not piped output"),
                          "pipe: result-in-pipe outcome-channel hint"),
        None    => { println!("shell-test: FAIL — timed out after result-in-pipe"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // observe now — the shell brokers a one-shot observe-now service that prints
    // a static metrics frame. Its output is ASYNCHRONOUS (the prompt returns
    // before observe-now is scheduled), so wait on observe's own summary line
    // rather than on gsh>. This also exercises the gated introspection path
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
    // `gsh>` prompt is still in the stream — absorb it before issuing caps.
    let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5));

    // observe now as a record producer (docs/records.md): the only PIPEABLE form (bare
    // `observe` is the live loop). Carries the `ticks` (cumulative cpu-time) column status omits.
    send(&mut write_half, b"observe now | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":"),
                          "observe record: now | to json carries the ticks column"),
        None    => { println!("shell-test: FAIL — observe now|to json timeout"); fail += 1; }
    }
    send(&mut write_half, b"observe now | select name ticks | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":") && !r.contains("\"core\":"),
                          "observe record: select name ticks projects the metric columns"),
        None    => { println!("shell-test: FAIL — observe now|select timeout"); fail += 1; }
    }
    // The live loop must REFUSE to be piped (it owns the screen and never yields a stream),
    // loudly — not hang the shell waiting on a recv that never comes.
    send(&mut write_half, b"observe | sort reverse ticks\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("live view can't be piped"),
                          "observe record: bare live observe refuses to be piped (loud)"),
        None    => { println!("shell-test: FAIL — observe pipe-refusal timeout"); fail += 1; }
    }
    send(&mut write_half, b"caps shell\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
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
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("caps for logger"), "caps logger: header");
            check!(!r.contains("spawn"), "least-privilege: logger does NOT hold spawn");
        }
        None => { println!("shell-test: FAIL — timed out after caps logger"); fail += 2; }
    }

    // caps as a record producer (docs/records.md): piped, it emits resource/rights rows.
    send(&mut write_half, b"caps | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"resource\":") && r.contains("\"rights\":"),
                          "caps record: to json renders resource/rights"),
        None    => { println!("shell-test: FAIL — caps|to json timeout"); fail += 1; }
    }
    // where on the resource column: the shell holds the spawn cap, so this keeps a row.
    send(&mut write_half, b"caps | where resource=spawn\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("spawn") && r.contains("resource"),
                          "caps record: where resource=spawn keeps the spawn cap"),
        None    => { println!("shell-test: FAIL — caps|where timeout"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Singleton guard — spawning an already-live service (here the trusted-root
    // supervisor) must be refused, so the shell can't create a duplicate TCB.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"spawn supervisor\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("Core services") && r.contains("protected"),
                          "spawn: trusted-root refused with reason"),
        None    => { println!("shell-test: FAIL — timed out after spawn supervisor"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Per-utility help / version (0_conventions.md): every utility self-documents,
    // with a real example per usage row and a creator credit in `version`.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"write help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("write 0.1.0") && r.contains("overwrite"), "write help: header + version");
            check!(r.contains("<path>") && r.contains("e.g.") && r.contains("buy milk"), "write help: placeholder + real example");
        }
        None => { println!("shell-test: FAIL — timed out after `write help`  [×2]"); fail += 2; }
    }
    send(&mut write_half, b"ls version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("ls 0.1.0") && r.contains("Created by Bankole Ogundero"), "ls version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `ls version`"); fail += 1; }
    }
    send(&mut write_half, b"drives flash help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("drives flash") && r.contains("drives flash 0 data"), "subcommand help: drives flash help + example"),
        None    => { println!("shell-test: FAIL — timed out after `drives flash help`"); fail += 1; }
    }
    // Record-pipe verbs self-document too (utilities/31_records.md): they are pipe-only
    // stages, but `<verb> help` / `<verb> version` still resolve via the UTILS intercept.
    send(&mut write_half, b"where help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("where 0.1.0") && r.contains("status | where mem>0"), "where help: header + real example"),
        None    => { println!("shell-test: FAIL — timed out after `where help`"); fail += 1; }
    }
    send(&mut write_half, b"to help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("to 0.1.0") && r.contains("to json") && r.contains("to yaml"), "to help: header + json/yaml rows"),
        None    => { println!("shell-test: FAIL — timed out after `to help`"); fail += 1; }
    }
    send(&mut write_half, b"from version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("from 0.1.0") && r.contains("Created by Bankole Ogundero"), "from version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `from version`"); fail += 1; }
    }
    send(&mut write_half, b"select help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("select 0.1.0") && r.contains("status | select name core state"), "select help: header + real example"),
        None    => { println!("shell-test: FAIL — timed out after `select help`"); fail += 1; }
    }
    // The top-level `help` command itself conforms now (0_conventions.md §3, last open item):
    // its categorised list carries the version header (rule 6), and `help help` / `help version`
    // resolve like any other utility.
    send(&mut write_half, b"help version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("help 0.1.0") && r.contains("Created by Bankole Ogundero"), "help version: number + creator credit"),
        None    => { println!("shell-test: FAIL — timed out after `help version`"); fail += 1; }
    }
    send(&mut write_half, b"help help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
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
    let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5));
    send(&mut write_half, b"\x1b[A\r"); // Up arrow, then Enter
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "up-arrow history: recalled + ran the previous command"),
        None    => { println!("shell-test: FAIL — timed out after up-arrow history"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // In-line editing (extended-keyboard navigation cluster). The harness can't see
    // the cursor, but it CAN prove the edit by the resulting command's OUTPUT. Each
    // case builds a different final command via mid-line cursor moves + insert/delete.
    // -----------------------------------------------------------------------
    // Left-arrow + insert: type "echo AC", Left once (cursor between A and C), type "B"
    // → the line is "echo ABC". Output "ABC" proves the B was inserted mid-line.
    send(&mut write_half, b"echo AC\x1b[DB\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("ABC"), "left-arrow + insert: byte lands mid-line (echo ABC)"),
        None    => { println!("shell-test: FAIL — timed out after left+insert edit"); fail += 1; }
    }
    // Home + Right×5 + Delete: type "echo ZABC", Home (ESC[H) to the start, Right 5×
    // (past "echo ") to just before Z, Delete (ESC[3~) removes Z → "echo ABC".
    // Sent in <=16-byte pieces with a gap between so no single escape sequence is split
    // across a UART-FIFO drain (a real keyboard delivers each sequence's bytes atomically;
    // a 27-byte burst would split mid-sequence and isn't representative). esc() helps.
    let esc = |w: &mut std::net::TcpStream, b: &[u8]| { send(w, b); std::thread::sleep(Duration::from_millis(60)); };
    esc(&mut write_half, b"echo ZABC");
    esc(&mut write_half, b"\x1b[H");                                 // Home
    esc(&mut write_half, b"\x1b[C\x1b[C\x1b[C\x1b[C\x1b[C");          // Right x5 (15 bytes)
    esc(&mut write_half, b"\x1b[3~");                                // Delete
    send(&mut write_half, b"\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("ABC") && !r.contains("ZABC"), "home + right + Delete: forward-delete mid-line (echo ABC)"),
        None    => { println!("shell-test: FAIL — timed out after home/delete edit"); fail += 1; }
    }
    // Bare ESC clears the line: type "garbage", press ESC (no following byte → bare ESC),
    // then "cores" + Enter. If ESC cleared, the command is just "cores"; if it didn't, it
    // would be "garbagecores" → unknown. Output "cores: N" proves the clear.
    send(&mut write_half, b"garbage\x1b");
    std::thread::sleep(Duration::from_millis(400)); // let the bare-ESC wait elapse before more bytes
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")) && !r.contains("unknown"), "bare ESC clears the line"),
        None    => { println!("shell-test: FAIL — timed out after bare-ESC clear"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // chaos kill-storm: the bounded resilience exerciser. Kill `registry` 5 times; the supervisor
    // must respawn it each round (registry is auto-restarted). A pass proves: recovery held every
    // round, AND the kernel never panicked (a panic reboots; reaching the verdict + the prompt
    // proves it didn't). registry is the disk-free target, so this runs on the bare-metal build.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos kill-storm registry 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("recovered: 5/5") && r.contains("verdict: PASS"), "chaos: kill-storm registry — 5/5 recovered, PASS");
            check!(r.contains("recovered gen"), "chaos: report has per-round detail");
            check!(r.contains("kernel: alive"), "chaos: kill-storm — kernel alive (no panic)");
        }
        None => { println!("shell-test: FAIL — chaos kill-storm timed out (recovery stuck / panic?)"); fail += 3; }
    }
    // The shell is still responsive after the storm (registry recovered, the prompt works).
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell still responsive after the storm"),
        None    => { println!("shell-test: FAIL — shell unresponsive after chaos"); fail += 1; }
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
    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("drives-test: FAIL — timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // 1. `drives` — a raw, unformatted disk.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("raw") && r.contains("not formatted"), "drives: raw disk listed"),
        None    => { println!("drives-test: FAIL — timed out after `drives`"); fail += 1; }
    }

    // 2. `drives flash data` — confirm the [y/N], then format.
    send(&mut write_half, b"drives flash data\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            check!(true, "flash: destructive [y/N] confirm shown");
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
                Some(r) => check!(r.contains("formatted as GSFS"), "flash: formatted over IPC"),
                None    => { println!("drives-test: FAIL — timed out after confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL — no [y/N] confirm  [×2]"); fail += 2; }
    }

    // 3. `drives` — now a mounted GSFS labelled 'data'.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => {
            check!(r.contains("GSFS"), "drives: now formatted GSFS");
            check!(r.contains("data"), "drives: label 'data' shown");
        }
        None => { println!("drives-test: FAIL — timed out after `drives` (2)  [×2]"); fail += 2; }
    }

    // 4. `drives label archive` — rename, then confirm it stuck.
    send(&mut write_half, b"drives label archive\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("labelled 'archive'"), "label: rename acknowledged"),
        None    => { println!("drives-test: FAIL — timed out after `drives label`"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("archive"), "label: new label 'archive' listed"),
        None    => { println!("drives-test: FAIL — timed out after `drives` (3)"); fail += 1; }
    }

    // 5. `drives reset` — un-format back to raw (confirm [y/N]), then list shows raw.
    send(&mut write_half, b"drives reset\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(15)) {
                Some(r) => check!(r.contains("reset to raw"), "reset: un-formatted to raw"),
                None    => { println!("drives-test: FAIL — timed out after reset confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL — reset: no [y/N] confirm"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
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
            collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs))
        }};
    }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("files-test: FAIL — timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // Format the disk first (file commands need a filesystem).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
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
    // Tab-completion of a FILE PATH: a unique prefix fills in the rest, and the completed command
    // runs. /docs has note.txt + inside.txt, so 'i' and 'n' are unique. \t = Tab, then \r runs it.
    // Absolute path: `read /docs/i<Tab>` → `read /docs/inside.txt ` → runs → inside.txt's content.
    match run!(b"read /docs/i\t\r", 10) {
        Some(r) => check!(r.contains("nested-content"), "tab: /docs/i<Tab> completes to inside.txt + runs"),
        None    => { println!("files-test: FAIL — tab abs-path timeout"); fail += 1; }
    }
    // Relative path (cwd is /docs): `read n<Tab>` → `read note.txt ` → runs → note.txt's content.
    match run!(b"read n\t\r", 10) {
        Some(r) => check!(r.contains("hello world"), "tab: relative n<Tab> completes to note.txt + runs"),
        None    => { println!("files-test: FAIL — tab rel-path timeout"); fail += 1; }
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
            // box-drawing: a depth-1 entry gets a connector; the grandchild b.txt is the last
            // child of `sub`, so it draws `└── ` behind a 4-wide prefix (`    ` or `│   `).
            check!(r.contains("── a.txt"), "tree: child gets a box connector");
            check!(r.contains("    └── b.txt") || r.contains("│   └── b.txt"),
                   "tree: nests a grandchild under its parent (box prefix)");
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
        Some(r) => check!(r.contains("Matthew") && !r.contains("Mark"),
                          "roster codec: where filters records decoded from the service stream"),
        None    => { println!("files-test: FAIL — roster|where timeout"); fail += 1; }
    }
    match run!(b"roster | select name seat | to json\r", 16) {
        Some(r) => check!(r.contains("\"name\": \"Mark\"") && r.contains("\"seat\":") && !r.contains("\"role\""),
                          "roster codec: select + to json projects the decoded records"),
        None    => { println!("files-test: FAIL — roster|select timeout"); fail += 1; }
    }
    // It really is a record stream now (not text): a text filter is the loud, guided error.
    match run!(b"roster | match Mark\r", 16) {
        Some(r) => check!(r.contains("record stream") && r.contains("where"),
                          "roster codec: a text filter on the decoded records errors with guidance"),
        None    => { println!("files-test: FAIL — roster|match guard timeout"); fail += 1; }
    }
    // Bare `roster` (no pipe) renders the same table directly.
    match run!(b"roster\r", 16) {
        Some(r) => check!(r.contains("Matthew") && r.contains("John") && r.contains("role"),
                          "roster: callable bare — renders the record table"),
        None    => { println!("files-test: FAIL — bare roster timeout"); fail += 1; }
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

    // ── run: execute a script of commands (the .gsh runner). Authored on one line with `;`
    //    separators (no newline typing needed). The script reads a present then a missing file,
    //    printing `result` after each — so it exercises echo, execution, and pass/fail counting.
    let _ = run!(b"write /suite.gsh read /lsr/big.txt ; result ; read /lsr/nope ; result\r", 10);
    match run!(b"run /suite.gsh\r", 16) {
        Some(r) => {
            check!(r.contains("> read /lsr/big.txt") && r.contains("hello world"),
                   "run: echoes and executes a script command");
            check!(r.contains("Err(FileNotFound)"), "run: a failing line surfaces its Err");
            check!(r.contains("run: ran 4, failed 1"), "run: summary counts ran/failed");
        }
        None => { println!("files-test: FAIL — run timeout"); fail += 3; }
    }
    // a missing script reports not found (and `run` returns Err).
    match run!(b"run /no_such.gsh\r", 10) {
        Some(r) => check!(r.contains("not found"), "run: a missing script reports not found"),
        None    => { println!("files-test: FAIL — run(missing) timeout"); fail += 1; }
    }
    // scripts cannot nest: a `run` line inside a script is refused.
    let _ = run!(b"write /nest.gsh run /suite.gsh\r", 10);
    match run!(b"run /nest.gsh\r", 14) {
        Some(r) => check!(r.contains("cannot run another script"), "run: nested run is refused (stack-bounded)"),
        None    => { println!("files-test: FAIL — run(nest) timeout"); fail += 1; }
    }

    // ── assert: the verifying command. Content form (the pipe sink) is tested interactively —
    //    a `|` can't yet be authored into a script via `write` (the shell pipes the write line).
    match run!(b"roster | where role=core | assert contains Matthew\r", 16) {
        Some(r) => check!(r.contains("assert: ok"), "assert: contains holds on matching output"),
        None    => { println!("files-test: FAIL — assert contains timeout"); fail += 1; }
    }
    match run!(b"roster | where role=worker | assert contains Matthew\r", 16) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert: contains fails on non-matching output"),
        None    => { println!("files-test: FAIL — assert contains(fail) timeout"); fail += 1; }
    }
    match run!(b"roster | where role=core | assert lacks Mark\r", 16) {
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
    let _ = run!(b"write /check.gsh assert ok read /lsr/big.txt ; assert fails read /lsr/nope\r", 10);
    match run!(b"run /check.gsh\r", 16) {
        Some(r) => check!(r.contains("run: ran 2, failed 0"), "assert: a self-checking script passes (run aggregates)"),
        None    => { println!("files-test: FAIL — assert script timeout"); fail += 1; }
    }

    // ── Result model now spans the file commands + unknown-command. Exercise success and failure
    //    through `assert ok/fails` (the file ops return real Ok/Err, not Ok-wrapped).
    match run!(b"assert fails totallynotacommand\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: an unknown command is now Err (assert fails holds)"),
        None    => { println!("files-test: FAIL — unknown-cmd Err timeout"); fail += 1; }
    }
    match run!(b"assert ok mkdir /rdir\r", 10) {
        Some(r) => check!(r.contains("assert: ok") && r.contains("created /rdir"), "result: mkdir success is Ok"),
        None    => { println!("files-test: FAIL — assert ok mkdir timeout"); fail += 1; }
    }
    match run!(b"assert fails mkdir /no/such/parent/x\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: mkdir into a missing parent is Err"),
        None    => { println!("files-test: FAIL — assert fails mkdir timeout"); fail += 1; }
    }
    match run!(b"assert fails cd /nowhere\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: cd to a missing dir is Err"),
        None    => { println!("files-test: FAIL — assert fails cd timeout"); fail += 1; }
    }
    match run!(b"assert fails delete /nowhere\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: delete of a missing path is Err"),
        None    => { println!("files-test: FAIL — assert fails delete timeout"); fail += 1; }
    }
    // and `result` reflects a converted command directly.
    let _ = run!(b"ls /nowhere\r", 10);
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: ls of a missing dir → Err(FileNotFound)"),
        None    => { println!("files-test: FAIL — ls result timeout"); fail += 1; }
    }

    // ── service-control on the Result model: a protected core service is Err(Denied). ──
    match run!(b"assert fails spawn supervisor\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: spawn of a protected core service is Err (fails holds)"),
        None    => { println!("files-test: FAIL — assert fails spawn timeout"); fail += 1; }
    }
    let _ = run!(b"spawn supervisor\r", 10);
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(Denied)"), "result: spawn supervisor → Err(Denied)"),
        None    => { println!("files-test: FAIL — spawn result timeout"); fail += 1; }
    }

    // ── assert fails-with <Variant>: pin the SPECIFIC failure (precise negative test). ──
    match run!(b"assert fails-with FileNotFound read /nope\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "assert fails-with: holds on the exact variant"),
        None    => { println!("files-test: FAIL — fails-with FileNotFound timeout"); fail += 1; }
    }
    match run!(b"assert fails-with Denied spawn supervisor\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "assert fails-with: Denied for a protected spawn"),
        None    => { println!("files-test: FAIL — fails-with Denied timeout"); fail += 1; }
    }
    match run!(b"assert fails-with Denied read /nope\r", 10) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert fails-with: FAILS when the variant is wrong (got FileNotFound)"),
        None    => { println!("files-test: FAIL — fails-with wrong timeout"); fail += 1; }
    }

    // ── the last stragglers (caps, drives) + info commands are now on the Result model too. ──
    match run!(b"assert fails-with FileNotFound caps nobody\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: caps of a missing service → Err(FileNotFound)"),
        None    => { println!("files-test: FAIL — caps fails-with timeout"); fail += 1; }
    }
    match run!(b"assert ok caps shell\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: caps of a live service is Ok"),
        None    => { println!("files-test: FAIL — caps ok timeout"); fail += 1; }
    }
    match run!(b"assert ok drives\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: drives list (mounted GSFS) is Ok"),
        None    => { println!("files-test: FAIL — drives ok timeout"); fail += 1; }
    }
    match run!(b"assert fails drives bogus\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "result: drives unknown subcommand is Err"),
        None    => { println!("files-test: FAIL — drives fails timeout"); fail += 1; }
    }
    match run!(b"assert ok status\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: an info command (status) is Ok"),
        None    => { println!("files-test: FAIL — status ok timeout"); fail += 1; }
    }

    // chaos `save`: the report is recorded in memory during the storm, then written to fs at the
    // END (the catch-22-safe path — registry is the target, not fs, so fs is free to be written).
    match run!(b"chaos kill-storm registry 3 save /chaos.txt\r", 30) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("report saved to /chaos.txt"),
                          "chaos: storm + save report to a file"),
        None    => { println!("files-test: FAIL — chaos save timeout"); fail += 1; }
    }
    match run!(b"read /chaos.txt\r", 10) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("recovered gen"),
                          "chaos: saved report file holds the verdict + per-round detail"),
        None    => { println!("files-test: FAIL — read chaos report timeout"); fail += 1; }
    }

    // Regression — the registry-bootstrap bug chaos exposed on hardware (the DOUBLE storm).
    // The registry was just stormed above, so the shell's cached `registry` cap is now stale.
    // Storm `fs` too: the shell's `fs` cap dies, so the next storage op must reacquire `fs`
    // THROUGH the registry — which the shell must itself first reacquire from the kernel name
    // table (the bootstrap exception: you can't look the namer up in the namer). Before the
    // fix, the dead registry cap made fs-reacquire fail and storage stayed permanently
    // "unavailable". `ls /` succeeding proves the client resolved a name after a registry
    // restart — the property §22 Test 11 never pinned.
    // Storm fs WITH a save: the report+save run when fs (the target) has just restarted and is
    // still re-registering, so the chaos command must settle + reacquire fs THROUGH the (stale)
    // registry to land the save. This is the catch-22-safe path (record in memory, write at the
    // end) AND the double-storm registry-bootstrap path in one command. Generous timeout: the
    // settle + bounded save-retry yields are slow under TCG.
    match run!(b"chaos kill-storm fs 2 save /fsr.txt\r", 60) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("report saved to /fsr.txt"),
                          "chaos: fs storm recovers + report saved (settle + reacquire fs via stale registry)"),
        None    => { println!("files-test: FAIL — chaos fs storm+save timeout"); fail += 1; }
    }
    match run!(b"read /fsr.txt\r", 10) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("recovered gen"),
                          "chaos: fs-target report file persisted (catch-22-safe save landed)"),
        None    => { println!("files-test: FAIL — read fs chaos report timeout"); fail += 1; }
    }
    match run!(b"ls /\r", 10) {
        Some(r) => check!(!r.contains("storage unavailable"),
                          "registry bootstrap: shell reacquires fs through a restarted registry (double-storm)"),
        None    => { println!("files-test: FAIL — ls after double-storm timeout"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();
    println!("\nfiles-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// `osdev test edit` — drive the full-screen `edit` editor over the serial console end to end:
/// open a NEW file, type (with a backspace), save (^S) + quit (^Q), and `read` it back to prove
/// the bytes persisted; re-open the existing file and insert at the start; then open it, type
/// junk, ^Q and DISCARD (n) at the unsaved-changes prompt, and read to prove the junk was not
/// saved. Plus the no-arg usage. The editor's own TUI repaint is in the byte stream, but every
/// assertion is on the post-edit `read` output (captured after the prompt returns), never the
/// repaint — so we verify what actually hit the filesystem, not what was drawn.
pub fn run_edit(image_path: &Path, persist_path: &str, smp: u32) {
    println!("edit-test: booting (smp={smp}) with a RAW AHCI disk — scripted mode");

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
        eprintln!("edit-test: QEMU launch failed at {qemu}: {e}");
        std::process::exit(1);
    });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("edit-test: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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
            if $ok { println!("edit-test: PASS — {}", $label); pass += 1; }
            else   { println!("edit-test: FAIL — {}", $label); fail += 1; }
        };
    }
    // Send a command, then read the `read`-back output up to the next prompt.
    macro_rules! read_back {
        ($c:expr) => {{
            send(&mut write_half, $c);
            collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(8))
        }};
    }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("edit-test: FAIL — timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // The disk is pre-formatted host-side with a large baked file (`/big.txt`, ~400 lines / several
    // IO_CHUNK windows). Confirm fs mounted it — proves the editor has a filesystem to save to and
    // gives the large-file tests their fixture.
    match read_back!(b"ls /\r") {
        Some(r) => check!(r.contains("big.txt"), "setup: pre-baked /big.txt present"),
        None    => { println!("edit-test: FAIL — ls / timeout"); fail += 1; }
    }

    // 1. no-arg usage.
    match read_back!(b"edit\r") {
        Some(r) => check!(r.contains("usage: edit"), "edit (no arg): prints usage"),
        None    => { println!("edit-test: FAIL — usage timeout"); fail += 1; }
    }

    // 2. NEW file: type (with a backspace + a newline), save (^S), quit (^Q), read back.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        check!(true, "edit /e.txt: editor opened (status bar shown)");
        // "hello worldX" then Backspace (DEL) deletes the X; Enter inserts a newline; "second line".
        send(&mut write_half, b"hello worldX\x7f\rsecond line\x13"); // ^S save
        thread::sleep(Duration::from_millis(400));                   // let the save's fs round-trip land
        send(&mut write_half, b"\x11");                              // ^Q — unmodified after save → clean quit
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
            Some(_) => check!(true, "edit /e.txt: ^S then ^Q returned to the prompt"),
            None    => { println!("edit-test: FAIL — editor did not return to prompt"); fail += 1; }
        }
    } else { println!("edit-test: FAIL — editor did not open (×2)"); fail += 2; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => {
            check!(r.contains("hello world") && r.contains("second line"), "read /e.txt: saved text present");
            check!(!r.contains("worldX"), "read /e.txt: backspace took effect (no 'worldX')");
        }
        None => { println!("edit-test: FAIL — read after edit timeout (×2)"); fail += 2; }
    }

    // 3. EXISTING file: cursor opens at the start; insert "TOP ", save, quit, read.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"TOP \x13");
        thread::sleep(Duration::from_millis(400));
        send(&mut write_half, b"\x11");
        let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10));
    } else { println!("edit-test: FAIL — re-open editor"); fail += 1; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => check!(r.contains("TOP hello world"), "edit existing: insert-at-start saved ('TOP hello world')"),
        None    => { println!("edit-test: FAIL — read after edit-existing timeout"); fail += 1; }
    }

    // 4. Quit with unsaved changes, DISCARD (n) — the junk must NOT persist.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"ZZZJUNK\x11"); // type junk (now modified), then ^Q → prompt
        if collect_until(&buf, &mut cursor, b"discard", Duration::from_secs(6)).is_some() {
            check!(true, "edit: ^Q with unsaved changes shows the discard prompt");
            send(&mut write_half, b"n"); // n = discard & quit
            let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(8));
        } else { println!("edit-test: FAIL — no discard prompt"); fail += 1; }
    } else { println!("edit-test: FAIL — re-open editor for discard"); fail += 1; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => check!(r.contains("TOP hello world") && !r.contains("ZZZJUNK"), "edit discard: junk NOT saved (file unchanged)"),
        None    => { println!("edit-test: FAIL — read after discard timeout"); fail += 1; }
    }

    // ── Large file (piece-table windowed load + streaming save) ─────────────────────────────────
    // The editor is DRIVEN over serial; the result is verified on the DISK afterwards (a 16 KiB
    // file dumped back over the console renders ~400 lines on the fbcon — far too slow under TCG —
    // and the file commands that emit small output, `match`/`count`, read via one ≤4 KiB message so
    // they can't read it either). The disk is the actual deliverable, so we assert on it directly.
    //
    // 5. Open the multi-window /big.txt and insert "AAA " at offset 0 (start-edit), save, quit. The
    //    save streams ALL spans (the typed prefix + the windowed original tail) to a temp file and
    //    atomically replaces /big.txt.
    send(&mut write_half, b"edit /big.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(12)).is_some() {
        check!(true, "edit /big.txt: large file opened (windowed)");
        send(&mut write_half, b"AAA \x13");          // insert at start, ^S
        thread::sleep(Duration::from_millis(1200));   // multi-chunk save round-trip
        send(&mut write_half, b"\x11");               // ^Q (clean after save)
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(12)) {
            Some(_) => check!(true, "edit /big.txt: start-edit saved + returned to prompt"),
            None    => { println!("edit-test: FAIL — big.txt edit did not return"); fail += 1; }
        }
    } else { println!("edit-test: FAIL — big.txt did not open (×2)"); fail += 2; }

    // 6. Re-open /big.txt, PageDown into the file (windowed navigation past the first window), type
    //    a mid-file marker, save, quit. Exercises an edit that isn't in window 0.
    send(&mut write_half, b"edit /big.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(12)).is_some() {
        send(&mut write_half, b"\x1b[6~");            // PageDown (one screen down — past row 0)
        thread::sleep(Duration::from_millis(200));
        send(&mut write_half, b"MID \x13");           // insert mid-file marker, ^S
        thread::sleep(Duration::from_millis(1200));
        send(&mut write_half, b"\x11");               // ^Q
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(12)) {
            Some(_) => check!(true, "edit /big.txt: mid-file edit saved + returned to prompt"),
            None    => { println!("edit-test: FAIL — big.txt mid-edit did not return"); fail += 1; }
        }
    } else { println!("edit-test: FAIL — re-open big.txt for mid-edit"); fail += 1; }

    child.kill().ok();
    child.wait().ok();

    // Verify the SAVED bytes on the disk (the editor's actual output). Parse the GSFS root for
    // /big.txt, read its data region, and assert both edits landed and the far tail survived the
    // streaming save — windowed load + multi-chunk save proven end to end, without rendering 16 KiB.
    match gsfs_read_file(persist_path, b"big.txt") {
        Some(content) => {
            check!(content.starts_with(b"AAA EDITLINE 0000"),
                   "big.txt (disk): start-edit at offset 0 ('AAA EDITLINE 0000')");
            check!(window_find(&content, b"EDITLINE 0399").is_some(),
                   "big.txt (disk): windowed original tail preserved ('EDITLINE 0399')");
            check!(window_find(&content, b"MID ").is_some(),
                   "big.txt (disk): mid-file edit present ('MID ')");
        }
        None => { println!("edit-test: FAIL — could not read /big.txt back from the disk (×3)"); fail += 3; }
    }

    println!("\nedit-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// §22 Test 13 — **fs survives its own restart** (Phase D). Drive the shell on COM1 to write a
/// file, KILL `fs` over the COM2 control channel, then read the file back: the supervisor
/// respawns `fs`, `fs` re-mounts (the data persisted on disk), and the shell reacquires a fresh
/// `fs` cap via the registry (§14.3) — the file reads back, the kernel never panics. This is
/// the executable proof of the §6 amendment that made `fs`/`block-driver` restartable.
pub fn run_fs_restart(image_path: &Path, persist_path: &str, smp: u32) {
    println!("fs-restart: booting (smp={smp}) bare-metal + AHCI disk; shell on COM1, control on COM2");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path).unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();
    let ctrl_port  = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        "-serial",  &format!("tcp::{shell_port},server"),         // COM1: shell I/O + logs (QEMU waits for us)
        "-serial",  &format!("tcp::{ctrl_port},server,nowait"),   // COM2: control channel — nowait (we connect later)
        "-display", "none", "-no-reboot", "-no-shutdown",
    ])
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fs-restart: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("fs-restart: could not connect to shell serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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
    macro_rules! check { ($ok:expr, $label:expr) => {
        if $ok { println!("fs-restart: PASS — {}", $label); pass += 1; }
        else   { println!("fs-restart: FAIL — {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{
        send(&mut write_half, $c);
        collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs))
    }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-restart: FAIL — timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // Flash the disk, then write a file and read it back (pre-restart baseline).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
            Some(r) => check!(r.contains("formatted as GSFS"), "setup: flashed GSFS"),
            None    => { println!("fs-restart: FAIL — flash timeout"); fail += 1; }
        }
    } else { println!("fs-restart: FAIL — no flash confirm"); fail += 1; }
    match run!(b"write /t.txt survives-restart\r", 10) {
        Some(r) => check!(r.contains("wrote /t.txt"), "wrote /t.txt before restart"),
        None    => { println!("fs-restart: FAIL — write timeout"); fail += 1; }
    }
    match run!(b"read /t.txt\r", 10) {
        Some(r) => check!(r.contains("survives-restart"), "read /t.txt before restart"),
        None    => { println!("fs-restart: FAIL — read timeout"); fail += 1; }
    }

    // KILL fs over the COM2 control channel (kernel-side, no service_control cap needed).
    println!("fs-restart: sending 'KILL fs' over the control channel …");
    match retry_tcp_connect(ctrl_port, Duration::from_secs(10)) {
        Some(mut ctrl) => { thread::sleep(Duration::from_millis(100)); send(&mut ctrl, b"\nKILL fs\n");
            // The supervisor observes the death and respawns fs; wait for it to come back up.
            let restarted = collect_until(&buf, &mut cursor, b"supervisor: fs restarted", Duration::from_secs(20));
            check!(restarted.is_some(), "supervisor observed fs death and restarted it");
            // Wait until the fresh fs is serving again (it has re-mounted + re-registered).
            let serving = collect_until(&buf, &mut cursor, b"fs: serving file API", Duration::from_secs(20));
            check!(serving.is_some(), "restarted fs re-mounted and is serving");
            drop(ctrl);
        }
        None => { println!("fs-restart: FAIL — could not connect to control port"); fail += 1; }
    }

    // Read the file back: the shell must reacquire a fresh fs cap via the registry, and the
    // file must still be there (persisted on disk, recovered on remount). The headline check.
    // Retry a couple of times — reacquire returns None until fs has finished re-registering.
    let mut got = false;
    for _ in 0..4 {
        if let Some(r) = run!(b"read /t.txt\r", 10) {
            if r.contains("survives-restart") { got = true; break; }
        }
        thread::sleep(Duration::from_millis(500));
    }
    check!(got, "read /t.txt AFTER restart (shell reacquired fs, file persisted)");

    // No panic anywhere in the whole session.
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "kernel never panicked across the restart");

    child.kill().ok();
    child.wait().ok();
    println!("\nfs-restart: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// §22 Test 14 — file-as-capability (P2). Boot a pre-formatted disk, then run the shell's argless
/// `fcap` command, which creates its own throwaway file, opens it as a real kernel capability, and
/// self-checks every property: read/write via the cap, non-escalation (RO cap can't write at the
/// kernel OR fs layer), forged-handle rejection, revoke-on-close. We assert each line + the summary.
pub fn run_fs_filecap(image_path: &Path, persist_path: &str, smp: u32) {
    println!("file-cap: booting (smp={smp}) bare-metal + AHCI disk for the file-as-capability test");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path).unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("file-cap: QEMU launch failed: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("file-cap: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone tcp stream");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop { match read_half.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]) } }
        });
    }

    let mut pass = 0usize; let mut fail = 0usize; let mut cursor = 0usize;
    macro_rules! check { ($ok:expr, $label:expr) => {
        if $ok { println!("file-cap: PASS — {}", $label); pass += 1; } else { println!("file-cap: FAIL — {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("file-cap: FAIL — timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // `fcap` is self-contained: it creates and deletes its own throwaway file, so it takes no
    // argument and never touches a user's file.
    match run!(b"fcap\r", 15) {
        Some(r) => {
            check!(r.contains("opened rw (file cap)"), "fs minted a real file capability on open");
            check!(r.contains("write via cap OK"),     "wrote the file THROUGH the cap");
            check!(r.contains("read via cap OK"),      "read it back THROUGH the cap");
            check!(r.contains("rejected by kernel (non-escalation)"), "kernel refuses a RO cap's WRITE invoke (non-escalation)");
            check!(r.contains("op<=right"),            "fs refuses a write op under a read-validated right");
            check!(r.contains("forged handle rejected"), "a fabricated handle is not a capability (unforgeable)");
            check!(r.contains("revoked after close"),  "the cap is revoked on close (revocable)");
            check!(r.contains("revoked after rename"),  "rename revokes the cap (no confused-deputy via path reuse)");
            check!(r.contains("all file-capability checks passed"), "every file-cap property held");
        }
        None => { println!("file-cap: FAIL — fcap timed out"); fail += 1; }
    }
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic");

    child.kill().ok(); child.wait().ok();
    println!("\nfile-cap: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// `drives check` (fsck, Phase G): boot a PRE-FORMATTED disk whose superblock free count has
/// been deliberately drifted host-side (CRC re-stamped so it still mounts). `drives check`
/// must rebuild the free count from the tree and report the correct value (`expect_free`), with
/// 0 bad blocks, and the baked file must still read back. Proves the recovery layer repairs
/// allocation drift non-destructively.
pub fn run_fs_check(image_path: &Path, persist_path: &str, expect_free: u64, smp: u32) {
    println!("fs-check: booting (smp={smp}) with a pre-formatted disk whose free count was drifted");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path).unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fs-check: QEMU launch failed: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("fs-check: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone tcp stream");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop {
                match read_half.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]) }
            }
        });
    }

    let mut pass = 0usize; let mut fail = 0usize; let mut cursor = 0usize;
    macro_rules! check { ($ok:expr, $label:expr) => {
        if $ok { println!("fs-check: PASS — {}", $label); pass += 1; } else { println!("fs-check: FAIL — {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-check: FAIL — timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // The disk is already formatted (drifted free count); fs auto-mounted it. Run the fsck.
    let expect = format!("{} free", expect_free);
    match run!(b"drives check\r", 15) {
        Some(r) => {
            check!(r.contains(&expect), "free count rebuilt from the tree to the correct value");
            check!(r.contains("0 bad"), "no corrupt blocks reported");
            check!(r.contains("ok") || r.contains("consistent"), "reports consistent");
        }
        None => { println!("fs-check: FAIL — drives check timeout"); fail += 1; }
    }
    // The baked file survived the repair.
    match run!(b"read /alpha.txt\r", 10) {
        Some(r) => check!(r.contains("alpha-payload"), "baked file still reads back after check"),
        None    => { println!("fs-check: FAIL — read timeout"); fail += 1; }
    }
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic");

    child.kill().ok(); child.wait().ok();
    println!("\nfs-check: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// `drives scrub` (Phase K): boot a PRE-FORMATTED disk holding a clean file and a file with a
/// CORRUPTED data block. `drives scrub` must report `1 bad` without panicking, leave the disk
/// UNCHANGED (a second scrub still reports `1 bad` — read-only, no repair), and the clean file
/// must still read back. Proves a routine, non-destructive integrity sweep that detects bit-rot.
pub fn run_fs_scrub(image_path: &Path, persist_path: &str, smp: u32) {
    println!("fs-scrub: booting (smp={smp}) with a disk holding a clean file + a corrupted one");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let persist   = std::fs::canonicalize(persist_path).unwrap_or_else(|_| std::path::PathBuf::from(persist_path));
    let persist_str = persist.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={persist_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fs-scrub: QEMU launch failed: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("fs-scrub: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone tcp stream");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let buf2 = Arc::clone(&buf);
        thread::spawn(move || {
            let mut tmp = [0u8; 256];
            loop {
                match read_half.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]) }
            }
        });
    }

    let mut pass = 0usize; let mut fail = 0usize; let mut cursor = 0usize;
    macro_rules! check { ($ok:expr, $label:expr) => {
        if $ok { println!("fs-scrub: PASS — {}", $label); pass += 1; } else { println!("fs-scrub: FAIL — {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-scrub: FAIL — timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // First scrub: must detect the one corrupt file (read-only — reports, does not repair).
    match run!(b"drives scrub\r", 15) {
        Some(r) => {
            check!(r.contains("verified") && r.contains("bad"), "scrub ran and reported a block count");
            check!(r.contains("1 bad"), "scrub detected the 1 corrupt file");
            check!(r.contains("WARNING") && r.contains("bit-rot"), "scrub warned loudly about the bad block");
        }
        None => { println!("fs-scrub: FAIL — drives scrub timeout"); fail += 1; }
    }
    // Second scrub: identical result proves scrub is READ-ONLY (it did not repair the block).
    match run!(b"drives scrub\r", 15) {
        Some(r) => check!(r.contains("1 bad"), "second scrub still reports 1 bad (read-only, nothing repaired)"),
        None    => { println!("fs-scrub: FAIL — second drives scrub timeout"); fail += 1; }
    }
    // The clean file is untouched by the scrub.
    match run!(b"read /good.txt\r", 10) {
        Some(r) => check!(r.contains("good-payload-survives-the-scrub"), "clean file still reads back after scrub"),
        None    => { println!("fs-scrub: FAIL — read timeout"); fail += 1; }
    }
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic");

    child.kill().ok(); child.wait().ok();
    println!("\nfs-scrub: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// GSFS0008 feature-flag compatibility policy (Phase L): boot, one after another, three disks
/// that each carry an UNKNOWN feature bit this build doesn't recognise, and assert the mount
/// policy: an unknown `incompat` bit → REFUSE to mount (loud); an unknown `ro_compat` bit → mount
/// READ-ONLY (reads work, writes refused); an unknown `compat` bit → mount NORMALLY (writes work).
/// This is what lets the format evolve past 0008 without a reformat-only bump. Each disk also has
/// a baked `/baked.txt` so the read/write distinction is observable.
pub fn run_fs_compat(image_path: &Path, disk_incompat: &str, disk_ro: &str, disk_compat: &str, smp: u32) {
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let mut pass = 0usize; let mut fail = 0usize;
    macro_rules! check { ($ok:expr, $label:expr) => {
        if $ok { println!("fs-compat: PASS — {}", $label); pass += 1; } else { println!("fs-compat: FAIL — {}", $label); fail += 1; }
    }; }

    // Boot one disk, drive a few shell commands, return (per-command outputs, whole serial log).
    let boot = |disk_path: &str, cmds: &[&str]| -> (Vec<String>, String) {
        let disk = std::fs::canonicalize(disk_path).unwrap_or_else(|_| std::path::PathBuf::from(disk_path));
        let disk_str = disk.to_string_lossy().replace('\\', "/");
        let port = pick_free_port();
        let mut cmd = std::process::Command::new(&qemu);
        cmd.args([
            "-drive",   &format!("format=raw,file={image_str},if=ide"),
            "-device",  "ich9-ahci,id=ahci",
            "-drive",   &format!("id=data,format=raw,file={disk_str},if=none"),
            "-device",  "ide-hd,drive=data,bus=ahci.0",
            "-smp",     &smp.to_string(), "-m", "512M",
            "-serial",  &format!("tcp::{port},server"),
            "-serial",  "null",
            "-display", "none", "-no-reboot", "-no-shutdown",
        ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fs-compat: QEMU launch failed: {e}"); std::process::exit(1); });
        let stream = match retry_tcp_connect(port, Duration::from_secs(10)) {
            Some(s) => s,
            None => { eprintln!("fs-compat: could not connect to serial {port}"); child.kill().ok(); std::process::exit(1); }
        };
        let mut read_half = stream.try_clone().expect("clone tcp stream");
        let mut write_half = stream;
        let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let buf2 = Arc::clone(&buf);
            thread::spawn(move || {
                let mut tmp = [0u8; 256];
                loop { match read_half.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]) } }
            });
        }
        let mut cursor = 0usize;
        let mut outs = Vec::new();
        if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_some() {
            for c in cmds {
                let line = format!("{c}\r");
                send(&mut write_half, line.as_bytes());
                outs.push(collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(12)).unwrap_or_default());
            }
        }
        let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
        child.kill().ok(); child.wait().ok();
        (outs, whole)
    };

    // ── Scenario 1: unknown INCOMPAT bit → refuse to mount ──
    println!("fs-compat: boot 1 — disk with an unknown INCOMPAT feature (must refuse to mount), ~40s …");
    let (o1, log1) = boot(disk_incompat, &["read /baked.txt"]);
    check!(log1.contains("incompatible features"), "incompat: mount refused loudly (incompatible features)");
    check!(log1.contains("no filesystem") || log1.contains("awaiting drives flash"), "incompat: reported no usable filesystem");
    check!(!o1.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "incompat: file is NOT readable (did not mount)");
    check!(!log1.contains("KERNEL PANIC"), "incompat: no kernel panic");

    // ── Scenario 2: unknown RO_COMPAT bit → mount read-only ──
    println!("fs-compat: boot 2 — disk with an unknown RO_COMPAT feature (must mount READ-ONLY), ~40s …");
    let (o2, log2) = boot(disk_ro, &["read /baked.txt", "write /x.txt should-fail"]);
    check!(log2.contains("READ-ONLY"), "ro_compat: mounted read-only (loud)");
    check!(o2.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "ro_compat: reads still work (baked file readable)");
    check!(!o2.get(1).map(|s| s.contains("wrote") || s.contains("ok")).unwrap_or(false), "ro_compat: write was refused");
    check!(!log2.contains("KERNEL PANIC"), "ro_compat: no kernel panic");

    // ── Scenario 3: unknown COMPAT bit → mount normally, read-write ──
    println!("fs-compat: boot 3 — disk with an unknown COMPAT feature (must mount read-write), ~40s …");
    let (o3, log3) = boot(disk_compat, &["read /baked.txt", "write /x.txt hello-compat", "read /x.txt"]);
    check!(!log3.contains("READ-ONLY") && !log3.contains("incompatible"), "compat: mounted normally (not read-only, not refused)");
    check!(o3.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "compat: baked file readable");
    check!(o3.get(2).map(|s| s.contains("hello-compat")).unwrap_or(false), "compat: write+read-back works (read-write)");
    check!(!log3.contains("KERNEL PANIC"), "compat: no kernel panic");

    println!("\nfs-compat: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Boot bare-metal with a GSFS disk that has a self-checking `.gsh` baked in (host-side), then
/// `run /<script_name>` — proving the flash-and-run loop and piped asserts in a script.
pub fn run_script(image_path: &Path, disk_path: &str, script_name: &str, smp: u32) {
    println!("script-test: booting (smp={smp}) with a host-baked GSFS suite disk");

    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let disk      = std::fs::canonicalize(disk_path).unwrap_or_else(|_| std::path::PathBuf::from(disk_path));
    let disk_str  = disk.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-device",  "ich9-ahci,id=ahci",
        "-drive",   &format!("id=data,format=raw,file={disk_str},if=none"),
        "-device",  "ide-hd,drive=data,bus=ahci.0",
        "-smp",     &smp.to_string(),
        "-m",       "512M",
        "-serial",  &format!("tcp::{shell_port},server"),
        "-serial",  "null",
        "-display", "none", "-no-reboot", "-no-shutdown",
    ])
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| {
        eprintln!("script-test: QEMU launch failed at {qemu}: {e}"); std::process::exit(1);
    });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("script-test: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("script-test: FAIL — timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // Run the baked suite. The disk is GSFS (baked host-side), so the OS mounts it on boot and
    // /<script_name> is present — no on-device authoring.
    send(&mut write_half, format!("run /{script_name}\r").as_bytes());
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            println!("\n=== baked suite transcript ===\n{}\n=== end ===", r.trim());
            // Green iff the run summary is "failed 0" AND no assert printed a FAILED line.
            if r.contains("failed 0") && !r.contains("FAILED") {
                println!("script-test: PASS — baked suite ran green (failed 0)"); pass += 1;
            } else {
                println!("script-test: FAIL — baked suite not green"); fail += 1;
            }
        }
        None => { println!("script-test: FAIL — `run /{script_name}` timed out"); fail += 1; }
    }

    // `run … save <path>` — the orchestrator writes its OWN report to a file (direct, NOT a pipe),
    // so it can save while running its own inner pipelines (incl. `… | assert`, the heavy case)
    // WITHOUT the nested-capture stack overflow that `<orchestrator> | write` causes. Proves: no
    // crash/refusal, and the report file holds the tally.
    send(&mut write_half, format!("run /{script_name} save /report.txt\r").as_bytes());
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => if r.contains("saved report") && !r.contains("cannot start a pipe") && !r.contains("PUSER") {
            println!("script-test: PASS — run save: orchestrator wrote its report file (no crash)"); pass += 1;
        } else {
            println!("script-test: FAIL — run save did not write the report (refused/crashed?)"); fail += 1;
        },
        None    => { println!("script-test: FAIL — `run … save` timed out (stack overflow?)"); fail += 1; }
    }
    send(&mut write_half, b"read /report.txt\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
        Some(r) => if r.contains("ran ") && r.contains("failed 0") {
            println!("script-test: PASS — run save: report file holds the tally (ran N, failed 0)"); pass += 1;
        } else {
            println!("script-test: FAIL — report file missing the tally"); fail += 1;
        },
        None    => { println!("script-test: FAIL — read /report.txt timed out"); fail += 1; }
    }

    // Embed-and-autoprovision: `selfcheck` runs the shell-embedded extensive suite IN MEMORY
    // (no host bake) — the one-USB hardware path where the operator flashes only os.img,
    // `drives flash`es the SSD, then types `selfcheck`. The big suite + many service spawns
    // take a while under TCG, so allow a generous wall-clock window.
    send(&mut write_half, b"selfcheck\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(150)) {
        Some(r) => {
            if r.contains("failed 0") && !r.contains("FAILED") {
                println!("script-test: PASS — embedded `selfcheck` ran green (failed 0)"); pass += 1;
            } else {
                println!("\n=== selfcheck transcript ===\n{}\n=== end ===", r.trim());
                println!("script-test: FAIL — embedded `selfcheck` not green"); fail += 1;
            }
        }
        None => { println!("script-test: FAIL — `selfcheck` timed out"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();
    println!("\nscript-test: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
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

/// Read a top-level GSFS0008 file's bytes back from a disk image, host-side. Parses the superblock
/// for the root directory block, finds the named record, and concatenates its contiguous data
/// blocks' 508-byte payloads up to the file size. Used to verify the editor's on-disk save without
/// dumping the file over the (slow, capped) serial console. Contiguous files only (`type` 1) —
/// which is what a fresh-disk streaming save produces; returns None otherwise.
fn gsfs_read_file(image_path: &str, name: &[u8]) -> Option<Vec<u8>> {
    const BLOCK: usize = 512;
    const PAYLOAD: usize = 508;   // data bytes per block (last 4 = CRC32)
    const RECS: usize = 7;        // file_records per directory block
    let data = std::fs::read(image_path).ok()?;
    if data.len() < BLOCK || &data[0..8] != b"GSFS0008" { return None; }
    let root_first = u64::from_le_bytes(data.get(48..56)?.try_into().ok()?) as usize;
    let rd = root_first * BLOCK;
    for slot in 0..RECS {
        let r = rd + slot * 64;
        if data.get(r).copied()? != 1 { continue; }            // type 1 = contiguous file
        let nl = data[r + 1] as usize;
        if nl == 0 || nl > 38 || &data[r + 2..r + 2 + nl] != name { continue; }
        let size = u64::from_le_bytes(data[r + 40..r + 48].try_into().ok()?) as usize;
        let first = u64::from_le_bytes(data[r + 48..r + 56].try_into().ok()?) as usize;
        let nblocks = size.div_ceil(PAYLOAD).max(1);
        let mut out = Vec::with_capacity(size);
        for k in 0..nblocks {
            let bo = (first + k) * BLOCK;
            let take = (size - out.len()).min(PAYLOAD);
            out.extend_from_slice(data.get(bo..bo + take)?);
        }
        return Some(out);
    }
    None
}
