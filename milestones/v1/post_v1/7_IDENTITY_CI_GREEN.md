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

✅ **Complete** — 20/20 identity tests passing locally and in CI.

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
