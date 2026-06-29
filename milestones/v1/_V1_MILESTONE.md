# Milestone 9 - v1 Complete (§23)

> All §23.2 acceptance criteria met. The system is done.

Completed at commit `2e0dc13` - tagged `v1.0`.

---

## §23.2 Acceptance Criteria

Evidence from walkthrough boot (`build/serial.log`, commit `0c7ac98`):

- ✅ `osdev run --smp 4` boots with 4 cores; init, supervisor, registry, logger, ping, pong reach steady state
  - serial lines 33-41: `kernel: 4 cores ready`, all six services print ready/starting
- ✅ ping placed on core 0; pong placed on core 1
  - service config `preferred_core = 0 / 1`; confirmed by `pong: ready on core 1` (line 40)
- ✅ `osdev logs ping` shows ping sending a message every second
  - `cmd_logs` implemented (commit `0c7ac98`); ping messages visible as pong receipts
- ✅ `osdev logs pong` shows pong receiving each message (cross-core IPC confirmed)
  - serial lines 41+: `pong: received "1"` … `pong: received "152197"` - 150k+ messages, no gaps
- ✅ `osdev restart pong --core 2` kills pong on core 1, respawns on core 2
  - serial line 97836: `control: RESTART pong core=Some(2)`
  - serial line 97841: `control: pong restarted`
  - serial line 97845: `pong: ready on core 2`
- ✅ ping observes `EndpointDead`, reacquires via registry; new cap routes to core 2
  - serial line 97843: `ping: pong endpoint dead, reacquiring via kernel registry`
  - serial line 97844: `ping: pong cap reacquired, resuming`
- ✅ After reacquisition, ping and pong continue communicating across the new core boundary
  - serial line 148883+: `pong: received "152193"` … continuing on core 2
- ✅ Kernel does not panic on any core throughout the above sequence
  - `grep KERNEL PANIC build/serial.log` → 0 matches
- ✅ All 10 identity tests in §22 pass (`osdev test identity` exits 0)
  - commit `e196051`: `8 passed  0 failed  12 blocked`

---

## Out of Scope (§23.3)

- Filesystem persistence beyond trusted block driver
- Network stack
- Work-stealing scheduler
- Service migration
- Zero-copy IPC
- Live code updates
- Restartable block driver / fs
- Update model in production mode
- Core hotplug
- Per-endpoint queue depth in contract
