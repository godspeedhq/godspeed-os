//! Contract validation — §13.4.
//! Identity test harness — §22.
//!
//! Two jobs:
//!   1. `validate_all_contracts` — structural validation of all service.toml
//!      files against `contracts/schema/service.schema.json`.
//!   2. `run_identity_tests` — boot the OS in QEMU and run the §22 test
//!      suite, asserting serial output matches expected patterns.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Test definitions (§22).
// ---------------------------------------------------------------------------

struct TestSpec {
    id:       &'static str,
    name:     &'static str,
    spec_ref: &'static str,
    kind:     TestKind,
}

enum TestKind {
    /// Boot QEMU, watch serial for all `expect` lines within `timeout_secs`.
    /// Fail immediately if any `fail_on` line appears.
    WatchSerial {
        expect:       &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Boot QEMU, wait for `wait_for` line, send `restart_cmd` to the
    /// control port, then watch for `expect_after` lines.
    WithRestart {
        wait_for:     &'static str,
        restart_cmd:  &'static str,
        expect_after: &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Build kernel with `test-bad-registry` feature, create a separate
    /// image, boot QEMU with it, and watch for `expect` lines (e.g. a panic).
    WithBadTcb {
        expect:       &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Not implemented. Print reason; do not boot QEMU.
    Blocked {
        reason: &'static str,
    },
}

/// COM2 TCP port used by restart tests (distinct from interactive `osdev run` port 5555).
const TEST_CONTROL_PORT: u16 = 5556;

static TESTS: &[TestSpec] = &[
    TestSpec {
        id: "1A", name: "bootstrap_steady_state_positive", spec_ref: "§22 Test 1A",
        kind: TestKind::WatchSerial {
            expect: &[
                "kernel: 4 cores ready",
                "init: ready",
                "supervisor: ready",
                "registry: ready",
                "logger: ready",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "1B", name: "bootstrap_tcb_failure_panics", spec_ref: "§22 Test 1B",
        kind: TestKind::WithBadTcb {
            expect:       &["KERNEL PANIC", "reason: registry spawn failed"],
            fail_on:      &["supervisor: ready"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "2A", name: "cap_enforcement_positive", spec_ref: "§22 Test 2A",
        kind: TestKind::WatchSerial {
            expect:       &["cap-test: 2A pass — held cap validates OK"],
            fail_on:      &["KERNEL PANIC", "cap-test: 2A FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "2B", name: "cap_enforcement_negative", spec_ref: "§22 Test 2B",
        kind: TestKind::WatchSerial {
            expect: &[
                "cap-test: 2B pass — no cap returns CapNotHeld",
                "cap-test: 2C pass — wrong right returns CapInsufficientRights",
            ],
            fail_on:      &["KERNEL PANIC", "cap-test: 2B FAIL", "cap-test: 2C FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "3A", name: "ipc_same_core_positive", spec_ref: "§22 Test 3A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 3A recv OK", "probe: 3A send OK"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "3B", name: "ipc_no_send_right", spec_ref: "§22 Test 3B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 3B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 3B FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "4A", name: "endpoint_death_send_returns_dead", spec_ref: "§22 Test 4A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 4A pass"],
            fail_on:      &["KERNEL PANIC", "probe: 4A FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "4B", name: "blocked_sender_wakes_on_endpoint_death", spec_ref: "§22 Test 4B",
        kind: TestKind::WithRestart {
            wait_for:     "probe: 4B sender blocked",
            restart_cmd:  "KILL probe-4b-recv",
            expect_after: &["probe: 4B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 4B FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "5A", name: "cap_transfer_positive", spec_ref: "§22 Test 5A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 5A send OK", "probe: 5A recv OK"],
            fail_on:      &["KERNEL PANIC", "probe: 5A send FAIL", "probe: 5A recv FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "5B", name: "cap_transfer_negative", spec_ref: "§22 Test 5B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 5B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 5B FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "6A", name: "supervisor_restart_positive", spec_ref: "§22 Test 6A",
        kind: TestKind::WithRestart {
            wait_for:     "pong: received",
            restart_cmd:  "RESTART pong 1",
            expect_after: &["control: pong restarted"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "6B", name: "stale_cap_revoked_after_restart", spec_ref: "§22 Test 6B",
        kind: TestKind::WithRestart {
            wait_for:    "pong: received",
            restart_cmd: "RESTART pong 1",
            expect_after: &[
                "ping: pong endpoint dead, reacquiring via kernel registry",
                "ping: pong cap reacquired, resuming",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "7A", name: "memory_alloc_within_limit", spec_ref: "§22 Test 7A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 7A pass"],
            fail_on:      &["KERNEL PANIC", "probe: 7A FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "7B", name: "memory_beyond_limit", spec_ref: "§22 Test 7B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 7B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 7B FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "8A", name: "yield_advisory_works", spec_ref: "§22 Test 8A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 8A yielder ticked"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "8B", name: "non_yielding_service_preempted", spec_ref: "§22 Test 8B",
        kind: TestKind::WatchSerial {
            expect:       &["ping: sent 100 messages"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "9A", name: "cross_core_ipc_positive", spec_ref: "§22 Test 9A",
        kind: TestKind::WatchSerial {
            expect:       &["pong: ready on core", "pong: received"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "9B", name: "cross_core_no_authority_leak", spec_ref: "§22 Test 9B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 9B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 9B FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "10A", name: "restart_changes_core_transparently", spec_ref: "§22 Test 10A",
        kind: TestKind::WithRestart {
            wait_for:     "pong: received",
            restart_cmd:  "RESTART pong 2",
            expect_after: &["pong: ready on core 2"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "10B", name: "client_reacquires_after_core_change", spec_ref: "§22 Test 10B",
        kind: TestKind::WithRestart {
            wait_for:    "pong: received",
            restart_cmd: "RESTART pong 2",
            expect_after: &[
                "ping: pong endpoint dead, reacquiring via kernel registry",
                "ping: pong cap reacquired, resuming",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
];

// ---------------------------------------------------------------------------
// Test outcome.
// ---------------------------------------------------------------------------

enum TestOutcome {
    Pass,
    Fail(String),
    Blocked(&'static str),
}

// ---------------------------------------------------------------------------
// Public entry points.
// ---------------------------------------------------------------------------

/// Validate every `contracts/*.toml` against the JSON schema.
pub fn validate_all_contracts() {
    let schema_path = Path::new("contracts/schema/service.schema.json");
    let schema = load_schema(schema_path);

    let contracts = find_contracts();
    let mut failures = 0;

    for contract_path in &contracts {
        match validate_contract(&schema, contract_path) {
            Ok(())  => println!("OK  {}", contract_path.display()),
            Err(e)  => {
                eprintln!("FAIL {} — {}", contract_path.display(), e);
                failures += 1;
            }
        }
    }

    if failures > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and assert the §22 identity test suite.
pub fn run_identity_tests() {
    // Kill any running QEMU so ports are free.
    println!("identity: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("identity: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("identity: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests").expect("create build/tests/");

    println!("\nidentity: running {} tests\n", TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in TESTS {
        print!("  [{:>2}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass         => println!("PASS"),
            TestOutcome::Fail(r)      => println!("FAIL\n         → {r}"),
            TestOutcome::Blocked(r)   => println!("BLOCKED\n         → {r}"),
        }

        results.push((test, outcome));
    }

    // Summary.
    let passed  = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed  = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();
    let blocked = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Blocked(_))).count();

    println!("\n  {passed} passed  {failed} failed  {blocked} blocked");

    if failed > 0 { std::process::exit(1); }
}

// ---------------------------------------------------------------------------
// Per-test runner.
// ---------------------------------------------------------------------------

fn run_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::Blocked { reason } => TestOutcome::Blocked(reason),

        TestKind::WithBadTcb { expect, fail_on, timeout_secs } => {
            // Build kernel with the test-bad-registry feature (invalid registry ELF).
            let status = std::process::Command::new("cargo")
                .args([
                    "build", "--release", "-p", "kernel",
                    "--target", "x86_64-unknown-none",
                    "--features", "kernel/test-bad-registry",
                ])
                .status()
                .expect("failed to build kernel with test-bad-registry");
            if !status.success() {
                return TestOutcome::Fail(
                    "kernel build with test-bad-registry feature failed".to_string()
                );
            }

            let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
            let limine_dir = Path::new("tools/limine");
            let bad_img    = Path::new("build/tests/1B-bad-tcb.img");
            let bad_image  = crate::disk_image::create_at(kernel_elf, limine_dir, bad_img);
            crate::disk_image::install_bootloader(limine_dir, &bad_image);

            let serial = serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(&bad_image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }

        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = serial_path(test);
            // Truncate so poll_serial doesn't match stale content from a previous run.
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }

        TestKind::WithRestart { wait_for, restart_cmd, expect_after, fail_on, timeout_secs } => {
            let serial   = serial_path(test);
            // Truncate so poll_serial doesn't match stale content from a previous run.
            let _ = std::fs::write(&serial, b"");
            let deadline = Instant::now() + Duration::from_secs(*timeout_secs);
            let qemu     = crate::qemu::spawn_for_test(image, 4, &serial,
                                                        Some(TEST_CONTROL_PORT));

            // Wait for steady state.
            match poll_serial(&serial, &[wait_for], fail_on, deadline) {
                TestOutcome::Pass => {}
                other => { qemu.kill(); return other; }
            }

            // Small pause so IPC is flowing before we disrupt it.
            std::thread::sleep(Duration::from_millis(500));

            // Send restart command over COM2.
            // Leading '\n' flushes any partial byte the UART FIFO may hold
            // from the TCP connection setup on a freshly booted QEMU instance.
            let addr = format!("127.0.0.1:{TEST_CONTROL_PORT}");
            match std::net::TcpStream::connect(&addr) {
                Ok(mut s) => {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = s.write_all(format!("\n{restart_cmd}\n").as_bytes());
                }
                Err(e) => {
                    qemu.kill();
                    return TestOutcome::Fail(
                        format!("could not connect to control port {addr}: {e}")
                    );
                }
            }

            // Wait for post-restart assertions.
            let result = poll_serial(&serial, expect_after, fail_on, deadline);
            qemu.kill();
            result
        }
    }
}

// ---------------------------------------------------------------------------
// Serial polling.
// ---------------------------------------------------------------------------

/// Poll `path` until all `expect` substrings appear, any `fail_on` substring
/// appears, or `deadline` passes.
fn poll_serial(
    path:     &Path,
    expect:   &[&str],
    fail_on:  &[&str],
    deadline: Instant,
) -> TestOutcome {
    loop {
        if Instant::now() >= deadline {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let missing: Vec<String> = expect.iter()
                .filter(|e| !content.contains(**e))
                .map(|e| format!("\"{e}\""))
                .collect();
            return TestOutcome::Fail(format!(
                "timeout — lines not seen: {}",
                missing.join(", ")
            ));
        }

        let content = match std::fs::read_to_string(path) {
            Ok(s)  => s,
            Err(_) => { std::thread::sleep(Duration::from_millis(100)); continue; }
        };

        for &line in fail_on {
            if content.contains(line) {
                return TestOutcome::Fail(format!("saw fail marker: \"{line}\""));
            }
        }

        if expect.iter().all(|e| content.contains(e)) {
            return TestOutcome::Pass;
        }

        std::thread::sleep(Duration::from_millis(200));
    }
}

fn serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/{}-{}.log", test.id, test.name))
}

// ---------------------------------------------------------------------------
// Kill existing QEMU.
// ---------------------------------------------------------------------------

fn kill_existing_qemu() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", "qemu-system-x86_64.exe"])
            .output();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("pkill")
            .arg("qemu-system-x86_64")
            .output();
    }
    // Brief pause to let ports free up.
    std::thread::sleep(Duration::from_millis(500));
}

// ---------------------------------------------------------------------------
// Contract validation (§13.4) — stubs.
// ---------------------------------------------------------------------------

fn load_schema(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("failed to read schema at {}: {}", path.display(), e));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid JSON schema: {}", e))
}

fn find_contracts() -> Vec<PathBuf> {
    todo!("walk the repo for all files matching */contracts/*.toml")
}

fn validate_contract(schema: &serde_json::Value, path: &Path) -> Result<(), String> {
    todo!(
        "parse TOML → JSON, validate against schema using jsonschema crate, \
         return Ok or the first validation error"
    )
}
