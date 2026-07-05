// SPDX-License-Identifier: GPL-2.0-only
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
    println!("shell-test: booting OS (smp={smp}) - scripted mode");

    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");

    // COM1 → TCP server on a FRESH free port per run. A fixed port (was 5556)
    // collided with a stale QEMU left over from a previous/concurrent run, which
    // showed up as "could not connect". An ephemeral port can't collide - a leftover
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
        // Networking Phase 0: an e1000 NIC so the kernel's PCI scan prints it (docs/networking.md).
        // Confirms the detection works in QEMU + that the NIC doesn't disturb boot (the rest of the suite).
        "-device",  "e1000,netdev=n0",
        "-netdev",  "user,id=n0",
        // Phase 1 step 3: dump every frame on the NIC backend to a pcap, so we can confirm
        // nic-driver's TX frame actually left the card, not just that the NIC set DD.
        "-object",  "filter-dump,id=nicdump,netdev=n0,file=build/net-tx.pcap",
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
                println!("shell-test: PASS - {}", $label);
                pass += 1;
            } else {
                println!("shell-test: FAIL - {}", $label);
                fail += 1;
            }
        };
    }

    // -----------------------------------------------------------------------
    // Step 1: wait for first gsh> - boot complete, shell ready.
    // -----------------------------------------------------------------------
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(boot_out) => {
            check!(boot_out.contains("shell: ready"), "boot: shell ready message");
            // Naming migration (docs/naming-design.md): the supervisor builds a name→cap map as it
            // spawns the real services, then wires dependents from it. Bare-metal maps 6 services
            // (block-driver, fs, shell, xhci, ehci, nic-driver); names resolve via the kernel
            // directory, with no separate name service spawned.
            check!(boot_out.contains("name-cap map holds 7 service(s)"),
                   "naming: supervisor holds an endpoint cap for every real service");
            check!(!boot_out.contains("spawning registry") && !boot_out.contains("name-map + registry"),
                   "naming: no separate name service is spawned (the kernel directory resolves names)");
            // fs (block-driver) and shell (fs) are wired from the supervisor's map. Functional proof
            // = the files test (real disk I/O, file commands reaching fs, fs reaching block-driver).
            check!(boot_out.contains("fs wired from the name-cap map"),
                   "naming Phase 2: fs's block-driver peer wired from the map");
            check!(boot_out.contains("shell wired from the name-cap map"),
                   "naming Phase 3a: shell's fs peer wired from the map");
            // Networking Phase 0 (docs/networking.md): the kernel's PCI scan prints the NIC. QEMU's
            // e1000 = Intel 82540EM (vendor 0x8086). Confirms the detection path; on the T630 the same
            // print names whatever chipset it has.
            check!(boot_out.contains("pci: NIC") && boot_out.contains("vendor=0x8086"),
                   "phase0: e1000 NIC detected + printed at boot (vendor=0x8086)");
            // Networking Phase 1 step 2 (docs/networking.md): nic-driver receives the e1000's BAR by
            // name, RESETS the controller, and reads the MAC it reloaded from EEPROM. QEMU's default
            // e1000 MAC is 52:54:00:12:34:56. This proves PCI -> MMIO cap -> register R/W -> reset,
            // end to end - the foundation the whole stack (ARP/IP/ICMP/UDP/TCP) builds on.
            check!(boot_out.contains("nic-driver: e1000 up") && boot_out.contains("MAC 52:54:00"),
                   "phase1 step2: nic-driver brought the e1000 up (reset + read the MAC)");
            // Networking Phase 1 step 5 (docs/networking.md): nic-driver reached its serve loop - it
            // offers the FRAME INTERFACE (a request/reply where a request payload is a frame to
            // transmit and the reply is the frame that came back). ARP/IP now live in net-stack, not
            // here; nic-driver is pure mechanism (Commandment X). The full TX+RX round-trip is proven
            // end to end by net-stack's ARP resolution below.
            check!(boot_out.contains("nic-driver: serving the frame interface"),
                   "phase1 step5: nic-driver serves the frame interface (mechanism, not protocol)");
        }
        None => {
            // Print what we did receive to help diagnose failures.
            let received = {
                let g = buf.lock().unwrap();
                String::from_utf8_lossy(&g).into_owned()
            };
            println!("shell-test: FAIL - timed out waiting for first gsh>");
            println!("shell-test: received so far:\n{received}");
            child.kill().ok();
            child.wait().ok();
            std::process::exit(1);
        }
    }

    // These land just past the shell prompt (the protocol dance completes as boot finishes), so they
    // are checked here, in the order net-stack now runs them: DHCP first (self-config), then ARP, then
    // ICMP. collect_until advances the cursor, so the check order MUST match the log order.

    // Networking Phase 3 (docs/networking.md): net-stack now SELF-CONFIGURES - it does DHCP FIRST, so
    // the IP it learns is the one ARP + ICMP then use. A DHCP DISCOVER goes out over the frame
    // interface and slirp's OFFER comes back; a pass proves the UDP round-trip both ways, in net-stack.
    let dhcp_ok = collect_until(&buf, &mut cursor, b"net-stack: DHCP - offered", Duration::from_secs(12)).is_some();
    check!(dhcp_ok, "phase3 udp: net-stack got a DHCP offer (UDP; self-configures its IP)");

    // Networking Phase 2 (docs/networking.md): net-stack resolves the gateway (10.0.2.2) by ARP - the
    // proof of the frame interface END TO END. It builds the request, sends it THROUGH nic-driver
    // (request/reply), the gateway answers, nic-driver hands the reply frame back, and net-stack parses
    // the gateway's MAC. A pass means net-stack -> nic-driver -> TX -> reply -> RX -> net-stack, all
    // over the capability-mediated frame interface (ARP is policy in net-stack; the driver is mechanism).
    let arp_ok = collect_until(&buf, &mut cursor, b"net-stack: ARP - 10.0.2.2 is at", Duration::from_secs(12)).is_some();
    check!(arp_ok, "phase2 arp: net-stack resolved the gateway by ARP over the frame interface");

    // Networking Phase 2 step 2 (docs/networking.md): net-stack PINGS the gateway - the networking
    // analogue of v1's ping/pong. It builds an ICMP echo request (ICMP inside IPv4 inside Ethernet) to
    // the MAC ARP resolved, sends it THROUGH nic-driver, and reads back the echo REPLY. A pass proves
    // three protocol layers on the wire, both ways, all in net-stack over the frame interface.
    let icmp_ok = collect_until(&buf, &mut cursor, b"net-stack: ICMP - 10.0.2.2 echo reply", Duration::from_secs(12)).is_some();
    check!(icmp_ok, "phase2 icmp: net-stack pinged the gateway (ICMP echo reply received)");

    // The `net` utility (utilities/40_net.md): the shell queries net-stack BY NAME (it holds
    // ACQUIRE_ANY) and prints its status - the user-facing window onto the whole stack, and a pipe
    // producer. It runs after the boot dance, so net-stack is serving its frozen 15-byte record by now.
    send(&mut write_half, b"net\r");
    let net_out = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(6)).unwrap_or_default();
    check!(net_out.contains("ip       10.0.2.15"), "net: reports the DHCP-learned IP (10.0.2.15)");
    check!(net_out.contains("gateway  10.0.2.2 at 52:55:"), "net: reports the ARP-resolved gateway + MAC");
    check!(net_out.contains("ping     ok"), "net: reports the gateway ping OK");

    // net is a pipe PRODUCER (utilities/40_net.md §4): its three lines flow onward. `net | count`
    // proves it (count is an in-process filter, no disk needed).
    send(&mut write_half, b"net | count\r");
    let netcount = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(6)).unwrap_or_default();
    check!(netcount.contains("3 lines"), "net: is a pipe producer (net | count = 3 lines)");

    // net version (utilities/0_conventions.md rule 5).
    send(&mut write_half, b"net version\r");
    let netver = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(6)).unwrap_or_default();
    check!(netver.contains("net 0.1.0"), "net: version reports net 0.1.0");

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
            println!("shell-test: FAIL - timed out after `help`  [×5]");
            fail += 5;
        }
    }

    // -----------------------------------------------------------------------
    // tab completion of subcommand KEYWORDS (the second token). `observe n<Tab>` → `observe now`;
    // an ambiguous prefix shows the numbered menu (a digit selects), same UX as command/path
    // completion. Ctrl-C (0x03) clears the completed line so nothing executes - we assert on the
    // echoed completion only.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"observe n\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("observe now"), "tab: 'observe n' completes to 'observe now'"),
        None    => { println!("shell-test: FAIL - tab keyword completion timed out"); fail += 1; }
    }
    // The menu reprints the prompt ("gsh> write "), so collect that frame first…
    send(&mut write_half, b"write \t");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("1) append") && r.contains("2) prepend"), "tab: ambiguous 'write ' shows a numbered keyword menu"),
        None    => { println!("shell-test: FAIL - tab keyword menu timed out"); fail += 1; }
    }
    // …then the digit selects (echoes 'write append'); Ctrl-C clears so nothing executes.
    send(&mut write_half, b"1\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("write append"), "tab: menu digit 1 selects 'write append'"),
        None    => { println!("shell-test: FAIL - tab keyword menu selection timed out"); fail += 1; }
    }
    // pipe-stage keyword: a verb after `|` completes its first-arg keyword. `status | sort r` → reverse.
    send(&mut write_half, b"status | sort r\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("sort reverse"), "tab: pipe-stage 'sort r' completes to 'sort reverse'"),
        None    => { println!("shell-test: FAIL - tab pipe-stage keyword timed out"); fail += 1; }
    }
    // command-name completion AFTER a pipe (the segment's first word). `status | so` → `status | sort`.
    send(&mut write_half, b"status | so\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("status | sort"), "tab: command name completes after a pipe (so -> sort)"),
        None    => { println!("shell-test: FAIL - tab pipe command completion timed out"); fail += 1; }
    }
    // trailing modifier keyword (after the path arg). `mkdir /x p` → `mkdir /x parents`.
    send(&mut write_half, b"mkdir /x p\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("mkdir /x parents"), "tab: trailing modifier 'mkdir /x p' -> 'parents'"),
        None    => { println!("shell-test: FAIL - tab trailing modifier timed out"); fail += 1; }
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
            println!("shell-test: FAIL - timed out after `cores`");
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
            println!("shell-test: FAIL - timed out after `status`  [×2]");
            fail += 2;
        }
    }
    // Structured records: status as a typed table → where filter + to json rendering.
    send(&mut write_half, b"status | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("\"state\":"), "status | to json: JSON objects"),
        None    => { println!("shell-test: FAIL - status|to json timeout"); fail += 1; }
    }
    // Compact predicate: where col<op>val (no spaces, no quotes needed).
    send(&mut write_half, b"status | where name=shell\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("shell") && !r.contains("logger"), "status | where name=shell: filters rows"),
        None    => { println!("shell-test: FAIL - status|where timeout"); fail += 1; }
    }
    send(&mut write_half, b"status | where name=shell | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\": \"shell\"") && !r.contains("\"logger\""), "status | where … | to json: filtered JSON"),
        None    => { println!("shell-test: FAIL - status|where|json timeout"); fail += 1; }
    }
    // select: project columns.
    send(&mut write_half, b"status | where name=shell | select name state\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("name") && r.contains("state") && !r.contains("restarts"), "status | select: projects columns"),
        None    => { println!("shell-test: FAIL - status|select timeout"); fail += 1; }
    }
    // to yaml: the other edge rendering.
    send(&mut write_half, b"status | where name=shell | to yaml\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("- slot:") && r.contains("name: shell"), "status | to yaml: YAML mapping list"),
        None    => { println!("shell-test: FAIL - status|to yaml timeout"); fail += 1; }
    }
    // uptime - record producer: bare grid, JSON/YAML rendering, version/help.
    send(&mut write_half, b"uptime\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        // Headers present AND a sane value: within the first hour of this short test boot it must
        // read "0d 00:MM:SS". If boot-time capture had failed (boot=0), now−boot would be ~19000
        // days since 1970 - so "0d 00:" also proves the RTC-delta wall clock is wired correctly.
        Some(r) => check!(r.contains("uptime") && r.contains("seconds") && r.contains("0d 00:"),
                          "uptime: one-row grid (uptime + seconds), sane wall-clock value"),
        None    => { println!("shell-test: FAIL - uptime timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"uptime\":") && r.contains("\"seconds\":"), "uptime | to json: record with uptime + seconds"),
        None    => { println!("shell-test: FAIL - uptime|to json timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | to yaml\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("seconds:") && r.contains("uptime:"), "uptime | to yaml: YAML mapping"),
        None    => { println!("shell-test: FAIL - uptime|to yaml timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime | select seconds | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"seconds\":") && !r.contains("\"uptime\":"), "uptime | select seconds: projects the column"),
        None    => { println!("shell-test: FAIL - uptime|select timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("uptime 0.1.0"), "uptime version: number"),
        None    => { println!("shell-test: FAIL - uptime version timeout"); fail += 1; }
    }
    send(&mut write_half, b"uptime help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("uptime") && r.contains("seconds since boot"), "uptime help: header + example"),
        None    => { println!("shell-test: FAIL - uptime help timeout"); fail += 1; }
    }
    // Phase-0 of moving naming out of the kernel (docs/naming-design.md): the new
    // SpawnReturningEndpoint syscall hands the caller a SEND|GRANT cap to the spawned service's
    // endpoint. `spawncap pong` spawns pong, gets the cap, and sends a probe through it - proving
    // the returned cap actually routes. The old name-wiring path is untouched (purely additive).
    // Use `upper` (also has a recv endpoint) so the later `spawnwired` test can spawn `pong` itself.
    send(&mut write_half, b"spawncap upper\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(6)) {
        Some(r) => check!(r.contains("endpoint cap acquired; send Ok"),
                          "spawncap: SpawnReturningEndpoint returns a routable endpoint cap"),
        None    => { println!("shell-test: FAIL - spawncap timeout"); fail += 1; }
    }
    // Phase 0b (docs/naming-design.md): the kernel wires a child's send-peer from a CALLER-PASSED
    // cap (not a name). `spawnwired` spawns `greet` wired to a fresh `pong` via the SpawnWithCaps
    // syscall; greet sends to send_peer[0] = that cap → pong logs "pong: received". Proves the
    // child uses a cap the kernel installed from the spawner, end to end.
    send(&mut write_half, b"spawnwired\r");
    match collect_until(&buf, &mut cursor, b"pong: received", Duration::from_secs(8)) {
        Some(_) => check!(true,
                          "naming Phase 0b: child uses a caller-passed cap (greet -> pong via SpawnWithCaps)"),
        None    => { println!("shell-test: FAIL - naming Phase 0b: pong did not receive greet's message"); fail += 1; }
    }
    // drain back to the prompt before the next command.
    let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(3));
    // sort by a column (just exercise the path; ordering of the full table is host-dependent).
    send(&mut write_half, b"status | sort name | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"name\":") && r.contains("shell"), "status | sort name: sorts the table"),
        None    => { println!("shell-test: FAIL - status|sort timeout"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // starter-pack: echo / about / mem / caps (self)
    // -----------------------------------------------------------------------
    send(&mut write_half, b"echo PINGPONG42\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("PINGPONG42"), "echo: prints its argument"),
        None    => { println!("shell-test: FAIL - timed out after echo"); fail += 1; }
    }

    send(&mut write_half, b"about\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => {
            check!(r.contains("GodspeedOS"), "about: identity line");
            check!(r.contains("Bankole Ogundero"), "about: creator credit");
        }
        None => { println!("shell-test: FAIL - timed out after about  [×2]"); fail += 2; }
    }

    send(&mut write_half, b"mem\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("mem:") && r.contains("total"), "mem: reports usage"),
        None    => { println!("shell-test: FAIL - timed out after mem"); fail += 1; }
    }

    // date - the RTC clock (QEMU emulates the MC146818 and returns host time).
    // Default form is a full timestamp `Wkd YYYY-MM-DD HH:MM:SS`; `date epoch`
    // prints epoch seconds (digits, no date/time separators).
    send(&mut write_half, b"date\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains('-') && r.contains(':'), "date: full timestamp"),
        None    => { println!("shell-test: FAIL - timed out after date"); fail += 1; }
    }

    send(&mut write_half, b"date epoch\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.chars().any(|c| c.is_ascii_digit()), "date epoch: seconds since 1970"),
        None    => { println!("shell-test: FAIL - timed out after date epoch"); fail += 1; }
    }

    send(&mut write_half, b"caps\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("caps for shell"), "caps (no arg): shows this shell"),
        None    => { println!("shell-test: FAIL - timed out after caps (self)"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // unknown command
    // -----------------------------------------------------------------------
    send(&mut write_half, b"xyzzy\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("unknown: xyzzy"), "unknown command error"),
        None    => { println!("shell-test: FAIL - timed out after unknown command"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Tab completion: single match fills in; multiple → numbered menu, digit selects.
    // -----------------------------------------------------------------------
    // `fc` + Tab → only `fcap` matches → it is filled in. (Ctrl-C clears the line afterward.)
    send(&mut write_half, b"fc\t\x03");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("fcap"), "tab: single match completes (fc → fcap)"),
        None    => { println!("shell-test: FAIL - timed out after tab(fc)"); fail += 1; }
    }
    // `co` + Tab → cores / copy / count → numbered menu; digit `1` selects `cores`; Enter runs it.
    // The menu redraws its own `gsh> ` prompt, so the first collect ends at the menu; a second
    // collect captures the selection + the executed command's output.
    send(&mut write_half, b"co\t1\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(menu) => check!(menu.contains("1) cores") && menu.contains("copy"), "tab: numbered menu lists candidates"),
        None       => { println!("shell-test: FAIL - timed out waiting for tab menu"); fail += 1; }
    }
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(run) => check!(run.contains(&format!("cores: {smp}")), "tab: digit selects + runs the command (1 → cores)"),
        None      => { println!("shell-test: FAIL - timed out after tab selection"); fail += 1; }
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
        None    => { println!("shell-test: FAIL - timed out after to-mismatch pipe"); fail += 1; }
    }
    // A genuine non-producer (an action command) still can't start a pipe. `cd` never runs - the
    // pipe is rejected before stage 1 - so there is no side effect.
    send(&mut write_half, b"cd | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("'cd' cannot start a pipe because it's not a pipe source"),
                          "pipe: non-producer source error"),
        None    => { println!("shell-test: FAIL - timed out after non-producer pipe"); fail += 1; }
    }
    // An ORCHESTRATOR (selfcheck/run) must refuse loudly as a non-producer - NOT run and overflow
    // the stack by nesting captures (the HW shell-crash this guards against). Rejected before it
    // runs, so no drive is touched.
    send(&mut write_half, b"selfcheck | write /x.txt\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("'selfcheck' cannot start a pipe because it's not a pipe source"),
                          "pipe: orchestrator refused as non-producer (no nested-capture crash)"),
        None    => { println!("shell-test: FAIL - timed out after orchestrator pipe"); fail += 1; }
    }
    send(&mut write_half, b"status | result\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("checks a command's outcome, not piped output"),
                          "pipe: result-in-pipe outcome-channel hint"),
        None    => { println!("shell-test: FAIL - timed out after result-in-pipe"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // observe now - the shell brokers a one-shot observe-now service that prints
    // a static metrics frame. Its output is ASYNCHRONOUS (the prompt returns
    // before observe-now is scheduled), so wait on observe's own summary line
    // rather than on gsh>. This also exercises the gated introspection path
    // (observe-now holds the INTROSPECT cap; task_stat/inspect_* succeed).
    // -----------------------------------------------------------------------
    send(&mut write_half, b"observe now\r");
    match collect_until(&buf, &mut cursor, b"system state", Duration::from_secs(15)) {
        // `observe now` (interactive snapshot) no longer prefixes lines with "observe: " - the
        // legend line is the stable marker that the frame printed.
        Some(r) => check!(r.contains("legend"), "observe now: static frame printed"),
        None    => { println!("shell-test: FAIL - timed out waiting for observe now frame"); fail += 1; }
    }
    // The frame should carry the task table header (gated task_stat working).
    // Wait on RESTARTS (end of the header) so the chunk includes TASK + NAME.
    match collect_until(&buf, &mut cursor, b"RESTARTS", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("TASK") && r.contains("NAME"), "observe now: task table header"),
        None    => { println!("shell-test: FAIL - observe now: no task table"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // caps <service> - list a service's held capabilities (introspection path).
    // The shell holds the INTROSPECT cap, so it can read its own caps; introspect
    // itself must appear in the list.
    // -----------------------------------------------------------------------
    // The observe-now step stopped reading at the table header, so its trailing
    // `gsh>` prompt is still in the stream - absorb it before issuing caps.
    let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5));

    // observe now as a record producer (docs/records.md): the only PIPEABLE form (bare
    // `observe` is the live loop). Carries the `ticks` (cumulative cpu-time) column status omits.
    send(&mut write_half, b"observe now | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":"),
                          "observe record: now | to json carries the ticks column"),
        None    => { println!("shell-test: FAIL - observe now|to json timeout"); fail += 1; }
    }
    send(&mut write_half, b"observe now | select name ticks | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("\"ticks\":") && r.contains("\"name\":") && !r.contains("\"core\":"),
                          "observe record: select name ticks projects the metric columns"),
        None    => { println!("shell-test: FAIL - observe now|select timeout"); fail += 1; }
    }
    // The live loop must REFUSE to be piped (it owns the screen and never yields a stream),
    // loudly - not hang the shell waiting on a recv that never comes.
    send(&mut write_half, b"observe | sort reverse ticks\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("live view can't be piped"),
                          "observe record: bare live observe refuses to be piped (loud)"),
        None    => { println!("shell-test: FAIL - observe pipe-refusal timeout"); fail += 1; }
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
        None => { println!("shell-test: FAIL - timed out after caps"); fail += 3; }
    }

    // -----------------------------------------------------------------------
    // Least privilege (H10) - a non-spawning service must NOT hold the spawn cap.
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
        None => { println!("shell-test: FAIL - timed out after caps logger"); fail += 2; }
    }

    // caps as a record producer (docs/records.md): piped, it emits resource/rights rows.
    send(&mut write_half, b"caps | to json\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("\"resource\":") && r.contains("\"rights\":"),
                          "caps record: to json renders resource/rights"),
        None    => { println!("shell-test: FAIL - caps|to json timeout"); fail += 1; }
    }
    // where on the resource column: the shell holds the spawn cap, so this keeps a row.
    send(&mut write_half, b"caps | where resource=spawn\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("spawn") && r.contains("resource"),
                          "caps record: where resource=spawn keeps the spawn cap"),
        None    => { println!("shell-test: FAIL - caps|where timeout"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // Singleton guard - spawning an already-live service (here the trusted-root
    // supervisor) must be refused, so the shell can't create a duplicate TCB.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"spawn supervisor\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("supervisor") && r.contains("protected"),
                          "spawn: trusted-root refused with reason"),
        None    => { println!("shell-test: FAIL - timed out after spawn supervisor"); fail += 1; }
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
        None => { println!("shell-test: FAIL - timed out after `write help`  [×2]"); fail += 2; }
    }
    send(&mut write_half, b"ls version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("ls 0.1.0") && r.contains("Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors"), "ls version: number + creator credit"),
        None    => { println!("shell-test: FAIL - timed out after `ls version`"); fail += 1; }
    }
    send(&mut write_half, b"drives flash help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("drives flash") && r.contains("drives flash 0 data"), "subcommand help: drives flash help + example"),
        None    => { println!("shell-test: FAIL - timed out after `drives flash help`"); fail += 1; }
    }
    // Record-pipe verbs self-document too (utilities/31_records.md): they are pipe-only
    // stages, but `<verb> help` / `<verb> version` still resolve via the UTILS intercept.
    send(&mut write_half, b"where help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("where 0.1.0") && r.contains("status | where mem>0"), "where help: header + real example"),
        None    => { println!("shell-test: FAIL - timed out after `where help`"); fail += 1; }
    }
    send(&mut write_half, b"to help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("to 0.1.0") && r.contains("to json") && r.contains("to yaml"), "to help: header + json/yaml rows"),
        None    => { println!("shell-test: FAIL - timed out after `to help`"); fail += 1; }
    }
    send(&mut write_half, b"from version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("from 0.1.0") && r.contains("Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors"), "from version: number + creator credit"),
        None    => { println!("shell-test: FAIL - timed out after `from version`"); fail += 1; }
    }
    send(&mut write_half, b"select help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("select 0.1.0") && r.contains("status | select name core state"), "select help: header + real example"),
        None    => { println!("shell-test: FAIL - timed out after `select help`"); fail += 1; }
    }
    // The top-level `help` command itself conforms now (0_conventions.md §3, last open item):
    // its categorised list carries the version header (rule 6), and `help help` / `help version`
    // resolve like any other utility.
    send(&mut write_half, b"help version\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("help 0.1.0") && r.contains("Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors"), "help version: number + creator credit"),
        None    => { println!("shell-test: FAIL - timed out after `help version`"); fail += 1; }
    }
    send(&mut write_half, b"help help\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains("help 0.1.0") && r.contains("<command> help"), "help help: header + per-command hint"),
        None    => { println!("shell-test: FAIL - timed out after `help help`"); fail += 1; }
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
        None    => { println!("shell-test: FAIL - timed out after up-arrow history"); fail += 1; }
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
        None    => { println!("shell-test: FAIL - timed out after left+insert edit"); fail += 1; }
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
        None    => { println!("shell-test: FAIL - timed out after home/delete edit"); fail += 1; }
    }
    // Bare ESC clears the line: type "garbage", press ESC (no following byte → bare ESC),
    // then "cores" + Enter. If ESC cleared, the command is just "cores"; if it didn't, it
    // would be "garbagecores" → unknown. Output "cores: N" proves the clear.
    send(&mut write_half, b"garbage\x1b");
    std::thread::sleep(Duration::from_millis(400)); // let the bare-ESC wait elapse before more bytes
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")) && !r.contains("unknown"), "bare ESC clears the line"),
        None    => { println!("shell-test: FAIL - timed out after bare-ESC clear"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // chaos kill-storm: the bounded resilience exerciser. Kill `block-driver` 5 times; the supervisor
    // must respawn it each round. A pass proves: recovery held every round, AND the kernel never
    // panicked (a panic reboots; reaching the verdict + the prompt proves it didn't). block-driver
    // holds no disk state, so this runs cleanly on the bare-metal build.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos kill-storm block-driver 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("recovered: 5/5") && r.contains("verdict: PASS"), "chaos: kill-storm block-driver - 5/5 recovered, PASS");
            check!(r.contains("recovered gen"), "chaos: report has per-round detail");
            check!(r.contains("kernel: alive"), "chaos: kill-storm - kernel alive (no panic)");
        }
        None => { println!("shell-test: FAIL - chaos kill-storm timed out (recovery stuck / panic?)"); fail += 3; }
    }
    // The shell is still responsive after the storm (block-driver recovered, the prompt works).
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell still responsive after the storm"),
        None    => { println!("shell-test: FAIL - shell unresponsive after chaos"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // chaos flood-storm = the DRAIN resilience axis, and it only applies to DRAIN-style services (logger +
    // the drivers' idle paths), pinned below. It does NOT apply to fs/shell, which are REPLY-style (recv ->
    // do work -> reply): flooding one with junk makes it try to PROCESS the junk - fs does a block read and
    // blocks on the no-disk block-driver (which can never reply) - so it clogs by design, not by bug. That is
    // exactly why the chaos SWEEP kills reply-style services instead of flooding them. (The old `flood-storm
    // fs` step here only "passed" because the flood-storm verdict used to count QueueFull as "drained" - a
    // real test bug, fixed in this change.) fs's resilience is pinned by kill-storm + `osdev test fs-restart`.
    // FOLLOW-UP (noted, not done): fs could reject a malformed/too-short request early + reply an error
    // instead of blocking on block-driver - a defense-in-depth hardening for a hostile client.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell still responsive after the kill/mem-pressure storms"),
        None    => { println!("shell-test: FAIL - shell unresponsive after chaos storms"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // FLOOD-ENDPOINT DRAIN regression. The disease: a registered service that idles without recv'ing lets a
    // flood (or any stray send) fill its 16-deep queue and sit at 16/16 FOREVER. It recurred in logger (a
    // park stub) and the USB drivers' idle paths - including xhci's NO-CONTROLLER idle, which the original
    // sweep missed and this very pin caught. In QEMU there is no USB controller, so xhci/ehci sit in exactly
    // that no-controller idle path. Each must DRAIN under flood and survive; a regression here means a
    // non-draining idle loop came back.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos flood-storm logger 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("flood-storm logger") && r.contains("verdict: PASS"), "chaos: flood-storm logger - idle endpoint drains, PASS (was a park stub)");
            check!(r.contains("survived: 5/5"), "chaos: flood-storm logger - survived all 5 (drained, not clogged)");
        }
        None => { println!("shell-test: FAIL - chaos flood-storm logger timed out (endpoint clogged / panic?)"); fail += 2; }
    }
    send(&mut write_half, b"chaos flood-storm xhci 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("flood-storm xhci") && r.contains("verdict: PASS"), "chaos: flood-storm xhci - no-controller idle drains, PASS (the gap the sweep missed)");
            check!(r.contains("survived: 5/5"), "chaos: flood-storm xhci - survived all 5 (drained, not clogged)");
        }
        None => { println!("shell-test: FAIL - chaos flood-storm xhci timed out (endpoint clogged / panic?)"); fail += 2; }
    }
    send(&mut write_half, b"chaos flood-storm ehci 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("flood-storm ehci") && r.contains("verdict: PASS"), "chaos: flood-storm ehci - no-controller idle drains, PASS");
            check!(r.contains("survived: 5/5"), "chaos: flood-storm ehci - survived all 5 (drained, not clogged)");
        }
        None => { println!("shell-test: FAIL - chaos flood-storm ehci timed out (endpoint clogged / panic?)"); fail += 2; }
    }
    // block-driver's no-AHCI idle path (no SATA controller in this QEMU - `if=ide` disk, default pc machine)
    // was the SAME bare `loop { yield_cpu() }` as xhci's idle() - a third instance of the disease the sweep
    // missed. It must drain under flood too.
    send(&mut write_half, b"chaos flood-storm block-driver 5\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            check!(r.contains("flood-storm block-driver") && r.contains("verdict: PASS"), "chaos: flood-storm block-driver - no-AHCI idle drains, PASS (the gap the sweep missed)");
            check!(r.contains("survived: 5/5"), "chaos: flood-storm block-driver - survived all 5 (drained, not clogged)");
        }
        None => { println!("shell-test: FAIL - chaos flood-storm block-driver timed out (endpoint clogged / panic?)"); fail += 2; }
    }

    // -----------------------------------------------------------------------
    // chaos mem-pressure: on-device memory pressure (§22 S7). Each round spawns the mem-pressure (allocs
    // 4 MiB chunks to its 32 MiB limit, then AllocDenied), watches free frames drop, kills it, and
    // confirms the frames return to baseline (v1 reclaims at death). PASS = every round allocated +
    // reclaimed; the hog must NOT report an Ok-after-Denied accounting bug.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos mem-pressure 3\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(90)) {
        Some(r) => {
            check!(r.contains("chaos mem-pressure:") && r.contains("verdict: PASS"), "chaos: mem-pressure - alloc-to-limit + reclaim, PASS");
            check!(r.contains("clean cycles") && r.contains("3/3"), "chaos: mem-pressure - 3/3 clean cycles (no leak)");
            check!(!r.contains("mem-pressure: FAIL"), "chaos: mem-pressure - no Ok-after-AllocDenied accounting bug");
            check!(r.contains("kernel: alive"), "chaos: mem-pressure - kernel alive (no panic)");
        }
        None => { println!("shell-test: FAIL - chaos mem-pressure timed out (alloc/reclaim stuck / panic?)"); fail += 4; }
    }
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell still responsive after mem-pressure"),
        None    => { println!("shell-test: FAIL - shell unresponsive after mem-pressure"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // chaos kill-storm SUPERVISOR (Path C / Phase 6): the supervisor is restartable too - the KERNEL
    // respawns it on every death, unconditionally (no bound - a bound would be a reboot/DoS vector).
    // Storming it 4× and recovering every round proves the unkillable set is now {kernel} alone: the
    // shell (a separate task) survives killing its own spawner, and the prompt still answers after.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos kill-storm supervisor 4\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(45)) {
        Some(r) => {
            check!(r.contains("recovered: 4/4") && r.contains("verdict: PASS"), "chaos: kill-storm supervisor - 4/4 recovered, PASS");
            check!(r.contains("kernel-respawned"), "chaos: supervisor target reported as kernel-respawned");
            check!(r.contains("kernel: alive"), "chaos: kill-storm supervisor - kernel alive (no panic, no bound)");
        }
        None => { println!("shell-test: FAIL - chaos kill-storm supervisor timed out (recovery stuck / panic?)"); fail += 3; }
    }
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell responsive after storming the supervisor"),
        None    => { println!("shell-test: FAIL - shell unresponsive after supervisor chaos"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // chaos max-carnage: the chaos monkey - kill a RANDOM live service each round (everything but the
    // shell). The headline invariant is that the KERNEL SURVIVES arbitrary random carnage: the command
    // returning + reporting at all proves no panic (a panic reboots). Individual non-recoverable
    // victims staying dead is expected, so the verdict is about kernel survival, not per-service.
    // -----------------------------------------------------------------------
    // `chaos max-carnage N` now launches the chaos SERVICE: it takes the console foreground, runs N
    // rounds (kill / flood / kill-then-flood a random live service - the SHELL included now), prints its
    // report, hands the keyboard back, and self-terminates. The shell is muted during the run, but chaos
    // writes its report to the console either way, so we collect until its done-marker. (The launching
    // shell draws a `gsh>` right after the command, before chaos claims, so `gsh>` is NOT a usable
    // terminator here.)
    send(&mut write_half, b"chaos max-carnage all-services 5\r");
    // max-carnage shows a loud serial-required warning and waits for a y/N confirm (a bare Enter
    // cancels). Sync on the prompt, then type 'y' + Enter to proceed.
    let _ = collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(15));
    send(&mut write_half, b"y\r");
    match collect_until(&buf, &mut cursor, b"foreground returned to the shell", Duration::from_secs(120)) {
        Some(r) => {
            check!(r.contains("chaos max-carnage:"), "chaos: max-carnage launched the chaos service (foreground TUI)");
            check!(r.contains("report") && r.contains("kills") && r.contains("flooded"), "chaos: max-carnage report (per-service kills + floods)");
            check!(r.contains("total:") && r.contains("rounds"), "chaos: max-carnage ran a bounded round count + self-terminated");
            check!(r.contains("kernel: alive"), "chaos: max-carnage - kernel survived the kill+flood carnage (shell included)");
        }
        None => { println!("shell-test: FAIL - chaos max-carnage (service) timed out (wedged / foreground stuck?)"); fail += 4; }
    }
    // After chaos, the (possibly respawned) shell prints a startup banner and/or a regain prompt, so the
    // first `gsh>` we hit can precede the `cores` response. Drain prompts until the response appears -
    // this also realigns the cursor for the spawn-storm checks below.
    send(&mut write_half, b"cores\r");
    let mut responsive = false;
    for _ in 0..6 {
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
            Some(r) => { if r.contains(&format!("cores: {smp}")) { responsive = true; break; } }
            None    => break,
        }
    }
    check!(responsive, "chaos: shell responsive after max-carnage");

    // -----------------------------------------------------------------------
    // chaos spawn-storm: the global-ceiling test. Spawn mem-pressure tasks until the task-pool/memory ceiling
    // REFUSES a spawn (loud Err, no panic), then kill them all + confirm full reclaim. With 512 MiB of
    // RAM, ~15 hogs fill memory and footprint exhaustion refuses a spawn well before 30, so the ceiling
    // is reliably hit. The headline invariant is no panic at the ceiling + the swarm fully reclaims.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"chaos spawn-storm 30\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(120)) {
        Some(r) => {
            check!(r.contains("chaos spawn-storm:") && r.contains("verdict: PASS"), "chaos: spawn-storm - ceiling held + full reclaim, PASS");
            check!(r.contains("ceiling: HIT"), "chaos: spawn-storm - the kernel REFUSED a spawn at the ceiling (loud, no panic)");
            check!(r.contains("kernel: alive"), "chaos: spawn-storm - kernel alive (no panic under the swarm)");
            check!(r.contains("hogs left 0"), "chaos: spawn-storm - every hog reclaimed (no leak at scale)");
        }
        None => { println!("shell-test: FAIL - chaos spawn-storm timed out (ceiling panic / reclaim stuck?)"); fail += 4; }
    }
    send(&mut write_half, b"cores\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
        Some(r) => check!(r.contains(&format!("cores: {smp}")), "chaos: shell responsive after spawn-storm"),
        None    => { println!("shell-test: FAIL - shell unresponsive after spawn-storm"); fail += 1; }
    }

    // -----------------------------------------------------------------------
    // `kill shell` via the COMMAND (the interactive COM1 path the user types): the shell self-kills
    // and the supervisor respawns a FRESH prompt. This is the same kernel self-kill path a page fault
    // uses (deferred stack/PML4 reclaim), so the dead instance never corrupts anything; the new shell
    // must answer input afterward. Proves a user can `kill shell` and get the session back.
    // -----------------------------------------------------------------------
    send(&mut write_half, b"kill shell\r");
    match collect_until(&buf, &mut cursor, b"shell: ready", Duration::from_secs(20)) {
        Some(_) => check!(true, "kill shell (self-kill via command) - supervisor respawned a fresh prompt"),
        None    => { println!("shell-test: FAIL - kill shell did not respawn a fresh prompt"); fail += 1; }
    }
    let mut answered = false;
    for _ in 0..4 {
        send(&mut write_half, b"cores\r");
        if let Some(r) = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(5)) {
            if r.contains("cores:") { answered = true; break; }
        }
    }
    check!(answered, "the respawned shell answers commands (session recovered after `kill shell`)");

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
/// over IPC from a user command, and lists/relabels it - all with no reboot.
pub fn run_drives(image_path: &Path, persist_path: &str, smp: u32) {
    println!("drives-test: booting (smp={smp}) with a RAW AHCI disk - scripted mode");

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
            if $ok { println!("drives-test: PASS - {}", $label); pass += 1; }
            else   { println!("drives-test: FAIL - {}", $label); fail += 1; }
        };
    }

    // Boot complete.
    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        let got = { String::from_utf8_lossy(&buf.lock().unwrap()).into_owned() };
        println!("drives-test: FAIL - timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // 1. `drives` - a raw, unformatted disk.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("raw") && r.contains("not formatted"), "drives: raw disk listed"),
        None    => { println!("drives-test: FAIL - timed out after `drives`"); fail += 1; }
    }

    // 2. `drives flash data` - confirm the [y/N], then format.
    send(&mut write_half, b"drives flash data\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            check!(true, "flash: destructive [y/N] confirm shown");
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
                Some(r) => check!(r.contains("formatted as GSFS"), "flash: formatted over IPC"),
                None    => { println!("drives-test: FAIL - timed out after confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL - no [y/N] confirm  [×2]"); fail += 2; }
    }

    // 3. `drives` - now a mounted GSFS labelled 'data'.
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => {
            check!(r.contains("GSFS"), "drives: now formatted GSFS");
            check!(r.contains("data"), "drives: label 'data' shown");
        }
        None => { println!("drives-test: FAIL - timed out after `drives` (2)  [×2]"); fail += 2; }
    }

    // 4. `drives label archive` - rename, then confirm it stuck.
    send(&mut write_half, b"drives label archive\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("labelled 'archive'"), "label: rename acknowledged"),
        None    => { println!("drives-test: FAIL - timed out after `drives label`"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("archive"), "label: new label 'archive' listed"),
        None    => { println!("drives-test: FAIL - timed out after `drives` (3)"); fail += 1; }
    }

    // 5. `drives reset` - un-format back to raw (confirm [y/N]), then list shows raw.
    send(&mut write_half, b"drives reset\r");
    match collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)) {
        Some(_) => {
            send(&mut write_half, b"y\r");
            match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(15)) {
                Some(r) => check!(r.contains("reset to raw"), "reset: un-formatted to raw"),
                None    => { println!("drives-test: FAIL - timed out after reset confirm"); fail += 1; }
            }
        }
        None => { println!("drives-test: FAIL - reset: no [y/N] confirm"); fail += 1; }
    }
    send(&mut write_half, b"drives\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
        Some(r) => check!(r.contains("raw") && r.contains("not formatted"), "reset: drive now raw"),
        None    => { println!("drives-test: FAIL - timed out after `drives` (4)"); fail += 1; }
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
    println!("files-test: booting (smp={smp}) with a RAW AHCI disk - scripted mode");

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
            if $ok { println!("files-test: PASS - {}", $label); pass += 1; }
            else   { println!("files-test: FAIL - {}", $label); fail += 1; }
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
        println!("files-test: FAIL - timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // Format the disk first (file commands need a filesystem).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
            Some(r) => check!(r.contains("formatted as GSFS"), "setup: flashed GSFS"),
            None    => { println!("files-test: FAIL - flash timeout"); fail += 1; }
        }
    } else { println!("files-test: FAIL - no flash confirm"); fail += 1; }

    // mkdir + write + ls + read (absolute paths).
    match run!(b"mkdir /docs\r", 10) {
        Some(r) => check!(r.contains("created /docs"), "mkdir /docs"),
        None    => { println!("files-test: FAIL - mkdir timeout"); fail += 1; }
    }
    match run!(b"write /docs/note.txt hello world\r", 10) {
        Some(r) => check!(r.contains("wrote /docs/note.txt"), "write /docs/note.txt"),
        None    => { println!("files-test: FAIL - write timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("note.txt") && r.contains("file"), "ls /docs shows note.txt"),
        None    => { println!("files-test: FAIL - ls timeout"); fail += 1; }
    }
    match run!(b"read /docs/note.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "read /docs/note.txt"),
        None    => { println!("files-test: FAIL - read timeout"); fail += 1; }
    }

    // cd + relative path + `..`.
    match run!(b"cd /docs\r", 10) {
        Some(r) => check!(r.contains("/docs"), "cd /docs"),
        None    => { println!("files-test: FAIL - cd timeout"); fail += 1; }
    }
    match run!(b"write inside.txt nested-content\r", 10) {
        Some(r) => check!(r.contains("wrote /docs/inside.txt"), "write relative → /docs/inside.txt"),
        None    => { println!("files-test: FAIL - relative write timeout"); fail += 1; }
    }
    match run!(b"ls\r", 10) {
        Some(r) => check!(r.contains("note.txt") && r.contains("inside.txt"), "ls (cwd) shows both files"),
        None    => { println!("files-test: FAIL - ls cwd timeout"); fail += 1; }
    }
    // Tab-completion of a FILE PATH: a unique prefix fills in the rest, and the completed command
    // runs. /docs has note.txt + inside.txt, so 'i' and 'n' are unique. \t = Tab, then \r runs it.
    // Absolute path: `read /docs/i<Tab>` → `read /docs/inside.txt ` → runs → inside.txt's content.
    match run!(b"read /docs/i\t\r", 10) {
        Some(r) => check!(r.contains("nested-content"), "tab: /docs/i<Tab> completes to inside.txt + runs"),
        None    => { println!("files-test: FAIL - tab abs-path timeout"); fail += 1; }
    }
    // Relative path (cwd is /docs): `read n<Tab>` → `read note.txt ` → runs → note.txt's content.
    match run!(b"read n\t\r", 10) {
        Some(r) => check!(r.contains("hello world"), "tab: relative n<Tab> completes to note.txt + runs"),
        None    => { println!("files-test: FAIL - tab rel-path timeout"); fail += 1; }
    }
    match run!(b"mkdir sub\r", 10) {
        Some(r) => check!(r.contains("created /docs/sub"), "mkdir relative → /docs/sub"),
        None    => { println!("files-test: FAIL - mkdir relative timeout"); fail += 1; }
    }
    match run!(b"cd ..\r", 10) {
        Some(r) => check!(r.contains('/') && !r.contains("/docs"), "cd .. → root"),
        None    => { println!("files-test: FAIL - cd .. timeout"); fail += 1; }
    }
    match run!(b"read /docs/inside.txt\r", 10) {
        Some(r) => check!(r.contains("nested-content"), "read absolute after cd .."),
        None    => { println!("files-test: FAIL - final read timeout"); fail += 1; }
    }

    // copy + rename.
    match run!(b"copy /docs/note.txt /docs/note-copy.txt\r", 10) {
        Some(r) => check!(r.contains("copied"), "copy /docs/note.txt → note-copy.txt"),
        None    => { println!("files-test: FAIL - copy timeout"); fail += 1; }
    }
    match run!(b"read /docs/note-copy.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "read copy has same content"),
        None    => { println!("files-test: FAIL - read copy timeout"); fail += 1; }
    }
    match run!(b"rename /docs/note-copy.txt renamed.txt\r", 10) {
        Some(r) => check!(r.contains("renamed"), "rename note-copy.txt → renamed.txt"),
        None    => { println!("files-test: FAIL - rename timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("renamed.txt") && !r.contains("note-copy.txt"), "ls shows renamed, not old name"),
        None    => { println!("files-test: FAIL - ls after rename timeout"); fail += 1; }
    }

    // delete (GSFS0003: frees blocks, reclaims) - file then re-list shows it gone.
    match run!(b"delete /docs/renamed.txt\r", 10) {
        Some(r) => check!(r.contains("deleted"), "delete /docs/renamed.txt"),
        None    => { println!("files-test: FAIL - delete timeout"); fail += 1; }
    }
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(!r.contains("renamed.txt"), "ls: deleted file is gone"),
        None    => { println!("files-test: FAIL - ls after delete timeout"); fail += 1; }
    }

    // move (relink) - into the /docs/sub directory created earlier.
    match run!(b"move /docs/note.txt /docs/sub/note.txt\r", 10) {
        Some(r) => check!(r.contains("moved"), "move /docs/note.txt → /docs/sub/note.txt"),
        None    => { println!("files-test: FAIL - move timeout"); fail += 1; }
    }
    match run!(b"ls /docs/sub\r", 10) {
        Some(r) => check!(r.contains("note.txt"), "ls /docs/sub shows moved file"),
        None    => { println!("files-test: FAIL - ls sub timeout"); fail += 1; }
    }
    match run!(b"read /docs/sub/note.txt\r", 10) {
        Some(r) => check!(r.contains("hello world"), "moved file keeps its content"),
        None    => { println!("files-test: FAIL - read moved timeout"); fail += 1; }
    }

    // Directory growth: write 10 files into one directory (a dir block holds 8 entries),
    // forcing the directory to grow a second block - proving there's no per-dir cap.
    let _ = run!(b"mkdir /big\r", 10);
    for i in 1..=10 {
        let cmd = format!("write /big/f{} x\r", i);
        let _ = run!(cmd.as_bytes(), 10);
    }
    match run!(b"ls /big\r", 10) {
        Some(r) => {
            let n = (1..=10).filter(|i| r.contains(&format!("f{}", i))).count();
            check!(n == 10, "directory grew past 8 entries (no per-dir cap) - 10 files listed");
        }
        None => { println!("files-test: FAIL - ls /big timeout"); fail += 1; }
    }

    // find - whole-filesystem tree walk from root. Tree now: /docs/{inside.txt, sub/note.txt},
    // /big/{f1..f10}.
    match run!(b"find inside.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt") && r.contains("find: 1 match"), "find: locates /docs/inside.txt"),
        None    => { println!("files-test: FAIL - find timeout"); fail += 1; }
    }
    match run!(b"find note.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/sub/note.txt") && r.contains("find: 1 match"), "find: descends into subdir (/docs/sub/note.txt)"),
        None    => { println!("files-test: FAIL - find sub timeout"); fail += 1; }
    }
    match run!(b"find f5\r", 10) {
        Some(r) => check!(r.contains("/big/f5") && r.contains("find: 1 match"), "find: locates a file in a grown directory (/big/f5)"),
        None    => { println!("files-test: FAIL - find grown timeout"); fail += 1; }
    }
    match run!(b"find nope.txt\r", 10) {
        Some(r) => check!(r.contains("find: 0 match"), "find: reports 0 matches for a missing name"),
        None    => { println!("files-test: FAIL - find missing timeout"); fail += 1; }
    }
    // find substring: `.txt` matches inside.txt AND sub/note.txt (2). Exact match would be 0.
    match run!(b"find .txt\r", 10) {
        Some(r) => check!(r.contains("find: 2 match"), "find: substring match (.txt → 2 files)"),
        None    => { println!("files-test: FAIL - find substring timeout"); fail += 1; }
    }
    // find glob: `*.txt` is anchored - matches names ENDING in .txt (inside.txt, note.txt = 2),
    // unlike the substring form which would also match a "txt" anywhere.
    match run!(b"find *.txt\r", 10) {
        Some(r) => check!(r.contains("find: 2 match"), "find: glob '*.txt' (anchored, 2 files)"),
        None    => { println!("files-test: FAIL - find glob *.txt timeout"); fail += 1; }
    }
    // find glob `?`: f? matches the 2-char names f1..f9 (9) but NOT f10 (3 chars) - proves
    // single-char `?` and whole-name anchoring. Substring 'f?' would match nothing literally.
    match run!(b"find f?\r", 10) {
        Some(r) => check!(r.contains("find: 9 match"), "find: glob 'f?' (9 of f1..f10, not f10)"),
        None    => { println!("files-test: FAIL - find glob f? timeout"); fail += 1; }
    }
    // glob is anchored both ends: `inside.*` matches inside.txt (1), not a mere substring.
    match run!(b"find inside.*\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt") && r.contains("find: 1 match"), "find: glob 'inside.*' (1 file)"),
        None    => { println!("files-test: FAIL - find glob inside.* timeout"); fail += 1; }
    }

    // ls shows file sizes: /docs/inside.txt holds "nested-content" = 14 bytes.
    match run!(b"ls /docs\r", 10) {
        Some(r) => check!(r.contains("inside.txt") && r.contains("14 B"), "ls: shows file size (inside.txt 14 B)"),
        None    => { println!("files-test: FAIL - ls size timeout"); fail += 1; }
    }

    // mkdir parents: create a 3-deep chain in one call (none of /x, /x/y exist yet).
    match run!(b"mkdir /x/y/z parents\r", 10) {
        Some(r) => check!(r.contains("created /x/y/z"), "mkdir parents: created /x/y/z chain"),
        None    => { println!("files-test: FAIL - mkdir parents timeout"); fail += 1; }
    }
    match run!(b"ls /x/y\r", 10) {
        Some(r) => check!(r.contains("z") && r.contains("dir"), "mkdir parents: /x/y/z exists as a dir"),
        None    => { println!("files-test: FAIL - ls /x/y timeout"); fail += 1; }
    }
    // plain mkdir into a missing parent still fails (parents is opt-in).
    match run!(b"mkdir /no/such/dir\r", 10) {
        Some(r) => check!(r.contains("mkdir: failed"), "mkdir (no parents): fails on missing parent"),
        None    => { println!("files-test: FAIL - mkdir strict timeout"); fail += 1; }
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
        None => { println!("files-test: FAIL - tree timeout"); fail += 1; }
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
        None    => { println!("files-test: FAIL - copy recursive timeout"); fail += 1; }
    }
    match run!(b"read /orchard/branch/leaf2.txt\r", 10) {
        Some(r) => check!(r.contains("cherry"), "copy recursive: nested file copied with content"),
        None    => { println!("files-test: FAIL - read deep copy timeout"); fail += 1; }
    }
    match run!(b"read /orchard/leaf1.txt\r", 10) {
        Some(r) => check!(r.contains("apple"), "copy recursive: top-level file copied"),
        None    => { println!("files-test: FAIL - read shallow copy timeout"); fail += 1; }
    }
    // Guard: copying a directory into its own subtree is refused (would never terminate).
    match run!(b"copy /grove /grove/inner recursive\r", 10) {
        Some(r) => check!(r.contains("cannot copy into itself"), "copy recursive: refuses copy into own subtree"),
        None    => { println!("files-test: FAIL - copy-into-self timeout"); fail += 1; }
    }
    // Plain delete still refuses a non-empty directory (recursive is opt-in).
    match run!(b"delete /grove\r", 10) {
        Some(r) => check!(r.contains("delete: failed"), "delete (non-recursive): refuses non-empty dir"),
        None    => { println!("files-test: FAIL - delete non-empty timeout"); fail += 1; }
    }
    // delete recursive: removes the whole source subtree.
    match run!(b"delete /grove recursive\r", 12) {
        Some(r) => check!(r.contains("deleted (recursive)"), "delete recursive: /grove subtree removed"),
        None    => { println!("files-test: FAIL - delete recursive timeout"); fail += 1; }
    }
    match run!(b"ls /\r", 10) {
        Some(r) => check!(!r.contains("grove") && r.contains("orchard"),
                          "delete recursive: /grove gone, /orchard (the copy) survives"),
        None    => { println!("files-test: FAIL - ls after recursive delete timeout"); fail += 1; }
    }
    // The copy is independent - its nested file is intact after the source was deleted.
    match run!(b"read /orchard/branch/leaf2.txt\r", 10) {
        Some(r) => check!(r.contains("cherry"), "copy is independent of the deleted source"),
        None    => { println!("files-test: FAIL - read copy after delete timeout"); fail += 1; }
    }

    // ── write append: add to a file, and create-on-append ──────────────────────────
    let _ = run!(b"write /applog AAA\r", 10);
    let _ = run!(b"write append /applog BBB\r", 10);
    match run!(b"read /applog\r", 10) {
        Some(r) => check!(r.contains("AAABBB"), "write append: appends without overwriting"),
        None    => { println!("files-test: FAIL - read appended timeout"); fail += 1; }
    }
    // append to a missing file creates it.
    let _ = run!(b"write append /freshlog ZZZ\r", 10);
    match run!(b"read /freshlog\r", 10) {
        Some(r) => check!(r.contains("ZZZ"), "write append: creates the file when missing"),
        None    => { println!("files-test: FAIL - read created-by-append timeout"); fail += 1; }
    }
    // a path literally starting with "append" is still a path, not the keyword.
    match run!(b"write appendix.txt hi\r", 10) {
        Some(r) => check!(r.contains("wrote") && r.contains("appendix.txt"), "write: 'appendix.txt' is a path, not the append keyword"),
        None    => { println!("files-test: FAIL - appendix path timeout"); fail += 1; }
    }

    // ── cd - : toggle to the previous directory ────────────────────────────────────
    let _ = run!(b"cd /docs\r", 10);
    let _ = run!(b"cd /big\r", 10);
    match run!(b"cd -\r", 10) {
        Some(r) => check!(r.contains("/docs"), "cd -: returns to the previous directory"),
        None    => { println!("files-test: FAIL - cd - timeout"); fail += 1; }
    }
    match run!(b"cd -\r", 10) {
        Some(r) => check!(r.contains("/big"), "cd -: toggles back to where we just were"),
        None    => { println!("files-test: FAIL - cd - toggle timeout"); fail += 1; }
    }
    let _ = run!(b"cd /\r", 10);

    // ── pipes: built-ins and services compose, both directions (Appendix D) ─────────
    // builtin producer → write-file sink: capture echo's output to a file.
    let _ = run!(b"echo piped-to-file | write /pipe1.txt\r", 10);
    match run!(b"read /pipe1.txt\r", 10) {
        Some(r) => check!(r.contains("piped-to-file"), "pipe: builtin | write file (echo → file)"),
        None    => { println!("files-test: FAIL - pipe echo|write timeout"); fail += 1; }
    }
    // builtin producer (find, glob) → write-file sink: capture a listing to a file.
    match run!(b"find *.txt /docs | write /found.txt\r", 12) {
        Some(r) => check!(r.contains("piped") && r.contains("found.txt"), "pipe: find | write file (wired)"),
        None    => { println!("files-test: FAIL - pipe find|write timeout"); fail += 1; }
    }
    match run!(b"read /found.txt\r", 10) {
        Some(r) => check!(r.contains("/docs/inside.txt"), "pipe: find | write captured the matches"),
        None    => { println!("files-test: FAIL - read found.txt timeout"); fail += 1; }
    }
    // builtin producer → service filter (terminal): echo's text through `upper`, whose
    // uppercased output the shell prints to the console (the pipe's final result).
    match run!(b"echo hello pipes | upper\r", 14) {
        Some(r) => check!(r.contains("HELLO PIPES"), "pipe: builtin | service (echo → upper → HELLO PIPES)"),
        None    => { println!("files-test: FAIL - pipe echo|upper timeout"); fail += 1; }
    }
    // service producer → write-file sink: capture `greet`'s output to a file. The shell is the
    // sink: it drains greet's stream (EOT marker = end) and writes it.
    match run!(b"greet | write /greetout.txt\r", 14) {
        Some(r) => check!(r.contains("piped") && r.contains("greetout.txt"), "pipe: service | write file (greet → file)"),
        None    => { println!("files-test: FAIL - pipe greet|write timeout"); fail += 1; }
    }
    match run!(b"read /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("hello from godspeed"), "pipe: service | write captured greet's output"),
        None    => { println!("files-test: FAIL - read greetout timeout"); fail += 1; }
    }
    // ── multi-stage (3 stages): producer | filter | sink ───────────────────────────
    // builtin producer → service filter → write sink: echo → upper → file.
    match run!(b"echo lower text | upper | write /up.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("up.txt"), "pipe: 3-stage echo | upper | write (wired)"),
        None    => { println!("files-test: FAIL - 3-stage echo|upper|write timeout"); fail += 1; }
    }
    match run!(b"read /up.txt\r", 10) {
        Some(r) => check!(r.contains("LOWER TEXT"), "pipe: 3-stage filtered through upper to file"),
        None    => { println!("files-test: FAIL - read up.txt timeout"); fail += 1; }
    }
    // service producer → service filter → write sink: greet → upper → file.
    match run!(b"greet | upper | write /gu.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("gu.txt"), "pipe: 3-stage greet | upper | write (wired)"),
        None    => { println!("files-test: FAIL - 3-stage greet|upper|write timeout"); fail += 1; }
    }
    match run!(b"read /gu.txt\r", 10) {
        Some(r) => check!(r.contains("HELLO FROM GODSPEED") && r.contains("NO AMBIENT AUTHORITY HERE"),
                          "pipe: 3-stage greet → upper → file (all lines uppercased)"),
        None    => { println!("files-test: FAIL - read gu.txt timeout"); fail += 1; }
    }

    // ── match: the grep-equivalent line filter (direct, pipe, except, glob, quoting) ────
    // /greetout.txt holds greet's 3 lines. Direct form: keep lines matching a substring.
    match run!(b"match capability /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("no ambient authority"),
                          "match: direct, keeps only the matching line"),
        None    => { println!("files-test: FAIL - match direct timeout"); fail += 1; }
    }
    // `except`: keep the lines that do NOT match.
    match run!(b"match except capability /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("no ambient authority")
                          && !r.contains("capability pipes work"),
                          "match except: keeps the non-matching lines"),
        None    => { println!("files-test: FAIL - match except timeout"); fail += 1; }
    }
    // Pipe filter (last stage): a service producer's lines through match, printed to console.
    match run!(b"greet | match ambient\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "match: as a pipe filter (greet | match ambient)"),
        None    => { println!("files-test: FAIL - greet|match timeout"); fail += 1; }
    }
    // Anchored glob (whole-line): `*here` keeps lines ending in "here".
    match run!(b"greet | match *here\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("capability pipes work"),
                          "match: glob (anchored *here)"),
        None    => { println!("files-test: FAIL - match glob timeout"); fail += 1; }
    }
    // 3-stage with match in the MIDDLE: greet | match except hello | write.
    match run!(b"greet | match except hello | write /mx.txt\r", 16) {
        Some(r) => check!(r.contains("piped") && r.contains("mx.txt"), "match: mid-pipe filter (3-stage wired)"),
        None    => { println!("files-test: FAIL - 3-stage match timeout"); fail += 1; }
    }
    match run!(b"read /mx.txt\r", 10) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("hello from godspeed"),
                          "match: mid-pipe dropped the 'hello' line"),
        None    => { println!("files-test: FAIL - read mx.txt timeout"); fail += 1; }
    }
    // Minimal quoting: a quoted multi-word pattern is one argument. Without quoting, "two" and
    // "words" would split and nothing would match the input "two words".
    match run!(b"echo two words | match \"two words\"\r", 14) {
        Some(r) => check!(r.contains("two words"), "match: quoted multi-word pattern (\"two words\")"),
        None    => { println!("files-test: FAIL - match quoting timeout"); fail += 1; }
    }

    // ── count: the wc-equivalent (lines / words / bytes) ────────────────────────────
    // Pipe sink: count a producer's lines. greet emits 3 lines.
    match run!(b"greet | count\r", 14) {
        Some(r) => check!(r.contains("3 lines"), "count: pipe sink (greet | count → 3 lines)"),
        None    => { println!("files-test: FAIL - greet|count timeout"); fail += 1; }
    }
    // Direct: count a file (greet's 3 lines were written to /greetout.txt earlier).
    match run!(b"count /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("3 lines"), "count: direct file count (/greetout.txt → 3 lines)"),
        None    => { println!("files-test: FAIL - count direct timeout"); fail += 1; }
    }
    // Singular forms: echo emits one line, one word.
    match run!(b"echo hello | count\r", 14) {
        Some(r) => check!(r.contains("1 line,") && r.contains("1 word,"), "count: singular (1 line, 1 word)"),
        None    => { println!("files-test: FAIL - echo|count timeout"); fail += 1; }
    }
    // Composition: filter then count (producer | match | count) - drop the 'hello' line, count 2.
    match run!(b"greet | match except hello | count\r", 16) {
        Some(r) => check!(r.contains("2 lines"), "count: composes after a filter (greet | match except hello | count)"),
        None    => { println!("files-test: FAIL - greet|match|count timeout"); fail += 1; }
    }

    // ── sort: order the lines (greet emits hello / capability / no-ambient, out of order) ──
    // Ascending (byte order c < h < n): "capability …" comes before "hello …".
    match run!(b"greet | sort\r", 14) {
        Some(r) => {
            let (cap, hel) = (r.find("capability pipes work"), r.find("hello from godspeed"));
            check!(cap.is_some() && hel.is_some() && cap < hel, "sort: ascending (capability before hello)");
        }
        None => { println!("files-test: FAIL - greet|sort timeout"); fail += 1; }
    }
    // Descending: "no ambient …" comes before "capability …".
    match run!(b"greet | sort reverse\r", 14) {
        Some(r) => {
            let (na, cap) = (r.find("no ambient authority here"), r.find("capability pipes work"));
            check!(na.is_some() && cap.is_some() && na < cap, "sort reverse: descending (no-ambient before capability)");
        }
        None => { println!("files-test: FAIL - greet|sort reverse timeout"); fail += 1; }
    }
    // Direct file sort (/greetout.txt holds greet's lines in original order).
    match run!(b"sort /greetout.txt\r", 10) {
        Some(r) => {
            let (cap, hel) = (r.find("capability pipes work"), r.find("hello from godspeed"));
            check!(cap.is_some() && hel.is_some() && cap < hel, "sort: direct file sort (capability before hello)");
        }
        None => { println!("files-test: FAIL - sort direct timeout"); fail += 1; }
    }
    // Composition: sort is a filter, count after it still sees 3 lines.
    match run!(b"greet | sort | count\r", 16) {
        Some(r) => check!(r.contains("3 lines"), "sort: composes (greet | sort | count → 3 lines)"),
        None    => { println!("files-test: FAIL - greet|sort|count timeout"); fail += 1; }
    }

    // ── first / last: keep the first/last N lines (greet emits hello / capability / no-ambient) ──
    match run!(b"greet | first 1\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && !r.contains("no ambient authority here"),
                          "first: keeps only the first line (first 1)"),
        None    => { println!("files-test: FAIL - greet|first timeout"); fail += 1; }
    }
    match run!(b"greet | last 1\r", 14) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "last: keeps only the last line (last 1)"),
        None    => { println!("files-test: FAIL - greet|last timeout"); fail += 1; }
    }
    match run!(b"greet | first 2\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("capability pipes work")
                          && !r.contains("no ambient authority here"), "first 2: keeps the first two"),
        None    => { println!("files-test: FAIL - greet|first 2 timeout"); fail += 1; }
    }
    // Default count (no N) = 10, so all 3 greet lines pass.
    match run!(b"greet | last\r", 14) {
        Some(r) => check!(r.contains("hello from godspeed") && r.contains("no ambient authority here"),
                          "last: default N=10 (all 3 lines)"),
        None    => { println!("files-test: FAIL - greet|last default timeout"); fail += 1; }
    }
    // Direct form on a file.
    match run!(b"last 1 /greetout.txt\r", 10) {
        Some(r) => check!(r.contains("no ambient authority here") && !r.contains("hello from godspeed"),
                          "last: direct file (last 1 /greetout.txt)"),
        None    => { println!("files-test: FAIL - last direct timeout"); fail += 1; }
    }
    // Composition: sort then take the first line → the alphabetically-first ("capability …").
    match run!(b"greet | sort | first 1\r", 16) {
        Some(r) => check!(r.contains("capability pipes work") && !r.contains("hello from godspeed"),
                          "first: composes after sort (greet | sort | first 1)"),
        None    => { println!("files-test: FAIL - greet|sort|first timeout"); fail += 1; }
    }

    // ── from json: the byte↔record bridge (read text → parse → manipulate → render) ──
    let _ = run!(b"write /rec.json [{\"name\":\"alpha\",\"n\":1},{\"name\":\"beta\",\"n\":2}]\r", 10);
    // read (bytes) → from json (records) → default table render.
    match run!(b"read /rec.json | from json\r", 12) {
        Some(r) => check!(r.contains("alpha") && r.contains("beta") && r.contains("name"),
                          "from json: parses a json file into a table"),
        None    => { println!("files-test: FAIL - from json timeout"); fail += 1; }
    }
    // read → from json → where (record filter on parsed data) → to json.
    match run!(b"read /rec.json | from json | where n>1 | to json\r", 12) {
        Some(r) => check!(r.contains("beta") && !r.contains("alpha"),
                          "from json | where: filters parsed records"),
        None    => { println!("files-test: FAIL - from json|where timeout"); fail += 1; }
    }
    // read → from json → select → to json (column projection on parsed data).
    match run!(b"read /rec.json | from json | select name | to json\r", 12) {
        Some(r) => check!(r.contains("\"name\": \"alpha\"") && !r.contains("\"n\":"),
                          "from json | select: projects parsed columns"),
        None    => { println!("files-test: FAIL - from json|select timeout"); fail += 1; }
    }
    // round-trip across formats: json file → records → yaml → file → read back.
    let _ = run!(b"read /rec.json | from json | to yaml | write /rec.yaml\r", 12);
    match run!(b"read /rec.yaml\r", 10) {
        Some(r) => check!(r.contains("name: alpha") && r.contains("n: 2"),
                          "from json | to yaml | write: json→records→yaml round-trip"),
        None    => { println!("files-test: FAIL - json→yaml roundtrip timeout"); fail += 1; }
    }

    // ── a SERVICE producing records via the binary WIRE CODEC: `roster` builds a Table with the
    //    SDK, `encode`s it, and the shell `decode`s it straight back - NO `from json` round-trip
    //    (docs/records.md, sdk/rust/CLAUDE.md). Proves records cross a service boundary as records.
    match run!(b"roster | where role=core\r", 16) {
        Some(r) => check!(r.contains("Matthew") && !r.contains("Mark"),
                          "roster codec: where filters records decoded from the service stream"),
        None    => { println!("files-test: FAIL - roster|where timeout"); fail += 1; }
    }
    match run!(b"roster | select name seat | to json\r", 16) {
        Some(r) => check!(r.contains("\"name\": \"Mark\"") && r.contains("\"seat\":") && !r.contains("\"role\""),
                          "roster codec: select + to json projects the decoded records"),
        None    => { println!("files-test: FAIL - roster|select timeout"); fail += 1; }
    }
    // It really is a record stream now (not text): a text filter is the loud, guided error.
    match run!(b"roster | match Mark\r", 16) {
        Some(r) => check!(r.contains("record stream") && r.contains("where"),
                          "roster codec: a text filter on the decoded records errors with guidance"),
        None    => { println!("files-test: FAIL - roster|match guard timeout"); fail += 1; }
    }
    // Bare `roster` (no pipe) renders the same table directly.
    match run!(b"roster\r", 16) {
        Some(r) => check!(r.contains("Matthew") && r.contains("John") && r.contains("role"),
                          "roster: callable bare - renders the record table"),
        None    => { println!("files-test: FAIL - bare roster timeout"); fail += 1; }
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
        None    => { println!("files-test: FAIL - ls /lsr timeout"); fail += 1; }
    }
    // where on the `type` column keeps only directories.
    match run!(b"ls /lsr | where type=dir\r", 12) {
        Some(r) => check!(r.contains("kids") && !r.contains("big.txt"),
                          "ls record: where type=dir keeps the subdir, drops files"),
        None    => { println!("files-test: FAIL - ls|where type=dir timeout"); fail += 1; }
    }
    // select projects just the name column (no type/size keys).
    match run!(b"ls /lsr | select name | to json\r", 12) {
        Some(r) => check!(r.contains("\"name\": \"big.txt\"") && !r.contains("\"type\"") && !r.contains("\"size\""),
                          "ls record: select name projects one column"),
        None    => { println!("files-test: FAIL - ls|select timeout"); fail += 1; }
    }
    // where type=file | to json renders file rows with a numeric size, no subdir.
    match run!(b"ls /lsr | where type=file | to json\r", 12) {
        Some(r) => check!(r.contains("\"type\": \"file\"") && r.contains("\"size\":") && !r.contains("kids"),
                          "ls record: where type=file | to json renders file rows"),
        None    => { println!("files-test: FAIL - ls|where|to json timeout"); fail += 1; }
    }
    // column sort works on the listing: reverse size puts big.txt (11) before tiny.txt (1).
    match run!(b"ls /lsr | where type=file | sort reverse size | to json\r", 12) {
        Some(r) => {
            let (big, tiny) = (r.find("big.txt"), r.find("tiny.txt"));
            check!(big.is_some() && tiny.is_some() && big < tiny,
                   "ls record: sort reverse size orders files by byte size");
        }
        None => { println!("files-test: FAIL - ls|sort size timeout"); fail += 1; }
    }
    // a text filter on a record stream is a loud, guided error (not silent, not wrong output).
    match run!(b"ls /lsr | match big\r", 12) {
        Some(r) => check!(r.contains("record stream") && r.contains("where"),
                          "ls record: text filter (match) on records errors with guidance"),
        None    => { println!("files-test: FAIL - ls|match guard timeout"); fail += 1; }
    }

    // ── drives as a record producer: the attached disk as a row (index/label/status/size) ──
    // The test disk is GSFS-formatted (we've been writing files to it).
    match run!(b"drives | to json\r", 12) {
        Some(r) => check!(r.contains("\"status\": \"GSFS\"") && r.contains("\"size_mib\":"),
                          "drives record: to json renders the GSFS drive row"),
        None    => { println!("files-test: FAIL - drives|to json timeout"); fail += 1; }
    }

    // ── find as a record producer: each hit is a row (name/type/path) ──────────────────
    // /lsr holds big.txt + tiny.txt (files) and kids (dir).
    match run!(b"find big /lsr | to json\r", 12) {
        Some(r) => check!(r.contains("big.txt") && r.contains("\"type\": \"file\"") && r.contains("/lsr/big.txt"),
                          "find record: to json renders name/type/path"),
        None    => { println!("files-test: FAIL - find|to json timeout"); fail += 1; }
    }
    // where on the type column: a `*` glob matches all three, where type=dir keeps only kids.
    match run!(b"find * /lsr | where type=dir\r", 12) {
        Some(r) => check!(r.contains("kids") && !r.contains("big.txt"),
                          "find record: where type=dir keeps the subdir, drops the files"),
        None    => { println!("files-test: FAIL - find|where type=dir timeout"); fail += 1; }
    }
    // select projects the path column for files only.
    match run!(b"find *.txt /lsr | select path\r", 12) {
        Some(r) => check!(r.contains("/lsr/big.txt") && r.contains("/lsr/tiny.txt"),
                          "find record: select path projects the matched paths"),
        None    => { println!("files-test: FAIL - find|select timeout"); fail += 1; }
    }

    // ── result: the Ok/Err Result model on `read` (the first command converted). `result` prints
    //    the previous command's result in Rust's shape - Ok, or Err(<Variant>). (/lsr/big.txt was
    //    created in the ls section above.)
    let _ = run!(b"read /lsr/big.txt\r", 10);             // exists → Ok
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Ok") && !r.contains("Err"), "result: Ok after a successful read"),
        None    => { println!("files-test: FAIL - result(ok) timeout"); fail += 1; }
    }
    let _ = run!(b"read /lsr/does_not_exist\r", 10);      // missing → Err(FileNotFound)
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: Err(FileNotFound) after a missing read"),
        None    => { println!("files-test: FAIL - result(err) timeout"); fail += 1; }
    }
    // a blank line is not a command, so it leaves the last result unchanged. (Note `result`
    // itself succeeds, so it would reset to Ok - hence a fresh failing read right before.)
    let _ = run!(b"read /lsr/still_missing\r", 10);       // Err(FileNotFound)
    let _ = run!(b"\r", 8);                               // blank - not a command
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: a blank line leaves the last result unchanged"),
        None    => { println!("files-test: FAIL - result(blank) timeout"); fail += 1; }
    }

    // ── run: execute a script of commands (the .gsh runner). Authored on one line with `;`
    //    separators (no newline typing needed). The script reads a present then a missing file,
    //    printing `result` after each - so it exercises echo, execution, and pass/fail counting.
    let _ = run!(b"write /suite.gsh read /lsr/big.txt ; result ; read /lsr/nope ; result\r", 10);
    match run!(b"run /suite.gsh\r", 16) {
        Some(r) => {
            check!(r.contains("> read /lsr/big.txt") && r.contains("hello world"),
                   "run: echoes and executes a script command");
            check!(r.contains("Err(FileNotFound)"), "run: a failing line surfaces its Err");
            check!(r.contains("run: ran 4, failed 1"), "run: summary counts ran/failed");
        }
        None => { println!("files-test: FAIL - run timeout"); fail += 3; }
    }
    // a missing script reports not found (and `run` returns Err).
    match run!(b"run /no_such.gsh\r", 10) {
        Some(r) => check!(r.contains("not found"), "run: a missing script reports not found"),
        None    => { println!("files-test: FAIL - run(missing) timeout"); fail += 1; }
    }
    // scripts cannot nest: a `run` line inside a script is refused.
    let _ = run!(b"write /nest.gsh run /suite.gsh\r", 10);
    match run!(b"run /nest.gsh\r", 14) {
        Some(r) => check!(r.contains("cannot run another script"), "run: nested run is refused (stack-bounded)"),
        None    => { println!("files-test: FAIL - run(nest) timeout"); fail += 1; }
    }

    // ── gsh language (Slice 1: variables, $-expansion, params, fail). docs/scripting.md. Authored
    //    on one line with `;` (the file stores the `;`; `run` splits it into statements).
    // let + $-interpolation: `v=$n` prints `v=7` ONLY if $n expanded (the raw statement is `echo v=$n`).
    let _ = run!(b"write /gsh_a.gsh let n = 7 ; echo v=$n\r", 10);
    match run!(b"run /gsh_a.gsh\r", 14) {
        Some(r) => {
            check!(r.contains("v=7"), "gsh: let binds and $var interpolates");
            check!(r.contains("run: ran 2, failed 0"), "gsh: a let + echo run passes clean");
        }
        None => { println!("files-test: FAIL - gsh let timeout"); fail += 2; }
    }
    // undefined variable is a LOUD error (not a silent empty string) and fails the statement.
    let _ = run!(b"write /gsh_u.gsh echo $missing\r", 10);
    match run!(b"run /gsh_u.gsh\r", 14) {
        Some(r) => {
            check!(r.contains("undefined variable"), "gsh: undefined $var is loud");
            check!(r.contains("run: ran 1, failed 1"), "gsh: undefined $var fails the statement");
        }
        None => { println!("files-test: FAIL - gsh undefined timeout"); fail += 2; }
    }
    // immutable by default: reassigning a `let` binding is a loud error.
    let _ = run!(b"write /gsh_i.gsh let x = 1 ; x = 2\r", 10);
    match run!(b"run /gsh_i.gsh\r", 14) {
        Some(r) => check!(r.contains("cannot reassign immutable") && r.contains("run: ran 2, failed 1"),
                          "gsh: reassigning an immutable binding is loud"),
        None => { println!("files-test: FAIL - gsh immutable timeout"); fail += 1; }
    }
    // let mut + reassignment + params: `run … alpha beta` → $1/$2; who=alpha then who=beta.
    let _ = run!(b"write /gsh_m.gsh let mut who = $1 ; who = $2 ; echo picked-$who\r", 10);
    match run!(b"run /gsh_m.gsh alpha beta\r", 14) {
        Some(r) => check!(r.contains("picked-beta") && r.contains("run: ran 3, failed 0"),
                          "gsh: let mut reassigns; $1/$2 params expand"),
        None => { println!("files-test: FAIL - gsh mut/params timeout"); fail += 1; }
    }
    // fail stops the run: the statement after `fail` does not execute.
    let _ = run!(b"write /gsh_f.gsh echo one ; fail stop-here ; echo two\r", 10);
    match run!(b"run /gsh_f.gsh\r", 14) {
        Some(r) => check!(r.contains("fail: stop-here") && r.contains("run: ran 2, failed 1") && !r.contains("> echo two"),
                          "gsh: fail prints loudly and stops the run"),
        None => { println!("files-test: FAIL - gsh fail timeout"); fail += 1; }
    }

    // ── gsh Slice 2: if/else, comparisons, `in`, command conditions, `result` (docs/scripting.md §4).
    // numeric comparison, then-branch taken; the else block is skipped (never echoed).
    let _ = run!(b"write /g2a.gsh let n = 5 ; if $n > 3 { echo big } else { echo small }\r", 10);
    match run!(b"run /g2a.gsh\r", 16) {
        Some(r) => check!(r.contains("big") && !r.contains("small") && r.contains("run: ran 2, failed 0"),
                          "gsh: if takes the then-branch on a true comparison (else skipped, counted right)"),
        None => { println!("files-test: FAIL - gsh if-then timeout"); fail += 1; }
    }
    // false comparison → else-branch.
    let _ = run!(b"write /g2b.gsh let n = 2 ; if $n > 3 { echo big } else { echo small }\r", 10);
    match run!(b"run /g2b.gsh\r", 14) {
        Some(r) => check!(r.contains("small") && !r.contains("big"), "gsh: if takes the else-branch on a false comparison"),
        None => { println!("files-test: FAIL - gsh if-else timeout"); fail += 1; }
    }
    // else-if chain + string equality: only the matching arm runs.
    let _ = run!(b"write /g2c.gsh let g = B ; if $g == A { echo is-a } else if $g == B { echo is-b } else { echo other }\r", 10);
    match run!(b"run /g2c.gsh\r", 14) {
        Some(r) => check!(r.contains("is-b") && !r.contains("is-a") && !r.contains("other"),
                          "gsh: else-if chain runs only the matching arm"),
        None => { println!("files-test: FAIL - gsh else-if timeout"); fail += 1; }
    }
    // membership: `$x in a b c`.
    let _ = run!(b"write /g2d.gsh let role = core ; if $role in worker core courier { echo known } else { echo unknown }\r", 10);
    match run!(b"run /g2d.gsh\r", 14) {
        Some(r) => check!(r.contains("known") && !r.contains("unknown"), "gsh: `$x in ...` membership condition"),
        None => { println!("files-test: FAIL - gsh in timeout"); fail += 1; }
    }
    // negated command condition: `!read <missing>` is true (the read errors).
    let _ = run!(b"write /g2e.gsh if !read /nope_xyz.txt { echo absent } else { echo present }\r", 10);
    match run!(b"run /g2e.gsh\r", 16) {
        Some(r) => check!(r.contains("absent") && !r.contains("present"), "gsh: `!<command>` negates a command condition"),
        None => { println!("files-test: FAIL - gsh negate timeout"); fail += 1; }
    }
    // result comparison: branch on the previous statement's specific failure kind.
    let _ = run!(b"write /g2f.gsh read /nope_xyz.txt ; if result == FileNotFound { echo caught }\r", 10);
    match run!(b"run /g2f.gsh\r", 14) {
        Some(r) => check!(r.contains("caught") && r.contains("run: ran 2, failed 1"),
                          "gsh: `if result == FileNotFound` branches on the prior result kind"),
        None => { println!("files-test: FAIL - gsh result timeout"); fail += 1; }
    }
    // nested if: the executor handles block nesting (no native recursion).
    let _ = run!(b"write /g2h.gsh let x = 1 ; if $x == 1 { if $x < 5 { echo nested-ok } }\r", 10);
    match run!(b"run /g2h.gsh\r", 14) {
        Some(r) => check!(r.contains("nested-ok") && r.contains("run: ran 2, failed 0"), "gsh: nested if blocks execute"),
        None => { println!("files-test: FAIL - gsh nested timeout"); fail += 1; }
    }

    // ── gsh minifier: collapse whitespace OUTSIDE quotes, PRESERVE it inside (aggressive minify).
    // Heavy internal padding must still tokenize correctly (collapse is semantics-neutral outside
    // quotes, since gsh separates tokens by whitespace).
    let _ = run!(b"write /gmw.gsh let    x   =   7 ;   echo   val-$x\r", 10);
    match run!(b"run /gmw.gsh\r", 14) {
        Some(r) => check!(r.contains("val-7") && r.contains("run: ran 2, failed 0"),
                          "gsh: minifier collapses padded whitespace outside quotes (still tokenizes right)"),
        None => { println!("files-test: FAIL - gsh minify-collapse timeout"); fail += 1; }
    }
    // The harness expected string carries TWO spaces and is NOT itself minified, so a bug that
    // collapsed whitespace INSIDE quotes would drop a space and fail this.
    let _ = run!(b"write /gmq.gsh echo \"keep  gap\"\r", 10);
    match run!(b"run /gmq.gsh\r", 14) {
        Some(r) => check!(r.contains("keep  gap"), "gsh: minifier preserves whitespace INSIDE quotes"),
        None => { println!("files-test: FAIL - gsh minify-quote timeout"); fail += 1; }
    }

    // ── gsh Slice 3: switch (docs/scripting.md §6). No fallthrough; `_` default; multi-value arms.
    let _ = run!(b"write /g3a.gsh let cmd = start ; switch $cmd { start { echo starting } stop { echo stopping } _ { echo unknownc } }\r", 10);
    match run!(b"run /g3a.gsh\r", 16) {
        Some(r) => check!(r.contains("starting") && !r.contains("stopping") && !r.contains("unknownc") && r.contains("run: ran 2, failed 0"),
                          "gsh: switch runs the matching arm (others skipped, counted right)"),
        None => { println!("files-test: FAIL - gsh switch timeout"); fail += 1; }
    }
    // multiple values per arm.
    let _ = run!(b"write /g3b.gsh let r = courier ; switch $r { core { echo iscore } worker courier { echo helper } _ { echo otherr } }\r", 10);
    match run!(b"run /g3b.gsh\r", 16) {
        Some(r) => check!(r.contains("helper") && !r.contains("iscore") && !r.contains("otherr"), "gsh: switch multi-value arm matches"),
        None => { println!("files-test: FAIL - gsh switch multi timeout"); fail += 1; }
    }
    // `_` default arm.
    let _ = run!(b"write /g3c.gsh let x = zzz ; switch $x { a { echo isa } _ { echo fellthrough } }\r", 10);
    match run!(b"run /g3c.gsh\r", 16) {
        Some(r) => check!(r.contains("fellthrough") && !r.contains("isa"), "gsh: switch `_` default arm"),
        None => { println!("files-test: FAIL - gsh switch default timeout"); fail += 1; }
    }
    // switch on `result`.
    let _ = run!(b"write /g3d.gsh read /nope_xyz.txt ; switch result { Ok { echo wasok } FileNotFound { echo wasmissing } _ { echo otherr } }\r", 10);
    match run!(b"run /g3d.gsh\r", 16) {
        Some(r) => check!(r.contains("wasmissing") && !r.contains("wasok"), "gsh: `switch result` matches by result kind"),
        None => { println!("files-test: FAIL - gsh switch result timeout"); fail += 1; }
    }
    // Tier 1 complete: a greet-shape script (param + if + in + switch + else/fail) runs end to end.
    // Kept short so the `write` line fits the 128-char interactive input buffer (MAX_LINE).
    let _ = run!(b"write /g.gsh let r = $1 ; if $r in a b { switch $r { a { echo gotA } b { echo gotB } } } else { fail nomatch }\r", 12);
    match run!(b"run /g.gsh a\r", 16) {
        Some(r) => check!(r.contains("gotA") && !r.contains("gotB") && r.contains("failed 0"),
                          "gsh: Tier-1 greet-shape script (param+if+in+switch) runs"),
        None => { println!("files-test: FAIL - gsh greet timeout"); fail += 1; }
    }
    // the same script's failure path: a non-member arg takes the else and fails loudly.
    match run!(b"run /g.gsh z\r", 16) {
        Some(r) => check!(r.contains("fail: nomatch"), "gsh: greet-shape script fails loudly on a bad arg"),
        None => { println!("files-test: FAIL - gsh greet-fail timeout"); fail += 1; }
    }

    // ── fmt (utilities/39_fmt.md): one canonical layout, in place, semantics-preserving + idempotent.
    // A valid but jarring one-liner (`;`-joined, inline blocks) expands to canonical layout.
    let _ = run!(b"write /fm.gsh let n = 7 ; if $n > 5 { echo big } else { echo small }\r", 12);
    let _ = run!(b"fmt /fm.gsh\r", 12);
    match run!(b"read /fm.gsh\r", 12) {
        Some(r) => check!(r.contains("if $n > 5 {") && r.contains("    echo big") && r.contains("} else {"),
                          "fmt: cramped one-liner expands to canonical layout (4-space indent, one/line, K&R braces)"),
        None => { println!("files-test: FAIL - fmt layout timeout"); fail += 1; }
    }
    // Semantics preserved: the formatted script still runs and produces the same result.
    match run!(b"run /fm.gsh\r", 14) {
        Some(r) => check!(r.contains("big") && r.contains("failed 0"), "fmt: formatted script still runs (layout-only, semantics preserved)"),
        None => { println!("files-test: FAIL - fmt run timeout"); fail += 1; }
    }
    // Idempotent: after formatting, `fmt check` reports it canonical (a re-format would be a no-op).
    match run!(b"fmt check /fm.gsh\r", 12) {
        Some(r) => check!(!r.contains("not canonical") && !r.contains("won't parse"), "fmt: idempotent (fmt check on formatted output = canonical)"),
        None => { println!("files-test: FAIL - fmt idempotent timeout"); fail += 1; }
    }
    // check mode: flags a non-canonical file loudly, never writes.
    let _ = run!(b"write /fm2.gsh echo a ; echo b\r", 10);
    match run!(b"fmt check /fm2.gsh\r", 12) {
        Some(r) => check!(r.contains("not canonical"), "fmt check: flags a non-canonical file loudly (Err, never writes)"),
        None => { println!("files-test: FAIL - fmt check timeout"); fail += 1; }
    }
    // Guardrail: an unparseable script (unbalanced braces) is refused, file left untouched.
    let _ = run!(b"write /fmb.gsh if $x { echo hi\r", 10);
    match run!(b"fmt /fmb.gsh\r", 12) {
        Some(r) => check!(r.contains("won't parse") && r.contains("untouched"), "fmt: refuses an unparseable script, file untouched (guardrail)"),
        None => { println!("files-test: FAIL - fmt guardrail timeout"); fail += 1; }
    }

    // ── gsh Slice 4 (Tier 2): integer arithmetic in value position (docs/scripting.md §3).
    let _ = run!(b"write /a1.gsh let x = 2 + 3 * 4 ; echo x=$x\r", 10);
    match run!(b"run /a1.gsh\r", 14) {
        Some(r) => check!(r.contains("x=14"), "gsh: arithmetic precedence (* before +)"),
        None => { println!("files-test: FAIL - gsh arith prec timeout"); fail += 1; }
    }
    let _ = run!(b"write /a2.gsh let x = ( 2 + 3 ) * 4 ; echo x=$x\r", 10);
    match run!(b"run /a2.gsh\r", 14) {
        Some(r) => check!(r.contains("x=20"), "gsh: arithmetic parentheses group"),
        None => { println!("files-test: FAIL - gsh arith paren timeout"); fail += 1; }
    }
    let _ = run!(b"write /a3.gsh let a = 5 ; let b = 3 ; let s = $a + $b ; echo s=$s\r", 10);
    match run!(b"run /a3.gsh\r", 14) {
        Some(r) => check!(r.contains("s=8"), "gsh: arithmetic over $vars"),
        None => { println!("files-test: FAIL - gsh arith vars timeout"); fail += 1; }
    }
    let _ = run!(b"write /a4.gsh let q = 17 / 5 ; let r = 17 % 5 ; echo $q-$r\r", 10);
    match run!(b"run /a4.gsh\r", 14) {
        Some(r) => check!(r.contains("3-2"), "gsh: integer division and modulo"),
        None => { println!("files-test: FAIL - gsh arith divmod timeout"); fail += 1; }
    }
    // the loop idiom: reassign a mutable via arithmetic.
    let _ = run!(b"write /a5.gsh let mut i = 0 ; i = $i + 1 ; i = $i + 1 ; echo i=$i\r", 10);
    match run!(b"run /a5.gsh\r", 14) {
        Some(r) => check!(r.contains("i=2"), "gsh: reassign a mutable via arithmetic (i = $i + 1)"),
        None => { println!("files-test: FAIL - gsh arith reassign timeout"); fail += 1; }
    }
    // divide by zero is loud.
    let _ = run!(b"write /a6.gsh let x = 5 / 0\r", 10);
    match run!(b"run /a6.gsh\r", 14) {
        Some(r) => check!(r.contains("divide by zero") && r.contains("run: ran 1, failed 1"), "gsh: divide by zero is loud"),
        None => { println!("files-test: FAIL - gsh arith div0 timeout"); fail += 1; }
    }
    // a non-integer operand is loud.
    let _ = run!(b"write /a7.gsh let n = abc ; let x = $n + 1\r", 10);
    match run!(b"run /a7.gsh\r", 14) {
        Some(r) => check!(r.contains("not an integer") && r.contains("failed 1"), "gsh: non-integer arithmetic operand is loud"),
        None => { println!("files-test: FAIL - gsh arith nonint timeout"); fail += 1; }
    }
    // arithmetic on the left of a comparison.
    let _ = run!(b"write /a8.gsh let i = 4 ; if $i + 1 >= 5 { echo reached }\r", 10);
    match run!(b"run /a8.gsh\r", 14) {
        Some(r) => check!(r.contains("reached"), "gsh: arithmetic in a comparison condition"),
        None => { println!("files-test: FAIL - gsh arith cond timeout"); fail += 1; }
    }
    // a path with `/` and no spaces is NOT arithmetic (the space rule keeps paths and math distinct).
    let _ = run!(b"write /a9.gsh let d = /work ; echo $d/logs\r", 10);
    match run!(b"run /a9.gsh\r", 14) {
        Some(r) => check!(r.contains("/work/logs"), "gsh: `$dir/sub` is a path, not division (space rule)"),
        None => { println!("files-test: FAIL - gsh arith path timeout"); fail += 1; }
    }

    // ── gsh Slice 5 (Tier 2): $( ) command capture as a value (docs/scripting.md §3).
    // capture a producer builtin's output.
    let _ = run!(b"write /c1.gsh let x = $(echo hello) ; echo cap=$x\r", 10);
    match run!(b"run /c1.gsh\r", 14) {
        Some(r) => check!(r.contains("cap=hello"), "gsh: $(cmd) captures a builtin's output"),
        None => { println!("files-test: FAIL - gsh capture builtin timeout"); fail += 1; }
    }
    // capture date (non-empty, real).
    let _ = run!(b"write /c2.gsh let d = $(date) ; echo when:$d\r", 10);
    match run!(b"run /c2.gsh\r", 14) {
        Some(r) => check!(r.contains("when:") && r.contains("2026"), "gsh: $(date) captures the date stamp"),
        None => { println!("files-test: FAIL - gsh capture date timeout"); fail += 1; }
    }
    // capture $(read <file>) - a bare producer, and the file-staging idiom for a pipeline (run the
    // pipeline to a file, then capture the file). A `$(greet | count)` PIPELINE capture is refused
    // loudly (bounded stack); it can't be authored inline (the `|` in the write line is intercepted),
    // so the file-staging path is exercised in the baked scripts/smoke.gsh (osdev test script).
    let _ = run!(b"write /c3.gsh write /rr.txt captured-content ; let z = $(read /rr.txt) ; echo z=$z\r", 10);
    match run!(b"run /c3.gsh\r", 16) {
        Some(r) => check!(r.contains("z=captured-content"), "gsh: $(read <file>) captures file content"),
        None => { println!("files-test: FAIL - gsh capture read timeout"); fail += 1; }
    }
    // capture into a reassignment.
    let _ = run!(b"write /c4.gsh let mut x = a ; x = $(echo zed) ; echo x=$x\r", 10);
    match run!(b"run /c4.gsh\r", 14) {
        Some(r) => check!(r.contains("x=zed"), "gsh: $(cmd) captures into a reassignment"),
        None => { println!("files-test: FAIL - gsh capture reassign timeout"); fail += 1; }
    }
    // a bare non-producer is refused loudly (pipe it instead).
    let _ = run!(b"write /c5.gsh let x = $(status)\r", 10);
    match run!(b"run /c5.gsh\r", 14) {
        Some(r) => check!(r.contains("cannot capture") && r.contains("run: ran 1, failed 1"), "gsh: $(bare-non-producer) refused loudly"),
        None => { println!("files-test: FAIL - gsh capture refuse timeout"); fail += 1; }
    }

    // ── gsh Slice 6 (Tier 2): functions (docs/scripting.md §7). Called like a command, named params.
    let _ = run!(b"write /fn1.gsh fn greet name { echo hi-$name } ; greet Ada\r", 10);
    match run!(b"run /fn1.gsh\r", 14) {
        Some(r) => check!(r.contains("hi-Ada"), "gsh: fn defined + called with a named param"),
        None => { println!("files-test: FAIL - gsh fn call timeout"); fail += 1; }
    }
    // a call may precede its definition (the pre-scan indexes it).
    let _ = run!(b"write /fn2.gsh doit ; fn doit { echo doit-ok }\r", 10);
    match run!(b"run /fn2.gsh\r", 14) {
        Some(r) => check!(r.contains("doit-ok"), "gsh: a call may precede the fn definition"),
        None => { println!("files-test: FAIL - gsh fn prescan timeout"); fail += 1; }
    }
    // a function sees an IMMUTABLE global (scope = params + locals + immutable globals, §7).
    let _ = run!(b"write /fn3.gsh let g = GLOB ; fn showg { echo saw-$g } ; showg\r", 10);
    match run!(b"run /fn3.gsh\r", 14) {
        Some(r) => check!(r.contains("saw-GLOB"), "gsh: fn reads an immutable global"),
        None => { println!("files-test: FAIL - gsh fn global timeout"); fail += 1; }
    }
    // `return` ends the function early - the statement after it does not run.
    let _ = run!(b"write /fn4.gsh fn early { echo AA ; return ; echo BB } ; early\r", 10);
    match run!(b"run /fn4.gsh\r", 14) {
        Some(r) => check!(r.contains("AA") && !r.contains("BB"), "gsh: return ends the function early"),
        None => { println!("files-test: FAIL - gsh fn return timeout"); fail += 1; }
    }
    // recursion (bounded, no native recursion): each frame has its own scope + arithmetic.
    let _ = run!(b"write /fn5.gsh fn rec n { if $n > 0 { let m = $n - 1 ; rec $m } else { echo bottom-$n } } ; rec 3\r", 12);
    match run!(b"run /fn5.gsh\r", 16) {
        Some(r) => check!(r.contains("bottom-0"), "gsh: recursion (scoped params + arithmetic, explicit frames)"),
        None => { println!("files-test: FAIL - gsh fn recursion timeout"); fail += 1; }
    }
    // a missing argument is loud.
    let _ = run!(b"write /fn6.gsh fn need x { echo have-$x } ; need\r", 10);
    match run!(b"run /fn6.gsh\r", 14) {
        Some(r) => check!(r.contains("missing argument"), "gsh: a missing function argument is loud"),
        None => { println!("files-test: FAIL - gsh fn missing-arg timeout"); fail += 1; }
    }

    // ── gsh Slice 7 (Tier 2): libraries via `import` / `from … import … as …` (§7, load-time).
    // A shared library on disk, then main scripts that import from it.
    let _ = run!(b"write /liba.gsh fn hi { echo hi-lib } ; fn add2 n { echo got-$n }\r", 10);
    // whole-lib import: every function becomes callable.
    let _ = run!(b"write /mi1.gsh import /liba.gsh ; hi\r", 10);
    match run!(b"run /mi1.gsh\r", 14) {
        Some(r) => check!(r.contains("hi-lib"), "gsh: import <path> makes a lib function callable"),
        None => { println!("files-test: FAIL - gsh import-all timeout"); fail += 1; }
    }
    // selective import + a param.
    let _ = run!(b"write /mi2.gsh from /liba.gsh import add2 ; add2 Z\r", 10);
    match run!(b"run /mi2.gsh\r", 14) {
        Some(r) => check!(r.contains("got-Z"), "gsh: from <path> import <name> (selective) + param"),
        None => { println!("files-test: FAIL - gsh import-selective timeout"); fail += 1; }
    }
    // aliased import (`as`) renames the binding.
    let _ = run!(b"write /mi3.gsh from /liba.gsh import hi as greet ; greet\r", 10);
    match run!(b"run /mi3.gsh\r", 14) {
        Some(r) => check!(r.contains("hi-lib"), "gsh: from … import … as <alias> renames the binding"),
        None => { println!("files-test: FAIL - gsh import-as timeout"); fail += 1; }
    }
    // a name collision is LOUD (first definition wins) - `as` is how you resolve it.
    let _ = run!(b"write /mi4.gsh fn hi { echo main-hi } ; import /liba.gsh ; hi\r", 10);
    match run!(b"run /mi4.gsh\r", 14) {
        Some(r) => check!(r.contains("already defined") && r.contains("main-hi"), "gsh: an import name collision is loud"),
        None => { println!("files-test: FAIL - gsh import-collision timeout"); fail += 1; }
    }
    // `as` resolves the collision: both names coexist, no loud clash.
    let _ = run!(b"write /mi5.gsh fn hi { echo mine } ; from /liba.gsh import hi as libhi ; hi ; libhi\r", 12);
    match run!(b"run /mi5.gsh\r", 16) {
        Some(r) => check!(r.contains("mine") && r.contains("hi-lib") && !r.contains("already defined"), "gsh: `as` resolves a collision (both names coexist)"),
        None => { println!("files-test: FAIL - gsh import-as-resolve timeout"); fail += 1; }
    }

    // ── gsh Slice 8 (Tier 2): loops - `for … in words|range|$@`, unbounded `loop`, break/continue.
    let _ = run!(b"write /fl1.gsh for x in a b c { echo w-$x }\r", 10);
    match run!(b"run /fl1.gsh\r", 14) {
        Some(r) => check!(r.contains("w-a") && r.contains("w-b") && r.contains("w-c"), "gsh: for … in <words>"),
        None => { println!("files-test: FAIL - gsh for-words timeout"); fail += 1; }
    }
    let _ = run!(b"write /fl2.gsh for i in range 3 { echo n-$i }\r", 10);
    match run!(b"run /fl2.gsh\r", 14) {
        Some(r) => check!(r.contains("n-0") && r.contains("n-1") && r.contains("n-2") && !r.contains("n-3"), "gsh: for i in range N"),
        None => { println!("files-test: FAIL - gsh for-range timeout"); fail += 1; }
    }
    let _ = run!(b"write /fl3.gsh for i in range 2 5 { echo r-$i }\r", 10);
    match run!(b"run /fl3.gsh\r", 14) {
        Some(r) => check!(r.contains("r-2") && r.contains("r-4") && !r.contains("r-5") && !r.contains("r-1"), "gsh: for i in range A B"),
        None => { println!("files-test: FAIL - gsh for-range-ab timeout"); fail += 1; }
    }
    // unbounded loop + break, with a mutable slot counter (in place - no arena growth).
    let _ = run!(b"write /fl4.gsh let mut i = 0 ; loop { i = $i + 1 ; if $i > 3 { break } ; echo L-$i }\r", 10);
    match run!(b"run /fl4.gsh\r", 14) {
        Some(r) => check!(r.contains("L-1") && r.contains("L-3") && !r.contains("L-4"), "gsh: loop + break (slot counter)"),
        None => { println!("files-test: FAIL - gsh loop-break timeout"); fail += 1; }
    }
    // continue skips the rest of the body.
    let _ = run!(b"write /fl5.gsh for i in range 4 { if $i == 1 { continue } ; echo c-$i }\r", 10);
    match run!(b"run /fl5.gsh\r", 14) {
        Some(r) => check!(r.contains("c-0") && r.contains("c-2") && r.contains("c-3") && !r.contains("c-1"), "gsh: continue skips the rest of the body"),
        None => { println!("files-test: FAIL - gsh continue timeout"); fail += 1; }
    }
    // Reassign a mutable var to a 24-byte value 200x (~4.8 KiB cumulative): the fixed slot holds, but
    // the old bump-append would overflow the 4 KiB arena partway - so reaching -199 proves the slot fix.
    let _ = run!(b"write /fl6.gsh let mut n = z ; for i in range 200 { n = 12345678901234567890-$i } ; echo done-$n\r", 10);
    match run!(b"run /fl6.gsh\r", 25) {
        Some(r) => check!(r.contains("-199") && !r.contains("storage full"), "gsh: 200 long mutable reassignments (fixed slot, no arena blowup)"),
        None => { println!("files-test: FAIL - gsh slot-stress timeout"); fail += 1; }
    }

    // ── gsh: for line in (producer) - capture a producer's output, iterate its lines.
    // (1) Content binding: a single-line producer binds its line to $line (G-hello is expanded output,
    // so it can appear only via the loop body, never in the literal `> echo G-$line` transcript echo).
    let _ = run!(b"write /flsrc.txt hello\r", 10);
    let _ = run!(b"write /flp1.gsh for line in (read /flsrc.txt) { echo G-$line }\r", 10);
    match run!(b"run /flp1.gsh\r", 14) {
        Some(r) => check!(r.contains("G-hello"), "gsh: for line in (producer) binds each line to the loop var"),
        None => { println!("files-test: FAIL - gsh for-line bind timeout"); fail += 1; }
    }
    // (2) Multi-line: a producer with several lines iterates each. Count via a var and print it with an
    // `=END` delimiter (so CNT=1= can't substring-match CNT=10=); >= 2 proves real multi-line iteration.
    let _ = run!(b"mkdir /fld\r", 10);
    let _ = run!(b"write /fld/a.txt xxx\r", 10);
    let _ = run!(b"write /fld/b.txt yyy\r", 10);
    let _ = run!(b"write /flp2.gsh let mut c = 0 ; for line in (tree /fld) { c = $c + 1 } ; echo CNT=$c=END\r", 10);
    match run!(b"run /flp2.gsh\r", 14) {
        Some(r) => check!(r.contains("CNT=") && !r.contains("CNT=0=") && !r.contains("CNT=1="), "gsh: for line in (producer) iterates every line"),
        None => { println!("files-test: FAIL - gsh for-line count timeout"); fail += 1; }
    }
    // (3) break exits a for-line loop (and deletes the temp): the statement after break never runs.
    let _ = run!(b"write /flp3.gsh for line in (tree /fld) { break ; echo LEAK }\r", 10);
    match run!(b"run /flp3.gsh\r", 14) {
        Some(r) => check!(!r.contains("LEAK"), "gsh: break exits a for-line loop"),
        None => { println!("files-test: FAIL - gsh for-line break timeout"); fail += 1; }
    }
    // (4) A non-producer iter is refused loudly - the body never runs (XX appears nowhere).
    let _ = run!(b"write /flp4.gsh for line in (frobnicate) { echo XX }\r", 10);
    match run!(b"run /flp4.gsh\r", 14) {
        Some(r) => check!(!r.contains("XX"), "gsh: for line in (non-producer) refused, body skipped"),
        None => { println!("files-test: FAIL - gsh for-line refuse timeout"); fail += 1; }
    }
    // (5) Nested for-line loops must use DISTINCT temp files (id = each loop's `{` position), so the
    // inner loop's capture doesn't clobber the outer's. NEST-XY is expanded output only.
    let _ = run!(b"write /flx.txt X\r", 10);
    let _ = run!(b"write /fly.txt Y\r", 10);
    let _ = run!(b"write /flp5.gsh for a in (read /flx.txt) { for b in (read /fly.txt) { echo NEST-$a$b } }\r", 10);
    match run!(b"run /flp5.gsh\r", 14) {
        Some(r) => check!(r.contains("NEST-XY"), "gsh: nested for-line loops use distinct temps"),
        None => { println!("files-test: FAIL - gsh for-line nest timeout"); fail += 1; }
    }

    // ── gsh: if myfn { … } - function-valued conditions. The function is RUN (a control-flow jump);
    // we branch on its result. `f` ends in echo (Ok) -> then; `g` reads a missing file (Err) -> else.
    // A marker appears iff its echo executed, so contains/!contains is exact. (Scripts kept short - the
    // `write <path> <content>` line has a length limit; a too-long script is silently truncated.)
    let _ = run!(b"write /fc1.gsh fn f { echo IN } ; if f { echo THEN1 } else { echo ELSE1 }\r", 10);
    match run!(b"run /fc1.gsh\r", 14) {
        Some(r) => check!(r.contains("IN") && r.contains("THEN1") && !r.contains("ELSE1"), "gsh: if myfn - Ok result takes the then-branch"),
        None => { println!("files-test: FAIL - gsh if-fn then timeout"); fail += 1; }
    }
    let _ = run!(b"write /fc2.gsh fn g { read /nz } ; if g { echo THEN2 } else { echo ELSE2 }\r", 10);
    match run!(b"run /fc2.gsh\r", 14) {
        Some(r) => check!(r.contains("ELSE2") && !r.contains("THEN2"), "gsh: if myfn - Err result takes the else-branch"),
        None => { println!("files-test: FAIL - gsh if-fn else timeout"); fail += 1; }
    }
    let _ = run!(b"write /fc3.gsh fn g { read /nz } ; if !g { echo NEG3 }\r", 10);
    match run!(b"run /fc3.gsh\r", 14) {
        Some(r) => check!(r.contains("NEG3"), "gsh: if !myfn - negation of a function condition"),
        None => { println!("files-test: FAIL - gsh if-fn neg timeout"); fail += 1; }
    }
    let _ = run!(b"write /fc4.gsh fn h a { echo ARG-$a } ; if h hi { echo AFT4 }\r", 10);
    match run!(b"run /fc4.gsh\r", 14) {
        Some(r) => check!(r.contains("ARG-hi") && r.contains("AFT4"), "gsh: args pass to a function condition"),
        None => { println!("files-test: FAIL - gsh if-fn arg timeout"); fail += 1; }
    }
    let _ = run!(b"write /fc5.gsh fn g { read /nz } ; if g { echo A5 } else if 1 < 2 { echo EI5 } else { echo C5 }\r", 10);
    match run!(b"run /fc5.gsh\r", 14) {
        Some(r) => check!(r.contains("EI5") && !r.contains("A5") && !r.contains("C5"), "gsh: else if (comparison) after a function-if"),
        None => { println!("files-test: FAIL - gsh if-fn elseif timeout"); fail += 1; }
    }
    let _ = run!(b"write /fc6.gsh if read /nz { echo CMD1 } else { echo CMD2 }\r", 10);
    match run!(b"run /fc6.gsh\r", 14) {
        Some(r) => check!(r.contains("CMD2") && !r.contains("CMD1"), "gsh: a non-function condition is still a command condition"),
        None => { println!("files-test: FAIL - gsh if-cmd timeout"); fail += 1; }
    }

    // ── gsh: $(fn) - capture a FUNCTION's output into a variable. The body's echo goes to the capture
    // buffer (not the console); the trimmed buffer becomes the value. (Scripts kept short - the write
    // line length limit.)
    let _ = run!(b"write /cf1.gsh fn g n { echo Hi-$n } ; let m = $(g Ada) ; echo GOT-$m\r", 10);
    match run!(b"run /cf1.gsh\r", 14) {
        Some(r) => check!(r.contains("GOT-Hi-Ada"), "gsh: $(fn) captures a function's output into a variable"),
        None => { println!("files-test: FAIL - gsh fn-cap timeout"); fail += 1; }
    }
    let _ = run!(b"write /cf2.gsh fn f { echo 42 } ; let n = $(f) ; echo N-$n\r", 10);
    match run!(b"run /cf2.gsh\r", 14) {
        Some(r) => check!(r.contains("N-42"), "gsh: $(fn) with no args"),
        None => { println!("files-test: FAIL - gsh fn-cap noarg timeout"); fail += 1; }
    }
    // The captured value is usable like any variable (here in a condition).
    let _ = run!(b"write /cf3.gsh fn f { echo yes } ; let v = $(f) ; if $v == yes { echo CMATCH } else { echo CMISS }\r", 10);
    match run!(b"run /cf3.gsh\r", 14) {
        Some(r) => check!(r.contains("CMATCH") && !r.contains("CMISS"), "gsh: a $(fn)-captured value is usable downstream"),
        None => { println!("files-test: FAIL - gsh fn-cap use timeout"); fail += 1; }
    }
    // A nested $(fn) (a captured function capturing another) is refused loudly, not silently wrong.
    let _ = run!(b"write /cf4.gsh fn b { echo x } ; fn a { let y = $(b) ; echo done } ; let z = $(a) ; echo Z-$z\r", 10);
    match run!(b"run /cf4.gsh\r", 14) {
        Some(r) => check!(r.contains("nested") && r.contains("Z-done"), "gsh: nested $(fn) capture is refused loudly"),
        None => { println!("files-test: FAIL - gsh fn-cap nested timeout"); fail += 1; }
    }

    // ── gsh Slice 9 (Tier 2): defer - cleanup that runs on scope exit, LIFO, even on fail (§5).
    let _ = run!(b"write /df1.gsh defer echo deferred-ran ; echo main-ran\r", 10);
    match run!(b"run /df1.gsh\r", 14) {
        Some(r) => check!(r.contains("main-ran") && r.contains("deferred-ran"), "gsh: defer runs at script end"),
        None => { println!("files-test: FAIL - gsh defer timeout"); fail += 1; }
    }
    // defer runs even when the script `fail`s.
    let _ = run!(b"write /df2.gsh defer echo clean-on-fail ; fail boom\r", 10);
    match run!(b"run /df2.gsh\r", 14) {
        Some(r) => check!(r.contains("clean-on-fail"), "gsh: defer runs even on fail"),
        None => { println!("files-test: FAIL - gsh defer-on-fail timeout"); fail += 1; }
    }
    // defer is function-scoped: it runs when the function returns, before the caller continues.
    let _ = run!(b"write /df3.gsh fn f { defer echo fn-cleanup ; echo in-fn } ; f ; echo after\r", 10);
    match run!(b"run /df3.gsh\r", 14) {
        Some(r) => check!(r.contains("fn-cleanup") && r.contains("after"), "gsh: defer is function-scoped (runs on return)"),
        None => { println!("files-test: FAIL - gsh defer-fn timeout"); fail += 1; }
    }
    // defers run LIFO (the last registered runs first).
    let _ = run!(b"write /df4.gsh defer echo d-one ; defer echo d-two ; echo body\r", 10);
    match run!(b"run /df4.gsh\r", 14) {
        Some(r) => {
            let lifo = match (r.find("d-two"), r.find("d-one")) { (Some(t), Some(o)) => t < o, _ => false };
            check!(lifo, "gsh: defers run LIFO");
        }
        None => { println!("files-test: FAIL - gsh defer-lifo timeout"); fail += 1; }
    }

    // ── gsh Slice 10 (Tier 2): record aggregators - count (dual) + sum/min/max/avg (§5).
    let _ = run!(b"write /agg.json '[{\"n\":\"a\",\"v\":10},{\"n\":\"b\",\"v\":20},{\"n\":\"c\",\"v\":30}]'\r", 10);
    match run!(b"read /agg.json | from json | count\r", 14) {
        Some(r) => check!(r.contains("3"), "gsh: count rows of a record stream"),
        None => { println!("files-test: FAIL - agg count timeout"); fail += 1; }
    }
    match run!(b"read /agg.json | from json | sum v\r", 14) {
        Some(r) => check!(r.contains("60"), "gsh: sum a numeric column"),
        None => { println!("files-test: FAIL - agg sum timeout"); fail += 1; }
    }
    match run!(b"read /agg.json | from json | min v\r", 14) {
        Some(r) => check!(r.contains("10"), "gsh: min of a numeric column"),
        None => { println!("files-test: FAIL - agg min timeout"); fail += 1; }
    }
    match run!(b"read /agg.json | from json | max v\r", 14) {
        Some(r) => check!(r.contains("30"), "gsh: max of a numeric column"),
        None => { println!("files-test: FAIL - agg max timeout"); fail += 1; }
    }
    match run!(b"read /agg.json | from json | avg v\r", 14) {
        Some(r) => check!(r.contains("20"), "gsh: avg of a numeric column"),
        None => { println!("files-test: FAIL - agg avg timeout"); fail += 1; }
    }
    // reducing a NON-numeric column is loud, never a silent 0.
    match run!(b"read /agg.json | from json | sum n\r", 14) {
        Some(r) => check!(r.contains("not numeric"), "gsh: sum of a non-numeric column is loud"),
        None => { println!("files-test: FAIL - agg non-numeric timeout"); fail += 1; }
    }

    // ── gsh Slice 11 (Tier 2): console input (§8). `input` reads a line; the harness feeds the
    // answer as keystrokes (input blocks, so send the run WITHOUT waiting, then send the reply).
    let _ = run!(b"write /in1.gsh let x = $(input \"Name: \") ; echo got-$x\r", 10);
    send(&mut write_half, b"run /in1.gsh\r");
    match run!(b"Alice\r", 14) {
        Some(r) => check!(r.contains("got-Alice"), "gsh: input reads a console line, captured via $( )"),
        None => { println!("files-test: FAIL - gsh input timeout"); fail += 1; }
    }
    // input secret: the value is captured but TAINTED - echoing it is refused, and the secret never
    // reaches the console (invisible entry means the typed reply is not echoed either).
    let _ = run!(b"write /in2.gsh let pw = $(input secret \"Pass: \") ; echo pw-$pw\r", 10);
    send(&mut write_half, b"run /in2.gsh\r");
    match run!(b"hunter2\r", 14) {
        Some(r) => check!(r.contains("refusing to echo a secret") && !r.contains("hunter2"),
                          "gsh: input secret taints - echo refused, value stays off the console"),
        None => { println!("files-test: FAIL - gsh input-secret timeout"); fail += 1; }
    }

    // ── assert: the verifying command. Content form (the pipe sink) is tested interactively -
    //    a `|` can't yet be authored into a script via `write` (the shell pipes the write line).
    match run!(b"roster | where role=core | assert contains Matthew\r", 16) {
        Some(r) => check!(r.contains("assert: ok"), "assert: contains holds on matching output"),
        None    => { println!("files-test: FAIL - assert contains timeout"); fail += 1; }
    }
    match run!(b"roster | where role=worker | assert contains Matthew\r", 16) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert: contains fails on non-matching output"),
        None    => { println!("files-test: FAIL - assert contains(fail) timeout"); fail += 1; }
    }
    match run!(b"roster | where role=core | assert lacks Mark\r", 16) {
        Some(r) => check!(r.contains("assert: ok"), "assert: lacks holds when text is absent"),
        None    => { println!("files-test: FAIL - assert lacks timeout"); fail += 1; }
    }
    // result form (negative tests): `fails` holds when the command errors.
    match run!(b"assert fails read /lsr/nope\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "assert: fails holds when the command errors (negative test)"),
        None    => { println!("files-test: FAIL - assert fails timeout"); fail += 1; }
    }
    match run!(b"assert ok read /lsr/big.txt\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "assert: ok holds when the command succeeds"),
        None    => { println!("files-test: FAIL - assert ok timeout"); fail += 1; }
    }
    match run!(b"assert fails read /lsr/big.txt\r", 12) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert: fails reports when a command unexpectedly succeeds"),
        None    => { println!("files-test: FAIL - assert fails(neg) timeout"); fail += 1; }
    }
    // a self-checking script: standalone asserts via `run`, aggregated. (No `|`, so it can be
    // authored with `write`.) Both hold → 0 failures.
    let _ = run!(b"write /check.gsh assert ok read /lsr/big.txt ; assert fails read /lsr/nope\r", 10);
    match run!(b"run /check.gsh\r", 16) {
        Some(r) => check!(r.contains("run: ran 2, failed 0"), "assert: a self-checking script passes (run aggregates)"),
        None    => { println!("files-test: FAIL - assert script timeout"); fail += 1; }
    }

    // ── Result model now spans the file commands + unknown-command. Exercise success and failure
    //    through `assert ok/fails` (the file ops return real Ok/Err, not Ok-wrapped).
    match run!(b"assert fails totallynotacommand\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: an unknown command is now Err (assert fails holds)"),
        None    => { println!("files-test: FAIL - unknown-cmd Err timeout"); fail += 1; }
    }
    match run!(b"assert ok mkdir /rdir\r", 10) {
        Some(r) => check!(r.contains("assert: ok") && r.contains("created /rdir"), "result: mkdir success is Ok"),
        None    => { println!("files-test: FAIL - assert ok mkdir timeout"); fail += 1; }
    }
    match run!(b"assert fails mkdir /no/such/parent/x\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: mkdir into a missing parent is Err"),
        None    => { println!("files-test: FAIL - assert fails mkdir timeout"); fail += 1; }
    }
    match run!(b"assert fails cd /nowhere\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: cd to a missing dir is Err"),
        None    => { println!("files-test: FAIL - assert fails cd timeout"); fail += 1; }
    }
    match run!(b"assert fails delete /nowhere\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: delete of a missing path is Err"),
        None    => { println!("files-test: FAIL - assert fails delete timeout"); fail += 1; }
    }
    // and `result` reflects a converted command directly.
    let _ = run!(b"ls /nowhere\r", 10);
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(FileNotFound)"), "result: ls of a missing dir → Err(FileNotFound)"),
        None    => { println!("files-test: FAIL - ls result timeout"); fail += 1; }
    }

    // ── service-control on the Result model: a protected core service is Err(Denied). ──
    match run!(b"assert fails spawn supervisor\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: spawn of a protected core service is Err (fails holds)"),
        None    => { println!("files-test: FAIL - assert fails spawn timeout"); fail += 1; }
    }
    let _ = run!(b"spawn supervisor\r", 10);
    match run!(b"result\r", 10) {
        Some(r) => check!(r.contains("Err(Denied)"), "result: spawn supervisor → Err(Denied)"),
        None    => { println!("files-test: FAIL - spawn result timeout"); fail += 1; }
    }

    // ── assert fails-with <Variant>: pin the SPECIFIC failure (precise negative test). ──
    match run!(b"assert fails-with FileNotFound read /nope\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "assert fails-with: holds on the exact variant"),
        None    => { println!("files-test: FAIL - fails-with FileNotFound timeout"); fail += 1; }
    }
    match run!(b"assert fails-with Denied spawn supervisor\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "assert fails-with: Denied for a protected spawn"),
        None    => { println!("files-test: FAIL - fails-with Denied timeout"); fail += 1; }
    }
    match run!(b"assert fails-with Denied read /nope\r", 10) {
        Some(r) => check!(r.contains("assert: FAILED"), "assert fails-with: FAILS when the variant is wrong (got FileNotFound)"),
        None    => { println!("files-test: FAIL - fails-with wrong timeout"); fail += 1; }
    }

    // ── the last stragglers (caps, drives) + info commands are now on the Result model too. ──
    match run!(b"assert fails-with FileNotFound caps nobody\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: caps of a missing service → Err(FileNotFound)"),
        None    => { println!("files-test: FAIL - caps fails-with timeout"); fail += 1; }
    }
    match run!(b"assert ok caps shell\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: caps of a live service is Ok"),
        None    => { println!("files-test: FAIL - caps ok timeout"); fail += 1; }
    }
    match run!(b"assert ok drives\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: drives list (mounted GSFS) is Ok"),
        None    => { println!("files-test: FAIL - drives ok timeout"); fail += 1; }
    }
    match run!(b"assert fails drives bogus\r", 12) {
        Some(r) => check!(r.contains("assert: ok"), "result: drives unknown subcommand is Err"),
        None    => { println!("files-test: FAIL - drives fails timeout"); fail += 1; }
    }
    match run!(b"assert ok status\r", 10) {
        Some(r) => check!(r.contains("assert: ok"), "result: an info command (status) is Ok"),
        None    => { println!("files-test: FAIL - status ok timeout"); fail += 1; }
    }

    // chaos `save`: the report is recorded in memory during the storm, then written to fs at the
    // END (catch-22-safe - block-driver is the target, not fs, so fs is free to be written).
    match run!(b"chaos kill-storm block-driver 3 save /chaos.txt\r", 30) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("report saved to /chaos.txt"),
                          "chaos: storm + save report to a file"),
        None    => { println!("files-test: FAIL - chaos save timeout"); fail += 1; }
    }
    match run!(b"read /chaos.txt\r", 10) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("recovered gen"),
                          "chaos: saved report file holds the verdict + per-round detail"),
        None    => { println!("files-test: FAIL - read chaos report timeout"); fail += 1; }
    }

    // Regression: storm `fs` and the catch-22-safe save must settle + reacquire fs THROUGH THE
    // KERNEL DIRECTORY to land the report. Then `ls /` must reacquire fs the same way (not "storage
    // unavailable"). This pins client-resolution-after-restart via the directory, the property §22
    // Test 11 covers. Generous timeout (settle + bounded save-retry on TCG).
    match run!(b"chaos kill-storm fs 2 save /fsr.txt\r", 60) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("report saved to /fsr.txt"),
                          "chaos: fs storm recovers + report saved (settle + reacquire fs via the kernel directory)"),
        None    => { println!("files-test: FAIL - chaos fs storm+save timeout"); fail += 1; }
    }
    match run!(b"read /fsr.txt\r", 10) {
        Some(r) => check!(r.contains("verdict: PASS") && r.contains("recovered gen"),
                          "chaos: fs-target report file persisted (catch-22-safe save landed)"),
        None    => { println!("files-test: FAIL - read fs chaos report timeout"); fail += 1; }
    }
    match run!(b"ls /\r", 10) {
        Some(r) => check!(!r.contains("storage unavailable"),
                          "directory: shell reacquires fs after its own restart"),
        None    => { println!("files-test: FAIL - ls after fs-storm timeout"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();
    println!("\nfiles-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// `osdev test edit` - drive the full-screen `edit` editor over the serial console end to end:
/// open a NEW file, type (with a backspace), save (^S) + quit (^Q), and `read` it back to prove
/// the bytes persisted; re-open the existing file and insert at the start; then open it, type
/// junk, ^Q and DISCARD (n) at the unsaved-changes prompt, and read to prove the junk was not
/// saved. Plus the no-arg usage. The editor's own TUI repaint is in the byte stream, but every
/// assertion is on the post-edit `read` output (captured after the prompt returns), never the
/// repaint - so we verify what actually hit the filesystem, not what was drawn.
pub fn run_edit(image_path: &Path, persist_path: &str, smp: u32) {
    println!("edit-test: booting (smp={smp}) with a RAW AHCI disk - scripted mode");

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
            if $ok { println!("edit-test: PASS - {}", $label); pass += 1; }
            else   { println!("edit-test: FAIL - {}", $label); fail += 1; }
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
        println!("edit-test: FAIL - timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // The disk is pre-formatted host-side with a large baked file (`/big.txt`, ~400 lines / several
    // IO_CHUNK windows). Confirm fs mounted it - proves the editor has a filesystem to save to and
    // gives the large-file tests their fixture.
    match read_back!(b"ls /\r") {
        Some(r) => check!(r.contains("big.txt"), "setup: pre-baked /big.txt present"),
        None    => { println!("edit-test: FAIL - ls / timeout"); fail += 1; }
    }

    // 1. no-arg usage.
    match read_back!(b"edit\r") {
        Some(r) => check!(r.contains("usage: edit"), "edit (no arg): prints usage"),
        None    => { println!("edit-test: FAIL - usage timeout"); fail += 1; }
    }

    // 2. NEW file: type (with a backspace + a newline), save (^S), quit (^Q), read back.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        check!(true, "edit /e.txt: editor opened (status bar shown)");
        // "hello worldX" then Backspace (DEL) deletes the X; Enter inserts a newline; "second line".
        send(&mut write_half, b"hello worldX\x7f\rsecond line\x13"); // ^S save
        thread::sleep(Duration::from_millis(400));                   // let the save's fs round-trip land
        send(&mut write_half, b"\x11");                              // ^Q - unmodified after save → clean quit
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10)) {
            Some(_) => check!(true, "edit /e.txt: ^S then ^Q returned to the prompt"),
            None    => { println!("edit-test: FAIL - editor did not return to prompt"); fail += 1; }
        }
    } else { println!("edit-test: FAIL - editor did not open (×2)"); fail += 2; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => {
            check!(r.contains("hello world") && r.contains("second line"), "read /e.txt: saved text present");
            check!(!r.contains("worldX"), "read /e.txt: backspace took effect (no 'worldX')");
        }
        None => { println!("edit-test: FAIL - read after edit timeout (×2)"); fail += 2; }
    }

    // 3. EXISTING file: cursor opens at the start; insert "TOP ", save, quit, read.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"TOP \x13");
        thread::sleep(Duration::from_millis(400));
        send(&mut write_half, b"\x11");
        let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(10));
    } else { println!("edit-test: FAIL - re-open editor"); fail += 1; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => check!(r.contains("TOP hello world"), "edit existing: insert-at-start saved ('TOP hello world')"),
        None    => { println!("edit-test: FAIL - read after edit-existing timeout"); fail += 1; }
    }

    // 4. Quit with unsaved changes, DISCARD (n) - the junk must NOT persist.
    send(&mut write_half, b"edit /e.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"ZZZJUNK\x11"); // type junk (now modified), then ^Q → prompt
        if collect_until(&buf, &mut cursor, b"discard", Duration::from_secs(6)).is_some() {
            check!(true, "edit: ^Q with unsaved changes shows the discard prompt");
            send(&mut write_half, b"n"); // n = discard & quit
            let _ = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(8));
        } else { println!("edit-test: FAIL - no discard prompt"); fail += 1; }
    } else { println!("edit-test: FAIL - re-open editor for discard"); fail += 1; }
    match read_back!(b"read /e.txt\r") {
        Some(r) => check!(r.contains("TOP hello world") && !r.contains("ZZZJUNK"), "edit discard: junk NOT saved (file unchanged)"),
        None    => { println!("edit-test: FAIL - read after discard timeout"); fail += 1; }
    }

    // ── Large file (piece-table windowed load + streaming save) ─────────────────────────────────
    // The editor is DRIVEN over serial; the result is verified on the DISK afterwards (a 16 KiB
    // file dumped back over the console renders ~400 lines on the fbcon - far too slow under TCG -
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
            None    => { println!("edit-test: FAIL - big.txt edit did not return"); fail += 1; }
        }
    } else { println!("edit-test: FAIL - big.txt did not open (×2)"); fail += 2; }

    // 6. Re-open /big.txt, PageDown into the file (windowed navigation past the first window), type
    //    a mid-file marker, save, quit. Exercises an edit that isn't in window 0.
    send(&mut write_half, b"edit /big.txt\r");
    if collect_until(&buf, &mut cursor, b"Ctrl-S save", Duration::from_secs(12)).is_some() {
        send(&mut write_half, b"\x1b[6~");            // PageDown (one screen down - past row 0)
        thread::sleep(Duration::from_millis(200));
        send(&mut write_half, b"MID \x13");           // insert mid-file marker, ^S
        thread::sleep(Duration::from_millis(1200));
        send(&mut write_half, b"\x11");               // ^Q
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(12)) {
            Some(_) => check!(true, "edit /big.txt: mid-file edit saved + returned to prompt"),
            None    => { println!("edit-test: FAIL - big.txt mid-edit did not return"); fail += 1; }
        }
    } else { println!("edit-test: FAIL - re-open big.txt for mid-edit"); fail += 1; }

    child.kill().ok();
    child.wait().ok();

    // Verify the SAVED bytes on the disk (the editor's actual output). Parse the GSFS root for
    // /big.txt, read its data region, and assert both edits landed and the far tail survived the
    // streaming save - windowed load + multi-chunk save proven end to end, without rendering 16 KiB.
    match gsfs_read_file(persist_path, b"big.txt") {
        Some(content) => {
            check!(content.starts_with(b"AAA EDITLINE 0000"),
                   "big.txt (disk): start-edit at offset 0 ('AAA EDITLINE 0000')");
            check!(window_find(&content, b"EDITLINE 0399").is_some(),
                   "big.txt (disk): windowed original tail preserved ('EDITLINE 0399')");
            check!(window_find(&content, b"MID ").is_some(),
                   "big.txt (disk): mid-file edit present ('MID ')");
        }
        None => { println!("edit-test: FAIL - could not read /big.txt back from the disk (×3)"); fail += 3; }
    }

    println!("\nedit-test: {pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
}

/// §22 Test 13 - **fs survives its own restart** (Phase D). Drive the shell on COM1 to write a
/// file, KILL `fs` over the COM2 control channel, then read the file back: the supervisor
/// respawns `fs`, `fs` re-mounts (the data persisted on disk), and the shell reacquires a fresh
/// `fs` cap by name (§14.3) - the file reads back, the kernel never panics. This is
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
        "-serial",  &format!("tcp::{ctrl_port},server,nowait"),   // COM2: control channel - nowait (we connect later)
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
        if $ok { println!("fs-restart: PASS - {}", $label); pass += 1; }
        else   { println!("fs-restart: FAIL - {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{
        send(&mut write_half, $c);
        collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs))
    }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-restart: FAIL - timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // Flash the disk, then write a file and read it back (pre-restart baseline).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
            Some(r) => check!(r.contains("formatted as GSFS"), "setup: flashed GSFS"),
            None    => { println!("fs-restart: FAIL - flash timeout"); fail += 1; }
        }
    } else { println!("fs-restart: FAIL - no flash confirm"); fail += 1; }
    match run!(b"write /t.txt survives-restart\r", 10) {
        Some(r) => check!(r.contains("wrote /t.txt"), "wrote /t.txt before restart"),
        None    => { println!("fs-restart: FAIL - write timeout"); fail += 1; }
    }
    match run!(b"read /t.txt\r", 10) {
        Some(r) => check!(r.contains("survives-restart"), "read /t.txt before restart"),
        None    => { println!("fs-restart: FAIL - read timeout"); fail += 1; }
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
        None => { println!("fs-restart: FAIL - could not connect to control port"); fail += 1; }
    }

    // Read the file back: the shell must reacquire a fresh fs cap by name, and the
    // file must still be there (persisted on disk, recovered on remount). The headline check.
    // Retry a couple of times - reacquire returns None until fs has finished re-registering.
    let mut got = false;
    for _ in 0..4 {
        if let Some(r) = run!(b"read /t.txt\r", 10) {
            if r.contains("survives-restart") { got = true; break; }
        }
        thread::sleep(Duration::from_millis(500));
    }
    check!(got, "read /t.txt AFTER restart (shell reacquired fs, file persisted)");

    // ── Shell restartable ("nothing escapes"): KILL the shell itself over the control channel. The
    // shell is the user's interface - pre-Phase-this it was the one service that stayed dead forever
    // if killed (ensure_-wired + unwatched). Now the kernel notifies the supervisor of its death and
    // the supervisor respawns a FRESH prompt; the new shell must answer input (the session recovers,
    // the in-flight command is lost - a re-init, not a resume, §14.2/§25). Invariant 6.
    println!("fs-restart: sending 'KILL shell' over the control channel …");
    match retry_tcp_connect(ctrl_port, Duration::from_secs(10)) {
        Some(mut ctrl) => { thread::sleep(Duration::from_millis(100)); send(&mut ctrl, b"\nKILL shell\n");
            let restarted = collect_until(&buf, &mut cursor, b"supervisor: shell restarted", Duration::from_secs(20));
            check!(restarted.is_some(), "supervisor observed shell death and restarted it");
            let ready = collect_until(&buf, &mut cursor, b"shell: ready", Duration::from_secs(20));
            check!(ready.is_some(), "a fresh shell prompt came up after the kill");
            drop(ctrl);
        }
        None => { println!("fs-restart: FAIL - could not connect to control port (shell kill)"); fail += 1; }
    }
    // The FRESH shell answers commands - proof the session recovered, not just a log line.
    let mut answered = false;
    for _ in 0..4 {
        if let Some(r) = run!(b"cores\r", 10) {
            if r.contains("cores:") { answered = true; break; }
        }
        thread::sleep(Duration::from_millis(500));
    }
    check!(answered, "the restarted shell answers commands (session recovered)");

    // ── Directly-restartable driver/logger: kill `logger` over the control channel and confirm the
    // supervisor respawns it on its OWN death (not only via a lucky supervisor respawn). This is the
    // fix that keeps `chaos max-carnage` from leaving `xhci`/`ehci`/`logger` dead at the end of a run.
    // logger is the hardware-independent stand-in for xhci/ehci (same death-notification + restart
    // arm); the actual USB keyboard recovery is verified on real hardware (QEMU has no USB keyboard).
    println!("fs-restart: sending 'KILL logger' over the control channel …");
    match retry_tcp_connect(ctrl_port, Duration::from_secs(10)) {
        Some(mut ctrl) => { thread::sleep(Duration::from_millis(100)); send(&mut ctrl, b"\nKILL logger\n");
            let restarted = collect_until(&buf, &mut cursor, b"supervisor: logger restarted", Duration::from_secs(20));
            check!(restarted.is_some(), "supervisor respawned logger on its own death (directly restartable)");
            drop(ctrl);
        }
        None => { println!("fs-restart: FAIL - could not connect to control port (logger kill)"); fail += 1; }
    }

    // No panic anywhere in the whole session.
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "kernel never panicked across the restart");

    child.kill().ok();
    child.wait().ok();
    println!("\nfs-restart: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// `examples/counter` survives its OWN restart (§14 restart, §15 persistence). Bare-metal shell +
/// AHCI disk + the `counter` service (counter-test build). Flash the disk so `fs` mounts, let
/// `counter` persist a couple of increments to /counter.dat, KILL counter over the control channel,
/// and - after the supervisor respawns it - assert the fresh instance RECOVERED a non-zero count
/// from the file (not "starting at 0"). That single assertion is the proof the state survived.
pub fn run_counter(image_path: &Path, persist_path: &str, smp: u32) {
    let sc = crate::qemu::timeout_scale();
    println!("counter: booting (smp={smp}) bare-metal + AHCI disk + counter; shell on COM1, control on COM2");
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
        "-serial",  &format!("tcp::{ctrl_port},server,nowait"),   // COM2: control channel - nowait (we connect later)
        "-display", "none", "-no-reboot", "-no-shutdown",
    ])
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("counter: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("counter: could not connect to shell serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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
        if $ok { println!("counter: PASS - {}", $label); pass += 1; }
        else   { println!("counter: FAIL - {}", $label); fail += 1; }
    }; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40 * sc)).is_none() {
        println!("counter: FAIL - timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // Format the data disk so `fs` mounts a filesystem - counter's saves succeed only once one exists
    // (before this they fail loudly with FS_NOFS and the count lives only in RAM).
    send(&mut write_half, b"drives flash data\r");
    if collect_until(&buf, &mut cursor, b"[y/N]", Duration::from_secs(10 * sc)).is_some() {
        send(&mut write_half, b"y\r");
        match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20 * sc)) {
            Some(r) => check!(r.contains("formatted as GSFS"), "setup: flashed GSFS"),
            None    => { println!("counter: FAIL - flash timeout"); fail += 1; }
        }
    } else { println!("counter: FAIL - no flash confirm"); fail += 1; }

    // Let counter persist at least one increment. The first "counter: count=N saved" line AFTER the
    // format is the first SUCCESSFUL save (N >= 1) - pre-flash attempts logged "(save failed …)".
    let saved_n = collect_until(&buf, &mut cursor, b" saved", Duration::from_secs(40 * sc))
        .and_then(|chunk| digits_after(&chunk, "counter: count="));
    check!(matches!(saved_n, Some(n) if n >= 1), "counter persisted an increment to /counter.dat (count >= 1)");
    // Wait for a second successful save so the durable value is solidly > 0 before we kill.
    let _ = collect_until(&buf, &mut cursor, b" saved", Duration::from_secs(20 * sc));

    // KILL counter over the COM2 control channel; the supervisor must observe the death and respawn it.
    println!("counter: sending 'KILL counter' over the control channel …");
    match retry_tcp_connect(ctrl_port, Duration::from_secs(10)) {
        Some(mut ctrl) => { thread::sleep(Duration::from_millis(100)); send(&mut ctrl, b"\nKILL counter\n");
            let restarted = collect_until(&buf, &mut cursor, b"supervisor: counter restarted", Duration::from_secs(20 * sc));
            check!(restarted.is_some(), "supervisor observed counter death and restarted it");
            drop(ctrl);
        }
        None => { println!("counter: FAIL - could not connect to control port"); fail += 1; }
    }

    // THE PROOF: the respawned counter reconstructs its count from /counter.dat - it logs
    // "counter: recovered count=M from /counter.dat" with M > 0, NOT "starting at 0". The persisted
    // state survived a kill + respawn, which is the whole point of examples/counter (§14/§15).
    let recovered = collect_until(&buf, &mut cursor, b" from /counter.dat", Duration::from_secs(30 * sc))
        .and_then(|chunk| digits_after(&chunk, "counter: recovered count="));
    check!(matches!(recovered, Some(m) if m >= 1),
           "respawned counter RECOVERED a non-zero count from fs (survived its own restart)");
    if let Some(m) = recovered {
        println!("counter:   recovered count = {m} (the persisted value survived the restart)");
    }

    // No panic anywhere in the whole session.
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "kernel never panicked across the restart");

    // Always dump the raw serial for inspection (counter + shell share COM1).
    let _ = std::fs::write("build/tests/counter_serial.log", whole.as_bytes());

    child.kill().ok();
    child.wait().ok();
    println!("\ncounter: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Parse the unsigned integer immediately following the LAST occurrence of `marker` in `text` (a
/// serial chunk may hold several lines; the last is the most recent). None if marker/digits absent.
/// Reads counter's persisted/recovered count out of a "counter: …=N …" line.
fn digits_after(text: &str, marker: &str) -> Option<u64> {
    let start = text.rfind(marker)? + marker.len();
    let digits: String = text[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// `examples/reply-server` exercised by `examples/asker` - the request/reply (RPC) round-trip (§8,
/// §8.9). Boots the bare-metal set + reply-server + asker (reply-test build). The pair runs
/// autonomously: asker sends reply-server a request carrying an embedded REPLY capability,
/// reply-server replies over that cap, and asker checks that the reply echoes the exact request it
/// sent. No disk and no control channel - we only read COM1 and assert the round-trip happened.
///
/// THE PROOF is the line `asker: reply = <N> (echo OK)`: asker logs it only when a request it sent
/// (with an embedded reply cap) came back from reply-server with the identical payload - i.e. the
/// request reached the server AND its reply reached the client over the embedded cap. We also assert
/// the server logged `reply-server: replied to a request`, and that the kernel never panicked.
pub fn run_reply_server(image_path: &Path, smp: u32) {
    let sc = crate::qemu::timeout_scale();
    println!("reply-server: booting (smp={smp}) bare-metal + reply-server + asker; serial on COM1");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),   // COM1: shell I/O + logs (QEMU waits for us)
        "-serial",  "null",                                  // COM2: unused in this test
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("reply-server: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("reply-server: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half = stream.try_clone().expect("clone tcp stream");
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
        if $ok { println!("reply-server: PASS - {}", $label); pass += 1; } else { println!("reply-server: FAIL - {}", $label); fail += 1; }
    }; }

    // The reply-server comes up and parks on recv() (idle) until asker sends it a request.
    check!(collect_until(&buf, &mut cursor, b"reply-server: ready", Duration::from_secs(60 * sc)).is_some(),
           "reply-server came up and owns its endpoint");

    // THE PROOF: asker sent a request carrying an embedded reply cap, reply-server replied over it,
    // and the reply echoed the EXACT request payload - asker logs "(echo OK)" only then. One line is
    // the whole round-trip (request reached the server AND its reply reached the client back).
    let echo = collect_until(&buf, &mut cursor, b"(echo OK)", Duration::from_secs(60 * sc));
    check!(echo.as_deref().map_or(false, |c| c.contains("asker: reply =")),
           "asker got the reply back and it echoed the request (round-trip closed)");

    // And the server logged that it answered a request over the embedded reply cap - the other half.
    check!(collect_until(&buf, &mut cursor, b"reply-server: replied to a request", Duration::from_secs(20 * sc)).is_some(),
           "reply-server replied to a request over the client's embedded cap");

    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic across the round-trip");
    let _ = std::fs::write("build/tests/reply_server_serial.log", whole.as_bytes());

    child.kill().ok(); child.wait().ok();
    println!("\nreply-server: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Reply-side death-wake: a caller blocked awaiting a reply wakes with `ReplyDead`, it does NOT hang
/// (§8.6, Commandment VIII; `sdk/rust/src/service_context.rs::request_with_reply` -> kernel syscall 41
/// `Call`). Reuses the reply-test build (bare-metal set + reply-server + asker), COM1 for logs, COM2
/// for the control channel. `asker` (see examples/asker) sends one request the server never answers
/// (b"HANG") and blocks for the reply; we then KILL `reply-server` over COM2. Before this change the
/// blocked `recv` would hang forever; now the kernel finds the outstanding reply cap of the blocked
/// caller in the endpoint-death path and wakes it with `ReplyDead`, so `request_with_reply` returns
/// None and `asker` carries on.
///
/// THE PROOF is asker logging `HANG woke with no reply ... (ReplyDead recovered)` AFTER the server was
/// killed while it was demonstrably blocked (server logged it withheld the reply first). No hang, no
/// panic. This is the reply-side twin of §22 Test 4 (a blocked *sender* wakes with `EndpointDead`).
pub fn run_reply_dead(image_path: &Path, smp: u32) {
    let sc = crate::qemu::timeout_scale();
    println!("reply-dead: booting (smp={smp}) bare-metal + reply-server + asker; logs on COM1, control on COM2");
    let qemu       = crate::qemu::qemu_binary();
    let image_str  = image_path.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();
    let ctrl_port  = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),         // COM1: logs (QEMU waits for us)
        "-serial",  &format!("tcp::{ctrl_port},server,nowait"),   // COM2: control channel (connect later)
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("reply-dead: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("reply-dead: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half = stream.try_clone().expect("clone tcp stream");
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
        if $ok { println!("reply-dead: PASS - {}", $label); pass += 1; } else { println!("reply-dead: FAIL - {}", $label); fail += 1; }
    }; }

    // 1. The round-trip works first (asker got a real echo back) - the happy path is intact.
    check!(collect_until(&buf, &mut cursor, b"(echo OK)", Duration::from_secs(90 * sc)).is_some(),
           "asker got a normal reply back (Call happy path works)");

    // 2. asker sends the request the server never answers and blocks for the reply.
    check!(collect_until(&buf, &mut cursor, b"asker: sending HANG", Duration::from_secs(30 * sc)).is_some(),
           "asker sent the HANG request and is blocking for a reply");
    // 3. The server confirms it received the request but is withholding the reply - so asker is
    //    genuinely blocked awaiting a reply that will never come (the exact state that used to hang).
    check!(collect_until(&buf, &mut cursor, b"reply-server: HANG received", Duration::from_secs(30 * sc)).is_some(),
           "reply-server received the request and withheld its reply (asker now blocked)");

    // 4. KILL reply-server over the COM2 control channel while asker is blocked awaiting its reply.
    println!("reply-dead: sending 'KILL reply-server' over the control channel …");
    match retry_tcp_connect(ctrl_port, Duration::from_secs(10)) {
        Some(mut ctrl) => {
            thread::sleep(Duration::from_millis(100));
            send(&mut ctrl, b"\nKILL reply-server\n");
            // THE PROOF: the kernel found the blocked caller's outstanding reply cap in the
            // endpoint-death path and woke it with ReplyDead, so request_with_reply returned None and
            // asker logged its recovery - instead of hanging forever (the pre-change behaviour).
            let woke = collect_until(&buf, &mut cursor, b"asker: HANG woke with no reply", Duration::from_secs(30 * sc));
            check!(woke.is_some(), "asker woke with ReplyDead (blocked caller did NOT hang on peer death)");
            drop(ctrl);
        }
        None => { println!("reply-dead: FAIL - could not connect to control port"); fail += 1; }
    }

    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic across the reply-side death-wake");
    let _ = std::fs::write("build/tests/reply_dead_serial.log", whole.as_bytes());

    child.kill().ok(); child.wait().ok();
    println!("\nreply-dead: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// `examples/resource-server` exercised by `examples/holder` - delegated resource capabilities
/// (§7.10, P2 file-as-capability). Boots the bare-metal set + resource-server + holder (resource-test
/// build). The pair runs autonomously: resource-server MINTs a resource it owns, narrows a READ-ONLY
/// copy of the cap, and GRANTs it to holder; holder then proves the three §7.3 properties that make a
/// delegated resource cap a GENUINE capability. No disk and no control channel - we only read COM1 and
/// assert holder's three receipts.
///
/// THE THREE PROOFS, all from holder's serial log:
///   1. USE            - `holder: read OK`                       (the cap is usable for what it permits)
///   2. NON-ESCALATION - `holder: write denied (non-escalation)` (a READ-ONLY cap cannot WRITE, §7.3)
///   3. REVOCABLE      - `holder: revoked (CapRevoked)`          (after the owner revokes, the next use
///                                                                is stale, §7.5)
/// Plus: resource-server came up and granted the cap, and the kernel never panicked.
pub fn run_resource_server(image_path: &Path, smp: u32) {
    let sc = crate::qemu::timeout_scale();
    println!("resource-server: booting (smp={smp}) bare-metal + resource-server + holder; serial on COM1");
    let qemu      = crate::qemu::qemu_binary();
    let image_str = image_path.to_string_lossy().replace('\\', "/");
    let shell_port = pick_free_port();

    let mut cmd = std::process::Command::new(&qemu);
    cmd.args([
        "-drive",   &format!("format=raw,file={image_str},if=ide"),
        "-smp",     &smp.to_string(), "-m", "512M",
        "-serial",  &format!("tcp::{shell_port},server"),   // COM1: shell I/O + logs (QEMU waits for us)
        "-serial",  "null",                                  // COM2: unused in this test
        "-display", "none", "-no-reboot", "-no-shutdown",
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("resource-server: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("resource-server: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half = stream.try_clone().expect("clone tcp stream");
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
        if $ok { println!("resource-server: PASS - {}", $label); pass += 1; } else { println!("resource-server: FAIL - {}", $label); fail += 1; }
    }; }

    // The owner comes up, mints a resource it owns, and GRANTs holder a READ-ONLY copy of the cap.
    check!(collect_until(&buf, &mut cursor, b"resource-server: granted a resource cap to holder", Duration::from_secs(90 * sc)).is_some(),
           "resource-server minted a resource and granted holder a (read-only) cap to it");

    // PROOF 1 - USE: holder invoked the cap (READ) and the owner served it. A real cap is usable for
    // what it permits.
    check!(collect_until(&buf, &mut cursor, b"holder: read OK", Duration::from_secs(60 * sc)).is_some(),
           "holder USED the granted cap (read OK)");

    // PROOF 2 - NON-ESCALATION: holder invoked WRITE on its READ-ONLY cap; the KERNEL refused it
    // (CapInsufficientRights). Rights cannot widen on transfer (§7.3) - the cap is read-only, mechanically.
    check!(collect_until(&buf, &mut cursor, b"holder: write denied (non-escalation)", Duration::from_secs(30 * sc)).is_some(),
           "NON-ESCALATION: a READ-ONLY cap was denied a WRITE (§7.3)");

    // PROOF 3 - REVOCABLE: the owner revoked the resource (a generation bump), so holder's next use is
    // stale and returns CapRevoked (§7.5). The same mechanism fs uses to revoke a file cap on delete.
    check!(collect_until(&buf, &mut cursor, b"holder: revoked (CapRevoked)", Duration::from_secs(30 * sc)).is_some(),
           "REVOCABLE: after the owner revoked, the next use returned CapRevoked (§7.5)");

    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic across mint / use / revoke");
    let _ = std::fs::write("build/tests/resource_server_serial.log", whole.as_bytes());

    child.kill().ok(); child.wait().ok();
    println!("\nresource-server: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// §22 Test 14 - file-as-capability (P2). Boot a pre-formatted disk, then run the shell's argless
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
        if $ok { println!("file-cap: PASS - {}", $label); pass += 1; } else { println!("file-cap: FAIL - {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("file-cap: FAIL - timed out waiting for first gsh>");
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
        None => { println!("file-cap: FAIL - fcap timed out"); fail += 1; }
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
        if $ok { println!("fs-check: PASS - {}", $label); pass += 1; } else { println!("fs-check: FAIL - {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-check: FAIL - timed out waiting for first gsh>");
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
        None => { println!("fs-check: FAIL - drives check timeout"); fail += 1; }
    }
    // The baked file survived the repair.
    match run!(b"read /alpha.txt\r", 10) {
        Some(r) => check!(r.contains("alpha-payload"), "baked file still reads back after check"),
        None    => { println!("fs-check: FAIL - read timeout"); fail += 1; }
    }
    let whole = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    check!(!whole.contains("KERNEL PANIC"), "no kernel panic");

    child.kill().ok(); child.wait().ok();
    println!("\nfs-check: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// `drives scrub` (Phase K): boot a PRE-FORMATTED disk holding a clean file and a file with a
/// CORRUPTED data block. `drives scrub` must report `1 bad` without panicking, leave the disk
/// UNCHANGED (a second scrub still reports `1 bad` - read-only, no repair), and the clean file
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
        if $ok { println!("fs-scrub: PASS - {}", $label); pass += 1; } else { println!("fs-scrub: FAIL - {}", $label); fail += 1; }
    }; }
    macro_rules! run { ($c:expr, $secs:expr) => {{ send(&mut write_half, $c); collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs($secs)) }}; }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fs-scrub: FAIL - timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // First scrub: must detect the one corrupt file (read-only - reports, does not repair).
    match run!(b"drives scrub\r", 15) {
        Some(r) => {
            check!(r.contains("verified") && r.contains("bad"), "scrub ran and reported a block count");
            check!(r.contains("1 bad"), "scrub detected the 1 corrupt file");
            check!(r.contains("WARNING") && r.contains("bit-rot"), "scrub warned loudly about the bad block");
        }
        None => { println!("fs-scrub: FAIL - drives scrub timeout"); fail += 1; }
    }
    // Second scrub: identical result proves scrub is READ-ONLY (it did not repair the block).
    match run!(b"drives scrub\r", 15) {
        Some(r) => check!(r.contains("1 bad"), "second scrub still reports 1 bad (read-only, nothing repaired)"),
        None    => { println!("fs-scrub: FAIL - second drives scrub timeout"); fail += 1; }
    }
    // The clean file is untouched by the scrub.
    match run!(b"read /good.txt\r", 10) {
        Some(r) => check!(r.contains("good-payload-survives-the-scrub"), "clean file still reads back after scrub"),
        None    => { println!("fs-scrub: FAIL - read timeout"); fail += 1; }
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
        if $ok { println!("fs-compat: PASS - {}", $label); pass += 1; } else { println!("fs-compat: FAIL - {}", $label); fail += 1; }
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
    println!("fs-compat: boot 1 - disk with an unknown INCOMPAT feature (must refuse to mount), ~40s …");
    let (o1, log1) = boot(disk_incompat, &["read /baked.txt"]);
    check!(log1.contains("incompatible features"), "incompat: mount refused loudly (incompatible features)");
    check!(log1.contains("no filesystem") || log1.contains("awaiting drives flash"), "incompat: reported no usable filesystem");
    check!(!o1.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "incompat: file is NOT readable (did not mount)");
    check!(!log1.contains("KERNEL PANIC"), "incompat: no kernel panic");

    // ── Scenario 2: unknown RO_COMPAT bit → mount read-only ──
    println!("fs-compat: boot 2 - disk with an unknown RO_COMPAT feature (must mount READ-ONLY), ~40s …");
    let (o2, log2) = boot(disk_ro, &["read /baked.txt", "write /x.txt should-fail"]);
    check!(log2.contains("READ-ONLY"), "ro_compat: mounted read-only (loud)");
    check!(o2.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "ro_compat: reads still work (baked file readable)");
    check!(!o2.get(1).map(|s| s.contains("wrote") || s.contains("ok")).unwrap_or(false), "ro_compat: write was refused");
    check!(!log2.contains("KERNEL PANIC"), "ro_compat: no kernel panic");

    // ── Scenario 3: unknown COMPAT bit → mount normally, read-write ──
    println!("fs-compat: boot 3 - disk with an unknown COMPAT feature (must mount read-write), ~40s …");
    let (o3, log3) = boot(disk_compat, &["read /baked.txt", "write /x.txt hello-compat", "read /x.txt"]);
    check!(!log3.contains("READ-ONLY") && !log3.contains("incompatible"), "compat: mounted normally (not read-only, not refused)");
    check!(o3.get(0).map(|s| s.contains("baked-payload")).unwrap_or(false), "compat: baked file readable");
    check!(o3.get(2).map(|s| s.contains("hello-compat")).unwrap_or(false), "compat: write+read-back works (read-write)");
    check!(!log3.contains("KERNEL PANIC"), "compat: no kernel panic");

    println!("\nfs-compat: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Boot bare-metal with a GSFS disk that has a self-checking `.gsh` baked in (host-side), then
/// `run /<script_name>` - proving the flash-and-run loop and piped asserts in a script.
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
        println!("script-test: FAIL - timed out waiting for first gsh>\n{got}");
        child.kill().ok(); child.wait().ok();
        std::process::exit(1);
    }

    // Run the baked suite. The disk is GSFS (baked host-side), so the OS mounts it on boot and
    // /<script_name> is present - no on-device authoring.
    send(&mut write_half, format!("run /{script_name}\r").as_bytes());
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => {
            println!("\n=== baked suite transcript ===\n{}\n=== end ===", r.trim());
            // Green iff the run summary is "failed 0" AND no assert printed a FAILED line.
            if r.contains("failed 0") && !r.contains("FAILED") {
                println!("script-test: PASS - baked suite ran green (failed 0)"); pass += 1;
            } else {
                println!("script-test: FAIL - baked suite not green"); fail += 1;
            }
        }
        None => { println!("script-test: FAIL - `run /{script_name}` timed out"); fail += 1; }
    }

    // `run … save <path>` - the orchestrator writes its OWN report to a file (direct, NOT a pipe),
    // so it can save while running its own inner pipelines (incl. `… | assert`, the heavy case)
    // WITHOUT the nested-capture stack overflow that `<orchestrator> | write` causes. Proves: no
    // crash/refusal, and the report file holds the tally.
    send(&mut write_half, format!("run /{script_name} save /report.txt\r").as_bytes());
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)) {
        Some(r) => if r.contains("saved report") && !r.contains("cannot start a pipe") && !r.contains("PUSER") {
            println!("script-test: PASS - run save: orchestrator wrote its report file (no crash)"); pass += 1;
        } else {
            println!("script-test: FAIL - run save did not write the report (refused/crashed?)"); fail += 1;
        },
        None    => { println!("script-test: FAIL - `run … save` timed out (stack overflow?)"); fail += 1; }
    }
    send(&mut write_half, b"read /report.txt\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)) {
        Some(r) => if r.contains("ran ") && r.contains("failed 0") {
            println!("script-test: PASS - run save: report file holds the tally (ran N, failed 0)"); pass += 1;
        } else {
            println!("script-test: FAIL - report file missing the tally"); fail += 1;
        },
        None    => { println!("script-test: FAIL - read /report.txt timed out"); fail += 1; }
    }

    // Embed-and-autoprovision: `selfcheck` runs the shell-embedded extensive suite IN MEMORY
    // (no host bake) - the one-USB hardware path where the operator flashes only os.img,
    // `drives flash`es the SSD, then types `selfcheck`. The big suite + many service spawns
    // take a while under TCG, so allow a generous wall-clock window.
    send(&mut write_half, b"selfcheck\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(150)) {
        Some(r) => {
            // Always save the full live transcript so the run can be inspected line-by-line.
            let _ = std::fs::write("build/selfcheck-transcript.txt", r.as_bytes());
            if r.contains("failed 0") && !r.contains("FAILED") {
                println!("script-test: PASS - embedded `selfcheck` ran green (failed 0) - transcript -> build/selfcheck-transcript.txt"); pass += 1;
            } else {
                println!("\n=== selfcheck transcript ===\n{}\n=== end ===", r.trim());
                println!("script-test: FAIL - embedded `selfcheck` not green"); fail += 1;
            }
        }
        None => { println!("script-test: FAIL - `selfcheck` timed out"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();
    println!("\nscript-test: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Boot bare-metal with a 10 MB `.gsh` baked in and `run` it - proving the run-from-file BOUND
/// (SCRIPT_MAX): the streaming minifier reads ~7 KiB of CODE and truncates LOUDLY, the complex tour
/// still runs, and the kernel never panics or OOMs on a 10 MB file (§26.6.1, docs/scripting.md §9).
pub fn run_big_script(image_path: &Path, disk_path: &str, script_name: &str, smp: u32) {
    println!("big-script: booting (smp={smp}) with a 10 MB GSFS-baked .gsh");
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
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("big-script: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("big-script: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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

    let mut pass = 0usize;
    let mut fail = 0usize;
    let mut cursor = 0usize;
    macro_rules! check {
        ($ok:expr, $label:expr) => {
            if $ok { println!("big-script: PASS - {}", $label); pass += 1; }
            else   { println!("big-script: FAIL - {}", $label); fail += 1; }
        };
    }

    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("big-script: FAIL - timed out waiting for first gsh>");
        child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    // Run the 10 MB script. It loads BOUNDED (~7 KiB), truncates loudly, runs the tour + some blocks.
    send(&mut write_half, format!("run /{script_name}\r").as_bytes());
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(120)) {
        Some(r) => {
            let _ = std::fs::write("build/big-script-transcript.txt", r.as_bytes()); // full transcript for inspection
            let tail: String = r.lines().rev().take(50).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            println!("\n=== 10 MB run transcript (last 50 lines; full -> build/big-script-transcript.txt) ===\n{}\n=== end ===", tail.trim());
            check!(r.contains("===TEN-MEG-STRESS-BEGIN==="), "the run started (10 MB script loaded, not rejected)");
            check!(r.contains("===TEN-MEG-TOUR-DONE==="), "the complex feature-tour ran to completion (all features green)");
            check!(r.contains("truncated") || r.contains("CODE exceeds"), "the 10 MB script truncated LOUDLY at SCRIPT_MAX");
            check!(r.contains("blk-"), "dynamic blocks ran past the tour, up to the truncation point");
            check!(!r.contains("KERNEL PANIC"), "no kernel panic on a 10 MB file");
        }
        None => { println!("big-script: FAIL - `run /{script_name}` timed out (hang on 10 MB?)"); fail += 1; }
    }

    // The shell SURVIVED - it still answers after loading + truncating a 10 MB file (no wedge/OOM).
    send(&mut write_half, b"echo STILL-ALIVE\r");
    match collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(15)) {
        Some(r) => check!(r.contains("STILL-ALIVE"), "shell is responsive after the 10 MB run"),
        None => { println!("big-script: FAIL - shell unresponsive after the 10 MB run"); fail += 1; }
    }

    child.kill().ok();
    child.wait().ok();
    println!("\nbig-script: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Boot bare-metal with a jarring `jar.gsh` + a 10 MB `huge_fmt.gsh` baked in, run `fmt` on each,
/// and capture the REAL before/after (proving the formatter on-device) plus the 10 MB guardrail.
pub fn run_fmt_demo(image_path: &Path, disk_path: &str, smp: u32) {
    println!("fmt-demo: booting (smp={smp}) with jar.gsh + a 10 MB huge_fmt.gsh");
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
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());

    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fmt-demo: QEMU launch failed at {qemu}: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s,
        None => { eprintln!("fmt-demo: could not connect to serial {shell_port}"); child.kill().ok(); std::process::exit(1); }
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
        if $ok { println!("fmt-demo: PASS - {}", $label); pass += 1; } else { println!("fmt-demo: FAIL - {}", $label); fail += 1; }
    }; }
    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fmt-demo: FAIL - timed out waiting for first gsh>"); child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }

    send(&mut write_half, b"read /jar.gsh\r");
    let before = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)).unwrap_or_default();
    send(&mut write_half, b"fmt /jar.gsh\r");
    let _fmtres = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)).unwrap_or_default();
    send(&mut write_half, b"read /jar.gsh\r");
    let after  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)).unwrap_or_default();
    send(&mut write_half, b"fmt check /jar.gsh\r"); // idempotency: canonical after fmt
    let again  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(20)).unwrap_or_default();
    // Medium file: formats past one write buffer (multiple block-aligned flushes) - the case the tiny
    // files miss. Prove via fmt check -> result Ok. Fast, so it catches the alignment bug quickly.
    send(&mut write_half, b"fmt /med.gsh\r");
    let _med    = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(120)).unwrap_or_default();
    send(&mut write_half, b"fmt check /med.gsh\r");
    let medchk  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(120)).unwrap_or_default();
    send(&mut write_half, b"result\r");
    let medres  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(30)).unwrap_or_default();
    send(&mut write_half, b"fmt /huge_fmt.gsh\r"); // 10 MB: STREAMED format, NO size cap (slow in TCG)
    let huge   = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(600)).unwrap_or_default();
    // Prove it FORMATTED, positively and race-free: `fmt check` on the result, then `result` must be
    // `Ok` (a fmt that failed or didn't run would leave the file jarring -> `fmt check` = Err). An
    // empty/timed-out capture yields no "Ok", so this can't false-pass.
    send(&mut write_half, b"fmt check /huge_fmt.gsh\r");
    let hugechk = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(600)).unwrap_or_default();
    send(&mut write_half, b"result\r");
    let chkres = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(60)).unwrap_or_default();

    let _ = std::fs::write("build/fmt-before.txt", before.as_bytes());
    let _ = std::fs::write("build/fmt-after.txt", after.as_bytes());
    println!("\n========== BEFORE (read /jar.gsh) ==========\n{}", before.trim());
    println!("\n========== AFTER (read /jar.gsh) ==========\n{}", after.trim());
    println!("\n---------- 10 MB: fmt then fmt check ----------\n{}\n{}\nresult: {}\n==========", huge.trim(), hugechk.trim(), chkres.trim());

    check!(after.contains("if $n > 5 {") && after.contains("    echo big") && after.contains("} else {"),
           "jar.gsh reformatted to canonical layout (4-space indent, one/line, K&R braces)");
    check!(!before.contains("    echo big"), "the before was genuinely jarring (inline blocks, not indented)");
    check!(!again.contains("not canonical") && !again.contains("won't parse"), "fmt is idempotent (jar.gsh canonical after fmt)");
    check!(medres.contains("Ok") && !medchk.contains("not canonical") && !medchk.contains("won't parse"),
           "medium file formats correctly (multi-flush, block-aligned streamed write)");
    check!(chkres.contains("Ok") && !hugechk.contains("not canonical") && !hugechk.contains("won't parse"),
           "10 MB script FORMATTED via streaming - fmt check reports canonical (Ok), no file-size cap");
    check!(!huge.contains("KERNEL PANIC") && !hugechk.contains("KERNEL PANIC"), "no kernel panic on the 10 MB format");

    child.kill().ok(); child.wait().ok();
    println!("\nfmt-demo: {pass} passed, {fail} failed");
    if fail > 0 { std::process::exit(1); }
}

/// Boot bare-metal with a jarring `fi.gsh` baked in, format it TWICE, read both results, and diff
/// them - the first differing byte localizes the streaming chunk-boundary bug.
pub fn run_fmt_idem(image_path: &Path, disk_path: &str, smp: u32) {
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
    ]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap_or_else(|e| { eprintln!("fmt-idem: QEMU launch failed: {e}"); std::process::exit(1); });
    let stream = match retry_tcp_connect(shell_port, Duration::from_secs(10)) {
        Some(s) => s, None => { eprintln!("fmt-idem: no serial"); child.kill().ok(); std::process::exit(1); }
    };
    let mut read_half  = stream.try_clone().expect("clone");
    let mut write_half = stream;
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    { let buf2 = Arc::clone(&buf); thread::spawn(move || {
        let mut tmp = [0u8; 256];
        loop { match read_half.read(&mut tmp) { Ok(0) | Err(_) => break, Ok(n) => buf2.lock().unwrap().extend_from_slice(&tmp[..n]) } }
    }); }
    let mut cursor = 0usize;
    if collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(40)).is_none() {
        println!("fmt-idem: FAIL - no first gsh>"); child.kill().ok(); child.wait().ok(); std::process::exit(1);
    }
    send(&mut write_half, b"fmt /fi.gsh\r");   let _  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(90));
    send(&mut write_half, b"read /fi.gsh\r");   let a  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(90)).unwrap_or_default();
    send(&mut write_half, b"fmt /fi.gsh\r");   let _  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(90));
    send(&mut write_half, b"read /fi.gsh\r");   let b  = collect_until(&buf, &mut cursor, b"gsh>", Duration::from_secs(90)).unwrap_or_default();
    child.kill().ok(); child.wait().ok();

    let _ = std::fs::write("build/fmt-A.txt", a.as_bytes());
    let _ = std::fs::write("build/fmt-B.txt", b.as_bytes());
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let n = ab.len().min(bb.len());
    let mut i = 0usize; while i < n && ab[i] == bb[i] { i += 1; }
    if i == n && ab.len() == bb.len() {
        println!("fmt-idem: IDENTICAL - fmt IS idempotent on this file ({} bytes)", ab.len());
    } else {
        println!("fmt-idem: DIFFER at byte {} (A={} bytes, B={} bytes)", i, ab.len(), bb.len());
        let lo = i.saturating_sub(60);
        println!("  A[{}..]: {:?}", lo, String::from_utf8_lossy(&ab[lo..(i + 60).min(ab.len())]));
        println!("  B[{}..]: {:?}", lo, String::from_utf8_lossy(&bb[lo..(i + 60).min(bb.len())]));
    }
    println!("(full A/B saved to build/fmt-A.txt, build/fmt-B.txt)");
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
/// QEMU needs ~50-200 ms to open the port after launch.
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
/// dumping the file over the (slow, capped) serial console. Contiguous files only (`type` 1) -
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
