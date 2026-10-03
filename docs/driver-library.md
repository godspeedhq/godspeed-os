# The driver library: `gs::driver`

**Status: adopted 2026-10-02, from the operator's design guidance. One mechanism built (`wait`), one
driver converted (`wifi-driver`). Everything else below is the method, not a list of things to build.**

## The principle

> Drivers may be privileged consumers of hardware authority, but they should not be privileged
> consumers of complexity.

A driver holds authority nothing else holds: a register window, a DMA arena, an interrupt, the power to
its device. That is necessary, and the capability model already bounds it. What is NOT necessary is for
every driver to also carry its own copy of the machinery around that authority - its own bounded wait,
its own clock arithmetic, its own way of saying a wait gave up. The device remains complicated. The
interface for writing a Godspeed driver does not have to be.

## The layering

```text
  application            driver
       |                    |
    gs (std)           gs::driver   <- device-neutral mechanisms only
       |                    |
       +------ SDK ---------+       <- syscalls, the audited unsafe (CLAUDE.md 18.1)
               |
             kernel                 <- MISCIS, unchanged
```

A driver reaches for `gs::driver` wherever a safe, stable mechanism exists there, and for the SDK only
where one does not yet. **Nothing here adds a kernel responsibility.** `gs::driver` is the mechanisms
the kernel already offers, in the one shape a driver should use; MISCIS (CLAUDE.md 4.3) does not move.

## What goes in, and what never does

The question for every part of a driver is the one that decides membership:

> **Which parts of this driver describe the device? Which parts describe a reusable Godspeed mechanism?**

| Describes the device: stays in the driver | A mechanism: candidate for `gs::driver` |
|-------------------------------------------|------------------------------------------|
| The Broadcom control protocol (BCDC)      | Waiting for hardware, bounded by time (**built: `wait`**) |
| BDC framing, firmware command ids         | Interrupt waiting                        |
| The CLM blob, escan, `bss_info`           | DMA and buffer facilities                |
| A chip's recovery sequence                | Bus access                               |
| A controller's register map and its errata | Power leases and device power/reset authority |
|                                           | Firmware access                          |
|                                           | Safe MMIO                                |

**No device-class APIs, ever.** There is no `gs::driver::wifi`, `::audio`, `::ethernet` or `::usb`. A
class API is a guess about what every device of a kind needs, made before most of them exist, and it
freezes the first device's shape into every later one. A family of devices that genuinely shares a
protocol gets a DOMAIN library outside the standard library, built on `gs::driver`: `sdk/wifi` is the
first (the SDIO protocol, 802.11 frames, WPA2, the `Station` seam), and it is not part of `gs`.

## How a mechanism gets in: discovered, then tested

**Wi-Fi discovers; audio tests.** An abstraction is not designed in advance and then imposed. It is
found written out by hand - more than once, differently each time - in real drivers, moved here, and
the drivers converted to it. Then a second, INDEPENDENT kind of driver tries to use it:

- if it fits naturally, it is probably general;
- if it has to be bent, it is probably shaped like the driver it came from, and goes back.

Wi-Fi is the discovering driver because it is the newest and most complex. Audio is the planned
independent test, chosen because it shares no protocol with Wi-Fi at all - and it is to be written
WITHOUT forcing it through anything Wi-Fi-shaped.

## Unsafe

`gs` is `#![deny(unsafe_code)]`, and so is every driver. When a driver appears to need `unsafe`, that
is not a request for an exception. It is a question:

> **Which safe `gs::driver` abstraction is missing?**

The answer lands in `gs::driver`, or in the SDK's audited layer (CLAUDE.md 18.1) beneath it - never in
the driver.

## Migration: one mechanism at a time, never a rewrite

Drivers are not rewritten around a library that does not exist yet. The loop is:

1. In Wi-Fi, find ONE repeated, device-neutral interaction with the SDK.
2. Move it behind a safe `gs::driver` mechanism.
3. Convert Wi-Fi to it. Verify on hardware.
4. Repeat only where the result stays clear.

Then audio, as the independent test.

## Step 1: `gs::driver::wait` (2026-10-02)

**What was repeated.** Every driver waits for a register. Six of them wrote that loop by hand -
`xhci`'s `spin`, `dwc2`'s `wait_until`, `genet`'s mask wait, `dwmac`'s `wait_clear` and `mdio_idle`,
`wifi-driver`'s host - and the copies disagreed:

- **`wifi-driver`'s SDIO host bounded eight of its nine waits by an ITERATION COUNT**
  (`t > 1_000_000`), which the project's own rule forbids as a timeout (CLAUDE.md 26.6: a count is not
  a duration). Its ninth, `park`, used the clock.
- **The time-bounded ones disagreed about an uncalibrated clock** (`tsc_ticks_per_10ms` reads 0). The
  network drivers fall back to a poll count; `xhci` and `dwc2` turn the zero into a one-tick budget and
  give up after a single look.

**What was built.** `stdlib/rust/src/driver/wait.rs`: a `Budget` (microsecond resolution), a
`Deadline` for loops with more than one way out, and `until(ctx, budget, cond)` for the common single
condition. It polls rather than sleeps; it never logs, because only the driver knows which wait this
was, and expiry is a `Result` the caller must handle; on an uncalibrated machine its bound is
`UNCALIBRATED_POLLS` looks, the figure the network drivers had already settled on, and it says so.

**What was converted.** `services/wifi-driver/src/host.rs`, all nine waits. The budgets were chosen
from a measurement rather than a guess: on the Pi 4 at the Arm clock's maximum one register look costs
about 250 ns (the firmware upload: ~320 completion looks per command at ~5 ms per 1 KiB command), so
the million-look count was about a quarter second and the two-million one about half. The budgets are
twice those - `CONTROL_WAIT` 500 ms, `CARD_WAIT` 1000 ms - so no wait that succeeded under the count
can expire under the clock. The poll counts the upload reports (`take_waits`) are still counted; they
are an instrument now, not the bound.

**Verified:** QEMU (Pi 4 machine): the host resets, its clock stabilises and commands complete through
the new waits; no radio is emulated, so the firmware path is for the board.

**Verified on hardware, 2026-10-02 (Pi 4).** Two sessions, nine firmware uploads (boot, six
`wifi radio powercycle`, chaos recoveries), against a session on the count-bounded waits the same day:

| | count-bounded (before) | `gs::driver::wait` (after) |
|---|---|---|
| upload, `rung 4 ok` to `firmware written` | ~3.0 s | 3.03-3.05 s |
| completion looks per upload | 186,815 - 192,020 | 44,069 - 44,663 |
| FIFO looks per upload | ~8,925 | 0 |

The time is the card's, so it did not move; each look now also reads the clock, so about a quarter as
many fit in it, as predicted. The FIFO was ready at the first look every time, which a `Deadline` that
checks before it loops reports as zero. No wait expired. `chaos max-carnage all-services` (50 rounds)
recovered the radio and the network on the new waits.

## Step 1b: `genet` on `wait` (2026-10-02)

The first driver converted that `wait` was not built from, and the right one to go second: the Pi 4's
ethernet MAC, on the board at hand, and already the closest copy to what `wait` became - a time budget
with a 200,000-look fallback when the clock is uncalibrated. Its two waits (MDIO `START_BUSY`, 100 ms;
a DMA engine reporting itself started, 100 ms) are `wait::until` now, their budgets unchanged, and the
file's own `UNCALIBRATED_POLLS` is gone in favour of the library's identical one. It fitted without
bending: a register, a mask, a budget, and the driver saying in its own words what did not happen.

What did NOT move is worth saying, because it is the next candidate rather than an oversight:
`delay_us` waits a fixed time for nothing in particular ("give the PHY a moment"), which is a different
mechanism from waiting for a condition, so it stays in the driver until it is moved as its own step.
(This said it was the only hand-rolled delay among the drivers. It is not: step 1c found two in `xhci`,
so a fixed pause IS repeated, and is the next candidate - see below. Step 1f moved it.)

**Verified:** builds for every port (`nic-driver` is one crate on all four), and on the Pi 4 the same
day: no MDIO or DMA-start wait expired, cable ping 0% loss, the cable and the radio stepping in and out
for each other, and `chaos max-carnage all-services` (50 rounds) bringing `genet` up again on all 26 of
`nic-driver`'s restarts.

**Not converted yet, deliberately, one change at a time:**

| Driver | Board | Its wait | Note |
|---|---|---|---|
| `block-driver` (`sdhci.rs`) | Pi 2 | six `t > 1_000_000` loops | **count-bounded, the same defect `wifi-driver` had** - missed by the first census, which looked for named helpers and this one has none |
| `dwc2` | Pi 2 | `wait_until` | one-look budget when uncalibrated |
| `nic-driver` `dwmac` | VisionFive | `wait_clear`, `mdio_idle` | 200,000 and 20,000 looks when uncalibrated |
| ~~`sdk/wifi` (`sdio.rs`)~~ | Pi 4 | function-ready | done in step 1d |

## Step 1c: `xhci` on `wait` (2026-10-02)

The test the second driver could not give: a wait that LOGS. `xhci`'s `spin` takes the register
condition in words and prints it when the wait expires, and `wait` deliberately never logs. It fitted
anyway, and the shape it fitted in is the one the module intends: `spin` stays as `xhci`'s own thin
wrapper - `wait::until` underneath, the driver's line on `Err` - so the five waits that call it
(`PORTSC.PED`, `USBSTS.HCH` both ways, `USBSTS.CNR`, `USBCMD.HCRST`) are unchanged at the call site.

It fixed one case. `spin` built its budget from `duration_cycles`, which on an uncalibrated clock floors
to one tick, so every one of those waits gave up after a single look - a controller reset that never
waited for the reset. The library bounds that case by `UNCALIBRATED_POLLS` looks.

**What `xhci` showed that is NOT converted, and why.**

- **Two fixed busy-pauses** (a 2 ms settle after `HCRST`, the reset-recovery hold after a root-port
  reset), spinning on `read_tsc` for a set time. With `genet`'s `delay_us` that is three, in two
  drivers: a fixed pause is repeated, and it is the next mechanism. It is a different one from `wait` -
  nothing is being waited FOR - and on an uncalibrated clock a pause has no honest bound at all, which
  is a question to settle when it is built, not to answer by folding it into `wait`.
- **Three deadline loops that SLEEP between polls** (the hub port probe, mass-storage spin-up, a disk
  transfer). `Deadline` would measure them, but its uncalibrated fallback counts LOOKS, and 200,000
  looks with a sleep between each is not the bound it is for a loop that spins. They stay hand-written
  until that is resolved rather than converted into something that is quietly wrong. (This census was
  wrong, corrected in step 1e: of the three, only the hub probe sleeps, and two that DO sleep were
  missed. `xhci` built five deadlines by hand.)

**Verified:** builds for every port (`xhci` is one crate on three), and on the Pi 4 on 2026-10-03: no
`spin` wait expired anywhere in the session; the controller reset cleanly all 28 times it was asked
(boot, hot-plug of the keyboard and the stick, and every restart); `chaos max-carnage all-services` (50
rounds, 344 kills) brought the keyboard and the disk back. One `dir /` just after chaos waited out
`block-driver`'s 10 s while `xhci` was re-enumerating a replugged stick - none of its own waits
expired, and the next `dir /` answered; that is the driver being busy, not a wait giving up.

## Step 1d: a PACED wait, and `sdk/wifi` on it (2026-10-03)

Step 1c left a question open: a wait that sleeps between looks. `xhci` has three such loops and
`sdk/wifi` a fourth (the SDIO function-ready wait, a look a millisecond for up to three seconds), so the
shape is repeated, and it is not a new mechanism - it is `wait` with a pace. `Deadline::paced(budget,
pace)` and `until_paced` sleep the pace between looks themselves, through `Deadline::pause`, and that
is what answers the question: when the wait owns the pace, its uncalibrated bound can be the number of
paces that fit the budget rather than 200,000 looks that are each a sleep apart. On such a machine the
kernel's sleep is itself only approximate, so that is a count of pauses, not a duration; it still ends.

`sdk/wifi`'s two waits are on it:

- **Function ready**: three seconds, a look a millisecond, unchanged. It built its deadline by hand from
  `duration_cycles`, which on an uncalibrated clock floored to one tick: one read, then give up, and a
  reported elapsed time in ticks rather than milliseconds.
- **The card's CMD5 ready**: this was 100 asks back to back, a COUNT, however long 100 commands take on
  this bus. It is now the reference's duration - Linux's `mmc_send_io_op_cond` asks 100 times 10 ms
  apart, a second - paced the same way. A card that is ready at once, as this one has been, sees no
  difference.

`sdk/wifi` now depends on the stdlib, which is the layering above: a radio is a driver and reaches for
`gs::driver` like any other. Nothing in the stdlib depends on `sdk/wifi`, so there is no cycle.

`xhci`'s sleeping loops can now move, and are the next step rather than part of this one.

**Verified:** builds for every port, and on the Pi 4 on 2026-10-03: 49 function-ready waits (boot,
five `wifi radio powercycle`s, and the chaos restarts), every one READY at the first read in 1-2 ms -
the same figures as the sessions before it - and the card ready at its first CMD5 every time, so the
new pace was never needed. No wait expired. `chaos max-carnage all-services` (50 rounds, 364 kills)
recovered the radio, which rejoined and took its lease; a power cycle after it rejoined and `ping
8.8.8.8` answered 5 of 5.

## Step 1e: `xhci`'s hand-built deadlines on `wait` (2026-10-03)

Looking at them to convert them corrected step 1c's census. `xhci` built FIVE deadlines by hand, each
from `read_tsc` and `duration_cycles`, and three of them sleep:

| Loop | Budget | Sleeps | Now |
|---|---|---|---|
| hub port-status probe | `PROBE_ANSWER_MS` | 1 ms | `Deadline::paced` |
| root ports settling after a reset | `ROOT_PORT_SETTLE_MS` | 1 ms | `until_paced` |
| back-port re-scan while no keyboard is bound | `HUB_RESCAN_MS` | `IDLE_WAIT_MS` | `Deadline::paced` |
| mass storage spinning up, across `TEST UNIT READY` attempts | 20 s | no | `Deadline::start` |
| a disk transfer's completion (`msc.rs`) | `XFER_TIMEOUT_MS` | no | `Deadline::start` |

On a calibrated clock nothing changes: the budgets and the paces are the ones the loops already had.
On an uncalibrated one, each had the defect step 1c fixed in `spin` - `duration_cycles` floors to one
tick - and in two it was worse than giving up early: the spin-up deadline had passed before its first
check, so a stick was dropped without being asked once, and a disk transfer got one poll window before
it was declared dead.

One bound is long and is recorded rather than hidden: a look in the transfer wait is a whole poll window
of 4096 event-ring reads, so on an uncalibrated clock its `UNCALIBRATED_POLLS` looks are far more than
30 s. It ends, which the one-window version also did, but it ended wrongly.

`xhci` now has no hand-built deadline. What is left by hand is the two fixed busy-pauses, which wait for
nothing and are the next mechanism.

**Verified:** builds for every port, and on the Pi 4 on 2026-10-03: none of the five expired - no
spin-up give-up, no transfer without a completion, no `TIMEOUT` line - across boot, the keyboard and the
stick unplugged and replugged before and after chaos, the controller resetting 42 times, the disk
found again 24 times, and `chaos max-carnage all-services` (50 rounds, 348 kills). Hub probes answered
401 of 404, as before.

One number looked like a regression and is not: the first `xhci: alive` line after chaos charged 5.9 s
of a minute to the hub segment, against a few hundred milliseconds in earlier sessions. That minute held
5 re-enumerations (the stick and keyboard out for about 6 s, re-scanned every `HUB_RESCAN_MS`), and the
pass timer is not restarted across a re-enumeration, so the whole of one is charged to the hub segment
of the pass after it. The 5.9 s is the 6 s the devices were out. It is an accounting fact about the
heartbeat that predates this step, not time spent in a probe.

## Step 1f: `gs::driver::delay` - a hold, waiting for nothing (2026-10-03)

The second mechanism, and the first that is not `wait`. Some hardware needs a gap no register reports
the end of: let a reset bit land before the next write, give a port the recovery time a specification
demands before addressing it. There is no condition to look at, so it is not a wait with a budget; it
is a duration, and `delay::hold(ctx, Budget)` holds for it.

It was repeated, three times in two drivers on the Pi 4, and the copies disagreed about the
uncalibrated clock in the direction that matters:

| Hold | Length | Uncalibrated, before |
|---|---|---|
| `genet`, after each MAC reset write (three) | 10 us | yielded once - nothing at all when nothing else was runnable |
| `xhci`, settle after `HCRST` | 2 ms | `duration_cycles` floored to one tick: no hold |
| `xhci`, reset recovery after a root-port reset | `RESET_RECOVERY_MS` | the same: no hold |

A hold is a MINIMUM - the device needs at least this long - so the safe error is too long, never too
short, and both copies erred short. That settles the question step 1c left open, "a pause has no honest
bound on an uncalibrated clock": it does not need one, because it is not bounded above, it is bounded
BELOW. `hold` spins on a calibrated clock, measured by the counter as before; on an uncalibrated one it
sleeps as many scheduler quanta as the hold needs at the constitution's nominal 10 ms (CLAUDE.md 9.1),
at least one, because the quantum is the one duration the kernel can still measure there.

It does not sleep on a calibrated clock even for `xhci`'s 55 ms recovery hold, deliberately, in this
step: the kernel's sleep floors at a quantum, so a sleeping hold changes the timing the hardware was
verified with. `ehci` already parks for the bulk of a long hold and spins the remainder, which is the
shape a long hold should have; it is one driver, and when a second does it, it belongs here.

**Not converted, and why:**

| Driver | Board | Its hold | Note |
|---|---|---|---|
| `block-driver` (`ahci.rs`) | x86 | `COMRESET_HOLD_CYCLES`, `LINK_WAIT_CYCLES` | **raw cycle counts**, the same class `xhci`'s settle was before it was a duration |
| `ehci` | x86 | `delay_cycles` | parks then spins; takes cycles, not a duration |

**Verified:** builds for every port, and on the Pi 4 on 2026-10-03, with the cable in so both paths ran:
`genet` configured its MAC 22 times (boot and 21 `nic-driver` restarts under `chaos max-carnage`, 50
rounds, 343 kills) with no reset or MDIO failure, and the cable took a lease and answered ping before and
after; `xhci` reset its controller 36 times and found the disk 25 times, with no `TIMEOUT` and no
Transaction Error addressing a device, and the keyboard and stick came back from hot-plug after chaos.

## What this supersedes

`docs/stdlib-design.md` ("MMIO and DMA stay in the SDK, permanently") drew the line between `gs` and
the SDK at hardware. That line is moved, not erased: hardware AUTHORITY still comes only through the
SDK's audited layer, and `unsafe` still lives only there (18.1). What moves into `gs` is the safe,
device-neutral machinery a driver builds on - one mechanism at a time, each one found in real drivers
first. `backlog/71` (the path to a v1 promise) depends on this shape: it puts `gs::driver` OUTSIDE the
first covered surface, because hardware support keeps growing.
