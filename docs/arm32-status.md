<!-- SPDX-License-Identifier: GPL-2.0-only -->
# ARM32 (Raspberry Pi 2) port - status

Branch `feat/pi2-arm32-hardening`. This is the living status of the 32-bit ARM (ARMv7-A, BCM2836) port. It
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
- **Services:** `supervisor`, `logger`, `console`, `shell`, `ping`, `pong`, the driver services
  (`dwc2`, `block-driver` + `fs`, `nic-driver` + `net-stack`, `time`, `control`) and the example services
  (`observe`, `chaos`, `mem-pressure`, `counter`, `greet`, `upper`, `roster`, `reply-server`, `asker`,
  `resource-server`, `holder`) - all cross-compiled to `armv7a-none-eabi` and embedded. The embedded set
  is `arm_built` in `kernel/build.rs`, which is the single source of truth (see "Running a new service").
- **Cross-core IPC:** `ping` (core 0) -> `pong` (core 1) capability IPC runs under preemptive scheduling.
- **Display (HDMI/TV):** the **`console` service** owns the framebuffer and renders the terminal - the
  ANSI/CSI parser, the UTF-8 box glyphs, the shadow grid, the cursor and scrolling. The kernel is granted
  nothing beyond a minimal boot/panic blit (`kernel/src/bootcon`: plain ASCII, escapes discarded, no grid,
  no cursor; it clears at the bottom instead of scrolling), which exists because **a panic halts every
  core and so cannot ask a service to report it** (CLAUDE.md §11.4 amendment).

  Ownership is a state, not a convention: the kernel stops drawing the moment it GRANTS the framebuffer
  at spawn, and takes it back if `console` dies or on a panic. The mapping is Normal **non-cacheable** on
  both sides (`mmu::section_fb`), because a service cannot do cache maintenance and ARM leaves mismatched
  attributes for one physical page UNPREDICTABLE. Hardware-verified 2026-08-17: chaos 50 rounds with 0
  kernel panics and 0 liveness wedges, `selfcheck` 350/0, 0 console writes lost. Full account:
  `docs/console-service.md` §9.
- **Interactive shell:** a supervisor-spawned `gsh>` prompt over serial. Verified utilities in QEMU:
  `help`, `version` (`GodspeedOS 0.10.0`), `cores` (`4`), `mem`, `status`, `caps`, `roster`, pipes
  (`status | count` -> `3`), and graceful degradation (`ls` -> `ls: storage unavailable`).
- **Persistence (USB stick -> fs):** `block-driver` reaches a **USB mass-storage stick** through the
  `dwc2` SERVICE over the block IPC protocol, and `fs` mounts on top. `drives flash` formats GSFS, files
  write + read and **survive a reboot**, which unblocks the file utilities (`ls`, `read`, `write`, `edit`,
  `drives`, ...). Needs `--release` (see below).

  > **The SD/EMMC card is the boot medium and is NEVER written.** An earlier version of this document
  > described an Arasan SDHCI backend as the working storage path. It is **withdrawn**: `sdhci.rs` is kept
  > for reference but is deliberately not compiled in (`services/block-driver/src/main.rs`), because on a
  > single-slot Pi the EMMC *is* the card the board boots from, and GSFS's superblock at LBA 0 lands on the
  > partition table. **It corrupted two boot cards to RAW.** There is no safe way to use a single-slot Pi's
  > boot card as storage, so this is a hazard rather than a fallback.
- **Networking (USB-Ethernet -> net-stack):** a USB-net device is driven by the userspace **`dwc2`
  service** (DWC2 bulk frames) and reached by `nic-driver` over IPC (opcodes `0x10+` on `dwc2`'s endpoint -
  the same endpoint that serves the block protocol), which bridges to the **unchanged** `net-stack`. The
  `NET_DEVICE` syscalls (42-44) that used to front an in-kernel USB-net device are **stubbed inert** on
  this arch (`arch/arm/mod.rs`), because no frame passes through the kernel any more. Verified in QEMU (`arm_run.py --usbnet`): DHCP, ARP,
  ICMP, DNS, and the shell `net` + `ping` all work over USB. The Pi 2's onboard **LAN9514 (`smsc95xx`) is
  now driven on real hardware too**: DHCP, ARP and internet ping all work, with RX **interrupt-driven** (a
  bulk-IN is kept armed and its halt-ISR parses each burst and re-arms; the DWC2 core auto-retries NAKs in
  hardware, so an idle device costs no interrupts). The poll it replaced listened only ~3% of each 10 ms
  tick and the device's small RX FIFO dropped the rest - that was ~85% ping loss, now ~4% (ordinary
  internet loss). Link state is read from the PHY (MII BMSR), so an unplugged cable reports as unplugged.
- **Multiple USB devices coexist:** `enumerate_downstream` walks *every* hub port, gives each device a
  distinct address, and configures all of them; the single DWC2 host channel is time-shared by having each
  transfer carry a `Target` (address / max-packet / speed). Hot-plug in both directions is watched through
  the hub's status-change endpoint. Verified in QEMU with
  a keyboard + usb-net + usb-storage attached together: all three enumerate, networking flows (DHCP/ICMP),
  and the keyboard still types **while** the network is live - the shape the real Pi 2's LAN9514
  (hub + integrated ethernet, plus external keyboard ports) needs.
- **Graceful degradation (loud, not silent):** `xhci`/`ehci` (the x86 USB hosts) are not spawned on ARM at
  all - the supervisor `cfg`-excludes them, because this board's host controller is `dwc2`. Without a USB
  storage stick `block-driver` reports no disk and `fs` serves storage-unavailable (loud, not a hang);
  without a `--usbnet` device net-stack degrades - exactly §9.2/§11.3 ("continue with the services that
  started").

## Build + run

```
python scripts/arm_build.py                       # full stack, debug -> build/kernel7.img
python scripts/arm_build.py --release              # optimized; USE THIS for a usable shell (and the Pi)
python scripts/arm_build.py --release --qemu       # QEMU-targeted: identity DWC2 DMA (for USB testing)
python scripts/arm_run.py --release --secs 15 --cmd "status | count"   # boot in QEMU + drive the shell
python scripts/arm_run.py --release --usb          # boot in QEMU with an emulated usb-kbd (DWC2 path)
```

> **`--qemu` vs the default.** The only difference is the DWC2 USB DMA bus-address translation
> (`DMA_BUS_ALIAS`, `services/dwc2/src/regs.rs`): QEMU addresses ARM RAM directly (identity), real BCM2836
> silicon sees RAM through the VideoCore alias `0xC000_0000`. The default build is **hardware-correct**;
> pass `--qemu` only to test USB under emulation. Everything else (shell, storage, ping/pong) is identical.
>
> **This flag was a silent no-op until 2026-08-17, and the fix is worth knowing about.** The `qemu`
> feature had moved to the `dwc2` SERVICE when the USB stack left the kernel, but `arm_build.py` kept
> passing it to the KERNEL, where the feature still existed and nothing read it. So the build reported
> success, produced a hardware-alias binary, and USB under emulation STALLed in the DATA stage. The flag
> now goes to `dwc2` and the dead kernel feature is deleted, so there is exactly one place it can mean
> something. Verified by disassembling both builds: the hardware one contains
> `orr r2, r2, #0xC0000000` and the `--qemu` one does not.

`arm_build.py` cross-compiles the SDK + every arm-ported service to `armv7a-none-eabi`, builds the
kernel (which embeds them via `kernel/build.rs`'s `arm_built` allowlist), and objcopies to a flat
`build/kernel7.img`. The supervisor is built with its `bare-metal` feature (the "usable OS, quiet gsh>"
set: logger, `console` (the terminal), the driver services - `dwc2`, `block-driver` + `fs`,
`nic-driver` + `net-stack`, `time` - and the shell; no harness probes, `ping`/`pong` spawnable on demand). Deploy to a Pi by copying
`build/kernel7.img` **and `build/config.txt`** to the SD card's FAT boot partition (a file copy, not a
flash - `docs/multi-arch.md`); the **full procedure** (preparing the boot card *and* the storage USB
stick, with the disk-identification safety and the durability caveat) is **`docs/pi2-deploy.md`**.
Serial console is **115200 8N1** on the PL011. Prereqs: the same Rust nightly + `cargo` as x86, plus
Python 3 and `qemu-system-arm`. `osdev` itself is still x86-only; these scripts are the ARM equivalent of
`osdev build`/`run` until ARM becomes a first-class `osdev` target.

### Running a new service on the Pi 2

An arch-neutral service (SDK + syscalls only, no x86 hardware probe) runs on ARM unchanged. To get it
into the ARM image there is **one** list to edit:

1. Write the service as usual (`GETTING_STARTED.md`; the `service_main(ctx)` + contract pattern is
   arch-neutral).
2. Add its crate/binary name to **`arm_built`** in `kernel/build.rs`. That is the single source of truth:
   `scripts/arm_build.py` DERIVES its cross-compile list from it (`_arm_services()`), so there is no second
   list to keep in sync.

   > This used to be two allowlists "deliberately identical; keep them so", and they drifted exactly as
   > you would expect: the `console` service was added to one and not the other, so it shipped as an empty
   > placeholder. Every log line reported success and the display just quietly stayed on the kernel's boot
   > floor. The second list is deleted rather than checked.
4. Rebuild: `python scripts/arm_build.py --release`. If the supervisor should *spawn* it at boot, that is
   a supervisor-manifest change (same as x86), not an ARM-specific step.

A **hardware** driver is different - see `kernel/src/arch/arm/CLAUDE.md` (the ARM syscall ABI, the
userspace-driver rule, DMA cache coherence) and `kernel/src/arch/CLAUDE.md` ("Porting a driver: the
method").

## Known issues / gotchas

- **Debug frames overflow the 256 KiB user stack; use `--release`.** A debug (unoptimized) shell pipe
  frame (`status | count`'s record builder, ~600 KiB) or `fs`'s mount/journal frame exceeds the 256 KiB
  user stack and faults the task (it recovers via supervisor restart). Release frames fit; the release
  image is also 27x smaller. Build the usable OS in release.
- **No RTC on the Pi 2** (and QEMU raspi2b emulates none) - the x86 MC146818 CMOS RTC has no Pi
  equivalent. Both consequences are now **fixed rather than accepted**: `uptime` reads the monotonic
  generic timer (not a wall-clock delta from a frozen stamp), and the wall clock is set from the network
  by **SNTP** - net-stack fetches it and hands it to the **`time` SERVICE** over IPC (`OP_SET`), which
  owns plausibility, provenance and the floor and can refuse it. (This used to be a gated `SetClock`
  syscall; that syscall and `kernel/src/clock.rs`/`wallclock.rs` are deleted - the wall clock is not a
  kernel responsibility.) With no cable, `date` reads zeros and
  says so rather than inventing a time.
- **The `usermode` selftest** used VAs in the framebuffer region; it now maps at `0x5000_0000` (above
  every identity-mapped region) so it PASSes under QEMU and HW alike.

## Remaining work (hardware drivers - the "grok Linux, reimplement as a service" doctrine)

These are genuine new driver development, not recompilation. Each reads a working reference (u-boot /
Linux / bare-metal) for the register sequence and reimplements it as a capability service the
GodspeedOS way.

- **USB keyboard (DWC2)** - **DONE + HW-verified.** Driven by the userspace `services/dwc2`
  (`hid.rs` bind/poll, `hub.rs::enumerate_downstream`, HID boot protocol, keystrokes to the shell via
  `CONSOLE_PUSH`), interrupt-driven off `USB_VECTOR` - NOT polled from the timer tick. Clean under
  sustained use on hardware (2026-08-17). The two lessons below are kept because they still bite:
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
- **SD/EMMC block driver -> `fs`** - **WITHDRAWN, not done.** `sdhci.rs` is kept for reference but is
  NOT compiled in: on a single-slot Pi the EMMC *is* the boot card, and pointing GSFS at it (superblock
  at LBA 0, over the partition table) **corrupted two cards to RAW**. Storage on ARM is the USB stick
  through `dwc2`. The original entry read DONE with 'remaining: multi-block transfers' - work on a code
  path that is unreachable and must stay so. Historical detail follows:
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
- **LAN9514 (`smsc95xx`) for the real Pi 2** - **DONE + HW-verified.** Lives in
  `services/dwc2/src/net.rs` (`smsc_bring_up`, `link_up`, `link_reconfigure`). DHCP, ARP and internet
  ping all work on real hardware at ~4% loss, with RX interrupt-driven. This entry said
  'HW-UNVERIFIED' while the same document's 'What runs today' already reported it working - the
  contradiction is the tell that a status file was updated in one place only. Original notes:
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
- **SDK DMA cache-coherence (SEC-28) - ANSWERED, by mapping rather than by hooks.** The other option
  `dma.rs` itself named was taken: `DMA_ARENA_UNCACHED = true` on ARM, so the kernel maps a service's DMA
  arena non-cacheable at spawn and `sdk/rust/src/dma.rs` needs no cache maintenance. `services/dwc2`
  DMAs through it today. (`kernel/src/arch/arm/CLAUDE.md`'s SEC-28 bullet still says the opposite and
  needs the same correction.)
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
  now-working network is the better wall-clock path); and **faster multi-block transfers on the USB storage
  path** (a protocol change to `fs` + the block IPC that risks the working persistence path for marginal
  gain on a shell OS). The SD acceleration once listed here is moot - there is no SD storage path (see
  "Persistence" above).

## See also

- **`kernel/src/arch/arm/CLAUDE.md`** - the implementer's reference: the ARM syscall ABI (and its one
  wider-than-u32 constraint), the boot flow, the userspace-driver rule, and the SMP/DMA hazards.
- **`kernel/src/arch/CLAUDE.md`** - the arch boundary + "Porting a driver: the method" (the doctrine).
- **`docs/multi-arch.md`** - the cross-arch proof and per-arch bring-up notes.
- **Audits of this branch:** `docs/kernel-audit.md` Audit 5 (the arm32 kernel layer) and
  `docs/userspace-audit.md` Audit 4 (the arm SDK ABI).

## CLOSED: the ARM quantum stub, and why it could not be fixed alone (2026-07-31; fixed since)

> **FIXED.** `arch/arm/mod.rs::tsc_ticks_per_quantum()` now returns `timer_hz()/100` - the MEASURED
> timer rate, never `CNTFRQ`, which overstates by 19.2x on this board - and the SDK companions
> (`sleep_ms` / `duration_cycles`) landed with it. The account below is kept because its LESSON is
> permanent and was expensive: **a cycle count is not a portable duration**, and fixing the stub without
> the companion change is a regression, not a partial fix.
>
> Two of the three "leads for the next attempt" recorded below have also since become false on their
> own - query 16 is ungated now (and query 9 is deleted entirely), and the shell's
> `ESC_WAIT_CYCLES = 200_000_000` no longer exists (it is `ESC_WAIT_QUANTA = 10`, a monotonic-tick
> loop). The serial-input failure it blames is most likely the serial FLOOD documented further down
> this file, which was diagnosed later.

**What the defect was.** `tsc_ticks_per_quantum()` returned `0`, and `scheduler::cycles_to_ticks` reads
`0` as "fall back to exactly 1 tick" - so **every `sleep` and `recv_timeout` on this port collapsed to
one quantum (~10 ms) regardless of what the caller asked for**. A duration the caller chose, silently
replaced.

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

## CLOSED: the keyboard cannot be made interrupt-driven on this silicon (2026-08-14)

**Asked:** program the DWC2's periodic scheduler so the controller runs the keyboard's interrupt
endpoint itself and raises an IRQ on completion, instead of software sequencing it.

**Answer: the hardware cannot.** Measured on the Pi 2, not inferred:

```
dwc2-svc: GHWCFG2=0x228ddd50 GHWCFG4=0x1ff00020 - arch 2 (internal DMA), descriptor DMA NOT supported
```

`GHWCFG4` bit 30 is clear. Descriptor (scatter/gather) DMA is the only mode in which this core walks a
per-microframe schedule on its own; without it, software must sequence every split transaction. (QEMU
reports `GHWCFG4=0x00000000`, which is an unimplemented register rather than an answer - it cannot
settle this question, and the probe exists because the register was never read at all.)

**Why `HCCHAR.ODDFRM` is not a way around it.** The core will defer a periodic transfer to a frame of
the right parity, but ODDFRM selects odd/even FRAME only. The schedule needs MICROFRAME precision -
start-split at `(current+1)&7` skipping microframe 6, complete-split at +2, retrying NYET in the
following microframes - and 125 us resolution is below anything the controller will time for us. So
`wait_uframe` is not sloppiness; it is supplying timing the hardware has no mechanism to supply.

**What it costs, measured:** `kbd 2063ms` against `sleep 42862ms` = **4.6% of one core**, about 1.1% of
the machine, for a working keyboard. The outer loop is already interrupt-driven (`recv_timeout` on the
endpoint, woken by the kernel's IRQ delivery, `irq_unmask` for the level-triggered line); this cost is
entirely inside the split sequencing.

**The precedent agrees.** The Raspberry Pi's own `dwc_otg` driver solves this with an FIQ state machine
(`dwc_otg.fiq_fsm_enable`) - which is what you build when the core cannot schedule splits itself.

**The remaining options, and why the recommendation is to leave it:**

| Option | Verdict |
|---|---|
| Descriptor DMA | Impossible - the bit is clear on this silicon |
| FIQ/IRQ state machine in the kernel (the Pi's answer) | Works, and puts USB split scheduling back in RING 0 - re-adding a TCB member and undoing slice 5 for 4.6% of one core |
| Block on the channel IRQ per split step | The complete-split window is ~125 us; an IPC wake cannot hit it, and a missed window loses the keystroke (the TT discards after the frame). Trades CPU for a worse keyboard |
| Longer `bInterval` | Halves the cost, doubles keystroke latency. Available if the CPU ever matters more than the feel |

Recorded rather than closed-by-fixing (§26.3): the constraint is the hardware's, and the cheapest
correct answer is to keep paying 4.6% of one core.

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

### It is not just dead input - it is a FLOOD that STARVES the keyboard (2026-08-02)

The account above says input is "dead", and blames the HAT. Both are now known to be wrong, and the
correction is more useful than the original finding.

**What is actually happening.** A faulty RX line does not deliver silence. Measured on a Pi 2 with a
USB-TTL adapter attached: **~148 spurious bytes per second**, almost all `0xFF`, **with valid framing and
no error flags**. That last part is why every obvious filter misses them - a line held low gives break or
framing errors, a line held high gives nothing at all, but a line that is floating or weakly driven gives
a perfectly well-formed `0xFF`, because each glitch reads as a start bit and the line is high for the rest
of the frame. Nothing above the pin can tell it from a real byte.

**The damage is starvation, not junk.** The PL011 and the USB keyboard share ONE 256-byte input ring. At
148 bytes/s in and ~7/s out, the ring is permanently full - and `console_push_byte` silently dropped when
full, so **every keystroke from the USB keyboard was discarded for want of space**. That is what made a
full-screen app unquittable: `edit` repainted on phantom bytes, Ctrl+Q never got in, and the power switch
was the only way out. One faulty input source starving the working one is the real bug, and it is ours,
not the hardware's.

**It was probably NOT the HAT.** The decisive test was accidental: *unplugging the serial cable made
everything work*. If the HAT were driving GPIO15 that would change nothing, since the HAT is fitted either
way. So the noise arrives through the cable - suspect the RX jumper (adapter TX to pin 10), the ground
connection, or an adapter that tri-states TX rather than driving it.

This invalidates the "it is the pi hat" conclusion above. That was reached by eliminating variables (old
binaries failed; x86 worked with the same adapter), and the reasoning was sound *given the variables under
consideration* - but the cable was never one of them, so "the only difference is the HAT" was never true.
**A conclusion from elimination is only as good as the list of things you thought to eliminate.**

**Three fixes came out of it, all worth having regardless of the wiring:**

- `gpio_init_uart` sets a **pull-up on GPIO15 (RX)**. It used to disable the pull on both UART pins,
  reasoning they are "externally driven" - true of GPIO14 (TX, which we drive) and false of RX, which is
  an input and floats whenever nothing drives it. Any Pi running this port with no serial cable attached
  had a floating RX pin. (With the cable out, this alone makes it silent.)
- `pl011_rx_drain` **discards bytes the UART flagged** framing/parity/break/overrun instead of masking the
  flags off with `DR & 0xFF`, which promoted genuine line errors to input. Free on a healthy line. Note
  this did NOT fix the fault above, because those bytes carry no flags - it is a separate real bug found
  along the way.
- The receiver **shuts itself off** after 4096 bytes dropped for lack of ring space, reports loudly, and
  hands the ring to the keyboard. Serial output is untouched. It latches until reboot rather than
  retrying, because silently resuming a line just declared faulty is the fallback §26.7 forbids.

**A note on that threshold, because the first two attempts were wrong.** It first fired at 2000 discarded
*error* bytes and never triggered (the real fault produced ~945 in the session that motivated it, and
those had no error flags anyway). The second attempt looked for 512 *identical consecutive* bytes and also
never fired, because occasional `0x00`s among the `0xFF`s reset the run counter. Both were measuring a
proxy. The third measures the harm itself - bytes dropped because the ring was full - which is exactly the
starvation that matters and is indifferent to what the bytes contain. **A check that cannot fire in its
own worked example is worse than no check: it reads as "the line is fine".**

> **Unproven in the field.** The shut-off has been verified in QEMU not to false-positive on a healthy
> line, but has never been observed *firing* on real hardware - the Pi it was written for was fixed by
> unplugging the cable instead. Treat it as untested until something trips it.

