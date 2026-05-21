# Hardware: Identity Tests

Mirrors §22 (Tests 1–10 + IR1A/IR1B). Verifies constitutional invariants on real silicon.

**Build mode:** `osdev image --mode identity`

**Reference:** `tests/qemu/identity/CLAUDE.md` for full spec. This file tracks hardware results only.

## Test kinds on hardware

| Kind | Hardware status | Blocker |
|------|----------------|---------|
| `WatchSerial` | Verifiable now — read PuTTY log for expected strings | None |
| `WithRestart` | Blocked | COM2 physical control port not yet wired |
| `WithBadTcb` | Blocked | Needs a separate corrupt-TCB image build |

## Tests

### Verifiable now (WatchSerial)

| Test | Positive | Negative | Expected serial strings | HW status |
|------|----------|----------|------------------------|-----------|
| 1A — Bootstrap | ✓ | — | `kernel: 4 cores ready`, `supervisor: ready`, `registry: ready`, `logger: ready` | Pending |
| 2A — Cap held | ✓ | — | `cap-test: 2A pass` | Pending |
| 2B — No cap | — | ✓ | `cap-test: 2B pass` | Pending |
| 2C — Wrong right | — | ✓ | `cap-test: 2C pass` | Pending |
| 2D — Revoke | — | ✓ | `cap-test: revoke pass` | Pending |
| 2E — Endpoint dead | — | ✓ | `cap-test: endpoint-dead pass` | Pending |
| 2F — Grant | ✓ | — | `cap-test: grant pass` | Pending |
| 3A — IPC send | ✓ | — | `ipc-test: enqueue ok`, `ipc-test: dequeue ok` | Pending |
| 3B — IPC negative | — | ✓ | `ipc-test: queue-empty ok`, `ipc-test: queue-full ok`, `ipc-test: endpoint-dead ok` | Pending |
| 4A — EndpointDead | ✓ | — | `probe: 4A pass — EndpointDead after kill` | Pending |
| 5A — Grant positive | ✓ | — | `probe: 5A send OK` | Pending |
| 5B — Grant negative | — | ✓ | `probe: 5B pass — CapNotGrantable` | Pending |
| 3B probe — CapInsufficient | — | ✓ | `probe: 3B pass — CapInsufficientRights` | Pending |
| IR1A — Interrupt delivery | ✓ | — | (interrupt routing strings) | Pending |
| IR1B — No-driver discard | — | ✓ | (interrupt routing strings) | Pending |

### Blocked — WithRestart (COM2 required)

| Test | Blocked by |
|------|-----------|
| 4B — Blocked sender wakes EndpointDead | No COM2 control port — probe-4b-recv never killed |
| 6A — Supervisor restart positive | No COM2 control port |
| 6B — Stale cap after restart | No COM2 control port |
| 10A — Restart changes core | No COM2 control port |
| 10B — Client reacquires after core change | No COM2 control port |

### Blocked — WithBadTcb

| Test | Blocked by |
|------|-----------|
| 1B — TCB failure panics | Needs `osdev image` to embed corrupted registry binary |

## How to verify (WatchSerial tests)

1. `osdev image --mode identity`
2. Flash to USB, boot hardware
3. Open `build/putty_serial_output.log`
4. Confirm all strings from the "Expected serial strings" column appear
5. Confirm `KERNEL PANIC` does not appear
6. Record pass/fail below

## Pass record

| Date | Tests run | Passed | Failed | Notes |
|------|-----------|--------|--------|-------|
| — | — | — | — | First hardware identity run pending |

## Unblocking WithRestart

Wire the second COM port (COM2) on the hardware as a physical control channel. The dev machine opens a serial connection to COM2 and sends `RESTART <name> <core>\n` at the right moment. The supervisor processes this identically to the QEMU TCP COM2 channel.
