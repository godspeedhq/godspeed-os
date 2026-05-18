# Post-v1 Item 7 — Identity CI Green (20/20)

**Goal:** All 20 identity tests pass consistently in GitHub Actions CI.

---

## Root Cause

QEMU was running in pure TCG (software emulation) mode — no `-enable-kvm` flag.
With ~185 services spawned during boot, the supervisor sequence consumed 20–25 s of
wall-clock time in TCG, exhausting the 30 s per-test timeout before ping/pong could
exchange their first message.

The 6 consistently-failing tests all depended on cross-core IPC between ping (core 0)
and pong (core 1):

| Test | Waited for | Result |
|------|-----------|--------|
| 6A  `supervisor_restart_positive`    | `"pong: received"` then restart   | timeout |
| 6B  `stale_cap_revoked_after_restart`| `"pong: received"` then restart   | timeout |
| 8B  `non_yielding_service_preempted` | `"ping: sent 100 messages"`       | timeout |
| 9A  `cross_core_ipc_positive`        | `"pong: ready on core"` + `"pong: received"` | timeout (`"pong: ready on core"` appeared; `"pong: received"` did not) |
| 10A `restart_changes_core_transparently` | `"pong: received"` then restart | timeout |
| 10B `client_reacquires_after_core_change` | `"pong: received"` then restart | timeout |

Confirming signal: `"pong: ready on core"` appeared in test 9A serial output but
`"pong: received"` did not — pong started but ping never got a CPU turn before the clock expired.

## Fix

**`osdev/src/qemu.rs`** — `kvm_available()` detects `/dev/kvm` at runtime;
`spawn_for_test`, `spawn_for_test_custom`, and `run` all pass
`-enable-kvm -cpu host` when KVM is accessible.  Falls back silently to TCG
on Windows and on Linux hosts without KVM (e.g., nested VMs).

**`.github/workflows/identity.yml`** — Added "Enable KVM access" step (udev rule
to make `/dev/kvm` world-accessible on the GitHub runner) and serial log upload
artifact on failure for post-mortem diagnosis.

GitHub Actions `ubuntu-latest` runners have had KVM support since 2023.
With KVM, boot takes ~3–5 s, well within the 30 s per-test timeout.

## Status

✅ **Complete** — 20/20 identity tests passing locally (Windows TCG, 3/3 consecutive rounds) and in CI (Linux KVM).

### Additional fixes (session 2)

**`kernel/src/task/scheduler.rs`** — Deferred PML4 frame free in self-kill path to prevent
CR3 UAF KERNEL PF. Root cause: in the self-kill path the dying task's CR3 is still active
on the core when `reclaim_user_frames` hands the PML4 frame to `free_frame`. Another core's
`PageTable::new()` can immediately alloc and zero that frame. On a TLB miss on any kernel VA
(e.g. reading a `.rodata` format string) the hardware page-walker reads the zeroed PML4 →
entry 511 = 0 → "not present" → KERNEL PF. Fix: skip the PML4 frame in the self-kill reclaim
loop; store it in `CORE_PENDING_PML4[my_core]`; free it at the next `drain_pending_kstack`
call (timer tick or scheduler idle) when a different CR3 is already loaded.

**`osdev/src/validator.rs`** — Fixed test 10B `timeout_secs` 60 → 120. Pong spawns at ~70 s
in the test sequence, making the 60 s deadline unreachable.

### Additional fixes (session 3 — Windows TCG determinism)

Root cause of remaining flakiness: supervisor spawns 100+ probe services before pong/ping,
consuming ~25–30 s on Windows TCG. Tests with `timeout_secs: 120` that depended on
`"pong: received"` could see that string appear at t=80–100 s, leaving too little margin for
the `WithRestart` expect_after phase (another 30–40 s for kill + spawn + reacquisition on TCG).

Confirmed failing patterns from diagnostic runs:
- 6A: `wait_for` satisfied but `"control: pong restarted"` not seen before deadline
- 6B: `wait_for` satisfied but ping reacquisition strings not seen before deadline
- 10A: `"pong: received"` itself timed out on a slow TCG run

**`osdev/src/validator.rs`** — Increased timeouts for all pong-communication-dependent tests:

| Test | Old | Final | Reason |
|------|-----|-------|--------|
| 1A   | 30s | 120s  | supervisor spawns 178 probes before "ready"; loop takes up to 90s on loaded TCG |
| 7A   | 30s |  60s  | competing probe services delayed probe service output |
| 7B   | 30s |  60s  | same |
| 8A   | 60s | 120s  | yielder competes with 100+ probes for scheduler quanta |
| 8B   | 120s| 240s  | ping blocks on send when pong queue fills; unblocks only when pong spawns |
| 6A   | 120s| 300s  | WithRestart: 178 probes before pong; pong receive can appear at t≈175-180s, leaving <5s for restart phase |
| 6B   | 120s| 300s  | same + reacquisition phase (ping EndpointDead → registry lookup → cap reacquired) |
| 9A   | 120s| 300s  | WatchSerial: pong first receive at t≈175-180s on slow TCG runs |
| 10A  | 120s| 300s  | WithRestart: same as 6A |
| 10B  | 120s| 300s  | same as 6B |

Root cause: supervisor spawns 178 probe services before pong/ping on a sequential loop. Each
spawn takes ~100ms wall on TCG; 178 × 100ms = 17.8s for spawns, plus scheduling contention
after spawning means pong's first receive can occur anywhere from t=40s to t=180s depending on
system load. The 300s ceiling covers all observed cases with substantial margin.

KVM (CI): tests complete in <30s each; the 300s ceiling is never reached.
Windows TCG: generous ceiling covers variance without impacting fast runs.

### Additional fixes (session 4 — structural fix)

Root cause of back-to-back run failures: even with 300s timeouts, 3+ consecutive full suite runs
accumulate system load (60 QEMU instances) that could push `"pong: received"` to t≈175-180s, leaving
only ~120s for the restart phase of `WithRestart` tests. Occasionally the restart phase itself took
>120s under extreme load, causing 1–4 failures per batch on run 1.

**Structural fix applied:**

**`services/supervisor/src/main.rs`** — Moved pong/ping spawns to the **beginning** of `service_main`,
before all 178 probe spawns. `"supervisor: ready"` remains logged after all probes complete.
- Before: pong and ping spawned at lines 281-287 (after 178 probe spawns)
- After: pong and ping spawned first (lines 17-27), `"supervisor: ready"` still after all probes

With this change, `"pong: received"` appears at t≈5-10s instead of t≈40-180s on any load level.

**`osdev/src/validator.rs`** — `WithRestart` tests changed to trigger restart on `"supervisor: ready"`
(after probe-spawn loop ends, supervisor safely in yield loop) instead of `"pong: received"` (could
fire mid-spawn-loop). Timeouts cut significantly:

| Test | Session 3 | Session 4 | Change |
|------|-----------|-----------|--------|
| 6A   | 300s      | 180s      | `wait_for` → `"supervisor: ready"`; restart phase ≤30s; total ≤150s |
| 6B   | 300s      | 180s      | same |
| 8B   | 240s      | 120s      | pong ready before ping starts; no queue-full stall |
| 9A   | 300s      |  60s      | `"pong: received"` at t≈5-10s; no probe contention |
| 10A  | 300s      | 180s      | `wait_for` → `"supervisor: ready"` |
| 10B  | 300s      | 180s      | same |

**Why `"supervisor: ready"` as the trigger (not `"pong: received"`):** if restart fires while supervisor is
still in spawn loop (mid-`ctx.spawn()` sequence), the supervisor can re-encounter pong's name during
a subsequent probe spawn. Using `"supervisor: ready"` as the gate guarantees supervisor is in its
`loop { ctx.yield_cpu(); }` idle path — no spawns in flight, no possible ordering conflict.

Total per-run wall time on Windows TCG (fresh system): drops from ~2100s to ~1200s.

### Additional fixes (session 5 — 200/200 deterministic)

**Problem:** even after the session 4 structural fix, 3–4 failures per 200 tests remained under
extreme accumulated back-to-back host load. The `WithRestart` tests (6A, 6B, 10A, 10B) still used
`"supervisor: ready"` as their gate, which required waiting for all 160+ non-identity probe services
to finish spawning on TCG — occasionally taking close to the full 240–300s deadline, leaving
insufficient margin for the restart phase.

**Root cause identified:** `osdev test identity` was building and booting the full supervisor binary
(160+ probe services) even for the 20 identity tests that only need 15 probe services. The 160+
probe spawn loop was the source of all timing variance.

**Fix 1 — `identity-only` Cargo feature (`services/supervisor/Cargo.toml`, `src/main.rs`):**
- New `identity-only = []` feature excludes all non-identity probes at compile time.
- `spawn_extended_probes()` helper contains all 160+ non-identity spawns, compiled out when
  `identity-only` is enabled.
- Result: `"supervisor: ready"` appears in ~3 s on TCG instead of 30–200 s.

**Fix 2 — `cmd_build_identity()` (`osdev/src/main.rs`):**
- New build function builds supervisor with `--features supervisor/identity-only`.
- `run_identity_tests()` calls `cmd_build_identity()` instead of `cmd_build()`.
- Full `cmd_build()` (all probes) is unchanged for property/fuzz/stress/perf/etc. test suites.

**Fix 3 — per-test QEMU isolation sleep (`osdev/src/validator.rs`):**
- 500 ms sleep after each test in the identity loop.
- Gives Windows time to reclaim the 512 MiB QEMU pages before the next QEMU instance starts.
- Prevents accumulation of memory pressure across 20 sequential QEMU instances.

**Result:** 200/200 across 10 consecutive back-to-back runs on Windows TCG. Zero failures.
Previous best was 197/200 (1–2% failure rate under extreme load). Now fully deterministic.

| Metric | Before session 5 | After session 5 |
|--------|-----------------|-----------------|
| `"supervisor: ready"` time on TCG | 30–200 s | ~3 s |
| Back-to-back 10-run score | 196–197/200 | 200/200 |
| WithRestart test margin | ~0–40 s | ~237 s |
| Per-test isolation | none | 500 ms OS reclaim pause |

### Session 6 — timeout cleanup and documentation

**Context:** session 5 fixed the root cause (identity-only supervisor). The inflated timeouts from sessions 3–4 were compensation for a problem that no longer exists. Session 6 trimmed them to reflect actual operation and recorded the verified deterministic result.

**Timeout reductions (`osdev/src/validator.rs`):**

| Test | Session 5 value | Session 6 value | Rationale |
|------|----------------|----------------|-----------|
| 4A   | 60s            | 30s            | identity-only; no probe contention |
| 6A   | 240s           | 60s            | supervisor: ready ~3s; restart phase ~5s; 60s is ~6× margin |
| 6B   | 240s           | 60s            | same as 6A |
| 10A  | 300s           | 60s            | same as 6A + core-2 ready ~5s |
| 10B  | 300s           | 60s            | same as 10A |

**Verification:** 200/200 across 10 consecutive back-to-back runs with reduced timeouts (2026-05-18, Windows TCG).

| Run | Passed | Failed |
|-----|--------|--------|
| 1–10 (each) | 20 | 0 |
| **Total** | **200** | **0** |

**Documentation updates:**

- **`tests/qemu/identity/CLAUDE.md`** — updated timeout rationale section to reference `identity-only` build, corrected timeout table, added 10-run consecutive pass record.
- **`CLAUDE.md` (root)** — replaced all 14 mermaid diagram blocks with ASCII art `text` blocks (renders in any viewer); removed ping/pong from §4.1 architectural layered view (they are demo services in `examples/`, not architectural components); clarified §15 note on stateless services.
- **`README.md`** — full refactor: ASCII architecture diagram (no ping/pong), concise principle and test-suite tables, trimmed philosophy prose.
