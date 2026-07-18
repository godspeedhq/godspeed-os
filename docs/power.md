# Power and Idle Efficiency

> **Status:** Design note, active on `feat/power-efficiency`. Non-normative until any part
> lands as an amendment. Records the strategy for reducing CPU/power draw without hurting
> latency or robustness. Companion to the implementation work on this branch.

## 1. The problem

GodspeedOS is already lean on RAM (a few MiB used of gigabytes) and most services sit at
**0% CPU** - they block in `recv` and wake only when a message arrives. The lone exceptions,
visible in `observe`, are the USB host-controller drivers **`xhci`** and **`ehci`**, which peg
their cores at **100% CPU continuously**.

On a desktop this is merely wasteful. On a laptop it is a battery and thermal problem: two of
four cores can never sleep, they run hot forever, and `idle_can_halt` (the mechanism that lets an
idle core `hlt` and cool) is defeated for those cores because they are never idle.

This is not something USB inherently requires. Every *other* service already does the efficient
thing. The USB drivers are the outliers, and the reason is history, not necessity: they settled
into a **busy-spin** watching for input instead of blocking.

## 2. The tradeoff, named precisely

There are three ways a driver can watch for input, and they are not equal:

| Approach | CPU idle | Latency | Notes |
|----------|----------|---------|-------|
| **Busy-poll** (current) | 100% | ~0 | `loop { check_event_ring(); }`, never blocks. Core never sleeps. |
| **Sleep-poll** (rejected) | lower | = sleep interval | `loop { check(); sleep(N); }`. Shorten `N` to fix lag and CPU climbs back. No free lunch. |
| **Interrupt-driven** (goal) | ~0% | interrupt latency + `bInterval` | `loop { recv(irq); process(); }`. Low CPU *and* low latency. |

The prior "cool-c1" attempt picked **sleep-poll** and was rightly rejected: on the T630 the
keyboard felt laggy, because sleep-poll literally trades CPU for keystroke latency. The lesson is
permanent: **the answer to "polling burns CPU" is not "poll slower" - it is "stop polling and
block on the interrupt."** See the `xhci-cool-c1 REJECTED` record.

## 3. The key realization: the controller already polls, in hardware

A USB keyboard sits on an **interrupt endpoint**. The xHCI/EHCI controller polls that endpoint in
hardware every `bInterval` (~8-10 ms for a boot keyboard). When there is no keypress - 99.9% of
the time - the controller finds nothing and stays quiet. When a key is pressed it completes the
transfer and raises a **controller interrupt** (we already see both fire: `MSI enabled ...
vector=0x28` on xhci, `USBSTS.USBINT set` on ehci, `kernel deliver() vector=0x29`).

So the software driver spinning on the event ring is **redundant with polling the hardware is
already doing for us, better**. The fix is to let the controller do the periodic polling it was
built for, and wake our driver only when a report actually lands. The polling does not disappear;
it moves to where it is cheap.

This is exactly how Linux and macOS work, and it is worth being clear that it is not exotic:

- **Linux** submits a URB on the interrupt-IN endpoint and returns; the host-controller driver
  gets an MSI on completion, runs its ISR, fires the URB completion callback (`usbhid`), which
  processes the report and re-submits. Between keystrokes the CPU does nothing for USB.
- **macOS** does the same via IOKit (`IOUSBHostFamily`/`IOHIDFamily`): an async interrupt-endpoint
  read, completed by an interrupt, a callback that re-arms. Event-driven, no software poll.

Interrupt-driven USB is the industry-standard driver model. GodspeedOS's busy-spin is simply the
part not done yet.

## 4. The fix: block on the interrupt (matches every other service)

The constitution already describes this model (§12): "Kernel routes interrupts. ... Driver Service
... `recv()` returns interrupt event." The change is to make the USB driver main loop **block on
its interrupt endpoint** - the same `recv`-blocking shape every other service already uses - rather
than spin on the controller's event ring.

When blocked, the driver's core draws near-zero power and can `hlt` (`idle_can_halt`). A completion
interrupt wakes it instantly on a real keypress. Expected result: the two pegged cores drop from
**100% to ~0% at rest**, spiking only while input is actually flowing.

## 5. The sharp edges (and what probably bit the last attempt)

Interrupt-driven is the right model, but "the keyboard felt delayed" is a real risk to design out.
The likely culprits are not the model itself:

1. **Cross-core wake latency.** The USB IRQ lands on core 0 (`kernel deliver() vector=0x29 on core
   0`) while the driver runs on another core (ehci on core 3, xhci on core 2), so every keystroke
   pays a cross-core IPI + reschedule to wake the driver (§8.8). Bounded, but per-keystroke.
   **Mitigation: pin the USB driver to the same core the IRQ routes to**, making the wake local -
   no IPI. This alone may explain past lag.
2. **Interrupt moderation.** xHCI has an interrupt-moderation register (IMOD). If set to coalesce,
   interrupts are batched and input feels delayed. It wants to be low/off for HID.
3. **Missed interrupt = dead keyboard.** The failure mode of *pure* interrupt-driven: if one
   completion interrupt is ever lost (a re-queue race, a controller quirk), a driver blocked
   forever leaves the keyboard dead. This is the real reason a busy-poll feels "safe", and it must
   be designed around, not ignored.

## 6. The shape we aim for: interrupt-primary + a slow watchdog

The GodspeedOS "bounded and loud" answer to the missed-interrupt risk is **interrupt-primary with a
slow watchdog fallback**:

- The main loop blocks on the interrupt endpoint (asleep, ~0% CPU, core can `hlt`).
- A completion interrupt wakes it instantly on a real keypress; it processes the report(s) and
  re-arms.
- **Hot-plug rides the port-status-change interrupt**, not a port poll, so even enumeration stops
  needing a spin.
- A *slow* watchdog wake (order 250 ms - 1 s) is the only residual poll. It costs almost nothing,
  and if an interrupt was ever missed it re-checks the ring and self-heals instead of
  dead-keyboarding. 99.99% asleep, instant on input, robust against a lost IRQ.

This is strictly better than busy-spin on both axes (CPU and latency) *and* more robust, because a
busy-spin has no self-heal story either - it just happens to re-read the ring constantly.

## 7. The next lever: tickless idle

The USB fix is the 90% win. The next lever, and the one with the most GodspeedOS-specific tension,
is **tickless idle**. Today every core takes a ~100 Hz scheduler tick (the 10 ms quantum), so even
a fully idle, blocked core wakes 100 times a second for a tick that finds nothing to do. Linux
(`NO_HZ`/dynticks) and macOS stop the tick on an idle core and let it sleep until a real event.

The tension is real and is why this is a later, careful step, not a free flip:

- The **liveness watchdog** detects a silently-wedged core *by* the tick (a core that stops ticking
  is how we notice it stalled). Tickless idle removes that signal for idle cores, so the watchdog
  must learn to distinguish "idle on purpose" from "wedged".
- **Preemption** is tick-driven; a core with runnable work still needs its tick. Tickless applies
  only to a core whose run queue is empty and that is parked.

Reconciling "let idle cores sleep through the tick" with "still notice a core that stopped ticking
because it wedged" is the design knot. **§14 is the worked design that solves it** - and the key move
is that GodspeedOS should **slow** the idle tick rather than stop it, which sidesteps the watchdog
tension entirely and keeps a lost-wake safety net.

## 8. Further out: efficiency-core placement (arch-gated)

Apple's battery edge is mostly *not* the driver model (Linux matches it); it is system-wide wakeup
minimization (timer coalescing, tickless), owning the silicon to sleep deeper than a
general-purpose OS safely can, and **heterogeneous cores** - routing light work (a keystroke
handler) onto tiny efficiency cores while the performance cores stay asleep.

GodspeedOS is unusually well-shaped for the last one: it already separates *identity* from
*location* and places services statically (§9.2). On a big.LITTLE / Apple-Silicon-style ARM target
- exactly where the aarch64 port is headed (`docs/aarch64.md`) - the placement policy could route
light services (USB, logger) onto efficiency cores *by class*, not just by core number. The
architecture is already shaped for it; the hardware is not here yet.

The honest ceiling: GodspeedOS runs on commodity hardware with firmware it cannot trust (the
Goldmont+ APIC power-gate quirk that forced the `sti`-only idle spin is exactly this), so it will
always be more conservative about the deepest sleep states than Apple, who owns the whole stack.
Linux is in the same boat, for the same reason. We can match the driver model now, borrow the
wakeup-minimization ideas next, and treat deep-package-sleep as hardware-gated.

## 9. How we measure (not "it feels fine")

The last attempt was judged by feel and failed. This one is judged by numbers:

- **`observe`** before/after: `xhci`/`ehci` should drop from 100% to ~0% at rest, spiking only
  while typing.
- **Typing latency on real hardware.** QEMU's timing lies (proven repeatedly this project), so the
  T630 and Wyse 5070 are the judges. A held key's auto-repeat cadence and a fast-typing burst are
  the stress tests.
- **Measure on the Wyse specifically**, whose Goldmont+ APIC has the power-gate quirk we already
  had to design `idle_can_halt` around; USB interrupt delivery through C-states is exactly the kind
  of thing that behaves differently there.

## 10. Constitution alignment

- **§12 (Drivers and Interrupts)**: the target model is the one the spec already describes -
  drivers receive interrupt events, they do not poll hardware.
- **§26.6 (Bounded behaviour)**: a busy-spin is unbounded CPU by construction; interrupt-primary +
  a bounded watchdog is bounded and explicit.
- **§20 / §26.12 (Correctness before performance)**: here correctness and efficiency align - the
  interrupt-driven design is both more correct (it is the intended model) and more efficient. No
  invariant is traded for the win.

## 11. Critical prior art: this was built, tested, and reverted

Before writing anything, know this: **block-on-interrupt USB was fully implemented, wired end to end,
hardware-tested on the T630/Wyse, and then deliberately reverted to busy-poll** (merged branch
`feat/usb-interrupt-driven`, merge `49021aa`). The kernel infrastructure survives and is live; only
the drivers' *reliance* on it was removed. The revert arc:

- `e767956` ehci: block on the interrupt instead of busy-polling
- `9d1c43c` / `b0e40e2` usb: per-core / BSP shared-tick `recv_timeout` wake (fighting input lag)
- `e68a1aa` **ehci: revert to busy-poll - "EHCI interrupt is dead"**
- `efff4d1` xhci: busy-poll only while a key is held (auto-repeat), block otherwise
- `fafcd0e` usb: scale both back to busy-poll - "return to the flawless state"

So this is **not** "add an interrupt path". It is "make a reverted approach actually work", and the
two controllers are different problems:

- **xHCI (MSI, edge-triggered) - tractable.** Reverted for *tuning* (input lag, sluggish auto-repeat,
  hot-plug wedges), not a hardware wall. The MSI-X interrupt is still enabled and drained today
  (`services/xhci/src/main.rs`), it just does not gate the loop. **The genuinely new lever: the
  drivers stayed pinned to cores 2/3 while the USB IRQ lands on core 0, so every wake paid a
  cross-core IPI - co-locating the driver with its IRQ core was never tried**, and it aims straight
  at the lag the revert cited. Auto-repeat needs care because a *held* key emits no new USB reports
  (repeats are synthesized from a timer), so the loop must still wake on a deadline while a key is
  down - the `efff4d1` "busy-poll only while held" hybrid is one answer.
- **EHCI (level INTx) - the hard blocker.** The driver documents that its level line *stops
  asserting* once the async schedule goes cold, so a blocked `recv` never wakes (`deliver()` fired
  **zero** times once it blocked, across many T630 flashes). That is a hardware-model conflict, not
  tuning. Worth re-checking whether the keyboard's interrupt transfers belong on EHCI's *periodic*
  schedule (frame-driven, always live) rather than the async schedule before concluding it is
  impossible - but until that is re-verified on hardware, **EHCI stays busy-poll**.

## 12. Phasing (revised for the prior art)

1. **Phase 1a - xHCI interrupt-driven, with IRQ-core co-location (this branch first).** The new
   angle the last attempt missed: route the xHCI MSI to, and pin the driver on, the same core, so
   the wake is local (no IPI). Block on `recv_timeout` when idle; keep a short deadline / brief
   busy-poll only while a key is held (auto-repeat); a slow watchdog wake keeps hot-plug detection
   alive. Judge by `observe` (100% -> ~0% at rest) and **hardware typing feel** - QEMU timing lies,
   so the T630/Wyse are the sole judges. The Wyse (xHCI only, no EHCI) gets the full win here.
2. **Phase 1b - EHCI: DEFERRED (needs a from-scratch periodic-schedule engine; see §13).** The
   investigation found the keyboard sits on EHCI's ASYNC schedule and the PERIODIC schedule is
   entirely absent, so a blocked driver cannot wake - hardware-confirmed on the T630. Interrupt-driven
   EHCI requires building periodic split-interrupt scheduling from scratch (§13): a medium-to-large
   lift that benefits ONLY the T630 (legacy USB 2.0; modern laptops are xHCI-only, so it does not
   serve the laptop-battery goal). EHCI stays busy-poll (the known-good state). Do not re-break the
   flawless state for a CPU number on a non-laptop.
3. **Phase 2 - tickless-ish idle.** Stop ticking a parked, empty-run-queue core, reconciled with
   the liveness watchdog and preemption (§7). The next system-level lever.
4. **Future (arch-gated) - efficiency-core placement.** On a big.LITTLE ARM target, route light
   services onto E-cores via the placement policy (§8).

> **Discipline for this branch:** the busy-poll state is the known-good baseline. Every change is
> hardware-judged against it, behind a feature flag or an easy revert, and we do not trade typing
> feel or hot-plug robustness for a CPU percentage (§26.12 - correctness before performance). The
> failure mode to respect is a dead or laggy keyboard, which is exactly what reverted the last
> attempt.

## 13. EHCI investigation: interrupt-driven needs the periodic schedule (verdict)

A deep read of `services/ehci/src/main.rs` settled whether interrupt-driven EHCI is viable. Verdict:
**not without building a from-scratch periodic-schedule engine.**

**What the code does today.** The keyboard's interrupt-IN queue head is linked into the **async**
schedule (`ASYNCLISTADDR` + `USBCMD.ASE`), with the head-of-reclamation-list bit set and the QH's
S-mask / C-mask fields (dword 2 bits 0-15) **zero** - the structural signature of an async QH. The
**periodic** schedule is entirely absent: `PERIODICLISTBASE` is never programmed (it stays 0, matching
the boot log), there is no frame-list array in the DMA arena, and there is no `USBCMD.PSE` path. The
driver keeps the endpoint alive by re-arming a fresh ACTIVE qTD every time one completes (`arm_int`).

**Why blocking fails (confirmed, not just claimed).** An async interrupt endpoint only advances while
the driver re-arms it. Block the driver and it stops re-arming, the QH goes idle, no qTD retires, no
`USBSTS.USBINT`, no `deliver()`. The driver's own note ("INTx never reached the kernel once the driver
blocked, proven across many T630 flashes") is exactly consistent with this. A **periodic** interrupt
endpoint is different: the controller walks the periodic frame list every microframe autonomously, so
a completion raises `USBINT` regardless of driver activity - which is what lets a blocked driver wake.

**What interrupt-driven EHCI would require (from scratch).**
1. Allocate a periodic frame list (1024 dwords, 4 KiB-aligned) in the DMA arena.
2. Program `PERIODICLISTBASE`, enable `USBCMD.PSE`, wait `USBSTS.PSS`.
3. Build the interrupt QH with a non-zero **S-mask** (start-split microframe) and, because the T630
   keyboard is a low-speed device behind a high-speed hub's Transaction Translator, a **C-mask**
   (complete-split microframes) - a periodic split-interrupt transfer.
4. Link the QH into the periodic tree at the polling interval, instead of the async reclamation ring.
5. Keep IOC on the qTD (already done) so periodic completions raise `USBINT` autonomously; then the
   driver can genuinely block on `EHCI_INT_VECTOR` (0x29).

This is a **medium-to-large** change - a second schedule engine, new DMA regions, split-interrupt S/C
mask computation, periodic-tree linking - and every existing helper (`control`, `arm_int`,
`poll_devices`) assumes the async model. The simpler "async + short watchdog + block" alternative is a
**hardware-proven dead-end on the T630** (INTx goes cold), so it is not an option here.

**ROI and recommendation - why this is DEFERRED, not built now.**
- **It benefits only the T630.** EHCI is legacy USB 2.0. The Wyse 5070 is xHCI-only (its EHCI driver
  just idles). Modern laptops are xHCI-only too - EHCI was phased out of new silicon years ago. So
  interrupt-driven EHCI does **not** serve the stated laptop-battery goal; the xHCI work (Phase 1a,
  done) is the part that does, and it already covers any current laptop.
- **The T630 is a desktop thin-client, not battery-powered.** The only real payoff there is fan/heat
  from the one hot core - worth something, but not the headline.
- **It cannot be validated without the T630.** QEMU's split-TT periodic emulation may not faithfully
  reproduce the T630's rate-matching hub, so this must be built iteratively against real hardware,
  behind a feature flag (default = busy-poll, the known-good state), with the operator testing typing
  feel and hot-plug each step. Writing it blind risks the T630's *only* keyboard path.
- **The higher-ROI next lever is Phase 2 (tickless idle, §7).** It reclaims idle-tick wakeups on
  **every** core and machine - including whatever core EHCI busy-polls on - so it helps the T630's
  power/heat picture too, without the risk of rebuilding the T630's keyboard stack. It is the better
  next investment for the laptop goal.

**Decision left to the operator:** pursue the periodic-schedule engine (feature-flagged, T630-iterative)
when the fan/heat justifies it, or leave EHCI on busy-poll and move to Phase 2. This section is the
ready-to-implement design for whenever it is chosen. No speculative driver code was written, to keep
the T630 keyboard's known-good path intact.

## 14. Phase 2 design: slow the idle tick (tickless idle, done the GodspeedOS way)

This is the build-ready design for §7. The investigation of the scheduler/timer/watchdog machinery
(`kernel/src/task/scheduler.rs`, `arch/x86_64/boot.rs`, `arch/x86_64/interrupts.rs`) settled the shape.

### 14.1 The mechanism today

- **Per-core, self-re-arming timer.** Each core (BSP and every AP) arms its own LAPIC timer at
  `init_local_apic`. On real HW it is TSC-Deadline one-shot: the timer ISR re-arms it every tick,
  right after EOI. ~100 Hz (~10 ms quantum, PIT-calibrated). QEMU uses periodic mode (hardware
  auto-reload; the watchdog is disabled there).
- **Idle cores still tick.** When a core's run queue is empty it calls `wait_for_interrupt()` -
  `sti; hlt` where `IDLE_CAN_HALT` is true, else `sti`-only spin. But **the timer keeps firing every
  quantum regardless**, so an idle core wakes 100x/s to run an ISR that finds nothing and re-arms
  itself. That is the waste.
- **The liveness watchdog (the tension).** Each core stamps its own TSC into `CORE_LAST_TICK_TSC[cid]`
  in the timer ISR. Every core, every tick, checks every *other* core and **panics the whole machine**
  if one has made no progress for `tpq * 300` (~3 s). The stamp is per-core-self; the **check is
  cross-core**. So a core that simply stops ticking does not self-exempt - it is policed by all the
  others and gets the machine panicked after ~3 s. Nothing today distinguishes "idle on purpose" from
  "wedged"; the tick *is* the only liveness signal.
- **The BSP-only monotonic clock (the second hard constraint).** `MONOTONIC_TICKS` is advanced in
  exactly one place - `scan_timed_wakes`, driven only from the **BSP's** timer ISR (`cid == 0`). It is
  the single core-independent clock (AMD cores need not share a TSC), and every timed wake rides it:
  `recv_timeout`, the xHCI hot-plug watchdog, keyboard auto-repeat, `block_rpc` deadlines. The BSP
  also drives COM2 control (`process_pending`) and COM1 input (`uart_rx_poll`) from its tick. So if
  the **BSP** ever stops ticking, all timed wakes and console input die system-wide.

### 14.2 The design: slow the idle tick on halt-capable APs

The elegant move is **not** to stop the idle tick (which forces a watchdog exemption and loses the
lost-wake safety net) but to **slow** it:

- **Idle AP** (run queue empty, `cid != 0`, `IDLE_CAN_HALT`): re-arm the timer at a long
  `IDLE_QUANTUM` (~1 s) instead of the ~10 ms quantum, then `hlt`. The core now wakes ~1x/s instead
  of ~100x/s - or immediately on real work.
- **Busy AP** (>=1 runnable task): the normal ~10 ms quantum, unchanged (preemption needs it). The
  transition idle->busy must re-arm the quantum promptly when a task is picked, or a newly-runnable
  busy task would go un-preempted for up to ~1 s. A per-core "idle-tick mode" flag (set when arming
  long, cleared + re-armed to quantum when `pick_next` returns a task) is the mechanism.
- **BSP (`cid == 0`): always the ~10 ms quantum.** It is the heartbeat - the monotonic clock, timed
  wakes, and COM2/COM1 polling all ride it. One always-ticking core is the cost of not re-homing that
  clock (Phase 2b, §14.4). This is why the win is on the APs.

**Why this sidesteps the watchdog entirely (the payoff):** a slow-ticking idle core *still stamps*
`CORE_LAST_TICK_TSC` every ~1 s. Since `IDLE_QUANTUM` (~1 s) is comfortably under the watchdog
threshold (`tpq * 300` ~ 3 s), every policing core still sees it as alive. **No watchdog change is
needed** - the stamp and check stay exactly as they are. (Design constraint: `IDLE_QUANTUM` MUST stay
below the watchdog threshold, or the watchdog would false-panic an idle core. Couple them, e.g.
`IDLE_QUANTUM = threshold / 3`, so they can never drift into a false panic.)

**Why the lost-wake safety net survives:** today the 100 Hz timer is not only preemption - it is also
a 10 ms re-poll of the run queue that *recovers a lost cross-core wake* (a dropped `WAKE_RECEIVER`
IPI). A slow idle tick keeps that recovery, just at ~1 s granularity: a parked core that missed its
wake IPI re-checks its run queue within ~1 s and picks up the work. A *fully* tickless core would lose
this entirely (a lost wake = parked forever, invisible). Slowing rather than stopping keeps the
GodspeedOS "bounded and recoverable" property (§26.6, §26.7) for a negligible power difference between
1 Hz and 0 Hz.

**Waking on real work is unchanged:** a `WAKE_RECEIVER` IPI (cross-core send / `wake_by_slot`) or a
device IRQ wakes a `hlt`-ed core immediately, regardless of the timer. The slow tick is only the
fallback; real work still wakes the core instantly.

### 14.3 Gating and scope

- **Only where `IDLE_CAN_HALT` is true.** The win is real only when the core actually `hlt`s between
  ticks (then fewer ticks = deeper/longer sleep = less power). On Goldmont+ (the Wyse 5070), C-states
  are BIOS-locked and `IDLE_CAN_HALT` is false, so idle cores `sti`-spin and never sleep - slowing the
  timer saves nothing there (their idle spin is a separate, firmware-gated problem, and note `observe`
  hides it: an idle core reads 0% *task-share* while physically spinning). So Phase 2a helps the T630
  and typical laptops with working C-states/ARAT, not the Wyse.
- **Only the APs** (`cid != 0`). The BSP stays 100 Hz (§14.2).
- **Expected win:** on a 4-core T630, three APs drop from ~100 Hz to ~1 Hz idle wakeups (sleeping ~1 s
  at a time instead of 10 ms); one core (BSP) keeps the heartbeat. This is the deep-sleep enabler a
  laptop needs. It does NOT rescue a core EHCI busy-polls at 100% - only interrupt-driving or gating
  EHCI does that (§13).

### 14.4 Phase 2b (future, harder): a fully-tickless BSP

To let core 0 also sleep deep, its tick-borne duties must move off the scheduler tick onto an
always-on timer decoupled from it: `scan_timed_wakes` + `MONOTONIC_TICKS` (re-armed as a one-shot to
the *next* pending deadline rather than a fixed 100 Hz), and COM2/COM1 polling (interrupt-driven UART
RX instead of polled). That is a larger change and is deferred; Phase 2a (AP slow-tick) captures most
of the win first.

### 14.5 Validation

- **QEMU proves correctness, not power.** Under load: preemption still fair (two busy tasks on one AP
  still round-robin at 10 ms), timed wakes still fire (recv_timeout, auto-repeat, hot-plug), a
  cross-core wake still schedules promptly, and no false liveness panic over a multi-minute idle soak.
- **Hardware (T630, `IDLE_CAN_HALT` true) proves power.** `observe` will *not* show it - an idle core
  already reads 0% task-share. Need a wakeup counter (ticks/sec per core) or a real thermal/power
  read. A minute of idle should show the APs at ~1 tick/s and the BSP at ~100.
- **Risk:** this touches the scheduler timer core; the known-good baseline is "always 100 Hz". A bug
  is a missed preemption (a busy task starves) or a stalled timed wake - serious, so build it behind
  care and iterate on hardware, exactly like the driver work. The watchdog-coupling constraint
  (§14.2) is the one invariant that must not be violated.
