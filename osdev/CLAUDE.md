# osdev/

Host-side developer CLI (§17). Builds for the developer's machine, not the kernel target.

## Commands

| Command                     | What it does |
|-----------------------------|-------------|
| `osdev new <name>`          | Scaffold a new service (dir, Cargo.toml, src/main.rs, contract) |
| `osdev build`               | Build kernel + all services for the bare-metal target |
| `osdev run [--smp N]`       | Boot in QEMU with N cores (default 4) |
| `osdev publish [service]`   | Package a service update |
| `osdev restart <service>`   | Restart a service in the running OS |
| `osdev logs <service>`      | Tail service log output |
| `osdev status <service>`    | Show service state + assigned core |
| `osdev caps <service>`      | Show held capabilities |
| `osdev test identity`       | Run §22 identity test suite (20 tests) |
| `osdev test property`       | Run property tests (P1–P10) |
| `osdev test fuzz`           | Run fuzz tests (F1–F8) |
| `osdev test stress`         | Run stress tests (S1–S10) |
| `osdev test perf`           | Run performance benchmarks (B1–B10) ✅ 10/10 |
| `osdev test perf:<ID>`      | Run a single benchmark (e.g. `perf:B2`) |
| `osdev test perf-brutal`    | Run brutal performance benchmarks (BP1–BP10) ✅ 10/10 |
| `osdev test adv`            | Run adversarial / red-team tests (A1–A10) ✅ 10/10 |
| `osdev test adv-brutal`     | Run brutal adversarial tests (BA1–BA10) ✅ 10/10 |
| `osdev test chaos`          | Run chaos / partial-failure tests (C1–C7) ✅ 7/7 |
| `osdev test chaos-brutal`   | Run brutal chaos tests (BC1–BC7) ✅ 7/7 |
| `osdev validate`            | Validate all contracts against the JSON schema |
| `osdev image`               | Build with `bare-metal` supervisor + create UEFI-bootable `build/os.img` (GPT + ESP + BOOTX64.EFI) |
| `osdev image --mode perf`   | Same image, `perf-only` supervisor (B1–B10 probes) |
| `osdev image --mode perf-brutal` | Same image, `perf-brutal-only` supervisor (BP1–BP10 probes) |
| `osdev image --mode identity` | Same image, `identity-only` supervisor (WatchSerial identity tests) |

## Files

| File             | Responsibility |
|------------------|---------------|
| `src/main.rs`    | CLI parsing (`clap`), dispatch to handlers |
| `src/validator.rs`| Contract validation + all test suite runners (identity, property, fuzz, stress, perf, adversarial, chaos, and their brutal variants) |
| `src/qemu.rs`    | QEMU launch helpers (`spawn_for_test`, `spawn_for_test_custom`) — file-based serial (`-serial file:`) on all platforms |
| `src/disk_image.rs` | UEFI GPT disk image creation: protective MBR, GPT headers (CRC32), EFI System Partition (FAT32), `BOOTX64.EFI`, `limine.conf`, `kernel.elf` |

## Build

```
cargo build -p osdev
```

No `--target` flag — this is a host binary.

## QEMU path

osdev expects `qemu-system-x86_64` to be on PATH (or at the configured path). On Windows: `C:\Program Files\qemu\qemu-system-x86_64.exe`. Serial output is captured from stdio and parsed for test assertions and log streaming.

## Iteration loop (§17)

```
edit → osdev build → osdev publish → osdev restart <service> → osdev logs <service>
```

Only the changed service restarts; the kernel and other services keep running.

## Bare-metal USB image (`osdev image`)

Creates a UEFI-bootable disk image at `build/os.img` for writing to a USB drive.

**Build mode:** Uses `supervisor/bare-metal` feature — spawns only TCB services + ping + pong. Probe services are excluded because they require the QEMU control port (COM2/TCP:5555) to complete and would stall indefinitely on real hardware.

**Image layout:**

```
build/os.img (64 MiB, GPT)
  Protective MBR (LBA 0)
  Primary GPT header (LBA 1)
  GPT partition entries (LBA 2–33)
  EFI System Partition — FAT32 (LBA 2048–131038)
    EFI/BOOT/BOOTX64.EFI   ← Limine UEFI bootloader
    limine.conf             ← timeout: -1, kernel_path: boot():/kernel.elf
    kernel.elf              ← kernel binary
  Secondary GPT entries (LBA 131039)
  Secondary GPT header (LBA 131071)
```

**Prerequisites:** `tools/limine/BOOTX64.EFI` must be present (download from Limine 12.x release zip). The file is not committed (`tools/` is gitignored).

**Writing to USB (Windows):** Use Cygwin `dd` in an elevated shell:
```
dd if=build/os.img of=/dev/sdb bs=1M
```
where `/dev/sdb` corresponds to the target `PhysicalDriveN`. Use `diskpart` → `list disk` to identify the drive number first.

**Serial console:** Connect at 115200 8N1. On successful boot, expect:
```
kernel: 4 cores ready
supervisor: ready
ping: starting
pong: ready on core 1
pong: received "1"
pong: received "2"
...
```
