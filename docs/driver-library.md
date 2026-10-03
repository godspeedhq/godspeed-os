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
so a fixed pause IS repeated, and is the next candidate - see below.)

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
  until that is resolved rather than converted into something that is quietly wrong.

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

`xhci`'s three sleeping loops can now move, and are the next step rather than part of this one.

**Verified:** builds for every port, and on the Pi 4 on 2026-10-03: 49 function-ready waits (boot,
five `wifi radio powercycle`s, and the chaos restarts), every one READY at the first read in 1-2 ms -
the same figures as the sessions before it - and the card ready at its first CMD5 every time, so the
new pace was never needed. No wait expired. `chaos max-carnage all-services` (50 rounds, 364 kills)
recovered the radio, which rejoined and took its lease; a power cycle after it rejoined and `ping
8.8.8.8` answered 5 of 5.

## What this supersedes

`docs/stdlib-design.md` ("MMIO and DMA stay in the SDK, permanently") drew the line between `gs` and
the SDK at hardware. That line is moved, not erased: hardware AUTHORITY still comes only through the
SDK's audited layer, and `unsafe` still lives only there (18.1). What moves into `gs` is the safe,
device-neutral machinery a driver builds on - one mechanism at a time, each one found in real drivers
first. `backlog/71` (the path to a v1 promise) depends on this shape: it puts `gs::driver` OUTSIDE the
first covered surface, because hardware support keeps growing.
