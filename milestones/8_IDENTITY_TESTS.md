# Milestone 8 — Identity Test Suite (§22)

> All 10 identity tests pass. The system is the system the spec describes.

## Test Harness

- [ ] `osdev test identity` boots kernel in QEMU and streams serial
- [ ] 30 s timeout per test; FAIL with diff on mismatch
- [ ] Tests run with `-smp 4` minimum

## Tests

- [ ] **Test 1A** — Healthy multi-core boot: all TCB services reach ready within 5 s
- [ ] **Test 1B** — TCB failure panics: corrupt registry binary → `KERNEL PANIC`
- [ ] **Test 2A** — Cap enforcement positive: service with `log_write` can log
- [ ] **Test 2B** — Cap enforcement negative: service without `log_write` gets `CapNotHeld`
- [ ] **Test 3A** — IPC send/recv same core: message payload matches
- [ ] **Test 3B** — IPC without SEND right: returns `CapInsufficientRights`, queue depth 0
- [ ] **Test 4A** — Send after endpoint death returns `EndpointDead`
- [ ] **Test 4B** — Blocked sender wakes with `EndpointDead` after kill (16-deep queue filled first)
- [ ] **Test 5A** — Cap transfer with GRANT right succeeds; sender loses the cap
- [ ] **Test 5B** — Cap transfer without GRANT right returns `CapNotGrantable`
- [ ] **Test 6A** — Supervisor restart: new PID, no panic, other services alive
- [ ] **Test 6B** — Stale cap revoked after restart; fresh cap via registry works
- [ ] **Test 7A** — Memory alloc within limit: service stays alive
- [ ] **Test 7B** — Alloc beyond limit returns `AllocDenied`; protection violation kills service
- [ ] **Test 8A** — Yielding service and co-tenant both make progress
- [ ] **Test 8B** — Non-yielding tight-loop service is preempted; co-tenant gets ≥40 log lines/s
- [ ] **Test 9A** — Cross-core IPC: message from core 0 received on core 1
- [ ] **Test 9B** — No authority leak: cap forgery attempt returns `CapForgeryAttempted`
- [ ] **Test 10A** — Restart with `placement_override=2`: service moves to core 2
- [ ] **Test 10B** — Client reacquires after core change; new cap routes to core 2

## Acceptance

`osdev test identity` exits 0 with all 20 cases marked PASS.
