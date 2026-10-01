# 69. The Pi 4 radio does not survive a respawn of its driver: the firmware traps on any chip the host can reset, and only a power cycle is a power-on

**Status: CLOSED 2026-10-01.** Every case recovers without a reboot, and the stopped-firmware case now comes up cold EVERY time. The "warm chip" below was a SLOW HOST: the Pi firmware drops the Arm cores to their minimum clock a minute after boot when no OS sets a rate, and a firmware uploaded that slowly traps at start (`docs/wifi.md` 55). The `power` service now holds the clock at its maximum for the length of each load through a lease (`docs/power.md` 15, `CpuClock` syscall 55); on the card, five loads from boot to nearly four minutes after it - three `powercycle`s and an `off hard` / `on` - all came up cold and joined (`docs/wifi.md` 57). Why a slow upload traps is recorded as open in `docs/wifi.md` 57, not chased.

**Previous status: OPEN - PARTLY RESOLVED 2026-10-01.** The respawn of a driver whose firmware is still RUNNING is resolved in userspace, by not restarting the firmware at all (`docs/wifi.md` 46): the card answers CMD52 with function 2 up, so the firmware is alive; stages 9-11 are skipped, the bus is brought up on it, the CLM is not re-sent (refused while up), and it joins from `/wifi.keys`. Hardware: post-storm instance adopted, scanned, joined, pinged 5/5; the kill cost the link ~7 s. No reset, no power cycle. What stays OPEN is a firmware that has STOPPED - killed mid-upload, or not answering. `DevicePower` (syscall 54, CLAUDE.md 12.3 amendment 2026-10-01) lets the driver cut and restore the radio's power, but a power cycle comes up cold only SOME of the time: the rest come up warm and the firmware traps at start (`docs/wifi.md` 47, 48). A warm chip is recovered by `wifi radio powercycle`, one cycle per run and re-runnable, and no case costs a reboot - but nothing yet makes a power cycle a power-on every time. The chain below stands as the record of why a firmware RESTART needs power on this chip.

**Original status: OPEN - established 2026-09-30/10-01 over seven boots under `chaos max-carnage`, the last one the operator's own choice of experiment (zero the RAM) before any kernel change; the remedy is a kernel-side action the operator has not authorised. `docs/wifi.md` 45 has the full chain.**

## What a respawn now does, and where it ends

A killed `wifi-driver` is respawned by the supervisor onto a chip the dead instance left running. Five
things about that chip differ from power-on, and the driver now handles four of them exactly as the
reference does: the SDIO I/O side (CCCR RES), the ARM core's state for register reads (reset and released
halted), the 802.11 core (reset before the upload), and function 2's readiness (waited for by time). The
fifth is the chip as a whole: its firmware, uploaded and released on a chip reset through the PMU
watchdog, traps at its first instructions in ROM (`trap type 0x1, epc 0x0009384c, sp 0`). A chip reset
is not a power-on for the CYW43455, and Broadcom's own driver never treats it as one: `brcmf_sdio_bus_reset`
power-cycles the card through `mmc_hw_reset`.

Measured: 57 kills, 52 watchdog resets, 48 confirmed fresh, 1 join (the boot's), 0 panics, 0 wedges.

## What would close it

The WLAN regulator (`WL_REG_ON`) on the Pi 4 is a pin on the firmware's GPIO expander, set through the
mailbox. Driving it off and on from the driver now EXISTS - `DevicePower`, 2026-10-01 - and it was NOT
sufficient: with the pin read back low, cycles come up cold only some of the time (`docs/wifi.md` 47),
and the hold-off is not the variable (50 ms, 500 ms, 2 s and 75 s all produced warm starts). The leading
reading is that the HOST side of the bus is not in its boot state at the rising edge of `WL_REG_ON`, where
the chip samples its straps. The clock-only change and the userspace park of the SDIO host both tested
that and neither fixed it. What is left is the SDIO PADS themselves (pull resistors and pin mux, in the
GPIO block the kernel owns): park them (input, pulls off) inside `DevicePower` for the off window. That
is a wider kernel change on a battle-tested port, so it is recorded here for the operator's decision
rather than made.

## What would NOT close it, tried

- Identification alone (CMD0/CMD5): the card does not answer CMD5 warm. Fixed by CCCR RES.
- Sizing the ARM's memory: reads zero on a running core AND on a core held in reset. Fixed by
  reset-and-release halted.
- The 802.11 core left running by the old firmware: reset before the upload. Correct; not sufficient.
- Waiting longer for function 2: three seconds changes nothing when the firmware has trapped.
- The chipcommon watchdog (0x80): half a reset - CMD52 answers, CMD5 never again, until power-on.
- The PMU watchdog (0x634): a whole reset by every host-visible sign, and the firmware still traps.
- Zeroing the RAM the upload does not rewrite (203 KiB between image and NVRAM), on the reasoning
  that a power cycle clears RAM and a watchdog does not: the firmware traps identically (`type 0x1,
  epc 0x9384c, pc 0x25`, record at `0x25ff08`). The difference is not in RAM. Kept, as hygiene.
- Stopping the card clock just before the power-on (2026-10-01): seven completed warm loads, seven traps,
  against about four cold in seven without it. Removed.
- Parking the SDIO host for the WHOLE off window (software reset, clocks off; `docs/wifi.md` 48),
  2026-10-01: REFUTED. 13:58 cold, 14:30:39 trapped, 14:30:54 trapped - one cold in three - and 14:43:49
  `off hard` then `on` trapped too. Seventy-five seconds powered down had already trapped without it.
  The park stays in the code as the host's honest state while the chip is unpowered; it is not the fix.
  The next experiment is the kernel's pad park above, on the operator's word.

## Until then

A respawned `wifi-driver` ADOPTS a live firmware and rejoins (no reset, no power cycle). A stopped one is
power-cycled by the driver ONCE; if the chip comes up warm, the driver serves `radio down` for the rest
of its life with its reason - `wifi status` prints the firmware trapped at start, no working radio on
its bus, or the bring-up stopped (`docs/wifi.md` 49) - and `net` says `the radio is not joined`. `wifi
radio on` and `wifi radio powercycle` retry from there: `powercycle` cuts the chip's power through
`DevicePower` and restarts the driver, one cycle per run, and can be run again. No reboot is
needed in any of these states. The cable path is unaffected.
