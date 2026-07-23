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
- **Persistence (SD/EMMC -> fs):** `block-driver` drives the Pi 2's BCM2835 EMMC (Arasan SDHCI) from
  **userspace** (PIO; the kernel grants it the EMMC MMIO window at spawn, `arch::arm::map_fixed_driver_mmio`),
  and `fs` mounts on top. Verified in QEMU (`--sd` an image): `drives flash` formats GSFS, files write +
  read, and **survive a reboot** (re-mount + read-back). This unblocks the file utilities (`ls`, `read`,
  `write`, `edit`, `drives`, ...). Needs `--release` (see below).
- **Graceful degradation (loud, not silent):** the USB/NIC drivers are not ported yet, so `xhci`, `ehci`,
  `nic-driver`, `net-stack` are placeholders that fail their spawn **loudly** ("kernel will name-wire it")
  and the system continues to a usable shell - exactly §9.2/§11.3 ("continue with the services that
  started"). Without an attached SD image `block-driver` finds no card and `fs` serves storage-unavailable
  (loud, not a hang).

## Build + run

```
python scripts/arm_build.py                       # full stack, debug -> build/kernel7.img
python scripts/arm_build.py --release              # optimized; USE THIS for a usable shell (and the Pi)
python scripts/arm_build.py --release --qemu       # QEMU-targeted: identity DWC2 DMA (for USB testing)
python scripts/arm_build.py --feature arm-shell    # logger+shell only (kernel-spawned, no supervisor)
python scripts/arm_run.py --release --secs 15 --cmd "status | count"   # boot in QEMU + drive the shell
python scripts/arm_run.py --release --usb          # boot in QEMU with an emulated usb-kbd (DWC2 path)
```

> **`--qemu` vs the default.** The only current difference is the DWC2 USB DMA bus-address translation
> (`arch/arm/dwc2.rs`): QEMU addresses ARM RAM directly (identity), real BCM2836 silicon sees RAM through
> the VideoCore alias `0xC000_0000`. The default build is **hardware-correct**; pass `--qemu` only to test
> USB under emulation. Everything else (shell, SD/fs, ping/pong) is identical between the two.

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

- **Debug frames overflow the 256 KiB user stack; use `--release`.** A debug (unoptimized) shell pipe
  frame (`status | count`'s record builder, ~600 KiB) or `fs`'s mount/journal frame exceeds the 256 KiB
  user stack and faults the task (it recovers via supervisor restart). Release frames fit; the release
  image is also 27x smaller. Build the usable OS in release.
- **No RTC on the Pi 2** (and QEMU raspi2b emulates none), so `date`/`uptime` read zeros. Not a bug -
  the x86 MC146818 CMOS RTC has no Pi equivalent; a real clock needs NTP or an I2C RTC module.
- **The `usermode` selftest** used VAs in the framebuffer region; it now maps at `0x5000_0000` (above
  every identity-mapped region) so it PASSes under QEMU and HW alike.

## Remaining work (hardware drivers - the "grok Linux, reimplement as a service" doctrine)

These are genuine new driver development, not recompilation. Each reads a working reference (u-boot /
Linux / bare-metal) for the register sequence and reimplements it as a capability service the
GodspeedOS way.

- **USB keyboard (DWC2)** - **working in QEMU** (kernel-side, `arch/arm/dwc2.rs`); real-Pi verification
  pending. The full path runs end to end under `qemu-system-arm -M raspi2b,usb=on -device usb-kbd`: DMA
  control transfers, enumerate the **hub** the keyboard sits behind (the Pi 2's LAN9514 topology, and
  QEMU's NEC-hub model), select HID **boot protocol**, and poll the interrupt IN endpoint from the timer
  tick -> `decode_report` -> `console_push_byte`. Keys typed on the emulated keyboard reach the `gsh>`
  prompt (verified: injecting `hello` via the QEMU monitor `sendkey` echoes to the shell). Two lessons:
  (1) QEMU's DWC2 model emulates **only the DMA engine**, not slave/PIO - so the driver uses internal DMA
  (also how u-boot/Linux drive it), bracketed with cache maintenance for the A7's non-coherent DMA;
  (2) the HCDMA buffer address is the VideoCore bus alias `0xC000_0000 | phys` on **real hardware** but
  identity (`0`) under **QEMU**, selected by the `qemu` cargo feature (`scripts/arm_build.py --qemu`) so
  the shipped image stays hardware-correct. **Build for QEMU test:** `arm_build.py --release --qemu`;
  **build for the Pi:** `arm_build.py --release` (default = hardware alias). Real-Pi bring-up may still
  need the hard-won register quirks (halt-all-channels at init, `FSLSPClkSel=0` for the HS PHY) that QEMU
  does not exercise - see the `dwc2.rs` comments + git log.
- **SD/EMMC block driver -> `fs`** - **DONE** (2026-07-23): userspace `block-driver` SDHCI/PIO backend +
  the kernel's fixed-peripheral MMIO grant; `fs` mounts + persists in QEMU. Remaining: real-hardware
  verification on a Pi, and multi-block/faster transfers (PIO single-block today).
- **USB bulk transfers (DWC2)** - **DONE + QEMU-verified** (2026-07-23). `bulk_xfer` (the third transfer
  type after control + interrupt) is the shared foundation for USB mass storage and USB-Ethernet. Proven
  end to end against QEMU's `usb-storage`: a Bulk-Only Transport + minimal SCSI layer (`bot_command`,
  TEST UNIT READY / REQUEST SENSE to clear the power-on UNIT ATTENTION, READ CAPACITY(10), READ(10)) reads
  a planted block-0 signature back correctly through a multi-packet bulk IN. Test:
  `qemu-system-arm -M raspi2b,usb=on -device usb-storage,drive=ud -drive if=none,id=ud,format=raw,file=<img>`
  -> serial shows `msc capacity ...` + `BULK TRANSFER VERIFIED`. A real USB flash drive is thus already
  detected + read on the Pi 2; promoting it to a `block-driver` backend (alongside SD/EMMC) is a small
  further step.
- **USB-Ethernet frame path (CDC-ECM)** - **DONE + QEMU-verified** (2026-07-23). A CDC-ECM driver
  (`configure_cdc_ecm`) brings up QEMU's `usb-net` gadget: it finds the ECM config (control class
  0x02/subclass 0x06 + a data interface with bulk endpoints), selects it, reads the station MAC from the
  ECM functional descriptor's string, activates the data interface's bulk endpoints (SET_INTERFACE), and
  enables the packet filter. CDC-ECM carries **raw ethernet frames over bulk, no per-packet header**, so
  the frame path is exactly `bulk_xfer`. Proven end to end by an **ARP round-trip through QEMU's user-net**:
  `net_verify_arp` broadcasts an ARP request for the gateway (10.0.2.2) and receives the reply over bulk IN
  (gateway MAC 52:55:0a:00:02:02) -> `USB-ETHERNET FRAME TX/RX VERIFIED`. Test:
  `qemu-system-arm -M raspi2b,usb=on -netdev user,id=n0 -device usb-net,netdev=n0`. This is a real driver
  for CDC-ECM USB dongles, and it validates the whole in-kernel USB frame path.
- **LAN9514 (`smsc95xx`) + net-stack bridge -> full Pi 2 networking** - the remaining work, in two parts.
  (1) The real Pi 2 onboard NIC is a **vendor-specific** `smsc95xx` device (class 0xFF, VID 0x0424), *not*
  CDC-ECM - it needs its own device-setup + framing layer (register reads/writes via vendor control
  requests, a TX command word + RX status word around each frame). QEMU does not emulate it, so this layer
  is HW-verified later; but it plugs into the same enumeration + `bulk_xfer` that CDC-ECM already proved.
  (2) **Bridge** the in-kernel USB-net driver to the userspace `net-stack` over frame IPC (the USB analog
  of how the in-kernel DWC2 keyboard feeds `console_push_byte`) - the genuinely new plumbing, since on x86
  the NIC is a userspace PCIe driver. Once bridged, the existing `net-stack` (ARP/IPv4/ICMP/UDP/DHCP) and
  the `net`/`ping` utilities work over USB. The CDC-ECM path can drive that bridge in QEMU for verification.
- **SDK DMA cache-coherence (SEC-28)** - `sdk/rust/src/dma.rs` assumes x86 coherent DMA; any real ARM
  driver needs cache-maintenance hooks (clean-before-device-read, invalidate-before-CPU-read) first.

## See also

- **`kernel/src/arch/arm/CLAUDE.md`** - the implementer's reference: the ARM syscall ABI (and its one
  wider-than-u32 constraint), the boot flow, the in-kernel-driver rule, and the SMP/DMA hazards.
- **`kernel/src/arch/CLAUDE.md`** - the arch boundary + "Porting a driver: the method" (the doctrine).
- **`docs/multi-arch.md`** - the cross-arch proof and per-arch bring-up notes.
- **Audits of this branch:** `docs/kernel-audit.md` Audit 5 (the arm32 kernel layer) and
  `docs/userspace-audit.md` Audit 4 (the arm SDK ABI).
