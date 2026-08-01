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
- **Networking (USB-Ethernet -> net-stack):** a CDC-ECM USB-net device is driven in-kernel (DWC2 bulk
  frames) and bridged to the **unchanged** userspace `net-stack` via three `NET_DEVICE`-gated syscalls; a
  per-arch `nic-driver` backend speaks the frame IPC. Verified in QEMU (`arm_run.py --usbnet`): DHCP, ARP,
  ICMP, DNS, and the shell `net` + `ping` all work over USB. The Pi 2's onboard **LAN9514 (`smsc95xx`) is
  now driven on real hardware too**: DHCP, ARP and internet ping all work, with RX **interrupt-driven** (a
  bulk-IN is kept armed and its halt-ISR parses each burst and re-arms; the DWC2 core auto-retries NAKs in
  hardware, so an idle device costs no interrupts). The poll it replaced listened only ~3% of each 10 ms
  tick and the device's small RX FIFO dropped the rest - that was ~85% ping loss, now ~4% (ordinary
  internet loss). Link state is read from the PHY (MII BMSR), so an unplugged cable reports as unplugged.
- **Multiple USB devices coexist:** `enumerate_downstream` walks *every* hub port, gives each device a
  distinct address, and configures all of them; the single DWC2 host channel is time-shared by having each
  transfer path re-select its device (`select_device`: address / max-packet / speed). Verified in QEMU with
  a keyboard + usb-net + usb-storage attached together: all three enumerate, networking flows (DHCP/ICMP),
  and the keyboard still types **while** the network is live - the shape the real Pi 2's LAN9514
  (hub + integrated ethernet, plus external keyboard ports) needs.
- **Graceful degradation (loud, not silent):** `xhci`/`ehci` (front/back USB host on x86) are not ported,
  so they fail their spawn **loudly** and the system continues to a usable shell - exactly §9.2/§11.3
  ("continue with the services that started"). Without an attached SD image `block-driver` finds no card
  and `fs` serves storage-unavailable (loud, not a hang); without a `--usbnet` device net-stack degrades.

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
`build/kernel7.img` **and `build/config.txt`** to the SD card's FAT boot partition (a file copy, not a
flash - `docs/multi-arch.md`); the **full procedure** (preparing the boot card *and* the storage USB
stick, with the disk-identification safety and the durability caveat) is **`docs/pi2-deploy.md`**.
Serial console is **115200 8N1** on the PL011. Prereqs: the same Rust nightly + `cargo` as x86, plus
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
- **No RTC on the Pi 2** (and QEMU raspi2b emulates none) - the x86 MC146818 CMOS RTC has no Pi
  equivalent. Both consequences are now **fixed rather than accepted**: `uptime` reads the monotonic
  generic timer (not a wall-clock delta from a frozen stamp), and the wall clock is set from the network
  by **SNTP** - net-stack syncs once after the boot DHCP dance and on demand via `date sync` (the gated
  `SetClock` syscall 50 / `SET_CLOCK` capability, granted by name to net-stack on ARM only; x86's CMOS
  clock remains the authority there and the syscall is refused). With no cable, `date` reads zeros and
  says so rather than inventing a time.
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
- **USB-net -> net-stack bridge -> full networking** - **DONE + QEMU-verified** (2026-07-23). The
  in-kernel USB-net device is bridged to the **unchanged** userspace `net-stack` so the whole networking
  stack runs over USB. Mechanism: three kernel syscalls (`NetFrameTx`/`NetFrameRx`/`NetInfo`, 42-44, gated
  by a `NET_DEVICE` cap) move raw ethernet frames to/from the CDC-ECM device (core-0-guarded, the DWC2's
  core). A per-arch `nic-driver` backend (cfg-split exactly like block-driver's AHCI/SDHCI) bridges those
  syscalls to the frame IPC net-stack speaks - the frame IS the message, `[3]`=MAC/link, `[4]`=RX,
  `[9]`=batch. net-stack, nic-driver co-located on core 0; both added to the ARM build lists + spawned by
  the supervisor. **Verified end to end** with `arm_run.py --usbnet`: net-stack does DHCP (`ip 10.0.2.15`),
  ARP (`gateway 10.0.2.2 at 52:55:0a:00:02:02`), ICMP (`ping ok`), DNS (`10.0.2.3`), and the interactive
  shell `net` + `ping 10.0.2.2` (`Reply from 10.0.2.2: bytes=32 ... TTL=255`) all work over USB. A real ARM
  bug fell out: `now_epoch_monotonic()` was a `0` stub, so `calibrate_tsc_hz` spun ~100M yields and every
  deadline wait never expired, hanging net-stack before its serve loop - now wired to the generic timer
  (`cntpct()/timer_hz()`).
- **LAN9514 (`smsc95xx`) for the real Pi 2** - **written, HW-UNVERIFIED** (2026-07-23). The Pi 2's onboard
  NIC is a **vendor-specific** `smsc95xx` device (class 0xFF, VID 0x0424), *not* CDC-ECM and not
  QEMU-emulated. `configure_smsc95xx` is a clean reimplementation from the working u-boot/Linux `smsc95xx`
  reference (per the driver doctrine): chip config via **vendor control requests** (bRequest 0xA0 write /
  0xA1 read, register offset in wIndex), lite-reset + PHY-reset, MAC from the chip's ADDRH/ADDRL (firmware-
  programmed) with a locally-administered fallback, MDIO PHY auto-negotiation, MAC TX/RX enable. Each TX
  frame is prefixed with the **8-byte TX command word** and each RX frame carries a **4-byte RX status
  word** (`net_frame_tx`/`rx` branch on `NET_KIND`). It slots into `enumerate_downstream` alongside CDC-ECM
  over the same enumeration + `bulk_xfer` + `net_frame_*` bridge, so the whole stack above it (nic-driver,
  net-stack, `net`/`ping`) works unchanged once the device comes up. **Every hardware wait is bounded**, so
  a wrong assumption leaves the NIC unconfigured (net-stack degrades) rather than hanging the boot. QEMU
  never exercises this branch, so it awaits **real-Pi verification** - the MAC-from-VideoCore-mailbox is a
  known refinement for that pass.
- **SDK DMA cache-coherence (SEC-28)** - `sdk/rust/src/dma.rs` assumes x86 coherent DMA; any real ARM
  driver needs cache-maintenance hooks (clean-before-device-read, invalidate-before-CPU-read) first.
- **Watchdog / PM reset (`hardware_reset`) - DONE + QEMU-verified** (2026-07-23). Was a stub that spun, so
  the shell `reboot` (and the Ctrl+Alt+Del chord that routes through it) hung the Pi 2 instead of resetting
  it. Now does the BCM2835 power-management watchdog reset (`arch/arm/mod.rs`): write `PM_WDOG` (peripheral
  base + 0x100024) a short timeout and `PM_RSTC` (base + 0x10001c) a full-reset request, both gated by the
  `0x5A` password; the SoC resets when the watchdog fires. Verified in QEMU (`reboot` re-runs the kernel
  from its boot banner - boot markers go 1 -> 2).
- **Hardware RNG (BCM2835) - DONE + QEMU-verified** (2026-07-23). An entropy source: `hw_random()` reads
  the SoC RNG, exposed ungated as InspectKernel query 19 (entropy confers no authority) with a `random [n]`
  shell command. QEMU's `raspi2b` emulates it (`random 3` -> three distinct u32s). x86 RDRAND is a trivial
  follow-up (stubbed `None` today).
- **GPIO (BCM2835) - DONE + QEMU-verified** (2026-07-23). Drive the SoC pins: a **capability-gated** `Gpio`
  syscall (GPIO carries the UART/SD lines, so only the `shell` holds `GPIO_DEVICE`) with a `gpio
  <input|output|high|low|read> <pin>` command. QEMU verifies the readback (pin high -> reads 1, low -> 0);
  on real hardware it drives actual pins (blink an LED, read a button).
- **Still nice-to-haves (deliberately not built - no consumer yet, §26.2):** USB **mouse** (a console OS
  has no pointer to consume it); **I2C/SPI** (would enable an external RTC module - but NTP over the
  now-working network is the better wall-clock path); and **DMA-accelerated / multi-block SD** (a real
  speed win, but a protocol change to `fs` + the block IPC that risks the working persistence path for
  marginal gain on a shell OS - the block-driver is single-block PIO today, correct just not fast).

## See also

- **`kernel/src/arch/arm/CLAUDE.md`** - the implementer's reference: the ARM syscall ABI (and its one
  wider-than-u32 constraint), the boot flow, the in-kernel-driver rule, and the SMP/DMA hazards.
- **`kernel/src/arch/CLAUDE.md`** - the arch boundary + "Porting a driver: the method" (the doctrine).
- **`docs/multi-arch.md`** - the cross-arch proof and per-arch bring-up notes.
- **Audits of this branch:** `docs/kernel-audit.md` Audit 5 (the arm32 kernel layer) and
  `docs/userspace-audit.md` Audit 4 (the arm SDK ABI).

## OPEN: the ARM quantum stub cannot be fixed naively (2026-07-31)

`arch/arm/mod.rs::tsc_ticks_per_quantum()` returns `0`, and `scheduler::cycles_to_ticks` reads `0` as
"fall back to exactly 1 tick" - so **every `sleep` and `recv_timeout` on this port collapses to one
quantum (~10 ms) regardless of what the caller asked for**. A duration the caller chose, silently
replaced. That is a real defect and it is still open.

**An attempt to fix it (branch `fix/arm-tsc-quantum`, not merged) killed serial input on the Pi**, and
the cause was never identified. Recording it so the next attempt does not repeat the search:

- The attempt returned `timer_hz()/100` (the MEASURED rate, never `CNTFRQ` - which overstates by 19.2x
  here), and added an SDK `sleep_ms(ms)`/`duration_cycles(ms)` so that four x86-calibrated constants
  did not become absurd (shell muted-poll + observe q-poll + `observe` repaint: ~30 ms -> ~60 s;
  `examples/counter`: ~1 s -> ~33 min). **Fixing the stub alone IS a regression** - that part is
  certain and any future attempt needs the same companion change.
- **Symptom:** serial input completely dead on hardware; the USB keyboard unaffected. Bisected across
  three hardware boots: known-good `main` works; the full change fails; and a probe with the kernel
  quantum reverted to `0` but `sleep_ms` + the ms constants retained **also fails** - from a cold boot,
  so it is not state- or core-placement-dependent.
- **What that leaves is not explanatory.** With the stub at `0`, `sleep_ms(30)` computes to `sleep(1)`
  = 1 tick, identical to the `sleep(60_000_000)` it replaced (which also floored to 1 tick). The SDK
  diff is purely additive; `observe`/`counter` are not in the input path. 70 lines that cannot account
  for the symptom.
- **QEMU cannot reproduce it** - driving the shell over serial with `arm_run.py --cmd "help"` works on
  the failing build. Hardware-only.

**Leads for the next attempt**, in the order worth trying:
1. The only new *behaviour* in the shell's hot loop is that `sleep_ms` issues an extra syscall -
   **InspectKernel query 16** - before each sleep. Query 16 is NOT in the ungated set (`docs`/
   `syscall/CLAUDE.md`: ungated are 0,3,9,10,11,12,13), so it is gated on INTROSPECT. Check what that
   syscall does on ARM from the shell's context before assuming it is harmless; caching the value once
   at startup instead of per-sleep would sidestep it entirely and is the cheapest thing to try.
2. The shell is known to run near its 64 KiB user-stack ceiling (`project_shell_stack_pipe`); two extra
   frames in `service_main`'s loop are not obviously safe on this port.
3. Unrelated but found while tracing, and worth fixing on its own: `shell`'s `ESC_WAIT_CYCLES =
   200_000_000` is "~100 ms at ~2 GHz" in **`read_tsc` cycles**, and `read_tsc` on the Pi is the ~1 MHz
   generic timer - so a bare-ESC wait is **~200 SECONDS** here. Same class of bug (an x86-calibrated
   cycle count), independent of the quantum.

## HARDWARE GOTCHA: a GPIO HAT can kill serial INPUT while output still works

**Symptom.** The Pi keeps printing to the serial console perfectly - boot log, service logs, command
output, everything - but typing into it does nothing. The USB keyboard still works, so the shell is
plainly alive and reading input from somewhere.

**Cause.** A HAT sits on the GPIO header, which is where the serial console lives (GPIO14 = TX,
GPIO15 = RX). Output only needs the Pi to drive GPIO14, and nothing contends for that. Input needs the
USB-TTL adapter to drive GPIO15 - and if the HAT is holding that pin, the adapter cannot win. Hence the
asymmetry: **TX fine, RX dead.** (A HAT with an ID EEPROM on pins 27/28 can also make the firmware load
an overlay that reconfigures the UART before our kernel ever runs.)

**How to recognise it, and why it is worth writing down.** On 2026-07-31 this cost most of a day. It
looks *exactly* like a software regression: serial typing worked in the morning, then stopped, and every
kernel built afterwards "broke" it. Three separate commits were blamed and one good change (the
`tsc_ticks_per_quantum` fix) was reverted on that false evidence before the hardware was suspected.

**The tests that settle it, cheapest first:**
1. **Flash a kernel that provably worked earlier** - ideally the exact released binary, checksum-verified.
   If the same bytes now fail, nothing in the tree is responsible. *This is the test to run second, not
   eighth: when every build after a baseline fails, re-test the baseline.*
2. **Try the same adapter and terminal on another machine.** Serial input works there -> the shared kit
   is fine and the fault is Pi-side.
3. **Remove the HAT**, wire the adapter straight to GPIO14/15/GND, boot, type.
4. **Loopback the adapter** (its own TX shorted to its own RX, off the Pi). Characters echo -> the
   adapter and terminal are healthy, so it is the wire or the pin.

**Meanwhile the port is still testable.** Serial OUTPUT keeps working, so logs still reach the host -
drive the shell from the USB keyboard and read results over serial as normal. v0.8.1 was validated
entirely this way (`selfcheck` 349/0, 100 chaos rounds).

### It is not just dead input - it is a FLOOD, and the kernel now says so (2026-08-01)

The account above says input is "dead". That understates it, and the difference matters because the
*flood* is what you actually feel. A held RX line is not silence: the PL011 reports it as a continuous
**break condition**, and every one of those enqueues a byte.

The PL011 returns its error flags **in the data register itself** - framing (bit 8), parity (9), break
(10), overrun (11), in the same read as the data. `pl011_rx_drain` used to do `DR & 0xFF`, masking the
flags off and promoting every noise event to real input. So the console ring filled with spurious `0x00`
forever.

That is invisible at a shell prompt (a null is not printable, so nothing appears) but brutal for a
**full-screen app**, which blocks in `ConsoleRead`, wakes on each null, discards it as unprintable, and
repaints. Measured on the Pi 2 with `edit` open: **966 full-screen repaints, 963 of them byte-identical,
while the document changed twice.** After discarding flagged bytes: **21 repaints, none duplicated.**

Two consequences, both now in the code:

- `pl011_rx_drain` **discards** any byte the UART flagged and clears the sticky error. Free on a healthy
  line.
- It **reports** the condition once, with the discarded-byte count, naming the likely cause. Silently
  dropping noise would leave "serial input does not work" looking identical to "serial input is being
  flooded and thrown away", and only one of those tells you to check the wiring (invariant 12). If you
  see `pl011: RX line errors - discarded N bytes ...` in the boot log, that is this, and step 3 above is
  your fix.

A note on the threshold: it was first set at 2,000 discarded bytes and **never fired**, because the
session that motivated it discarded only ~945. A check that cannot trip in its own worked example is
worse than no check - it reads as "the line is fine". It is now 128, which clears connect-time glitches
and trips within seconds of a held line.
