# Hardware: Fuzz Tests

Mirrors §22 Fuzz Tests (F1–F8). Crash resistance under adversarial inputs.

**Reference:** `tests/qemu/fuzz/CLAUDE.md` for full spec.

## Hardware applicability

Fuzz tests require rapid iteration (millions of inputs per surface). The flash-boot cycle (~2 min per flash) makes hardware fuzzing impractical as a primary vehicle. Hardware fuzz testing has a different purpose: **confirming that inputs which crashed QEMU also crash (or don't crash) on silicon**, ruling out emulation artifacts.

| Fuzz surface | HW approach | Notes |
|---|---|---|
| F1 - Syscall args (1M iters) | Flash with high-count probe, observe for panic | Long runtime; one flash covers it |
| F2 - Syscall numbers | Same | |
| F3 - ELF binaries | Bake specific bit-flip mutations into image | One mutation per flash |
| F4 - Service contracts | Bake malformed contracts into image at build | Build-time; verifiable at boot |
| F5 - IPC message bodies | Flash with fuzz-IPC probe | |
| F6 - Embedded caps | Same | |
| F7 - Cap generation field | Same | |
| F8 - Memory request values | Same | |

**Primary vehicle:** QEMU (fast iteration). Hardware is a regression-confirmation platform.

**Build mode:** Will need a `fuzz-only` supervisor feature (not yet built).

## Status

All pending. Hardware fuzz runs not yet attempted.

## Pass record

| Date | Surface | Inputs | Panics | Notes |
|------|---------|--------|--------|-------|
| - | - | - | - | No hardware fuzz runs yet |
