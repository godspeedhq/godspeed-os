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
| `osdev test shell`          | Scripted shell smoke-test: boot, help, cores, status, unknown |
| `osdev test files`          | Files/records/pipes/`result`/`run`/`assert` over a RAW AHCI disk (129 checks) |
| `osdev test fs-corrupt`     | GSFS0006 integrity + backup superblock: corrupt the primary superblock (→ **recovers from the backup**), both copies (→ loud "no filesystem"), a root directory block (→ loud dir-CRC mismatch, no garbage), and a file **data block** (→ loud data-CRC mismatch, read refused); asserts no panic (§3.12). 14 checks |
| `osdev test fs-check`       | fsck / `drives check` (Phase G): boot a disk whose superblock free count was drifted host-side (both copies, CRC re-stamped); `drives check` rebuilds the correct free count + bitmap from the tree, reports 0 bad, the file survives. 5 checks |
| `osdev test fs-ioretry`     | block I/O retry (Phase H): `io-error-test` build forces the first read/write commands to fail; block-driver retries + recovers the transient (boot self-test read succeeds, fs round-trips), no panic. 5 checks |
| `osdev test fs-large`       | Large files: write + read a 200 KiB file in streaming chunks (WriteNew/WriteAt/ReadAt), then re-verify it across a reboot on the same disk (boot 1 writes, boot 2 re-reads). Proves the streaming path + durability |
| `osdev test fs-journal`     | Crash-consistency: (1) a `journal-crash-test` build halts right after a transaction's commit record is durable; the next boot's mount REPLAYS it from the journal (file recovered exactly). (2) a normal build REJECTS a journal commit with a bad CRC (no replay, mounts clean). 11 checks |
| `osdev test fs-restart`     | §22 Test 13 (Phase D): fs survives its own restart. Shell writes a file, `KILL fs` over the control channel, supervisor respawns fs, fs re-mounts + re-registers, the shell reacquires fs via the registry and reads the file back; no panic. 7 checks |
| `osdev test script`         | Two paths: (1) bake `scripts/smoke.gsh` into a GSFS disk and `run /smoke.gsh` (host-baked-file path, incl. a piped assert); (2) `selfcheck` — run the shell-embedded extensive suite (`scripts/selfcheck.gsh`) IN MEMORY. Both assert `ran N, failed 0`. The embedded suite isn't a disk file because an on-disk file is one ≤4 KiB IPC message (`MAX_FILE_BYTES`); rodata is not. |
| `osdev mkfs <image>`        | Format a disk image as GSFS0003 (empty) |
| `osdev script-disk <out> <script.gsh>` | Build a flashable GSFS data disk with `<script>` baked in as `/<basename>` — `dd` it to the data drive, boot, `run /<basename>` (the hardware self-check) |
| `osdev validate`            | Validate all contracts against the JSON schema |
| `osdev shell [--smp N]`     | Boot in QEMU with the interactive shell on stdin/stdout (bare-metal build — no probe services; type `help` at `gsh>` prompt; Ctrl-A X to quit) |
| `osdev image`               | Build with `bare-metal` supervisor + create UEFI-bootable `build/os.img` (GPT + ESP + BOOTX64.EFI) |
| `osdev image --mode perf`   | Same image, `perf-only` supervisor (B1–B10 probes) |
| `osdev image --mode perf-brutal` | Same image, `perf-brutal-only` supervisor (BP1–BP10 probes) |
| `osdev image --mode identity` | Same image, `identity-only` supervisor (WatchSerial identity tests) |
| `osdev image --mode fuzz`   | Same image, `fuzz-only` supervisor (§22 F1–F8 + BF1–BF8 self-run over serial; F3/BF3 need test-bad-elf, F4 is host-only) |

## Files

| File             | Responsibility |
|------------------|---------------|
| `src/main.rs`    | CLI parsing (`clap`), dispatch to handlers |
| `src/validator.rs`| Contract validation + all test suite runners (identity, property, fuzz, stress, perf, adversarial, chaos, and their brutal variants) |
| `src/qemu.rs`    | QEMU launch helpers (`spawn_for_test`, `spawn_for_test_custom`) — file-based serial (`-serial file:`) on all platforms |
| `src/disk_image.rs` | UEFI GPT disk image creation: protective MBR, GPT headers (CRC32), EFI System Partition (FAT32), `BOOTX64.EFI`, `limine.conf`, `kernel.elf` |

### GSFS host-side writer (`src/main.rs`)

`format_superblock` writes an empty GSFS0003 (superblock + free bitmap + root dir), and
`gsfs_add_file` bakes a file into it (allocate a contiguous extent, write content, add a root
`file_record`, update the free count) — a host-side mirror of the `fs` write path, kept in lockstep
with the on-disk format documented at the top of `main.rs` and in `docs/persistence.md` §6.4. This
is what lets `osdev script-disk` ship a `.gsh` suite to hardware: bake → `dd` to the data drive →
boot → `run /suite.gsh`. `osdev test script` proves the loop end to end (incl. piped asserts a
script file can carry but on-device typing can't).

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
