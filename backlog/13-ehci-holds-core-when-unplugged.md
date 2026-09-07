# 13. EHCI holds a core while a device is unplugged, and xHCI `Enable Slot` timeouts

**Severity:** cosmetic-to-minor. Neither costs a test failure on any machine; both are pre-existing and
predate the single-core work.
**Observed:** 2026-09-07, HP T630 and Wyse 5070, both booted single-core.

## A. `ehci` burns a core for a few seconds after a device is unplugged

`observe` reports `ehci` at 100% for as long as the device stays unplugged, and 0% the moment it is
plugged back in. The core total agrees frame for frame, so this is real CPU:

```
ehci=100%  core=100%   53s      <- unplugged, sustained across every frame
ehci=100%  core=100%   54s ... 57s
ehci=100%  core=20%    58s      <- transition
ehci=0%    core=18%    59s      <- device replugged
```

The user recalls the same behaviour from months before this branch, and nothing in the single-core
work touches it.

**Two wrong readings were published about this before the above; both are recorded because the way
they were reached is the thing to avoid.** First, "a task cannot use 100% of a core that is 53% busy,
so the instrument is lying" - the 53% frame was captured while the connector was PARTIALLY INSERTED,
a physical state that is neither plugged nor unplugged, so it was never comparable. Second, "it is a
3-second burst that settles" - that came from pairing an `ehci` row list with a `core` line list from
two separate greps, without checking the rows were from the same frame. Paired in file order they
agree exactly, and the drop is the REPLUG, not a decay.

Both errors have the same shape: a story built from readings that were never established to be
comparable. The instrument was consistent throughout; the pairing was not.

Not the cause, though all three were real and are fixed:

- `delay_cycles` was a bare `while read_tsc() {}` holding the core for the whole delay. Now sleeps for
  the bulk and spins only the remainder, which preserves the USB 2.0 7.1.7.5 minimum-hold contract.
- `wait()` polled MMIO with `yield_cpu` for up to 250 ms. `yield_cpu` leaves the task RUNNABLE, so a
  single-core scheduler hands the core straight back - yielding is not the same as not using the core.
  Now yields 2 ms then parks.
- The qTD completion wait in `control` was a bare spin with a **one second** budget
  (`CTRL_XFER_CYCLES = 2_000_000_000`), spent in full whenever a transfer does not complete. Now
  yields 2 ms then parks, and logs once when a transfer runs out its budget.

Those three are worth keeping on their own merits - each one held a core with no yield or no sleep -
but the 100% survives all three, so the remaining cost is somewhere else on the unplug path. What is known:
the rescan loop is NOT spinning (5 port-census cycles in a minute, so `sleep_ms(HOTPLUG_POLL_MS)` is
working), and the hub's own control transfers SUCCEED while the keyboard is the thing unplugged.

**Where to look next, and what to do first:** instrument the unplug path before changing it. It has no
heartbeat - `ehci: alive` lives inside `poll_devices`, which only runs while a device is present - so
the exact window that misbehaves is the one window that reports nothing. Three fixes were flashed at
this on reasoning alone and none of them landed; the fourth attempt should measure first. A work-time
counter around `wait_for_connection` would settle it in one boot.

## B. `xhci: Enable Slot - no completion`, 105 times on the Wyse

Root port 15 failed to enumerate three times in a pass and was skipped until replug, with
`post-command USBSTS=0x00000018 (HCH=0 HSE=0 HCE=0 CNR=0)` - the controller is healthy and simply does
not complete the command. Costs no test failure and does not affect the bound device. Unknown whether
it is a marginal port, a marginal device, or a driver gap; nobody has tried a different device in that
port, which is the cheapest first experiment.
