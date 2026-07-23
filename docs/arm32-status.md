<!-- SPDX-License-Identifier: GPL-2.0-only -->
# ARM32 (Raspberry Pi 2) port - status

Branch `feat/pi2-arm32`. This is the living status of the 32-bit ARM (ARMv7-A, BCM2836) port. It
records what runs, how to build/run it, and what remains. It trails the spec (`CLAUDE.md` wins on any
conflict) and complements `docs/multi-arch.md` (the cross-arch proof).

## What runs today (QEMU `raspi2b` + real Pi 2 hardware)

The **arch-neutral half of GodspeedOS runs on ARM32** - the OS above the hardware drivers:

- **Boot + machine layer:** HYP->SVC drop, MMU (short descriptors, 1 MB sections), exception vectors,
  generic timer, PL011 console (in *and* out), 4-core SMP. All boot selftests PASS (context, preempt,
  neutral-scheduler, frame-alloc, SVC, **usermode**, loader, MMU, timer, tick).
- **The real OS bootstrap:** the kernel makes its one direct spawn (the **supervisor**), which spawns
  services from its manifest through the neutral spawn path (per-task address spaces, PL0 user mode,
  banked-register trap frames, fault-survival: a PL0 fault kills just that task and the kernel continues).
- **Services:** `supervisor`, `logger`, `shell`, `ping`, `pong`, and the example services
  (`observe`, `chaos`, `mem-pressure`, `counter`, `greet`, `upper`, `roster`, `reply-server`, `asker`,
  `resource-server`, `holder`) - all cross-compiled to `armv7a-none-eabi` and embedded.
- **Cross-core IPC:** `ping` (core 0) -> `pong` (core 1) capability IPC runs under preemptive scheduling.
- **Interactive shell:** a supervisor-spawned `gsh>` prompt over serial. Verified utilities in QEMU:
  `help`, `version` (`GodspeedOS 0.7.0`), `cores` (`4`), `mem`, `status`, `caps`, `roster`, pipes
  (`status | count` -> `3`), and graceful degradation (`ls` -> `ls: storage unavailable`).
- **Graceful degradation (loud, not silent):** the Pi 2 *has* eMMC and USB, but their **drivers are not
  ported to ARM yet**, so the hardware services (`block-driver`, `fs`, `xhci`, `ehci`, `nic-driver`,
  `net-stack`) are placeholders that fail their spawn **loudly** ("kernel will name-wire it" / "returned
  no endpoint cap") and the system continues to a usable shell - exactly §9.2/§11.3 ("continue with the
  services that started").

## Build + run

```
python scripts/arm_build.py                       # full stack, debug -> build/kernel7.img
python scripts/arm_build.py --release              # optimized (773 KiB; USE THIS for a usable shell)
python scripts/arm_build.py --feature arm-shell    # logger+shell only (kernel-spawned, no supervisor)
python scripts/arm_run.py --release --secs 15 --cmd "status | count"   # boot in QEMU + drive the shell
```

`arm_build.py` cross-compiles the SDK + every arm-ported service to `armv7a-none-eabi`, builds the
kernel (which embeds them via `kernel/build.rs`'s `arm_built` allowlist), and objcopies to a flat
`build/kernel7.img`. The supervisor is built with its `bare-metal` feature (the "usable OS, quiet gsh>"
set: logger + shell, no harness probes; `ping`/`pong` spawnable on demand). Deploy to a Pi by copying
`build/kernel7.img` to the SD card's FAT32 partition (a file copy, not a flash - `docs/multi-arch.md`);
serial console is **115200 8N1** on the PL011. Prereqs: the same Rust nightly + `cargo` as x86, plus
Python 3 and `qemu-system-arm`. `osdev` itself is still x86-only; these scripts are the ARM equivalent of
`osdev build`/`run` until ARM becomes a first-class `osdev` target.

### Running a new service on the Pi 2

An arch-neutral service (SDK + syscalls only, no x86 hardware probe) runs on ARM unchanged. To get it
into the ARM image you add it to **two** allowlists that must stay in sync:

1. Write the service as usual (`GETTING_STARTED.md`; the `service_main(ctx)` + contract pattern is
   arch-neutral).
2. Add its crate/binary name to **`arm_built`** in `kernel/build.rs` (so the kernel embeds its real ARM
   ELF instead of the empty placeholder).
3. Add the same name to **`ARM_SERVICES`** in `scripts/arm_build.py` (so the build cross-compiles it to
   `armv7a-none-eabi` before the kernel embeds it). The two lists are deliberately identical; keep them
   so.
4. Rebuild: `python scripts/arm_build.py --release`. If the supervisor should *spawn* it at boot, that is
   a supervisor-manifest change (same as x86), not an ARM-specific step.

A **hardware** driver is different - see `kernel/src/arch/arm/CLAUDE.md` (the ARM syscall ABI, the
in-kernel-driver rule, DMA cache coherence) and `kernel/src/arch/CLAUDE.md` ("Porting a driver: the
method").

## Known issues / gotchas

- **Debug pipes overflow the user stack; use `--release`.** A debug (unoptimized) shell pipe frame
  (e.g. `status | count`'s record builder) is ~600 KiB, exceeding the 256 KiB user stack, so it faults
  the shell (which recovers via supervisor restart). Release frames fit and pipes run cleanly. The
  release image is also 27x smaller. Build the usable OS in release.
- **No RTC on the Pi 2** (and QEMU raspi2b emulates none), so `date`/`uptime` read zeros. Not a bug -
  the x86 MC146818 CMOS RTC has no Pi equivalent; a real clock needs NTP or an I2C RTC module.
- **The `usermode` selftest** used VAs in the framebuffer region; it now maps at `0x5000_0000` (above
  every identity-mapped region) so it PASSes under QEMU and HW alike.

## Remaining work (hardware drivers - the "grok Linux, reimplement as a service" doctrine)

These are genuine new driver development, not recompilation. Each reads a working reference (u-boot /
Linux / bare-metal) for the register sequence and reimplements it as a capability service the
GodspeedOS way.

- **USB keyboard (DWC2)** - *in progress* (kernel-side, `arch/arm/dwc2.rs`). Control transfers via PIO;
  the current blocker's fix (halt-all-channels at init, `FSLSPClkSel` for the HS PHY) is being
  hardware-verified. See git log + memory. QEMU's DWC2 cannot complete transfers, so this needs the Pi.
- **SD/EMMC block driver -> `fs`** - the Pi 2 has an Arasan SDHCI controller (QEMU emulates it), so this
  is QEMU-developable. `fs` is arch-neutral and embed-ready; it just needs a working block-driver
  backend (the current one is AHCI/PCI, x86-only). Unblocks persistence + the file utilities.
- **LAN9514 USB-Ethernet -> `net-stack`** - far-future; the Pi 2 NIC is behind the USB hub.
- **SDK DMA cache-coherence (SEC-28)** - `sdk/rust/src/dma.rs` assumes x86 coherent DMA; any real ARM
  driver needs cache-maintenance hooks (clean-before-device-read, invalidate-before-CPU-read) first.

## See also

- **`kernel/src/arch/arm/CLAUDE.md`** - the implementer's reference: the ARM syscall ABI (and its one
  wider-than-u32 constraint), the boot flow, the in-kernel-driver rule, and the SMP/DMA hazards.
- **`kernel/src/arch/CLAUDE.md`** - the arch boundary + "Porting a driver: the method" (the doctrine).
- **`docs/multi-arch.md`** - the cross-arch proof and per-arch bring-up notes.
- **Audits of this branch:** `docs/kernel-audit.md` Audit 5 (the arm32 kernel layer) and
  `docs/userspace-audit.md` Audit 4 (the arm SDK ABI).
