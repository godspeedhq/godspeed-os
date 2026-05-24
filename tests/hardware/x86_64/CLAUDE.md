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
| `bare-metal` | `bare-metal` | pong + ping only | Stability, smoke test, S6/S8 24h soak |
| `perf` | `perf-only` | B1–B10 probes | Regular perf benchmarks |
| `perf-brutal` | `perf-brutal-only` | BP1–BP10 probes | Brutal perf benchmarks |
| `identity` | `identity-only` | 15 identity probes | Identity WatchSerial tests |
| `stress` | `stress-only` | S1–S10 probes | Stress benchmarks |
| `adv` | `adv-only` | A1–A10 probes | Adversarial tests |
| `chaos` | `chaos-only` | C2–C7 probes | Chaos tests |
| `b2-only` | `b2-only` | perf-b2 + echo | B2 isolation (Goldmont+ investigation) |
| `bp2-only` | `bp2-only` | perf-bp2 + echo | BP2 isolation (Goldmont+ investigation) |

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
| `4_STRESS.md` | Stress | S1–S10 | 8/10 (S3/S9 Goldmont+ backburner); S6/S8 24h bare-metal soak pending |
| `5_PERFORMANCE.md` | Performance (regular) | B1–B10 | 9/10 (2026-05-24); B2 Goldmont+ backburner; isolation confirmed not load-dependent |
| `12_PERFORMANCE_BRUTAL.md` | Performance (brutal) | BP1–BP10 | 9/10 (2026-05-21); BP2 Goldmont+ backburner |
| `6_ADVERSARIAL.md` | Adversarial | A1–A10 | ✅ 10/10 (2026-05-24) |
| `7_CHAOS.md` | Chaos | C1–C7 | ✅ 5/5 PASS (4-core, 2026-05-24); C1 partial (2-core degraded boot verified); C4 skipped |

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
