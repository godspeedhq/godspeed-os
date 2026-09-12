# 22. Pi 4: the display went blank during `selfcheck` while the system stayed fully alive

**Severity:** unexplained. Nothing was lost and nothing hung - but the machine's only screen stopped
showing a running system, which is the failure mode the console service exists to prevent.
**Status: OPEN, one occurrence, cause NOT established.** Recorded per 26.7 rather than guessed at.

## What happened

Boot of `af8dee41` on the Raspberry Pi 4, 2026-09-12 20:35:12. Operator report: "my tv went black
during the selfcheck". The serial capture (`build/serial_output.log`, boot 1 = lines 1-2233) says the
system was healthy throughout and for a further 53 seconds afterwards:

```
20:35:35  gsh> selfcheck
20:36:15  run: ran 461, failed 0
20:36:31  gsh>                      <- bare prompts: Enter pressed at a blank screen
20:36:32  gsh> drives               <- typed blind, ANSWERED
20:36:34    0  data  GSFS  15267 MiB (15263 MiB free)
20:36:34  gsh> ls                   <- typed blind, ANSWERED
20:36:36  /  (7 entries) ...
20:37:08  <power cycle by the operator>
```

The second boot ran `selfcheck` + `ping` + `drives` with the display working normally, so it is not
reproducible on demand.

## What the log RULES OUT

Checked in the boot-1 capture, so the next attempt does not re-derive them:

- **Not a panic or a reset.** No `KERNEL PANIC`, no watchdog, no self-reboot. The reboot at 20:37:08
  is the operator's power cycle, 32 seconds after the last shell command was answered.
- **The `console` service never died, never restarted, and never lost the framebuffer.** It is spawned
  once at 20:35:13 (`spawn[fb]: 'console' 1920x1080 at phys 0x3e402000 -> VA 0x58000000`), reports
  `console: serving the display`, and `status` shows it `Ready`/`Running` with `restarts 0` at
  20:35:40 and again at 20:36:14. The kernel never took the framebuffer back.
- **Not the framebuffer being reallocated under it.** The fb sits at phys `0x3e402000`, inside the
  firmware gap between the two usable regions the device tree reports (`0x1b02000..0x3b400000` and
  `0x40000000..0x80000000`). The frame allocator cannot hand it out.
- **Not the keyboard or the shell.** Both answered after the screen was blank - that is what the two
  blind commands above prove.
- **Not `b44258d0` / `af8dee41` in any obvious way.** Both touch `nic-driver` and `block-driver` only,
  both are dead-code removal and a mechanical `cfg` substitution, and neither is in the display path.
  This is NOT a clearance - it is one boot, and the branch has no earlier Pi 4 boot at these commits
  to compare against. It is the reason to suspect elsewhere first, not to stop looking here.

## What the log CANNOT say

Whether the `console` service was still painting. Everything above shows it alive and scheduled; none
of it shows a pixel reaching the panel. The two cases have completely different fixes:

- **console stopped serving** (jammed queue, stuck in a paint) - a service bug;
- **console kept painting into a buffer nobody scans out** - firmware/HDMI side, nothing to do with
  this code at all.

## The next concrete step, and it costs nothing to build

The discriminator already exists and is readable over serial while the screen is blank:

```
events metrics | where owner contains console        # run TWICE, ~5 s apart
status                                               # console's queue depth
```

`console msgs.received` is already published (seen at 2368 then 2432 in this very log). If it CLIMBS
while the screen is black, console is alive and painting and the fault is downstream of this system.
If it is FROZEN, console stopped serving - and `status`'s queue column says whether it is jammed at
16/16, which is the shape `project_console_service_perf` records.

So: **next time the screen goes black, run those two before power-cycling.** One occurrence with that
pair of readings settles which half of the system to look in; without it, any change is a guess.
