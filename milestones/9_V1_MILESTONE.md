# Milestone 9 — v1 Complete (§23)

> All §23.2 acceptance criteria met. The system is done.

## §23.2 Acceptance Criteria

- [ ] `osdev run --smp 4` boots with 4 cores; init, supervisor, registry, logger, ping, pong reach steady state
- [ ] ping placed on core 0; pong placed on core 1
- [ ] `osdev logs ping` shows ping sending a message every second
- [ ] `osdev logs pong` shows pong receiving each message (cross-core IPC confirmed)
- [ ] `osdev restart pong --core 2` kills pong on core 1, respawns on core 2
- [ ] ping observes `EndpointDead`, reacquires via registry; new cap routes to core 2
- [ ] After reacquisition, ping and pong continue communicating across the new core boundary
- [ ] Kernel does not panic on any core throughout the above sequence
- [ ] All 10 identity tests in §22 pass (`osdev test identity` exits 0)

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
