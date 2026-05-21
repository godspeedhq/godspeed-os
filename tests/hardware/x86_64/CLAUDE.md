# tests/hardware/x86_64/

Hardware test suite for GodspeedOS on real x86_64 silicon. Mirrors the QEMU test categories (§22) but executed via the manual flash-boot-observe cycle instead of an automated harness.

## Hardware

| Component | Spec |
|-----------|------|
| CPU | x86_64, 4 cores, ~3 GHz |
| RAM | 4 GB |
| Boot | UEFI USB (`osdev image --mode X` → `dd` to USB) |
| Serial | Null modem COM1 → PuTTY 115200 8N1, logged to `build/putty_serial_output.log` |
| Control | COM2 — not yet wired (blocks WithRestart tests) |

## Build modes

| `--mode` | Supervisor feature | Spawns | Used for |
|----------|--------------------|--------|----------|
| `bare-metal` | `bare-metal` | pong + ping only | Stability, smoke test |
| `perf` | `perf-only` | B1–B10 probes | Regular perf benchmarks |
| `perf-brutal` | `perf-brutal-only` | BP1–BP10 probes | Brutal perf benchmarks |
| `identity` | `identity-only` | 15 identity probes | Identity WatchSerial tests |

## Flash procedure

```
osdev image --mode <M>
# In elevated Cygwin shell:
dd if=build/os.img of=/dev/sdb bs=4M
# Reboot hardware from USB
```

Use `diskpart` → `list disk` to identify the correct drive number (`sdb` = PhysicalDrive1, etc.).

## Serial capture

Open PuTTY, configure Session logging: **All session output**, append to `build/putty_serial_output.log`. Leave PuTTY connected for the duration of the test.

## Test categories

| File | Category | QEMU §22 | Hardware status |
|------|----------|----------|-----------------|
| `1_IDENTITY.md` | Identity | §22 (20/20 QEMU) | Partial — WatchSerial tests verifiable; WithRestart blocked (no COM2) |
| `2_PROPERTY.md` | Property | P1–P10 | Pending |
| `3_FUZZ.md` | Fuzz | F1–F8 | Pending |
| `4_STRESS.md` | Stress | S1–S10 | Partial — S6/S8 underway |
| `5_PERFORMANCE.md` | Performance (regular) | B1–B10 | Pending — no hardware run yet |
| `12_PERFORMANCE_BRUTAL.md` | Performance (brutal) | BP1–BP10 | Partial — 5/10 complete (BP3/4/7/8/10) |
| `6_ADVERSARIAL.md` | Adversarial | A1–A10 | Pending |
| `7_CHAOS.md` | Chaos | C1–C7 | Pending — C1 first target |

## Key differences from QEMU

| QEMU harness | Hardware equivalent |
|---|---|
| Automated: QEMU boots, serial parsed programmatically | Manual: observe PuTTY log for expected strings |
| `WithRestart`: harness sends `KILL`/`RESTART` via COM2 TCP | Blocked until COM2 physical port is wired as control channel |
| `WithBadTcb`: harness boots corrupt-TCB image variant | Needs a separate corrupt-TCB `osdev image` build |
| KVM: tests complete in <30 s | Flash cycle: ~2 min per image reflash |
| Iteration: unlimited (CI reruns) | Iteration: one boot per flash |

## Verification method

For each WatchSerial test, after booting:
1. Open `build/putty_serial_output.log`
2. Confirm every expected string appears
3. Confirm no `fail_on` string appears
4. Record result in the relevant test file's pass record

## COM2 control channel (future)

WithRestart tests (identity 4B, 6A, 6B, 10A, 10B) need a second serial port wired as a control channel — the hardware equivalent of QEMU's COM2 TCP port. When available, the dev machine sends `RESTART <name> <core>\n` over COM2 to trigger supervisor restarts mid-test.
