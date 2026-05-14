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
    /// Build kernel with `test-bad-elf` feature, create a separate image,
    /// boot QEMU, and watch for the pass string (§22 Fuzz F3).
    WithBadElf {
        expect:       &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Host-side contract validation fuzz (§22 Fuzz F4): runs inline without QEMU.
    /// `ok` inputs must pass schema validation; `bad` inputs must not cause a panic.
    ContractFuzz {
        ok:  &'static [&'static str],
        bad: &'static [&'static [u8]],
    },
    /// Not implemented. Print reason; do not boot QEMU.
    Blocked {
        reason: &'static str,
    },
}

/// COM2 TCP port used by restart tests (distinct from interactive `osdev run` port 5555).
const TEST_CONTROL_PORT: u16 = 5556;

// ---------------------------------------------------------------------------
// Fuzz test definitions (Milestone 10 Phase 1).
// ---------------------------------------------------------------------------

static FUZZ_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "F1", name: "syscall_args_no_panic", spec_ref: "§22 Fuzz F1",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F1 pass (100/10)"],
            fail_on:      &["KERNEL PANIC", "fuzz: F1 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "F2", name: "syscall_numbers_no_panic", spec_ref: "§22 Fuzz F2",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F2 pass (50000/50000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: F2 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "F5", name: "ipc_message_bodies_no_panic", spec_ref: "§22 Fuzz F5",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F5 pass (1000/1000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: F5 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "F6", name: "embedded_cap_slots_no_panic", spec_ref: "§22 Fuzz F6",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F6 pass (1000/1000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: F6 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "F7", name: "stale_cap_generation_no_panic", spec_ref: "§22 Fuzz F7",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F7 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "fuzz: F7 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "F8", name: "memory_request_sizes_no_panic", spec_ref: "§22 Fuzz F8",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: F8 pass"],
            fail_on:      &["KERNEL PANIC", "fuzz: F8 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "F3", name: "elf_loader_no_panic", spec_ref: "§22 Fuzz F3",
        kind: TestKind::WithBadElf {
            expect:       &["fuzz: F3 pass (77/77)"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "F4", name: "contract_validator_no_panic", spec_ref: "§22 Fuzz F4",
        kind: TestKind::ContractFuzz {
            ok: &[
                concat!(
                    "name = \"ping\"\nversion = \"0.1.0\"\n\n",
                    "[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n\n",
                    "[capabilities]\nlog_write = true",
                ),
                concat!(
                    "name = \"pong\"\nversion = \"0.2.0\"\n\n",
                    "[resources.memory]\nrequest = \"16MiB\"\nlimit = \"32MiB\"\n\n",
                    "[capabilities]\nipc_send = [\"ping\"]\nipc_receive = [\"pong\"]\nlog_write = true",
                ),
                concat!(
                    "name = \"probe\"\nversion = \"0.1.0\"\n\n",
                    "[resources.memory]\nrequest = \"4MiB\"\nlimit = \"8MiB\"\n\n",
                    "[capabilities]\nlog_write = true\n\n",
                    "[placement]\ncore = 0",
                ),
            ],
            bad: &[
                b"",                          // empty — missing required fields
                b"\xFF\xFE",                  // non-UTF-8
                b"[unclosed",                 // invalid TOML
                // missing name
                b"version = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // missing version
                b"name = \"ping\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // missing resources
                b"name = \"ping\"\nversion = \"0.1.0\"\n[capabilities]\nlog_write = true",
                // missing capabilities
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"",
                // name wrong type (integer)
                b"name = 42\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // bad name pattern (uppercase)
                b"name = \"Ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // bad version (not semver)
                b"name = \"ping\"\nversion = \"not-semver\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // extra top-level field (additionalProperties: false)
                b"name = \"ping\"\nversion = \"0.1.0\"\nextra = true\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // bad memory unit (MB instead of MiB)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MB\"\nlimit = \"64MB\"\n[capabilities]\nlog_write = true",
                // placement.core out of range (max 15)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true\n[placement]\ncore = 100",
                // log_write wrong type (string instead of boolean)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = \"yes\"",
            ],
        },
    },
];

// ---------------------------------------------------------------------------
// Property test definitions (Milestone 9 Phase 1).
// ---------------------------------------------------------------------------

static PROPERTY_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "P1", name: "cap_unforgeable", spec_ref: "§7.3 §3.1",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P1 pass (10000/10000)"],
            fail_on:      &["KERNEL PANIC", "prop: P1 FAIL"],
            timeout_secs: 90,
        },
    },
    TestSpec {
        id: "P2", name: "generation_monotonic_across_lifetime", spec_ref: "§7.5",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P2 pass (3 iter x 2 cycles)"],
            fail_on:      &["KERNEL PANIC", "prop: P2 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "P3", name: "cap_rights_never_widen_on_transfer", spec_ref: "§7.3",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P3 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "prop: P3 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "P6", name: "queue_depth_invariant", spec_ref: "§8.5",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P6 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "prop: P6 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "P8", name: "restart_resolves_higher_generation", spec_ref: "§14.2",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P8 pass (5 iter)"],
            fail_on:      &["KERNEL PANIC", "prop: P8 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "P4", name: "alloc_accounting_exact", spec_ref: "§10.3",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P4 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "prop: P4 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "P5", name: "endpoint_has_one_owner", spec_ref: "§8.3",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P5 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "prop: P5 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "P7", name: "tlb_shootdown_no_stale_mappings", spec_ref: "§10.5",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P7 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "prop: P7 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "P9", name: "generation_invalidates_all_holders", spec_ref: "§7.5",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P9 pass"],
            fail_on:      &["KERNEL PANIC", "prop: P9 FAIL"],
            timeout_secs: 90,
        },
    },
    TestSpec {
        id: "P10", name: "send_returns_defined_outcome", spec_ref: "§8.6",
        kind: TestKind::WatchSerial {
            expect:       &["prop: P10 pass (10000/10000)"],
            fail_on:      &["KERNEL PANIC", "prop: P10 FAIL"],
            timeout_secs: 60,
        },
    },
];

// ---------------------------------------------------------------------------
// Stress test definitions (Milestone 11 Phase 1).
// ---------------------------------------------------------------------------

static STRESS_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "S1", name: "ipc_saturation", spec_ref: "§22 Stress S1",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S1 pass (10000/10000)"],
            fail_on:      &["KERNEL PANIC", "stress: S1 FAIL"],
            timeout_secs: 90,
        },
    },
    TestSpec {
        id: "S2", name: "restart_storm_kstack_pool", spec_ref: "§22 Stress S2",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S2 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "stress: S2 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "S3", name: "cross_core_ipc_thrash", spec_ref: "§22 Stress S3",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S3 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "stress: S3 FAIL"],
            timeout_secs: 200,
        },
    },
    TestSpec {
        id: "S4", name: "cap_table_churn_monotonic_gen", spec_ref: "§22 Stress S4",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S4 pass (10/10)"],
            fail_on:      &["KERNEL PANIC", "stress: S4 FAIL"],
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "S7", name: "memory_pressure_alloc_cycle", spec_ref: "§22 Stress S7",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S7 pass (100/100)"],
            fail_on:      &["KERNEL PANIC", "stress: S7 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "S10", name: "cascading_revocation_cross_core", spec_ref: "§22 Stress S10",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S10 pass (3/3 caps dead)"],
            fail_on:      &["KERNEL PANIC", "stress: S10 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "S5", name: "generation_monotonic_1000_cycles", spec_ref: "§22 Stress S5",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S5 pass (1000/1000)"],
            fail_on:      &["KERNEL PANIC", "stress: S5 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "S6", name: "ipc_self_ping_stability", spec_ref: "§22 Stress S6",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S6 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "stress: S6 FAIL"],
            timeout_secs: 200,
        },
    },
    TestSpec {
        id: "S8", name: "idle_scheduler_heartbeat", spec_ref: "§22 Stress S8",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S8 pass (600 yields)"],
            fail_on:      &["KERNEL PANIC", "stress: S8 FAIL"],
            timeout_secs: 200,
        },
    },
    TestSpec {
        id: "S9", name: "cross_core_ipi_storm", spec_ref: "§22 Stress S9",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S9 pass (1000/1000)"],
            fail_on:      &["KERNEL PANIC", "stress: S9 FAIL"],
            timeout_secs: 180,
        },
    },
];

// ---------------------------------------------------------------------------
// Performance benchmark definitions (Milestone 12).
// ---------------------------------------------------------------------------

static PERF_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "B1", name: "ipc_same_core_roundtrip_latency", spec_ref: "§22 Perf B1",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B1 done"],
            fail_on:      &["KERNEL PANIC", "perf: B1 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "B2", name: "ipc_cross_core_roundtrip_latency", spec_ref: "§22 Perf B2",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B2 done"],
            fail_on:      &["KERNEL PANIC", "perf: B2 FAIL"],
            timeout_secs: 250,
        },
    },
    TestSpec {
        id: "B3", name: "syscall_yield_floor", spec_ref: "§22 Perf B3",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B3 done"],
            fail_on:      &["KERNEL PANIC", "perf: B3 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "B4", name: "cap_validation_throughput", spec_ref: "§22 Perf B4",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B4 done"],
            fail_on:      &["KERNEL PANIC", "perf: B4 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "B5", name: "spawn_syscall_cost", spec_ref: "§22 Perf B5",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B5 done"],
            fail_on:      &["KERNEL PANIC", "perf: B5 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "B6", name: "restart_kill_plus_spawn_cost", spec_ref: "§22 Perf B6",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B6 done"],
            fail_on:      &["KERNEL PANIC", "perf: B6 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "B7", name: "cap_table_throughput", spec_ref: "§22 Perf B7",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B7 done"],
            fail_on:      &["KERNEL PANIC", "perf: B7 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "B8", name: "allocator_throughput", spec_ref: "§22 Perf B8",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B8 done"],
            fail_on:      &["KERNEL PANIC", "perf: B8 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "B9", name: "message_copy_4kib", spec_ref: "§22 Perf B9",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B9 done"],
            fail_on:      &["KERNEL PANIC", "perf: B9 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "B10", name: "scheduler_decision_cost", spec_ref: "§22 Perf B10",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B10 done"],
            fail_on:      &["KERNEL PANIC", "perf: B10 FAIL"],
            timeout_secs: 30,
        },
    },
];

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

    std::fs::create_dir_all("build/tests/1_IDENTITY").expect("create build/tests/1_IDENTITY/");

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

/// Boot the OS in QEMU and assert the Milestone 9 property test suite.
pub fn run_property_tests() {
    println!("property: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("property: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("property: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/2_PROPERTY").expect("create build/tests/2_PROPERTY/");

    println!("\nproperty: running {} tests\n", PROPERTY_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in PROPERTY_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_property_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n         → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n         → {r}"),
        }

        results.push((test, outcome));
    }

    let passed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();

    println!("\n  {passed} passed  {failed} failed");

    if failed > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and assert the Milestone 10 fuzz test suite.
pub fn run_fuzz_tests() {
    println!("fuzz: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("fuzz: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("fuzz: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/3_FUZZ").expect("create build/tests/3_FUZZ/");

    println!("\nfuzz: running {} tests\n", FUZZ_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in FUZZ_TESTS {
        print!("  [{:>2}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_fuzz_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n         → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n         → {r}"),
        }

        results.push((test, outcome));
    }

    let passed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();

    println!("\n  {passed} passed  {failed} failed");

    if failed > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and assert the Milestone 11 stress test suite.
pub fn run_stress_tests() {
    println!("stress: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("stress: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("stress: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/4_STRESS").expect("create build/tests/4_STRESS/");

    println!("\nstress: running {} tests\n", STRESS_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in STRESS_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_stress_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n         → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n         → {r}"),
        }

        results.push((test, outcome));
    }

    let passed  = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed  = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();
    let blocked = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Blocked(_))).count();

    println!("\n  {passed} passed  {failed} failed  {blocked} blocked");

    if failed > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and run the Milestone 12 performance benchmark suite.
///
/// Pass criterion: each benchmark logs `perf: BN done` without panicking.
/// After all benchmarks pass, extracted RDTSC metrics are written to
/// `tests/qemu/perf/baseline.json` for future regression comparisons.
pub fn run_perf_tests() {
    println!("perf: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("perf: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("perf: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/5_PERFORMANCE")
        .expect("create build/tests/5_PERFORMANCE/");

    println!("\nperf: running {} benchmarks\n", PERF_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in PERF_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_perf_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n         → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n         → {r}"),
        }

        results.push((test, outcome));
    }

    let passed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();

    println!("\n  {passed} passed  {failed} failed");

    if passed > 0 {
        collect_perf_baseline(&results);
    }

    if failed > 0 { std::process::exit(1); }
}

/// Extract metric values from each test's serial log and write baseline.json.
///
/// Parses lines matching `perf: BN key=value` and emits a JSON map.
/// Absolute RDTSC values are not comparable across QEMU hosts or versions;
/// the file is useful for detecting large regressions within one environment.
fn collect_perf_baseline(results: &[(&TestSpec, TestOutcome)]) {
    let mut metrics = serde_json::Map::new();

    for (test, outcome) in results {
        if !matches!(outcome, TestOutcome::Pass) { continue; }
        let serial = perf_serial_path(test);
        let content = std::fs::read_to_string(&serial).unwrap_or_default();
        let prefix  = format!("perf: {} ", test.id);
        for line in content.lines() {
            if line.contains(&prefix) && !line.contains("done") && !line.contains("FAIL") {
                let mut obj = serde_json::Map::new();
                for token in line.split_whitespace() {
                    if let Some((k, v)) = token.split_once('=') {
                        if let Ok(n) = v.parse::<u64>() {
                            obj.insert(k.to_string(), serde_json::Value::Number(n.into()));
                        }
                    }
                }
                if !obj.is_empty() {
                    metrics.insert(test.id.to_string(), serde_json::Value::Object(obj));
                }
            }
        }
    }

    let baseline = serde_json::json!({
        "note": "QEMU TCG RDTSC cycle counts — not comparable across hosts or QEMU versions; useful for detecting large regressions within one environment",
        "regression_threshold_pct": 10,
        "metrics": metrics,
    });

    std::fs::create_dir_all("tests/qemu/perf").ok();
    let out = serde_json::to_string_pretty(&baseline).unwrap_or_default();
    match std::fs::write("tests/qemu/perf/baseline.json", &out) {
        Ok(()) => println!("perf: baseline written to tests/qemu/perf/baseline.json"),
        Err(e) => eprintln!("perf: could not write baseline.json: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Per-test runner.
// ---------------------------------------------------------------------------

fn run_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::Blocked { reason } => TestOutcome::Blocked(reason),
        TestKind::WithBadElf { .. } => TestOutcome::Blocked("WithBadElf only runs via osdev test fuzz"),
        TestKind::ContractFuzz { .. } => TestOutcome::Blocked("ContractFuzz only runs via osdev test fuzz"),

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

fn run_property_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = property_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("property tests only use WatchSerial"),
    }
}

fn run_fuzz_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = fuzz_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }

        TestKind::WithBadElf { expect, fail_on, timeout_secs } => {
            let status = std::process::Command::new("cargo")
                .args([
                    "build", "--release", "-p", "kernel",
                    "--target", "x86_64-unknown-none",
                    "--features", "kernel/test-bad-elf",
                ])
                .status()
                .expect("failed to invoke cargo");
            if !status.success() {
                return TestOutcome::Fail(
                    "kernel build with test-bad-elf feature failed".to_string()
                );
            }

            let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
            let limine_dir = Path::new("tools/limine");
            let bad_img    = Path::new("build/tests/3_FUZZ/F3-bad-elf.img");
            let bad_image  = crate::disk_image::create_at(kernel_elf, limine_dir, bad_img);
            crate::disk_image::install_bootloader(limine_dir, &bad_image);

            let serial = fuzz_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(&bad_image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }

        TestKind::ContractFuzz { ok, bad } => {
            let schema_path = Path::new("contracts/schema/service.schema.json");
            let schema = load_schema(schema_path);

            for (i, &src) in ok.iter().enumerate() {
                match validate_contract_source(&schema, src.as_bytes()) {
                    Ok(()) => {}
                    Err(e) => return TestOutcome::Fail(
                        format!("ok[{i}] unexpectedly rejected by schema: {e}")
                    ),
                }
            }

            for (i, &src) in bad.iter().enumerate() {
                let s = schema.clone();
                let b = src.to_vec();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    validate_contract_source(&s, &b)
                }));
                if result.is_err() {
                    return TestOutcome::Fail(format!(
                        "bad[{i}] caused a panic in validate_contract_source"
                    ));
                }
            }

            TestOutcome::Pass
        }

        _ => TestOutcome::Blocked("unexpected test kind in fuzz suite"),
    }
}

fn run_stress_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::Blocked { reason } => TestOutcome::Blocked(reason),
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = stress_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("stress tests only use WatchSerial or Blocked"),
    }
}

fn run_perf_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = perf_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("perf tests only use WatchSerial"),
    }
}

fn serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/1_IDENTITY/{}-{}.log", test.id, test.name))
}

fn property_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/2_PROPERTY/{}-{}.log", test.id, test.name))
}

fn fuzz_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/3_FUZZ/{}-{}.log", test.id, test.name))
}

fn stress_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/4_STRESS/{}-{}.log", test.id, test.name))
}

fn perf_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/5_PERFORMANCE/{}-{}.log", test.id, test.name))
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
    let mut out = Vec::new();
    collect_contract_files(Path::new("."), &mut out, 0);
    out
}

fn collect_contract_files(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 { return; }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            // Skip directories that can't contain service contracts.
            if name.starts_with('.') || name == "target" || name == "build" { continue; }
            if name == "contracts" {
                // Collect .toml files directly inside this contracts/ dir.
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for f in inner.flatten() {
                        let fp = f.path();
                        if fp.is_file() && fp.extension().map_or(false, |e| e == "toml") {
                            out.push(fp);
                        }
                    }
                }
            } else {
                collect_contract_files(&path, out, depth + 1);
            }
        }
    }
}

fn validate_contract(schema: &serde_json::Value, path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    validate_contract_source(schema, &bytes)
}

fn validate_contract_source(schema: &serde_json::Value, bytes: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|e| format!("invalid UTF-8: {e}"))?;
    let toml_val: toml::Value = toml::from_str(text)
        .map_err(|e| format!("TOML parse error: {e}"))?;
    let json_str = serde_json::to_string(&toml_val)
        .map_err(|e| format!("JSON serialize error: {e}"))?;
    let json_val: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("JSON deserialize error: {e}"))?;
    if jsonschema::is_valid(schema, &json_val) {
        Ok(())
    } else {
        Err("contract does not conform to schema".to_string())
    }
}
