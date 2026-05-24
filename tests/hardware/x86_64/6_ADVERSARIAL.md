# Hardware: Adversarial Tests

Mirrors §22 Adversarial Tests (A1–A10). Capability isolation under direct attack on real silicon.

**Reference:** `tests/qemu/adversarial/CLAUDE.md` for full spec.

**Status: Pending** — `adv-only` build mode ready; awaiting hardware run.

## Hardware applicability

Adversarial tests are self-contained attack probes — they attempt the attack and report success or failure via serial. No COM2 control needed. All 10 probes run unmodified on hardware via `osdev image --mode adv`.

A7 (timing side-channel) is more meaningful on real hardware than on QEMU TCG because hardware has genuine cache timing variation. A QEMU pass is necessary but not sufficient — a hardware pass is the authoritative result.

| ID | Attack | Status |
|----|--------|--------|
| A1 | Random u64 values used as caps (10000 iters) | Pending |
| A2 | Brute-force endpoint IDs across u32 space | Pending |
| A3 | Alloc beyond contract limit through every syscall path | Pending |
| A4 | Use cap with rights not held (RECV cap as SEND) | Pending |
| A5 | TOCTOU: race syscall with revocation | Pending |
| A6 | Fill cap table to DoS kernel | Pending |
| A7 | Detect IPC partner identity via timing | Pending |
| A8 | Monopolize core via tight loop (preemption verified) | Pending |
| A9 | Spawn service directly, bypassing supervisor | Pending |
| A10 | Pass kernel addresses as syscall args | Pending |

## Build mode

```
osdev image --mode adv
# Rufus DD Image mode → USB → reboot hardware, observe PuTTY
```

`adv-only` supervisor spawns pong + ping + all 13 adversarial probe tasks (A1–A10 with passive victims). No QEMU harness required — all probes are self-contained.

**Expected serial lines (any order, 10/10):**
```
adv: A1 pass (10000/10000)
adv: A2 pass — all slot values returned defined errors
adv: A3 pass — alloc beyond limit rejected without panic
adv: A4 pass — CapInsufficientRights on RECV cap used as SEND
adv: A5 pass — EndpointDead after kill
adv: A6 pass — cap table filled then rejected without panic
adv: A7 pass — timing analysis completed without panic
adv: A8 pass — witness ran despite tight-loop hog
adv: A9 pass — spawn of unknown service returned Err
adv: A10 pass — kernel addrs as syscall args rejected without panic
```

No `KERNEL PANIC` and no line containing `FAIL` allowed.

## Pass record

| Date | Completed | Notes |
|------|-----------|-------|
| — | — | No hardware adversarial runs yet |
