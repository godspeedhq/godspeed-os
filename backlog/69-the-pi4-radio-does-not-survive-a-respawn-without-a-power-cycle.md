# 69. The Pi 4 radio does not survive a respawn of its driver: the firmware traps on any chip the host can reset, and only a power cycle is a power-on

**Status: RESOLVED 2026-10-01 in userspace, by not restarting the firmware at all (`docs/wifi.md` 46).** A respawn ADOPTS the firmware the dead instance left running: the card answers CMD52 with function 2 up, so the firmware is alive; stages 9-11 are skipped, the bus is brought up on it, the CLM is not re-sent (refused while up), and it joins from `/wifi.keys`. Hardware: post-storm instance adopted, scanned, joined, pinged 5/5; the kill cost the link ~7 s. No reset, no power cycle. The chain below stands as the record of why a firmware RESTART needs power on this chip - that is now the rare case (a firmware killed mid-upload, or trapped), reported honestly and costing a reboot, rather than every respawn.

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
mailbox. The kernel already drives it once, at boot, when it powers the SDIO domain the driver is later
granted. Driving it off and on again when `wifi-driver` is (re)spawned - a per-device-class action in
`arch/aarch64`, mechanism and not policy - would hand every instance a chip as after power-on, and the
four host-side steps above become belt and braces. It is a kernel change on a battle-tested port, so it
is recorded here for the operator's decision rather than made.

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

## Until then

A respawned `wifi-driver` identifies the chip, uploads the firmware, reads out the trap, and serves
`radio down` honestly for the rest of its life. `net` says `the radio is not joined`. A reboot restores
it. The cable path is unaffected.
