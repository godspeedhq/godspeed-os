# The driver library: `gs::driver`

**Status: adopted 2026-10-02, from the operator's design guidance. Three mechanisms built (`wait`,
`delay`, `irq`); nine drivers converted across all four ports. "Current state" below says what exists; the
method sections say how anything gets in; the dated steps are the record.**

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

### What this supersedes

`docs/stdlib-design.md` ("MMIO and DMA stay in the SDK, permanently") drew the line between `gs` and
the SDK at hardware. That line is moved, not erased: hardware AUTHORITY still comes only through the
SDK's audited layer, and `unsafe` still lives only there (18.1). What moves into `gs` is the safe,
device-neutral machinery a driver builds on - one mechanism at a time, each one found in real drivers
first. `backlog/71` (the path to a v1 promise) depends on this shape: it puts `gs::driver` OUTSIDE the
first covered surface, because hardware support keeps growing.

## Current state (2026-10-09)

What exists now. The dated steps after the method sections are the record of how each piece got here,
and why; read them for the reasoning, not to find out what the library contains.

**Built.**

- **`wait`** - bounded waits for a condition. `Budget` (`us` / `ms`); `until` and `until_paced`;
  `Deadline::start` (polling) and `Deadline::paced` (sleeping a pace between looks), each with
  `expired`, `pause` and `elapsed_us`; `calibrated`. On an uncalibrated clock a polling wait gets
  `UNCALIBRATED_POLLS` (200,000) looks and a paced one the paces that fit its budget. It never logs.
  `Since` (2026-10-06) is a moment KEPT across calls, which a `Deadline` cannot be because it borrows the
  context: `now`, `passed(budget)`, `elapsed_us`, and (2026-10-09) `elapsed_ms` and `elapsed_ticks`. Its
  first users were the shared WiFi serve loop and the USB dongle's channel sweep; since every service
  moved onto `gs` (2026-10-09) it is how every driver keeps a heartbeat or a backoff. Uncalibrated,
  every budget has passed, so a dwell cannot become a hang. `ticks` and `ticks_per_10ms` (2026-10-09)
  are the counter and its rate, for code that MEASURES in ticks - the SDK's key repeat, `xhci`'s segment
  counters, `dwc2`'s sleep-accuracy sweep - and are documented as never the way to write a wait.
- **`delay`** - holds, for gaps nothing reports the end of. `hold` spins on a calibrated clock;
  `hold_parked` sleeps first and spins the rest, for holds of tens of milliseconds. On an uncalibrated
  clock both sleep whole scheduler quanta, erring long, because a hold is a minimum.
  **`hold_parked` came in with one caller (`ehci`)**, on probation - step 1l says why. It has three
  now (checked 2026-10-09): `ehci`, `wifi-usb` (`rtl8188.rs`) and `sdk/audio`'s settings retry, so
  the second driver step 1l waited for has arrived, though not the `xhci` hold it named.
- **`irq`** - waiting for a device's interrupt on the endpoint its clients also send to. `Irq::granted`,
  `routed`, `seen`; `wait(budget)` returns `Interrupt`, `Request(message)` or `Timeout`, so a request is
  handed back to be served and never dropped; `rearm` re-opens a level-triggered line (a no-op for MSI).
  With no interrupt routed the same loop is a timed wait that still serves requests. Its users are
  `audio-driver` (step 2) and `pwm-audio`, which runs on the no-interrupt path. `ehci` and `dwc2` re-arm
  through it (2026-10-09): each used to unmask a vector NUMBER of its own, and each number was proven
  equal to the one the kernel grants it (0x29, `hw_irqs_for`), so `Irq::granted(..).rearm` is the same
  unmask without a driver naming a vector. `xhci` reads its MSI vector through `Irq::vector`. Their WAIT
  loops are still hand-written - moving those onto `Irq::wait` changes a driver's structure and is owed
  its own hardware card on each board.

**Converted** (step in brackets): `wifi-driver` (1), `genet` (1b, 1f), `xhci` (1c, 1e, 1f), `sdk/wifi`
(1d), `dwc2` (1g, 1h), `dwmac` (1i), the x86 `nic-driver` (1j), `ahci` (1k), `ehci` (1l - **not yet
verified on hardware**). `audio-driver` and `pwm-audio` were written on the library from the start (`wait`, `delay`, `irq`).

**Left by hand, each with its reason in its step.**

| Site | Board | Why | Step |
|---|---|---|---|
| `nic-driver` `RX_POLL_MAX` | x86 | waits for traffic, tuned against `net-stack`'s deadline; needs a measurement | 1j |
| `dwc2`'s complete-split NYET retry | Pi 2 | a retry count of sleeps on the keyboard path | 1g |
| `dwmac`'s `rgmii_loopback_sweep` | VisionFive | compiled but never run | 1i |
| `block-driver` `sdhci.rs` | none | not compiled | 1b |

**Open gaps, recorded rather than hidden.**

- Where one LOOK is a whole transfer - `dwc2`'s hub reset and `net::bulk`, `xhci`'s disk transfer - the
  uncalibrated bound of 200,000 looks can be hours. It ends, which the one-look deadlines it replaced
  did not do correctly; but no one chose it as a bound.
- `xhci`'s 55 ms reset-recovery hold still spins (`delay::hold`). It was the named candidate to make
  `hold_parked` a mechanism two drivers use; `wifi-usb` became the second instead.
- None of the boards this work was tested on is uncalibrated, so every uncalibrated path above is
  reasoned and unit-tested, not observed.

## What goes in, and what never does

The question for every part of a driver is the one that decides membership:

> **Which parts of this driver describe the device? Which parts describe a reusable Godspeed mechanism?**

| Describes the device: stays in the driver | A mechanism: candidate for `gs::driver` |
|-------------------------------------------|------------------------------------------|
| The Broadcom control protocol (BCDC)      | Waiting for hardware, bounded by time (**built: `wait`, polling or paced**) |
|                                           | Holding still for a set minimum time (**built: `delay::hold`, `delay::hold_parked`**) |
| BDC framing, firmware command ids         | Interrupt waiting (**built: `irq`**)      |
| The CLM blob, escan, `brcmf_bss_info_le`           | DMA and buffer facilities                |
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

**What was repeated.** Every driver waits for a register. Five drivers wrote that loop by hand, six
times -
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

**Not converted at the time, deliberately, one change at a time (since: see the later steps):**

| Driver | Board | Its wait | Note |
|---|---|---|---|
| `block-driver` (`sdhci.rs`) | none | ten count-bounded loops | **not a candidate: the file is not compiled.** `block-driver`'s `main.rs` deliberately has no `mod sdhci`, because on the Pi 2 the SD card is the boot medium and using it as storage destroyed two boot cards. Code that is never built cannot be converted and verified, so it stays as it is; this row said otherwise until 2026-10-03 |
| ~~`dwc2`~~ | Pi 2 | `wait_until` and ten more | done in steps 1g and 1h |
| ~~`nic-driver` `dwmac`~~ | VisionFive | `wait_clear`, `mdio_idle`, transmit | done in step 1i |
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
  is a question to settle when it is built, not to answer by folding it into `wait`. (Step 1f built
  it.)
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

`xhci`'s sleeping loops can now move, and are the next step rather than part of this one. (Step 1e
moved them.)

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
nothing and are the next mechanism. (Step 1f moved them.)

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
| ~~`block-driver` (`ahci.rs`)~~ | x86 | two raw-cycle holds and seven iteration-count loops | done in step 1k |
| ~~`ehci`~~ | x86 | its reset delay, `wait`, the control transfer | done in step 1l |

**Verified:** builds for every port, and on the Pi 4 on 2026-10-03, with the cable in so both paths ran:
`genet` configured its MAC 22 times (boot and 21 `nic-driver` restarts under `chaos max-carnage`, 50
rounds, 343 kills) with no reset or MDIO failure, and the cable took a lease and answered ping before and
after; `xhci` reset its controller 36 times and found the disk 25 times, with no `TIMEOUT` and no
Transaction Error addressing a device, and the keyboard and stick came back from hot-plug after chaos.

## Step 1g: `dwc2` on `wait` - the first driver off the Pi 4 (2026-10-03)

The Pi 2's USB host: keyboard, hub, mass storage and the smsc95xx network adapter. The first conversion
on another board and another ISA (ARMv7), and the largest: ten hand-built deadlines, every one from
`read_tsc` and `duration_cycles`, so every one gave up after a single look on an uncalibrated clock.

| Wait | Budget | Now |
|---|---|---|
| `core::wait_until` - core reset, host mode, FIFO flushes (eight callers) | 100-500 ms | `until` |
| `chan::wait_halt` - a channel retiring its transfer | caller's | `Deadline::start` |
| `chan::wait_for_uframe` - a microframe boundary, sleeping sub-ms gaps itself | 2 ms | `Deadline::paced` |
| `chan::wait_until_at_least`, `chan::wait_uframe_abs` - the periodic split's schedule | 2 ms | `Deadline::start` |
| `hub` - a hub port finishing its reset | 200 ms | `Deadline::start` |
| `msc::bulk_xfer` - a bulk transfer, 1 ms between attempts | caller's | `Deadline::paced` |
| `msc::with_busy_retry` - a busy stick, 5 ms between attempts | caller's | `Deadline::paced` |
| `net::bulk` - an adapter transfer | caller's | `Deadline::start` |

On a calibrated clock nothing changes: the budgets and the paces are the ones the loops had.
`wait_for_uframe` is paced although it sleeps its own sub-millisecond gaps rather than calling
`pause`: on an uncalibrated clock each of those sleeps is a whole scheduler quantum, and the pace is what
keeps its bound to the two looks a 2 ms budget holds rather than 200,000 of them.

Two bounds are long on an uncalibrated clock and recorded rather than hidden, as `xhci`'s transfer wait
was: in the hub reset wait a look is a whole control transfer, and in `net::bulk` a whole channel
attempt, so the library's look count is far more than their budgets. They end; before, they ended after
one look.

**Not converted, and why:**

- **`chan::halt`** spins on a COUNT (`t > 100_000`), the defect itself. It takes no `ServiceContext`,
  because it runs from `program` / `program_ping` before every transfer, so converting it means threading
  `ctx` through the channel API. That is a change of its own and is left for one. (Step 1h made it.)
- **The complete-split NYET retry** counts 500 attempts with a 1 ms sleep between. It is a retry budget
  on the keyboard's control path, and the count is of sleeps, not of spins.
- **Plain sleeps, rate timers and the heartbeat** (`core.rs` settles, `hub.rs` and `enumerate.rs`
  address gaps, `main.rs`'s pass budget and back-offs) wait for no condition and are not waits.

**Verified:** builds for every port (`dwc2` is the Pi 2's alone), and on the Pi 2 on 2026-10-03: `dwc2`
came up 25 times (boot and the restarts under `chaos max-carnage all-services`, 50 rounds, 301 kills),
each through the converted core reset; no wait expired - no core-reset failure, no hub port that did
not finish its reset, no channel that never halted, no split given up. The keyboard, the stick and the
smsc95xx all came back: `dir /` and `ping 8.8.8.8` (0% loss) after chaos, and again after the keyboard
and the stick were unplugged and replugged. The three "dwc2 answered op 0x01 while we asked 0x12" lines
`nic-driver` prints when the stick is pulled are older than this step (the 2026-10-02 Pi 2 log has them).

## Step 1h: `dwc2`'s `chan::halt` on `wait` (2026-10-03)

The one `dwc2` wait step 1g left: `halt` spun until the core retired a channel, bounded by 100,000
register reads - a count, the defect itself. It runs from `program` / `program_ping` before every
transfer, which is why it had no `ServiceContext`; those two, `halt` and `net::arm_in` now take one, and
every caller already held it.

**The budget is chosen not to be shorter than the count.** How long 100,000 peripheral reads take on the
Pi 2 was never measured, and a budget that ran out before a halt that used to land would abandon the
channel - the exact failure `halt` exists to prevent (a request left in the core's few-entry queue,
which once stopped transmit forever). So `HALT_WAIT` is 50 ms: the count at about 0.5 us a read, an
upper estimate. The change can only wait longer, and only when a halt is not landing anyway. A shorter
figure needs a measurement.

Expiry stays quiet, as it was: this runs before every transfer, and the transfer that follows reports
the channel's state itself. Making it loud is a separate decision about a hot path.

`wait::until` looks at the condition before it starts the deadline, so a channel already idle (the usual
case) or one that retires at the first look costs no clock read at all.

**Verified:** builds for every port, and on the Pi 2 on 2026-10-03: `dwc2` came up 22 times (boot and
`chaos max-carnage all-services`, 50 rounds, 285 kills) with no channel that never halted, no split
given up and no hub reset left unfinished; `ping 8.8.8.8` answered with 0% loss before chaos, after it
(14 of 14) and after the keyboard and the stick were unplugged and replugged both before and after
chaos, and `dir /` listed the stick each time.

## Step 1i: `dwmac` on `wait` (2026-10-03)

The VisionFive 2 Lite's ethernet MAC - a third ISA (RISC-V 64) and a second MAC after `genet`. Like
`genet` it was already time-bounded and already honest about the uncalibrated case; unlike `genet` it
wrote that case out three times, as a separate loop beside each timed one, with two different ceilings:

| Wait | Budget | Uncalibrated, before | Now |
|---|---|---|---|
| `mdio_idle` - the MDIO master idle | `MDIO_US` | its own 20,000 looks | `until` |
| `wait_clear` - the DMA soft reset clearing | `RESET_US` | 200,000 looks | `until`, its time from `Ok` |
| transmit - the engine handing a descriptor back | `TX_US` | 200,000 looks, and the write-back NOT kept | `Deadline::start` |

The file's own two uncalibrated ceilings, the `per_10ms` field and the tick helper
are gone; the bring-up line that says the machine is uncalibrated now asks `wait::calibrated`. One
behaviour changed, on an uncalibrated machine only: the transmit wait now records the descriptor's
write-back there too, which the separate loop had dropped, so a failed send can say why on every
machine. MDIO's uncalibrated ceiling rises from 20,000 looks to the library's 200,000; on such a
machine either is a count, not a time.

**Not converted:** the turnaround wait in `rgmii_loopback_sweep` counts 20,000 spins, but the sweep is
compiled and deliberately not run (`let _ = rgmii_loopback_sweep;`). Code that is not run cannot be
converted and verified, so it is left, with a note at the place it is not called.

**Verified:** builds for every port, and on the VisionFive 2 Lite on 2026-10-03: `nic-driver` started
28 times (boot and `chaos max-carnage all-services`, 50 rounds, 339 kills) and found its PHY over MDIO
every time, with no MDIO wait that did not go idle; the 8 starts that lived long enough to reach the DMA
reset all cleared it in 1 us; no transmit failed, and `ping 8.8.8.8` answered with 0% loss before chaos
and after it. The other 20 starts were killed while waiting up to 5 s for the PHY to renegotiate after
its own reset, which comes before the DMA reset by design (`wait_for_link`), not a wait that failed.

## Step 1j: the x86 `nic-driver` (RTL8168 and e1000) on `wait` and `delay` (2026-10-03)

The first x86 driver, and the first of three there, done one at a time (`ahci` and `ehci` follow). Five
of its six waits:

| Wait | Before | Now |
|---|---|---|
| RTL8168 reset self-clearing | 300,000 yields - a count | `until`, `RESET_WAIT` |
| e1000 reset self-clearing | 1,000,000 yields - a count, and its expiry silent | `until`, `RESET_WAIT`, expiry logged |
| RTL8168 quiesce before the reset | yields until `read_tsc() < end` | `delay::hold` |
| RTL8168 and e1000 transmit confirmed | yields until `read_tsc() < end` | `await_tx`: `Deadline::start`, yielding between looks |

**The reset budget is Linux's, with headroom.** A count of yields is no time at all - on the T630 50,000
of them took over two seconds, so the RTL bound was over twelve seconds and the e1000's over forty -
and `r8169` polls the same bit 100 times 100 us apart. `RESET_WAIT` is 100 ms, ten times that. The three timed loops compared the counter with a
plain `<`, which a counter wrapping mid-wait ends at once; the library compares a wrapping difference.

`await_tx` keeps the yield between looks, because a send that has not landed at the first look is
usually microseconds away and the core is better given back. On an uncalibrated machine its bound is
the library's look count, and a look a yield apart is not a time either; recorded, as the sleeping
loops were before the pace existed.

**Not converted: the RX poll** (`RX_POLL_MAX`, 8,000 yields, in two places). It does not wait on the
hardware for a condition it will reach - it waits for TRAFFIC, which may never come, and the figure was
tuned on the T630 to stay under `net-stack`'s request deadline (50,000 yields took longer than it). A
time bound there is right, but choosing it needs that deadline and a measurement together, so it is left
for a step of its own rather than guessed.

**Verified:** builds for every port, and in QEMU on 2026-10-03 for the e1000: `osdev test shell` 215 passed, 0 failed (the two skips are the EHCI flood, which this QEMU has no controller for); the e1000 came up 6 times, its reset never failed to self-clear, and ARP, ping to the gateway and continuous `ping` passed through it, as did `chaos max-carnage`. And on the T630 (RTL8168) the same day: the RTL8168 reset self-cleared on all 20 starts (boot and `chaos max-carnage all-services`, 50 rounds, 337 kills), no transmit timed out, and `ping 8.8.8.8` answered 4 of 4 before chaos and after it.

## Step 1k: `ahci` on `wait` and `delay` (2026-10-03)

The x86 SATA disk, and the first conversion where the budgets had to be DERIVED rather than kept,
because most of what it had were not durations at all:

| Wait | Before | Now |
|---|---|---|
| command engine stopping (CR, FR) - three sites | 1,000,000 MMIO reads | `until`, `ENGINE_STOP_WAIT` 500 ms (AHCI 1.3.1 10.1.2) |
| PHY link up after COMRESET, BSY clear after it | 1,000,000 reads each | `until`, `LINK_WAIT` |
| port idle before a command | 2,000,000 reads | `until`, `CMD_IDLE_WAIT` 1 s |
| command slot clearing | 5,000,000 reads | `until`, `CMD_DONE_WAIT` 5 s |
| boot link waits and task-file ready (three) | `400_000_000` raw counter cycles | `until`, `LINK_WAIT` 300 ms |
| COMRESET DET hold (two) | `4_000_000` raw counter cycles | `delay::hold`, `COMRESET_HOLD` 2 ms |

**The command budgets come from the caller's deadline.** `fs` gives every block request 30 s and
`issue_io` makes up to three attempts with a port recovery between them, so one attempt has to fit in
under ten seconds or a failing disk is reported to nobody - `fs` has already given up. 1 s idle, 5 s
command and about 1.6 s of recovery (an engine stop and a full COMRESET) is about 7.6 s an attempt and
23 s for three. The counts they replace could not promise that: at a microsecond an MMIO read, three
attempts of seven million reads is 21 s before any recovery.

**The raw cycle counts were durations only on the T630.** 400,000,000 cycles is 200 ms on its ~2 GHz
counter and longer on a slower one; `LINK_WAIT` is 300 ms, which keeps every board at or above what it
had. The COMRESET hold is 2 ms, twice the spec's minimum, as the old count was on the T630.

`issue` and `identify` now take a `ServiceContext`; every caller already held one. Expiry behaves as
before everywhere: the port-bring-up waits proceed after their bound and leave the command that follows
to report the port, and the command waits return the same errors they did.

**Not converted here:** `block-driver`'s `xhciblk.rs` (the Pi 4 and VisionFive disk, through `xhci`)
builds its capacity deadline by hand. It is the same crate on other boards, so it is left for a step
that is tested on them.

**Verified:** builds for every port, and in QEMU on 2026-10-03 on an `ich9-ahci` disk: `osdev test fs-restart` 11 passed, 0 failed - GSFS flashed, a file written and read, `fs` killed, re-mounted and the file read back. (`osdev test shell` passed too, 215 of 215, but it attaches no SATA disk, so it does not count here.) And on the T630 the same day, on its Samsung SATA SSD: `block-driver` brought the port up and IDENTIFYd the disk on all 24 starts (boot and `chaos max-carnage all-services`, 50 rounds, 347 kills), no command timed out, waited on a busy port, failed or needed a retry, and after chaos `selfcheck` ran 524 with 0 failed, its filesystem check finding 17 files consistent with nothing to repair.

## Step 1l: `ehci` on `wait`, and `delay::hold_parked` (2026-10-03)

The last x86 driver. It also adds `delay::hold_parked` with ONE caller, which departs from step 1f's
condition ("when a second does it, it belongs here") and from this library's own rule that a mechanism
is found written out more than once. Recorded rather than hidden: it was moved so `ehci` could drop its
last hand-built timing loop, and it is on probation until a second driver parks. `xhci`'s 55 ms
recovery hold is the candidate. (This step first said `ehci` was the second driver to need a long hold
and so met step 1f's condition. `xhci` is the other long hold, and it still spins; the 2026-10-03 audit
caught the claim.)

**`delay::hold_parked`.** `ehci`'s own reset delay (delay_cycles, now gone) slept through a USB
reset timing (100 ms reset hold, 20 ms recovery, 50 ms debounce) and spun out only what the sleep left, because spinning all of it read
100% of a core in `observe` with nothing plugged in - the rescan loop holds on every turn. A MINIMUM
allows the late wake a sleep can give; the spin covers an early one. That is now
`delay::hold_parked`, and `ehci`'s eleven reset timings call it. It is a separate call from `hold`
rather than a change to it, so `xhci`'s 55 ms recovery hold, verified spinning on the Pi 4, keeps the
timing it was verified with; moving it to the parked hold is a step of its own.

| Wait | Before | Now |
|---|---|---|
| reset hold, recovery, debounce (eleven sites) | `200_000_000` / `40_000_000` / `100_000_000` raw counter cycles | `delay::hold_parked`, 100 / 20 / 50 ms |
| `wait` - a register bit | 250 ms, ended by `read_tsc() >= end` | `await_hw` |
| a control transfer's status qTD | `2_000_000_000` raw cycles (~1 s on the T630), ended by `>=` | `await_hw`, 1 s |

`await_hw` keeps the shape both waits already had - yield for the first 2 ms, then park between looks
(a 1 ms sleep, one scheduler quantum in practice) - on two PACED deadlines, one for the budget and one
for the spin phase. The spin phase's has to be paced: as first written it was a polling deadline,
which on an uncalibrated clock never expires, so every look was a yield and the budget's 250 or 1,000
paced looks were spent yielding - ending the wait far short of its budget. The audit caught it before
any hardware ran it; paced, the spin phase ends after two looks and the rest park. The comparisons it
replaced were `read_tsc() >= end` against a `wrapping_add`, which a counter wrapping mid-wait ends at
once. The raw cycle counts were the intended times only on the T630's ~2 GHz counter; they are those
times now on every board.

**Verified:** builds for every port. On the T630, NOT YET.

## Step 2: `gs::driver::irq` - interrupt waiting, tested by audio (2026-10-03)

**Where it came from.** Three drivers wait for their interrupt by hand - `xhci`, `ehci` and `dwc2` - and
each idles the same way: a timed receive on its endpoint, then a decision about what woke it. The
kernel's notification is a one-byte message naming the vector; anything else on the endpoint is a
client's request. The copies' own comments record what getting that wrong cost:

- **`dwc2` took a request and dropped it** whenever it had no disk, so `block-driver` - blocked in
  request/reply - hung before its first log line, and `fs` with it. A receive consumes; it does not peek.
- **`xhci` counted every wake as an interrupt**, so with a disk attached its "waking on interrupts" line
  was set by disk requests and proved nothing about MSI.
- **A receive deadline of zero blocks forever**, so a budget that rounds to zero turns the watchdog
  into a hang.

`irq::Irq::wait` is that loop once: it returns `Interrupt`, `Request(message)` or `Timeout`, and a
request comes back to the caller to be served. Its deadline is never zero (a host test pins it, with
the uncalibrated case floored to one scheduler quantum, the answer `duration_cycles` already gives).
With no interrupt routed it is a timed wait that still serves requests, so a driver keeps one loop.

**The test was audio, which none of the three resembles.** `audio-driver` refills a ring of sound when
the HD Audio stream says a period has played: each buffer descriptor asks for an interrupt on
completion, and the driver asks for a vector in its spawn row (`hwclass::pci_irq`). It fit without
bending - the refill loop's wait became `irq.wait(watchdog)`, and the three outcomes are three arms: the
interrupt clears the stream's status and re-arms, a request is refused (the driver has no protocol
until A4), a timeout counts a watchdog wake. The idle loop uses the same wait, which also passes over a
late interrupt raised while the stream was stopping instead of treating it as a request.

**Verified in QEMU** (`build/audio_irq_qemu.log`): the kernel routed MSI vector `0x30` to the HD Audio
controller, the tone played with 0 underruns, and the refill ran on **13 interrupts and 0 watchdog
wakes** - the same in the instance the supervisor restarted after a kill, which got the same vector
back. The prediction was 12, the periods the tone fills (11.7, rounded up); 13 twice is not explained
yet. The likely cause is the stream's position lagging its interrupt, so the twelfth look reads just
short of the tone, and that is a hypothesis until the position at each interrupt is logged.

**Not done: `xhci`, `ehci` and `dwc2` are still hand-written.** Converting them is the step that proves
the library replaced its sources rather than joined them, and each needs its own hardware (the T630
and the Pi 2), so it is its own work, one driver per step.
