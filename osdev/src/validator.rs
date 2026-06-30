// GodspeedOS - Created by Bankole Ogundero.
//
// This software is provided "as is", without warranty or guarantee of any kind,
// express or implied. The author makes no guarantee of its correctness, reliability,
// or fitness for any purpose, and accepts no liability for any damages arising from
// its use. Use at your own risk.

//! Contract validation - §13.4.
//! Identity test harness - §22.
//!
//! Two jobs:
//!   1. `validate_all_contracts` - structural validation of all service.toml
//!      files against `contracts/schema/service.schema.json`.
//!   2. `run_identity_tests` - boot the OS in QEMU and run the §22 test
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
    /// Build kernel with `test-bad-supervisor` feature, create a separate
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
    /// Build kernel with `test-bad-elf-brutal` feature, boot QEMU, watch for pass (BF3).
    WithBadElfBrutal {
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
    /// Boot QEMU with a reduced SMP count (chaos C1). `smp` cores instead of 4.
    DegradedSmp {
        smp:          u32,
        expect:       &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Boot QEMU with both a reduced SMP count and reduced RAM (chaos C4).
    DegradedEnv {
        smp:          u32,
        ram_mib:      u32,
        expect:       &'static [&'static str],
        fail_on:      &'static [&'static str],
        timeout_secs: u64,
    },
    /// Not implemented. Print reason; do not boot QEMU.
    Blocked {
        reason: &'static str,
    },
}

/// Base COM2 TCP port for restart tests (distinct from interactive `osdev run` port 5555).
/// Each WithRestart test gets a unique port by incrementing this counter, so back-to-back
/// tests never conflict on a port still in TIME_WAIT from the previous QEMU instance.
const TEST_CONTROL_PORT_BASE: u16 = 5556;
static NEXT_CONTROL_PORT: std::sync::atomic::AtomicU16 =
    std::sync::atomic::AtomicU16::new(TEST_CONTROL_PORT_BASE);

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
                b"",                          // empty - missing required fields
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
            timeout_secs: 180, // raised: 90 s was borderline under 200-task QEMU TCG load
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
            expect:       &["stress: S3 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "stress: S3 FAIL"],
            timeout_secs: 400, // 50 cross-core msgs; scaled down - spawn at ~280 s leaves ~120 s for IPC
        },
    },
    TestSpec {
        id: "S4", name: "cap_table_churn_monotonic_gen", spec_ref: "§22 Stress S4",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S4 pass (10/10)"],
            fail_on:      &["KERNEL PANIC", "stress: S4 FAIL"],
            timeout_secs: 600, // raised: pass can arrive during a burst phase that misses 300 s
        },
    },
    TestSpec {
        id: "S7", name: "memory_pressure_alloc_cycle", spec_ref: "§22 Stress S7",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S7 pass (100/100)"],
            fail_on:      &["KERNEL PANIC", "stress: S7 FAIL"],
            timeout_secs: 120, // raised: pass arrives just past 60 s under 200-task QEMU TCG load
        },
    },
    TestSpec {
        id: "S10", name: "cascading_revocation_cross_core", spec_ref: "§22 Stress S10",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S10 pass (3/3 caps dead)"],
            fail_on:      &["KERNEL PANIC", "stress: S10 FAIL"],
            timeout_secs: 120, // raised: 30 s too tight under 200-task QEMU TCG scheduling variance
        },
    },
    TestSpec {
        id: "S5", name: "generation_monotonic_500_cycles", spec_ref: "§22 Stress S5",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S5 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "stress: S5 FAIL"],
            timeout_secs: 400, // 500 cycles; scaled down from 1000 - BS5 extends to 5000
        },
    },
    TestSpec {
        id: "S6", name: "ipc_self_ping_stability", spec_ref: "§22 Stress S6",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S6 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "stress: S6 FAIL"],
            timeout_secs: 200, // 500 self-ping rounds; scaled down from 5000 to fit QEMU TCG
        },
    },
    TestSpec {
        id: "S8", name: "idle_scheduler_heartbeat", spec_ref: "§22 Stress S8",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S8 pass (5 yields)"],
            fail_on:      &["KERNEL PANIC", "stress: S8 FAIL"],
            timeout_secs: 200, // 5 yield rounds; reduced from 50: each yield costs ~500 ms under 200-task load
        },
    },
    TestSpec {
        id: "S9", name: "cross_core_ipi_storm", spec_ref: "§22 Stress S9",
        kind: TestKind::WatchSerial {
            expect:       &["stress: S9 pass (100/100)"],
            fail_on:      &["KERNEL PANIC", "stress: S9 FAIL"],
            timeout_secs: 480, // 100 total msgs (50/sender); spawn at ~340 s leaves ~140 s for IPC
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
            // N reduced 200→50 in probe. Same-core round-trips need two scheduler
            // context switches; with 160+ tasks each costs ~800ms wall.
            // Spawn at ~48-150s + 50×800ms ≈ 40s work → worst-case ~190s < 300s.
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "B2", name: "ipc_cross_core_roundtrip_latency", spec_ref: "§22 Perf B2",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B2 done"],
            fail_on:      &["KERNEL PANIC", "perf: B2 FAIL"],
            // N reduced 200→50 in probe. Spawn at ~153s + 50×~1.8s ≈ 90s work →
            // worst-case ~243s; raised to 300s to match other IPC/yield benchmarks.
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "B3", name: "syscall_yield_floor", spec_ref: "§22 Perf B3",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B3 done"],
            fail_on:      &["KERNEL PANIC", "perf: B3 FAIL"],
            // N reduced 1000→10 in probe. Brutal stress tasks make each yield cost
            // 3-5s wall; 10×3.5s = 35s work + spawn ~128s ≪ 300s timeout.
            timeout_secs: 300,
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
            // perf-b8 (slot ~160) spawns 4-120s depending on QEMU TCG load from
            // brutal tests. Work itself is fast (16384 allocs). 300s covers worst case.
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "B9", name: "message_copy_4kib", spec_ref: "§22 Perf B9",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B9 done"],
            fail_on:      &["KERNEL PANIC", "perf: B9 FAIL"],
            // perf-b9 (slot ~160) spawns 12-163s depending on QEMU TCG load.
            // 200 blocking sends are fast once running. 300s covers worst-case boot.
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "B10", name: "scheduler_decision_cost", spec_ref: "§22 Perf B10",
        kind: TestKind::WatchSerial {
            expect:       &["perf: B10 done"],
            fail_on:      &["KERNEL PANIC", "perf: B10 FAIL"],
            // N reduced 1000→10 in probe. Mirrors B3: brutal stress tasks cost 3-5s/yield;
            // 10×3.5s = 35s work + spawn ~128s ≪ 300s timeout.
            timeout_secs: 300,
        },
    },
];

// ---------------------------------------------------------------------------
// Adversarial test definitions (Milestone 13).
// ---------------------------------------------------------------------------

static ADV_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "A1", name: "random_cap_slots_no_panic", spec_ref: "§22 Adversarial A1",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A1 pass (10000/10000)"],
            fail_on:      &["KERNEL PANIC", "adv: A1 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "A2", name: "endpoint_id_brute_force_no_panic", spec_ref: "§22 Adversarial A2",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A2 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A2 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A3", name: "alloc_beyond_limit_no_panic", spec_ref: "§22 Adversarial A3",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A3 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A3 FAIL"],
            // adv-a3 is spawned after ~120 preceding tasks (adv-ba*, chaos-bc*,
            // prop-p*, fuzz-f*, stress-s*, chaos-c*); at burst rate ~80 lines/sec
            // that's ~25 s just to reach the spawn - 30 s fires before spawn completes.
            timeout_secs: 90,
        },
    },
    TestSpec {
        id: "A4", name: "recv_cap_as_send_cap_insufficient_rights", spec_ref: "§22 Adversarial A4",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A4 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A4 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A5", name: "toctou_kill_then_send_endpoint_dead", spec_ref: "§22 Adversarial A5",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A5 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A5 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A6", name: "cap_table_fill_no_panic", spec_ref: "§22 Adversarial A6",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A6 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A6 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A7", name: "ipc_timing_no_panic", spec_ref: "§22 Adversarial A7",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A7 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A7 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A8", name: "preemption_prevents_monopoly", spec_ref: "§22 Adversarial A8",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A8 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A8 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "A9", name: "direct_spawn_bypasses_supervisor", spec_ref: "§22 Adversarial A9",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A9 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A9 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A10", name: "kernel_addr_as_syscall_arg_no_panic", spec_ref: "§22 Adversarial A10",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A10 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A10 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A11", name: "introspection_denied_without_cap",
        spec_ref: "§3.1; docs/introspection-capability.md",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A11 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A11 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A12", name: "reboot_denied_without_cap",
        spec_ref: "§3.1; REBOOT cap - syscall/dispatch.rs handle_reboot",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A12 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A12 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "A13", name: "acquire_send_cap_denied_without_cap",
        spec_ref: "§3.1; ACQUIRE_ANY - syscall/dispatch.rs handle_acquire_send_cap",
        kind: TestKind::WatchSerial {
            expect:       &["adv: A13 pass"],
            fail_on:      &["KERNEL PANIC", "adv: A13 FAIL"],
            timeout_secs: 30,
        },
    },
];

// ---------------------------------------------------------------------------
// Chaos test definitions (Milestone 14).
// ---------------------------------------------------------------------------

static CHAOS_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "C1", name: "degraded_smp_boot", spec_ref: "§22 Chaos C1",
        kind: TestKind::DegradedSmp {
            smp:          2,
            expect:       &["kernel: 2 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "C2", name: "non_tcb_fault_system_continues", spec_ref: "§22 Chaos C2",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: C2 pass"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "C3", name: "alloc_pressure_no_panic", spec_ref: "§22 Chaos C3",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: C3 pass"],
            fail_on:      &["KERNEL PANIC", "chaos: C3 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "C4", name: "degraded_env_minimal_ram", spec_ref: "§22 Chaos C4",
        kind: TestKind::DegradedEnv {
            smp:          4,
            ram_mib:      192,
            expect:       &["kernel: 4 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "C5", name: "kernel_stack_probe_recursive_syscalls", spec_ref: "§22 Chaos C5",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: C5 pass"],
            fail_on:      &["KERNEL PANIC", "chaos: C5 FAIL"],
            // 100 recursive yield_cpu() calls; each yield costs ~500ms wall under
            // load (150+ competing tasks); 100 × 500ms = 50s work + ~8s boot-to-spawn.
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "C6", name: "starved_core_others_unaffected", spec_ref: "§22 Chaos C6",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: C6 pass"],
            fail_on:      &["KERNEL PANIC", "chaos: C6 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "C7", name: "tlb_shootdown_under_load_no_corruption", spec_ref: "§22 Chaos C7",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: C7 pass"],
            fail_on:      &["KERNEL PANIC", "chaos: C7 FAIL"],
            timeout_secs: 120,
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal chaos test definitions (Milestone 21).
// ---------------------------------------------------------------------------

static BRUTAL_CHAOS_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BC1", name: "degraded_smp_1_core", spec_ref: "§22 Brutal Chaos BC1",
        kind: TestKind::DegradedSmp {
            smp:          1,
            expect:       &["kernel: 1 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "BC2", name: "five_simultaneous_faults_survive", spec_ref: "§22 Brutal Chaos BC2",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: BC2 pass - 5 simultaneous non-TCB faults; system survived"],
            fail_on:      &["KERNEL PANIC", "chaos: BC2 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BC3", name: "alloc_deny_2500_cycles", spec_ref: "§22 Brutal Chaos BC3",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: BC3 pass - 2500 alloc-deny cycles without panic"],
            fail_on:      &["KERNEL PANIC", "chaos: BC3 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BC4", name: "degraded_env_96mib_ram", spec_ref: "§22 Brutal Chaos BC4",
        kind: TestKind::DegradedEnv {
            smp:          4,
            ram_mib:      96,
            expect:       &["kernel: 4 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "BC5", name: "kernel_stack_500_recursive_syscalls", spec_ref: "§22 Brutal Chaos BC5",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: BC5 pass"],
            fail_on:      &["KERNEL PANIC", "chaos: BC5 FAIL"],
            timeout_secs: 600,
        },
    },
    TestSpec {
        id: "BC6", name: "two_hog_cores_others_unaffected", spec_ref: "§22 Brutal Chaos BC6",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: BC6 pass - 2-core hog starvation; core 0 still alive"],
            fail_on:      &["KERNEL PANIC", "chaos: BC6 FAIL"],
            timeout_secs: 600,
        },
    },
    TestSpec {
        id: "BC7", name: "tlb_shootdown_15_cycles", spec_ref: "§22 Brutal Chaos BC7",
        kind: TestKind::WatchSerial {
            expect:       &["chaos: BC7 pass - 15 cross-core TLB shootdowns survived"],
            fail_on:      &["KERNEL PANIC", "chaos: BC7 FAIL"],
            timeout_secs: 900,
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal identity test definitions (Milestone 15).
// ---------------------------------------------------------------------------

static BRUTAL_IDENTITY_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "T11", name: "queue_boundary_exactness", spec_ref: "§22 Brutal Identity T11",
        kind: TestKind::WatchSerial {
            expect:       &["identity: T11 pass"],
            fail_on:      &["KERNEL PANIC", "identity: T11 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "T12", name: "cap_delegation_chain_a_b_c", spec_ref: "§22 Brutal Identity T12",
        kind: TestKind::WatchSerial {
            expect:       &["identity: T12 pass"],
            fail_on:      &["KERNEL PANIC", "identity: T12 FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "T13", name: "cross_core_blocked_send_wakes_endpoint_dead", spec_ref: "§22 Brutal Identity T13",
        kind: TestKind::WatchSerial {
            expect:       &["identity: T13 pass"],
            fail_on:      &["KERNEL PANIC", "identity: T13 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "SMP-2", name: "smp_escalation_2_cores", spec_ref: "§22 Test 1A at smp=2",
        kind: TestKind::DegradedSmp {
            smp:          2,
            expect:       &["kernel: 2 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "SMP-8", name: "smp_escalation_8_cores", spec_ref: "§22 Test 1A at smp=8",
        kind: TestKind::DegradedSmp {
            smp:          8,
            expect:       &["kernel: 8 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "SMP-16", name: "smp_escalation_16_cores", spec_ref: "§22 Test 1A at smp=16",
        kind: TestKind::DegradedSmp {
            smp:          16,
            expect:       &["kernel: 16 cores ready", "supervisor: ready"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 120,
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal property test definitions (Milestone 16).
// ---------------------------------------------------------------------------

static BRUTAL_PROPERTY_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BP1", name: "cap_unforgeability_100k", spec_ref: "§22 Brutal Property BP1",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP1 pass (100000/100000)"],
            fail_on:      &["KERNEL PANIC", "prop: BP1 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP2", name: "generation_monotonic_20_cycles", spec_ref: "§22 Brutal Property BP2",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP2 pass (20/20)"],
            fail_on:      &["KERNEL PANIC", "prop: BP2 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP3", name: "cap_rights_never_widen_10k", spec_ref: "§22 Brutal Property BP3",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP3 pass (10000/10000)"],
            fail_on:      &["KERNEL PANIC", "prop: BP3 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP4", name: "alloc_accounting_2k", spec_ref: "§22 Brutal Property BP4",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP4 pass (2000/2000)"],
            fail_on:      &["KERNEL PANIC", "prop: BP4 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP5", name: "endpoint_ownership_150_cycles", spec_ref: "§22 Brutal Property BP5",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP5 pass (150/150)"],
            fail_on:      &["KERNEL PANIC", "prop: BP5 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP6", name: "queue_invariants_2k", spec_ref: "§22 Brutal Property BP6",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP6 pass (2000/2000)"],
            fail_on:      &["KERNEL PANIC", "prop: BP6 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP7", name: "tlb_shootdown_150_cycles", spec_ref: "§22 Brutal Property BP7",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP7 pass (150/150)"],
            fail_on:      &["KERNEL PANIC", "prop: BP7 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP8", name: "restart_higher_gen_20_iter", spec_ref: "§22 Brutal Property BP8",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP8 pass (20 iter)"],
            fail_on:      &["KERNEL PANIC", "prop: BP8 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP9", name: "all_3_slots_invalidated_10_cycles", spec_ref: "§22 Brutal Property BP9",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP9 pass (10/10"],
            fail_on:      &["KERNEL PANIC", "prop: BP9 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BP10", name: "send_defined_outcome_100k", spec_ref: "§22 Brutal Property BP10",
        kind: TestKind::WatchSerial {
            expect:       &["prop: BP10 pass (100000/100000)"],
            fail_on:      &["KERNEL PANIC", "prop: BP10 FAIL"],
            timeout_secs: 180,
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal fuzz test definitions (Milestone 17).
// ---------------------------------------------------------------------------

static BRUTAL_FUZZ_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BF1", name: "syscall_args_500_rounds", spec_ref: "§22 Brutal Fuzz BF1",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF1 pass (500/10)"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF1 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BF2", name: "syscall_numbers_200k", spec_ref: "§22 Brutal Fuzz BF2",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF2 pass (200000/200000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF2 FAIL"],
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "BF3", name: "elf_loader_263_inputs", spec_ref: "§22 Brutal Fuzz BF3",
        kind: TestKind::WithBadElfBrutal {
            expect:       &["fuzz: BF3 pass (263/263)"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "BF4", name: "contract_validator_extended", spec_ref: "§22 Brutal Fuzz BF4",
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
                concat!(
                    "name = \"logger\"\nversion = \"1.0.0\"\n\n",
                    "[resources.memory]\nrequest = \"8MiB\"\nlimit = \"16MiB\"\n\n",
                    "[capabilities]\nlog_write = true\nipc_receive = [\"logger\"]",
                ),
                concat!(
                    "name = \"block-driver\"\nversion = \"0.3.0\"\n\n",
                    "[resources.memory]\nrequest = \"64MiB\"\nlimit = \"128MiB\"\n\n",
                    "[capabilities]\nlog_write = true\nipc_send = [\"fs\"]\nipc_receive = [\"block-driver\"]",
                ),
            ],
            bad: &[
                b"",                          // empty
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
                // name with spaces
                b"name = \"my service\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // name with slash
                b"name = \"ping/pong\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // version with only major (no minor.patch)
                b"name = \"ping\"\nversion = \"1\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // version with four parts
                b"name = \"ping\"\nversion = \"1.0.0.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // memory.request missing but limit present
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // memory.limit missing but request present
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\n[capabilities]\nlog_write = true",
                // ipc_send wrong type (string, not array)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nipc_send = \"pong\"",
                // ipc_receive wrong type (boolean)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nipc_receive = true",
                // placement.core wrong type (string)
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true\n[placement]\ncore = \"zero\"",
                // placement.core negative
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true\n[placement]\ncore = -1",
                // extra field inside capabilities
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true\nunknown_cap = true",
                // extra field inside resources.memory
                b"name = \"ping\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\nextra = 1\n[capabilities]\nlog_write = true",
                // null bytes (not valid UTF-8)
                b"\x00\x00\x00",
                // single null byte
                b"\x00",
                // only whitespace
                b"   \n\t\n   ",
                // very long name (>64 chars)
                b"name = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
                // name empty string
                b"name = \"\"\nversion = \"0.1.0\"\n[resources.memory]\nrequest = \"32MiB\"\nlimit = \"64MiB\"\n[capabilities]\nlog_write = true",
            ],
        },
    },
    TestSpec {
        id: "BF5", name: "ipc_message_bodies_5k", spec_ref: "§22 Brutal Fuzz BF5",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF5 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF5 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BF6", name: "embedded_cap_slots_5k", spec_ref: "§22 Brutal Fuzz BF6",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF6 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF6 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BF7", name: "stale_cap_generation_200_cycles", spec_ref: "§22 Brutal Fuzz BF7",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF7 pass (200/200)"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF7 FAIL"],
            timeout_secs: 180,
        },
    },
    TestSpec {
        id: "BF8", name: "memory_request_5k_random", spec_ref: "§22 Brutal Fuzz BF8",
        kind: TestKind::WatchSerial {
            expect:       &["fuzz: BF8 pass"],
            fail_on:      &["KERNEL PANIC", "fuzz: BF8 FAIL"],
            timeout_secs: 120,
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal performance benchmark definitions (Milestone 19).
// ---------------------------------------------------------------------------

static BRUTAL_PERF_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BP1", name: "ipc_same_core_roundtrip_1000", spec_ref: "§22 Brutal Perf BP1",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP1 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "perf: BP1 FAIL"],
            timeout_secs: 600, // same ceiling as BP2 - QEMU TCG variance under 200-task load
        },
    },
    TestSpec {
        id: "BP2", name: "ipc_cross_core_roundtrip_1000", spec_ref: "§22 Brutal Perf BP2",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP2 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "perf: BP2 FAIL"],
            timeout_secs: 600, // 2.4× B2's 250s - 5× samples + IPI overhead under load
        },
    },
    TestSpec {
        id: "BP3", name: "yield_floor_2000", spec_ref: "§22 Brutal Perf BP3",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP3 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 600, // raised: 300s insufficient under 200-task QEMU TCG scheduling load
        },
    },
    TestSpec {
        id: "BP4", name: "cap_validation_50000", spec_ref: "§22 Brutal Perf BP4",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP4 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "perf: BP4 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BP5", name: "spawn_cost_50_cycles", spec_ref: "§22 Brutal Perf BP5",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP5 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 600, // raised to match BP6 ceiling; 50 spawn cycles under brutal load
        },
    },
    TestSpec {
        id: "BP6", name: "restart_cost_50_cycles", spec_ref: "§22 Brutal Perf BP6",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP6 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 600, // kill+spawn is ~2× spawn; raised to match BP2 ceiling
        },
    },
    TestSpec {
        id: "BP7", name: "cap_ir_throughput_5000", spec_ref: "§22 Brutal Perf BP7",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP7 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 300, // 10× B7's 30s - 5× cycles × concurrent load factor
        },
    },
    TestSpec {
        id: "BP8", name: "alloc_throughput_to_limit", spec_ref: "§22 Brutal Perf BP8",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP8 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BP9", name: "message_copy_4kib_400", spec_ref: "§22 Brutal Perf BP9",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP9 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 600,
        },
    },
    TestSpec {
        id: "BP10", name: "scheduler_pick_next_2000", spec_ref: "§22 Brutal Perf BP10",
        kind: TestKind::WatchSerial {
            expect:       &["perf: BP10 done"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 600, // raised: 300s insufficient under 200-task QEMU TCG scheduling load
        },
    },
];

// ---------------------------------------------------------------------------
// Brutal stress test definitions (Milestone 18).
// ---------------------------------------------------------------------------

static BRUTAL_STRESS_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BS1", name: "ipc_saturation_50k", spec_ref: "§22 Brutal Stress BS1",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS1 pass (50000/50000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS1 FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "BS2", name: "restart_storm_200_cycles", spec_ref: "§22 Brutal Stress BS2",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS2 pass (200/200)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS2 FAIL"],
            timeout_secs: 480, // 4× S2's 120s - 200 cycles under heavy concurrent load
        },
    },
    TestSpec {
        id: "BS3", name: "cross_core_thrash_2000_msgs", spec_ref: "§22 Brutal Stress BS3",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS3 pass (2000/2000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS3 FAIL"],
            timeout_secs: 1200, // 4× S3 sends under heavy concurrent TLB-shootdown pressure
        },
    },
    TestSpec {
        id: "BS4", name: "cap_table_churn_50_cycles", spec_ref: "§22 Brutal Stress BS4",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS4 pass (50/50)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS4 FAIL"],
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "BS5", name: "generation_monotonic_5000_cycles", spec_ref: "§22 Brutal Stress BS5",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS5 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS5 FAIL"],
            timeout_secs: 720, // 6× S5's 120s - extra margin for concurrent kill/respawn pressure
        },
    },
    TestSpec {
        id: "BS6", name: "ipc_self_ping_20000_rounds", spec_ref: "§22 Brutal Stress BS6",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS6 pass (20000/20000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS6 FAIL"],
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "BS7", name: "memory_pressure_500_passes", spec_ref: "§22 Brutal Stress BS7",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS7 pass (500/500)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS7 FAIL"],
            timeout_secs: 120,
        },
    },
    TestSpec {
        id: "BS8", name: "idle_scheduler_heartbeat_3000", spec_ref: "§22 Brutal Stress BS8",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS8 pass (3000 yields)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 300,
        },
    },
    TestSpec {
        id: "BS9", name: "cross_core_ipi_storm_5000_msgs", spec_ref: "§22 Brutal Stress BS9",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS9 pass (5000/5000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS9 FAIL"],
            timeout_secs: 420,
        },
    },
    TestSpec {
        id: "BS10", name: "cascading_revocation_50_cycles", spec_ref: "§22 Brutal Stress BS10",
        kind: TestKind::WatchSerial {
            expect:       &["stress: BS10 pass (50/50 cycles)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "stress: BS10 FAIL"],
            timeout_secs: 240,
        },
    },
];

static TESTS: &[TestSpec] = &[
    TestSpec {
        id: "1A", name: "bootstrap_steady_state_positive", spec_ref: "§22 Test 1A",
        kind: TestKind::WatchSerial {
            expect: &[
                "kernel: 4 cores ready",
                // (no "init: ready" - init is removed; the kernel spawns the supervisor directly, Phase 5)
                "supervisor: ready",
                "logger: ready",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 120, // 30 was too tight: supervisor spawns 178 probe services before logging "ready" (~90s on loaded TCG)
        },
    },
    TestSpec {
        id: "1B", name: "bootstrap_tcb_failure_panics", spec_ref: "§22 Test 1B",
        kind: TestKind::WithBadTcb {
            // Path C / Phase 5: init is removed, so the KERNEL spawns the supervisor directly. A
            // corrupt supervisor ELF fails that spawn and the kernel panics (`panic!("supervisor
            // spawn failed: ...")`) - the §6.2 TCB-failure path, now kernel-direct (no init abort,
            // hence no "reason:" prefix).
            expect:       &["KERNEL PANIC", "supervisor spawn failed"],
            fail_on:      &["supervisor: ready"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "2A", name: "cap_enforcement_positive", spec_ref: "§22 Test 2A",
        kind: TestKind::WatchSerial {
            expect:       &["cap-test: 2A pass - held cap validates OK"],
            fail_on:      &["KERNEL PANIC", "cap-test: 2A FAIL"],
            timeout_secs: 30,
        },
    },
    TestSpec {
        id: "2B", name: "cap_enforcement_negative", spec_ref: "§22 Test 2B",
        kind: TestKind::WatchSerial {
            expect: &[
                "cap-test: 2B pass - no cap returns CapNotHeld",
                "cap-test: 2C pass - wrong right returns CapInsufficientRights",
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
            timeout_secs: 60,
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
            wait_for:     "supervisor: ready", // pong/ping spawn first; wait for all probes done before restart
            restart_cmd:  "RESTART pong 1",
            expect_after: &["control: pong restarted"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60, // identity-only supervisor: ready ~3s; restart phase ~5s; 60s is ~6× margin
        },
    },
    TestSpec {
        id: "6B", name: "stale_cap_revoked_after_restart", spec_ref: "§22 Test 6B",
        kind: TestKind::WithRestart {
            wait_for:    "supervisor: ready", // pong/ping spawn first; wait for all probes done before restart
            restart_cmd: "RESTART pong 1",
            expect_after: &[
                "ping: pong endpoint dead, reacquiring via the kernel name directory",
                "ping: pong cap reacquired, resuming",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60, // identity-only supervisor: ready ~3s; reacquisition phase ~5s; 60s is ~6× margin
        },
    },
    TestSpec {
        id: "7A", name: "memory_alloc_within_limit", spec_ref: "§22 Test 7A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 7A pass"],
            fail_on:      &["KERNEL PANIC", "probe: 7A FAIL"],
            timeout_secs: 60, // 30 was too tight under 100+ competing probe services
        },
    },
    TestSpec {
        id: "7B", name: "memory_beyond_limit", spec_ref: "§22 Test 7B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 7B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 7B FAIL"],
            timeout_secs: 60, // 30 was too tight under 100+ competing probe services
        },
    },
    TestSpec {
        id: "8A", name: "yield_advisory_works", spec_ref: "§22 Test 8A",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 8A yielder ticked"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 120, // 60 was too tight: yielder competes with 100+ probe services for scheduler quanta
        },
    },
    TestSpec {
        id: "8B", name: "non_yielding_service_preempted", spec_ref: "§22 Test 8B",
        kind: TestKind::WatchSerial {
            expect:       &["ping: sent 20 messages"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 120, // pong/ping spawn first so pong is ready when ping starts; no queue-full stall
        },
    },
    TestSpec {
        id: "9A", name: "cross_core_ipc_positive", spec_ref: "§22 Test 9A",
        kind: TestKind::WatchSerial {
            expect:       &["pong: ready on core", "pong: received"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60, // pong/ping spawn first; "pong: received" appears at t≈5-10s
        },
    },
    TestSpec {
        id: "9B", name: "cross_core_no_authority_leak", spec_ref: "§22 Test 9B",
        kind: TestKind::WatchSerial {
            expect:       &["probe: 9B pass"],
            fail_on:      &["KERNEL PANIC", "probe: 9B FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "10A", name: "restart_changes_core_transparently", spec_ref: "§22 Test 10A",
        kind: TestKind::WithRestart {
            wait_for:     "supervisor: ready", // pong/ping spawn first; wait for all probes done before restart
            restart_cmd:  "RESTART pong 2",
            expect_after: &["pong: ready on core 2"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60, // identity-only supervisor: ready ~3s; restart + core-2 ready ~5s; 60s is ~6× margin
        },
    },
    TestSpec {
        id: "10B", name: "client_reacquires_after_core_change", spec_ref: "§22 Test 10B",
        kind: TestKind::WithRestart {
            wait_for:    "supervisor: ready", // pong/ping spawn first; wait for all probes done before restart
            restart_cmd: "RESTART pong 2",
            expect_after: &[
                "ping: pong endpoint dead, reacquiring via the kernel name directory",
                "ping: pong cap reacquired, resuming",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60, // identity-only supervisor: ready ~3s; reacquisition phase ~5s; 60s is ~6× margin
        },
    },
    // ----------------------------------------------------------------
    // Interrupt-routing identity tests - §12.2, §12.3.
    // ----------------------------------------------------------------
    TestSpec {
        id: "IR1A", name: "irq_delivery_driver_receives", spec_ref: "§12.2 §12.3",
        kind: TestKind::WithRestart {
            wait_for:     "probe: 11A ready",   // probe is alive and blocking on recv
            restart_cmd:  "FIRE_IRQ 33",        // inject IRQ 33 via COM2 control channel
            expect_after: &["probe: 11A pass irq=33"],
            fail_on:      &["KERNEL PANIC", "probe: 11A FAIL"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        id: "IR1B", name: "irq_unregistered_discard_no_panic", spec_ref: "§12.2",
        kind: TestKind::WithRestart {
            wait_for:     "supervisor: ready",  // system fully up
            restart_cmd:  "FIRE_IRQ 34",        // no driver registered for IRQ 34
            expect_after: &["control: FIRE_IRQ 34"],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        // §22 Test 11: the kernel name-directory is the namer. This pins the directory's restart
        // property - a service killed and respawned must re-establish without a kernel panic, and
        // the supervisor
        // re-wires it from its map (the directory records the new instance, so clients reacquire it
        // by name). `block-driver` is the disk-free restartable target.
        id: "11", name: "name_resolves_after_restart_via_directory", spec_ref: "§22 Test 11",
        kind: TestKind::WithRestart {
            wait_for:     "supervisor: ready",
            restart_cmd:  "KILL block-driver",
            expect_after: &[
                "supervisor: block-driver died, restarting",
                "supervisor: block-driver restarted",
                "name-map + block-driver",       // re-recorded → directory resolves the new instance
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 60,
        },
    },
    TestSpec {
        // §22 Test 15 (Path C / Phase 6): the SUPERVISOR is restartable - its death is no longer a
        // kernel panic. Killing it (via the operator control channel) must NOT panic the kernel; the
        // KERNEL respawns it (the last-resort recovery anchor, §3.7); the respawned supervisor
        // RECONCILES - adopts the still-running services (e.g. block-driver) instead of duplicating
        // them - and reaches "ready" again. Proves the unkillable set is now {kernel} alone (§6.2).
        id: "15", name: "supervisor_survives_own_restart", spec_ref: "§22 Test 15",
        kind: TestKind::WithRestart {
            wait_for:     "supervisor: ready",
            restart_cmd:  "KILL supervisor",
            // Both markers are RESPAWN-ONLY (a fresh boot logs neither): "respawning" proves the
            // kernel is the recovery anchor; "adopted running block-driver" proves the respawned
            // supervisor reconciled the live services (the last step of its boot, just before it
            // re-enters its loop) instead of panicking or duplicating them. (We avoid asserting
            // "supervisor: ready" - that string also appears on the first boot, before the kill.)
            expect_after: &[
                "kernel: supervisor died - respawning",
                "supervisor: adopted running block-driver",
            ],
            fail_on:      &["KERNEL PANIC"],
            timeout_secs: 90,
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
                eprintln!("FAIL {} - {}", contract_path.display(), e);
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

    // Build with identity-only supervisor: spawns only the 15 identity probe
    // services so supervisor: ready appears in < 10 s on TCG (vs 30-200 s with
    // the full 160+ probe set).  This is the primary flakiness fix for
    // WithRestart tests whose deadline budget was being eaten by the spawn loop.
    println!("identity: building (identity-only supervisor)...");
    crate::cmd_build_identity();

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

        // Isolation: give the OS 500 ms to reclaim the QEMU process's 512 MiB
        // pages before the next QEMU instance starts.  Prevents accumulated
        // memory pressure from degrading boot times across back-to-back tests.
        std::thread::sleep(Duration::from_millis(500));
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

/// Boot the OS in QEMU and run the Milestone 13 adversarial / red-team test suite.
///
/// Pass criterion: each attack logs `adv: AN pass` without a kernel panic.
pub fn run_adv_tests() {
    println!("adv: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("adv: building...");
    // Build the LEAN adv-only supervisor (11 self-contained probes), not the full
    // ~180-probe set. The full boot takes 18-120 s under TCG, far past the 30 s
    // per-test timeout, so every adv test would time out before `supervisor: ready`.
    // (Mirrors run_identity_tests → cmd_build_identity.)
    crate::cmd_build_adv();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("adv: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/6_ADVERSARIAL")
        .expect("create build/tests/6_ADVERSARIAL/");

    println!("\nadv: running {} attacks\n", ADV_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in ADV_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_adv_one(test, &image_path);

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

/// Boot the OS in QEMU and run the Milestone 14 chaos test suite.
///
/// C1 and C4 use degraded QEMU environments (fewer cores, less RAM).
/// C2-C3, C5-C7 use the standard 4-core 512M image.
/// Pass criterion: each scenario logs its `chaos: CN pass` marker without a kernel panic.
pub fn run_chaos_tests() {
    println!("chaos: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("chaos: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("chaos: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/7_CHAOS").expect("create build/tests/7_CHAOS/");

    println!("\nchaos: running {} tests\n", CHAOS_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in CHAOS_TESTS {
        print!("  [{:>2}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_chaos_one(test, &image_path);

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

/// Boot the OS in QEMU and run the Milestone 21 brutal chaos test suite.
///
/// BC1 and BC4 use degraded QEMU environments (fewer cores, less RAM).
/// BC2-BC3, BC5-BC7 use the standard 4-core image.
/// Pass criterion: each scenario logs its `chaos: BCN pass` marker without a kernel panic.
pub fn run_chaos_brutal_tests() {
    println!("chaos-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("chaos-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("chaos-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/14_CHAOS_BRUTAL")
        .expect("create build/tests/14_CHAOS_BRUTAL/");

    println!("\nchaos-brutal: running {} tests\n", BRUTAL_CHAOS_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_CHAOS_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_chaos_brutal_one(test, &image_path);

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

/// Boot the OS in QEMU and run the Milestone 15 brutal identity test suite.
///
/// T11-T13 are correctness tests that must always pass.
/// SMP-2 / SMP-8 / SMP-16 are escalation tests: they run until the machine
/// ceiling is found - the first timeout is the hardware limit, not a failure.
pub fn run_brutal_identity_tests() {
    println!("identity-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("identity-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("identity-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/8_IDENTITY_BRUTAL")
        .expect("create build/tests/8_IDENTITY_BRUTAL/");

    println!("\nidentity-brutal: running {} tests\n", BRUTAL_IDENTITY_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_IDENTITY_TESTS {
        print!("  [{:>5}]  {:50}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_identity_one(test, &image_path);

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

    // Only the correctness tests (T11-T13) are hard failures.
    // SMP escalation timeouts are expected at the machine ceiling.
    let correctness_failed = results.iter().filter(|(t, o)| {
        !t.id.starts_with("SMP") && matches!(o, TestOutcome::Fail(_))
    }).count();

    if correctness_failed > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and run the Milestone 16 brutal property test suite.
///
/// BP1-BP10 are 5-10× escalated-iteration variants of P1-P10. All must pass.
pub fn run_brutal_property_tests() {
    println!("property-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("property-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("property-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/9_PROPERTY_BRUTAL")
        .expect("create build/tests/9_PROPERTY_BRUTAL/");

    println!("\nproperty-brutal: running {} tests\n", BRUTAL_PROPERTY_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_PROPERTY_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_property_one(test, &image_path);

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

/// Boot the OS in QEMU and run the Milestone 17 brutal fuzz test suite.
///
/// BF1/BF2/BF5-BF8 are QEMU-based fuzz probes (5-10× iteration escalation).
/// BF3 builds with `test-bad-elf-brutal` and halts after 263 inputs.
/// BF4 is host-side contract validator fuzz with 30+ bad inputs.
/// All must pass (no kernel panic, no FAIL marker).
pub fn run_brutal_fuzz_tests() {
    println!("fuzz-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("fuzz-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("fuzz-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/10_FUZZ_BRUTAL")
        .expect("create build/tests/10_FUZZ_BRUTAL/");

    println!("\nfuzz-brutal: running {} tests\n", BRUTAL_FUZZ_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_FUZZ_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_fuzz_one(test, &image_path, limine_dir);

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

/// Boot the OS in QEMU and run the Milestone 18 brutal stress test suite.
///
/// BS1-BS10 are 4-5× escalated-iteration variants of S1-S10. All must pass
/// (no kernel panic, no FAIL marker, no resource leak).
pub fn run_brutal_stress_tests() {
    println!("stress-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("stress-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("stress-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/11_STRESS_BRUTAL")
        .expect("create build/tests/11_STRESS_BRUTAL/");

    println!("\nstress-brutal: running {} tests\n", BRUTAL_STRESS_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_STRESS_TESTS {
        print!("  [{:>3}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_stress_one(test, &image_path);

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

/// Boot the OS in QEMU and run the Milestone 19 brutal performance benchmark suite.
///
/// Pass criterion: each benchmark logs `perf: BPN done` without panicking.
/// After all benchmarks pass, extracted RDTSC metrics are written to
/// `build/tests/12_PERFORMANCE_BRUTAL/baseline.json`.
pub fn run_brutal_perf_tests() {
    println!("perf-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("perf-brutal: building...");
    crate::cmd_build_brutal_perf();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("perf-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/12_PERFORMANCE_BRUTAL")
        .expect("create build/tests/12_PERFORMANCE_BRUTAL/");

    println!("\nperf-brutal: running {} benchmarks\n", BRUTAL_PERF_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_PERF_TESTS {
        print!("  [{:>4}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_perf_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n          → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n          → {r}"),
        }

        results.push((test, outcome));
    }

    let passed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();

    println!("\n  {passed} passed  {failed} failed");

    if passed > 0 {
        collect_brutal_perf_baseline(&results);
    }

    if failed > 0 { std::process::exit(1); }
}

/// Boot the OS in QEMU and run the Milestone 12 performance benchmark suite.
///
/// Pass criterion: each benchmark logs `perf: BN done` without panicking.
/// After all benchmarks pass, extracted RDTSC metrics are written to
/// `tests/qemu/perf/baseline.json` for future regression comparisons.
pub fn run_perf_tests() {
    run_perf_tests_filtered(None);
}

pub fn run_perf_tests_filtered(filter: Option<&str>) {
    println!("perf: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("perf: building...");
    crate::cmd_build_perf();

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

    let tests: Vec<&TestSpec> = PERF_TESTS.iter()
        .filter(|t| filter.map_or(true, |f| t.id.eq_ignore_ascii_case(f)))
        .collect();

    if tests.is_empty() {
        eprintln!("perf: no tests match filter {:?}", filter);
        std::process::exit(1);
    }

    println!("\nperf: running {} benchmark(s)\n", tests.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in &tests {
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
        "note": "QEMU TCG RDTSC cycle counts - not comparable across hosts or QEMU versions; useful for detecting large regressions within one environment",
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
        TestKind::WithBadElfBrutal { .. } => TestOutcome::Blocked("WithBadElfBrutal only runs via osdev test fuzz-brutal"),
        TestKind::ContractFuzz { .. } => TestOutcome::Blocked("ContractFuzz only runs via osdev test fuzz or fuzz-brutal"),

        TestKind::WithBadTcb { expect, fail_on, timeout_secs } => {
            // Build kernel with the test-bad-supervisor feature (invalid supervisor ELF): the
            // supervisor is the corrupt-and-fail TCB.
            let status = std::process::Command::new("cargo")
                .args([
                    "build", "--release", "-p", "kernel",
                    "--target", "x86_64-unknown-none",
                    "--features", "kernel/test-bad-supervisor",
                ])
                .status()
                .expect("failed to build kernel with test-bad-supervisor");
            if !status.success() {
                return TestOutcome::Fail(
                    "kernel build with test-bad-supervisor feature failed".to_string()
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
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }

        TestKind::DegradedSmp { .. } =>
            TestOutcome::Blocked("DegradedSmp only runs via osdev test chaos"),
        TestKind::DegradedEnv { .. } =>
            TestOutcome::Blocked("DegradedEnv only runs via osdev test chaos"),

        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = serial_path(test);
            // Truncate so poll_serial doesn't match stale content from a previous run.
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }

        TestKind::WithRestart { wait_for, restart_cmd, expect_after, fail_on, timeout_secs } => {
            let serial   = serial_path(test);
            // Truncate so poll_serial doesn't match stale content from a previous run.
            let _ = std::fs::write(&serial, b"");
            let deadline = Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale());
            // Allocate a unique port so back-to-back WithRestart tests don't collide on
            // a socket still in TIME_WAIT from the just-killed QEMU instance.
            let ctrl_port = NEXT_CONTROL_PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let qemu     = crate::qemu::spawn_for_test(image, 4, &serial, Some(ctrl_port));

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
            // Keep the TcpStream alive for the duration of the post-restart poll:
            // dropping it immediately after write_all causes QEMU to tear down
            // the TCP→COM2 bridge before the kernel reads the UART FIFO, silently
            // losing the command.  _ctrl_stream is dropped after poll_serial exits.
            let addr = format!("127.0.0.1:{ctrl_port}");
            let _ctrl_stream = match std::net::TcpStream::connect(&addr) {
                Ok(mut s) => {
                    std::thread::sleep(Duration::from_millis(50));
                    let _ = s.write_all(format!("\n{restart_cmd}\n").as_bytes());
                    Some(s)
                }
                Err(e) => {
                    qemu.kill();
                    return TestOutcome::Fail(
                        format!("could not connect to control port {addr}: {e}")
                    );
                }
            };

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
        let content = match std::fs::read_to_string(path) {
            Ok(s)  => s,
            Err(_) => {
                // File may be transiently locked by QEMU on Windows (exclusive
                // write handle on the serial file). Retry until the deadline,
                // then fall through to the same 600ms grace period used for
                // normal content timeouts so we never report a different error
                // class just because the read raced with QEMU's file handle.
                if Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                // At deadline: wait for QEMU to flush and release, then do a
                // final read - same grace period as the normal timeout branch.
                std::thread::sleep(Duration::from_millis(600));
                let content = std::fs::read_to_string(path).unwrap_or_default();
                let missing: Vec<String> = expect.iter()
                    .filter(|e| !content.contains(**e))
                    .map(|e| format!("\"{e}\""))
                    .collect();
                if missing.is_empty() {
                    return TestOutcome::Pass;
                }
                return TestOutcome::Fail(format!(
                    "timeout - lines not seen: {}",
                    missing.join(", ")
                ));
            }
        };

        for &line in fail_on {
            if content.contains(line) {
                return TestOutcome::Fail(format!("saw fail marker: \"{line}\""));
            }
        }

        if expect.iter().all(|e| content.contains(e)) {
            return TestOutcome::Pass;
        }

        if Instant::now() >= deadline {
            // QEMU buffers serial writes; give it 600ms to flush before the
            // final check so we don't report a false timeout.
            std::thread::sleep(Duration::from_millis(600));
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let missing: Vec<String> = expect.iter()
                .filter(|e| !content.contains(**e))
                .map(|e| format!("\"{e}\""))
                .collect();
            if missing.is_empty() {
                return TestOutcome::Pass;
            }
            return TestOutcome::Fail(format!(
                "timeout - lines not seen: {}",
                missing.join(", ")
            ));
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
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
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
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
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
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
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
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("stress tests only use WatchSerial or Blocked"),
    }
}

fn run_adv_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = adv_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("adv tests only use WatchSerial"),
    }
}

fn run_chaos_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = chaos_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        TestKind::DegradedSmp { smp, expect, fail_on, timeout_secs } => {
            let serial = chaos_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test_custom(image, *smp, 512, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        TestKind::DegradedEnv { smp, ram_mib, expect, fail_on, timeout_secs } => {
            let serial = chaos_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test_custom(image, *smp, *ram_mib, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("chaos tests use WatchSerial, DegradedSmp, or DegradedEnv"),
    }
}

fn run_brutal_property_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_property_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal property tests only use WatchSerial"),
    }
}

fn run_brutal_identity_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_identity_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        TestKind::DegradedSmp { smp, expect, fail_on, timeout_secs } => {
            let serial = brutal_identity_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test_custom(image, *smp, 512, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal identity tests use WatchSerial or DegradedSmp"),
    }
}

fn run_brutal_fuzz_one(test: &TestSpec, image: &Path, limine_dir: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_fuzz_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }

        TestKind::WithBadElfBrutal { expect, fail_on, timeout_secs } => {
            let status = std::process::Command::new("cargo")
                .args([
                    "build", "--release", "-p", "kernel",
                    "--target", "x86_64-unknown-none",
                    "--features", "kernel/test-bad-elf-brutal",
                ])
                .status()
                .expect("failed to invoke cargo");
            if !status.success() {
                return TestOutcome::Fail(
                    "kernel build with test-bad-elf-brutal feature failed".to_string()
                );
            }

            let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
            let bad_img    = Path::new("build/tests/10_FUZZ_BRUTAL/BF3-bad-elf-brutal.img");
            let bad_image  = crate::disk_image::create_at(kernel_elf, limine_dir, bad_img);
            crate::disk_image::install_bootloader(limine_dir, &bad_image);

            let serial = brutal_fuzz_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(&bad_image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
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

        _ => TestOutcome::Blocked("brutal fuzz tests use WatchSerial, WithBadElfBrutal, or ContractFuzz"),
    }
}

fn run_brutal_stress_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_stress_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal stress tests only use WatchSerial"),
    }
}

fn run_perf_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = perf_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("perf tests only use WatchSerial"),
    }
}

fn serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/1_IDENTITY/{}-{}.log", test.id, test.name))
}

fn brutal_property_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/9_PROPERTY_BRUTAL/{}-{}.log", test.id, test.name))
}

fn brutal_identity_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/8_IDENTITY_BRUTAL/{}-{}.log", test.id, test.name))
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

fn adv_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/6_ADVERSARIAL/{}-{}.log", test.id, test.name))
}

fn chaos_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/7_CHAOS/{}-{}.log", test.id, test.name))
}

fn chaos_brutal_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/14_CHAOS_BRUTAL/{}-{}.log", test.id, test.name))
}

fn run_chaos_brutal_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = chaos_brutal_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        TestKind::DegradedSmp { smp, expect, fail_on, timeout_secs } => {
            let serial = chaos_brutal_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test_custom(image, *smp, 512, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        TestKind::DegradedEnv { smp, ram_mib, expect, fail_on, timeout_secs } => {
            let serial = chaos_brutal_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test_custom(image, *smp, *ram_mib, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal chaos tests use WatchSerial, DegradedSmp, or DegradedEnv"),
    }
}

fn brutal_fuzz_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/10_FUZZ_BRUTAL/{}-{}.log", test.id, test.name))
}

fn brutal_stress_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/11_STRESS_BRUTAL/{}-{}.log", test.id, test.name))
}

fn brutal_perf_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/12_PERFORMANCE_BRUTAL/{}-{}.log", test.id, test.name))
}

fn run_brutal_perf_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_perf_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal perf tests only use WatchSerial"),
    }
}

fn collect_brutal_perf_baseline(results: &[(&TestSpec, TestOutcome)]) {
    let mut metrics = serde_json::Map::new();

    for (test, outcome) in results {
        if !matches!(outcome, TestOutcome::Pass) { continue; }
        let serial  = brutal_perf_serial_path(test);
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
        "note": "Brutal perf - QEMU TCG RDTSC cycle counts at 5× iteration counts; not comparable across hosts",
        "regression_threshold_pct": 10,
        "metrics": metrics,
    });

    let out = serde_json::to_string_pretty(&baseline).unwrap_or_default();
    let path = "build/tests/12_PERFORMANCE_BRUTAL/baseline.json";
    match std::fs::write(path, &out) {
        Ok(()) => println!("perf-brutal: baseline written to {path}"),
        Err(e) => eprintln!("perf-brutal: could not write baseline.json: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Brutal adversarial test definitions (Milestone 20).
// ---------------------------------------------------------------------------

static BRUTAL_ADV_TESTS: &[TestSpec] = &[
    TestSpec {
        id: "BA1", name: "cap_forgery_50k", spec_ref: "§22 Brutal Adv BA1",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA1 pass (50000/50000)"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "adv: BA1 FAIL"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA2", name: "slot_sweep_extended", spec_ref: "§22 Brutal Adv BA2",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA2 pass - extended slot sweep returned defined errors"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA3", name: "alloc_edge_cycles_5x", spec_ref: "§22 Brutal Adv BA3",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA3 pass \u{2014} 5\u{d7} alloc edge cycles rejected without panic"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA4", name: "recv_cap_as_send_5x", spec_ref: "§22 Brutal Adv BA4",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA4 pass \u{2014} 5\u{d7} RECV-cap-as-SEND rejected; non-SEND caps rejected"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA5", name: "toctou_kill_send_5x", spec_ref: "§22 Brutal Adv BA5",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA5 pass"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:", "adv: BA5 FAIL"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA6", name: "cap_table_fill_5x", spec_ref: "§22 Brutal Adv BA6",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA6 pass \u{2014} 5\u{d7} cap-table fill returned None without panic"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA7", name: "timing_side_channel_500", spec_ref: "§22 Brutal Adv BA7",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA7 pass - 500 timing sends completed without panic"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA8", name: "hog_preemption_witness", spec_ref: "§22 Brutal Adv BA8",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA8 pass - witness ran 200 yields despite tight-loop hog"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA9", name: "direct_spawn_bypass_5x", spec_ref: "§22 Brutal Adv BA9",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA9 pass - 5 direct-spawn bypasses returned Err"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
    TestSpec {
        id: "BA10", name: "kernel_addr_patterns_20x", spec_ref: "§22 Brutal Adv BA10",
        kind: TestKind::WatchSerial {
            expect:       &["adv: BA10 pass - 20 kernel addr patterns rejected without panic"],
            fail_on:      &["KERNEL PANIC", "KERNEL PF:"],
            timeout_secs: 900,
        },
    },
];

pub fn run_brutal_adv_tests() {
    println!("adv-brutal: stopping any running QEMU instances...");
    kill_existing_qemu();

    println!("adv-brutal: building...");
    crate::cmd_build();

    let kernel_elf = Path::new("target/x86_64-unknown-none/release/kernel");
    if !kernel_elf.exists() {
        eprintln!("adv-brutal: kernel ELF not found at {}", kernel_elf.display());
        std::process::exit(1);
    }

    let limine_dir = Path::new("tools/limine");
    let image_path = crate::disk_image::create(kernel_elf, limine_dir);
    crate::disk_image::install_bootloader(limine_dir, &image_path);

    std::fs::create_dir_all("build/tests/13_ADVERSARIAL_BRUTAL")
        .expect("create build/tests/13_ADVERSARIAL_BRUTAL/");

    println!("\nadv-brutal: running {} tests\n", BRUTAL_ADV_TESTS.len());

    let mut results: Vec<(&TestSpec, TestOutcome)> = Vec::new();

    for test in BRUTAL_ADV_TESTS {
        print!("  [{:>4}]  {:45}  ({})  … ", test.id, test.name, test.spec_ref);
        let _ = std::io::stdout().flush();

        let outcome = run_brutal_adv_one(test, &image_path);

        match &outcome {
            TestOutcome::Pass       => println!("PASS"),
            TestOutcome::Fail(r)    => println!("FAIL\n          → {r}"),
            TestOutcome::Blocked(r) => println!("BLOCKED\n          → {r}"),
        }

        results.push((test, outcome));
    }

    let passed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Pass)).count();
    let failed = results.iter().filter(|(_, o)| matches!(o, TestOutcome::Fail(_))).count();

    println!("\n  {passed} passed  {failed} failed");

    if failed > 0 { std::process::exit(1); }
}

fn brutal_adv_serial_path(test: &TestSpec) -> PathBuf {
    PathBuf::from(format!("build/tests/13_ADVERSARIAL_BRUTAL/{}-{}.log", test.id, test.name))
}

fn run_brutal_adv_one(test: &TestSpec, image: &Path) -> TestOutcome {
    match &test.kind {
        TestKind::WatchSerial { expect, fail_on, timeout_secs } => {
            let serial = brutal_adv_serial_path(test);
            let _ = std::fs::write(&serial, b"");
            let qemu   = crate::qemu::spawn_for_test(image, 4, &serial, None);
            let result = poll_serial(&serial, expect, fail_on,
                                     Instant::now() + Duration::from_secs(*timeout_secs * crate::qemu::timeout_scale()));
            qemu.kill();
            result
        }
        _ => TestOutcome::Blocked("brutal adv tests only use WatchSerial"),
    }
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
// Contract validation (§13.4) - stubs.
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
